use super::{Parcel, parcel::Intrinsic};
use crate::{interpreter::Interp, value::{gc_state_handle, Value}};
use lumen_common::buffer::ByteStore;

impl Interp {
    pub(crate) fn adopt(&mut self, mut parcel: Parcel) -> Value {
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
            });
            if parcel.side.buffers.contains_key(&(crate::value::Gc::as_ptr(&object) as usize))
                || parcel.side.maps.contains_key(&(crate::value::Gc::as_ptr(&object) as usize))
                || parcel.side.typed.contains_key(&(crate::value::Gc::as_ptr(&object) as usize))
                || parcel.side.views.contains_key(&(crate::value::Gc::as_ptr(&object) as usize))
                || parcel.side.regexps.contains_key(&(crate::value::Gc::as_ptr(&object) as usize)) {
                self.gc_pin(&object);
            }
        }
        self.array_buffers.extend(parcel.side.buffers.drain().map(|(pointer, bytes)| (pointer, ByteStore::new(bytes).into())));
        self.map_data.extend(parcel.side.maps.drain());
        self.typed_arrays.extend(parcel.side.typed.drain());
        self.ta_buffer.extend(parcel.side.ta_buffer.drain());
        self.data_views.extend(parcel.side.views.drain());
        self.regexps.extend(parcel.side.regexps.drain());
        for (pointer, handle) in parcel.side.shared.drain() { handle.adopt_into(self, pointer); }
        for (object, function) in parcel.functions.drain(..) {
            object.borrow_mut().call = crate::value::Callable::user(function, self.global_env.clone());
        }
        parcel.adopted = true;
        std::mem::replace(&mut parcel.root, Value::Undefined)
    }
}
