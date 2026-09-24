//! Default-parameter eligibility and initialization in local or captured bindings.
use super::{CResult, Compiler, Op};
use crate::ast::{ArrayElem, Expr, Function, HoistOp, PropDef, PropKey};

impl Compiler {
    pub(super) fn parameter_default(
        &mut self,
        slot: u16,
        expr: &Expr,
        cap: Option<u32>,
    ) -> CResult {
        self.emit(Op::LoadLocal(slot));
        self.emit(Op::Undef);
        self.emit(Op::StrictEq);
        let skip = self.emit(Op::JumpIfFalse(0));
        self.expr(expr)?;
        self.emit(match cap {
            Some(name) => Op::StoreCap(name),
            None => Op::StoreLocal(slot),
        });
        self.patch(skip);
        Ok(())
    }
}

pub(super) fn captured_default_safe(func: &Function, name: &str, _expr: &Expr) -> bool {
    // The expression itself passes `default_expr_safe` (checked by the caller): no closures,
    // so nothing can observe the activation binding before the default is stored into it —
    // reads of earlier parameters see initialized bindings, later ones are banned.
    // Hoisted function bindings are seeded into the activation before bytecode runs. Do not
    // overwrite one with a default; supporting that case needs separate parameter/body scopes.
    !crate::interpreter::collect_hoist_ops(&func.body(), func.is_strict, &[])
        .iter()
        .any(|op| matches!(op, HoistOp::Fn(n, _) | HoistOp::AnnexB(n, _) if n == name))
}

/// [`default_expr_safe`] over every expression inside a destructuring parameter pattern
/// (property defaults and computed keys, nested patterns included).
pub(super) fn pattern_exprs_safe(
    p: &crate::ast::Pattern,
    banned: &std::collections::HashSet<&str>,
) -> bool {
    use crate::ast::{ArrayPatElem, Pattern};
    match p {
        Pattern::Ident(_) => true,
        Pattern::Object(o) => o.props.iter().all(|prop| {
            (match &prop.key {
                PropKey::Computed(k) => default_expr_safe(k, banned),
                _ => true,
            }) && prop.default.as_ref().is_none_or(|d| default_expr_safe(d, banned))
                && pattern_exprs_safe(&prop.value, banned)
        }),
        Pattern::Array(elems) => elems.iter().all(|e| match e {
            ArrayPatElem::Hole => true,
            ArrayPatElem::Elem { pattern, default } => {
                default.as_ref().is_none_or(|d| default_expr_safe(d, banned))
                    && pattern_exprs_safe(pattern, banned)
            }
            ArrayPatElem::Rest(p) => pattern_exprs_safe(p, banned),
        }),
        Pattern::Member(_) => false,
    }
}

/// Whether a parameter default is in the compiler's lowerable subset: no reference to any
/// *banned* name (this parameter itself or a later one — the spec's param-scope TDZ would throw
/// where slots would read a seeded `undefined`), and no nested function/class (whose capture
/// analysis of a *parameter expression* scope the slot model doesn't carry). Whitelist
/// recursion: unknown constructs answer false (the function stays on the tree-walker).
pub(super) fn default_expr_safe(e: &Expr, banned: &std::collections::HashSet<&str>) -> bool {
    match e {
        Expr::Num(_)
        | Expr::BigInt(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Undefined
        | Expr::This
        | Expr::Regex { .. } => true,
        Expr::Ident(n) => !banned.contains(n.as_str()),
        Expr::Paren(x) | Expr::ToStr(x) | Expr::Unary { arg: x, .. } => {
            default_expr_safe(x, banned)
        }
        Expr::Update { arg, .. } => default_expr_safe(arg, banned),
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            default_expr_safe(left, banned) && default_expr_safe(right, banned)
        }
        Expr::Cond { test, cons, alt } => {
            default_expr_safe(test, banned)
                && default_expr_safe(cons, banned)
                && default_expr_safe(alt, banned)
        }
        Expr::Member { obj, .. } => default_expr_safe(obj, banned),
        Expr::Index { obj, index, .. } => {
            default_expr_safe(obj, banned) && default_expr_safe(index, banned)
        }
        Expr::Call { callee, args, .. } | Expr::New { callee, args, .. } => {
            default_expr_safe(callee, banned)
                && args.iter().all(|a| match a {
                    ArrayElem::Item(x) | ArrayElem::Spread(x) => default_expr_safe(x, banned),
                    ArrayElem::Hole => true,
                })
        }
        Expr::Array(elems) => elems.iter().all(|a| match a {
            ArrayElem::Item(x) | ArrayElem::Spread(x) => default_expr_safe(x, banned),
            ArrayElem::Hole => true,
        }),
        Expr::Object(props) => props.iter().all(|p| match p {
            PropDef::KeyValue { key, value } => {
                !matches!(key, PropKey::Computed(_)) && default_expr_safe(value, banned)
            }
            _ => false,
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use crate::ast::Stmt;
    use crate::bytecode::{compile, Op};

    fn compiled(source: &str) -> Option<std::rc::Rc<crate::bytecode::Chunk>> {
        let stmts =
            crate::parser::parse_script(source, false).unwrap_or_else(|e| panic!("{}", e.message));
        let Stmt::FuncDecl(f) = &stmts[0] else {
            panic!("function expected")
        };
        compile(f)
    }

    #[test]
    fn captured_literal_default_initializes_its_binding() {
        let chunk = compiled("function f(x={}) { return () => x; }")
            .expect("literal captured default should compile");
        assert!(chunk
            .jit_ops()
            .iter()
            .any(|op| matches!(op, Op::StoreCap(_))));
    }

    #[test]
    fn closure_defaults_or_hoist_conflicts_stay_in_the_oracle() {
        // An effectful default cannot observe the activation binding it initializes (no
        // closure in it can close over the parameter scope): it compiles.
        assert!(compiled("function f(x=make()) { return () => x; }").is_some());
        assert!(compiled("function f(x={}) { function x() {} return () => x; }").is_none());
        assert!(compiled("function f(x=()=>1) { return () => x; }").is_none());
    }

    #[test]
    fn captured_effectful_defaults_and_rest_match_the_oracle() {
        use crate::bytecode::Tier;
        use crate::{Completion, Engine};
        let src = "var n = 0; function make() { n++; return n; }
            function f(a, x = make() + a, ...rest) { return () => [a, x, rest.length, n].join(); }
            [f(1)(), f(1, 7)(), f(2, undefined, 3, 4)()].join('|')";
        let mut out = Vec::new();
        for tier in [Tier::Interp, Tier::Bytecode] {
            let mut e = Engine::new();
            e.interp.tier = tier;
            e.interp.tier_threshold = 0;
            out.push(match e.eval(src, false).expect("parse") {
                Completion::Value(v) => v,
                Completion::Throw { name, message } => panic!("threw {name}: {message}"),
            });
        }
        assert_eq!(out[0], "1,2,0,1|1,7,0,1|2,4,2,2");
        assert_eq!(out[0], out[1]);
    }
}

/// Whether the rest parameter `name` of `func` is provably never read, so its array need not
/// be built: the only identifier-bounded mention of `name` in the function's source text is the
/// declaration itself. Textual and conservative — any other mention (a property name, a string,
/// a comment, a nested function) counts as a use — and no dynamic reach: no `eval` anywhere in
/// the text, and no `\` (a unicode-escaped identifier could spell the name).
pub(super) fn rest_unused(func: &Function, name: &str) -> bool {
    // A precompiled function's text is compressed (kept only for `toString`); decompressing it
    // for this would hold a whole store block for the process's life.
    if matches!(func.source, crate::ast::FnSource::Kept { .. }) {
        return false;
    }
    let Some(src) = func.source.as_str() else {
        return false;
    };
    if src.contains("eval") || src.contains('\\') {
        return false;
    }
    let is_id = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut mentions = 0;
    for (at, _) in src.match_indices(name) {
        let before = src[..at].chars().next_back();
        let after = src[at + name.len()..].chars().next();
        if !before.is_some_and(is_id) && !after.is_some_and(is_id) {
            mentions += 1;
            if mentions > 1 {
                return false;
            }
        }
    }
    mentions == 1
}
