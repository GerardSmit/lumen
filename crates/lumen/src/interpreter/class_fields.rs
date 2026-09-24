//! Class instance fields on the compiled tiers.
//!
//! A class's instance field initializers run as ONE synthetic strict method compiled to bytecode
//! (see [`crate::bytecode::class_fields`]): its body is one DefineField per field, in declaration
//! order, each receiving the initializer's value — the tree-walker's per-field scope, `this`
//! binding and AST evaluation are paid once per class instead of once per field per instance.
//! The method closes over a per-class scope below the field environment that carries the
//! `%fieldinit%` marker (so a direct `eval` reached from a nested arrow still sees
//! field-initializer code), and its `this` is the instance.
//!
//! Eligibility: a compiling tier, fields without decorator transforms, and a body the bytecode
//! compiler accepts. Anything else keeps `Interp::init_instance_fields`' tree-walking path.
use super::{Abrupt, Interp};
use crate::ast::{ArrayElem, Expr, FnSource, Function, Stmt};
use crate::value::{Exotic, Gc, Property, Value};
use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

/// Constructions a class runs on the tree-walker before its initializer is compiled (a class
/// expression evaluated per iteration and constructed once never pays for a compile).
const COMPILE_AFTER: u8 = 0;

/// The compiled-initializer state of one class (see [`super::ClassInfo`]).
#[derive(Default)]
pub(crate) enum FieldCode {
    /// Not attempted yet: the number of constructions so far.
    Cold(u8),
    /// The synthetic initializer method.
    Ready(Value),
    /// Not eligible (or the compiler refused): the tree-walking path, always.
    #[default]
    Off,
}

impl FieldCode {
    pub(crate) fn new() -> FieldCode {
        FieldCode::Cold(0)
    }
}

impl Interp {
    /// Initialize the fields (and private methods / decorator initializers) of `ctor`'s class on
    /// `this` through the compiled initializer. `None`: not handled here — the caller runs the
    /// tree-walking path.
    pub(crate) fn init_instance_fields_compiled(
        &mut self,
        ctor: &Value,
        this: &Value,
    ) -> Option<Result<(), Abrupt>> {
        let Value::Obj(c) = ctor else {
            return Some(Ok(()));
        };
        let ptr = Gc::as_ptr(c) as usize;
        let ci = self.class_info.get_mut(&ptr)?;
        if ci.fields.is_empty()
            && ci.private_members.is_empty()
            && ci.instance_initializers.is_empty()
        {
            return Some(Ok(()));
        }
        let init = match &mut ci.field_code {
            FieldCode::Ready(f) => f.clone(),
            FieldCode::Off => return None,
            FieldCode::Cold(n) => {
                #[allow(clippy::absurd_extreme_comparisons)] // tunable; 0 = compile at once
                if *n < COMPILE_AFTER {
                    *n += 1;
                    return None;
                }
                match self.compile_field_initializer(ptr) {
                    Some(f) => f,
                    None => {
                        if let Some(ci) = self.class_info.get_mut(&ptr) {
                            ci.field_code = FieldCode::Off;
                        }
                        return None;
                    }
                }
            }
        };
        let ci = self.class_info.get(&ptr)?;
        let has_privs = !ci.private_members.is_empty();
        let privs = if has_privs {
            ci.private_members.clone()
        } else {
            Vec::new()
        };
        let initializers = if ci.instance_initializers.is_empty() {
            Vec::new()
        } else {
            ci.instance_initializers.clone()
        };
        Some(self.run_compiled_fields(&init, this, &privs, &initializers))
    }

    fn run_compiled_fields(
        &mut self,
        init: &Value,
        this: &Value,
        privs: &[(String, Property)],
        initializers: &[Value],
    ) -> Result<(), Abrupt> {
        // PrivateMethodOrAccessorAdd (exactly as the tree-walking path).
        if let (Value::Obj(o), Some((k, _))) = (this, privs.first()) {
            if o.borrow().props.contains(k.as_str()) {
                return Err(self.throw(
                    "TypeError",
                    "cannot initialize private methods of a class twice on the same object",
                ));
            }
            if !o.borrow().extensible {
                return Err(self.throw(
                    "TypeError",
                    "cannot add private members to a non-extensible object",
                ));
            }
            for (k, p) in privs {
                o.borrow_mut().props.insert(k.as_str(), p.clone());
            }
        }
        let saved_super = std::mem::replace(&mut self.super_call_ok, false);
        let saved_field = std::mem::replace(&mut self.in_field_init_code, true);
        let r = (|me: &mut Self| -> Result<(), Abrupt> {
            if !matches!(init, Value::Undefined) {
                me.call(init.clone(), this.clone(), &[])?;
            }
            for f in initializers {
                me.call(f.clone(), this.clone(), &[])?;
            }
            Ok(())
        })(self);
        self.super_call_ok = saved_super;
        self.in_field_init_code = saved_field;
        r
    }

    /// Build and compile the synthetic initializer method of the class at `ptr`, recording the
    /// result. `None`: not eligible.
    fn compile_field_initializer(&mut self, ptr: usize) -> Option<Value> {
        if matches!(self.tier, crate::bytecode::Tier::Interp) {
            return None;
        }
        let ci = self.class_info.get(&ptr)?;
        if ci.fields.iter().any(|f| !f.transforms.is_empty()) {
            return None;
        }
        let field_env = ci.field_env.clone();
        let body: Vec<Stmt> = ci
            .fields
            .iter()
            .map(|f| {
                let named = f.init.as_ref().is_some_and(crate::eval::is_anonymous_fn);
                Stmt::Expr(Expr::Call {
                    callee: Box::new(Expr::Ident(
                        crate::bytecode::class_fields::DEFINE_FIELD.to_string(),
                    )),
                    args: vec![
                        ArrayElem::Item(Expr::Str(Rc::from(f.key.as_str()))),
                        ArrayElem::Item(f.init.clone().unwrap_or(Expr::Undefined)),
                        ArrayElem::Item(Expr::Bool(named)),
                    ],
                    optional: false,
                    pos: crate::ast::NO_POS,
                })
            })
            .collect();
        let init = if body.is_empty() {
            // Only private methods / decorator initializers: nothing to compile.
            Value::Undefined
        } else {
            let func = Rc::new(Function {
                name: None,
                params: Vec::new(),
                body: RefCell::new(Some(Rc::new(body))),
                lazy: RefCell::new(None),
                lazy_error: OnceCell::new(),
                is_arrow: false,
                is_strict: true,
                expr_body: false,
                is_generator: false,
                is_async: false,
                is_method: true,
                is_fn_expr: false,
                source: FnSource::None,
                scan: Cell::new(0),
                hoist: RefCell::new(None),
                body_used: Cell::new(false),
                calls: Cell::new(0),
                code: OnceCell::new(),
                fn_maps: OnceCell::new(),
            });
            let chunk = crate::bytecode::compile(&func)?;
            let _ = func.code.set(Some(chunk));
            let scope = super::new_scope(Some(field_env));
            crate::eval::bind(&scope, "%fieldinit%", Value::Bool(true));
            self.make_function(func, scope)
        };
        if let Some(ci) = self.class_info.get_mut(&ptr) {
            ci.field_code = FieldCode::Ready(init.clone());
        }
        Some(init)
    }

    /// DefineField for one instance field on `this`: PrivateFieldAdd for a private name,
    /// CreateDataPropertyOrThrow otherwise.
    pub(crate) fn define_instance_field(
        &mut self,
        this: &Value,
        key: &Rc<str>,
        v: Value,
    ) -> Result<(), Abrupt> {
        let Value::Obj(o) = this else {
            if Interp::is_private_key(key) {
                return Err(self.throw("TypeError", "cannot add a private field"));
            }
            return crate::builtins::cdp_or_throw(self, this, key, v).map_err(Abrupt::Throw);
        };
        if Interp::is_private_key(key) {
            // PrivateFieldAdd: stamped directly on the object (bypassing proxy traps); a second
            // add or a non-extensible receiver is a TypeError.
            if o.borrow().props.contains(key) {
                return Err(self.throw(
                    "TypeError",
                    "cannot initialize the same private field twice",
                ));
            }
            if !o.borrow().extensible {
                return Err(self.throw(
                    "TypeError",
                    "cannot add a private field to a non-extensible object",
                ));
            }
            o.borrow_mut()
                .props
                .insert(key.clone(), Property::data(v, true, false, false));
            return Ok(());
        }
        // An absent, non-numeric key on an extensible ordinary object: the ordinary
        // [[DefineOwnProperty]] of a fresh data property, without the descriptor round trip.
        let ordinary_key = !matches!(
            key.as_bytes().first(),
            None | Some(b'0'..=b'9' | b'-' | b'I' | b'N')
        );
        if ordinary_key {
            let optr = Gc::as_ptr(o) as usize;
            let fast = {
                let b = o.borrow();
                b.exotic == Exotic::None && b.extensible && !b.props.contains(key)
            } && (self.proxies.is_empty() || !self.proxies.contains_key(&optr))
                && (self.typed_arrays.is_empty() || !self.typed_arrays.contains_key(&optr))
                && (self.deferred_ns.is_empty() || !self.deferred_ns.contains_key(&optr))
                && !Gc::ptr_eq(o, &self.global);
            if fast {
                o.borrow_mut().props.insert(key.clone(), Property::plain(v));
                return Ok(());
            }
        }
        crate::builtins::cdp_or_throw(self, this, key, v).map_err(Abrupt::Throw)
    }
}
