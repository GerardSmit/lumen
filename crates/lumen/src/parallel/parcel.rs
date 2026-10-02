use crate::value::{gc_snapshot, enter_gc_state, Callable, Gc, GcState, Value};
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
        Self { bytes: 128 << 20, objects: 2_000_000, queue_depth: 16, active_tasks: 8 }
    }
}

pub(crate) struct HeapGuard(Arc<GcState>);
impl HeapGuard {
    pub(crate) fn enter(heap: &Arc<GcState>) -> Self {
        Self(enter_gc_state(heap.clone()))
    }
}
impl Drop for HeapGuard {
    fn drop(&mut self) { enter_gc_state(self.0.clone()); }
}

pub(crate) enum Intrinsic {
    Object, Array, String, Number, Boolean, Error(&'static str), Extra(&'static str),
}

#[derive(Default)]
pub(crate) struct SideTables {
    pub buffers: std::collections::HashMap<usize, Vec<u8>>,
    pub maps: std::collections::HashMap<usize, crate::builtins::collection_data::CollectionData>,
    pub typed: std::collections::HashMap<usize, crate::value::TaInfo>,
    pub ta_buffer: std::collections::HashMap<usize, Value>,
    pub views: std::collections::HashMap<usize, (usize, usize, usize, bool)>,
    pub regexps: std::collections::HashMap<usize, std::rc::Rc<crate::regex::Regex>>,
    pub shared: std::collections::HashMap<usize, crate::interpreter::SharedBufferHandle>,
}

/// A closed value graph in an exclusively owned heap. No handles are exposed
/// until adoption; sender handles are always deeply copied.
pub struct Parcel {
    pub(crate) heap: Arc<GcState>,
    pub(crate) root: Value,
    pub(crate) protos: Vec<(Gc, Intrinsic)>,
    pub(crate) side: SideTables,
    pub(crate) bytes: usize,
    pub(crate) objects: usize,
    pub(crate) adopted: bool,
}
// SAFETY: only the builder creates graphs; it never stores a sender handle.
// Publication consumes the parcel. Adoption consumes it on one receiving thread.
unsafe impl Send for Parcel {}

impl Parcel {
    pub(crate) fn empty() -> Self {
        Self { heap: GcState::new(), root: Value::Undefined, protos: Vec::new(), side: SideTables::default(), bytes: 0, objects: 0, adopted: false }
    }
    pub fn bytes(&self) -> usize { self.bytes }
    pub fn objects(&self) -> usize { self.objects }
}
impl Drop for Parcel {
    fn drop(&mut self) {
        if self.adopted { return; }
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
        drop(objects);
    }
}
