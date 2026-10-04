use crate::value::{Callable, Gc, GcState, Value, enter_gc_state, gc_snapshot};
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub bytes: usize,
    pub objects: usize,
    pub queue_depth: usize,
    pub active_tasks: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            bytes: 128 << 20,
            objects: 2_000_000,
            queue_depth: 16,
            active_tasks: 8,
        }
    }
}

pub(crate) struct HeapGuard(Arc<GcState>);
impl HeapGuard {
    pub(crate) fn enter(heap: &Arc<GcState>) -> Self {
        Self(enter_gc_state(heap.clone()))
    }
}
impl Drop for HeapGuard {
    fn drop(&mut self) {
        enter_gc_state(self.0.clone());
    }
}

pub(crate) enum Intrinsic {
    Object,
    Array,
    String,
    Number,
    Boolean,
    Function,
    Error(&'static str),
    Extra(&'static str),
    Global(String),
}

#[derive(Default)]
pub(crate) struct SideTables {
    pub buffers: std::collections::HashMap<usize, Buffer>,
    pub maps: std::collections::HashMap<usize, crate::builtins::collection_data::CollectionData>,
    pub typed: std::collections::HashMap<usize, crate::value::TaInfo>,
    pub ta_buffer: std::collections::HashMap<usize, Value>,
    pub views: std::collections::HashMap<usize, (usize, usize, usize, bool)>,
    pub regexps: std::collections::HashMap<usize, std::rc::Rc<crate::regex::Regex>>,
    pub shared: std::collections::HashMap<usize, crate::interpreter::SharedBufferHandle>,
    pub classes: std::collections::HashMap<usize, crate::interpreter::ClassInfo>,
}

pub(crate) struct Buffer {
    pub bytes: Vec<u8>,
    pub max_len: Option<usize>,
    pub readonly: bool,
}

#[cfg(feature = "aot-native")]
pub(crate) struct NativeFunction {
    pub trusted_glue: bool,
    pub object: Gc,
    pub bytes: Arc<[u8]>,
    pub hash: [u8; 32],
    pub index: u32,
    pub env: Option<crate::interpreter::Env>,
}

#[cfg(feature = "aot-native")]
pub(crate) struct NativeClass {
    pub trusted_glue: bool,
    pub object: Gc,
    pub bytes: Arc<[u8]>,
    pub hash: [u8; 32],
    pub env: crate::interpreter::Env,
    pub derived: bool,
    pub body: Option<u32>,
    pub fields: Vec<crate::native_aot::classes::Field>,
    pub private_members: Vec<(String, crate::value::Property)>,
    pub initializers: Vec<Value>,
}

/// A closed value graph in an exclusively owned heap. No handles are exposed
/// until adoption; sender handles are always deeply copied.
pub struct Parcel {
    pub(crate) heap: Arc<GcState>,
    pub(crate) root: Value,
    pub(crate) protos: Vec<(Gc, Intrinsic)>,
    pub(crate) side: SideTables,
    pub(crate) functions: Vec<(
        Gc,
        std::rc::Rc<crate::ast::Function>,
        Option<crate::interpreter::Env>,
    )>,
    #[cfg(feature = "aot-native")]
    pub(crate) native_functions: Vec<NativeFunction>,
    #[cfg(feature = "aot-native")]
    pub(crate) native_classes: Vec<NativeClass>,
    pub(crate) scopes: Vec<crate::interpreter::Env>,
    pub(crate) scope_intrinsics: Vec<(crate::interpreter::Env, String, Intrinsic)>,
    pub(crate) class_protos: Vec<(Gc, Value)>,
    pub(crate) symbol_keys: Vec<(Gc, String, &'static str)>,
    pub(crate) field_symbols: Vec<(usize, usize, &'static str)>,
    pub(crate) bytes: usize,
    pub(crate) objects: usize,
    /// Host-owned transferable objects carried by structured-clone messages.
    /// Values are opaque capability ids; the receiving host reifies them only
    /// after it has accepted the parcel on the destination realm.
    pub(crate) attachments: Vec<(u64, u8)>,
    pub(crate) adopted: bool,
}
// SAFETY: only the builder creates graphs; it never stores a sender handle.
// Publication consumes the parcel. Adoption consumes it on one receiving thread.
unsafe impl Send for Parcel {}

impl Parcel {
    pub(crate) fn empty() -> Self {
        Self {
            heap: GcState::new(),
            root: Value::Undefined,
            protos: Vec::new(),
            side: SideTables::default(),
            functions: Vec::new(),
            #[cfg(feature = "aot-native")]
            native_functions: Vec::new(),
            #[cfg(feature = "aot-native")]
            native_classes: Vec::new(),
            scopes: Vec::new(),
            scope_intrinsics: Vec::new(),
            class_protos: Vec::new(),
            symbol_keys: Vec::new(),
            field_symbols: Vec::new(),
            bytes: 0,
            objects: 0,
            attachments: Vec::new(),
            adopted: false,
        }
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn objects(&self) -> usize {
        self.objects
    }
    pub fn attachments(&self) -> &[(u64, u8)] {
        &self.attachments
    }
}
impl Drop for Parcel {
    fn drop(&mut self) {
        if self.adopted {
            return;
        }
        let _entered = HeapGuard::enter(&self.heap);
        // Keep every box alive while breaking cycles; a walk must not free slots.
        let objects = gc_snapshot();
        for object in &objects {
            let mut object = object.borrow_mut();
            object.props.clear();
            object.proto = None;
            object.call = Callable::None;
        }
        self.root = Value::Undefined;
        self.protos.clear();
        self.side = SideTables::default();
        self.functions.clear();
        #[cfg(feature = "aot-native")]
        self.native_functions.clear();
        #[cfg(feature = "aot-native")]
        self.native_classes.clear();
        self.scopes.clear();
        self.scope_intrinsics.clear();
        self.class_protos.clear();
        self.symbol_keys.clear();
        drop(objects);
    }
}
