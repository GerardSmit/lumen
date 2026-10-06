//! Brand and slot access for structured clone.
//!
//! A host serializer must classify objects by their internal slots, never by properties the
//! script can replace (`Symbol.toStringTag`, `constructor`, prototype methods). These methods
//! read the engine's side tables and build the matching intrinsic objects directly, so
//! `lumen_host::structured_clone` runs no author code except the getters the specification
//! itself runs (`[[Get]]` of an own property, `Error.prototype.stack`).

use crate::builtins::collection_data::{CollectionData, CollectionKind};
use crate::interpreter::{abrupt_value, Interp};
use crate::value::{Exotic, Gc, Object, TaInfo, TaKind, Value};

/// What an object is, for `StructuredSerializeInternal`.
#[derive(Clone)]
pub enum CloneBrand {
    /// A Date and its [[DateValue]].
    Date(f64),
    /// A RegExp and its original source and flags.
    RegExp { source: String, flags: String },
    /// A Boolean object and its [[BooleanData]].
    Boolean(bool),
    /// A Number object and its [[NumberData]].
    Number(f64),
    /// A String object and its [[StringData]] (a string value).
    String(Value),
    /// A BigInt object and its [[BigIntData]] (a BigInt value).
    BigInt(Value),
    /// An object with [[ErrorData]].
    Error,
    /// An Array (including a lazy split view).
    Array,
    /// A Map.
    Map,
    /// A Set.
    Set,
    /// An ArrayBuffer; `detached` buffers cannot be cloned.
    ArrayBuffer { detached: bool },
    /// A SharedArrayBuffer.
    SharedArrayBuffer,
    /// A TypedArray: its element kind, [[ViewedArrayBuffer]], [[ByteOffset]] and current length
    /// (`None` when the view is out of bounds or its buffer is detached).
    TypedArray {
        kind: TaKind,
        buffer: Value,
        byte_offset: usize,
        length: Option<usize>,
    },
    /// A DataView: [[ViewedArrayBuffer]], [[ByteOffset]] and current byte length (`None` when
    /// out of bounds or detached).
    DataView {
        buffer: Value,
        byte_offset: usize,
        byte_length: Option<usize>,
    },
    /// An object that can never be cloned: the interface or kind name for the error message.
    Uncloneable(&'static str),
    /// Anything else: a plain object as far as the engine is concerned.
    Ordinary,
}

impl Interp {
    /// Classify an object by its internal slots (a primitive is `Ordinary`).
    pub fn clone_brand(&self, value: &Value) -> CloneBrand {
        let Value::Obj(object) = value else {
            return CloneBrand::Ordinary;
        };
        let pointer = Gc::as_ptr(object) as usize;
        if self.proxies.contains_key(&pointer) {
            return CloneBrand::Uncloneable("Proxy");
        }
        if value.is_callable() {
            return CloneBrand::Uncloneable("Function");
        }
        {
            let borrowed = object.borrow();
            match borrowed.exotic {
                Exotic::Date => {
                    if let Some(time) = borrowed.date_value() {
                        return CloneBrand::Date(time);
                    }
                }
                Exotic::BoolWrap => {
                    if let Some(flag) = borrowed.bool_wrap() {
                        return CloneBrand::Boolean(flag);
                    }
                }
                Exotic::NumWrap => {
                    if let Some(number) = borrowed.num_wrap() {
                        return CloneBrand::Number(number);
                    }
                }
                Exotic::StrWrap => {
                    if let Some(text) = borrowed.str_wrap() {
                        return CloneBrand::String(Value::Str(text));
                    }
                }
                Exotic::BigIntWrap => {
                    if let Some(number) = borrowed.bigint_wrap() {
                        return CloneBrand::BigInt(Value::BigInt(number));
                    }
                }
                Exotic::SymWrap => return CloneBrand::Uncloneable("Symbol"),
                Exotic::Error => return CloneBrand::Error,
                Exotic::Array | Exotic::SplitView => return CloneBrand::Array,
                Exotic::None | Exotic::Arguments => {}
            }
        }
        if let Some(info) = self.typed_arrays.get(&pointer).copied() {
            let buffer = self
                .ta_buffer
                .get(&pointer)
                .cloned()
                .unwrap_or(Value::Undefined);
            return CloneBrand::TypedArray {
                kind: info.kind,
                buffer,
                byte_offset: info.offset,
                length: self.ta_len(&info),
            };
        }
        if let Some((buffer, offset, length, track)) = self.data_views.get(&pointer).copied() {
            let viewed = object
                .borrow()
                .props
                .get("\u{0}dv_buffer")
                .map(|property| property.value())
                .unwrap_or(Value::Undefined);
            let byte_length = self.array_buffers.get(&buffer).and_then(|store| {
                let available = store.len();
                let length = if track {
                    available.checked_sub(offset)?
                } else {
                    length
                };
                (offset.checked_add(length)? <= available).then_some(length)
            });
            return CloneBrand::DataView {
                buffer: viewed,
                byte_offset: offset,
                byte_length,
            };
        }
        if object.borrow().props.contains("\u{0}ab_max_byte_length") {
            return if self.shared_buffers.contains_key(&pointer) {
                CloneBrand::SharedArrayBuffer
            } else {
                CloneBrand::ArrayBuffer {
                    detached: !self.array_buffers.contains_key(&pointer),
                }
            };
        }
        if let Some(regex) = self.regexps.get(&pointer) {
            return CloneBrand::RegExp {
                source: regex.source.clone(),
                flags: regex.flags.clone(),
            };
        }
        if let Some(data) = self.map_data.get(&pointer) {
            return match data.kind() {
                CollectionKind::Map => CloneBrand::Map,
                CollectionKind::Set => CloneBrand::Set,
                CollectionKind::WeakMap => CloneBrand::Uncloneable("WeakMap"),
                CollectionKind::WeakSet => CloneBrand::Uncloneable("WeakSet"),
            };
        }
        if crate::eval::promise_fast::promise_state(value).is_some() {
            return CloneBrand::Uncloneable("Promise");
        }
        if self.is_weak_ref_or_registry(pointer) {
            return CloneBrand::Uncloneable("WeakRef");
        }
        CloneBrand::Ordinary
    }

    /// The entries of a Map (`[[MapData]]`) or Set (key twice), in insertion order, copied so
    /// the caller can run script while it walks them.
    pub fn collection_entries(&self, value: &Value) -> Option<Vec<(Value, Value)>> {
        let pointer = Gc::as_ptr(value.as_obj()?) as usize;
        let data = self.map_data.get(&pointer)?;
        Some(
            data.iter()
                .map(|(key, value)| (key.unpack(), value.unpack()))
                .collect(),
        )
    }

    /// `Object.keys(value)` without the script-visible `Object`: the enumerable own string
    /// keys in order.
    pub fn enumerable_own_string_keys(&mut self, value: &Value) -> Result<Vec<Value>, Value> {
        let array = crate::builtins::object_keys_array(self, value.clone())?;
        let length = match self.member_get(&array, "length")? {
            Value::Num(length) if length >= 0.0 => length as usize,
            _ => 0,
        };
        (0..length)
            .map(|index| self.member_get(&array, &index.to_string()))
            .collect()
    }

    /// A `RangeError` ("Maximum call stack size exceeded") once the native stack is nearly
    /// exhausted; recursive host walkers call it before each level.
    pub fn check_native_stack_for_host(&mut self) -> Result<(), Value> {
        self.check_native_stack().map_err(abrupt_value)
    }

    /// CreateDataPropertyOrThrow(target, key, value): never runs a setter and never changes the
    /// prototype for the key `__proto__`.
    pub fn create_data_property(
        &mut self,
        target: &Value,
        key: &str,
        value: Value,
    ) -> Result<(), Value> {
        crate::builtins::cdp_or_throw(self, target, key, value)
    }

    /// A RegExp in this realm from an original source and flags.
    pub fn new_regexp_value(&mut self, source: &str, flags: &str) -> Result<Value, Value> {
        let prototype = self.extra_protos.get("RegExp").cloned();
        self.make_regexp_with_proto(source, flags, prototype)
            .map_err(abrupt_value)
    }

    /// `Object(primitive)` for a Boolean, Number, String, BigInt or Symbol, from the engine's
    /// own prototypes.
    pub fn new_boxed_primitive(&mut self, primitive: Value) -> Value {
        crate::builtins::box_primitive(self, primitive)
    }

    /// An empty Map, or Set when `set`.
    pub fn new_collection_value(&mut self, set: bool) -> Value {
        let (name, kind) = if set {
            ("Set", CollectionKind::Set)
        } else {
            ("Map", CollectionKind::Map)
        };
        let object = Object::new(self.extra_protos.get(name).cloned());
        let pointer = Gc::as_ptr(&object) as usize;
        self.gc_pin(&object);
        self.map_data.insert(pointer, CollectionData::new(kind));
        Value::Obj(object)
    }

    /// Append an entry to a collection made by [`Interp::new_collection_value`]; a Set ignores
    /// `value`.
    pub fn collection_insert(&mut self, collection: &Value, key: Value, value: Value) {
        let Some(object) = collection.as_obj() else {
            return;
        };
        let pointer = Gc::as_ptr(object) as usize;
        if let Some(data) = self.map_data.get_mut(&pointer) {
            let value = if data.has_values() { value } else { key.clone() };
            data.insert(key, value);
        }
    }

    /// A TypedArray of `kind` over `buffer` (an ArrayBuffer or SharedArrayBuffer value). A
    /// misaligned or out-of-range window is a `RangeError`.
    pub fn new_typed_array_view(
        &mut self,
        kind: TaKind,
        buffer: &Value,
        byte_offset: usize,
        length: usize,
    ) -> Result<Value, Value> {
        let Some(buffer_object) = buffer.as_obj() else {
            return Err(self.make_error("TypeError", "typed array buffer is not an ArrayBuffer"));
        };
        let buffer_pointer = Gc::as_ptr(buffer_object) as usize;
        let Some(available) = self.array_buffers.get(&buffer_pointer).map(|store| store.len())
        else {
            return Err(self.make_error("TypeError", "typed array buffer is detached"));
        };
        let size = kind.elsize();
        let end = length
            .checked_mul(size)
            .and_then(|bytes| bytes.checked_add(byte_offset));
        if byte_offset % size != 0 || end.is_none_or(|end| end > available) {
            return Err(self.make_error("RangeError", "typed array window is out of bounds"));
        }
        let object = Object::new(self.extra_protos.get(kind.name()).cloned());
        let pointer = Gc::as_ptr(&object) as usize;
        self.gc_pin(&object);
        object.borrow().ic_plain.set(false);
        self.typed_arrays.insert(
            pointer,
            TaInfo {
                buffer: buffer_pointer,
                offset: byte_offset,
                len: length,
                kind,
                track: false,
            },
        );
        self.ta_buffer.insert(pointer, buffer.clone());
        Ok(Value::Obj(object))
    }

    /// A DataView over `buffer` (an ArrayBuffer or SharedArrayBuffer value).
    pub fn new_data_view_value(
        &mut self,
        buffer: &Value,
        byte_offset: usize,
        byte_length: usize,
    ) -> Result<Value, Value> {
        let Some(buffer_object) = buffer.as_obj() else {
            return Err(self.make_error("TypeError", "DataView buffer is not an ArrayBuffer"));
        };
        let buffer_pointer = Gc::as_ptr(buffer_object) as usize;
        let Some(available) = self.array_buffers.get(&buffer_pointer).map(|store| store.len())
        else {
            return Err(self.make_error("TypeError", "DataView buffer is detached"));
        };
        if byte_offset
            .checked_add(byte_length)
            .is_none_or(|end| end > available)
        {
            return Err(self.make_error("RangeError", "DataView window is out of bounds"));
        }
        let object = Object::new(self.extra_protos.get("DataView").cloned());
        let pointer = Gc::as_ptr(&object) as usize;
        self.gc_pin(&object);
        self.data_views
            .insert(pointer, (buffer_pointer, byte_offset, byte_length, false));
        object.borrow_mut().props.insert(
            "\u{0}dv_buffer",
            crate::value::Property::data(buffer.clone(), true, false, false),
        );
        Ok(Value::Obj(object))
    }
}

impl TaKind {
    /// The byte structured clone writes for this element type.
    pub fn clone_code(self) -> u8 {
        match self {
            TaKind::I8 => 0,
            TaKind::U8 => 1,
            TaKind::U8Clamped => 2,
            TaKind::I16 => 3,
            TaKind::U16 => 4,
            TaKind::I32 => 5,
            TaKind::U32 => 6,
            TaKind::F32 => 7,
            TaKind::F64 => 8,
            TaKind::I64 => 9,
            TaKind::U64 => 10,
            TaKind::F16 => 11,
        }
    }

    /// The element type a [`TaKind::clone_code`] byte names.
    pub fn from_clone_code(code: u8) -> Option<TaKind> {
        Some(match code {
            0 => TaKind::I8,
            1 => TaKind::U8,
            2 => TaKind::U8Clamped,
            3 => TaKind::I16,
            4 => TaKind::U16,
            5 => TaKind::I32,
            6 => TaKind::U32,
            7 => TaKind::F32,
            8 => TaKind::F64,
            9 => TaKind::I64,
            10 => TaKind::U64,
            11 => TaKind::F16,
            _ => return None,
        })
    }

    /// Bytes per element.
    pub fn element_size(self) -> usize {
        self.elsize()
    }
}

impl Interp {
    /// The source text of a function, as `Function.prototype.toString` renders it.
    pub fn function_source_text(&self, function: &Value) -> String {
        crate::builtins::function_proto::function_source_text(function)
    }
}

/// A number that identifies an object for as long as it is alive and reachable (0 for a
/// primitive); structured clone keys its memory table on it.
pub fn object_identity(value: &Value) -> usize {
    match value {
        Value::Obj(object) => Gc::as_ptr(object) as usize,
        _ => 0,
    }
}
