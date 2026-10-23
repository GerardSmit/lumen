//! Legacy sloppy-mode `f.arguments` / `f.caller`, as V8 has them.
//!
//! ## The properties
//! A sloppy ordinary function (not an arrow, method, generator, async function or class) has two
//! own non-configurable properties, `arguments` and `caller` (in V8's key order), that read
//! `null` while the function is not running and ignore writes (a TypeError in strict code).
//! They are accessors without a setter whose getters are the [`ARGUMENTS_GETTER`] /
//! [`CALLER_GETTER`] intrinsics, so every property path (the inline caches included) calls into
//! the computation below. V8 reports them as non-writable data properties, but a value that
//! changes behind a non-writable, non-configurable data descriptor breaks the object-model
//! invariant test262 checks (`built-ins/Object/internals/DefineOwnProperty/
//! consistent-value-function-*`), so `Object.getOwnPropertyDescriptor` shows the accessor.
//! Every other function inherits %Function.prototype%'s %ThrowTypeError% accessors. The two
//! accessor boxes live on a realm intrinsic object, and each closure shares them
//! (`value::Accessors` is copy-on-write). AST map templates retain only neutral
//! descriptor placeholders, so a cached/shared AST cannot retain another realm.
//!
//! ## `f.caller`
//! Computed from the frame stack on the read (the same walk a stack trace does, see
//! `interpreter::stack_trace`): the innermost activation of `f`, then the next frame below it
//! that is a function (top-level and eval code are skipped). `null` when there is none, when it
//! is strict, or when `f` was called back from a native function: a native tags
//! `Interp::cur_site` with `SITE_NATIVE` while it runs (see `Interp::dispatch_native`), and the
//! callback's frame records that tag as its caller site. Natives V8 implements as call
//! adaptors or C++ builtins (`Function.prototype.call` / `apply`, `Reflect.apply` /
//! `construct`, `eval`, `JSON.parse` / `stringify`) clear the tag ([`native_transparent`],
//! [`caller_transparent`]).
//!
//! ## `f.arguments`
//! A fresh unmapped arguments object for the innermost activation, built on the read. Its
//! elements are V8's: a parameter V8 keeps in a register (simple parameter list, no `arguments`
//! object, no direct `eval`, not captured by a closure, the last of duplicate names) shows its
//! *current* value; every other element (captured, mapped, defaulted or destructured
//! parameters, surplus arguments) the value it was called with.
//!
//! What the read needs is recorded per activation ([`ReflectStash`]: the actual arguments and
//! where the parameters live — the tree-walker's scope, or a compiled frame's slot array) only
//! once a read can happen: [`note_source`] flips [`enabled`] when a parsed source mentions
//! `.arguments` / `.caller` or those names as computed string keys (the first `f.arguments` read in the
//! process flips it too), and every sloppy activation checks it next to its existing
//! `arguments`-object test. Until then no call does any extra work, and JIT direct call sites
//! take their slow path (which records it) once the flag is up. An activation that started
//! before the flag went up (a read spelled some other way, or from precompiled code that was
//! never parsed) or that ran as whole-function native code has no record: its read yields an
//! arguments object with just `length` 0.
//!
//! Limitation: inside a hot loop compiled by the JIT, a parameter held in a register is written
//! back to its slot only when the loop exits, so a read from a callee made inside that loop sees
//! the value it had on loop entry.
use crate::ast::{Function, Pattern};
use crate::interpreter::frames::{site_is_native, FnFrame, ReflectStash, SITE_NATIVE};
use crate::interpreter::{Env, Interp};
use crate::value::{Callable, Gc, Object, Property, Value};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// `extra_protos` keys of the two getter intrinsics.
pub(crate) const ARGUMENTS_GETTER: &str = "%FunctionArgumentsGetter%";
pub(crate) const CALLER_GETTER: &str = "%FunctionCallerGetter%";
const OWN_PROPS: &str = "%FunctionLegacyOwnProps%";

/// Whether activations record what a reflective `f.arguments` read needs.
#[inline(always)]
pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// The flag's address (a `bool` byte), read by JIT direct call sites.
pub(crate) fn enabled_addr() -> usize {
    ENABLED.as_ptr() as usize
}

/// Raise [`enabled`] before `src` runs when it could read `f.arguments` / `f.caller` (it mentions
/// `.arguments` / `.caller`, or those names as computed string keys): every parse runs this, so the
/// activation a script's first read is made from already records what the read needs.
pub(crate) fn note_source(src: &str) {
    if !enabled() && may_reflect(src) {
        ENABLED.store(true, Ordering::Relaxed);
    }
}

/// Whether `src` mentions `.arguments` / `.caller`, or those names as computed string keys.
fn may_reflect(src: &str) -> bool {
    fn ident_end(rest: &str) -> bool {
        !rest
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$')
    }
    for name in ["arguments", "caller"] {
        let mut from = 0;
        while let Some(at) = lumen_common::search::find_from(src.as_bytes(), name.as_bytes(), from) {
            from = at + name.len();
            let head = src[..at].trim_end();
            let before = head.chars().next_back();
            let after = &src[at + name.len()..];
            // A string literal counts only as a computed key (`f["caller"]`), not as text
            // (`"arguments"` in an error message).
            let quoted = matches!(before, Some('\'' | '"' | '`'))
                && after.starts_with(['\'', '"', '`'])
                && head[..head.len() - 1].trim_end().ends_with('[');
            if (before == Some('.') && ident_end(after)) || quoted {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod source_scan_tests {
    use super::may_reflect;

    #[test]
    fn only_member_reads_and_computed_keys_count() {
        for src in ["f.caller", "f . arguments;", "f['arguments']", "f[ \"caller\" ]"] {
            assert!(may_reflect(src), "{src}");
        }
        for src in [
            "f.callers",
            "var arguments_ = 1",
            r#"o=1===i?"argument":"arguments""#,
            r#"(i="between "+t,"arguments")"#,
            "throw new Error('caller')",
        ] {
            assert!(!may_reflect(src), "{src}");
        }
    }
}

/// Whether `func`'s function objects carry the legacy own `arguments` / `caller` properties.
#[inline]
pub(crate) fn is_legacy(func: &Function) -> bool {
    !func.is_strict && !func.is_arrow && !func.is_method && !func.is_generator && !func.is_async
}

/// Record the innermost (just pushed) compiled frame's arguments: `head` then `tail` are the
/// actual arguments (split because the leading ones may have been moved into the slots), and
/// `slots` the frame's slot array.
#[cold]
#[inline(never)]
pub(super) fn stash_compiled(i: &mut Interp, head: &[Value], tail: &[Value], slots: *const Value) {
    let args: Rc<[Value]> = head.iter().chain(tail).cloned().collect();
    put(
        i,
        ReflectStash {
            args,
            scope: None,
            slots,
        },
    );
}

/// Record the innermost (just pushed) tree-walker frame's arguments and parameter scope.
#[cold]
#[inline(never)]
pub(crate) fn stash_tree(i: &mut Interp, args: &[Value], scope: &Env) {
    put(
        i,
        ReflectStash {
            args: Rc::from(args),
            scope: Some(scope.clone()),
            slots: std::ptr::null(),
        },
    );
}

fn put(i: &mut Interp, stash: ReflectStash) {
    if let Some(fr) = i.fn_frames.last_mut() {
        fr.extra.get_or_insert_with(Default::default).reflect = Some(stash);
    }
}

/// Clear the native-caller tag on entry to a native that V8 does not show as a caller (see the
/// module docs): its callee sees the JS frame that called it as the caller.
#[inline]
pub(crate) fn caller_transparent(i: &mut Interp) {
    if site_is_native(i.cur_site) {
        i.cur_site &= !SITE_NATIVE;
    }
}

/// [`caller_transparent`] for a call adaptor (`call`, `apply`, `Reflect.apply` / `construct`,
/// `eval`), which has no stack-trace frame of its own in V8 either.
#[inline]
pub(crate) fn native_transparent(i: &mut Interp) {
    caller_transparent(i);
    if i.native_top != 0 {
        let ctx = unsafe { &*(i.native_top as *const crate::interpreter::frames::NativeCtx) };
        ctx.hidden.set(true);
    }
}

/// The call stack, oldest first: `fn_frames` then the JIT's pending direct-call records.
fn with_frames<T>(i: &Interp, f: impl FnOnce(&[&FnFrame]) -> T) -> T {
    let pending = super::jit::pending_frames(i);
    let frames: Vec<&FnFrame> = i.fn_frames.iter().chain(pending.iter()).collect();
    f(&frames)
}

/// The legacy function `this` names, if it is one.
fn legacy_fn(this: &Value) -> Option<(Gc, Rc<Function>)> {
    let Value::Obj(o) = this else { return None };
    let func = match &o.borrow().call {
        Callable::User(u) if is_legacy(&u.func) => u.func.clone(),
        _ => return None,
    };
    Some((o.clone(), func))
}

/// `f.caller` (see the module docs).
pub(crate) fn read_caller(i: &mut Interp, this: &Value) -> Value {
    let Some((f, _)) = legacy_fn(this) else {
        return Value::Null;
    };
    let p = Gc::as_ptr(&f) as usize;
    with_frames(i, |frames| {
        let Some(mut k) = frames.iter().rposition(|fr| fr.fn_ptr == p) else {
            return Value::Null;
        };
        loop {
            // A frame called back from a native: the builtin is the caller.
            if frames[k].fn_ptr != 0 && site_is_native(frames[k].caller_site) {
                return Value::Null;
            }
            if k == 0 {
                return Value::Null;
            }
            k -= 1;
            let fr = frames[k];
            // Top-level and eval code are not callers: look through them.
            if fr.fn_ptr == 0 {
                continue;
            }
            return if fr.strict {
                Value::Null
            } else {
                Value::Obj(fr.callee())
            };
        }
    })
}

/// Which parameters of `func` V8 keeps in registers, so that `f.arguments` shows their current
/// value (see the module docs).
fn live_params(func: &Function) -> Vec<bool> {
    let n = func.params.len();
    let simple = func
        .params
        .iter()
        .all(|p| !p.rest && p.default.is_none() && matches!(p.pattern, Pattern::Ident(_)));
    let names: Vec<&str> = func
        .params
        .iter()
        .map(|p| match &p.pattern {
            Pattern::Ident(n) => n.as_str(),
            _ => "",
        })
        .collect();
    // An `arguments` object (named in the body, an inner arrow or a direct `eval`, which the
    // scan counts) context-allocates every parameter — unless a parameter shadows it.
    let uses_arguments =
        func.scan_flags() & crate::ast::SCAN_ARGUMENTS != 0 && !names.contains(&"arguments");
    if !simple || uses_arguments {
        return vec![false; n];
    }
    #[cfg(not(feature = "compiler"))]
    return vec![false; n];
    #[cfg(feature = "compiler")]
    let captured = super::CaptureScan::run(func)
        .map(|c| c.0)
        .unwrap_or_default();
    #[cfg(feature = "compiler")]
    (0..n)
        .map(|k| !captured.contains(names[k]) && !names[k + 1..].contains(&names[k]))
        .collect()
}

/// `f.arguments` (see the module docs).
pub(crate) fn read_arguments(i: &mut Interp, this: &Value) -> Value {
    let Some((f, func)) = legacy_fn(this) else {
        return Value::Null;
    };
    // From now on activations record what a read needs.
    if !enabled() {
        ENABLED.store(true, Ordering::Relaxed);
    }
    let p = Gc::as_ptr(&f) as usize;
    // Only `fn_frames` entries carry records (a JIT direct call that could need one takes the
    // slow path); a pending JIT record still makes the function active.
    let found = with_frames(i, |frames| {
        frames.iter().rposition(|fr| fr.fn_ptr == p).map(|k| {
            frames[k]
                .extra
                .as_deref()
                .and_then(|x| x.reflect.as_ref())
                .map(|s| (s.args.clone(), s.scope.clone(), s.slots))
        })
    });
    let Some(stash) = found else {
        return Value::Null;
    };
    let mut vals: Vec<Value> = Vec::new();
    let mut scope_for_obj = i.global_env.clone();
    if let Some((args, scope, slots)) = stash {
        vals = args.to_vec();
        let live = live_params(&func);
        for (k, v) in vals.iter_mut().enumerate().take(live.len()) {
            if !live[k] {
                continue;
            }
            if !slots.is_null() {
                // SAFETY: the frame is on the stack, so its slot array (at least one slot per
                // positional parameter) is alive and not reallocated.
                *v = unsafe { (*slots.add(k)).clone() };
            } else if let (Some(s), Pattern::Ident(name)) = (&scope, &func.params[k].pattern) {
                if let Some(b) = s.borrow().vars.get(name) {
                    *v = b.value.clone();
                }
            }
        }
        if let Some(s) = scope {
            scope_for_obj = s;
        }
    }
    let ao = i.make_arguments_object(&func, &vals, &scope_for_obj, &f);
    // A snapshot: never mapped onto the parameters.
    let ptr = Gc::as_ptr(&ao) as usize;
    if i.mapped_arguments.remove(&ptr).is_some() {
        i.gc_pins.remove(&ptr);
    }
    Value::Obj(ao)
}

/// Install the getter intrinsics (see the module docs). Their `this` is the function read.
pub(crate) fn install(i: &mut Interp) {
    let a = i.make_native("get arguments", 0, |i, this, _| {
        Ok(read_arguments(i, &this))
    });
    let c = i.make_native("get caller", 0, |i, this, _| Ok(read_caller(i, &this)));
    let properties=Object::new(None);
    {
        let mut properties=properties.borrow_mut();
        properties.props.insert("arguments",Property::accessor_prop(Some(Value::Obj(a.clone())),None,false,false));
        properties.props.insert("caller",Property::accessor_prop(Some(Value::Obj(c.clone())),None,false,false));
    }
    i.extra_protos.insert(ARGUMENTS_GETTER, a);
    i.extra_protos.insert(CALLER_GETTER, c);
    i.extra_protos.insert(OWN_PROPS,properties);
}

/// The own `arguments` / `caller` properties of a legacy function (see [`is_legacy`]), in
/// V8's order (they go between `name` and `prototype`). `None` before [`install`] (a function
/// made that early falls back to `Interp::get_member_recv`'s computed read).
pub(crate) fn own_props(i: &Interp) -> Option<[(&'static str, Property); 2]> {
    let properties=i.extra_protos.get(OWN_PROPS)?.borrow();
    Some([
        ("arguments", properties.props.get("arguments")?.clone()),
        ("caller", properties.props.get("caller")?.clone()),
    ])
}

/// The finished map of one legacy function's closures: the function's template with this realm's
/// `arguments` / `caller` accessors in place, held as a shared entry block so a closure owns no
/// entries until something writes to it. Kept per realm (not in the AST, which must not retain
/// realm values). The collector does not count a shared block's references (see
/// `Props::visit_object_refs`): they are the realm's own intrinsics, which it keeps anyway.
pub(crate) struct LegacyFnProps {
    func: std::rc::Weak<Function>,
    plain: crate::value::Props,
    named: Option<(Rc<str>, crate::value::Props)>,
}

/// [`Interp::legacy_fn_props`]' per-function cache. Entries for dead functions are dropped
/// whenever the map doubles.
#[derive(Default)]
pub(crate) struct LegacyFnCache {
    map: crate::fasthash::FastMap<usize, LegacyFnProps>,
    prune_at: usize,
}

impl LegacyFnCache {
    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.prune_at = 0;
    }
}

impl Interp {
    /// The shared map for a closure of the legacy function `func` whose template is `base` (the
    /// non-eager `fn_map`, holding the accessor placeholders), with its inferred `name` when one
    /// is given and the function has none. The flag says whether the name was applied. `None`
    /// when the realm has no legacy properties yet or `base` has no placeholders.
    pub(crate) fn legacy_fn_props(
        &self,
        func: &Rc<Function>,
        base: &crate::value::Props,
        name: Option<&str>,
    ) -> Option<(crate::value::Props, bool)> {
        let own = own_props(self)?;
        let mut cache = self.legacy_fn_cache.borrow_mut();
        let key = Rc::as_ptr(func) as usize;
        if !cache.map.contains_key(&key) {
            if !(base.contains("arguments") && base.contains("caller")) {
                return None;
            }
            if cache.map.len() >= cache.prune_at.max(1024) {
                cache.map.retain(|_, e| e.func.strong_count() > 0);
                cache.prune_at = cache.map.len() * 2;
            }
            let mut plain = base.clone();
            for (key, property) in &own {
                *plain.get_mut(key)? = property.clone();
            }
            plain.share_entries();
            cache.map.insert(
                key,
                LegacyFnProps {
                    func: Rc::downgrade(func),
                    plain,
                    named: None,
                },
            );
        }
        let entry = cache.map.get_mut(&key)?;
        let Some(name) = name else {
            return Some((entry.plain.clone(), false));
        };
        let (cached, named) = entry.named.get_or_insert_with(|| {
            let mut p = entry.plain.clone();
            p.insert(
                crate::value::fn_key(1),
                Property::data(Value::str(name), false, false, true),
            );
            p.share_entries();
            (Rc::from(name), p)
        });
        Some(if &**cached == name {
            (named.clone(), true)
        } else {
            (entry.plain.clone(), false)
        })
    }

    /// For NamedEvaluation of a closure that still shares its function's legacy map `current`:
    /// the shared map carrying `name`, so closures of one site keep sharing.
    pub(crate) fn legacy_named_props(
        &self,
        func: &Rc<Function>,
        current: &crate::value::Props,
        name: &str,
    ) -> Option<crate::value::Props> {
        let shares = self
            .legacy_fn_cache
            .borrow()
            .map
            .get(&(Rc::as_ptr(func) as usize))
            .is_some_and(|e| current.shares_entries_with(&e.plain));
        if !shares {
            return None;
        }
        let base = func.fn_maps.get()?.fn_map.clone();
        self.legacy_fn_props(func, &base, Some(name))
            .and_then(|(p, done)| done.then_some(p))
    }
}

pub(crate) fn own_props_ready(i:&Interp)->bool {
    i.extra_protos.contains_key(OWN_PROPS)
}

#[cfg(test)]
mod tests {
    use crate::bytecode::Tier;
    use crate::{Completion, Engine};

    fn run(src: &str, tier: Tier) -> String {
        let mut e = Engine::new();
        e.set_tier(tier);
        e.set_tier_threshold(0);
        match e.eval(src, false).unwrap() {
            Completion::Value(v) => v.to_string(),
            Completion::Throw { name, message } => format!("throw {name}: {message}"),
        }
    }

    #[test]
    fn legacy_arguments_and_caller_match_v8_on_every_tier() {
        // The first read anywhere turns recording on (see `enabled`); make it here so the
        // result does not depend on which test ran first.
        let src = r#"
            (function () {}).arguments;
            var out = [];
            function j(a) { return Array.prototype.join.call(a) + "/" + a.length; }
            function f(a, b) { var r = f.arguments; a = 10; r[1] = 99;
                return [r === f.arguments, j(f.arguments), b, f.arguments.callee === f].join(); }
            out.push(f(1, 2, 3), f.arguments);
            function g(a) { arguments[0] = 7; return j(g.arguments); }
            out.push(g(1));
            function c(a) { a = 9; function k() { return a; } return j(c.arguments); }
            out.push(c(1, 2));
            function d(a, a) { a = 9; return j(d.arguments); }
            out.push(d(1, 2));
            function r(n) { if (n > 0) return r(n - 1) + "," + r.arguments[0]; return r.arguments[0]; }
            out.push(r(3));
            function inner() { return inner.caller; }
            function outer() { return inner(); }
            function so() { "use strict"; return inner(); }
            out.push(outer() === outer, inner(), so(), [1].map(inner)[0],
                inner.call() === null, (function cc() { return inner.call() === cc; })(),
                (function ev() { return eval("inner()") === ev; })());
            out.push(["length", "name", "arguments", "caller", "prototype"].join() ===
                Object.getOwnPropertyNames(function () {}).join());
            var dc = Object.getOwnPropertyDescriptor(inner, "caller");
            out.push([typeof dc.get, dc.set, dc.enumerable, dc.configurable].join());
            f.arguments = 5; out.push(delete f.caller);
            try { (() => 1).caller; } catch (e) { out.push(e.name); }
            try { (function () { "use strict"; }).arguments; } catch (e) { out.push(e.name); }
            out.join("|")
        "#;
        let want = "false,10,2,3/3,2,true||1/1|1,2/2|1,9/2|0,1,2,3|true||||true|true|true|true|\
                    function,,false,false|\
                    false|TypeError|TypeError";
        for tier in [Tier::Interp, Tier::Bytecode] {
            let got = run(src, tier);
            if tier == Tier::Bytecode && std::env::var_os("LUMEN_JIT_EAGER").is_some() {
                // Whole-function native code records no `ReflectStash` and keeps assigned
                // parameters in registers (the JIT's side of this is still open), so the
                // `f`, `d` and `r` activations' element values are skipped under stress mode.
                let skip = |s: &str| {
                    s.split('|')
                        .enumerate()
                        .filter(|(k, _)| ![0, 4, 5].contains(k))
                        .map(|(_, p)| p.to_string())
                        .collect::<Vec<_>>()
                };
                assert_eq!(skip(&got), skip(want), "{tier:?} (eager JIT)");
            } else {
                assert_eq!(got, want, "{tier:?}");
            }
        }
    }

    fn global(e: &Engine, name: &str) -> crate::value::Gc {
        match e.interp.global.borrow().props.get(name).map(|p| p.value()) {
            Some(crate::value::Value::Obj(o)) => o,
            _ => panic!("{name} is not an object"),
        }
    }

    #[test]
    fn closures_of_one_legacy_function_share_one_entry_block() {
        let mut e = Engine::new();
        e.eval(
            "function mk() { return function () {}; }
             function named() { return { f: function () {} }; }
             var a = mk(), b = mk(), c = named().f, d = named().f;",
            false,
        )
        .unwrap();
        let (a, b, c, d) = (global(&e, "a"), global(&e, "b"), global(&e, "c"), global(&e, "d"));
        assert!(a.borrow().props.shares_entries_with(&b.borrow().props));
        assert!(c.borrow().props.shares_entries_with(&d.borrow().props));
        assert_eq!(a.borrow().props.shape(), c.borrow().props.shape());
        let name = |o: &crate::value::Gc| match o.borrow().props.get("name").map(|p| p.value()) {
            Some(crate::value::Value::Str(s)) => s.to_string(),
            _ => panic!("name"),
        };
        assert_eq!(name(&a), "");
        assert_eq!(name(&c), "f");
    }

    #[test]
    fn writing_to_a_shared_legacy_closure_leaves_its_siblings_alone() {
        let src = r#"
            function mk() { return function () {}; }
            var a = mk(), b = mk(), out = [];
            out.push(Object.getOwnPropertyNames(a).join());
            a.extra = 1;
            Object.defineProperty(b, "name", { value: "renamed" });
            var c = mk();
            out.push(a.extra, b.name, c.name === "", "extra" in c, Object.getOwnPropertyNames(c).join());
            var ga = Object.getOwnPropertyDescriptor(a, "arguments").get;
            var gc = Object.getOwnPropertyDescriptor(c, "arguments").get;
            out.push(ga === gc, delete c.name, c.name, c.hasOwnProperty("name"));
            var o = { m: function () {}, n: function () {} };
            out.push(o.m.name, o.n.name, Object.getOwnPropertyNames(o.m).join(),
                     Object.getOwnPropertyDescriptor(o.m, "arguments").configurable);
            o.m.arguments = 1;
            out.push(o.m.arguments, o.n.arguments, (function () {}).prototype !== undefined);
            out.join("|")
        "#;
        let want = "length,name,arguments,caller,prototype|1|renamed|true|false|\
                    length,name,arguments,caller,prototype|true|true||false|m|n|\
                    length,name,arguments,caller,prototype|false|||true";
        for tier in [Tier::Interp, Tier::Bytecode] {
            assert_eq!(run(src, tier), want, "{tier:?}");
        }
    }

    #[test]
    fn shared_legacy_maps_survive_collections_of_closure_cycles() {
        let mut e = Engine::new();
        e.eval(
            "function mk() { var f = function () { return f; }; f.self = f; return f; }
             var keep = mk(), n = 0;
             for (var i = 0; i < 2000; i++) { mk(); n++; }",
            false,
        )
        .unwrap();
        for _ in 0..3 {
            e.interp.gc_collect();
        }
        let out = e
            .eval(
                "var g = Object.getOwnPropertyDescriptor(keep, 'arguments').get;
                 for (var i = 0; i < 2000; i++) { mk(); }
                 [typeof g, g.call(keep), keep.arguments, keep.caller, n].join()",
                false,
            )
            .unwrap();
        for _ in 0..3 {
            e.interp.gc_collect();
        }
        assert!(matches!(&out, Completion::Value(v) if v == "function,,,,2000"));
        assert!(matches!(
            e.eval("(function () {}).arguments === null", false).unwrap(),
            Completion::Value(v) if v == "true"
        ));
    }
}
