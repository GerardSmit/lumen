//! Boxed operation semantics shared by bytecode and native execution.
use crate::interpreter::{Abrupt, Interp};
use crate::value::Value;
use std::cell::Cell;

// Native ABI ids: append new operations, never reorder the table.
pub const OP_NAMES: &[&str] = &[
    "Const",
    "Undef",
    "Dup",
    "Pop",
    "LoadLocal",
    "StoreLocal",
    "LoadCap",
    "StoreCap",
    "StoreCapInit",
    "UpdateCap",
    "UpdateName",
    "UpdateNameCached",
    "MakeClosure",
    "UpdateLocal",
    "Tdz",
    "LoadName",
    "StoreName",
    "StoreNameCached",
    "LoadThis",
    "LoadLexicalThis",
    "GetProp",
    "GetPropThis",
    "GetPropLocal",
    "SetProp",
    "SetPropDrop",
    "SetPropThisDrop",
    "SetPropLocalDrop",
    "ToStr",
    "GetIter",
    "ForInKeys",
    "ForInStepL",
    "IterStepL",
    "IterCloseL",
    "IterAbortL",
    "DestructureGuard",
    "DestructureArr",
    "DeleteProp",
    "DeleteElem",
    "CallSpread",
    "CallSpreadThis",
    "AppendProp",
    "GetElem",
    "SetElem",
    "SetElemDrop",
    "GetElemLocal",
    "ArgsLen",
    "ArgsGet",
    "ApplyArgs",
    "SetElemLocal",
    "SetElemLocalDrop",
    "UpdateProp",
    "UpdateElem",
    "ToPropKey",
    "ToPropKeyLocal",
    "Dup2",
    "GetMethod",
    "GetMethodElem",
    "Add",
    "Sub",
    "Mul",
    "Div",
    "Mod",
    "BitAnd",
    "BitOr",
    "BitXor",
    "Shl",
    "Shr",
    "UShr",
    "Lt",
    "Gt",
    "Le",
    "Ge",
    "EqEq",
    "NotEq",
    "StrictEq",
    "StrictNotEq",
    "InstanceOf",
    "GenBin",
    "Neg",
    "Plus",
    "Not",
    "BitNot",
    "Typeof",
    "TypeofIs",
    "TypeofName",
    "Void",
    "Jump",
    "JumpIfFalse",
    "JumpIfNotCmp",
    "JumpIfNotCmpLL",
    "JumpIfNotCmpLK",
    "ArithLL",
    "ArithLK",
    "JumpIfFalsePeek",
    "JumpIfTruePeek",
    "JumpIfNotNullishPeek",
    "Call",
    "LoadNameForCall",
    "CallWithThis",
    "New",
    "MakeRegExp",
    "MakeArray",
    "MakeObject",
    "Throw",
    "Return",
    "ReturnUndef",
    "Await",
    "PushHandler",
    "PopHandler",
    "GetPrivate",
    "SetPrivate",
    "GetPrivateMethod",
    "PrivateIn",
    "UpdatePrivate",
    "NewObject",
    "InitProp",
    "InitPropComputed",
    "InitMethod",
    "CopyDataProps",
    "SetProtoLit",
    "MakeClass",
    "Nip",
    "SuperGet",
    "SuperGetElem",
    "SuperBase",
    "SuperMethod",
    "SuperMethodElem",
    "ObjRest",
    "NewArrayLit",
    "ArrayAppend",
    "ArrayAppendSpread",
    "ArrayHole",
    "ArrayCbGuard",
    "ArrayCbHas",
    "ArrayCbDone",
    "SwitchLK",
    "SuperCtor",
    "SuperCall",
    "SuperCallSpread",
    "DerivedReturn",
    "InitialYield",
    "Yield",
    "YieldDelegate",
    "LoadCallee",
    "LoadNewTarget",
    "Concat",
    "DefineField",
    "BlkNew",
    "BlkDecl",
    "BlkCopy",
    "BlkLoad",
    "BlkStore",
    "BlkInit",
    "BlkUpdate",
    "InEnv",
    "TailCall",
    "TailCallSpread",
    "ImportCall",
    "GetAsyncIter",
    "AsyncIterNext",
    "AsyncIterResult",
    "AsyncCloseCall",
    "AsyncCloseCheck",
    "AsyncDelegateInit",
    "AsyncDelegateCall",
    "AsyncDelegateResult",
    "AsyncDelegateCloseReject",
    "AsyncDelegateSpecial",
    "NewSpread",
    "IterRestL",
];

pub const ENTER: u32 = 0xff0;
pub const SAFEPOINT: u32 = 0xff1;
pub const LAND: u32 = 0xff2;
pub const RESUME: u32 = 0xff3;
pub const TYPE_GUARD: u32 = 0xff4;
pub const NUM_BINARY: u32 = 0xff5;
pub const SHAPE_GUARD: u32 = 0xff6;
pub const SHAPE_GET_PROP: u32 = 0xff7;
pub const OP_BASE: u32 = 0x1000;

/// Stable profile type codes, matching the boxed Value tag ABI.
pub fn value_kind(value: &Value) -> u32 {
    match value {
        Value::Undefined => 0,
        Value::Empty => 1,
        Value::Null => 2,
        Value::Bool(_) => 3,
        Value::Num(_) => 4,
        Value::BigInt(_) => 5,
        Value::Str(_) => 6,
        Value::Sym(_) => 7,
        Value::Obj(_) => 8,
    }
}

/// Structural property shape, independent of process-local shape ids and symbol ids.
/// A zero result means the receiver cannot use this portable guard.
pub(crate) fn shape_hash(value: &Value) -> u64 {
    let Value::Obj(object) = value else { return 0 };
    let Ok(object) = object.try_borrow() else {
        return 0;
    };
    if !object.ic_plain.get() {
        return 0;
    }
    let mut hash = lumen_common::fasthash::FNV1A64_OFFSET;
    let mut feed = |bytes: &[u8]| hash = lumen_common::fasthash::fnv1a64(hash, bytes);
    for (name, property) in object.props.iter_named() {
        if Interp::is_sym_key(name) || Interp::is_private_key(name) {
            return 0;
        }
        feed(&(name.len() as u64).to_le_bytes());
        feed(name.as_bytes());
        feed(&[
            property.accessor() as u8,
            property.writable() as u8,
            property.enumerable() as u8,
            property.configurable() as u8,
        ]);
    }
    hash
}

/// Canonical identities in a fresh engine. Capture this before host initialization and use
/// the same enumeration before target restoration. Descriptor edges never call JS getters.
pub(crate) fn snapshot_intrinsics(i: &Interp) -> Vec<(String, Value)> {
    use std::collections::{BTreeSet, VecDeque};
    let mut queue = VecDeque::from([("globalThis".to_owned(), Value::Obj(i.global.clone()))]);
    let mut extra_protos: Vec<_> = i.extra_protos.iter().collect();
    extra_protos.sort_by(|a, b| a.0.cmp(b.0));
    for (name, object) in extra_protos {
        queue.push_back((format!("extra-proto:{name}"), Value::Obj(object.clone())));
    }
    #[cfg(feature = "aot-native")]
    {
        let mut modules: Vec<_> = i.native_module_names.iter().collect();
        modules.sort();
        for name in modules {
            if let Some(namespace) = i.modules.get(name) {
                queue.push_back((format!("native-module:{name}"), namespace.clone()));
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut seen_symbols = BTreeSet::new();
    let mut result = Vec::new();
    for (name, symbol, _) in &i.wk_syms {
        result.push((format!("symbol.wellknown:{name}"), symbol.clone()));
    }
    while let Some((path, value)) = queue.pop_front() {
        if let Value::Sym(symbol) = &value {
            if seen_symbols.insert(symbol.id) {
                result.push((path, value));
            }
            continue;
        }
        let Value::Obj(object) = &value else { continue };
        if !seen.insert(crate::value::Gc::as_ptr(object) as usize) {
            continue;
        }
        result.push((path.clone(), value.clone()));
        let object = object.borrow();
        if let Some(proto) = &object.proto {
            queue.push_back((format!("{path}/prototype"), Value::Obj(proto.clone())));
        }
        for name in object.props.keys() {
            let Some(property) = object.props.get(&name) else {
                continue;
            };
            let name = if Interp::is_sym_key(&name) {
                let Some(Value::Sym(symbol)) = i.sym_from_key(&name) else {
                    continue;
                };
                let Some((name, _, _)) = i
                    .wk_syms
                    .iter()
                    .find(|(_, value, _)| matches!(value, Value::Sym(s) if s.id == symbol.id))
                else {
                    continue;
                };
                format!("@symbol:{name}")
            } else {
                name.to_string()
            };
            let prefix = format!("{path}/{}:{name}", name.len());
            if !property.accessor() {
                queue.push_back((format!("{prefix}/value"), property.value()));
            }
            if let Some(getter) = property.getter() {
                queue.push_back((format!("{prefix}/get"), getter.clone()));
            }
            if let Some(setter) = property.setter() {
                queue.push_back((format!("{prefix}/set"), setter.clone()));
            }
        }
    }
    result
}

std::thread_local! {
    static CLOSED_WORLD_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Scope dynamic-code restrictions to a native invocation, including JS callbacks.
/// A suspension exits this scope; the native resume driver enters it again.
pub(crate) fn closed_world<R>(body: impl FnOnce() -> R) -> R {
    struct Restore(usize);
    impl Drop for Restore {
        fn drop(&mut self) {
            CLOSED_WORLD_DEPTH.with(|depth| depth.set(self.0));
        }
    }
    let restore = Restore(CLOSED_WORLD_DEPTH.with(|depth| {
        let previous = depth.get();
        depth.set(previous + 1);
        previous
    }));
    let result = body();
    drop(restore);
    result
}

pub(crate) fn dynamic_code_disabled() -> bool {
    !cfg!(feature = "compiler") || CLOSED_WORLD_DEPTH.with(|depth| depth.get() != 0)
}

#[inline]
pub(crate) fn unary(i: &mut Interp, operator: &str, value: Value) -> Result<Value, Abrupt> {
    match (operator, value) {
        ("-", Value::Num(n)) => Ok(Value::Num(-n)),
        ("+", Value::Num(n)) => Ok(Value::Num(n)),
        ("~", Value::Num(n)) => Ok(Value::Num(!crate::eval::to_int32(n) as f64)),
        ("!", value) => Ok(Value::Bool(!i.to_boolean(&value))),
        ("void", _) => Ok(Value::Undefined),
        (operator, value) => i.eval_unary_vm(operator, value),
    }
}

pub(crate) fn delete_property(
    i: &mut Interp,
    base: Value,
    key: &str,
    strict: bool,
) -> Result<Value, Abrupt> {
    if matches!(base, Value::Undefined | Value::Null) {
        return Err(i.throw(
            "TypeError",
            format!("cannot delete property '{key}' of null or undefined"),
        ));
    }
    i.delete_prop_with(base, key, strict)
}

pub(crate) fn delete_element(
    i: &mut Interp,
    base: Value,
    key: Value,
    strict: bool,
) -> Result<Value, Abrupt> {
    let key = i.to_property_key(&key)?;
    delete_property(i, base, &key, strict)
}

pub(crate) fn destructure_guard(i: &mut Interp, value: &Value) -> Result<(), Abrupt> {
    if matches!(value, Value::Undefined | Value::Null) {
        return Err(i.throw("TypeError", "cannot destructure null or undefined"));
    }
    Ok(())
}

#[cfg(all(test, feature = "compiler"))]
mod tests {
    use super::*;

    #[test]
    fn closed_world_scope_restores_nested_invocations() {
        assert!(!dynamic_code_disabled());
        closed_world(|| {
            assert!(dynamic_code_disabled());
            closed_world(|| assert!(dynamic_code_disabled()));
            assert!(dynamic_code_disabled());
        });
        assert!(!dynamic_code_disabled());
    }

    #[test]
    fn native_policy_follows_builtin_identity_and_keeps_shadowed_functions() {
        let mut engine = crate::Engine::new();
        engine.eval("function custom() { let eval = x => x + 1; return eval(2); } function indirect() { const run = eval; return run('1'); } function direct() { return eval('1'); } function create() { return Function('return 1')(); } function bound() { return eval.bind(null)('1'); }", false).unwrap();
        let function = |engine: &crate::Engine, name: &str| {
            engine
                .interp
                .global
                .borrow()
                .props
                .get(name)
                .unwrap()
                .value()
        };
        let custom = function(&engine, "custom");
        assert!(matches!(
            closed_world(|| engine.interp.call(custom, Value::Undefined, &[])),
            Ok(Value::Num(3.0))
        ));
        for name in ["indirect", "direct", "create", "bound"] {
            let callable = function(&engine, name);
            let Err(Abrupt::Throw(error)) =
                closed_world(|| engine.interp.call(callable, Value::Undefined, &[]))
            else {
                panic!("dynamic code must throw");
            };
            let error_name = engine
                .interp
                .get_member(&error, "name")
                .unwrap_or_else(|_| panic!("throw has no name"));
            assert!(matches!(error_name, Value::Str(name) if &*name == "EvalError"));
        }
        let ordinary = function(&engine, "create");
        assert!(matches!(
            engine.interp.call(ordinary, Value::Undefined, &[]),
            Ok(Value::Num(1.0))
        ));
    }
}
