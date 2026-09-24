//! Legacy `f.arguments` / `f.caller` reflection for compiled activations.
//!
//! The tree-walker stashes what a reflective `f.arguments` read needs (the callee, the actual
//! arguments, the scope) on every sloppy activation's [`FnFrame`](crate::interpreter::FnFrame).
//! A compiled frame keeps no such record on its hot path: the stash — an `Rc<[Value]>` of the
//! arguments per call — is only paid once some loaded source could perform the read at all,
//! i.e. mentions `.arguments` / `.caller` (or those names as string keys). [`note_source`] runs
//! over every script, eval and dynamic-function source before it executes, so no activation of
//! a function that could be reflected on predates the flag.
//!
//! The conjured object is an unmapped snapshot of the actual arguments (a compiled body's
//! parameters live in slots, which a mapped object cannot alias): unlike the tree-walker's, it
//! does not reflect a parameter reassigned before the read.
use crate::interpreter::{Env, Interp};
use crate::value::{Callable, Value};
use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether compiled sloppy activations record their arguments for reflection.
#[inline(always)]
pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// The flag's address (a `bool` byte), read by JIT direct call sites.
pub(crate) fn enabled_addr() -> usize {
    ENABLED.as_ptr() as usize
}

/// Enable reflection support when `src` could read `f.arguments` / `f.caller`.
pub(crate) fn note_source(src: &str) {
    if enabled() {
        return;
    }
    fn ident_end(rest: &str) -> bool {
        !rest
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$')
    }
    let mut hit = false;
    for name in ["arguments", "caller"] {
        for (at, _) in src.match_indices(name) {
            let before = src[..at].trim_end().chars().next_back();
            let after = &src[at + name.len()..];
            if (before == Some('.') && ident_end(after))
                || (matches!(before, Some('\'' | '"' | '`'))
                    && after.starts_with(['\'', '"', '`']))
            {
                hit = true;
                break;
            }
        }
    }
    if hit {
        ENABLED.store(true, Ordering::Relaxed);
    }
}

/// Record the innermost (just pushed) compiled frame's arguments: the body's own `arguments`
/// object when it has one, else the actual arguments to build one from on a reflective read.
#[cold]
#[inline(never)]
pub(super) fn stash(i: &mut Interp, env: &Env, args: &[Value], own: Option<&Value>) {
    let Some(fr) = i.fn_frames.last_mut() else {
        return;
    };
    let x = fr.extra.get_or_insert_with(Default::default);
    if let Some(ao) = own {
        x.args_obj = ao.clone();
        return;
    }
    let callee = fr.callee();
    let func = match &callee.borrow().call {
        Callable::User(u) => u.func.clone(),
        _ => return,
    };
    let x = i.fn_frames.last_mut().unwrap().extra.as_deref_mut().unwrap();
    x.lazy = Some((func, std::rc::Rc::from(args), env.clone()));
    x.unmapped = true;
}

#[cfg(test)]
mod tests {
    use crate::bytecode::Tier;
    use crate::{Completion, Engine};

    #[test]
    fn compiled_sloppy_frames_reflect_arguments_and_caller() {
        let src = r#"
            var obj = { test: function(a) { var r = obj.test.arguments;
                return [r !== null, r[0], r[1], r.length, r.callee === obj.test].join(); } };
            function foo() { return [foo.arguments.length, foo.caller === null, outer()].join(); }
            function outer() { return inner(); }
            function inner() { return inner.caller === outer; }
            function uses() { arguments[0] = 3; return uses.arguments === arguments; }
            [obj.test(5, undefined), foo(), foo.arguments, foo.caller, uses(1)].join('|')
        "#;
        let mut out = Vec::new();
        for tier in [Tier::Interp, Tier::Bytecode] {
            let mut e = Engine::new();
            e.set_tier(tier);
            e.set_tier_threshold(0);
            out.push(match e.eval(src, false).unwrap() {
                Completion::Value(v) => v.to_string(),
                Completion::Throw { name, message } => format!("throw {name}: {message}"),
            });
        }
        assert_eq!(out[0], out[1]);
        assert_eq!(out[1], "true,5,,2,true|0,true,true|||true");
    }
}
