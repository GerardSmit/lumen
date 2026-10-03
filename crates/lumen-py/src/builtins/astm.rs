//! `_ast`: the AST node classes, built from the generated [`super::astnodes`] table as CPython's
//! `init_types` builds them (`type(name, (base,), ...)` under the native `ast.AST`).

use std::collections::HashMap;
use crate::object::Obj;

/// The node classes by name, for converting a parsed tree into `_ast` objects.
#[derive(Default)]
pub struct AstTypes {
    pub by_name: HashMap<&'static str, Obj>,
}

#[lumen_bind::module(name = "_ast")]
pub mod _ast {
    #![allow(clippy::new_ret_no_self)]
    use super::AstTypes;
    use crate::bind::{opaque_instance, KwArgs, This};
    use crate::builtins::astnodes::NODE_TYPES;
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};

    const PY_CF_ONLY_AST: i64 = 0x400;
    const PY_CF_TYPE_COMMENTS: i64 = 0x1000;
    const PY_CF_ALLOW_TOP_LEVEL_AWAIT: i64 = 0x2000;

    #[class(name = "AST", module = "ast", hint(py(mutable)))]
    pub struct AST;

    #[methods]
    impl AST {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
            let _ = (args, kwargs);
            let Value::Obj(cls) = cls.0 else { return Err(it.type_error("AST.__new__(X): X is not a type object")) };
            Ok(opaque_instance(&cls, AST))
        }

        // `ast_type_init`: positional arguments fill `_fields` in order, keywords set any
        // attribute.
        #[proto(init)]
        fn __init__(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<()> {
            let cls = it.type_of(&slf.0);
            let fields = it.get_attr_str(&Value::Obj(cls.clone()), "_fields")?;
            let fields = it.iterate_to_vec(&fields)?;
            if fields.len() < args.len() {
                let name = it.type_name(&cls);
                let n = fields.len();
                let s = if n == 1 { "" } else { "s" };
                return Err(it.type_error(&format!("{name} constructor takes at most {n} positional argument{s}")));
            }
            for (name, v) in fields.iter().zip(args) {
                let Value::Obj(name) = name else { continue };
                it.set_attr(&slf.0, name, v.clone())?;
            }
            for (key, v) in kwargs.to_vec() {
                let k = Value::Obj(key.clone());
                let mut at = None;
                for (i, f) in fields.iter().enumerate() {
                    if it.values_eq(f, &k)? {
                        at = Some(i);
                        break;
                    }
                }
                if at.is_some_and(|p| p < args.len()) {
                    let t = it.tp_name(&cls);
                    let k = key.as_str_kind().unwrap_or("");
                    return Err(it.type_error(&format!("{t} got multiple values for argument '{k}'")));
                }
                it.set_attr(&slf.0, &key, v)?;
            }
            Ok(())
        }

        #[method(name = "__reduce__")]
        fn reduce(slf: This<Value>, it: &mut Interp) -> R<Value> {
            let cls = it.type_of(&slf.0);
            let Value::Obj(o) = &slf.0 else { return Ok(Value::tuple(vec![Value::Obj(cls), Value::tuple(Vec::new())])) };
            let d = it.instance_dict(o);
            Ok(Value::tuple(vec![Value::Obj(cls), Value::tuple(Vec::new()), Value::Obj(d)]))
        }
    }

    fn str_tuple<'a>(names: impl Iterator<Item = &'a str>) -> Value {
        Value::tuple(names.map(Value::str).collect())
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let _ = build(it, m);
    }

    fn build(it: &mut Interp, m: &Value) -> R<()> {
        let Value::Obj(m) = m else { return Ok(()) };
        let d = it.module_dict(m);
        let ast = crate::bind::type_object::<AST>(it);
        let ast_v = Value::Obj(ast.clone());
        let empty = Value::tuple(Vec::new());
        it.set_attr_str(&ast_v, "_fields", empty.clone())?;
        it.set_attr_str(&ast_v, "__match_args__", empty.clone())?;
        it.set_attr_str(&ast_v, "_attributes", empty)?;
        dict_set_str(&d, "AST", ast_v.clone());
        for (name, v) in [("PyCF_ALLOW_TOP_LEVEL_AWAIT", PY_CF_ALLOW_TOP_LEVEL_AWAIT), ("PyCF_ONLY_AST", PY_CF_ONLY_AST), ("PyCF_TYPE_COMMENTS", PY_CF_TYPE_COMMENTS)] {
            dict_set_str(&d, name, Value::Int(v));
        }
        let type_ty = Value::Obj(it.types.type_.clone());
        let mut made = AstTypes::default();
        made.by_name.insert("AST", ast);
        for n in NODE_TYPES {
            let base = made.by_name.get(n.base).cloned().ok_or_else(|| it.type_error("ast base"))?;
            let fields = str_tuple(n.fields.iter().map(|f| f.0));
            let ns = it.new_dict();
            dict_set_str(&ns, "_fields", fields.clone());
            dict_set_str(&ns, "__match_args__", fields);
            dict_set_str(&ns, "__module__", Value::str("ast"));
            dict_set_str(&ns, "__doc__", Value::str(n.doc));
            let args = vec![Value::str(n.name), Value::tuple(vec![Value::Obj(base)]), Value::Obj(ns)];
            let ty = it.call(&type_ty, args, Vec::new())?;
            if let Some(attrs) = n.attributes {
                it.set_attr_str(&ty, "_attributes", str_tuple(attrs.iter().copied()))?;
            }
            for f in n.none_defaults {
                it.set_attr_str(&ty, f, Value::None)?;
            }
            dict_set_str(&d, n.name, ty.clone());
            if let Value::Obj(t) = ty {
                made.by_name.insert(n.name, t);
            }
        }
        *it.native_state::<AstTypes>() = made;
        Ok(())
    }
}
