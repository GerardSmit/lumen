//! Compiler-host CommonJS initialization using the native closed-world contract.
use crate::interpreter::{new_var_scope, Env, Interp, Abrupt};
use crate::value::{Gc, Property, Value};
use std::{cell::RefCell, rc::Rc};

pub(crate) struct Unit {
    pub path: String,
    pub env: Env,
    pub module: Value,
    pub require: Value,
    function: Rc<crate::ast::Function>,
    links: Vec<(String, u32)>,
    pub state: u8,
}

pub(crate) type Units = Rc<RefCell<Vec<Option<Unit>>>>;

fn run(i: &mut Interp, units: &Units, index: u32) -> Result<Value, Abrupt> {
    let (env, module, function, state) = {
        let units = units.borrow();
        let Some(Some(unit)) = units.get(index as usize) else { return Err(i.throw("EvalError", "require target is not a CommonJS snapshot unit")); };
        (unit.env.clone(), unit.module.clone(), unit.function.clone(), unit.state)
    };
    if state == 0 {
        units.borrow_mut()[index as usize].as_mut().unwrap().state = 1;
        let exports = i.get_member(&module, "exports")?;
        let arguments = ["exports", "require", "module", "__filename", "__dirname"].map(|name| i.get_var(name, &env)).into_iter().collect::<Result<Vec<_>, _>>()?;
        let function = i.make_function(function, env);
        let result = i.call(function, exports, &arguments);
        units.borrow_mut()[index as usize].as_mut().unwrap().state = if result.is_ok() { 2 } else { 3 };
        result?;
    } else if state == 3 { return Err(i.throw("EvalError", "CommonJS snapshot unit initialization failed")); }
    i.get_member(&module, "exports")
}

impl crate::Engine {
    /// Route host ESM facades to this initialized closed-world CommonJS graph.
    pub fn install_snapshot_cjs_require_router(&mut self) -> Result<(), String> {
        if self.interp.snapshot_cjs_require.is_some() { return Err("CommonJS snapshot require router already installed".into()); }
        let units = self.interp.snapshot_cjs.clone().ok_or("CommonJS snapshot units are not initialized")?;
        let original = self.interp.global.borrow().props.get("require").map(|property| property.value().clone()).unwrap_or(Value::Undefined);
        let require = self.interp.new_native_fn("require", 1, Rc::new(move |i, _, args| {
            let name = i.to_string(args.first().unwrap_or(&Value::Undefined)).map_err(crate::interpreter::abrupt_value)?;
            let name = name.strip_prefix("aot:/").unwrap_or(name.as_str());
            let matches: Vec<_> = units.borrow().iter().enumerate().filter_map(|(index, unit)| {
                unit.as_ref().filter(|unit| name == unit.path || name.strip_suffix(&unit.path).is_some_and(|prefix| prefix.ends_with('/'))).map(|_| index as u32)
            }).collect();
            match matches.as_slice() {
                [index] => run(i, &units, *index).map_err(crate::interpreter::abrupt_value),
                _ => Err(crate::interpreter::abrupt_value(i.throw("EvalError", "CommonJS facade target is not uniquely present in the native image"))),
            }
        }));
        self.interp.snapshot_cjs_require = Some(original);
        self.interp.global.borrow_mut().props.insert("require", Property::plain(require));
        Ok(())
    }

    pub fn finish_cjs_snapshot_initialization(&mut self) {
        if let Some(original) = self.interp.snapshot_cjs_require.take() {
            self.interp.global.borrow_mut().props.insert("require", Property::plain(original));
        }
    }

    /// Initialize bundled CommonJS without the source-backed Node Module loader.
    pub fn initialize_cjs_snapshot(&mut self, input: &[(String, crate::precompiled::CompiledUnit, Vec<(String, u32)>)], entry: u32) -> Result<Value, String> {
        self.prepare_cjs_snapshot(input)?;
        let units = self.interp.snapshot_cjs.clone().unwrap();
        run(&mut self.interp, &units, entry).map_err(|error| format!("CommonJS snapshot initialization: {}", self.interp.to_string(&crate::interpreter::abrupt_value(error)).map_or_else(|_| "failed".into(), |value| value.to_string())))
    }

    /// Register lazy wrappers before the host ESM loader establishes evaluation order.
    pub fn prepare_cjs_snapshot(&mut self, input: &[(String, crate::precompiled::CompiledUnit, Vec<(String, u32)>)]) -> Result<(), String> {
        if self.interp.snapshot_cjs.is_some() { return Err("CommonJS snapshot initialization already installed".into()); }
        let units: Units = Rc::new(RefCell::new(Vec::with_capacity(input.len())));
        for (index, (path, compiled, links)) in input.iter().enumerate() {
            if compiled.kind() != crate::SourceKind::CommonJs { units.borrow_mut().push(None); continue; }
            let mut function = compiled.snapshot_cjs_function()?;
            Rc::get_mut(&mut function).unwrap().params = ["exports", "require", "module", "__filename", "__dirname"].into_iter().map(|name| crate::ast::Param {
                pattern: crate::ast::Pattern::Ident(name.into()), default: None, rest: false,
            }).collect();
            let env = new_var_scope(Some(self.interp.global_env.clone()));
            let module = Value::Obj(self.interp.new_object());
            let exports = Value::Obj(self.interp.new_object());
            module.as_obj().unwrap().borrow_mut().props.insert("exports", Property::plain(exports.clone()));
            crate::eval::bind(&env, "%cjsmodule%", module.clone());
            let captured = Rc::downgrade(&units);
            let require = self.interp.new_native_fn("require", 1, Rc::new(move |i, _, args| {
                let captured = captured.upgrade().ok_or_else(|| crate::interpreter::abrupt_value(i.throw("EvalError", "CommonJS snapshot initializer has been released")))?;
                let name = i.to_string(args.first().unwrap_or(&Value::Undefined)).map_err(crate::interpreter::abrupt_value)?;
                let target = captured.borrow()[index].as_ref().unwrap().links.iter().find(|(key, _)| key == name.as_str()).map(|(_, target)| *target);
                match target {
                    Some(target) => run(i, &captured, target).map_err(crate::interpreter::abrupt_value),
                    #[cfg(feature = "aot-native")]
                    None if i.native_module_names.contains(name.as_str()) => {
                        let namespace = i.modules[name.as_str()].clone();
                        let default = i.get_member(&namespace, "default").map_err(crate::interpreter::abrupt_value)?;
                        Ok(if matches!(default, Value::Undefined) { namespace } else { default })
                    }
                    None => Err(crate::interpreter::abrupt_value(i.throw("EvalError", "require target is not in the native image"))),
                }
            }));
            for (name, value) in [("module", module.clone()), ("exports", exports), ("require", require.clone()),
                ("__filename", Value::from_string(path.clone())), ("__dirname", Value::from_string(path.rsplit_once('/').map_or("", |(dir, _)| dir).into()))] {
                crate::eval::bind(&env, name, value);
            }
            units.borrow_mut().push(Some(Unit { path: path.clone(), env, module, require, function, links: links.clone(), state: 0 }));
        }
        self.interp.snapshot_cjs = Some(units.clone());
        Ok(())
    }

    /// Evaluated module records for seeding a host facade loader's cache.
    pub fn snapshot_cjs_modules(&self) -> Vec<(String, Value)> {
        self.interp.snapshot_cjs.as_ref().map_or_else(Vec::new, |units| units.borrow().iter().flatten().filter(|unit| unit.state == 2).map(|unit| (unit.path.clone(), unit.module.clone())).collect())
    }
}

pub(crate) fn require_ids(units: &Option<Units>) -> std::collections::BTreeMap<usize, u32> {
    units.as_ref().map_or_else(Default::default, |units| units.borrow().iter().enumerate().filter_map(|(index, unit)| unit.as_ref().and_then(|unit| unit.require.as_obj()).map(|object| (Gc::as_ptr(object) as usize, index as u32))).collect())
}
