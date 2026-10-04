//! The parser's tree as `_ast` node objects (`compile(..., PyCF_ONLY_AST)`, i.e. `ast.parse`).
//! The tree records start positions only, so `end_lineno`/`end_col_offset` are None; nodes
//! the tree keeps no position for (aliases, patterns) take their statement's.

use std::collections::HashMap;

use crate::ast::*;
use crate::bind::opaque_instance;
use crate::builtins::astm::_ast::AST;
use crate::builtins::astm::AstTypes;
use crate::object::*;
use crate::vm::{dict_set_str, Interp};

/// What `compile` was asked to produce.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Exec,
    Eval,
    Single,
}

struct Conv<'a> {
    it: &'a mut Interp,
    types: HashMap<&'static str, Obj>,
    singletons: HashMap<&'static str, Value>,
}

pub fn module_to_py(it: &mut Interp, m: &Module, mode: Mode) -> R<Value> {
    it.import_module("_ast")?;
    let types = it.native_state::<AstTypes>().by_name.clone();
    let mut c = Conv {
        it,
        types,
        singletons: HashMap::new(),
    };
    match mode {
        Mode::Exec => {
            let body = c.stmts(&m.body)?;
            c.node(
                "Module",
                vec![("body", body), ("type_ignores", Value::list(Vec::new()))],
            )
        }
        Mode::Single => {
            let body = c.stmts(&m.body)?;
            c.node("Interactive", vec![("body", body)])
        }
        Mode::Eval => match m.body.as_slice() {
            [Stmt {
                kind: StmtKind::Expr(e),
                ..
            }] => {
                let body = c.expr(e)?;
                c.node("Expression", vec![("body", body)])
            }
            _ => Err(c.it.type_error("expected an expression")),
        },
    }
}

impl Conv<'_> {
    fn node(&mut self, name: &str, fields: Vec<(&str, Value)>) -> R<Value> {
        let Some(cls) = self.types.get(name).cloned() else {
            return Err(self.it.type_error(&format!("_ast has no node type {name}")));
        };
        let v = opaque_instance(&cls, AST);
        if let Value::Obj(o) = &v {
            let d = self.it.instance_dict(o);
            for (k, x) in fields {
                dict_set_str(&d, k, x);
            }
        }
        Ok(v)
    }

    fn at(&mut self, name: &str, mut fields: Vec<(&str, Value)>, pos: Pos) -> R<Value> {
        fields.push(("lineno", Value::Int(pos.line as i64)));
        fields.push(("col_offset", Value::Int(pos.col as i64)));
        fields.push(("end_lineno", Value::None));
        fields.push(("end_col_offset", Value::None));
        self.node(name, fields)
    }

    /// The shared instance of a field-less node (`Load()`, `Add()`, ...), as CPython shares them.
    fn single(&mut self, name: &'static str) -> R<Value> {
        if let Some(v) = self.singletons.get(name) {
            return Ok(v.clone());
        }
        let v = self.node(name, Vec::new())?;
        self.singletons.insert(name, v.clone());
        Ok(v)
    }

    fn list<T>(&mut self, xs: &[T], mut f: impl FnMut(&mut Self, &T) -> R<Value>) -> R<Value> {
        let mut out = Vec::with_capacity(xs.len());
        for x in xs {
            out.push(f(self, x)?);
        }
        Ok(Value::list(out))
    }

    fn opt_expr(&mut self, e: Option<&Expr>) -> R<Value> {
        e.map_or(Ok(Value::None), |e| self.expr(e))
    }

    fn ident(s: &Ident) -> Value {
        Value::str(s)
    }

    fn opt_ident(s: &Option<Ident>) -> Value {
        s.as_ref().map_or(Value::None, Self::ident)
    }

    fn idents(xs: &[Ident]) -> Value {
        Value::list(xs.iter().map(Self::ident).collect())
    }

    fn stmts(&mut self, body: &[Stmt]) -> R<Value> {
        self.list(body, |c, s| c.stmt(s))
    }

    fn exprs(&mut self, xs: &[Expr]) -> R<Value> {
        self.list(xs, |c, e| c.expr(e))
    }

    fn ctx(&mut self, ctx: Ctx) -> R<Value> {
        self.single(match ctx {
            Ctx::Load => "Load",
            Ctx::Store => "Store",
            Ctx::Del => "Del",
        })
    }

    fn binop(&mut self, op: BinOp) -> R<Value> {
        self.single(match op {
            BinOp::Add => "Add",
            BinOp::Sub => "Sub",
            BinOp::Mult => "Mult",
            BinOp::MatMult => "MatMult",
            BinOp::Div => "Div",
            BinOp::Mod => "Mod",
            BinOp::Pow => "Pow",
            BinOp::LShift => "LShift",
            BinOp::RShift => "RShift",
            BinOp::BitOr => "BitOr",
            BinOp::BitXor => "BitXor",
            BinOp::BitAnd => "BitAnd",
            BinOp::FloorDiv => "FloorDiv",
        })
    }

    fn cmpop(&mut self, op: &CmpOp) -> R<Value> {
        self.single(match op {
            CmpOp::Eq => "Eq",
            CmpOp::NotEq => "NotEq",
            CmpOp::Lt => "Lt",
            CmpOp::LtE => "LtE",
            CmpOp::Gt => "Gt",
            CmpOp::GtE => "GtE",
            CmpOp::Is => "Is",
            CmpOp::IsNot => "IsNot",
            CmpOp::In => "In",
            CmpOp::NotIn => "NotIn",
        })
    }

    fn arguments(&mut self, a: &Arguments) -> R<Value> {
        let posonlyargs = self.list(&a.posonlyargs, |c, x| c.arg(x))?;
        let args = self.list(&a.args, |c, x| c.arg(x))?;
        let vararg = a.vararg.as_ref().map_or(Ok(Value::None), |x| self.arg(x))?;
        let kwonlyargs = self.list(&a.kwonlyargs, |c, x| c.arg(x))?;
        let kw_defaults = self.list(&a.kw_defaults, |c, x| c.opt_expr(x.as_ref()))?;
        let kwarg = a.kwarg.as_ref().map_or(Ok(Value::None), |x| self.arg(x))?;
        let defaults = self.exprs(&a.defaults)?;
        self.node(
            "arguments",
            vec![
                ("posonlyargs", posonlyargs),
                ("args", args),
                ("vararg", vararg),
                ("kwonlyargs", kwonlyargs),
                ("kw_defaults", kw_defaults),
                ("kwarg", kwarg),
                ("defaults", defaults),
            ],
        )
    }

    fn arg(&mut self, a: &Arg) -> R<Value> {
        let annotation = self.opt_expr(a.annotation.as_ref())?;
        self.at(
            "arg",
            vec![
                ("arg", Self::ident(&a.arg)),
                ("annotation", annotation),
                ("type_comment", Value::None),
            ],
            a.pos,
        )
    }

    fn keyword(&mut self, k: &Keyword) -> R<Value> {
        let value = self.expr(&k.value)?;
        self.at(
            "keyword",
            vec![("arg", Self::opt_ident(&k.arg)), ("value", value)],
            k.pos,
        )
    }

    fn alias(&mut self, a: &Alias, pos: Pos) -> R<Value> {
        self.at(
            "alias",
            vec![
                ("name", Self::ident(&a.name)),
                ("asname", Self::opt_ident(&a.asname)),
            ],
            pos,
        )
    }

    fn type_param(&mut self, t: &TypeParam) -> R<Value> {
        match &t.kind {
            TypeParamKind::TypeVar { name, bound } => {
                let bound = self.opt_expr(bound.as_ref())?;
                self.at(
                    "TypeVar",
                    vec![("name", Self::ident(name)), ("bound", bound)],
                    t.pos,
                )
            }
            TypeParamKind::ParamSpec { name } => {
                self.at("ParamSpec", vec![("name", Self::ident(name))], t.pos)
            }
            TypeParamKind::TypeVarTuple { name } => {
                self.at("TypeVarTuple", vec![("name", Self::ident(name))], t.pos)
            }
        }
    }

    fn type_params(&mut self, ts: &[TypeParam]) -> R<Value> {
        self.list(ts, |c, t| c.type_param(t))
    }

    fn stmt(&mut self, s: &Stmt) -> R<Value> {
        let pos = s.pos;
        match &s.kind {
            StmtKind::FunctionDef(f) => {
                let args = self.arguments(&f.args)?;
                let body = self.stmts(&f.body)?;
                let decorator_list = self.exprs(&f.decorators)?;
                let returns = self.opt_expr(f.returns.as_ref())?;
                let type_params = self.type_params(&f.type_params)?;
                let name = if f.is_async {
                    "AsyncFunctionDef"
                } else {
                    "FunctionDef"
                };
                self.at(
                    name,
                    vec![
                        ("name", Self::ident(&f.name)),
                        ("args", args),
                        ("body", body),
                        ("decorator_list", decorator_list),
                        ("returns", returns),
                        ("type_comment", Value::None),
                        ("type_params", type_params),
                    ],
                    pos,
                )
            }
            StmtKind::ClassDef(c) => {
                let bases = self.exprs(&c.bases)?;
                let keywords = self.list(&c.keywords, |cv, k| cv.keyword(k))?;
                let body = self.stmts(&c.body)?;
                let decorator_list = self.exprs(&c.decorators)?;
                let type_params = self.type_params(&c.type_params)?;
                self.at(
                    "ClassDef",
                    vec![
                        ("name", Self::ident(&c.name)),
                        ("bases", bases),
                        ("keywords", keywords),
                        ("body", body),
                        ("decorator_list", decorator_list),
                        ("type_params", type_params),
                    ],
                    pos,
                )
            }
            StmtKind::Return(v) => {
                let value = self.opt_expr(v.as_ref())?;
                self.at("Return", vec![("value", value)], pos)
            }
            StmtKind::Delete(targets) => {
                let targets = self.exprs(targets)?;
                self.at("Delete", vec![("targets", targets)], pos)
            }
            StmtKind::Assign { targets, value } => {
                let targets = self.exprs(targets)?;
                let value = self.expr(value)?;
                self.at(
                    "Assign",
                    vec![
                        ("targets", targets),
                        ("value", value),
                        ("type_comment", Value::None),
                    ],
                    pos,
                )
            }
            StmtKind::AugAssign { target, op, value } => {
                let target = self.expr(target)?;
                let op = self.binop(*op)?;
                let value = self.expr(value)?;
                self.at(
                    "AugAssign",
                    vec![("target", target), ("op", op), ("value", value)],
                    pos,
                )
            }
            StmtKind::AnnAssign {
                target,
                annotation,
                value,
                simple,
            } => {
                let target = self.expr(target)?;
                let annotation = self.expr(annotation)?;
                let value = self.opt_expr(value.as_ref())?;
                self.at(
                    "AnnAssign",
                    vec![
                        ("target", target),
                        ("annotation", annotation),
                        ("value", value),
                        ("simple", Value::Int(*simple as i64)),
                    ],
                    pos,
                )
            }
            StmtKind::For {
                target,
                iter,
                body,
                orelse,
                is_async,
            } => {
                let target = self.expr(target)?;
                let iter = self.expr(iter)?;
                let body = self.stmts(body)?;
                let orelse = self.stmts(orelse)?;
                let name = if *is_async { "AsyncFor" } else { "For" };
                self.at(
                    name,
                    vec![
                        ("target", target),
                        ("iter", iter),
                        ("body", body),
                        ("orelse", orelse),
                        ("type_comment", Value::None),
                    ],
                    pos,
                )
            }
            StmtKind::While { test, body, orelse } => {
                let test = self.expr(test)?;
                let body = self.stmts(body)?;
                let orelse = self.stmts(orelse)?;
                self.at(
                    "While",
                    vec![("test", test), ("body", body), ("orelse", orelse)],
                    pos,
                )
            }
            StmtKind::If { test, body, orelse } => {
                let test = self.expr(test)?;
                let body = self.stmts(body)?;
                let orelse = self.stmts(orelse)?;
                self.at(
                    "If",
                    vec![("test", test), ("body", body), ("orelse", orelse)],
                    pos,
                )
            }
            StmtKind::With {
                items,
                body,
                is_async,
            } => {
                let items = self.list(items, |c, w| {
                    let context_expr = c.expr(&w.context_expr)?;
                    let optional_vars = c.opt_expr(w.optional_vars.as_ref())?;
                    c.node(
                        "withitem",
                        vec![
                            ("context_expr", context_expr),
                            ("optional_vars", optional_vars),
                        ],
                    )
                })?;
                let body = self.stmts(body)?;
                let name = if *is_async { "AsyncWith" } else { "With" };
                self.at(
                    name,
                    vec![
                        ("items", items),
                        ("body", body),
                        ("type_comment", Value::None),
                    ],
                    pos,
                )
            }
            StmtKind::Match { subject, cases } => {
                let subject = self.expr(subject)?;
                let cases = self.list(cases, |c, mc| {
                    let pattern = c.pattern(&mc.pattern, pos)?;
                    let guard = c.opt_expr(mc.guard.as_ref())?;
                    let body = c.stmts(&mc.body)?;
                    c.node(
                        "match_case",
                        vec![("pattern", pattern), ("guard", guard), ("body", body)],
                    )
                })?;
                self.at("Match", vec![("subject", subject), ("cases", cases)], pos)
            }
            StmtKind::Raise { exc, cause } => {
                let exc = self.opt_expr(exc.as_ref())?;
                let cause = self.opt_expr(cause.as_ref())?;
                self.at("Raise", vec![("exc", exc), ("cause", cause)], pos)
            }
            StmtKind::Try {
                body,
                handlers,
                orelse,
                finalbody,
                is_star,
            } => {
                let body = self.stmts(body)?;
                let handlers = self.list(handlers, |c, h| {
                    let typ = c.opt_expr(h.typ.as_ref())?;
                    let body = c.stmts(&h.body)?;
                    c.at(
                        "ExceptHandler",
                        vec![
                            ("type", typ),
                            ("name", Self::opt_ident(&h.name)),
                            ("body", body),
                        ],
                        h.pos,
                    )
                })?;
                let orelse = self.stmts(orelse)?;
                let finalbody = self.stmts(finalbody)?;
                let name = if *is_star { "TryStar" } else { "Try" };
                self.at(
                    name,
                    vec![
                        ("body", body),
                        ("handlers", handlers),
                        ("orelse", orelse),
                        ("finalbody", finalbody),
                    ],
                    pos,
                )
            }
            StmtKind::Assert { test, msg } => {
                let test = self.expr(test)?;
                let msg = self.opt_expr(msg.as_ref())?;
                self.at("Assert", vec![("test", test), ("msg", msg)], pos)
            }
            StmtKind::Import(names) => {
                let names = self.list(names, |c, a| c.alias(a, pos))?;
                self.at("Import", vec![("names", names)], pos)
            }
            StmtKind::ImportFrom {
                module,
                names,
                level,
            } => {
                let names = self.list(names, |c, a| c.alias(a, pos))?;
                self.at(
                    "ImportFrom",
                    vec![
                        ("module", Self::opt_ident(module)),
                        ("names", names),
                        ("level", Value::Int(*level as i64)),
                    ],
                    pos,
                )
            }
            StmtKind::Global(names) => self.at("Global", vec![("names", Self::idents(names))], pos),
            StmtKind::Nonlocal(names) => {
                self.at("Nonlocal", vec![("names", Self::idents(names))], pos)
            }
            StmtKind::TypeAlias {
                name,
                type_params,
                value,
            } => {
                let name = self.expr(name)?;
                let type_params = self.type_params(type_params)?;
                let value = self.expr(value)?;
                self.at(
                    "TypeAlias",
                    vec![
                        ("name", name),
                        ("type_params", type_params),
                        ("value", value),
                    ],
                    pos,
                )
            }
            StmtKind::Expr(e) => {
                let value = self.expr(e)?;
                self.at("Expr", vec![("value", value)], pos)
            }
            StmtKind::Pass => self.at("Pass", Vec::new(), pos),
            StmtKind::Break => self.at("Break", Vec::new(), pos),
            StmtKind::Continue => self.at("Continue", Vec::new(), pos),
        }
    }

    fn comprehensions(&mut self, gens: &[Comprehension]) -> R<Value> {
        self.list(gens, |c, g| {
            let target = c.expr(&g.target)?;
            let iter = c.expr(&g.iter)?;
            let ifs = c.exprs(&g.ifs)?;
            c.node(
                "comprehension",
                vec![
                    ("target", target),
                    ("iter", iter),
                    ("ifs", ifs),
                    ("is_async", Value::Int(g.is_async as i64)),
                ],
            )
        })
    }

    fn constant(&mut self, k: &Constant, pos: Pos) -> R<Value> {
        let value = crate::compile::const_value(k);
        self.at(
            "Constant",
            vec![("value", value), ("kind", Value::None)],
            pos,
        )
    }

    fn expr(&mut self, e: &Expr) -> R<Value> {
        let pos = e.pos;
        match &e.kind {
            ExprKind::BoolOp { op, values } => {
                let op = self.single(match op {
                    BoolOp::And => "And",
                    BoolOp::Or => "Or",
                })?;
                let values = self.exprs(values)?;
                self.at("BoolOp", vec![("op", op), ("values", values)], pos)
            }
            ExprKind::NamedExpr { target, value } => {
                let target = self.expr(target)?;
                let value = self.expr(value)?;
                self.at("NamedExpr", vec![("target", target), ("value", value)], pos)
            }
            ExprKind::BinOp { left, op, right } => {
                let left = self.expr(left)?;
                let op = self.binop(*op)?;
                let right = self.expr(right)?;
                self.at(
                    "BinOp",
                    vec![("left", left), ("op", op), ("right", right)],
                    pos,
                )
            }
            ExprKind::UnaryOp { op, operand } => {
                let op = self.single(match op {
                    UnaryOp::Invert => "Invert",
                    UnaryOp::Not => "Not",
                    UnaryOp::UAdd => "UAdd",
                    UnaryOp::USub => "USub",
                })?;
                let operand = self.expr(operand)?;
                self.at("UnaryOp", vec![("op", op), ("operand", operand)], pos)
            }
            ExprKind::Lambda { args, body } => {
                let args = self.arguments(args)?;
                let body = self.expr(body)?;
                self.at("Lambda", vec![("args", args), ("body", body)], pos)
            }
            ExprKind::IfExp { test, body, orelse } => {
                let test = self.expr(test)?;
                let body = self.expr(body)?;
                let orelse = self.expr(orelse)?;
                self.at(
                    "IfExp",
                    vec![("test", test), ("body", body), ("orelse", orelse)],
                    pos,
                )
            }
            ExprKind::Dict { keys, values } => {
                let keys = self.list(keys, |c, k| c.opt_expr(k.as_ref()))?;
                let values = self.exprs(values)?;
                self.at("Dict", vec![("keys", keys), ("values", values)], pos)
            }
            ExprKind::Set(elts) => {
                let elts = self.exprs(elts)?;
                self.at("Set", vec![("elts", elts)], pos)
            }
            ExprKind::ListComp { elt, generators }
            | ExprKind::SetComp { elt, generators }
            | ExprKind::GeneratorExp { elt, generators } => {
                let name = match &e.kind {
                    ExprKind::ListComp { .. } => "ListComp",
                    ExprKind::SetComp { .. } => "SetComp",
                    _ => "GeneratorExp",
                };
                let elt = self.expr(elt)?;
                let generators = self.comprehensions(generators)?;
                self.at(name, vec![("elt", elt), ("generators", generators)], pos)
            }
            ExprKind::DictComp {
                key,
                value,
                generators,
            } => {
                let key = self.expr(key)?;
                let value = self.expr(value)?;
                let generators = self.comprehensions(generators)?;
                self.at(
                    "DictComp",
                    vec![("key", key), ("value", value), ("generators", generators)],
                    pos,
                )
            }
            ExprKind::Await(v) => {
                let value = self.expr(v)?;
                self.at("Await", vec![("value", value)], pos)
            }
            ExprKind::Yield(v) => {
                let value = self.opt_expr(v.as_deref())?;
                self.at("Yield", vec![("value", value)], pos)
            }
            ExprKind::YieldFrom(v) => {
                let value = self.expr(v)?;
                self.at("YieldFrom", vec![("value", value)], pos)
            }
            ExprKind::Compare {
                left,
                ops,
                comparators,
            } => {
                let left = self.expr(left)?;
                let ops = self.list(ops, |c, op| c.cmpop(op))?;
                let comparators = self.exprs(comparators)?;
                self.at(
                    "Compare",
                    vec![("left", left), ("ops", ops), ("comparators", comparators)],
                    pos,
                )
            }
            ExprKind::Call {
                func,
                args,
                keywords,
            } => {
                let func = self.expr(func)?;
                let args = self.exprs(args)?;
                let keywords = self.list(keywords, |c, k| c.keyword(k))?;
                self.at(
                    "Call",
                    vec![("func", func), ("args", args), ("keywords", keywords)],
                    pos,
                )
            }
            ExprKind::JoinedStr(values) => {
                let values = self.exprs(values)?;
                self.at("JoinedStr", vec![("values", values)], pos)
            }
            ExprKind::FormattedValue {
                value,
                conversion,
                format_spec,
            } => {
                let value = self.expr(value)?;
                let conversion = Value::Int(conversion.map_or(-1, |c| c as i64));
                let format_spec = self.opt_expr(format_spec.as_deref())?;
                self.at(
                    "FormattedValue",
                    vec![
                        ("value", value),
                        ("conversion", conversion),
                        ("format_spec", format_spec),
                    ],
                    pos,
                )
            }
            ExprKind::Constant(k) => self.constant(k, pos),
            ExprKind::Attribute { value, attr, ctx } => {
                let value = self.expr(value)?;
                let ctx = self.ctx(*ctx)?;
                self.at(
                    "Attribute",
                    vec![("value", value), ("attr", Self::ident(attr)), ("ctx", ctx)],
                    pos,
                )
            }
            ExprKind::Subscript { value, slice, ctx } => {
                let value = self.expr(value)?;
                let slice = self.expr(slice)?;
                let ctx = self.ctx(*ctx)?;
                self.at(
                    "Subscript",
                    vec![("value", value), ("slice", slice), ("ctx", ctx)],
                    pos,
                )
            }
            ExprKind::Starred { value, ctx } => {
                let value = self.expr(value)?;
                let ctx = self.ctx(*ctx)?;
                self.at("Starred", vec![("value", value), ("ctx", ctx)], pos)
            }
            ExprKind::Name { id, ctx } => {
                let ctx = self.ctx(*ctx)?;
                self.at("Name", vec![("id", Self::ident(id)), ("ctx", ctx)], pos)
            }
            ExprKind::List { elts, ctx } | ExprKind::Tuple { elts, ctx } => {
                let name = if matches!(e.kind, ExprKind::List { .. }) {
                    "List"
                } else {
                    "Tuple"
                };
                let elts = self.exprs(elts)?;
                let ctx = self.ctx(*ctx)?;
                self.at(name, vec![("elts", elts), ("ctx", ctx)], pos)
            }
            ExprKind::Slice { lower, upper, step } => {
                let lower = self.opt_expr(lower.as_deref())?;
                let upper = self.opt_expr(upper.as_deref())?;
                let step = self.opt_expr(step.as_deref())?;
                self.at(
                    "Slice",
                    vec![("lower", lower), ("upper", upper), ("step", step)],
                    pos,
                )
            }
        }
    }

    fn patterns(&mut self, ps: &[Pattern], pos: Pos) -> R<Value> {
        self.list(ps, |c, p| c.pattern(p, pos))
    }

    fn pattern(&mut self, p: &Pattern, pos: Pos) -> R<Value> {
        match p {
            Pattern::MatchValue(e) => {
                let value = self.expr(e)?;
                self.at("MatchValue", vec![("value", value)], e.pos)
            }
            Pattern::MatchSingleton(k) => {
                let value = crate::compile::const_value(k);
                self.at("MatchSingleton", vec![("value", value)], pos)
            }
            Pattern::MatchSequence(ps) => {
                let patterns = self.patterns(ps, pos)?;
                self.at("MatchSequence", vec![("patterns", patterns)], pos)
            }
            Pattern::MatchMapping {
                keys,
                patterns,
                rest,
            } => {
                let keys = self.exprs(keys)?;
                let patterns = self.patterns(patterns, pos)?;
                self.at(
                    "MatchMapping",
                    vec![
                        ("keys", keys),
                        ("patterns", patterns),
                        ("rest", Self::opt_ident(rest)),
                    ],
                    pos,
                )
            }
            Pattern::MatchClass {
                cls,
                patterns,
                kwd_attrs,
                kwd_patterns,
            } => {
                let pos = cls.pos;
                let cls = self.expr(cls)?;
                let patterns = self.patterns(patterns, pos)?;
                let kwd_patterns = self.patterns(kwd_patterns, pos)?;
                self.at(
                    "MatchClass",
                    vec![
                        ("cls", cls),
                        ("patterns", patterns),
                        ("kwd_attrs", Self::idents(kwd_attrs)),
                        ("kwd_patterns", kwd_patterns),
                    ],
                    pos,
                )
            }
            Pattern::MatchStar(name) => {
                self.at("MatchStar", vec![("name", Self::opt_ident(name))], pos)
            }
            Pattern::MatchAs { pattern, name } => {
                let pattern = match pattern {
                    Some(p) => self.pattern(p, pos)?,
                    None => Value::None,
                };
                self.at(
                    "MatchAs",
                    vec![("pattern", pattern), ("name", Self::opt_ident(name))],
                    pos,
                )
            }
            Pattern::MatchOr(ps) => {
                let patterns = self.patterns(ps, pos)?;
                self.at("MatchOr", vec![("patterns", patterns)], pos)
            }
        }
    }
}
