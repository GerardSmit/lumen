//! Module validation, run at the end of decoding: index spaces, constant expressions, and every
//! function body type-checked with the specification's validation algorithm (an operand stack of
//! possibly-unknown types and a stack of control frames). A module that passes cannot underflow
//! the interpreter's operand stack, branch out of range, or reference a missing entity.

use std::collections::HashSet;

use super::parse::{
    ExportKind, FuncType, GlobalType, ImportKind, Module, Reader, TableType, ValType,
};

/// The most targets a `br_table` may list (V8's `kV8MaxWasmFunctionBrTableSize`).
pub const MAX_BR_TABLE_SIZE: u32 = 65520;

type R<T> = Result<T, String>;

struct Env<'m> {
    m: &'m Module,
    funcs: Vec<&'m FuncType>,
    tables: Vec<TableType>,
    globals: Vec<GlobalType>,
    mems: usize,
    data_count: Option<u32>,
}

pub fn module(m: &Module, data_count: Option<u32>) -> R<()> {
    let mut funcs = Vec::new();
    let mut tables = Vec::new();
    let mut globals = Vec::new();
    let mut mems = 0;
    for imp in &m.imports {
        match &imp.kind {
            ImportKind::Func(t) => funcs.push(ty(m, *t)?),
            ImportKind::Table(t) => tables.push(*t),
            ImportKind::Memory(_) => mems += 1,
            ImportKind::Global(g) => globals.push(*g),
        }
    }
    for &t in &m.func_types {
        funcs.push(ty(m, t)?);
    }
    tables.extend(m.tables.iter().copied());
    mems += m.memories.len();
    if mems > 1 {
        return Err("wasm: multiple memories are not supported".into());
    }
    let mut env = Env {
        m,
        funcs,
        tables,
        globals,
        mems,
        data_count,
    };
    // A defined global's initializer sees the globals before it.
    for g in &m.globals {
        env.const_expr(&g.init, g.ty.val, env.globals.len())?;
        env.globals.push(g.ty);
    }

    let mut names = HashSet::new();
    for e in &m.exports {
        if !names.insert(e.name.as_str()) {
            return Err(format!("wasm: duplicate export name {:?}", e.name));
        }
        let len = match e.kind {
            ExportKind::Func => env.funcs.len(),
            ExportKind::Table => env.tables.len(),
            ExportKind::Memory => env.mems,
            ExportKind::Global => env.globals.len(),
        };
        if e.index as usize >= len {
            return Err(format!("wasm: export {:?} index out of range", e.name));
        }
    }
    if let Some(s) = m.start {
        let t = env
            .funcs
            .get(s as usize)
            .ok_or("wasm: start function index out of range")?;
        if !t.params.is_empty() || !t.results.is_empty() {
            return Err("wasm: start function must have type [] -> []".into());
        }
    }
    for seg in &m.elems {
        let t = env
            .tables
            .get(seg.table as usize)
            .ok_or("wasm: element segment table index out of range")?;
        if t.elem != ValType::FuncRef {
            return Err("wasm: element segment table is not a funcref table".into());
        }
        env.const_expr(&seg.offset, ValType::I32, env.globals.len())?;
        if seg
            .func_indices
            .iter()
            .any(|&f| f as usize >= env.funcs.len())
        {
            return Err("wasm: element segment function index out of range".into());
        }
    }
    for seg in &m.data {
        if let Some((mem, offset)) = &seg.active {
            if *mem as usize >= env.mems {
                return Err("wasm: data segment memory index out of range".into());
            }
            env.const_expr(offset, ValType::I32, env.globals.len())?;
        }
    }
    for (i, body) in m.code.iter().enumerate() {
        let f = m.imported_func_count as usize + i;
        let fty = env.funcs[f];
        let mut locals = fty.params.clone();
        locals.extend_from_slice(&body.locals);
        Func::new(&env, locals)
            .run(&body.code, fty)
            .map_err(|e| format!("{e} (in function {f})"))?;
    }
    Ok(())
}

fn ty(m: &Module, t: u32) -> R<&FuncType> {
    m.types
        .get(t as usize)
        .ok_or_else(|| "wasm: type index out of range".into())
}

fn is_ref(t: ValType) -> bool {
    matches!(t, ValType::FuncRef | ValType::ExternRef)
}

fn ref_type(b: u8) -> R<ValType> {
    match b {
        0x70 => Ok(ValType::FuncRef),
        0x6f => Ok(ValType::ExternRef),
        _ => Err(format!("wasm: bad reference type 0x{b:x}")),
    }
}

fn val_type(b: u8) -> Option<ValType> {
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

impl Env<'_> {
    /// A constant expression yielding one `expected` value; `global.get` may read the first
    /// `visible_globals` globals, if immutable.
    fn const_expr(&self, code: &[u8], expected: ValType, visible_globals: usize) -> R<()> {
        let mut r = Reader::new(code);
        let mut stack = Vec::new();
        loop {
            match r.byte()? {
                0x41 => {
                    r.i32()?;
                    stack.push(ValType::I32);
                }
                0x42 => {
                    r.i64_leb()?;
                    stack.push(ValType::I64);
                }
                0x43 => {
                    r.bytes(4)?;
                    stack.push(ValType::F32);
                }
                0x44 => {
                    r.bytes(8)?;
                    stack.push(ValType::F64);
                }
                0x23 => {
                    let i = r.u32()? as usize;
                    let g = self.globals[..visible_globals]
                        .get(i)
                        .ok_or("wasm: constant expression global index out of range")?;
                    if g.mutable {
                        return Err("wasm: constant expression reads a mutable global".into());
                    }
                    stack.push(g.val);
                }
                0xd0 => stack.push(ref_type(r.byte()?)?),
                0xd2 => {
                    if r.u32()? as usize >= self.funcs.len() {
                        return Err("wasm: ref.func index out of range".into());
                    }
                    stack.push(ValType::FuncRef);
                }
                0x0b => break,
                op => return Err(format!("wasm: opcode 0x{op:x} is not constant")),
            }
        }
        if !r.eof() || stack != [expected] {
            return Err("wasm: type mismatch in constant expression".into());
        }
        Ok(())
    }
}

// ---- function bodies --------------------------------------------------------------------------

struct Frame {
    op: u8,
    params: Vec<ValType>,
    results: Vec<ValType>,
    height: usize,
    unreachable: bool,
}

impl Frame {
    fn labels(&self) -> &[ValType] {
        if self.op == 0x03 {
            &self.params
        } else {
            &self.results
        }
    }
}

struct Func<'e, 'm> {
    env: &'e Env<'m>,
    locals: Vec<ValType>,
    /// `None` is the unknown type of a value popped from an unreachable stack.
    vals: Vec<Option<ValType>>,
    ctrls: Vec<Frame>,
}

fn mismatch<T>() -> R<T> {
    Err("wasm: type mismatch".into())
}

impl<'e, 'm> Func<'e, 'm> {
    fn new(env: &'e Env<'m>, locals: Vec<ValType>) -> Self {
        Func {
            env,
            locals,
            vals: Vec::new(),
            ctrls: Vec::new(),
        }
    }

    fn push(&mut self, t: ValType) {
        self.vals.push(Some(t));
    }
    fn push_all(&mut self, ts: &[ValType]) {
        self.vals.extend(ts.iter().map(|&t| Some(t)));
    }
    fn pop_any(&mut self) -> R<Option<ValType>> {
        let f = self.ctrls.last().ok_or("wasm: no open frame")?;
        if self.vals.len() == f.height {
            return if f.unreachable {
                Ok(None)
            } else {
                Err("wasm: operand stack underflow".into())
            };
        }
        Ok(self.vals.pop().flatten())
    }
    fn pop(&mut self, want: ValType) -> R<Option<ValType>> {
        let got = self.pop_any()?;
        match got {
            Some(t) if t != want => mismatch(),
            _ => Ok(got),
        }
    }
    fn pop_all(&mut self, ts: &[ValType]) -> R<Vec<Option<ValType>>> {
        let mut out = vec![None; ts.len()];
        for (i, &t) in ts.iter().enumerate().rev() {
            out[i] = self.pop(t)?;
        }
        Ok(out)
    }
    fn unop(&mut self, a: ValType, r: ValType) -> R<()> {
        self.pop(a)?;
        self.push(r);
        Ok(())
    }
    fn binop(&mut self, a: ValType, r: ValType) -> R<()> {
        self.pop(a)?;
        self.pop(a)?;
        self.push(r);
        Ok(())
    }

    fn push_ctrl(&mut self, op: u8, params: Vec<ValType>, results: Vec<ValType>) {
        let height = self.vals.len();
        self.push_all(&params);
        self.ctrls.push(Frame {
            op,
            params,
            results,
            height,
            unreachable: false,
        });
    }
    fn pop_ctrl(&mut self) -> R<Frame> {
        let results = self
            .ctrls
            .last()
            .ok_or("wasm: unbalanced end")?
            .results
            .clone();
        self.pop_all(&results)?;
        let f = self.ctrls.pop().ok_or("wasm: unbalanced end")?;
        if self.vals.len() != f.height {
            return Err("wasm: values remaining on the stack at end of block".into());
        }
        Ok(f)
    }
    fn label(&self, depth: u32) -> R<Vec<ValType>> {
        let i = self
            .ctrls
            .len()
            .checked_sub(depth as usize + 1)
            .ok_or("wasm: branch depth out of range")?;
        Ok(self.ctrls[i].labels().to_vec())
    }
    fn unreachable(&mut self) -> R<()> {
        let f = self.ctrls.last_mut().ok_or("wasm: no open frame")?;
        self.vals.truncate(f.height);
        f.unreachable = true;
        Ok(())
    }

    fn block_type(&self, r: &mut Reader) -> R<(Vec<ValType>, Vec<ValType>)> {
        let b = *r.data.get(r.pos).ok_or("wasm: truncated block type")?;
        if b == 0x40 {
            r.pos += 1;
            return Ok((vec![], vec![]));
        }
        if let Some(t) = val_type(b) {
            r.pos += 1;
            return Ok((vec![], vec![t]));
        }
        let idx = r.s33()?;
        let t = usize::try_from(idx)
            .ok()
            .and_then(|i| self.env.m.types.get(i))
            .ok_or("wasm: block type index out of range")?;
        Ok((t.params.clone(), t.results.clone()))
    }
    fn local(&self, i: u32) -> R<ValType> {
        self.locals
            .get(i as usize)
            .copied()
            .ok_or_else(|| "wasm: local index out of range".into())
    }
    fn global(&self, i: u32) -> R<GlobalType> {
        self.env
            .globals
            .get(i as usize)
            .copied()
            .ok_or_else(|| "wasm: global index out of range".into())
    }
    fn table(&self, i: u32) -> R<ValType> {
        Ok(self
            .env
            .tables
            .get(i as usize)
            .ok_or("wasm: table index out of range")?
            .elem)
    }
    fn memory(&self) -> R<()> {
        if self.env.mems == 0 {
            return Err("wasm: memory instruction without a memory".into());
        }
        Ok(())
    }
    fn zero_byte(r: &mut Reader) -> R<()> {
        if r.byte()? != 0 {
            return Err("wasm: expected a zero byte (memory index 0)".into());
        }
        Ok(())
    }
    fn memarg(&self, r: &mut Reader, natural_align: u32) -> R<()> {
        self.memory()?;
        if r.u32()? > natural_align {
            return Err("wasm: alignment must not be larger than natural".into());
        }
        r.u32()?;
        Ok(())
    }
    fn data(&self, i: u32) -> R<()> {
        match self.env.data_count {
            None => Err("wasm: data index used without a data count section".into()),
            Some(n) if i >= n => Err("wasm: data segment index out of range".into()),
            _ => Ok(()),
        }
    }
    fn elem(&self, i: u32) -> R<()> {
        if i as usize >= self.env.m.elems.len() {
            return Err("wasm: element segment index out of range".into());
        }
        Ok(())
    }

    fn run(mut self, code: &[u8], ty: &FuncType) -> R<()> {
        self.ctrls.push(Frame {
            op: 0x02,
            params: vec![],
            results: ty.results.clone(),
            height: 0,
            unreachable: false,
        });
        let mut r = Reader::new(code);
        while !r.eof() {
            if self.ctrls.is_empty() {
                return Err("wasm: operators remaining after end of function".into());
            }
            self.op(&mut r)?;
        }
        // The body's implicit final `end`.
        if self.ctrls.len() != 1 {
            return Err("wasm: unterminated block".into());
        }
        self.pop_ctrl()?;
        Ok(())
    }

    fn op(&mut self, r: &mut Reader) -> R<()> {
        use ValType::*;
        let op = r.byte()?;
        match op {
            0x00 => self.unreachable()?,
            0x01 => {}
            0x02 | 0x03 => {
                let (p, res) = self.block_type(r)?;
                self.pop_all(&p)?;
                self.push_ctrl(op, p, res);
            }
            0x04 => {
                let (p, res) = self.block_type(r)?;
                self.pop(I32)?;
                self.pop_all(&p)?;
                self.push_ctrl(op, p, res);
            }
            0x05 => {
                let f = self.pop_ctrl()?;
                if f.op != 0x04 {
                    return Err("wasm: else without if".into());
                }
                self.push_ctrl(0x05, f.params, f.results);
            }
            0x0b => {
                let f = self.pop_ctrl()?;
                if f.op == 0x04 && f.params != f.results {
                    return Err("wasm: if without else must not change the stack type".into());
                }
                self.push_all(&f.results);
            }
            0x0c => {
                let l = self.label(r.u32()?)?;
                self.pop_all(&l)?;
                self.unreachable()?;
            }
            0x0d => {
                let l = self.label(r.u32()?)?;
                self.pop(I32)?;
                let vs = self.pop_all(&l)?;
                self.vals.extend(vs);
            }
            0x0e => {
                let n = r.u32()?;
                if n > MAX_BR_TABLE_SIZE {
                    return Err("wasm: br_table has too many targets".into());
                }
                let mut targets = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    targets.push(r.u32()?);
                }
                let default = self.label(r.u32()?)?;
                self.pop(I32)?;
                for t in targets {
                    let l = self.label(t)?;
                    if l.len() != default.len() {
                        return Err("wasm: br_table targets differ in arity".into());
                    }
                    let vs = self.pop_all(&l)?;
                    self.vals.extend(vs);
                }
                self.pop_all(&default)?;
                self.unreachable()?;
            }
            0x0f => {
                let res = self.ctrls[0].results.clone();
                self.pop_all(&res)?;
                self.unreachable()?;
            }
            0x10 => {
                let f = self
                    .env
                    .funcs
                    .get(r.u32()? as usize)
                    .ok_or("wasm: function index out of range")?;
                self.pop_all(&f.params)?;
                self.push_all(&f.results);
            }
            0x11 => {
                let t = ty(self.env.m, r.u32()?)?;
                if self.table(r.u32()?)? != FuncRef {
                    return Err("wasm: call_indirect through a non-funcref table".into());
                }
                self.pop(I32)?;
                self.pop_all(&t.params)?;
                self.push_all(&t.results);
            }
            0x1a => {
                self.pop_any()?;
            }
            0x1b => {
                self.pop(I32)?;
                let a = self.pop_any()?;
                let b = self.pop_any()?;
                if a.is_some_and(is_ref) || b.is_some_and(is_ref) {
                    return Err("wasm: untyped select of reference values".into());
                }
                if a.is_some() && b.is_some() && a != b {
                    return mismatch();
                }
                self.vals.push(a.or(b));
            }
            0x1c => {
                if r.u32()? != 1 {
                    return Err("wasm: typed select must have exactly one type".into());
                }
                let t = val_type(r.byte()?).ok_or("wasm: bad select type")?;
                self.pop(I32)?;
                self.pop(t)?;
                self.pop(t)?;
                self.push(t);
            }
            0x20 => {
                let t = self.local(r.u32()?)?;
                self.push(t);
            }
            0x21 => {
                let t = self.local(r.u32()?)?;
                self.pop(t)?;
            }
            0x22 => {
                let t = self.local(r.u32()?)?;
                self.unop(t, t)?;
            }
            0x23 => {
                let g = self.global(r.u32()?)?;
                self.push(g.val);
            }
            0x24 => {
                let g = self.global(r.u32()?)?;
                if !g.mutable {
                    return Err("wasm: global.set of an immutable global".into());
                }
                self.pop(g.val)?;
            }
            0x25 => {
                let t = self.table(r.u32()?)?;
                self.unop(I32, t)?;
            }
            0x26 => {
                let t = self.table(r.u32()?)?;
                self.pop(t)?;
                self.pop(I32)?;
            }
            0x28..=0x35 => {
                let (t, align) = match op {
                    0x28 => (I32, 2),
                    0x29 => (I64, 3),
                    0x2a => (F32, 2),
                    0x2b => (F64, 3),
                    0x2c | 0x2d => (I32, 0),
                    0x2e | 0x2f => (I32, 1),
                    0x30 | 0x31 => (I64, 0),
                    0x32 | 0x33 => (I64, 1),
                    _ => (I64, 2),
                };
                self.memarg(r, align)?;
                self.unop(I32, t)?;
            }
            0x36..=0x3e => {
                let (t, align) = match op {
                    0x36 => (I32, 2),
                    0x37 => (I64, 3),
                    0x38 => (F32, 2),
                    0x39 => (F64, 3),
                    0x3a => (I32, 0),
                    0x3b => (I32, 1),
                    0x3c => (I64, 0),
                    0x3d => (I64, 1),
                    _ => (I64, 2),
                };
                self.memarg(r, align)?;
                self.pop(t)?;
                self.pop(I32)?;
            }
            0x3f => {
                self.memory()?;
                Self::zero_byte(r)?;
                self.push(I32);
            }
            0x40 => {
                self.memory()?;
                Self::zero_byte(r)?;
                self.unop(I32, I32)?;
            }
            0x41 => {
                r.i32()?;
                self.push(I32);
            }
            0x42 => {
                r.i64_leb()?;
                self.push(I64);
            }
            0x43 => {
                r.bytes(4)?;
                self.push(F32);
            }
            0x44 => {
                r.bytes(8)?;
                self.push(F64);
            }
            0x45 => self.unop(I32, I32)?,
            0x46..=0x4f => self.binop(I32, I32)?,
            0x50 => self.unop(I64, I32)?,
            0x51..=0x5a => self.binop(I64, I32)?,
            0x5b..=0x60 => self.binop(F32, I32)?,
            0x61..=0x66 => self.binop(F64, I32)?,
            0x67..=0x69 => self.unop(I32, I32)?,
            0x6a..=0x78 => self.binop(I32, I32)?,
            0x79..=0x7b => self.unop(I64, I64)?,
            0x7c..=0x8a => self.binop(I64, I64)?,
            0x8b..=0x91 => self.unop(F32, F32)?,
            0x92..=0x98 => self.binop(F32, F32)?,
            0x99..=0x9f => self.unop(F64, F64)?,
            0xa0..=0xa6 => self.binop(F64, F64)?,
            0xa7 => self.unop(I64, I32)?,
            0xa8 | 0xa9 => self.unop(F32, I32)?,
            0xaa | 0xab => self.unop(F64, I32)?,
            0xac | 0xad => self.unop(I32, I64)?,
            0xae | 0xaf => self.unop(F32, I64)?,
            0xb0 | 0xb1 => self.unop(F64, I64)?,
            0xb2 | 0xb3 => self.unop(I32, F32)?,
            0xb4 | 0xb5 => self.unop(I64, F32)?,
            0xb6 => self.unop(F64, F32)?,
            0xb7 | 0xb8 => self.unop(I32, F64)?,
            0xb9 | 0xba => self.unop(I64, F64)?,
            0xbb => self.unop(F32, F64)?,
            0xbc => self.unop(F32, I32)?,
            0xbd => self.unop(F64, I64)?,
            0xbe => self.unop(I32, F32)?,
            0xbf => self.unop(I64, F64)?,
            0xc0 | 0xc1 => self.unop(I32, I32)?,
            0xc2..=0xc4 => self.unop(I64, I64)?,
            0xd0 => {
                let t = ref_type(r.byte()?)?;
                self.push(t);
            }
            0xd1 => {
                if self.pop_any()?.is_some_and(|t| !is_ref(t)) {
                    return mismatch();
                }
                self.push(I32);
            }
            0xd2 => {
                if r.u32()? as usize >= self.env.funcs.len() {
                    return Err("wasm: ref.func index out of range".into());
                }
                self.push(FuncRef);
            }
            0xfc => self.op_fc(r)?,
            other => return Err(format!("wasm: unknown opcode 0x{other:x}")),
        }
        Ok(())
    }

    fn op_fc(&mut self, r: &mut Reader) -> R<()> {
        use ValType::*;
        match r.u32()? {
            0 | 1 => self.unop(F32, I32)?,
            2 | 3 => self.unop(F64, I32)?,
            4 | 5 => self.unop(F32, I64)?,
            6 | 7 => self.unop(F64, I64)?,
            8 => {
                self.data(r.u32()?)?;
                self.memory()?;
                Self::zero_byte(r)?;
                self.pop_all(&[I32; 3])?;
            }
            9 => self.data(r.u32()?)?,
            10 => {
                self.memory()?;
                Self::zero_byte(r)?;
                Self::zero_byte(r)?;
                self.pop_all(&[I32; 3])?;
            }
            11 => {
                self.memory()?;
                Self::zero_byte(r)?;
                self.pop_all(&[I32; 3])?;
            }
            12 => {
                self.elem(r.u32()?)?;
                if self.table(r.u32()?)? != FuncRef {
                    return mismatch();
                }
                self.pop_all(&[I32; 3])?;
            }
            13 => self.elem(r.u32()?)?,
            14 => {
                let dst = self.table(r.u32()?)?;
                if self.table(r.u32()?)? != dst {
                    return mismatch();
                }
                self.pop_all(&[I32; 3])?;
            }
            15 => {
                let t = self.table(r.u32()?)?;
                self.pop(I32)?;
                self.pop(t)?;
                self.push(I32);
            }
            16 => {
                self.table(r.u32()?)?;
                self.push(I32);
            }
            17 => {
                let t = self.table(r.u32()?)?;
                self.pop_all(&[I32, t, I32])?;
            }
            other => return Err(format!("wasm: unknown 0xfc opcode {other}")),
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod tests;
