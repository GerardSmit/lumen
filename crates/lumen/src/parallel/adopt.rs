use super::{parcel::Intrinsic, Parcel};
use crate::{
    interpreter::Interp,
    value::{gc_state_handle, Value},
};
use lumen_common::buffer::ByteStore;

impl Interp {
    /// Consume the parcel's heap and side tables on this realm's owner thread.
    pub fn adopt(&mut self, parcel: Parcel) -> Value {
        self.try_adopt(parcel)
            .unwrap_or_else(|_| panic!("parcel adoption failed; use try_adopt for native parcels"))
    }

    pub fn try_adopt(&mut self, mut parcel: Parcel) -> Result<Value, Value> {
        #[cfg(feature = "aot-native")]
        let mut programs: std::collections::HashMap<
            [u8; 32],
            std::rc::Rc<crate::native_aot::NativeProgram>,
        > = std::collections::HashMap::new();
        #[cfg(feature = "aot-native")]
        for (hash, bytes, trusted_glue) in parcel
            .native_functions
            .iter()
            .map(|function| (function.hash, function.bytes.clone(), function.trusted_glue))
            .chain(
                parcel
                    .native_classes
                    .iter()
                    .map(|class| (class.hash, class.bytes.clone(), class.trusted_glue)),
            )
        {
            if let Some(program) = programs.get(&hash) {
                if program.trusted_glue != trusted_glue {
                    return Err(self
                        .make_error("DataCloneError", "native parcel trust provenance mismatch"));
                }
                continue;
            }
            let program = crate::native_aot::load_authenticated_program(self, bytes, trusted_glue)
                .map_err(|message| self.make_error("DataCloneError", &message))?;
            if program.image.blob_hash() != hash {
                return Err(
                    self.make_error("DataCloneError", "native parcel image identity mismatch")
                );
            }
            programs.insert(hash, program);
        }
        gc_state_handle().absorb(&parcel.heap);
        for (object, intrinsic) in parcel.protos.drain(..) {
            object.borrow_mut().proto = Some(match intrinsic {
                Intrinsic::Object => self.object_proto.clone(),
                Intrinsic::Array => self.array_proto.clone(),
                Intrinsic::String => self.string_proto.clone(),
                Intrinsic::Number => self.number_proto.clone(),
                Intrinsic::Boolean => self.boolean_proto.clone(),
                Intrinsic::Function => self.function_proto.clone(),
                Intrinsic::Error(name) => self.error_protos[name].clone(),
                Intrinsic::Extra(name) => self.extra_protos[name].clone(),
                Intrinsic::Global(name) => self
                    .global
                    .borrow()
                    .props
                    .get(&name)
                    .unwrap()
                    .value()
                    .as_obj()
                    .unwrap()
                    .clone(),
            });
            if parcel
                .side
                .buffers
                .contains_key(&(crate::value::Gc::as_ptr(&object) as usize))
                || parcel
                    .side
                    .maps
                    .contains_key(&(crate::value::Gc::as_ptr(&object) as usize))
                || parcel
                    .side
                    .typed
                    .contains_key(&(crate::value::Gc::as_ptr(&object) as usize))
                || parcel
                    .side
                    .views
                    .contains_key(&(crate::value::Gc::as_ptr(&object) as usize))
                || parcel
                    .side
                    .regexps
                    .contains_key(&(crate::value::Gc::as_ptr(&object) as usize))
                || parcel
                    .side
                    .shared
                    .contains_key(&(crate::value::Gc::as_ptr(&object) as usize))
            {
                self.gc_pin(&object);
            }
        }
        self.array_buffers
            .extend(parcel.side.buffers.drain().map(|(pointer, buffer)| {
                let mut store = ByteStore::new(buffer.bytes);
                if let Some(max) = buffer.max_len {
                    store = store.with_max_len(max);
                }
                if buffer.readonly {
                    store.set_readonly();
                }
                (pointer, store.into())
            }));
        self.map_data.extend(parcel.side.maps.drain());
        self.typed_arrays.extend(parcel.side.typed.drain());
        self.ta_buffer.extend(parcel.side.ta_buffer.drain());
        self.data_views.extend(parcel.side.views.drain());
        self.regexps.extend(parcel.side.regexps.drain());
        for (pointer, handle) in parcel.side.shared.drain() {
            handle.adopt_into(self, pointer);
        }
        for scope in parcel.scopes.drain(..) {
            scope.borrow_mut().parent = Some(self.global_env.clone());
        }
        for (scope, name, intrinsic) in parcel.scope_intrinsics.drain(..) {
            let value = match intrinsic {
                Intrinsic::Object => Value::Obj(self.object_proto.clone()),
                Intrinsic::Array => Value::Obj(self.array_proto.clone()),
                Intrinsic::Function => Value::Obj(self.function_proto.clone()),
                Intrinsic::Error(name) => Value::Obj(self.error_protos[name].clone()),
                Intrinsic::Extra(name) => Value::Obj(self.extra_protos[name].clone()),
                Intrinsic::Global(name) => self.global.borrow().props.get(&name).unwrap().value(),
                _ => unreachable!(),
            };
            scope.borrow_mut().vars.get_mut(&name).unwrap().value = value;
        }
        for (object, parent) in parcel.class_protos.drain(..) {
            object.borrow_mut().proto = parent.as_obj().cloned();
        }
        for (object, old, name) in parcel.symbol_keys.drain(..) {
            let key = self
                .wk_syms
                .iter()
                .find(|(candidate, _, _)| *candidate == name)
                .unwrap()
                .2
                .to_string();
            let property = object.borrow().props.get(&old).unwrap().clone();
            object.borrow_mut().props.remove(&old);
            object.borrow_mut().props.insert(key, property);
        }
        for (pointer, index, name) in parcel.field_symbols.drain(..) {
            parcel.side.classes.get_mut(&pointer).unwrap().fields[index].key = self
                .wk_syms
                .iter()
                .find(|(candidate, _, _)| *candidate == name)
                .unwrap()
                .2
                .to_string();
        }
        for (pointer, info) in parcel.side.classes.drain() {
            self.class_info.insert(pointer, info);
        }
        for (object, function, environment) in parcel.functions.drain(..) {
            if self
                .class_info
                .contains_key(&(crate::value::Gc::as_ptr(&object) as usize))
            {
                self.gc_pin(&object);
            }
            object.borrow_mut().call = crate::value::Callable::user(
                function,
                environment.unwrap_or_else(|| self.global_env.clone()),
            );
        }
        #[cfg(feature = "aot-native")]
        for function in parcel.native_functions.drain(..) {
            function.object.borrow_mut().call =
                crate::value::Callable::Aot(Box::new(crate::value::AotCallable {
                    program: programs[&function.hash].clone(),
                    function_index: function.index,
                    env: function.env.unwrap_or_else(|| self.global_env.clone()),
                }));
        }
        #[cfg(feature = "aot-native")]
        for class in parcel.native_classes.drain(..) {
            if class.body.is_none() {
                class.object.borrow_mut().call =
                    crate::value::Callable::Native(crate::native_aot::classes::default_constructor);
            }
            class.object.borrow_mut().is_constructor = true;
            self.gc_pin(&class.object);
            self.native_classes.insert(
                crate::value::Gc::as_ptr(&class.object) as usize,
                std::rc::Rc::new(crate::native_aot::classes::NativeClass {
                    program: programs[&class.hash].clone(),
                    env: class.env,
                    derived: class.derived,
                    body: class.body,
                    fields: class.fields,
                    private_members: class.private_members,
                    initializers: class.initializers,
                }),
            );
        }
        parcel.adopted = true;
        Ok(std::mem::replace(&mut parcel.root, Value::Undefined))
    }
}
