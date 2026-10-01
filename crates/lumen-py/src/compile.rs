//! AST to bytecode compiler.

use crate::ast::*;
use crate::pyint::{BigInt, PyInt};
use crate::bytecode::*;
use crate::object::*;
use crate::symtable::{self, mangle, Sc, SymTable};
use std::collections::BTreeMap;
use std::rc::Rc;

#[derive(Debug, Clone)]
pub struct CompileError {
    pub msg: String,
    pub line: u32,
}

type Label = u32;

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum ConstKey {
    None,
    True,
    False,
    Ellipsis,
    Int(i64),
    Float(u64),
    Str(Rc<str>),
}

enum FBlock<'a> {
    Loop { top: Label, end: Label, has_iter: bool },
    TryExcept,
    FinallyBody(&'a [Stmt]),
    FinallyHandler,
    ExceptHandler(Option<Rc<str>>),
    With { is_async: bool },
    SavedValue,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UnitKind {
    Module,
    Function,
    Class,
}

struct Unit<'a> {
    scope: usize,
    kind: UnitKind,
    name: Rc<str>,
    qualname: Rc<str>,
    ops: Vec<Op>,
    lines: Vec<u32>,
    labels: Vec<u32>,
    consts: Vec<Value>,
    const_keys: BTreeMap<ConstKey, u32>,
    names: Vec<Obj>,
    name_idx: BTreeMap<Rc<str>, u32>,
    varnames: Vec<Rc<str>>,
    var_idx: BTreeMap<Rc<str>, u32>,
    cells: Vec<Rc<str>>,
    cell_idx: BTreeMap<Rc<str>, u32>,
    ncellvars: usize,
    fblocks: Vec<FBlock<'a>>,
    line: u32,
    private: Option<Rc<str>>,
    is_async: bool,
    is_gen: bool,
}

struct Compiler<'a> {
    st: SymTable,
    units: Vec<Unit<'a>>,
    filename: Rc<str>,
}

type CResult<T> = Result<T, CompileError>;

pub fn compile_module(m: &Module, filename: &str) -> CResult<Rc<Code>> {
    let st = symtable::build(m).map_err(|e| CompileError { msg: e.msg, line: e.line })?;
    let mut c = Compiler { st, units: Vec::new(), filename: filename.into() };
    c.push_unit(0, UnitKind::Module, "<module>".into(), "<module>".into());
    if let Some(d) = docstring(&m.body) {
        c.load_const(Value::str(&d));
        let di = c.name_idx("__doc__");
        c.emit(Op::StoreName(di));
    }
    c.module_body(&m.body)?;
    c.emit(Op::LoadConst(0));
    let none = c.const_idx(Value::None);
    c.set_last(Op::LoadConst(none));
    c.emit(Op::ReturnValue);
    Ok(c.pop_unit(&Arguments::default(), 0))
}

pub fn compile_eval(e: &Expr, filename: &str) -> CResult<Rc<Code>> {
    let st = symtable::build_expr(e).map_err(|e| CompileError { msg: e.msg, line: e.line })?;
    let mut c = Compiler { st, units: Vec::new(), filename: filename.into() };
    c.push_unit(0, UnitKind::Module, "<module>".into(), "<module>".into());
    c.expr(e)?;
    c.emit(Op::ReturnValue);
    Ok(c.pop_unit(&Arguments::default(), 0))
}

fn label_of(op: &Op) -> Option<u32> {
    match op {
        Op::Jump(t)
        | Op::JumpIfFalse(t)
        | Op::JumpIfTrue(t)
        | Op::JumpIfFalseKeep(t)
        | Op::JumpIfTrueKeep(t)
        | Op::ForIter(t)
        | Op::SetupBlock(t)
        | Op::SetupWith(t)
        | Op::WithExceptEnd(t)
        | Op::EndAsyncFor(t) => Some(*t),
        _ => None,
    }
}

fn with_label(op: Op, t: u32) -> Op {
    match op {
        Op::Jump(_) => Op::Jump(t),
        Op::JumpIfFalse(_) => Op::JumpIfFalse(t),
        Op::JumpIfTrue(_) => Op::JumpIfTrue(t),
        Op::JumpIfFalseKeep(_) => Op::JumpIfFalseKeep(t),
        Op::JumpIfTrueKeep(_) => Op::JumpIfTrueKeep(t),
        Op::ForIter(_) => Op::ForIter(t),
        Op::SetupBlock(_) => Op::SetupBlock(t),
        Op::SetupWith(_) => Op::SetupWith(t),
        Op::WithExceptEnd(_) => Op::WithExceptEnd(t),
        Op::EndAsyncFor(_) => Op::EndAsyncFor(t),
        o => o,
    }
}

fn const_value(c: &Constant) -> Value {
    match c {
        Constant::None => Value::None,
        Constant::True => Value::Bool(true),
        Constant::False => Value::Bool(false),
        Constant::Ellipsis => Value::Ellipsis,
        Constant::Int(s) => match s.parse::<i64>() {
            Ok(i) => Value::Int(i),
            Err(_) => Value::big(BigInt::parse_signed(s, 10).unwrap_or_else(BigInt::zero)),
        },
        Constant::Float(f) => Value::Float(*f),
        Constant::Complex(f) => Value::Obj(Object::new(Kind::Complex(0.0, *f))),
        Constant::Str(s) => Value::str(s),
        Constant::Bytes(b) => Value::bytes(b.to_vec()),
    }
}

fn has_annassign(body: &[Stmt]) -> bool {
    body.iter().any(|s| match &s.kind {
        StmtKind::AnnAssign { .. } => true,
        StmtKind::If { body, orelse, .. } | StmtKind::While { body, orelse, .. } | StmtKind::For { body, orelse, .. } => {
            has_annassign(body) || has_annassign(orelse)
        }
        StmtKind::With { body, .. } => has_annassign(body),
        StmtKind::Try { body, handlers, orelse, finalbody, .. } => {
            has_annassign(body)
                || handlers.iter().any(|h| has_annassign(&h.body))
                || has_annassign(orelse)
                || has_annassign(finalbody)
        }
        _ => false,
    })
}

fn docstring(body: &[Stmt]) -> Option<Rc<str>> {
    match body.first() {
        Some(Stmt { kind: StmtKind::Expr(Expr { kind: ExprKind::Constant(Constant::Str(s)), .. }), .. }) => Some(s.clone()),
        _ => None,
    }
}

impl<'a> Compiler<'a> {
    fn u(&mut self) -> &mut Unit<'a> {
        self.units.last_mut().unwrap()
    }

    fn err<T>(&self, msg: &str, line: u32) -> CResult<T> {
        Err(CompileError { msg: msg.into(), line })
    }

    fn push_unit(&mut self, scope: usize, kind: UnitKind, name: Rc<str>, qualname: Rc<str>) {
        let sc = &self.st.scopes[scope];
        let mut varnames: Vec<Rc<str>> = Vec::new();
        let mut cells: Vec<Rc<str>> = Vec::new();
        let mut ncell = 0;
        if kind == UnitKind::Function {
            for p in &sc.params {
                varnames.push(p.clone());
            }
            for (n, s) in &sc.syms {
                if s.scope == Sc::Local && !varnames.contains(n) {
                    varnames.push(n.clone());
                }
            }
        }
        for (n, s) in &sc.syms {
            if s.scope == Sc::Cell {
                cells.push(n.clone());
            }
        }
        if sc.has_class_cell {
            cells.push("__class__".into());
        }
        ncell += cells.len();
        for (n, s) in &sc.syms {
            if s.scope == Sc::Free {
                cells.push(n.clone());
            }
        }
        for n in &sc.extra_free {
            if !cells.contains(n) {
                cells.push(n.clone());
            }
        }
        let var_idx = varnames.iter().enumerate().map(|(i, n)| (n.clone(), i as u32)).collect();
        let cell_idx = cells.iter().enumerate().map(|(i, n)| (n.clone(), i as u32)).collect();
        let private = sc.private.clone();
        let is_async = sc.is_async;
        let is_gen = sc.is_gen;
        let line = sc.line;
        self.units.push(Unit {
            scope,
            kind,
            name,
            qualname,
            ops: Vec::new(),
            lines: Vec::new(),
            labels: Vec::new(),
            consts: Vec::new(),
            const_keys: BTreeMap::new(),
            names: Vec::new(),
            name_idx: BTreeMap::new(),
            varnames,
            var_idx,
            cells,
            cell_idx,
            ncellvars: ncell,
            fblocks: Vec::new(),
            line,
            private,
            is_async,
            is_gen,
        });
    }

    fn pop_unit(&mut self, args: &Arguments, first_line: u32) -> Rc<Code> {
        let mut u = self.units.pop().unwrap();
        let labels = std::mem::take(&mut u.labels);
        for op in u.ops.iter_mut() {
            if let Some(l) = label_of(op) {
                *op = with_label(*op, labels[l as usize]);
            }
        }
        let sc = &self.st.scopes[u.scope];
        let mut flags = 0;
        if args.vararg.is_some() {
            flags |= CO_VARARGS;
        }
        if args.kwarg.is_some() {
            flags |= CO_VARKEYWORDS;
        }
        if u.kind == UnitKind::Class {
            flags |= CO_CLASS_BODY;
        }
        if u.kind == UnitKind::Function {
            match (sc.is_async && !sc.is_comp, sc.is_gen) {
                (true, true) => flags |= CO_ASYNC_GENERATOR,
                (true, false) => flags |= CO_COROUTINE,
                (false, true) => flags |= CO_GENERATOR,
                _ => {}
            }
            if sc.is_comp && sc.is_async && !sc.is_genexp {
                flags |= CO_COROUTINE;
            }
        }
        let mut cell_args = Vec::new();
        for (ci, n) in u.cells.iter().enumerate().take(u.ncellvars) {
            if let Some(&vi) = u.var_idx.get(n) {
                if u.kind == UnitKind::Function && sc.params.contains(n) {
                    cell_args.push((ci as u32, vi));
                }
            }
        }
        let npos = (args.posonlyargs.len() + args.args.len()) as u32;
        let doc = None;
        Rc::new(Code {
            name: u.name,
            qualname: u.qualname,
            filename: self.filename.clone(),
            first_line: if first_line == 0 { 1 } else { first_line },
            ops: u.ops,
            lines: u.lines,
            consts: u.consts,
            names: u.names,
            varnames: u.varnames,
            cellvars: u.cells[..u.ncellvars].to_vec(),
            freevars: u.cells[u.ncellvars..].to_vec(),
            argcount: npos,
            posonly: args.posonlyargs.len() as u32,
            kwonly: args.kwonlyargs.len() as u32,
            flags,
            cell_args,
            doc,
        })
    }

    fn emit(&mut self, op: Op) -> usize {
        let u = self.u();
        u.ops.push(op);
        u.lines.push(u.line);
        u.ops.len() - 1
    }

    fn set_last(&mut self, op: Op) {
        let u = self.u();
        let n = u.ops.len();
        u.ops[n - 1] = op;
    }

    fn new_label(&mut self) -> Label {
        let u = self.u();
        u.labels.push(u32::MAX);
        (u.labels.len() - 1) as u32
    }

    fn bind(&mut self, l: Label) {
        let u = self.u();
        u.labels[l as usize] = u.ops.len() as u32;
    }

    fn const_idx(&mut self, v: Value) -> u32 {
        let key = match &v {
            Value::None => Some(ConstKey::None),
            Value::Bool(true) => Some(ConstKey::True),
            Value::Bool(false) => Some(ConstKey::False),
            Value::Ellipsis => Some(ConstKey::Ellipsis),
            Value::Int(i) => Some(ConstKey::Int(*i)),
            Value::Float(f) => Some(ConstKey::Float(f.to_bits())),
            Value::Obj(o) => match &o.kind {
                Kind::Str(s) => Some(ConstKey::Str(s.s.to_string().into())),
                _ => None,
            },
            _ => None,
        };
        let u = self.u();
        if let Some(k) = &key {
            if let Some(&i) = u.const_keys.get(k) {
                return i;
            }
        }
        u.consts.push(v);
        let i = (u.consts.len() - 1) as u32;
        if let Some(k) = key {
            u.const_keys.insert(k, i);
        }
        i
    }

    fn load_const(&mut self, v: Value) {
        let i = self.const_idx(v);
        self.emit(Op::LoadConst(i));
    }

    fn name_idx(&mut self, name: &str) -> u32 {
        let u = self.u();
        if let Some(&i) = u.name_idx.get(name) {
            return i;
        }
        let v = match Value::str(name) {
            Value::Obj(o) => o,
            _ => unreachable!(),
        };
        u.names.push(v);
        let i = (u.names.len() - 1) as u32;
        u.name_idx.insert(name.into(), i);
        i
    }

    fn mangled(&mut self, name: &str) -> Rc<str> {
        let p = self.u().private.clone();
        mangle(&p, name)
    }

    fn attr_idx(&mut self, name: &str) -> u32 {
        let m = self.mangled(name);
        self.name_idx(&m)
    }

    fn sym_scope(&mut self, name: &str) -> Sc {
        let sid = self.u().scope;
        self.st.scopes[sid].sym_scope(name)
    }

    fn name_load(&mut self, name: &str) {
        let m = self.mangled(name);
        let sc = self.sym_scope(&m);
        let kind = self.u().kind;
        let op = match (kind, sc) {
            (UnitKind::Function, Sc::Local) => Op::LoadFast(self.u().var_idx[&m]),
            (_, Sc::Cell | Sc::Free) => {
                let ci = self.u().cell_idx[&m];
                if kind == UnitKind::Class && sc == Sc::Free {
                    Op::LoadClassDeref(ci)
                } else {
                    Op::LoadDeref(ci)
                }
            }
            (UnitKind::Function, _) | (_, Sc::GlobalExplicit) => Op::LoadGlobal(self.name_idx(&m)),
            _ => Op::LoadName(self.name_idx(&m)),
        };
        self.emit(op);
    }

    fn name_store(&mut self, name: &str) {
        let m = self.mangled(name);
        let sc = self.sym_scope(&m);
        let kind = self.u().kind;
        let op = match (kind, sc) {
            (UnitKind::Function, Sc::Local) => Op::StoreFast(self.u().var_idx[&m]),
            (_, Sc::Cell | Sc::Free) => Op::StoreDeref(self.u().cell_idx[&m]),
            (UnitKind::Function, _) | (_, Sc::GlobalExplicit) => Op::StoreGlobal(self.name_idx(&m)),
            _ => Op::StoreName(self.name_idx(&m)),
        };
        self.emit(op);
    }

    fn name_del(&mut self, name: &str) {
        let m = self.mangled(name);
        let sc = self.sym_scope(&m);
        let kind = self.u().kind;
        let op = match (kind, sc) {
            (UnitKind::Function, Sc::Local) => Op::DelFast(self.u().var_idx[&m]),
            (_, Sc::Cell | Sc::Free) => Op::DelDeref(self.u().cell_idx[&m]),
            (UnitKind::Function, _) | (_, Sc::GlobalExplicit) => Op::DelGlobal(self.name_idx(&m)),
            _ => Op::DelName(self.name_idx(&m)),
        };
        self.emit(op);
    }

    fn module_body(&mut self, body: &'a [Stmt]) -> CResult<()> {
        if has_annassign(body) {
            self.emit(Op::SetupAnnotations);
        }
        self.stmts(body)
    }

    fn stmts(&mut self, body: &'a [Stmt]) -> CResult<()> {
        for s in body {
            self.stmt(s)?;
        }
        Ok(())
    }

    fn in_function(&mut self) -> bool {
        self.units.iter().any(|u| u.kind == UnitKind::Function)
    }

    fn stmt(&mut self, s: &'a Stmt) -> CResult<()> {
        self.u().line = s.pos.line;
        match &s.kind {
            StmtKind::Expr(e) => {
                if matches!(e.kind, ExprKind::Constant(_)) {
                    return Ok(());
                }
                self.expr(e)?;
                self.emit(Op::Pop);
            }
            StmtKind::Pass => {}
            StmtKind::Assign { targets, value } => {
                self.expr(value)?;
                for (i, t) in targets.iter().enumerate() {
                    if i + 1 < targets.len() {
                        self.emit(Op::Dup);
                    }
                    self.store_target(t)?;
                }
            }
            StmtKind::AugAssign { target, op, value } => self.aug_assign(target, *op, value)?,
            StmtKind::AnnAssign { target, annotation, value, simple } => {
                if let Some(v) = value {
                    self.expr(v)?;
                    self.store_target(target)?;
                }
                let kind = self.u().kind;
                if kind != UnitKind::Function {
                    if *simple {
                        if let ExprKind::Name { id, .. } = &target.kind {
                            self.expr(annotation)?;
                            let a = self.name_idx("__annotations__");
                            self.emit(Op::LoadName(a));
                            let m = self.mangled(id);
                            self.load_const(Value::str(&m));
                            self.emit(Op::StoreSubscr);
                        }
                    }
                } else if value.is_none() {
                    if let ExprKind::Attribute { value, .. } | ExprKind::Subscript { value, .. } = &target.kind {
                        self.expr(value)?;
                        self.emit(Op::Pop);
                    }
                }
            }
            StmtKind::Delete(ts) => {
                for t in ts {
                    self.del_target(t)?;
                }
            }
            StmtKind::Return(v) => {
                if !self.in_function() || self.u().kind == UnitKind::Class {
                    return self.err("'return' outside function", s.pos.line);
                }
                match v {
                    Some(v) => self.expr(v)?,
                    None => self.load_const(Value::None),
                }
                self.unwind(0, true)?;
                self.emit(Op::ReturnValue);
            }
            StmtKind::If { test, body, orelse } => {
                let l_else = self.new_label();
                let l_end = self.new_label();
                self.jump_if(test, l_else, false)?;
                self.stmts(body)?;
                if !orelse.is_empty() {
                    self.emit(Op::Jump(l_end));
                }
                self.bind(l_else);
                self.stmts(orelse)?;
                self.bind(l_end);
            }
            StmtKind::While { test, body, orelse } => {
                let l_top = self.new_label();
                let l_else = self.new_label();
                let l_end = self.new_label();
                self.bind(l_top);
                let always = matches!(&test.kind, ExprKind::Constant(Constant::True))
                    || matches!(&test.kind, ExprKind::Constant(Constant::Int(s)) if &**s != "0");
                if !always {
                    self.jump_if(test, l_else, false)?;
                }
                self.u().fblocks.push(FBlock::Loop { top: l_top, end: l_end, has_iter: false });
                self.stmts(body)?;
                self.u().fblocks.pop();
                self.u().line = s.pos.line;
                self.emit(Op::Jump(l_top));
                self.bind(l_else);
                self.stmts(orelse)?;
                self.bind(l_end);
            }
            StmtKind::For { target, iter, body, orelse, is_async } => {
                self.for_stmt(target, iter, body, orelse, *is_async)?;
            }
            StmtKind::Break => {
                let idx = self.find_loop(s.pos.line, "'break' outside loop")?;
                self.unwind(idx + 1, false)?;
                if let FBlock::Loop { end, has_iter, .. } = self.u().fblocks[idx] {
                    if has_iter {
                        self.emit(Op::Pop);
                    }
                    self.emit(Op::Jump(end));
                }
            }
            StmtKind::Continue => {
                let idx = self.find_loop(s.pos.line, "'continue' not properly in loop")?;
                self.unwind(idx + 1, false)?;
                if let FBlock::Loop { top, .. } = self.u().fblocks[idx] {
                    self.emit(Op::Jump(top));
                }
            }
            StmtKind::Raise { exc, cause } => {
                let mut n = 0;
                if let Some(e) = exc {
                    self.expr(e)?;
                    n = 1;
                    if let Some(c) = cause {
                        self.expr(c)?;
                        n = 2;
                    }
                }
                self.emit(Op::Raise(n));
            }
            StmtKind::Assert { test, msg } => {
                let l_ok = self.new_label();
                self.jump_if(test, l_ok, true)?;
                self.emit(Op::LoadAssertionError);
                if let Some(m) = msg {
                    self.expr(m)?;
                    self.emit(Op::Call(1));
                }
                self.emit(Op::Raise(1));
                self.bind(l_ok);
            }
            StmtKind::Global(_) | StmtKind::Nonlocal(_) => {}
            StmtKind::Import(names) => {
                for a in names {
                    self.load_const(Value::Int(0));
                    match &a.asname {
                        Some(_) => self.load_const(Value::tuple(Vec::new())),
                        None => self.load_const(Value::None),
                    }
                    let ni = self.name_idx(&a.name);
                    self.emit(Op::ImportName(ni));
                    match &a.asname {
                        Some(n) => self.name_store(n),
                        None => {
                            let top = a.name.split('.').next().unwrap_or("").to_string();
                            self.name_store(&top);
                        }
                    }
                }
            }
            StmtKind::ImportFrom { module, names, level } => {
                self.load_const(Value::Int(*level as i64));
                let fl: Vec<Value> = names.iter().map(|a| Value::str(&a.name)).collect();
                self.load_const(Value::tuple(fl));
                let ni = self.name_idx(module.as_deref().unwrap_or(""));
                self.emit(Op::ImportName(ni));
                if names.len() == 1 && &*names[0].name == "*" {
                    self.emit(Op::ImportStar);
                } else {
                    for a in names {
                        let ai = self.name_idx(&a.name);
                        self.emit(Op::ImportFrom(ai));
                        let n = a.asname.clone().unwrap_or_else(|| a.name.clone());
                        self.name_store(&n);
                    }
                    self.emit(Op::Pop);
                }
            }
            StmtKind::FunctionDef(f) => self.function_def(f, s.pos.line)?,
            StmtKind::ClassDef(c) => self.class_def(c, s.pos.line)?,
            StmtKind::With { items, body, is_async } => self.with_stmt(items, body, *is_async)?,
            StmtKind::Try { body, handlers, orelse, finalbody, is_star } => {
                if finalbody.is_empty() {
                    self.try_except(body, handlers, orelse, *is_star)?;
                } else {
                    self.try_finally(body, handlers, orelse, finalbody, *is_star)?;
                }
            }
            StmtKind::Match { subject, cases } => self.match_stmt(subject, cases)?,
        }
        Ok(())
    }

    fn find_loop(&mut self, line: u32, msg: &str) -> CResult<usize> {
        let u = self.u();
        for (i, b) in u.fblocks.iter().enumerate().rev() {
            if matches!(b, FBlock::Loop { .. }) {
                return Ok(i);
            }
        }
        Err(CompileError { msg: msg.into(), line })
    }

    /// Emits the cleanup code for leaving every block above index `down_to`.
    fn unwind(&mut self, down_to: usize, has_value: bool) -> CResult<()> {
        let mut popped: Vec<FBlock<'a>> = Vec::new();
        while self.u().fblocks.len() > down_to {
            let fb = self.u().fblocks.pop().unwrap();
            match &fb {
                FBlock::Loop { has_iter, .. } => {
                    if *has_iter && has_value {
                        self.emit(Op::Swap(2));
                        self.emit(Op::Pop);
                    }
                }
                FBlock::TryExcept => {
                    self.emit(Op::PopBlock);
                }
                FBlock::FinallyBody(body) => {
                    self.emit(Op::PopBlock);
                    let body: &'a [Stmt] = body;
                    if has_value {
                        self.u().fblocks.push(FBlock::SavedValue);
                    }
                    self.stmts(body)?;
                    if has_value {
                        self.u().fblocks.pop();
                    }
                }
                FBlock::SavedValue => {
                    if has_value {
                        self.emit(Op::Swap(2));
                    }
                    self.emit(Op::Pop);
                }
                FBlock::FinallyHandler => {
                    self.emit(Op::PopBlock);
                    self.emit(Op::UnwindExc(has_value as u32));
                }
                FBlock::ExceptHandler(name) => {
                    self.emit(Op::PopBlock);
                    if let Some(n) = name {
                        self.load_const(Value::None);
                        self.name_store(n);
                        self.name_del(n);
                    }
                    self.emit(Op::UnwindExc(has_value as u32));
                }
                FBlock::With { is_async } => {
                    self.emit(Op::PopBlock);
                    if has_value {
                        self.emit(Op::Swap(2));
                    }
                    self.emit(Op::WithCallExit);
                    if *is_async {
                        self.emit(Op::GetAwaitable);
                        self.load_const(Value::None);
                        self.emit(Op::YieldFrom);
                    }
                    self.emit(Op::Pop);
                }
            }
            popped.push(fb);
        }
        while let Some(fb) = popped.pop() {
            self.u().fblocks.push(fb);
        }
        Ok(())
    }

    fn for_stmt(&mut self, target: &'a Expr, iter: &'a Expr, body: &'a [Stmt], orelse: &'a [Stmt], is_async: bool) -> CResult<()> {
        let l_top = self.new_label();
        let l_else = self.new_label();
        let l_end = self.new_label();
        self.expr(iter)?;
        if is_async {
            self.emit(Op::GetAIter);
            let l_stop = self.new_label();
            self.bind(l_top);
            self.emit(Op::SetupBlock(l_stop));
            self.emit(Op::GetANext);
            self.load_const(Value::None);
            self.emit(Op::YieldFrom);
            self.emit(Op::PopBlock);
            self.store_target(target)?;
            self.u().fblocks.push(FBlock::Loop { top: l_top, end: l_end, has_iter: true });
            self.stmts(body)?;
            self.u().fblocks.pop();
            self.emit(Op::Jump(l_top));
            self.bind(l_stop);
            self.emit(Op::EndAsyncFor(l_else));
        } else {
            self.emit(Op::GetIter);
            self.bind(l_top);
            self.emit(Op::ForIter(l_else));
            self.store_target(target)?;
            self.u().fblocks.push(FBlock::Loop { top: l_top, end: l_end, has_iter: true });
            self.stmts(body)?;
            self.u().fblocks.pop();
            self.emit(Op::Jump(l_top));
        }
        self.bind(l_else);
        self.stmts(orelse)?;
        self.bind(l_end);
        Ok(())
    }

    fn try_except(&mut self, body: &'a [Stmt], handlers: &'a [ExceptHandler], orelse: &'a [Stmt], is_star: bool) -> CResult<()> {
        if is_star {
            return self.try_star(body, handlers, orelse);
        }
        let l_handler = self.new_label();
        let l_else = self.new_label();
        let l_end = self.new_label();
        let l_cleanup = self.new_label();
        self.emit(Op::SetupBlock(l_handler));
        self.u().fblocks.push(FBlock::TryExcept);
        self.stmts(body)?;
        self.u().fblocks.pop();
        self.emit(Op::PopBlock);
        self.emit(Op::Jump(l_else));
        self.bind(l_handler);
        self.emit(Op::PushExcInfo);
        self.emit(Op::SetupBlock(l_cleanup));
        for h in handlers {
            self.u().line = h.pos.line;
            let l_next = self.new_label();
            if let Some(t) = &h.typ {
                self.expr(t)?;
                self.emit(Op::CheckExcMatch);
                self.emit(Op::JumpIfFalse(l_next));
            }
            if let Some(n) = &h.name {
                self.emit(Op::Dup);
                self.name_store(n);
            }
            self.u().fblocks.push(FBlock::ExceptHandler(h.name.clone()));
            self.stmts(&h.body)?;
            self.u().fblocks.pop();
            self.emit(Op::PopBlock);
            if let Some(n) = &h.name {
                self.load_const(Value::None);
                self.name_store(n);
                self.name_del(n);
            }
            self.emit(Op::Pop);
            self.emit(Op::PopExcInfo(0));
            self.emit(Op::Jump(l_end));
            self.bind(l_next);
        }
        self.emit(Op::PopBlock);
        self.emit(Op::EndFinally);
        self.bind(l_cleanup);
        self.emit(Op::CleanupReraise);
        self.bind(l_else);
        self.stmts(orelse)?;
        self.bind(l_end);
        Ok(())
    }

    fn try_star(&mut self, body: &'a [Stmt], handlers: &'a [ExceptHandler], orelse: &'a [Stmt]) -> CResult<()> {
        let l_handler = self.new_label();
        let l_else = self.new_label();
        let l_end = self.new_label();
        let l_cleanup = self.new_label();
        let l_ok = self.new_label();
        self.emit(Op::SetupBlock(l_handler));
        self.u().fblocks.push(FBlock::TryExcept);
        self.stmts(body)?;
        self.u().fblocks.pop();
        self.emit(Op::PopBlock);
        self.emit(Op::Jump(l_else));
        self.bind(l_handler);
        self.emit(Op::PushExcInfo);
        self.emit(Op::SetupBlock(l_cleanup));
        self.emit(Op::ExcStarWrap);
        for h in handlers {
            self.u().line = h.pos.line;
            let l_next = self.new_label();
            if let Some(t) = &h.typ {
                self.expr(t)?;
            } else {
                return self.err("except* requires an exception type", h.pos.line);
            }
            self.emit(Op::ExcStarSplit);
            self.emit(Op::Dup);
            self.emit(Op::JumpIfFalse(l_next));
            if let Some(n) = &h.name {
                self.emit(Op::Dup);
                self.name_store(n);
            }
            self.stmts(&h.body)?;
            if let Some(n) = &h.name {
                self.load_const(Value::None);
                self.name_store(n);
                self.name_del(n);
            }
            self.bind(l_next);
            self.emit(Op::Pop);
        }
        self.emit(Op::ExcStarEnd);
        self.emit(Op::Dup);
        self.emit(Op::JumpIfFalse(l_ok));
        self.emit(Op::PopBlock);
        self.emit(Op::EndFinally);
        self.bind(l_ok);
        self.emit(Op::PopBlock);
        self.emit(Op::Pop);
        self.emit(Op::PopExcInfo(0));
        self.emit(Op::Jump(l_end));
        self.bind(l_cleanup);
        self.emit(Op::CleanupReraise);
        self.bind(l_else);
        self.stmts(orelse)?;
        self.bind(l_end);
        Ok(())
    }

    fn try_finally(
        &mut self,
        body: &'a [Stmt],
        handlers: &'a [ExceptHandler],
        orelse: &'a [Stmt],
        finalbody: &'a [Stmt],
        is_star: bool,
    ) -> CResult<()> {
        let l_exc = self.new_label();
        let l_cleanup = self.new_label();
        let l_end = self.new_label();
        self.emit(Op::SetupBlock(l_exc));
        self.u().fblocks.push(FBlock::FinallyBody(finalbody));
        if handlers.is_empty() {
            self.stmts(body)?;
        } else {
            self.try_except(body, handlers, orelse, is_star)?;
        }
        self.u().fblocks.pop();
        self.emit(Op::PopBlock);
        self.stmts(finalbody)?;
        self.emit(Op::Jump(l_end));
        self.bind(l_exc);
        self.emit(Op::PushExcInfo);
        self.emit(Op::SetupBlock(l_cleanup));
        self.u().fblocks.push(FBlock::FinallyHandler);
        self.stmts(finalbody)?;
        self.u().fblocks.pop();
        self.emit(Op::PopBlock);
        self.emit(Op::EndFinally);
        self.bind(l_cleanup);
        self.emit(Op::CleanupReraise);
        self.bind(l_end);
        Ok(())
    }

    fn with_stmt(&mut self, items: &'a [WithItem], body: &'a [Stmt], is_async: bool) -> CResult<()> {
        let item = &items[0];
        let l_exc = self.new_label();
        let l_end = self.new_label();
        self.expr(&item.context_expr)?;
        if is_async {
            self.emit(Op::BeforeAsyncWith);
            self.emit(Op::GetAwaitable);
            self.load_const(Value::None);
            self.emit(Op::YieldFrom);
            match &item.optional_vars {
                Some(t) => self.store_target(t)?,
                None => {
                    self.emit(Op::Pop);
                }
            }
            self.emit(Op::SetupBlock(l_exc));
        } else {
            self.emit(Op::SetupWith(l_exc));
            match &item.optional_vars {
                Some(t) => self.store_target(t)?,
                None => {
                    self.emit(Op::Pop);
                }
            }
        }
        self.u().fblocks.push(FBlock::With { is_async });
        if items.len() > 1 {
            self.with_stmt(&items[1..], body, is_async)?;
        } else {
            self.stmts(body)?;
        }
        self.u().fblocks.pop();
        self.emit(Op::PopBlock);
        self.emit(Op::WithCallExit);
        if is_async {
            self.emit(Op::GetAwaitable);
            self.load_const(Value::None);
            self.emit(Op::YieldFrom);
        }
        self.emit(Op::Pop);
        self.emit(Op::Jump(l_end));
        self.bind(l_exc);
        self.emit(Op::WithExceptStart);
        if is_async {
            self.emit(Op::GetAwaitable);
            self.load_const(Value::None);
            self.emit(Op::YieldFrom);
        }
        self.emit(Op::WithExceptEnd(l_end));
        self.bind(l_end);
        Ok(())
    }

    fn aug_assign(&mut self, target: &'a Expr, op: BinOp, value: &'a Expr) -> CResult<()> {
        match &target.kind {
            ExprKind::Name { id, .. } => {
                self.name_load(id);
                self.expr(value)?;
                self.u().line = target.pos.line;
                self.emit(Op::Inplace(op));
                self.name_store(id);
            }
            ExprKind::Attribute { value: obj, attr, .. } => {
                self.expr(obj)?;
                self.emit(Op::Dup);
                let ai = self.attr_idx(attr);
                self.emit(Op::LoadAttr(ai));
                self.expr(value)?;
                self.u().line = target.pos.line;
                self.emit(Op::Inplace(op));
                self.emit(Op::Swap(2));
                self.emit(Op::StoreAttr(ai));
            }
            ExprKind::Subscript { value: obj, slice, .. } => {
                self.expr(obj)?;
                self.expr(slice)?;
                self.emit(Op::DupTwo);
                self.emit(Op::Subscr);
                self.expr(value)?;
                self.u().line = target.pos.line;
                self.emit(Op::Inplace(op));
                self.emit(Op::Rot3);
                self.emit(Op::StoreSubscr);
            }
            _ => return self.err("illegal expression for augmented assignment", target.pos.line),
        }
        Ok(())
    }

    fn store_target(&mut self, t: &'a Expr) -> CResult<()> {
        self.u().line = t.pos.line;
        match &t.kind {
            ExprKind::Name { id, .. } => self.name_store(id),
            ExprKind::Attribute { value, attr, .. } => {
                self.expr(value)?;
                let ai = self.attr_idx(attr);
                self.emit(Op::StoreAttr(ai));
            }
            ExprKind::Subscript { value, slice, .. } => {
                self.expr(value)?;
                self.expr(slice)?;
                self.emit(Op::StoreSubscr);
            }
            ExprKind::Tuple { elts, .. } | ExprKind::List { elts, .. } => {
                let star = elts.iter().position(|e| matches!(e.kind, ExprKind::Starred { .. }));
                match star {
                    None => {
                        self.emit(Op::UnpackSequence(elts.len() as u32));
                    }
                    Some(i) => {
                        self.emit(Op::UnpackEx(i as u32, (elts.len() - i - 1) as u32));
                    }
                }
                for e in elts {
                    match &e.kind {
                        ExprKind::Starred { value, .. } => self.store_target(value)?,
                        _ => self.store_target(e)?,
                    }
                }
            }
            ExprKind::Starred { value, .. } => self.store_target(value)?,
            _ => return self.err("cannot assign to expression", t.pos.line),
        }
        Ok(())
    }

    fn del_target(&mut self, t: &'a Expr) -> CResult<()> {
        self.u().line = t.pos.line;
        match &t.kind {
            ExprKind::Name { id, .. } => self.name_del(id),
            ExprKind::Attribute { value, attr, .. } => {
                self.expr(value)?;
                let ai = self.attr_idx(attr);
                self.emit(Op::DelAttr(ai));
            }
            ExprKind::Subscript { value, slice, .. } => {
                self.expr(value)?;
                self.expr(slice)?;
                self.emit(Op::DelSubscr);
            }
            ExprKind::Tuple { elts, .. } | ExprKind::List { elts, .. } => {
                for e in elts {
                    self.del_target(e)?;
                }
            }
            _ => return self.err("cannot delete expression", t.pos.line),
        }
        Ok(())
    }

    /// Jumps to `label` when the truth value of `e` equals `sense`; falls through otherwise.
    fn jump_if(&mut self, e: &'a Expr, label: Label, sense: bool) -> CResult<()> {
        match &e.kind {
            ExprKind::UnaryOp { op: UnaryOp::Not, operand } => return self.jump_if(operand, label, !sense),
            ExprKind::BoolOp { op, values } => {
                let is_and = *op == BoolOp::And;
                if is_and != sense {
                    for v in values {
                        self.jump_if(v, label, sense)?;
                    }
                } else {
                    let end = self.new_label();
                    for v in &values[..values.len() - 1] {
                        self.jump_if(v, end, !sense)?;
                    }
                    self.jump_if(&values[values.len() - 1], label, sense)?;
                    self.bind(end);
                }
                return Ok(());
            }
            ExprKind::Constant(Constant::True) if !sense => return Ok(()),
            _ => {}
        }
        self.expr(e)?;
        self.u().line = e.pos.line;
        self.emit(if sense { Op::JumpIfTrue(label) } else { Op::JumpIfFalse(label) });
        Ok(())
    }

    fn expr(&mut self, e: &'a Expr) -> CResult<()> {
        self.u().line = e.pos.line;
        match &e.kind {
            ExprKind::Constant(c) => {
                let v = const_value(c);
                self.load_const(v);
            }
            ExprKind::Name { id, ctx } => match ctx {
                Ctx::Load => self.name_load(id),
                Ctx::Store => self.name_store(id),
                Ctx::Del => self.name_del(id),
            },
            ExprKind::BoolOp { op, values } => {
                let end = self.new_label();
                for (i, v) in values.iter().enumerate() {
                    self.expr(v)?;
                    if i + 1 < values.len() {
                        self.emit(if *op == BoolOp::And { Op::JumpIfFalseKeep(end) } else { Op::JumpIfTrueKeep(end) });
                    }
                }
                self.bind(end);
            }
            ExprKind::NamedExpr { target, value } => {
                self.expr(value)?;
                self.emit(Op::Dup);
                self.store_target(target)?;
            }
            ExprKind::BinOp { left, op, right } => {
                self.expr(left)?;
                self.expr(right)?;
                self.u().line = e.pos.line;
                self.emit(Op::Binary(*op));
            }
            ExprKind::UnaryOp { op, operand } => {
                if let (UnaryOp::USub, ExprKind::Constant(c)) = (op, &operand.kind) {
                    match c {
                        Constant::Int(s) => {
                            let s2: String = format!("-{}", s);
                            let v = match s2.parse::<i64>() {
                                Ok(i) => Value::Int(i),
                                Err(_) => Value::big(BigInt::parse_signed(&s2, 10).unwrap_or_else(BigInt::zero)),
                            };
                            self.load_const(v);
                            return Ok(());
                        }
                        Constant::Float(f) => {
                            self.load_const(Value::Float(-*f));
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                self.expr(operand)?;
                self.u().line = e.pos.line;
                self.emit(Op::Unary(match op {
                    UnaryOp::Invert => crate::bytecode::UnOp::Invert,
                    UnaryOp::Not => crate::bytecode::UnOp::Not,
                    UnaryOp::UAdd => crate::bytecode::UnOp::Pos,
                    UnaryOp::USub => crate::bytecode::UnOp::Neg,
                }));
            }
            ExprKind::Lambda { args, body } => {
                let key = e as *const Expr as usize;
                self.make_function("<lambda>".into(), args, None, FnBody::Expr(body), key, false, e.pos.line)?;
            }
            ExprKind::IfExp { test, body, orelse } => {
                let l_else = self.new_label();
                let l_end = self.new_label();
                self.jump_if(test, l_else, false)?;
                self.expr(body)?;
                self.emit(Op::Jump(l_end));
                self.bind(l_else);
                self.expr(orelse)?;
                self.bind(l_end);
            }
            ExprKind::Dict { keys, values } => self.dict_display(keys, values)?,
            ExprKind::Set(elts) => {
                if elts.iter().any(|x| matches!(x.kind, ExprKind::Starred { .. })) {
                    self.emit(Op::BuildSet(0));
                    for x in elts {
                        match &x.kind {
                            ExprKind::Starred { value, .. } => {
                                self.expr(value)?;
                                self.emit(Op::SetUpdate(1));
                            }
                            _ => {
                                self.expr(x)?;
                                self.emit(Op::SetAdd(1));
                            }
                        }
                    }
                } else {
                    for x in elts {
                        self.expr(x)?;
                    }
                    let is_const = |x: &Expr| match &x.kind {
                        ExprKind::Constant(_) => true,
                        ExprKind::UnaryOp { op: UnaryOp::USub, operand } => {
                            matches!(operand.kind, ExprKind::Constant(Constant::Int(_) | Constant::Float(_)))
                        }
                        _ => false,
                    };
                    if elts.len() > 2 && elts.iter().all(is_const) {
                        self.emit(Op::BuildSetConst(elts.len() as u32));
                    } else {
                        self.emit(Op::BuildSet(elts.len() as u32));
                    }
                }
            }
            ExprKind::List { elts, .. } => self.seq_display(elts, false)?,
            ExprKind::Tuple { elts, .. } => self.seq_display(elts, true)?,
            ExprKind::ListComp { elt, generators } => self.comprehension(e, generators, &[elt], CompKind::List)?,
            ExprKind::SetComp { elt, generators } => self.comprehension(e, generators, &[elt], CompKind::Set)?,
            ExprKind::DictComp { key, value, generators } => {
                self.comprehension(e, generators, &[key, value], CompKind::Dict)?
            }
            ExprKind::GeneratorExp { elt, generators } => self.comprehension(e, generators, &[elt], CompKind::Gen)?,
            ExprKind::Await(v) => {
                self.expr(v)?;
                self.u().line = e.pos.line;
                self.emit(Op::GetAwaitable);
                self.load_const(Value::None);
                self.emit(Op::YieldFrom);
            }
            ExprKind::Yield(v) => {
                match v {
                    Some(v) => self.expr(v)?,
                    None => self.load_const(Value::None),
                }
                self.u().line = e.pos.line;
                let (is_async, is_gen) = {
                    let u = self.u();
                    (u.is_async, u.is_gen)
                };
                if is_async && is_gen {
                    self.emit(Op::AsyncGenWrap);
                }
                self.emit(Op::YieldValue);
            }
            ExprKind::YieldFrom(v) => {
                self.expr(v)?;
                self.u().line = e.pos.line;
                self.emit(Op::GetYieldFromIter);
                self.load_const(Value::None);
                self.emit(Op::YieldFrom);
            }
            ExprKind::Compare { left, ops, comparators } => {
                self.expr(left)?;
                if ops.len() == 1 {
                    self.expr(&comparators[0])?;
                    self.u().line = e.pos.line;
                    self.emit(Op::Compare(ops[0]));
                } else {
                    let cleanup = self.new_label();
                    let end = self.new_label();
                    for (i, op) in ops.iter().enumerate() {
                        self.expr(&comparators[i])?;
                        self.u().line = e.pos.line;
                        if i + 1 < ops.len() {
                            self.emit(Op::Dup);
                            self.emit(Op::Rot3);
                            self.emit(Op::Compare(*op));
                            self.emit(Op::JumpIfFalseKeep(cleanup));
                        } else {
                            self.emit(Op::Compare(*op));
                        }
                    }
                    self.emit(Op::Jump(end));
                    self.bind(cleanup);
                    self.emit(Op::Swap(2));
                    self.emit(Op::Pop);
                    self.bind(end);
                }
            }
            ExprKind::Call { func, args, keywords } => self.call(e, func, args, keywords)?,
            ExprKind::JoinedStr(parts) => {
                for p in parts {
                    self.expr(p)?;
                }
                if parts.is_empty() {
                    self.load_const(Value::str(""));
                } else if parts.len() > 1 {
                    self.emit(Op::BuildString(parts.len() as u32));
                } else if !matches!(parts[0].kind, ExprKind::FormattedValue { .. }) {
                } else {
                    self.emit(Op::BuildString(1));
                }
            }
            ExprKind::FormattedValue { value, conversion, format_spec } => {
                self.expr(value)?;
                if let Some(spec) = format_spec {
                    self.expr(spec)?;
                }
                let conv = match conversion {
                    None => 0,
                    Some('s') => 1,
                    Some('r') => 2,
                    Some(_) => 3,
                };
                self.u().line = e.pos.line;
                self.emit(Op::FormatValue(conv, format_spec.is_some()));
            }
            ExprKind::Attribute { value, attr, ctx } => {
                self.expr(value)?;
                let ai = self.attr_idx(attr);
                self.u().line = e.pos.line;
                match ctx {
                    Ctx::Load => self.emit(Op::LoadAttr(ai)),
                    Ctx::Del => self.emit(Op::DelAttr(ai)),
                    Ctx::Store => self.emit(Op::StoreAttr(ai)),
                };
            }
            ExprKind::Subscript { value, slice, .. } => {
                self.expr(value)?;
                self.expr(slice)?;
                self.u().line = e.pos.line;
                self.emit(Op::Subscr);
            }
            ExprKind::Starred { value, .. } => {
                self.expr(value)?;
            }
            ExprKind::Slice { lower, upper, step } => {
                for part in [lower, upper] {
                    match part {
                        Some(x) => self.expr(x)?,
                        None => self.load_const(Value::None),
                    }
                }
                match step {
                    Some(x) => {
                        self.expr(x)?;
                        self.emit(Op::BuildSlice(3));
                    }
                    None => {
                        self.emit(Op::BuildSlice(2));
                    }
                }
            }
        }
        Ok(())
    }

    fn seq_display(&mut self, elts: &'a [Expr], tuple: bool) -> CResult<()> {
        if elts.iter().any(|x| matches!(x.kind, ExprKind::Starred { .. })) {
            self.emit(Op::BuildList(0));
            for x in elts {
                match &x.kind {
                    ExprKind::Starred { value, .. } => {
                        self.expr(value)?;
                        self.emit(Op::ListExtend(1));
                    }
                    _ => {
                        self.expr(x)?;
                        self.emit(Op::ListAppend(1));
                    }
                }
            }
            if tuple {
                self.emit(Op::ListToTuple);
            }
        } else {
            for x in elts {
                self.expr(x)?;
            }
            self.emit(if tuple { Op::BuildTuple(elts.len() as u32) } else { Op::BuildList(elts.len() as u32) });
        }
        Ok(())
    }

    fn dict_display(&mut self, keys: &'a [Option<Expr>], values: &'a [Expr]) -> CResult<()> {
        if keys.iter().all(|k| k.is_some()) {
            for (k, v) in keys.iter().zip(values.iter()) {
                self.expr(k.as_ref().unwrap())?;
                self.expr(v)?;
            }
            self.emit(Op::BuildMap(keys.len() as u32));
            return Ok(());
        }
        self.emit(Op::BuildMap(0));
        let mut pending = 0;
        for (k, v) in keys.iter().zip(values.iter()) {
            match k {
                Some(k) => {
                    self.expr(k)?;
                    self.expr(v)?;
                    pending += 1;
                }
                None => {
                    if pending > 0 {
                        self.emit(Op::BuildMap(pending));
                        self.emit(Op::DictUpdate(1));
                        pending = 0;
                    }
                    self.expr(v)?;
                    self.emit(Op::DictUpdate(1));
                }
            }
        }
        if pending > 0 {
            self.emit(Op::BuildMap(pending));
            self.emit(Op::DictUpdate(1));
        }
        Ok(())
    }

    fn call(&mut self, e: &'a Expr, func: &'a Expr, args: &'a [Expr], keywords: &'a [Keyword]) -> CResult<()> {
        self.expr(func)?;
        let has_star = args.iter().any(|a| matches!(a.kind, ExprKind::Starred { .. }));
        let has_dstar = keywords.iter().any(|k| k.arg.is_none());
        if !has_star && !has_dstar {
            for a in args {
                self.expr(a)?;
            }
            self.u().line = e.pos.line;
            if keywords.is_empty() {
                self.emit(Op::Call(args.len() as u32));
            } else {
                for k in keywords {
                    self.expr(&k.value)?;
                }
                let names: Vec<Value> = keywords.iter().map(|k| Value::str(k.arg.as_ref().unwrap())).collect();
                self.load_const(Value::tuple(names));
                self.u().line = e.pos.line;
                self.emit(Op::CallKw((args.len() + keywords.len()) as u32));
            }
            return Ok(());
        }
        self.seq_display(args, true)?;
        if !keywords.is_empty() {
            let mut pending = 0u32;
            self.emit(Op::BuildMap(0));
            for k in keywords {
                match &k.arg {
                    Some(n) => {
                        self.load_const(Value::str(n));
                        self.expr(&k.value)?;
                        pending += 1;
                    }
                    None => {
                        if pending > 0 {
                            self.emit(Op::BuildMap(pending));
                            self.emit(Op::KwMerge);
                            pending = 0;
                        }
                        self.expr(&k.value)?;
                        self.emit(Op::KwMerge);
                    }
                }
            }
            if pending > 0 {
                self.emit(Op::BuildMap(pending));
                self.emit(Op::KwMerge);
            }
        }
        self.u().line = e.pos.line;
        self.emit(Op::CallEx(!keywords.is_empty() as u32));
        Ok(())
    }

    fn comprehension(&mut self, e: &'a Expr, gens: &'a [Comprehension], elts: &[&'a Expr], kind: CompKind) -> CResult<()> {
        let key = e as *const Expr as usize;
        let sid = self.st.ids[&key];
        let name: Rc<str> = match kind {
            CompKind::List => "<listcomp>",
            CompKind::Set => "<setcomp>",
            CompKind::Dict => "<dictcomp>",
            CompKind::Gen => "<genexpr>",
        }
        .into();
        let qual = self.qualname_for(&name);
        let line = e.pos.line;
        self.push_unit(sid, UnitKind::Function, name, qual);
        self.u().line = line;
        match kind {
            CompKind::List => {
                self.emit(Op::BuildList(0));
            }
            CompKind::Set => {
                self.emit(Op::BuildSet(0));
            }
            CompKind::Dict => {
                self.emit(Op::BuildMap(0));
            }
            CompKind::Gen => {}
        }
        self.comp_loops(gens, 0, elts, kind)?;
        if kind == CompKind::Gen {
            self.load_const(Value::None);
        }
        self.emit(Op::ReturnValue);
        let args = Arguments {
            args: vec![Arg { pos: e.pos, arg: ".0".into(), annotation: None }],
            ..Default::default()
        };
        let code = self.pop_unit(&args, line);
        self.emit_closure_function(code, 0)?;
        self.expr(&gens[0].iter)?;
        self.u().line = line;
        if gens[0].is_async {
            self.emit(Op::GetAIter);
        } else {
            self.emit(Op::GetIter);
        }
        self.emit(Op::Call(1));
        if gens[0].is_async && kind != CompKind::Gen || self.st.scopes[sid].is_async && kind != CompKind::Gen {
            self.emit(Op::GetAwaitable);
            self.load_const(Value::None);
            self.emit(Op::YieldFrom);
        }
        Ok(())
    }

    fn comp_loops(&mut self, gens: &'a [Comprehension], i: usize, elts: &[&'a Expr], kind: CompKind) -> CResult<()> {
        let g = &gens[i];
        let l_top = self.new_label();
        let l_end = self.new_label();
        if i == 0 {
            self.emit(Op::LoadFast(0));
        } else {
            self.expr(&g.iter)?;
            self.emit(if g.is_async { Op::GetAIter } else { Op::GetIter });
        }
        if g.is_async {
            let l_stop = self.new_label();
            self.bind(l_top);
            self.emit(Op::SetupBlock(l_stop));
            self.emit(Op::GetANext);
            self.load_const(Value::None);
            self.emit(Op::YieldFrom);
            self.emit(Op::PopBlock);
            self.store_target(&g.target)?;
            for c in &g.ifs {
                self.jump_if(c, l_top, false)?;
            }
            self.comp_inner(gens, i, elts, kind)?;
            self.emit(Op::Jump(l_top));
            self.bind(l_stop);
            self.emit(Op::EndAsyncFor(l_end));
            self.bind(l_end);
            return Ok(());
        }
        self.bind(l_top);
        self.emit(Op::ForIter(l_end));
        self.store_target(&g.target)?;
        for c in &g.ifs {
            self.jump_if(c, l_top, false)?;
        }
        self.comp_inner(gens, i, elts, kind)?;
        self.emit(Op::Jump(l_top));
        self.bind(l_end);
        Ok(())
    }

    fn comp_inner(&mut self, gens: &'a [Comprehension], i: usize, elts: &[&'a Expr], kind: CompKind) -> CResult<()> {
        if i + 1 < gens.len() {
            return self.comp_loops(gens, i + 1, elts, kind);
        }
        let depth = gens.len() as u32 + 1;
        match kind {
            CompKind::List => {
                self.expr(elts[0])?;
                self.emit(Op::ListAppend(depth));
            }
            CompKind::Set => {
                self.expr(elts[0])?;
                self.emit(Op::SetAdd(depth));
            }
            CompKind::Dict => {
                self.expr(elts[0])?;
                self.expr(elts[1])?;
                self.emit(Op::MapAdd(depth));
            }
            CompKind::Gen => {
                self.expr(elts[0])?;
                let (is_async, is_gen) = {
                    let u = self.u();
                    (u.is_async, u.is_gen)
                };
                if is_async && is_gen {
                    self.emit(Op::AsyncGenWrap);
                }
                self.emit(Op::YieldValue);
                self.emit(Op::Pop);
            }
        }
        Ok(())
    }

    fn qualname_for(&mut self, name: &str) -> Rc<str> {
        let p = self.units.last().unwrap();
        match p.kind {
            UnitKind::Module => name.into(),
            UnitKind::Class => format!("{}.{}", p.qualname, name).into(),
            UnitKind::Function => format!("{}.<locals>.{}", p.qualname, name).into(),
        }
    }

    /// Pushes a function object for `code` (closure included) with no defaults.
    fn emit_closure_function(&mut self, code: Rc<Code>, mut flags: u32) -> CResult<()> {
        if !code.freevars.is_empty() {
            for n in code.freevars.iter() {
                let idx = match self.u().cell_idx.get(n) {
                    Some(&i) => i,
                    None => return self.err(&format!("unresolved free variable '{}'", n), 0),
                };
                self.emit(Op::LoadClosure(idx));
            }
            self.emit(Op::BuildTuple(code.freevars.len() as u32));
            flags |= MF_CLOSURE;
        }
        let ci = self.const_idx(Value::Obj(Object::new(Kind::Code(code))));
        self.emit(Op::LoadConst(ci));
        self.emit(Op::MakeFunction(flags));
        Ok(())
    }

    fn function_def(&mut self, f: &'a FunctionDef, line: u32) -> CResult<()> {
        for d in &f.decorators {
            self.expr(d)?;
        }
        let key = f as *const FunctionDef as usize;
        self.make_function(f.name.clone(), &f.args, f.returns.as_ref(), FnBody::Stmts(&f.body), key, f.is_async, line)?;
        for _ in &f.decorators {
            self.u().line = line;
            self.emit(Op::Call(1));
        }
        self.name_store(&f.name);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn make_function(
        &mut self,
        name: Rc<str>,
        args: &'a Arguments,
        returns: Option<&'a Expr>,
        body: FnBody<'a>,
        key: usize,
        _is_async: bool,
        line: u32,
    ) -> CResult<()> {
        let mut flags = 0;
        if !args.defaults.is_empty() {
            for d in &args.defaults {
                self.expr(d)?;
            }
            self.emit(Op::BuildTuple(args.defaults.len() as u32));
            flags |= MF_DEFAULTS;
        }
        let kwd: Vec<(&Arg, &Expr)> =
            args.kwonlyargs.iter().zip(args.kw_defaults.iter()).filter_map(|(a, d)| d.as_ref().map(|d| (a, d))).collect();
        if !kwd.is_empty() {
            for (a, d) in &kwd {
                let m = self.mangled(&a.arg);
                self.load_const(Value::str(&m));
                self.expr(d)?;
            }
            self.emit(Op::BuildMap(kwd.len() as u32));
            flags |= MF_KWDEFAULTS;
        }
        let mut ann: Vec<(Rc<str>, &Expr)> = Vec::new();
        for a in args.posonlyargs.iter().chain(args.args.iter()) {
            if let Some(an) = &a.annotation {
                ann.push((a.arg.clone(), an));
            }
        }
        if let Some(a) = &args.vararg {
            if let Some(an) = &a.annotation {
                ann.push((a.arg.clone(), an));
            }
        }
        for a in &args.kwonlyargs {
            if let Some(an) = &a.annotation {
                ann.push((a.arg.clone(), an));
            }
        }
        if let Some(a) = &args.kwarg {
            if let Some(an) = &a.annotation {
                ann.push((a.arg.clone(), an));
            }
        }
        if let Some(r) = returns {
            ann.push(("return".into(), r));
        }
        if !ann.is_empty() {
            for (n, e) in &ann {
                let m = self.mangled(n);
                self.load_const(Value::str(&m));
                self.expr(e)?;
            }
            self.emit(Op::BuildMap(ann.len() as u32));
            flags |= MF_ANNOTATIONS;
        }
        let sid = self.st.ids[&key];
        let qual = self.qualname_for(&name);
        self.push_unit(sid, UnitKind::Function, name.clone(), qual);
        self.u().line = line;
        let mut doc = None;
        let nparams = self.st.scopes[sid].params.len();
        let cell_args: Vec<(u32, u32)> = {
            let u = self.u();
            u.cells
                .iter()
                .enumerate()
                .take(u.ncellvars)
                .filter_map(|(ci, n)| u.var_idx.get(n).map(|&vi| (ci as u32, vi)))
                .filter(|(_, vi)| (*vi as usize) < nparams)
                .collect()
        };
        for (ci, vi) in cell_args {
            self.emit(Op::LoadFast(vi));
            self.emit(Op::StoreDeref(ci));
        }
        match body {
            FnBody::Stmts(stmts) => {
                if let Some(d) = docstring(stmts) {
                    doc = Some(Value::str(&d));
                }
                self.stmts(stmts)?;
                self.load_const(Value::None);
                self.emit(Op::ReturnValue);
            }
            FnBody::Expr(e) => {
                self.expr(e)?;
                self.emit(Op::ReturnValue);
            }
        }
        let code = self.pop_unit(args, line);
        let code = match doc {
            Some(d) => match Rc::try_unwrap(code) {
                Ok(mut c) => {
                    c.doc = Some(d);
                    Rc::new(c)
                }
                Err(c) => c,
            },
            None => code,
        };
        self.emit_closure_function(code, flags)?;
        Ok(())
    }

    fn class_def(&mut self, c: &'a ClassDef, line: u32) -> CResult<()> {
        for d in &c.decorators {
            self.expr(d)?;
        }
        self.emit(Op::LoadBuildClass);
        let key = c as *const ClassDef as usize;
        let sid = self.st.ids[&key];
        let qual = self.qualname_for(&c.name);
        self.push_unit(sid, UnitKind::Class, c.name.clone(), qual.clone());
        self.u().line = line;
        let ni = self.name_idx("__name__");
        self.emit(Op::LoadName(ni));
        let mi = self.name_idx("__module__");
        self.emit(Op::StoreName(mi));
        self.load_const(Value::str(&qual));
        let qi = self.name_idx("__qualname__");
        self.emit(Op::StoreName(qi));
        if let Some(d) = docstring(&c.body) {
            self.load_const(Value::str(&d));
            let di = self.name_idx("__doc__");
            self.emit(Op::StoreName(di));
        }
        if has_annassign(&c.body) {
            self.emit(Op::SetupAnnotations);
        }
        self.stmts(&c.body)?;
        let has_cell = self.st.scopes[sid].has_class_cell;
        if has_cell {
            let ci = self.u().cell_idx["__class__"];
            self.emit(Op::LoadClosure(ci));
            let cn = self.name_idx("__classcell__");
            self.emit(Op::StoreName(cn));
        }
        self.load_const(Value::None);
        self.emit(Op::ReturnValue);
        let code = self.pop_unit(&Arguments::default(), line);
        self.emit_closure_function(code, 0)?;
        self.load_const(Value::str(&c.name));
        let has_star = c.bases.iter().any(|b| matches!(b.kind, ExprKind::Starred { .. }));
        let has_dstar = c.keywords.iter().any(|k| k.arg.is_none());
        if !has_star && !has_dstar {
            for b in &c.bases {
                self.expr(b)?;
            }
            self.u().line = line;
            if c.keywords.is_empty() {
                self.emit(Op::Call(2 + c.bases.len() as u32));
            } else {
                for k in &c.keywords {
                    self.expr(&k.value)?;
                }
                let names: Vec<Value> = c.keywords.iter().map(|k| Value::str(k.arg.as_ref().unwrap())).collect();
                self.load_const(Value::tuple(names));
                self.emit(Op::CallKw(2 + (c.bases.len() + c.keywords.len()) as u32));
            }
        } else {
            self.emit(Op::BuildList(1));
            // [fn, name] pair already below; rebuild as list: fn name -> list
            self.emit(Op::Pop);
            return self.err("starred class bases are not supported", line);
        }
        for _ in &c.decorators {
            self.u().line = line;
            self.emit(Op::Call(1));
        }
        self.name_store(&c.name);
        Ok(())
    }

    fn match_stmt(&mut self, subject: &'a Expr, cases: &'a [MatchCase]) -> CResult<()> {
        self.expr(subject)?;
        let l_end = self.new_label();
        for case in cases {
            let l_next = self.new_label();
            self.emit(Op::Dup);
            self.pattern(&case.pattern, l_next)?;
            if let Some(g) = &case.guard {
                self.jump_if(g, l_next, false)?;
            }
            self.emit(Op::Pop);
            self.stmts(&case.body)?;
            self.emit(Op::Jump(l_end));
            self.bind(l_next);
        }
        self.emit(Op::Pop);
        self.bind(l_end);
        Ok(())
    }

    /// Matches TOS against `p`. The subject is consumed on both the success path and the jump to
    /// `fail`.
    fn pattern(&mut self, p: &'a Pattern, fail: Label) -> CResult<()> {
        match p {
            Pattern::MatchValue(e) => {
                self.expr(e)?;
                self.emit(Op::Compare(CmpOp::Eq));
                self.emit(Op::JumpIfFalse(fail));
            }
            Pattern::MatchSingleton(c) => {
                self.load_const(const_value(c));
                self.emit(Op::Compare(CmpOp::Is));
                self.emit(Op::JumpIfFalse(fail));
            }
            Pattern::MatchAs { pattern, name } => match pattern {
                None => match name {
                    Some(n) => self.name_store(n),
                    None => {
                        self.emit(Op::Pop);
                    }
                },
                Some(inner) => {
                    if let Some(n) = name {
                        self.emit(Op::Dup);
                        self.name_store(n);
                    }
                    self.pattern(inner, fail)?;
                }
            },
            Pattern::MatchOr(alts) => {
                let l_ok = self.new_label();
                for (i, a) in alts.iter().enumerate() {
                    if i + 1 < alts.len() {
                        let l_try_next = self.new_label();
                        self.emit(Op::Dup);
                        self.pattern(a, l_try_next)?;
                        self.emit(Op::Pop);
                        self.emit(Op::Jump(l_ok));
                        self.bind(l_try_next);
                    } else {
                        self.pattern(a, fail)?;
                    }
                }
                self.bind(l_ok);
            }
            Pattern::MatchSequence(pats) => {
                let l_f = self.new_label();
                let l_done = self.new_label();
                self.emit(Op::MatchSequence);
                self.emit(Op::JumpIfFalse(l_f));
                let star = pats.iter().position(|p| matches!(p, Pattern::MatchStar(_)));
                let n = pats.len() as u32 - star.is_some() as u32;
                self.emit(Op::MatchLen(n, star.is_some()));
                self.emit(Op::JumpIfFalse(l_f));
                for (i, sp) in pats.iter().enumerate() {
                    self.emit(Op::Dup);
                    match star {
                        Some(si) if i == si => {
                            self.emit(Op::MatchStarSlice(si as u32, (pats.len() - si - 1) as u32));
                            match sp {
                                Pattern::MatchStar(Some(name)) => self.name_store(name),
                                _ => {
                                    self.emit(Op::Pop);
                                }
                            }
                        }
                        Some(si) if i > si => {
                            self.emit(Op::MatchSeqItem(i as i32 - pats.len() as i32));
                            self.pattern(sp, l_f)?;
                        }
                        _ => {
                            self.emit(Op::MatchSeqItem(i as i32));
                            self.pattern(sp, l_f)?;
                        }
                    }
                }
                self.emit(Op::Pop);
                self.emit(Op::Jump(l_done));
                self.bind(l_f);
                self.emit(Op::Pop);
                self.emit(Op::Jump(fail));
                self.bind(l_done);
            }
            Pattern::MatchMapping { keys, patterns, rest } => {
                let l_f = self.new_label();
                let l_f2 = self.new_label();
                let l_done = self.new_label();
                self.emit(Op::MatchMapping);
                self.emit(Op::JumpIfFalse(l_f));
                for k in keys {
                    self.expr(k)?;
                }
                self.emit(Op::MatchKeys(keys.len() as u32));
                self.emit(Op::Dup);
                self.load_const(Value::None);
                self.emit(Op::Compare(CmpOp::Is));
                self.emit(Op::JumpIfTrue(l_f2));
                for (i, sp) in patterns.iter().enumerate() {
                    self.emit(Op::Dup);
                    self.emit(Op::MatchSeqItem(i as i32));
                    self.pattern(sp, l_f2)?;
                }
                if let Some(r) = rest {
                    for k in keys {
                        self.expr(k)?;
                    }
                    self.emit(Op::MatchRest(keys.len() as u32));
                    self.name_store(r);
                }
                self.emit(Op::Pop);
                self.emit(Op::Pop);
                self.emit(Op::Jump(l_done));
                self.bind(l_f2);
                self.emit(Op::Pop);
                self.bind(l_f);
                self.emit(Op::Pop);
                self.emit(Op::Jump(fail));
                self.bind(l_done);
            }
            Pattern::MatchClass { cls, patterns, kwd_attrs, kwd_patterns } => {
                let l_f = self.new_label();
                let l_f2 = self.new_label();
                let l_done = self.new_label();
                self.expr(cls)?;
                let names: Vec<Value> = kwd_attrs.iter().map(|k| Value::str(k)).collect();
                self.load_const(Value::tuple(names));
                self.emit(Op::MatchClass(patterns.len() as u32, kwd_attrs.len() as u32));
                self.emit(Op::Dup);
                self.load_const(Value::None);
                self.emit(Op::Compare(CmpOp::Is));
                self.emit(Op::JumpIfTrue(l_f2));
                for (i, sp) in patterns.iter().chain(kwd_patterns.iter()).enumerate() {
                    self.emit(Op::Dup);
                    self.emit(Op::MatchSeqItem(i as i32));
                    self.pattern(sp, l_f2)?;
                }
                self.emit(Op::Pop);
                self.emit(Op::Pop);
                self.emit(Op::Jump(l_done));
                self.bind(l_f2);
                self.emit(Op::Pop);
                self.bind(l_f);
                self.emit(Op::Pop);
                self.emit(Op::Jump(fail));
                self.bind(l_done);
            }
            Pattern::MatchStar(_) => return self.err("star pattern outside sequence", 0),
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CompKind {
    List,
    Set,
    Dict,
    Gen,
}

enum FnBody<'a> {
    Stmts(&'a [Stmt]),
    Expr(&'a Expr),
}
