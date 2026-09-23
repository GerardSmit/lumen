//! Translate a WebAssembly function body to `lumen-codegen` IR.
//!
//! The generated function takes a `VmCtx` pointer as its first (I64) parameter, followed by the
//! wasm parameters, and returns the wasm results. The runtime lays out `VmCtx` as:
//!
//! | offset | field                                                              |
//! |--------|--------------------------------------------------------------------|
//! | 0      | `mem_base: *mut u8` — linear memory 0                              |
//! | 8      | `mem_len: u64` — its size in bytes                                 |
//! | 16     | `globals: *const *mut u64` — one cell pointer per global index     |
//!
//! Calls to function index `f` are `Call`s with external id `f` (imports included; the runtime
//! resolves imports to trampolines). Runtime helpers use ids from [`HELPER_BASE`]. Every function
//! a front end emits calls with the callee's `VmCtx` first.
//!
//! Functions using features without a native path (reference types, table operations,
//! `memory.init`/`data.drop`) are rejected with `Err` and stay on the interpreter.

use super::parse::{FuncBody, FuncType, ImportKind, Module, ValType};
use lumen_codegen::*;

/// Trap codes, shared with the runtime that maps them to `RuntimeError` messages.
pub mod trap {
    pub const UNREACHABLE: u32 = 0;
    pub const MEMORY_OOB: u32 = 1;
    pub const DIV_BY_ZERO: u32 = 2;
    pub const INT_OVERFLOW: u32 = 3;
    pub const INVALID_CONVERSION: u32 = 4;
    pub const TABLE_OOB: u32 = 5;
    pub const NULL_ELEMENT: u32 = 6;
    pub const SIGNATURE_MISMATCH: u32 = 7;
    /// A runtime helper reported a trap (its own message is recorded by the runtime).
    pub const HELPER: u32 = 8;

    pub fn message(code: u32) -> &'static str {
        match code {
            UNREACHABLE => "wasm: unreachable executed",
            MEMORY_OOB => "wasm: out of bounds memory access",
            DIV_BY_ZERO => "wasm: integer divide by zero",
            INT_OVERFLOW => "wasm: integer overflow",
            INVALID_CONVERSION => "wasm: invalid conversion to integer",
            TABLE_OOB => "wasm: undefined element (indirect call)",
            NULL_ELEMENT => "wasm: uninitialized table element",
            SIGNATURE_MISMATCH => "wasm: indirect call type mismatch",
            _ => "wasm: trap",
        }
    }
}

pub const VMCTX_MEM_BASE: i32 = 0;
pub const VMCTX_MEM_LEN: i32 = 8;
pub const VMCTX_GLOBALS: i32 = 16;

/// External ids at and above this are runtime helpers, not wasm functions.
pub const HELPER_BASE: u32 = 0x8000_0000;
/// `(vmctx, delta: i32) -> i32` — `memory.grow`.
pub const HELPER_MEMORY_GROW: u32 = HELPER_BASE;
/// `(vmctx, dst: i32, src: i32, n: i32) -> i32` — `memory.copy`; nonzero means trap.
pub const HELPER_MEMORY_COPY: u32 = HELPER_BASE + 1;
/// `(vmctx, dst: i32, val: i32, n: i32) -> i32` — `memory.fill`; nonzero means trap.
pub const HELPER_MEMORY_FILL: u32 = HELPER_BASE + 2;
/// `(vmctx, type_index: i32, table: i32, elem: i32) -> i64` — resolve an indirect callee to a
/// `FuncSlot { code: u64, vmctx: u64 }` pointer, or a trap code below 16.
pub const HELPER_RESOLVE_INDIRECT: u32 = HELPER_BASE + 3;

fn ir_type(t: ValType) -> Result<Type, String> {
    Ok(match t {
        ValType::I32 => Type::I32,
        ValType::I64 => Type::I64,
        ValType::F32 => Type::F32,
        ValType::F64 => Type::F64,
        ValType::FuncRef | ValType::ExternRef => {
            return Err("wasm jit: reference types are not supported".into())
        }
    })
}

fn ir_types(ts: &[ValType]) -> Result<Vec<Type>, String> {
    ts.iter().map(|&t| ir_type(t)).collect()
}

/// The native signature of a wasm function type: `VmCtx` first.
pub fn native_signature(ty: &FuncType) -> Result<Signature, String> {
    let mut params = vec![Type::I64];
    params.extend(ir_types(&ty.params)?);
    Ok(Signature::new(params, ir_types(&ty.results)?))
}

/// The type of function index `f` (imports first).
pub fn func_type(m: &Module, f: u32) -> Result<&FuncType, String> {
    let ti = if f < m.imported_func_count {
        m.imports
            .iter()
            .filter_map(|i| match i.kind {
                ImportKind::Func(t) => Some(t),
                _ => None,
            })
            .nth(f as usize)
    } else {
        m.func_types.get((f - m.imported_func_count) as usize).copied()
    };
    ti.and_then(|t| m.types.get(t as usize))
        .ok_or_else(|| format!("wasm jit: bad function index {f}"))
}

// ---- decoding ---------------------------------------------------------------------------------

struct Cursor<'a> {
    code: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn eof(&self) -> bool {
        self.pos >= self.code.len()
    }
    fn byte(&mut self) -> Result<u8, String> {
        let b = *self.code.get(self.pos).ok_or("wasm jit: truncated body")?;
        self.pos += 1;
        Ok(b)
    }
    fn uleb(&mut self) -> Result<u64, String> {
        let (mut result, mut shift) = (0u64, 0);
        loop {
            let b = self.byte()?;
            if shift < 64 {
                result |= ((b & 0x7f) as u64) << shift;
            }
            if b & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(self.uleb()? as u32)
    }
    fn sleb(&mut self) -> Result<i64, String> {
        let (mut result, mut shift) = (0i64, 0);
        loop {
            let b = self.byte()?;
            if shift < 64 {
                result |= ((b & 0x7f) as i64) << shift;
            }
            shift += 7;
            if b & 0x80 == 0 {
                if shift < 64 && b & 0x40 != 0 {
                    result |= -1i64 << shift;
                }
                return Ok(result);
            }
        }
    }
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let s = self
            .code
            .get(self.pos..self.pos + N)
            .ok_or("wasm jit: truncated immediate")?;
        self.pos += N;
        Ok(s.try_into().unwrap())
    }
    fn block_type(&mut self, m: &Module) -> Result<(Vec<Type>, Vec<Type>), String> {
        let b = *self.code.get(self.pos).ok_or("wasm jit: truncated blocktype")?;
        if b == 0x40 {
            self.pos += 1;
            return Ok((vec![], vec![]));
        }
        if let Some(t) = valtype(b) {
            self.pos += 1;
            return Ok((vec![], vec![ir_type(t)?]));
        }
        let idx = self.sleb()?;
        let ty = m
            .types
            .get(idx as usize)
            .ok_or("wasm jit: bad block type index")?;
        Ok((ir_types(&ty.params)?, ir_types(&ty.results)?))
    }
    /// Skip a memarg, returning its offset.
    fn memarg(&mut self) -> Result<u32, String> {
        let _align = self.uleb()?;
        self.u32()
    }
}

fn valtype(b: u8) -> Option<ValType> {
    Some(match b {
        0x7f => ValType::I32,
        0x7e => ValType::I64,
        0x7d => ValType::F32,
        0x7c => ValType::F64,
        0x70 => ValType::FuncRef,
        0x6f => ValType::ExternRef,
        _ => return None,
    })
}

// ---- translation ------------------------------------------------------------------------------

#[derive(PartialEq)]
enum FrameKind {
    Block,
    Loop,
    /// `else_block` is where the false edge goes; `else_args` are the block parameters.
    If {
        else_block: Block,
        else_args: Vec<Value>,
        has_else: bool,
    },
}

struct Frame {
    kind: FrameKind,
    /// Where `end` continues, with one parameter per result.
    end: Block,
    /// Loop header (the branch target of a loop).
    header: Option<Block>,
    params: Vec<Type>,
    results: Vec<Type>,
    /// Operand-stack height below this frame's parameters.
    height: usize,
    /// Whether any edge reaches `end`.
    end_reached: bool,
}

impl Frame {
    fn is_loop(&self) -> bool {
        self.kind == FrameKind::Loop
    }
}

struct Translator<'m, 'f> {
    m: &'m Module,
    b: FunctionBuilder<'f>,
    vmctx: Value,
    locals: Vec<Variable>,
    stack: Vec<Value>,
    ctrl: Vec<Frame>,
    reachable: bool,
    /// Nesting depth of blocks opened while unreachable.
    dead_depth: usize,
}

/// Translate function index `f` (a defined function) of `m`.
pub fn translate(m: &Module, f: u32) -> Result<Function, String> {
    let defined = f
        .checked_sub(m.imported_func_count)
        .ok_or("wasm jit: cannot translate an import")?;
    let body: &FuncBody = m
        .code
        .get(defined as usize)
        .ok_or("wasm jit: missing body")?;
    let ty = func_type(m, f)?;
    let sig = native_signature(ty)?;
    let mut func = Function::new(format!("wasm{f}"), sig);
    {
        let mut b = FunctionBuilder::new(&mut func);
        let entry = b.create_entry_block();
        let params = b.block_params(entry).to_vec();
        let vmctx = params[0];
        let mut locals = Vec::new();
        for (i, &t) in ty.params.iter().enumerate() {
            let v = b.declare_var(ir_type(t)?);
            b.def_var(v, params[i + 1]);
            locals.push(v);
        }
        for &t in &body.locals {
            let t = ir_type(t)?;
            let v = b.declare_var(t);
            let zero = zero(&mut b, t);
            b.def_var(v, zero);
            locals.push(v);
        }
        let results = ir_types(&ty.results)?;
        let ret = b.create_block();
        for &t in &results {
            b.append_block_param(ret, t);
        }
        let mut t = Translator {
            m,
            b,
            vmctx,
            locals,
            stack: Vec::new(),
            ctrl: vec![Frame {
                kind: FrameKind::Block,
                end: ret,
                header: None,
                params: vec![],
                results,
                height: 0,
                end_reached: false,
            }],
            reachable: true,
            dead_depth: 0,
        };
        let mut cur = Cursor {
            code: &body.code,
            pos: 0,
        };
        while !cur.eof() {
            t.op(&mut cur)?;
        }
        // The body's implicit final `end`.
        t.end()?;
        if !t.ctrl.is_empty() {
            return Err("wasm jit: unbalanced blocks".into());
        }
        if t.reachable {
            let rets = t.b.block_params(ret).to_vec();
            t.b.ret(&rets);
        }
        t.b.finish();
    }
    if let Err(e) = verify::verify(&func) {
        return Err(format!("wasm jit: translation produced invalid IR: {e}"));
    }
    Ok(func)
}

fn zero(b: &mut FunctionBuilder, t: Type) -> Value {
    match t {
        Type::I32 | Type::I64 => b.iconst(t, 0),
        Type::F32 => b.f32const_bits(0),
        Type::F64 => b.f64const_bits(0),
    }
}

impl Translator<'_, '_> {
    fn pop(&mut self) -> Result<Value, String> {
        self.stack.pop().ok_or_else(|| "wasm jit: operand stack underflow".into())
    }
    fn popn(&mut self, n: usize) -> Result<Vec<Value>, String> {
        if self.stack.len() < n {
            return Err("wasm jit: operand stack underflow".into());
        }
        Ok(self.stack.split_off(self.stack.len() - n))
    }
    fn peekn(&self, n: usize) -> Result<Vec<Value>, String> {
        if self.stack.len() < n {
            return Err("wasm jit: operand stack underflow".into());
        }
        Ok(self.stack[self.stack.len() - n..].to_vec())
    }
    fn push(&mut self, v: Value) {
        self.stack.push(v);
    }

    fn frame_at(&mut self, depth: u32) -> Result<usize, String> {
        let d = depth as usize;
        if d >= self.ctrl.len() {
            return Err("wasm jit: branch depth out of range".into());
        }
        Ok(self.ctrl.len() - 1 - d)
    }

    /// The target block and argument count of a branch to frame `i`; marks the edge.
    fn branch_target(&mut self, i: usize) -> (Block, usize) {
        let f = &mut self.ctrl[i];
        if f.is_loop() {
            (f.header.unwrap(), f.params.len())
        } else {
            f.end_reached = true;
            (f.end, f.results.len())
        }
    }

    fn unreachable_from_here(&mut self) {
        self.reachable = false;
    }

    // ----- memory ----------------------------------------------------------------------------

    fn mem_base(&mut self) -> Value {
        self.b.load(MemKind::I64, self.vmctx, VMCTX_MEM_BASE)
    }
    fn mem_len(&mut self) -> Value {
        self.b.load(MemKind::I64, self.vmctx, VMCTX_MEM_LEN)
    }

    /// Bounds-check `addr + offset .. + bytes` and return the host address and folded offset.
    fn host_addr(&mut self, addr: Value, offset: u32, bytes: u32) -> (Value, i32) {
        if self.m.memories.is_empty() && self.m.imported_mem_count == 0 {
            // Validation would reject this; trap defensively.
            self.b.trap(trap::MEMORY_OOB);
        }
        let ea = self.b.convert(ConvOp::Uext, Type::I64, addr);
        let len = self.mem_len();
        let span = self.b.iconst(Type::I64, offset as i64 + bytes as i64);
        let end = self.b.binary(BinaryOp::Iadd, ea, span);
        let oob = self.b.icmp(IntCC::Ugt, end, len);
        self.b.trap_if(oob, trap::MEMORY_OOB);
        let base = self.mem_base();
        let host = self.b.binary(BinaryOp::Iadd, base, ea);
        if offset <= i32::MAX as u32 {
            (host, offset as i32)
        } else {
            let off = self.b.iconst(Type::I64, offset as i64);
            (self.b.binary(BinaryOp::Iadd, host, off), 0)
        }
    }

    fn load(&mut self, kind: MemKind, cur: &mut Cursor) -> Result<(), String> {
        let offset = cur.memarg()?;
        let addr = self.pop()?;
        let (host, off) = self.host_addr(addr, offset, kind.bytes());
        let v = self.b.load(kind, host, off);
        self.push(v);
        Ok(())
    }

    fn store(&mut self, kind: MemKind, cur: &mut Cursor) -> Result<(), String> {
        let offset = cur.memarg()?;
        let value = self.pop()?;
        let addr = self.pop()?;
        let (host, off) = self.host_addr(addr, offset, kind.bytes());
        self.b.store(kind, host, value, off);
        Ok(())
    }

    fn global_cell(&mut self, idx: u32) -> Value {
        let table = self.b.load(MemKind::I64, self.vmctx, VMCTX_GLOBALS);
        self.b.load(MemKind::I64, table, (idx * 8) as i32)
    }

    fn global_type(&self, idx: u32) -> Result<Type, String> {
        let imported: Vec<_> = self
            .m
            .imports
            .iter()
            .filter_map(|i| match &i.kind {
                ImportKind::Global(g) => Some(g.val),
                _ => None,
            })
            .collect();
        let t = if (idx as usize) < imported.len() {
            imported[idx as usize]
        } else {
            self.m
                .globals
                .get(idx as usize - imported.len())
                .ok_or("wasm jit: bad global index")?
                .ty
                .val
        };
        ir_type(t)
    }

    // ----- calls -----------------------------------------------------------------------------

    fn helper(&mut self, id: u32, params: Vec<Type>, results: Vec<Type>, args: &[Value]) -> Vec<Value> {
        let mut p = vec![Type::I64];
        p.extend(params);
        let fr = self.b.func.import_function(Signature::new(p, results), id);
        let mut a = vec![self.vmctx];
        a.extend_from_slice(args);
        self.b.call_fn(fr, &a)
    }

    fn call(&mut self, f: u32) -> Result<(), String> {
        let ty = func_type(self.m, f)?.clone();
        let sig = native_signature(&ty)?;
        let args = self.popn(ty.params.len())?;
        let fr = self.b.func.import_function(sig, f);
        let mut a = vec![self.vmctx];
        a.extend(args);
        let rs = self.b.call_fn(fr, &a);
        self.stack.extend(rs);
        Ok(())
    }

    fn call_indirect(&mut self, type_idx: u32, table: u32) -> Result<(), String> {
        let ty = self
            .m
            .types
            .get(type_idx as usize)
            .ok_or("wasm jit: bad type index")?
            .clone();
        let sig = native_signature(&ty)?;
        let elem = self.pop()?;
        let args = self.popn(ty.params.len())?;
        let ti = self.b.iconst(Type::I32, type_idx as i64);
        let tb = self.b.iconst(Type::I32, table as i64);
        let slot = self.helper(
            HELPER_RESOLVE_INDIRECT,
            vec![Type::I32, Type::I32, Type::I32],
            vec![Type::I64],
            &[ti, tb, elem],
        )[0];
        for code in [trap::TABLE_OOB, trap::NULL_ELEMENT, trap::SIGNATURE_MISMATCH, trap::HELPER] {
            let c = self.b.iconst(Type::I64, code as i64);
            let is = self.b.icmp(IntCC::Eq, slot, c);
            self.b.trap_if(is, code);
        }
        let code = self.b.load(MemKind::I64, slot, 0);
        let callee_vmctx = self.b.load(MemKind::I64, slot, 8);
        let sr = self.b.func.import_signature(sig);
        let mut a = vec![callee_vmctx];
        a.extend(args);
        let rs = self.b.call_indirect(sr, code, &a);
        self.stack.extend(rs);
        Ok(())
    }

    // ----- control ---------------------------------------------------------------------------

    fn push_frame(&mut self, kind: FrameKind, params: Vec<Type>, results: Vec<Type>, header: Option<Block>) -> Result<(), String> {
        let end = self.b.create_block();
        for &t in &results {
            self.b.append_block_param(end, t);
        }
        let height = self
            .stack
            .len()
            .checked_sub(params.len())
            .ok_or("wasm jit: block parameters underflow")?;
        self.ctrl.push(Frame {
            kind,
            end,
            header,
            params,
            results,
            height,
            end_reached: false,
        });
        Ok(())
    }

    fn end(&mut self) -> Result<(), String> {
        if self.dead_depth > 0 {
            self.dead_depth -= 1;
            return Ok(());
        }
        let mut frame = self.ctrl.pop().ok_or("wasm jit: unbalanced end")?;
        if self.reachable {
            let args = self.popn(frame.results.len())?;
            self.b.jump(frame.end, &args);
            frame.end_reached = true;
        }
        if let FrameKind::If {
            else_block,
            else_args,
            has_else: false,
        } = &frame.kind
        {
            // No else arm: the parameters pass straight through.
            self.b.switch_to_block(*else_block);
            self.b.jump(frame.end, else_args);
            frame.end_reached = true;
        }
        if let Some(h) = frame.header {
            self.b.seal_block(h);
        }
        self.b.seal_block(frame.end);
        self.stack.truncate(frame.height);
        if frame.end_reached {
            if self.ctrl.is_empty() {
                // Function end: the caller emits the return from the parameters.
                self.b.switch_to_block(frame.end);
                self.reachable = true;
                return Ok(());
            }
            self.b.switch_to_block(frame.end);
            let params = self.b.block_params(frame.end).to_vec();
            self.stack.extend(params);
            self.reachable = true;
        } else {
            self.reachable = false;
        }
        Ok(())
    }

    fn op(&mut self, cur: &mut Cursor) -> Result<(), String> {
        let op = cur.byte()?;
        if !self.reachable {
            return self.dead_op(op, cur);
        }
        use BinaryOp::*;
        match op {
            0x00 => {
                self.b.trap(trap::UNREACHABLE);
                self.unreachable_from_here();
            }
            0x01 => {}
            0x02 => {
                let (p, r) = cur.block_type(self.m)?;
                self.push_frame(FrameKind::Block, p, r, None)?;
            }
            0x03 => {
                let (p, r) = cur.block_type(self.m)?;
                let header = self.b.create_block();
                let mut hp = Vec::new();
                for &t in &p {
                    hp.push(self.b.append_block_param(header, t));
                }
                let args = self.popn(p.len())?;
                self.b.jump(header, &args);
                self.b.switch_to_block(header);
                self.stack.extend(hp);
                self.push_frame(FrameKind::Loop, p, r, Some(header))?;
            }
            0x04 => {
                let (p, r) = cur.block_type(self.m)?;
                let cond = self.pop()?;
                let then = self.b.create_block();
                let else_block = self.b.create_block();
                let else_args = self.peekn(p.len())?;
                self.push_frame(
                    FrameKind::If {
                        else_block,
                        else_args,
                        has_else: false,
                    },
                    p,
                    r,
                    None,
                )?;
                self.b.brif(cond, then, &[], else_block, &[]);
                self.b.seal_block(then);
                self.b.seal_block(else_block);
                self.b.switch_to_block(then);
            }
            0x05 => self.else_()?,
            0x0b => self.end()?,
            0x0c => {
                let i = self.frame_at(cur.u32()?)?;
                let (target, n) = self.branch_target(i);
                let args = self.peekn(n)?;
                self.b.jump(target, &args);
                self.unreachable_from_here();
            }
            0x0d => {
                let i = self.frame_at(cur.u32()?)?;
                let cond = self.pop()?;
                let (target, n) = self.branch_target(i);
                let args = self.peekn(n)?;
                let cont = self.b.create_block();
                self.b.brif(cond, target, &args, cont, &[]);
                self.b.seal_block(cont);
                self.b.switch_to_block(cont);
            }
            0x0e => {
                let n = cur.u32()?;
                let mut depths = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    depths.push(cur.u32()?);
                }
                let default = cur.u32()?;
                let index = self.pop()?;
                let di = self.frame_at(default)?;
                let (dblock, arity) = self.branch_target(di);
                let args = self.peekn(arity)?;
                let mut targets = Vec::with_capacity(depths.len());
                for d in depths {
                    let i = self.frame_at(d)?;
                    let (blk, n) = self.branch_target(i);
                    if n != arity {
                        return Err("wasm jit: br_table targets differ in arity".into());
                    }
                    targets.push((blk, args.clone()));
                }
                self.b.br_table(index, &targets, (dblock, &args));
                self.unreachable_from_here();
            }
            0x0f => {
                let (target, n) = self.branch_target(0);
                let args = self.peekn(n)?;
                self.b.jump(target, &args);
                self.unreachable_from_here();
            }
            0x10 => {
                let f = cur.u32()?;
                self.call(f)?;
            }
            0x11 => {
                let t = cur.u32()?;
                let table = cur.u32()?;
                self.call_indirect(t, table)?;
            }
            0x1a => {
                self.pop()?;
            }
            0x1b | 0x1c => {
                if op == 0x1c {
                    let n = cur.u32()?;
                    for _ in 0..n {
                        let t = cur.byte()?;
                        ir_type(valtype(t).ok_or("wasm jit: bad select type")?)?;
                    }
                }
                let c = self.pop()?;
                let f = self.pop()?;
                let t = self.pop()?;
                let v = self.b.select(c, t, f);
                self.push(v);
            }
            0x20 => {
                let i = cur.u32()? as usize;
                let var = *self.locals.get(i).ok_or("wasm jit: bad local")?;
                let v = self.b.use_var(var);
                self.push(v);
            }
            0x21 | 0x22 => {
                let i = cur.u32()? as usize;
                let var = *self.locals.get(i).ok_or("wasm jit: bad local")?;
                let v = if op == 0x21 { self.pop()? } else { *self.stack.last().ok_or("wasm jit: underflow")? };
                self.b.def_var(var, v);
            }
            0x23 => {
                let i = cur.u32()?;
                let t = self.global_type(i)?;
                let cell = self.global_cell(i);
                let kind = mem_kind(t);
                let v = self.b.load(kind, cell, 0);
                self.push(v);
            }
            0x24 => {
                let i = cur.u32()?;
                let t = self.global_type(i)?;
                let v = self.pop()?;
                let cell = self.global_cell(i);
                self.b.store(mem_kind(t), cell, v, 0);
            }
            0x28 => self.load(MemKind::I32, cur)?,
            0x29 => self.load(MemKind::I64, cur)?,
            0x2a => self.load(MemKind::F32, cur)?,
            0x2b => self.load(MemKind::F64, cur)?,
            0x2c => self.load(MemKind::I32S8, cur)?,
            0x2d => self.load(MemKind::I32U8, cur)?,
            0x2e => self.load(MemKind::I32S16, cur)?,
            0x2f => self.load(MemKind::I32U16, cur)?,
            0x30 => self.load(MemKind::I64S8, cur)?,
            0x31 => self.load(MemKind::I64U8, cur)?,
            0x32 => self.load(MemKind::I64S16, cur)?,
            0x33 => self.load(MemKind::I64U16, cur)?,
            0x34 => self.load(MemKind::I64S32, cur)?,
            0x35 => self.load(MemKind::I64U32, cur)?,
            0x36 => self.store(MemKind::I32, cur)?,
            0x37 => self.store(MemKind::I64, cur)?,
            0x38 => self.store(MemKind::F32, cur)?,
            0x39 => self.store(MemKind::F64, cur)?,
            0x3a => self.store(MemKind::I32U8, cur)?,
            0x3b => self.store(MemKind::I32U16, cur)?,
            0x3c => self.store(MemKind::I64U8, cur)?,
            0x3d => self.store(MemKind::I64U16, cur)?,
            0x3e => self.store(MemKind::I64U32, cur)?,
            0x3f => {
                cur.byte()?;
                let len = self.mem_len();
                let sh = self.b.iconst(Type::I64, 16);
                let pages = self.b.binary(Ushr, len, sh);
                let v = self.b.convert(ConvOp::Wrap, Type::I32, pages);
                self.push(v);
            }
            0x40 => {
                cur.byte()?;
                let delta = self.pop()?;
                let r = self.helper(HELPER_MEMORY_GROW, vec![Type::I32], vec![Type::I32], &[delta]);
                self.push(r[0]);
            }
            0x41 => {
                let v = self.b.iconst(Type::I32, cur.sleb()? as i32 as i64);
                self.push(v);
            }
            0x42 => {
                let v = self.b.iconst(Type::I64, cur.sleb()?);
                self.push(v);
            }
            0x43 => {
                let v = self.b.f32const_bits(u32::from_le_bytes(cur.bytes::<4>()?));
                self.push(v);
            }
            0x44 => {
                let v = self.b.f64const_bits(u64::from_le_bytes(cur.bytes::<8>()?));
                self.push(v);
            }
            0x45 | 0x50 => self.unary(UnaryOp::Eqz)?,
            0x46..=0x4f => self.icmp(op - 0x46)?,
            0x51..=0x5a => self.icmp(op - 0x51)?,
            0x5b..=0x60 => self.fcmp(op - 0x5b)?,
            0x61..=0x66 => self.fcmp(op - 0x61)?,
            0x67..=0x78 => self.int_op(op - 0x67)?,
            0x79..=0x8a => self.int_op(op - 0x79)?,
            0x8b..=0x98 => self.float_op(op - 0x8b)?,
            0x99..=0xa6 => self.float_op(op - 0x99)?,
            0xa7 => self.conv(ConvOp::Wrap, Type::I32)?,
            0xa8 => self.trunc(Type::I32, true)?,
            0xa9 => self.trunc(Type::I32, false)?,
            0xaa => self.trunc(Type::I32, true)?,
            0xab => self.trunc(Type::I32, false)?,
            0xac => self.conv(ConvOp::Sext, Type::I64)?,
            0xad => self.conv(ConvOp::Uext, Type::I64)?,
            0xae => self.trunc(Type::I64, true)?,
            0xaf => self.trunc(Type::I64, false)?,
            0xb0 => self.trunc(Type::I64, true)?,
            0xb1 => self.trunc(Type::I64, false)?,
            0xb2 | 0xb4 => self.conv(ConvOp::FromSint, Type::F32)?,
            0xb3 | 0xb5 => self.conv(ConvOp::FromUint, Type::F32)?,
            0xb6 => self.conv(ConvOp::Demote, Type::F32)?,
            0xb7 | 0xb9 => self.conv(ConvOp::FromSint, Type::F64)?,
            0xb8 | 0xba => self.conv(ConvOp::FromUint, Type::F64)?,
            0xbb => self.conv(ConvOp::Promote, Type::F64)?,
            0xbc => self.conv(ConvOp::Bitcast, Type::I32)?,
            0xbd => self.conv(ConvOp::Bitcast, Type::I64)?,
            0xbe => self.conv(ConvOp::Bitcast, Type::F32)?,
            0xbf => self.conv(ConvOp::Bitcast, Type::F64)?,
            0xc0 | 0xc2 => self.unary(UnaryOp::Sext8)?,
            0xc1 | 0xc3 => self.unary(UnaryOp::Sext16)?,
            0xc4 => self.unary(UnaryOp::Sext32)?,
            0xfc => self.op_fc(cur)?,
            other => return Err(format!("wasm jit: unsupported opcode {other:#04x}")),
        }
        Ok(())
    }

    fn else_(&mut self) -> Result<(), String> {
        if self.dead_depth > 0 {
            return Ok(());
        }
        let reachable = self.reachable;
        let results = self.ctrl.last().ok_or("wasm jit: else without if")?.results.len();
        if reachable {
            let args = self.popn(results)?;
            let end = self.ctrl.last().unwrap().end;
            self.b.jump(end, &args);
            self.ctrl.last_mut().unwrap().end_reached = true;
        }
        let f = self.ctrl.last_mut().unwrap();
        let FrameKind::If {
            else_block,
            else_args,
            has_else,
        } = &mut f.kind
        else {
            return Err("wasm jit: else without if".into());
        };
        *has_else = true;
        let (else_block, else_args) = (*else_block, else_args.clone());
        let height = f.height;
        self.b.switch_to_block(else_block);
        self.stack.truncate(height);
        self.stack.extend(else_args);
        self.reachable = true;
        Ok(())
    }

    /// Skip an instruction in unreachable code, tracking block nesting.
    fn dead_op(&mut self, op: u8, cur: &mut Cursor) -> Result<(), String> {
        match op {
            0x02..=0x04 => {
                cur.block_type(self.m)?;
                self.dead_depth += 1;
            }
            0x05 => self.else_()?,
            0x0b => self.end()?,
            0x0c | 0x0d | 0x10 | 0x20..=0x24 | 0xd2 => {
                cur.uleb()?;
            }
            0x0e => {
                let n = cur.u32()?;
                for _ in 0..=n {
                    cur.uleb()?;
                }
            }
            0x11 => {
                cur.uleb()?;
                cur.uleb()?;
            }
            0x1c => {
                let n = cur.u32()?;
                for _ in 0..n {
                    cur.byte()?;
                }
            }
            0x25 | 0x26 => {
                cur.uleb()?;
            }
            0x28..=0x3e => {
                cur.memarg()?;
            }
            0x3f | 0x40 => {
                cur.byte()?;
            }
            0x41 | 0x42 => {
                cur.sleb()?;
            }
            0x43 => {
                cur.bytes::<4>()?;
            }
            0x44 => {
                cur.bytes::<8>()?;
            }
            0xd0 => {
                cur.byte()?;
            }
            0xfc => {
                let sub = cur.u32()?;
                match sub {
                    0..=7 => {}
                    8 => {
                        cur.uleb()?;
                        cur.byte()?;
                    }
                    9 | 13 | 15..=17 => {
                        cur.uleb()?;
                    }
                    10 | 12 | 14 => {
                        cur.uleb()?;
                        cur.uleb()?;
                    }
                    11 => {
                        cur.byte()?;
                    }
                    _ => return Err(format!("wasm jit: unsupported 0xfc op {sub}")),
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn unary(&mut self, op: UnaryOp) -> Result<(), String> {
        let a = self.pop()?;
        let v = self.b.unary(op, a);
        self.push(v);
        Ok(())
    }

    fn icmp(&mut self, k: u8) -> Result<(), String> {
        use IntCC::*;
        let cc = [Eq, Ne, Slt, Ult, Sgt, Ugt, Sle, Ule, Sge, Uge][k as usize];
        let b = self.pop()?;
        let a = self.pop()?;
        let v = self.b.icmp(cc, a, b);
        self.push(v);
        Ok(())
    }

    fn fcmp(&mut self, k: u8) -> Result<(), String> {
        use FloatCC::*;
        let cc = [Eq, Ne, Lt, Gt, Le, Ge][k as usize];
        let b = self.pop()?;
        let a = self.pop()?;
        let v = self.b.fcmp(cc, a, b);
        self.push(v);
        Ok(())
    }

    fn int_op(&mut self, k: u8) -> Result<(), String> {
        use BinaryOp::*;
        match k {
            0 => return self.unary(UnaryOp::Clz),
            1 => return self.unary(UnaryOp::Ctz),
            2 => return self.unary(UnaryOp::Popcnt),
            _ => {}
        }
        let op = [
            Iadd, Isub, Imul, Sdiv, Udiv, Srem, Urem, Band, Bor, Bxor, Ishl, Sshr, Ushr, Rotl, Rotr,
        ][k as usize - 3];
        let b = self.pop()?;
        let a = self.pop()?;
        let ty = self.b.func.value_type(a);
        if matches!(op, Sdiv | Udiv | Srem | Urem) {
            let zero = self.b.iconst(ty, 0);
            let z = self.b.icmp(IntCC::Eq, b, zero);
            self.b.trap_if(z, trap::DIV_BY_ZERO);
            if op == Sdiv {
                let min = self.b.iconst(ty, if ty == Type::I32 { i32::MIN as i64 } else { i64::MIN });
                let neg1 = self.b.iconst(ty, -1);
                let a_min = self.b.icmp(IntCC::Eq, a, min);
                let b_neg1 = self.b.icmp(IntCC::Eq, b, neg1);
                let both = self.b.binary(Band, a_min, b_neg1);
                self.b.trap_if(both, trap::INT_OVERFLOW);
            }
        }
        let v = self.b.binary(op, a, b);
        self.push(v);
        Ok(())
    }

    fn float_op(&mut self, k: u8) -> Result<(), String> {
        use BinaryOp::*;
        let un = [
            UnaryOp::Fabs,
            UnaryOp::Fneg,
            UnaryOp::Ceil,
            UnaryOp::Floor,
            UnaryOp::Trunc,
            UnaryOp::Nearest,
            UnaryOp::Sqrt,
        ];
        if (k as usize) < un.len() {
            return self.unary(un[k as usize]);
        }
        let op = [Fadd, Fsub, Fmul, Fdiv, Fmin, Fmax, Fcopysign][k as usize - un.len()];
        let b = self.pop()?;
        let a = self.pop()?;
        let v = self.b.binary(op, a, b);
        self.push(v);
        Ok(())
    }

    fn conv(&mut self, op: ConvOp, to: Type) -> Result<(), String> {
        let a = self.pop()?;
        let v = self.b.convert(op, to, a);
        self.push(v);
        Ok(())
    }

    /// Trapping float → int truncation: guard NaN and range, then the unchecked conversion.
    fn trunc(&mut self, to: Type, signed: bool) -> Result<(), String> {
        let x = self.pop()?;
        let from = self.b.func.value_type(x);
        let nan = self.b.fcmp(FloatCC::Ne, x, x);
        self.b.trap_if(nan, trap::INVALID_CONVERSION);
        // Exclusive bounds: the input is in range iff lo < x < hi.
        let (lo, hi): (f64, f64) = match (to, signed, from) {
            (Type::I32, true, Type::F64) => (-2147483649.0, 2147483648.0),
            (Type::I32, true, _) => (-2147483904.0, 2147483648.0),
            (Type::I32, false, _) => (-1.0, 4294967296.0),
            (Type::I64, true, Type::F64) => (-9223372036854777856.0, 9223372036854775808.0),
            (Type::I64, true, _) => (-9223373136366403584.0, 9223372036854775808.0),
            (Type::I64, false, _) => (-1.0, 18446744073709551616.0),
            _ => unreachable!(),
        };
        let (lo, hi) = if from == Type::F32 {
            (self.b.f32const(lo as f32), self.b.f32const(hi as f32))
        } else {
            (self.b.f64const(lo), self.b.f64const(hi))
        };
        let below = self.b.fcmp(FloatCC::Le, x, lo);
        let above = self.b.fcmp(FloatCC::Ge, x, hi);
        let out = self.b.binary(BinaryOp::Bor, below, above);
        self.b.trap_if(out, trap::INT_OVERFLOW);
        let op = if signed { ConvOp::ToSint } else { ConvOp::ToUint };
        let v = self.b.convert(op, to, x);
        self.push(v);
        Ok(())
    }

    fn op_fc(&mut self, cur: &mut Cursor) -> Result<(), String> {
        let sub = cur.u32()?;
        match sub {
            0..=7 => {
                let to = if sub < 4 { Type::I32 } else { Type::I64 };
                let op = if sub % 2 == 0 { ConvOp::ToSintSat } else { ConvOp::ToUintSat };
                self.conv(op, to)
            }
            10 | 11 => {
                if sub == 10 {
                    cur.byte()?;
                    cur.byte()?;
                } else {
                    cur.byte()?;
                }
                let args = self.popn(3)?;
                let id = if sub == 10 { HELPER_MEMORY_COPY } else { HELPER_MEMORY_FILL };
                let r = self.helper(id, vec![Type::I32; 3], vec![Type::I32], &args)[0];
                self.b.trap_if(r, trap::MEMORY_OOB);
                Ok(())
            }
            other => Err(format!("wasm jit: unsupported 0xfc op {other}")),
        }
    }
}

fn mem_kind(t: Type) -> MemKind {
    match t {
        Type::I32 => MemKind::I32,
        Type::I64 => MemKind::I64,
        Type::F32 => MemKind::F32,
        Type::F64 => MemKind::F64,
    }
}
