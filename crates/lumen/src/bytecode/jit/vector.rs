//! Pure counted typed-array loops: validate views/range once, then process full vectors.
//! No JS, allocation or storage mutation occurs between view acquisition and the scalar tail.
use super::*;
use lumen_codegen::VectorOp;

impl Tr<'_, '_> {
    pub(super) fn vector_prefix(&mut self, pc: usize) {
        if !cfg!(any(
            target_arch = "aarch64",
            target_arch = "wasm32",
            target_arch = "x86_64"
        )) {
            return;
        }
        let ops = self.ops;
        let (index, bound, length, mut q, end) = match ops.get(pc).copied() {
            Some(Op::JumpIfNotCmpLL(CmpKind::Lt, i, n, end)) => {
                (i, Some(n), None, pc + 1, end as usize)
            }
            Some(Op::LoadLocal(i)) => match (ops.get(pc + 1), ops.get(pc + 2)) {
                (Some(Op::GetPropLocal(a, n, _)), Some(Op::JumpIfNotCmp(CmpKind::Lt, end)))
                    if self
                        .chunk
                        .names
                        .get(*n as usize)
                        .is_some_and(|n| &**n == "length") =>
                {
                    (i, None, Some(*a), pc + 3, *end as usize)
                }
                _ => return,
            },
            _ => return,
        };
        let load_index = |q: usize| matches!(ops.get(q), Some(Op::LoadLocal(s)) if *s == index);
        if !load_index(q) || !load_index(q + 1) {
            return;
        }
        q += 2;
        let Some(Op::GetElemLocal(a)) = ops.get(q).copied() else {
            return;
        };
        q += 1;
        let mut rhs = None;
        let mut multiply = false;
        if load_index(q) {
            let Some(Op::GetElemLocal(b)) = ops.get(q + 1).copied() else {
                return;
            };
            multiply = match ops.get(q + 2) {
                Some(Op::Mul) => true,
                Some(Op::Add) => false,
                _ => return,
            };
            rhs = Some(b);
            q += 3;
        }
        let Some(Op::SetElemLocalDrop(dst)) = ops.get(q).copied() else {
            return;
        };
        if !matches!(ops.get(q + 1), Some(Op::UpdateLocal(s, UpdKind::IncDiscard)) if *s == index)
            || !matches!(ops.get(q + 2), Some(Op::Jump(h)) if *h as usize == pc)
            || q + 2 > self.backedge
            || end <= q + 2
            || !self.kinds[index as usize].is_num()
            || bound.is_some_and(|n| !self.kinds[n as usize].is_num())
        {
            return;
        }
        // All source and destination kinds must agree. Other layouts use the existing scalar JIT.
        let kind = self.ta_kinds[a as usize];
        let f64_lanes = kind == helpers::ta_code::F64;
        if (!f64_lanes && kind != helpers::ta_code::I32)
            || (!f64_lanes && multiply)
            || self.ta_kinds[dst as usize] != kind
            || rhs.is_some_and(|b| self.ta_kinds[b as usize] != kind)
        {
            return;
        }
        let scalar = self.fb.create_block();
        let mut numeric = vec![index];
        if let Some(n) = bound {
            numeric.push(n);
        }
        for s in numeric {
            for flag in [self.tdz[s as usize], self.undef[s as usize]]
                .into_iter()
                .flatten()
            {
                let value = self.fb.use_var(flag);
                let zero = self.i32c(0);
                let valid = self.fb.icmp(IntCC::Eq, value, zero);
                layout::guard(&mut self.fb, valid, scalar);
            }
        }
        let Entry::Num(first) = self.ssa_entry(index as usize) else {
            unreachable!()
        };
        let limit = if let Some(n) = bound {
            let Entry::Num(n) = self.ssa_entry(n as usize) else {
                unreachable!()
            };
            n
        } else {
            let array = self.slot_ptr(length.unwrap() as usize);
            self.call(Helper::TaLength, &[self.frame, array])
                .expect("typed-array length")
        };
        let narrow = |this: &mut Self, x| {
            let i = this.fb.convert(ConvOp::ToSintSat, Type::I32, x);
            let back = this.fb.convert(ConvOp::FromSint, Type::F64, i);
            let exact = this.fb.fcmp(FloatCC::Eq, x, back);
            layout::guard(&mut this.fb, exact, scalar);
            let zero = this.i32c(0);
            let positive = this.fb.icmp(IntCC::Sge, i, zero);
            layout::guard(&mut this.fb, positive, scalar);
            i
        };
        let first = narrow(self, first);
        let limit = narrow(self, limit);
        let start = self.fb.convert(ConvOp::Uext, Type::I64, first);
        let limit = self.fb.convert(ConvOp::Uext, Type::I64, limit);
        let mut views = Vec::new();
        let mut arrays = vec![dst, a];
        if let Some(b) = rhs {
            arrays.push(b);
        }
        for (position, &array) in arrays.iter().enumerate() {
            let ptr = self.slot_ptr(array as usize);
            let write = self.i32c((position == 0) as i64);
            let actual = self
                .call(Helper::TaView, &[self.frame, ptr, write])
                .expect("typed-array view");
            let wanted = self.i32c(kind as i64);
            let valid = self.fb.icmp(IntCC::Eq, actual, wanted);
            layout::guard(&mut self.fb, valid, scalar);
            let data = self.fb.load(PTR_MEM, self.frame, FRAME_TA_DATA);
            let count = self.fb.load(PTR_MEM, self.frame, FRAME_TA_LEN);
            let wide_count = if PTR == Type::I32 {
                self.fb.convert(ConvOp::Uext, Type::I64, count)
            } else {
                count
            };
            let in_range = self.fb.icmp(IntCC::Ule, limit, wide_count);
            layout::guard(&mut self.fb, in_range, scalar);
            views.push((data, count));
        }
        let shift = self.ptrc(if f64_lanes { 3 } else { 2 });
        // A memory32 view may end at 2^32; keep alias ranges wide so that end
        // cannot wrap to zero and falsely classify an overlap as disjoint.
        let wide_views: Vec<_> = views
            .iter()
            .map(|&(data, count)| {
                if PTR == Type::I32 {
                    (
                        self.fb.convert(ConvOp::Uext, Type::I64, data),
                        self.fb.convert(ConvOp::Uext, Type::I64, count),
                    )
                } else {
                    (data, count)
                }
            })
            .collect();
        let alias_shift = self.i64c(if f64_lanes { 3 } else { 2 });
        let dst_end = self.fb.binary(BinaryOp::Ishl, wide_views[0].1, alias_shift);
        let dst_end = self.fb.binary(BinaryOp::Iadd, wide_views[0].0, dst_end);
        for &(data, count) in &wide_views[1..] {
            let bytes = self.fb.binary(BinaryOp::Ishl, count, alias_shift);
            let source_end = self.fb.binary(BinaryOp::Iadd, data, bytes);
            let same = self.fb.icmp(IntCC::Eq, data, wide_views[0].0);
            let before = self.fb.icmp(IntCC::Ule, source_end, wide_views[0].0);
            let after = self.fb.icmp(IntCC::Ule, dst_end, data);
            let disjoint = self.fb.binary(BinaryOp::Bor, before, after);
            let safe = self.fb.binary(BinaryOp::Bor, same, disjoint);
            layout::guard(&mut self.fb, safe, scalar);
        }
        // Bound work between the ordinary backedge safepoints; the scalar tail handles all leftovers.
        let lanes = if f64_lanes { 2 } else { 4 };
        let cap = self.i64c(64 * lanes);
        let cap = self.fb.binary(BinaryOp::Iadd, start, cap);
        let smaller = self.fb.icmp(IntCC::Ult, limit, cap);
        let stop = self.fb.select(smaller, limit, cap);
        let step = self.i64c(lanes);
        let loop_b = self.fb.create_block();
        let cursor = self.fb.append_block_param(loop_b, Type::I64);
        let run = self.fb.create_block();
        self.fb.jump(loop_b, &[start]);
        self.fb.switch_to_block(loop_b);
        let next = self.fb.binary(BinaryOp::Iadd, cursor, step);
        let full = self.fb.icmp(IntCC::Ule, next, stop);
        self.fb.brif(full, run, &[], scalar, &[]);
        self.fb.seal_block(run);
        self.fb.switch_to_block(run);
        let i = if PTR == Type::I32 {
            self.fb.convert(ConvOp::Wrap, PTR, cursor)
        } else {
            cursor
        };
        let offset = self.fb.binary(BinaryOp::Ishl, i, shift);
        let addresses: Vec<V> = views
            .iter()
            .map(|&(data, _)| self.fb.binary(BinaryOp::Iadd, data, offset))
            .collect();
        let x = self.fb.load(MemKind::V128, addresses[1], 0);
        if self.prefetch {
            for &address in &addresses[1..] {
                self.fb.prefetch(address, 64);
            }
        }
        let result = if rhs.is_some() {
            let y = self.fb.load(MemKind::V128, addresses[2], 0);
            self.fb.vector_binary(
                if f64_lanes {
                    if multiply {
                        VectorOp::F64x2Mul
                    } else {
                        VectorOp::F64x2Add
                    }
                } else {
                    VectorOp::I32x4Add
                },
                x,
                y,
            )
        } else {
            x
        };
        self.fb.store(MemKind::V128, addresses[0], result, 0);
        let value = if self.kinds[index as usize] == Kind::Int32 {
            self.fb.convert(ConvOp::Wrap, Type::I32, next)
        } else {
            self.fb.convert(ConvOp::FromUint, Type::F64, next)
        };
        self.fb.def_var(self.vars[index as usize].unwrap(), value);
        self.fb.jump(loop_b, &[next]);
        self.fb.seal_block(loop_b);
        self.fb.seal_block(scalar);
        self.fb.switch_to_block(scalar);
        self.invalidate_elems();
        self.invalidate_ta();
    }
}
