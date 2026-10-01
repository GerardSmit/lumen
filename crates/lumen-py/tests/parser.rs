//! Parser tests: hand-written assertions, a differential corpus against CPython's `ast.dump`,
//! and (ignored by default) a stdlib stress run.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use lumen_py::ast::*;
use lumen_py::parse;

const SEP: &str = "\n#---\n";

fn at(pos: Option<Pos>) -> String {
    pos.map_or(String::new(), |p| format!("@{}:{}", p.line, p.col))
}

type Fields<'a> = Vec<(&'a str, Option<String>)>;

fn node(name: &str, pos: Option<Pos>, fields: Fields) -> String {
    let parts: Vec<String> = fields
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| format!("{k}={v}")))
        .collect();
    format!("{name}{}({})", at(pos), parts.join(", "))
}

fn lst(items: Vec<String>) -> Option<String> {
    if items.is_empty() {
        None
    } else {
        Some(format!("[{}]", items.join(", ")))
    }
}

fn some(s: String) -> Option<String> {
    Some(s)
}

fn py_float(f: f64) -> String {
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if f == 0.0 {
        return "0.0".into();
    }
    let sci = format!("{f:e}");
    let (mant, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    if (-4..16).contains(&exp) {
        let mut s = if exp >= 0 {
            let int_len = exp as usize + 1;
            let mut d = digits.clone();
            while d.len() < int_len {
                d.push('0');
            }
            let (i, frac) = d.split_at(int_len);
            if frac.is_empty() {
                format!("{i}.0")
            } else {
                format!("{i}.{frac}")
            }
        } else {
            format!("0.{}{}", "0".repeat((-exp - 1) as usize), digits)
        };
        if f < 0.0 {
            s.insert(0, '-');
        }
        s
    } else {
        let sign = if f < 0.0 { "-" } else { "" };
        let m = if digits.len() == 1 {
            digits.clone()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        format!(
            "{sign}{m}e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        )
    }
}

fn nonprintable(c: char) -> bool {
    let u = c as u32;
    u < 0x20
        || (0x7f..=0xa0).contains(&u)
        || u == 0xad
        || (0x2000..=0x200f).contains(&u)
        || (0x2028..=0x202f).contains(&u)
        || (0x205f..=0x206f).contains(&u)
        || u == 0x3000
        || u == 0xfeff
        || (0xe000..=0xf8ff).contains(&u)
        || (0xfff0..=0xffff).contains(&u)
}

fn py_str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if nonprintable(c) => {
                let u = c as u32;
                if u < 0x100 {
                    write!(out, "\\x{u:02x}").unwrap();
                } else if u < 0x10000 {
                    write!(out, "\\u{u:04x}").unwrap();
                } else {
                    write!(out, "\\U{u:08x}").unwrap();
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

fn py_bytes_repr(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from("b");
    out.push(quote);
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            c if c as char == quote => {
                out.push('\\');
                out.push(c as char);
            }
            0x20..=0x7e => out.push(c as char),
            c => write!(out, "\\x{c:02x}").unwrap(),
        }
    }
    out.push(quote);
    out
}

struct Dumper {
    pos: bool,
}

impl Dumper {
    fn p(&self, pos: Pos) -> Option<Pos> {
        self.pos.then_some(pos)
    }

    fn exprs(&self, v: &[Expr]) -> Option<String> {
        lst(v.iter().map(|e| self.expr(e)).collect())
    }

    fn stmts(&self, v: &[Stmt]) -> Option<String> {
        lst(v.iter().map(|s| self.stmt(s)).collect())
    }

    fn opt_expr(&self, e: &Option<Expr>) -> Option<String> {
        e.as_ref().map(|e| self.expr(e))
    }

    fn boxed(&self, e: &Option<Box<Expr>>) -> Option<String> {
        e.as_ref().map(|e| self.expr(e))
    }

    fn names(v: &[Ident]) -> Option<String> {
        lst(v.iter().map(|n| py_str_repr(n)).collect())
    }

    fn ctx(c: Ctx) -> Option<String> {
        some(format!("{c:?}()"))
    }

    fn op(o: BinOp) -> Option<String> {
        some(format!("{o:?}()"))
    }

    fn constant(c: &Constant) -> String {
        match c {
            Constant::None => "None".into(),
            Constant::True => "True".into(),
            Constant::False => "False".into(),
            Constant::Ellipsis => "Ellipsis".into(),
            Constant::Int(s) => s.to_string(),
            Constant::Float(f) => py_float(*f),
            Constant::Complex(f) => format!("{}j", py_float(*f).trim_end_matches(".0")),
            Constant::Str(s) => py_str_repr(s),
            Constant::Bytes(b) => py_bytes_repr(b),
        }
    }

    fn comps(&self, gens: &[Comprehension]) -> Option<String> {
        lst(gens
            .iter()
            .map(|g| {
                node(
                    "comprehension",
                    None,
                    vec![
                        ("target", some(self.expr(&g.target))),
                        ("iter", some(self.expr(&g.iter))),
                        ("ifs", self.exprs(&g.ifs)),
                        ("is_async", some((g.is_async as u8).to_string())),
                    ],
                )
            })
            .collect())
    }

    fn arg(&self, a: &Arg) -> String {
        node(
            "arg",
            self.p(a.pos),
            vec![
                ("arg", some(py_str_repr(&a.arg))),
                ("annotation", self.opt_expr(&a.annotation)),
            ],
        )
    }

    fn arguments(&self, a: &Arguments) -> String {
        node(
            "arguments",
            None,
            vec![
                (
                    "posonlyargs",
                    lst(a.posonlyargs.iter().map(|x| self.arg(x)).collect()),
                ),
                ("args", lst(a.args.iter().map(|x| self.arg(x)).collect())),
                ("vararg", a.vararg.as_ref().map(|x| self.arg(x))),
                (
                    "kwonlyargs",
                    lst(a.kwonlyargs.iter().map(|x| self.arg(x)).collect()),
                ),
                (
                    "kw_defaults",
                    lst(a
                        .kw_defaults
                        .iter()
                        .map(|d| d.as_ref().map_or("None".to_string(), |e| self.expr(e)))
                        .collect()),
                ),
                ("kwarg", a.kwarg.as_ref().map(|x| self.arg(x))),
                ("defaults", self.exprs(&a.defaults)),
            ],
        )
    }

    fn keywords(&self, ks: &[Keyword]) -> Option<String> {
        lst(ks
            .iter()
            .map(|k| {
                node(
                    "keyword",
                    self.p(k.pos),
                    vec![
                        ("arg", k.arg.as_ref().map(|a| py_str_repr(a))),
                        ("value", some(self.expr(&k.value))),
                    ],
                )
            })
            .collect())
    }

    fn expr(&self, e: &Expr) -> String {
        let pos = self.p(e.pos);
        match &e.kind {
            ExprKind::BoolOp { op, values } => node(
                "BoolOp",
                pos,
                vec![
                    ("op", some(format!("{op:?}()"))),
                    ("values", self.exprs(values)),
                ],
            ),
            ExprKind::NamedExpr { target, value } => node(
                "NamedExpr",
                pos,
                vec![
                    ("target", some(self.expr(target))),
                    ("value", some(self.expr(value))),
                ],
            ),
            ExprKind::BinOp { left, op, right } => node(
                "BinOp",
                pos,
                vec![
                    ("left", some(self.expr(left))),
                    ("op", Self::op(*op)),
                    ("right", some(self.expr(right))),
                ],
            ),
            ExprKind::UnaryOp { op, operand } => node(
                "UnaryOp",
                pos,
                vec![
                    ("op", some(format!("{op:?}()"))),
                    ("operand", some(self.expr(operand))),
                ],
            ),
            ExprKind::Lambda { args, body } => node(
                "Lambda",
                pos,
                vec![
                    ("args", some(self.arguments(args))),
                    ("body", some(self.expr(body))),
                ],
            ),
            ExprKind::IfExp { test, body, orelse } => node(
                "IfExp",
                pos,
                vec![
                    ("test", some(self.expr(test))),
                    ("body", some(self.expr(body))),
                    ("orelse", some(self.expr(orelse))),
                ],
            ),
            ExprKind::Dict { keys, values } => node(
                "Dict",
                pos,
                vec![
                    (
                        "keys",
                        lst(keys
                            .iter()
                            .map(|k| k.as_ref().map_or("None".to_string(), |e| self.expr(e)))
                            .collect()),
                    ),
                    ("values", self.exprs(values)),
                ],
            ),
            ExprKind::Set(elts) => node("Set", pos, vec![("elts", self.exprs(elts))]),
            ExprKind::ListComp { elt, generators } => node(
                "ListComp",
                pos,
                vec![
                    ("elt", some(self.expr(elt))),
                    ("generators", self.comps(generators)),
                ],
            ),
            ExprKind::SetComp { elt, generators } => node(
                "SetComp",
                pos,
                vec![
                    ("elt", some(self.expr(elt))),
                    ("generators", self.comps(generators)),
                ],
            ),
            ExprKind::DictComp {
                key,
                value,
                generators,
            } => node(
                "DictComp",
                pos,
                vec![
                    ("key", some(self.expr(key))),
                    ("value", some(self.expr(value))),
                    ("generators", self.comps(generators)),
                ],
            ),
            ExprKind::GeneratorExp { elt, generators } => node(
                "GeneratorExp",
                pos,
                vec![
                    ("elt", some(self.expr(elt))),
                    ("generators", self.comps(generators)),
                ],
            ),
            ExprKind::Await(v) => node("Await", pos, vec![("value", some(self.expr(v)))]),
            ExprKind::Yield(v) => node("Yield", pos, vec![("value", self.boxed(v))]),
            ExprKind::YieldFrom(v) => node("YieldFrom", pos, vec![("value", some(self.expr(v)))]),
            ExprKind::Compare {
                left,
                ops,
                comparators,
            } => node(
                "Compare",
                pos,
                vec![
                    ("left", some(self.expr(left))),
                    ("ops", lst(ops.iter().map(|o| format!("{o:?}()")).collect())),
                    ("comparators", self.exprs(comparators)),
                ],
            ),
            ExprKind::Call {
                func,
                args,
                keywords,
            } => node(
                "Call",
                pos,
                vec![
                    ("func", some(self.expr(func))),
                    ("args", self.exprs(args)),
                    ("keywords", self.keywords(keywords)),
                ],
            ),
            ExprKind::JoinedStr(values) => {
                node("JoinedStr", pos, vec![("values", self.exprs(values))])
            }
            ExprKind::FormattedValue {
                value,
                conversion,
                format_spec,
            } => node(
                "FormattedValue",
                pos,
                vec![
                    ("value", some(self.expr(value))),
                    (
                        "conversion",
                        some(conversion.map_or(-1, |c| c as i32).to_string()),
                    ),
                    ("format_spec", self.boxed(format_spec)),
                ],
            ),
            ExprKind::Constant(c) => {
                node("Constant", pos, vec![("value", some(Self::constant(c)))])
            }
            ExprKind::Attribute { value, attr, ctx } => node(
                "Attribute",
                pos,
                vec![
                    ("value", some(self.expr(value))),
                    ("attr", some(py_str_repr(attr))),
                    ("ctx", Self::ctx(*ctx)),
                ],
            ),
            ExprKind::Subscript { value, slice, ctx } => node(
                "Subscript",
                pos,
                vec![
                    ("value", some(self.expr(value))),
                    ("slice", some(self.expr(slice))),
                    ("ctx", Self::ctx(*ctx)),
                ],
            ),
            ExprKind::Starred { value, ctx } => node(
                "Starred",
                pos,
                vec![("value", some(self.expr(value))), ("ctx", Self::ctx(*ctx))],
            ),
            ExprKind::Name { id, ctx } => node(
                "Name",
                pos,
                vec![("id", some(py_str_repr(id))), ("ctx", Self::ctx(*ctx))],
            ),
            ExprKind::List { elts, ctx } => node(
                "List",
                pos,
                vec![("elts", self.exprs(elts)), ("ctx", Self::ctx(*ctx))],
            ),
            ExprKind::Tuple { elts, ctx } => node(
                "Tuple",
                pos,
                vec![("elts", self.exprs(elts)), ("ctx", Self::ctx(*ctx))],
            ),
            ExprKind::Slice { lower, upper, step } => node(
                "Slice",
                pos,
                vec![
                    ("lower", self.boxed(lower)),
                    ("upper", self.boxed(upper)),
                    ("step", self.boxed(step)),
                ],
            ),
        }
    }

    fn pattern(&self, p: &Pattern) -> String {
        match p {
            Pattern::MatchValue(e) => node("MatchValue", None, vec![("value", some(self.expr(e)))]),
            Pattern::MatchSingleton(c) => node(
                "MatchSingleton",
                None,
                vec![("value", some(Self::constant(c)))],
            ),
            Pattern::MatchSequence(ps) => node(
                "MatchSequence",
                None,
                vec![(
                    "patterns",
                    lst(ps.iter().map(|p| self.pattern(p)).collect()),
                )],
            ),
            Pattern::MatchMapping {
                keys,
                patterns,
                rest,
            } => node(
                "MatchMapping",
                None,
                vec![
                    ("keys", self.exprs(keys)),
                    (
                        "patterns",
                        lst(patterns.iter().map(|p| self.pattern(p)).collect()),
                    ),
                    ("rest", rest.as_ref().map(|r| py_str_repr(r))),
                ],
            ),
            Pattern::MatchClass {
                cls,
                patterns,
                kwd_attrs,
                kwd_patterns,
            } => node(
                "MatchClass",
                None,
                vec![
                    ("cls", some(self.expr(cls))),
                    (
                        "patterns",
                        lst(patterns.iter().map(|p| self.pattern(p)).collect()),
                    ),
                    ("kwd_attrs", Self::names(kwd_attrs)),
                    (
                        "kwd_patterns",
                        lst(kwd_patterns.iter().map(|p| self.pattern(p)).collect()),
                    ),
                ],
            ),
            Pattern::MatchStar(n) => node(
                "MatchStar",
                None,
                vec![("name", n.as_ref().map(|n| py_str_repr(n)))],
            ),
            Pattern::MatchAs { pattern, name } => node(
                "MatchAs",
                None,
                vec![
                    ("pattern", pattern.as_ref().map(|p| self.pattern(p))),
                    ("name", name.as_ref().map(|n| py_str_repr(n))),
                ],
            ),
            Pattern::MatchOr(ps) => node(
                "MatchOr",
                None,
                vec![(
                    "patterns",
                    lst(ps.iter().map(|p| self.pattern(p)).collect()),
                )],
            ),
        }
    }

    fn aliases(a: &[Alias]) -> Option<String> {
        lst(a
            .iter()
            .map(|a| {
                node(
                    "alias",
                    None,
                    vec![
                        ("name", some(py_str_repr(&a.name))),
                        ("asname", a.asname.as_ref().map(|n| py_str_repr(n))),
                    ],
                )
            })
            .collect())
    }

    fn stmt(&self, s: &Stmt) -> String {
        let pos = self.p(s.pos);
        match &s.kind {
            StmtKind::FunctionDef(f) => node(
                if f.is_async {
                    "AsyncFunctionDef"
                } else {
                    "FunctionDef"
                },
                pos,
                vec![
                    ("name", some(py_str_repr(&f.name))),
                    ("args", some(self.arguments(&f.args))),
                    ("body", self.stmts(&f.body)),
                    ("decorator_list", self.exprs(&f.decorators)),
                    ("returns", self.opt_expr(&f.returns)),
                ],
            ),
            StmtKind::ClassDef(c) => node(
                "ClassDef",
                pos,
                vec![
                    ("name", some(py_str_repr(&c.name))),
                    ("bases", self.exprs(&c.bases)),
                    ("keywords", self.keywords(&c.keywords)),
                    ("body", self.stmts(&c.body)),
                    ("decorator_list", self.exprs(&c.decorators)),
                ],
            ),
            StmtKind::Return(v) => node("Return", pos, vec![("value", self.opt_expr(v))]),
            StmtKind::Delete(t) => node("Delete", pos, vec![("targets", self.exprs(t))]),
            StmtKind::Assign { targets, value } => node(
                "Assign",
                pos,
                vec![
                    ("targets", self.exprs(targets)),
                    ("value", some(self.expr(value))),
                ],
            ),
            StmtKind::AugAssign { target, op, value } => node(
                "AugAssign",
                pos,
                vec![
                    ("target", some(self.expr(target))),
                    ("op", Self::op(*op)),
                    ("value", some(self.expr(value))),
                ],
            ),
            StmtKind::AnnAssign {
                target,
                annotation,
                value,
                simple,
            } => node(
                "AnnAssign",
                pos,
                vec![
                    ("target", some(self.expr(target))),
                    ("annotation", some(self.expr(annotation))),
                    ("value", self.opt_expr(value)),
                    ("simple", some((*simple as u8).to_string())),
                ],
            ),
            StmtKind::For {
                target,
                iter,
                body,
                orelse,
                is_async,
            } => node(
                if *is_async { "AsyncFor" } else { "For" },
                pos,
                vec![
                    ("target", some(self.expr(target))),
                    ("iter", some(self.expr(iter))),
                    ("body", self.stmts(body)),
                    ("orelse", self.stmts(orelse)),
                ],
            ),
            StmtKind::While { test, body, orelse } => node(
                "While",
                pos,
                vec![
                    ("test", some(self.expr(test))),
                    ("body", self.stmts(body)),
                    ("orelse", self.stmts(orelse)),
                ],
            ),
            StmtKind::If { test, body, orelse } => node(
                "If",
                pos,
                vec![
                    ("test", some(self.expr(test))),
                    ("body", self.stmts(body)),
                    ("orelse", self.stmts(orelse)),
                ],
            ),
            StmtKind::With {
                items,
                body,
                is_async,
            } => node(
                if *is_async { "AsyncWith" } else { "With" },
                pos,
                vec![
                    (
                        "items",
                        lst(items
                            .iter()
                            .map(|i| {
                                node(
                                    "withitem",
                                    None,
                                    vec![
                                        ("context_expr", some(self.expr(&i.context_expr))),
                                        ("optional_vars", self.opt_expr(&i.optional_vars)),
                                    ],
                                )
                            })
                            .collect()),
                    ),
                    ("body", self.stmts(body)),
                ],
            ),
            StmtKind::Match { subject, cases } => node(
                "Match",
                pos,
                vec![
                    ("subject", some(self.expr(subject))),
                    (
                        "cases",
                        lst(cases
                            .iter()
                            .map(|c| {
                                node(
                                    "match_case",
                                    None,
                                    vec![
                                        ("pattern", some(self.pattern(&c.pattern))),
                                        ("guard", self.opt_expr(&c.guard)),
                                        ("body", self.stmts(&c.body)),
                                    ],
                                )
                            })
                            .collect()),
                    ),
                ],
            ),
            StmtKind::Raise { exc, cause } => node(
                "Raise",
                pos,
                vec![("exc", self.opt_expr(exc)), ("cause", self.opt_expr(cause))],
            ),
            StmtKind::Try {
                body,
                handlers,
                orelse,
                finalbody,
                is_star,
            } => node(
                if *is_star { "TryStar" } else { "Try" },
                pos,
                vec![
                    ("body", self.stmts(body)),
                    (
                        "handlers",
                        lst(handlers
                            .iter()
                            .map(|h| {
                                node(
                                    "ExceptHandler",
                                    self.p(h.pos),
                                    vec![
                                        ("type", self.opt_expr(&h.typ)),
                                        ("name", h.name.as_ref().map(|n| py_str_repr(n))),
                                        ("body", self.stmts(&h.body)),
                                    ],
                                )
                            })
                            .collect()),
                    ),
                    ("orelse", self.stmts(orelse)),
                    ("finalbody", self.stmts(finalbody)),
                ],
            ),
            StmtKind::Assert { test, msg } => node(
                "Assert",
                pos,
                vec![("test", some(self.expr(test))), ("msg", self.opt_expr(msg))],
            ),
            StmtKind::Import(n) => node("Import", pos, vec![("names", Self::aliases(n))]),
            StmtKind::ImportFrom {
                module,
                names,
                level,
            } => node(
                "ImportFrom",
                pos,
                vec![
                    ("module", module.as_ref().map(|m| py_str_repr(m))),
                    ("names", Self::aliases(names)),
                    ("level", some(level.to_string())),
                ],
            ),
            StmtKind::Global(n) => node("Global", pos, vec![("names", Self::names(n))]),
            StmtKind::Nonlocal(n) => node("Nonlocal", pos, vec![("names", Self::names(n))]),
            StmtKind::Expr(e) => node("Expr", pos, vec![("value", some(self.expr(e)))]),
            StmtKind::Pass => node("Pass", pos, vec![]),
            StmtKind::Break => node("Break", pos, vec![]),
            StmtKind::Continue => node("Continue", pos, vec![]),
        }
    }

    fn module(&self, m: &Module) -> String {
        node("Module", None, vec![("body", self.stmts(&m.body))])
    }
}

/// Renders a module exactly like CPython's `ast.dump(tree)`.
fn dump(m: &Module) -> String {
    Dumper { pos: false }.module(m)
}

fn dump_pos(m: &Module) -> String {
    Dumper { pos: true }.module(m)
}

fn d(src: &str) -> String {
    match parse(src, "<test>") {
        Ok(m) => dump(&m),
        Err(e) => panic!("parse failed for {src:?}: {e}"),
    }
}

fn dump_or_error(src: &str) -> String {
    match parse(src, "<test>") {
        Ok(m) => dump(&m),
        Err(_) => "SyntaxError".into(),
    }
}

fn err(src: &str) -> String {
    match parse(src, "<test>") {
        Ok(_) => panic!("expected syntax error for {src:?}"),
        Err(e) => e.msg,
    }
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/parser_corpus")
}

const PY_DUMP: &str = r#"
import ast, sys
sep = "\n#---\n"
srcs = sys.stdin.read().split(sep)
out = []
for s in srcs:
    try:
        out.append(ast.dump(ast.parse(s)))
    except (SyntaxError, ValueError, RecursionError, MemoryError):
        out.append("SyntaxError")
sys.stdout.write(sep.join(out))
"#;

const PY_DUMP_POS: &str = r#"
import ast, sys
sep = "\n#---\n"

LINES = []

def dump(n):
    if isinstance(n, list):
        return "[" + ", ".join(dump(x) for x in n) + "]"
    if not isinstance(n, ast.AST):
        return repr(n)
    parts = []
    for f in n._fields:
        v = getattr(n, f, None)
        if v is None and f in ("value",) and isinstance(n, (ast.Constant, ast.MatchSingleton)):
            parts.append("value=None"); continue
        if v is None or v == []:
            continue
        parts.append(f + "=" + dump(v))
    at = ""
    if isinstance(n, (ast.stmt, ast.expr, ast.excepthandler, ast.arg, ast.keyword)):
        col = len(LINES[n.lineno - 1].encode()[:n.col_offset].decode(errors="replace"))
        at = "@%d:%d" % (n.lineno, col)
    return type(n).__name__ + at + "(" + ", ".join(parts) + ")"

srcs = sys.stdin.read().split(sep)
out = []
for s in srcs:
    try:
        LINES[:] = s.split("\n")
        out.append(dump(ast.parse(s)))
    except (SyntaxError, ValueError, RecursionError, MemoryError):
        out.append("SyntaxError")
sys.stdout.write(sep.join(out))
"#;

fn python_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_python(script: &str, srcs: &[String]) -> Vec<String> {
    use std::io::Write;
    let mut child = Command::new("python3")
        .args(["-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn python3");
    let input = srcs.join(SEP);
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        stdin.write_all(input.as_bytes()).unwrap();
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .split(SEP)
        .map(str::to_string)
        .collect()
}

fn snippets() -> Vec<String> {
    let text = std::fs::read_to_string(corpus_dir().join("snippets.txt")).unwrap();
    text.split(SEP).map(str::to_string).collect()
}

fn corpus_files() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(corpus_dir())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "py"))
        .collect();
    v.sort();
    v
}

fn all_corpus_sources() -> Vec<String> {
    let mut v = snippets();
    for f in corpus_files() {
        v.push(std::fs::read_to_string(f).unwrap());
    }
    v
}

#[test]
fn corpus_matches_stored_dumps() {
    let expected_path = corpus_dir().join("expected.txt");
    let srcs = all_corpus_sources();
    if std::env::var_os("LUMEN_PY_BLESS").is_some() {
        let dumps = run_python(PY_DUMP, &srcs);
        std::fs::write(&expected_path, dumps.join(SEP)).unwrap();
    }
    let expected: Vec<String> = std::fs::read_to_string(expected_path)
        .unwrap()
        .split(SEP)
        .map(str::to_string)
        .collect();
    assert_eq!(
        expected.len(),
        srcs.len(),
        "expected.txt is stale; rerun with LUMEN_PY_BLESS=1"
    );
    let mut failures = Vec::new();
    for (src, want) in srcs.iter().zip(&expected) {
        let got = dump_or_error(src);
        if &got != want {
            failures.push(format!(
                "--- source:\n{src}\n--- expected:\n{want}\n--- got:\n{got}\n"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn corpus_matches_live_python() {
    if !python_available() {
        return;
    }
    let srcs = all_corpus_sources();
    let want = run_python(PY_DUMP, &srcs);
    let mut failures = Vec::new();
    for (src, want) in srcs.iter().zip(&want) {
        let got = dump_or_error(src);
        if &got != want {
            failures.push(format!(
                "--- source:\n{src}\n--- python:\n{want}\n--- got:\n{got}\n"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn corpus_positions_match_python() {
    if !python_available() {
        return;
    }
    let srcs = all_corpus_sources();
    let want = run_python(PY_DUMP_POS, &srcs);
    let mut failures = Vec::new();
    for (src, want) in srcs.iter().zip(&want) {
        let got = match parse(src, "<t>") {
            Ok(m) => dump_pos(&m),
            Err(_) => "SyntaxError".into(),
        };
        if &got != want {
            failures.push(format!(
                "--- source:\n{src}\n--- python:\n{want}\n--- got:\n{got}\n"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Rewrites every non-ASCII char and `\u`/`\U` escape as `<U+XXXX>`, so the comparison does not
/// depend on how closely the dumper's printability rules track Python's.
fn normalize_unicode(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let n = match (c, chars.get(i + 1)) {
            ('\\', Some('u')) => 4,
            ('\\', Some('U')) => 8,
            _ => 0,
        };
        let hex: String = chars.iter().skip(i + 2).take(n).collect();
        if n > 0 && hex.len() == n && hex.chars().all(|h| h.is_ascii_hexdigit()) {
            out.push_str(&format!(
                "<U+{:X}>",
                u32::from_str_radix(&hex, 16).unwrap_or(0)
            ));
            i += 2 + n;
        } else if c.is_ascii() {
            out.push(c);
            i += 1;
        } else {
            out.push_str(&format!("<U+{:X}>", c as u32));
            i += 1;
        }
    }
    out
}

fn has_surrogate_escape(normalized: &str) -> bool {
    normalized.match_indices("<U+D").any(|(i, _)| {
        let rest = &normalized[i + 4..];
        rest.len() > 3
            && rest.as_bytes()[3] == b'>'
            && matches!(rest.as_bytes()[0], b'8'..=b'9' | b'A'..=b'F')
    })
}

fn stdlib_dir() -> Option<PathBuf> {
    let out = Command::new("python3")
        .args([
            "-c",
            "import sysconfig; print(sysconfig.get_paths()['stdlib'])",
        ])
        .output()
        .ok()?;
    Some(PathBuf::from(String::from_utf8(out.stdout).ok()?.trim()))
}

fn collect_py(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if p.is_dir() {
            if name != "site-packages" && name != "__pycache__" {
                collect_py(&p, out);
            }
        } else if name.ends_with(".py") {
            out.push(p);
        }
    }
}

/// Parses every stdlib `.py` file; with `LUMEN_PY_STDLIB_DIFF=1` also compares each AST dump
/// with CPython's. Run with `cargo test -p lumen-py --test parser -- --ignored --nocapture`.
#[test]
#[ignore]
fn stdlib_parse() {
    let Some(root) = stdlib_dir() else { return };
    let mut files = Vec::new();
    collect_py(&root, &mut files);
    files.sort();
    let with_pos = std::env::var_os("LUMEN_PY_STDLIB_POS").is_some();
    let diff = with_pos || std::env::var_os("LUMEN_PY_STDLIB_DIFF").is_some();
    let (mut ok, mut failed, mut mismatched, mut known) = (0, Vec::new(), Vec::new(), 0);
    let mut sources = Vec::new();
    let mut names = Vec::new();
    for f in &files {
        let Ok(src) = std::fs::read_to_string(f) else {
            continue;
        };
        match parse(&src, "<stdlib>") {
            Ok(_) => {
                ok += 1;
                if diff {
                    names.push(f.clone());
                    sources.push(src);
                }
            }
            Err(e) => failed.push(format!("{}: {e}", f.display())),
        }
    }
    println!("parsed {ok}/{} files", ok + failed.len());
    for f in failed.iter().take(40) {
        println!("FAIL {f}");
    }
    if diff {
        for (chunk_names, chunk_src) in names.chunks(200).zip(sources.chunks(200)) {
            let want = run_python(if with_pos { PY_DUMP_POS } else { PY_DUMP }, chunk_src);
            for ((name, src), want) in chunk_names.iter().zip(chunk_src).zip(want) {
                let want = normalize_unicode(&want).replace(", kind='u'", "");
                let known_gap = want.contains("type_params=") || has_surrogate_escape(&want);
                let got = match parse(src, "<stdlib>") {
                    Ok(m) if with_pos => dump_pos(&m),
                    Ok(m) => dump(&m),
                    Err(_) => "SyntaxError".into(),
                };
                let got = normalize_unicode(&got);
                if got != want {
                    if known_gap {
                        known += 1;
                        continue;
                    }
                    let at = got
                        .bytes()
                        .zip(want.bytes())
                        .position(|(a, b)| a != b)
                        .unwrap_or(0);
                    let ctx = |s: &str| {
                        let from = s
                            .char_indices()
                            .map(|(i, _)| i)
                            .rfind(|&i| i <= at.saturating_sub(150))
                            .unwrap_or(0);
                        let to = s
                            .char_indices()
                            .map(|(i, _)| i)
                            .find(|&i| i >= at + 150)
                            .unwrap_or(s.len());
                        s[from..to].to_string()
                    };
                    mismatched.push(format!(
                        "{}\n  got:  {}\n  want: {}",
                        name.display(),
                        ctx(&got),
                        ctx(&want)
                    ));
                }
            }
        }
        println!("{known} known-gap dump mismatches (type params, lone surrogates)");
        println!("{} other dump mismatches", mismatched.len());
        for m in mismatched.iter().take(40) {
            println!("MISMATCH {m}");
        }
    }
}

#[test]
fn parse_speed_10k_lines() {
    let block = "def f(a, b=1, *args, **kw):\n    x = [i * 2 for i in range(a) if i % 3]\n    if x and b:\n        return {'k': x, \"v\": f'{a!r:>{b}}'}\n    else:\n        return (a + b) * 3 - a ** 2 // b\n\n";
    let src = block.repeat(10_000 / 7);
    let start = std::time::Instant::now();
    let m = parse(&src, "<speed>").unwrap();
    assert!(!m.body.is_empty());
    assert!(
        start.elapsed().as_secs_f64() < 1.0,
        "took {:?}",
        start.elapsed()
    );
}

#[test]
fn deep_nesting_errors() {
    for open in ["(", "[", "{"] {
        let src = open.repeat(10_000);
        assert!(err(&src).contains("too many nested") || err(&src).contains("never closed"));
    }
    let src = format!("{}1{}", "(".repeat(150), ")".repeat(150));
    let _ = parse(&src, "<t>");
    let src = format!("x = {}1", "-".repeat(100_000));
    assert!(err(&src).contains("too many nested"));
    let src = format!("x = {}1", "not ".repeat(100_000));
    assert!(err(&src).contains("too many nested"));
    let src = format!("x = {}1", "lambda: ".repeat(100_000));
    assert!(err(&src).contains("too many nested"));
    let src = format!("x = 1{}", " ** 1".repeat(100_000));
    assert!(parse(&src, "<t>").is_ok() || err(&src).contains("too many nested"));
    let src = format!("x = f'{}'", "{f'".repeat(1000) + &"}'".repeat(1000));
    assert!(parse(&src, "<t>").is_err());
}

#[test]
fn nested_parens_within_limit_parse() {
    let src = format!("x = {}1{}\n", "(".repeat(30), ")".repeat(30));
    assert!(parse(&src, "<t>").is_ok());
    let src = format!("x = {}1{}\n", "[".repeat(30), "]".repeat(30));
    assert!(parse(&src, "<t>").is_ok());
}

#[test]
fn never_panics_on_garbage() {
    let pieces = [
        "(", ")", "[", "]", "{", "}", "'", "\"", "f'", "f\"{", "\\", "\n", "    ", "\t", ":", ",",
        "=", "lambda", "def ", "class ", "if ", "else", "for ", "in ", "match ", "case ", "*",
        "**", "@", "...", "0x", "1e", "1_", "\u{e9}", "#", "async ", "await ", "yield ", "not ",
        "is ", "as ", "with ", "try:", "except", "import ", "from ", ".", ";", "->", ":=", "!",
        "{x!r:", "}}", "{{", "'''", "\r\n", "\0",
    ];
    let mut seed: u64 = 0x9e3779b97f4a7c15;
    for _ in 0..3000 {
        let mut s = String::new();
        for _ in 0..(seed % 12) + 1 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            s.push_str(pieces[(seed >> 33) as usize % pieces.len()]);
        }
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let _ = parse(&s, "<fuzz>");
    }
}

#[test]
fn simple_dump() {
    assert_eq!(
        d("x = 1\n"),
        "Module(body=[Assign(targets=[Name(id='x', ctx=Store())], value=Constant(value=1))])"
    );
    assert_eq!(d(""), "Module()");
}

#[test]
fn positions() {
    let m = parse("x = 1\nif a:\n    foo(b, c)\n", "<t>").unwrap();
    assert_eq!(m.body[0].pos, Pos { line: 1, col: 0 });
    let StmtKind::If { body, test, .. } = &m.body[1].kind else {
        panic!()
    };
    assert_eq!(m.body[1].pos, Pos { line: 2, col: 0 });
    assert_eq!(test.pos, Pos { line: 2, col: 3 });
    assert_eq!(body[0].pos, Pos { line: 3, col: 4 });
    let StmtKind::Expr(call) = &body[0].kind else {
        panic!()
    };
    let ExprKind::Call { args, .. } = &call.kind else {
        panic!()
    };
    assert_eq!(args[1].pos, Pos { line: 3, col: 11 });
}

#[test]
fn error_positions_and_messages() {
    let e = parse("x = (1,\n", "<t>").unwrap_err();
    assert!(e.msg.contains("was never closed"));
    assert_eq!((e.line, e.col), (1, 4));
    let e = parse("a = 1\nb = = 2\n", "<t>").unwrap_err();
    assert_eq!(e.line, 2);
    assert!(err("f() = 1").contains("cannot assign to function call"));
    assert!(err("1 = x").contains("cannot assign to literal"));
    assert!(err("del f()").contains("cannot delete function call"));
    assert!(err("x +\n").contains("invalid syntax"));
    assert!(err("if x:\npass\n").contains("expected an indented block"));
    assert!(err("  x = 1\n").contains("unexpected indent"));
    assert!(err("a b").contains("invalid syntax"));
    assert!(err("f(a=1, 2)").contains("positional argument follows keyword argument"));
    assert!(err("def f(a=1, b): pass").contains("non-default argument"));
    assert!(err("b'a' 'b'").contains("cannot mix bytes and nonbytes"));
    assert!(err("try:\n    pass\n").contains("expected 'except' or 'finally'"));
    assert!(!err("x = yield = 1").is_empty());
    assert!(err("for x in y:\n    pass\n  z\n").contains("unindent"));
    assert!(err("type X = int").contains("type statements"));
    assert!(err("x = t'a'").contains("t-strings"));
}

#[test]
fn contexts() {
    assert_eq!(
        d("a, (b, [c, *d]) = e.f[0] = g"),
        "Module(body=[Assign(targets=[Tuple(elts=[Name(id='a', ctx=Store()), Tuple(elts=[Name(id='b', ctx=Store()), List(elts=[Name(id='c', ctx=Store()), Starred(value=Name(id='d', ctx=Store()), ctx=Store())], ctx=Store())], ctx=Store())], ctx=Store()), Subscript(value=Attribute(value=Name(id='e', ctx=Load()), attr='f', ctx=Load()), slice=Constant(value=0), ctx=Store())], value=Name(id='g', ctx=Load()))])"
    );
    assert!(d("del a.b, c[0], (d, e)").contains("ctx=Del()"));
}

#[test]
fn precedence() {
    assert_eq!(
        d("-2**2"),
        "Module(body=[Expr(value=UnaryOp(op=USub(), operand=BinOp(left=Constant(value=2), op=Pow(), right=Constant(value=2))))])"
    );
    assert_eq!(d("2**3**4"), dump_of_right_assoc_pow());
    assert_eq!(
        d("a or b and not c"),
        "Module(body=[Expr(value=BoolOp(op=Or(), values=[Name(id='a', ctx=Load()), BoolOp(op=And(), values=[Name(id='b', ctx=Load()), UnaryOp(op=Not(), operand=Name(id='c', ctx=Load()))])]))])"
    );
}

fn dump_of_right_assoc_pow() -> String {
    "Module(body=[Expr(value=BinOp(left=Constant(value=2), op=Pow(), right=BinOp(left=Constant(value=3), op=Pow(), right=Constant(value=4))))])".into()
}

#[test]
fn soft_keywords_as_identifiers() {
    for src in [
        "match = 1\ncase = 2\n_ = 3\n",
        "match(x)\n",
        "match[x] = 1\n",
        "print(match, case)\n",
        "match.x = 1\n",
        "match x:\n    case _:\n        pass\n",
        "type = 1\nprint(type(x))\n",
    ] {
        assert!(parse(src, "<t>").is_ok(), "{src}");
    }
}

#[test]
fn fstring_shapes() {
    assert_eq!(
        d("f'a{x!r:>{w}}b{{c}}'"),
        "Module(body=[Expr(value=JoinedStr(values=[Constant(value='a'), FormattedValue(value=Name(id='x', ctx=Load()), conversion=114, format_spec=JoinedStr(values=[Constant(value='>'), FormattedValue(value=Name(id='w', ctx=Load()), conversion=-1)])), Constant(value='b{c}')]))])"
    );
    assert!(d("f'{x=}'").contains("Constant(value='x=')"));
    assert!(d("f'{a[\"k\"]}'").contains("Subscript"));
    assert!(d("f\"{f'{x}'}\"").contains("JoinedStr(values=[FormattedValue(value=JoinedStr"));
    assert!(d("f'''{\n1 +\n2}'''").contains("BinOp"));
    assert!(err("f'{'").contains("unterminated") || err("f'{'").contains("f-string"));
    assert!(err("f'}'").contains("single '}'"));
    assert!(err("f'{}'").contains("valid expression required"));
}

#[test]
fn big_int_literal() {
    assert!(d("0xffffffffffffffffffffffffffffffff")
        .contains("value=340282366920938463463374607431768211455"));
}

#[test]
fn string_concat() {
    assert_eq!(
        d("'a' 'b' \"c\""),
        "Module(body=[Expr(value=Constant(value='abc'))])"
    );
    assert_eq!(
        d("b'a' b'b'"),
        "Module(body=[Expr(value=Constant(value=b'ab'))])"
    );
}

#[test]
fn prefixes_and_deletions_never_panic() {
    for src in snippets() {
        let chars: Vec<char> = src.chars().collect();
        for end in 0..=chars.len() {
            let prefix: String = chars[..end].iter().collect();
            let _ = parse(&prefix, "<prefix>");
        }
        for skip in 0..chars.len() {
            let s: String = chars
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != skip)
                .map(|(_, c)| *c)
                .collect();
            let _ = parse(&s, "<del>");
        }
    }
}
