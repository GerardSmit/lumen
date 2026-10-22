//! Parallel style resolution. A host that has threads supplies a
//! `StyleParallelism`; without one styling stays sequential and unchanged.
//!
//! The pre-pass computes the cascade top-down: the root on the calling
//! thread, then every worker pulls subtrees from one shared queue. A worker
//! that pops a subtree too heavy for one task styles only its root and queues
//! its children, so no chain of heavy nodes serializes the rest. Workers keep
//! private `StyleCache`s; results, pseudo-element styles included, are merged
//! into the shared cache on the calling thread, so layout later finds every
//! node as a cache hit and sees exactly the styles sequential computation
//! yields. Host callbacks stay on the calling thread: workers read answers it
//! resolved, and a task that asked for any other answer is discarded and
//! retried once the calling thread has asked the host.

use super::*;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
extern crate std;

/// Fork-join provider supplied by the host.
///
/// # Safety
///
/// While `run` executes, the implementor lets worker threads read the
/// `Document`, the `StyleIndex` and the text shaper concurrently. The host
/// guarantees that nothing mutates them meanwhile: `run` blocks its caller
/// and the threads it uses run nothing but `job`. Callbacks installed on the
/// `Document` are never called from workers.
pub unsafe trait StyleParallelism: Send + Sync {
    /// Number of threads `run` can use, including the caller.
    fn threads(&self) -> usize;
    /// Call `job(i)` exactly once for every `i` in `0..units`, on any thread,
    /// handing out indices dynamically, and return after all calls finished.
    fn run(&self, units: usize, job: &(dyn Fn(usize) + Sync));
    /// Index of the calling thread among the threads `run` uses, stable
    /// while `run` executes. Threads that share an index share their host
    /// misses, which only costs retries.
    fn worker(&self) -> usize { 0 }
    /// Called inside `run` by a thread that has nothing to do while other
    /// threads still work. Yielding the processor here lets a worker that
    /// shares the core finish sooner.
    fn wait(&self) { core::hint::spin_loop() }
}

/// `RefCell` with the same API that is safe to share: concurrent shared
/// borrows, exclusive writers, and waiting instead of panicking.
pub(crate) struct SyncRefCell<T> {
    state: AtomicUsize,
    value: UnsafeCell<T>,
}

const WRITER: usize = usize::MAX;

unsafe impl<T: Send + Sync> Sync for SyncRefCell<T> {}

impl<T> SyncRefCell<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self { state: AtomicUsize::new(0), value: UnsafeCell::new(value) }
    }
    pub(crate) fn get_mut(&mut self) -> &mut T { self.value.get_mut() }
    pub(crate) fn into_inner(self) -> T { self.value.into_inner() }
    pub(crate) fn borrow(&self) -> SyncRef<'_, T> {
        loop {
            let state = self.state.load(Ordering::Relaxed);
            if state != WRITER
                && self.state.compare_exchange_weak(state, state + 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                return SyncRef { cell: self };
            }
            core::hint::spin_loop();
        }
    }
    pub(crate) fn borrow_mut(&self) -> SyncRefMut<'_, T> {
        loop {
            if self.state.compare_exchange_weak(0, WRITER, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                return SyncRefMut { cell: self };
            }
            core::hint::spin_loop();
        }
    }
}

pub(crate) struct SyncRef<'a, T> { cell: &'a SyncRefCell<T> }

impl<T> core::ops::Deref for SyncRef<'_, T> {
    type Target = T;
    fn deref(&self) -> &T { unsafe { &*self.cell.value.get() } }
}

impl<T> Drop for SyncRef<'_, T> {
    fn drop(&mut self) { self.cell.state.fetch_sub(1, Ordering::Release); }
}

pub(crate) struct SyncRefMut<'a, T> { cell: &'a SyncRefCell<T> }

impl<T> core::ops::Deref for SyncRefMut<'_, T> {
    type Target = T;
    fn deref(&self) -> &T { unsafe { &*self.cell.value.get() } }
}

impl<T> core::ops::DerefMut for SyncRefMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T { unsafe { &mut *self.cell.value.get() } }
}

impl<T> Drop for SyncRefMut<'_, T> {
    fn drop(&mut self) { self.cell.state.store(0, Ordering::Release); }
}

impl<T: Default> Default for SyncRefCell<T> {
    fn default() -> Self { Self::new(T::default()) }
}

/// An `Rc` that may be shared between threads because shared access only
/// reads the value; cloning or dropping the `Rc` needs exclusive access.
pub(crate) struct ReadOnlyRc<T>(Rc<T>);

unsafe impl<T: Sync> Sync for ReadOnlyRc<T> {}

impl<T> ReadOnlyRc<T> {
    pub(crate) fn new(value: Rc<T>) -> Self { Self(value) }
    pub(crate) fn share(&mut self) -> Rc<T> { self.0.clone() }
}

impl<T> core::ops::Deref for ReadOnlyRc<T> {
    type Target = T;
    fn deref(&self) -> &T { &self.0 }
}

/// A host callback stored in the `Document`. Callbacks belong to the thread
/// that installed them, so while parallel style resolution shares the
/// document none is called: `HostGate` answers what the calling thread
/// resolved beforehand and records every other question as a miss.
pub(crate) struct Hook<T>(Option<T>);

// Test builds let the compiler check that every other `Document` field is
// `Sync`; elsewhere the callbacks keep `Document` from being `Sync`.
#[cfg(test)]
unsafe impl<T> Sync for Hook<T> {}

impl<T> Hook<T> {
    pub(crate) const fn none() -> Self { Self(None) }
    pub(crate) fn new(value: Option<T>) -> Self { Self(value) }
    pub(crate) fn is_some(&self) -> bool { self.0.is_some() }
    /// The callback, unless it is installed while the document is shared:
    /// then the question is a miss and the answer that of an absent callback.
    pub(crate) fn get(&self, gate: &HostGate) -> Option<&T> {
        let value = self.0.as_ref()?;
        if gate.frozen() {
            gate.miss(None);
            return None;
        }
        Some(value)
    }
    pub(crate) fn get_mut(&mut self) -> Option<&mut T> { self.0.as_mut() }
    /// Installed while the document is shared, so `HostGate` must answer.
    pub(crate) fn gated(&self, gate: &HostGate) -> bool { self.0.is_some() && gate.frozen() }
}

/// A question to a host callback that a worker could not ask.
pub(crate) enum HostQuestion {
    Target,
    Validity(NodeId),
    FormValue(NodeId),
    FormState(NodeId),
    CustomState(NodeId, String),
    TemplateAncestor(NodeId, NodeId),
}

#[derive(Default)]
struct HostAnswers {
    target: Option<Option<NodeId>>,
    validity: BTreeMap<u128, Option<crate::forms::ValidityState>>,
    form_value: BTreeMap<u128, Option<String>>,
    form_state: BTreeMap<u128, Option<crate::forms::FormSelectorState>>,
    custom_state: BTreeMap<u128, Vec<(String, bool)>>,
    template_ancestor: BTreeMap<(u128, u128), Result<bool, crate::Error>>,
}

/// While frozen, the document is shared between threads and its host
/// callbacks answer from values resolved on the owning thread. Every other
/// question counts as a miss of the worker that asked; work that saw one is
/// discarded.
#[derive(Default)]
pub(crate) struct HostGate {
    frozen: AtomicBool,
    misses: [AtomicUsize; GATE_WORKERS],
    provider: SyncRefCell<Option<Arc<dyn StyleParallelism>>>,
    answers: SyncRefCell<HostAnswers>,
    questions: SyncRefCell<Vec<HostQuestion>>,
}

const GATE_WORKERS: usize = 16;

impl HostGate {
    pub(crate) fn frozen(&self) -> bool { self.frozen.load(Ordering::Acquire) }

    /// The calling thread asked no unanswered question since the document was frozen.
    pub(crate) fn exact(&self) -> bool { !self.frozen() || self.worker_misses(self.current_worker()) == 0 }

    /// The miss slot of the calling thread.
    pub(crate) fn current_worker(&self) -> usize {
        self.provider.borrow().as_ref().map_or(0, |provider| provider.worker()) % GATE_WORKERS
    }

    /// Unanswered questions asked from `worker` since the document was frozen.
    pub(crate) fn worker_misses(&self, worker: usize) -> usize { self.misses[worker % GATE_WORKERS].load(Ordering::SeqCst) }

    fn miss(&self, question: Option<HostQuestion>) {
        self.misses[self.current_worker()].fetch_add(1, Ordering::SeqCst);
        if let Some(question) = question {
            let mut questions = self.questions.borrow_mut();
            if questions.len() < MAX_HOST_QUESTIONS { questions.push(question); }
        }
    }

    fn answer<R>(&self, find: impl FnOnce(&HostAnswers) -> Option<R>, question: impl FnOnce() -> HostQuestion, absent: R) -> R {
        if let Some(answer) = find(&self.answers.borrow()) { return answer; }
        self.miss(Some(question()));
        absent
    }

    pub(crate) fn target(&self) -> Option<NodeId> {
        self.answer(|answers| answers.target, || HostQuestion::Target, None)
    }
    pub(crate) fn validity(&self, node: NodeId) -> Option<crate::forms::ValidityState> {
        self.answer(|answers| answers.validity.get(&node.key()).copied(), || HostQuestion::Validity(node), None)
    }
    pub(crate) fn form_value(&self, node: NodeId) -> Option<String> {
        self.answer(|answers| answers.form_value.get(&node.key()).cloned(), || HostQuestion::FormValue(node), None)
    }
    pub(crate) fn form_state(&self, node: NodeId) -> Option<crate::forms::FormSelectorState> {
        self.answer(|answers| answers.form_state.get(&node.key()).copied(), || HostQuestion::FormState(node), None)
    }
    pub(crate) fn custom_state(&self, node: NodeId, name: &str) -> bool {
        self.answer(
            |answers| answers.custom_state.get(&node.key())?.iter().find(|(known, _)| known == name).map(|(_, state)| *state),
            || HostQuestion::CustomState(node, String::from(name)),
            false,
        )
    }
    pub(crate) fn template_ancestor(&self, ancestor: NodeId, node: NodeId) -> Result<bool, crate::Error> {
        self.answer(
            |answers| answers.template_ancestor.get(&(ancestor.key(), node.key())).copied(),
            || HostQuestion::TemplateAncestor(ancestor, node),
            Ok(false),
        )
    }
}

const MAX_HOST_QUESTIONS: usize = 4096;

impl Document {
    /// Share the document between threads: host callbacks stop being called.
    /// `provider` tells the gate which worker asks a question.
    pub(crate) fn freeze_host(&self, provider: Option<Arc<dyn StyleParallelism>>) {
        for misses in &self.host_gate.misses { misses.store(0, Ordering::SeqCst); }
        *self.host_gate.provider.borrow_mut() = provider;
        self.host_gate.frozen.store(true, Ordering::SeqCst);
    }

    pub(crate) fn thaw_host(&self) {
        self.host_gate.frozen.store(false, Ordering::SeqCst);
        *self.host_gate.provider.borrow_mut() = None;
    }

    /// Answer up front the questions selector matching asks for most: the
    /// target element and the state of form controls, so workers rarely miss.
    pub(crate) fn prime_host_answers(&self, root: NodeId) {
        {
            let mut questions = self.host_gate.questions.borrow_mut();
            questions.push(HostQuestion::Target);
            let mut stack = alloc::vec![root];
            let mut children = Vec::new();
            while let Some(node) = stack.pop() {
                if questions.len() >= MAX_HOST_QUESTIONS { break; }
                if let Ok(NodeKind::Element { name, .. }) = self.kind(node) {
                    let local = crate::svg::local_name(name);
                    let control = matches!(local, "input" | "select" | "textarea");
                    if control || matches!(local, "button" | "fieldset" | "output" | "form" | "object" | "option" | "optgroup") || local.contains('-') {
                        questions.push(HostQuestion::FormState(node));
                        questions.push(HostQuestion::Validity(node));
                    }
                    if control { questions.push(HostQuestion::FormValue(node)); }
                }
                element_children(self, node, &mut children);
                stack.extend(children.iter().copied());
            }
        }
        self.answer_host_questions();
    }

    /// Ask the host the questions workers recorded, on the owning thread.
    pub(crate) fn answer_host_questions(&self) -> bool {
        let questions = core::mem::take(&mut *self.host_gate.questions.borrow_mut());
        let answered = !questions.is_empty();
        for question in questions {
            match question {
                HostQuestion::Target => {
                    let target = self.target_element();
                    self.host_gate.answers.borrow_mut().target = Some(target);
                }
                HostQuestion::Validity(node) => {
                    let state = self.validity_state(node);
                    self.host_gate.answers.borrow_mut().validity.insert(node.key(), state);
                }
                HostQuestion::FormValue(node) => {
                    let value = self.with_form_value(node, |value| value.map(String::from));
                    self.host_gate.answers.borrow_mut().form_value.insert(node.key(), value);
                }
                HostQuestion::FormState(node) => {
                    let state = self.form_selector_state(node);
                    self.host_gate.answers.borrow_mut().form_state.insert(node.key(), state);
                }
                HostQuestion::CustomState(node, name) => {
                    let state = self.has_custom_state(node, &name);
                    let mut answers = self.host_gate.answers.borrow_mut();
                    let known = answers.custom_state.entry(node.key()).or_default();
                    if !known.iter().any(|(existing, _)| *existing == name) { known.push((name, state)); }
                }
                HostQuestion::TemplateAncestor(ancestor, node) => {
                    let result = self.is_host_including_inclusive_ancestor(ancestor, node);
                    self.host_gate.answers.borrow_mut().template_ancestor.insert((ancestor.key(), node.key()), result);
                }
            }
        }
        answered
    }

    pub(crate) fn forget_host_answers(&self) {
        *self.host_gate.answers.borrow_mut() = HostAnswers::default();
        self.host_gate.questions.borrow_mut().clear();
    }
}

const fn assert_send_sync<T: Send + Sync>() {}
#[cfg(test)]
const fn assert_sync<T: Sync + ?Sized>() {}
#[cfg(test)]
const _: () = assert_sync::<Document>();
const _: () = assert_send_sync::<Style>();
const _: () = assert_send_sync::<StyleIndex>();

/// The document as workers see it.
///
/// Every `Document` field but the host callbacks is `Sync`, which test builds
/// check; the callbacks are not called while the document is frozen.
struct SharedDocument<'a>(&'a Document);
unsafe impl Sync for SharedDocument<'_> {}

impl SharedDocument<'_> {
    fn get(&self) -> &Document { self.0 }
}


const _: () = assert_send_sync::<GeneratedStyle>();

/// A pseudo-element style a worker computed. `from` names the earlier result
/// whose style is the origin the pseudo-element was asked with; `None` is the
/// element itself.
struct PseudoOut {
    pseudo: PseudoElement,
    from: Option<usize>,
    role: BoxParentContext,
    generated: Option<GeneratedStyle>,
}

struct Entry {
    node: NodeId,
    parent: Arc<Style>,
    box_parent: BoxParentContext,
    style: Arc<Style>,
    viewport: bool,
    pseudos: Vec<PseudoOut>,
    pseudo_viewport: bool,
}

const MAX_DEPTH: usize = 512;
/// Empty polls of the task queue before an idle worker sleeps.
const IDLE_SPINS: u32 = 64;
/// Longest an idle worker sleeps before it polls again.
const IDLE_PARK: core::time::Duration = core::time::Duration::from_millis(2);
/// Tasks per thread the split aims for, so dynamic scheduling evens out
/// subtrees of different cost.
const UNITS_PER_THREAD: usize = 8;
/// Fewer uncomputed elements than this are not worth the threads.
const MIN_PARALLEL_ELEMENTS: usize = 256;
/// Rounds of workers: a round whose tasks needed host answers is repeated
/// once the calling thread has asked the host.
const HOST_ROUNDS: usize = 3;

fn element_children(document: &Document, node: NodeId, out: &mut Vec<NodeId>) {
    out.clear();
    if let Ok(children) = document.composed_children(node) {
        out.extend(children.into_iter().filter(|&child| matches!(document.kind(child), Ok(NodeKind::Element { .. }))));
    }
}

/// Per element of the composed tree below `root`, the number of elements in
/// its subtree (itself included) that `cache` holds no style for.
fn uncomputed_weights(document: &Document, root: NodeId, cache: &StyleCache) -> Vec<u32> {
    let mut weights: Vec<u32> = Vec::new();
    let mut order = Vec::new();
    let mut stack = alloc::vec![(root, None::<NodeId>, 0usize)];
    let mut children = Vec::new();
    while let Some((node, parent, depth)) = stack.pop() {
        if weights.len() <= node.index() { weights.resize(node.index() + 1, 0); }
        weights[node.index()] = u32::from(cache.node_entry(node).is_none());
        order.push((node, parent));
        if depth >= MAX_DEPTH { continue; }
        element_children(document, node, &mut children);
        stack.extend(children.iter().map(|&child| (child, Some(node), depth + 1)));
    }
    for &(node, parent) in order.iter().rev() {
        if let Some(parent) = parent {
            let weight = weights[node.index()];
            weights[parent.index()] = weights[parent.index()].saturating_add(weight);
        }
    }
    weights
}

/// Sibling subtrees waiting to be styled with the style they inherit. A task
/// of one node heavier than the budget is split by styling that node alone.
struct Task {
    weight: u32,
    parent: Arc<Style>,
    depth: usize,
    nodes: Vec<NodeId>,
}

impl PartialEq for Task {
    fn eq(&self, other: &Self) -> bool { self.weight == other.weight }
}
impl Eq for Task {}
impl PartialOrd for Task {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> { Some(self.cmp(other)) }
}
impl Ord for Task {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering { self.weight.cmp(&other.weight) }
}

/// Tasks for the children of a styled node: every heavy child alone, the
/// light ones grouped up to the budget. Children without uncomputed
/// elements need nothing.
fn child_tasks(children: &[NodeId], weights: &[u32], parent: &Arc<Style>, depth: usize, budget: u32) -> Vec<Task> {
    let mut tasks = Vec::new();
    let mut group = Vec::new();
    let mut group_weight = 0u32;
    for &child in children {
        let weight = weights.get(child.index()).copied().unwrap_or(0);
        if weight == 0 { continue; }
        if weight > budget {
            tasks.push(Task { weight, parent: parent.clone(), depth, nodes: alloc::vec![child] });
            continue;
        }
        group.push(child);
        group_weight += weight;
        if group_weight >= budget {
            tasks.push(Task { weight: group_weight, parent: parent.clone(), depth, nodes: core::mem::take(&mut group) });
            group_weight = 0;
        }
    }
    if !group.is_empty() { tasks.push(Task { weight: group_weight, parent: parent.clone(), depth, nodes: group }); }
    tasks
}

/// One round of workers: the queue, the tasks still to finish, and what the
/// workers hand back.
struct Round {
    queue: SyncRefCell<alloc::collections::BinaryHeap<Task>>,
    /// Tasks in `queue`, read without taking its lock.
    queued: AtomicUsize,
    /// Tasks queued or running; zero ends the round.
    outstanding: AtomicUsize,
    missed: SyncRefCell<Vec<Task>>,
    results: SyncRefCell<Vec<Vec<Entry>>>,
    parker: Parker,
}

/// Where idle workers sleep until a task is queued or the round ends.
#[derive(Default)]
struct Parker {
    lock: std::sync::Mutex<()>,
    wake: std::sync::Condvar,
    sleepers: AtomicUsize,
}

impl Parker {
    /// Sleep unless `ready` already holds. The sleeper registers before it
    /// checks `ready`, and publishers change state before they read the
    /// sleeper count (all `SeqCst`), so a wakeup cannot slip between the two;
    /// the timeout only bounds a host that preempts a publisher.
    fn park(&self, ready: impl Fn() -> bool) {
        let guard = self.lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.sleepers.fetch_add(1, Ordering::SeqCst);
        if !ready() {
            drop(self.wake.wait_timeout(guard, IDLE_PARK).unwrap_or_else(|poisoned| poisoned.into_inner()));
        }
        self.sleepers.fetch_sub(1, Ordering::SeqCst);
    }

    fn unpark(&self) {
        if self.sleepers.load(Ordering::SeqCst) == 0 { return; }
        drop(self.lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
        self.wake.notify_all();
    }
}

/// Counts a task as finished even when its styling unwinds.
struct Finished<'a>(&'a Round);

impl Drop for Finished<'_> {
    fn drop(&mut self) {
        if self.0.outstanding.fetch_sub(1, Ordering::SeqCst) == 1 { self.0.parker.unpark(); }
    }
}

/// Pseudo-element styles layout asks for next to the style of `node`:
/// `::marker` of list items, `::before` and `::after` when some rule can
/// generate them, and the markers of generated list items.
fn style_pseudos(document: &Document, index: &StyleIndex, text: &(dyn TextShaper + Sync), local: &mut StyleCache,
    node: NodeId, style: &Style) -> Vec<PseudoOut> {
    let mut out = Vec::new();
    let list_item = style.is_list_item();
    let generated = index.prestyles_generated_pseudos();
    if style.display == Display::None || !(list_item || generated) { return out; }
    let mut ask = |out: &mut Vec<PseudoOut>, from: Option<usize>, origin: &Style, pseudo: PseudoElement| -> Option<usize> {
        let generated = index.compute_pseudo_cached(document, node, origin, pseudo, Some(text), local).ok()?;
        let role = index.box_children_context(document, node, origin, Some(text), local).ok()?;
        let token = local.parent_token(Some(origin))?;
        local.pseudo_entry(node, pseudo, token, role)?;
        out.push(PseudoOut { pseudo, from, role, generated });
        Some(out.len() - 1)
    };
    if list_item { ask(&mut out, None, style, PseudoElement::Marker); }
    if generated {
        for (pseudo, marker) in [(PseudoElement::Before, PseudoElement::BeforeMarker), (PseudoElement::After, PseudoElement::AfterMarker)] {
            let Some(at) = ask(&mut out, None, style, pseudo) else { continue };
            let Some(origin) = out[at].generated.as_ref()
                .filter(|generated| generated.style.display != Display::None || generated.pseudo.is_marker())
                .map(|generated| generated.style.clone()) else { continue };
            ask(&mut out, Some(at), &origin, marker);
        }
    }
    out
}

/// Store worker results in `cache` the way layout would have computed them.
fn merge_entries(document: &Document, cache: &mut StyleCache, mut entries: Vec<Entry>) {
    entries.sort_unstable_by_key(|entry| entry.node.index());
    for entry in entries {
        if cache.node_entry(entry.node).is_some() { continue; }
        let Some(token) = cache.parent_token_with(Some(&entry.parent), Some(&entry.parent)) else { continue };
        cache.store(document, entry.node, token, entry.box_parent, &entry.style);
        if entry.viewport { cache.note_viewport_dependent(entry.node, false); }
        cache.computed += 1;
        for out in &entry.pseudos {
            let origin: &Style = match out.from {
                None => &entry.style,
                Some(from) => match &entry.pseudos[from].generated {
                    Some(generated) => &generated.style,
                    None => continue,
                },
            };
            let Some(token) = cache.parent_token(Some(origin)) else { continue };
            cache.store_pseudo(document, entry.node, out.pseudo, token, out.role, &out.generated);
        }
        if entry.pseudo_viewport { cache.note_viewport_dependent(entry.node, true); }
    }
}

impl StyleIndex {
    /// Install the host's fork-join provider; `None` restores sequential styling.
    ///
    /// # Safety
    ///
    /// See `StyleParallelism`.
    pub unsafe fn set_parallelism(&mut self, parallelism: Option<Arc<dyn StyleParallelism>>) {
        self.parallelism = parallelism;
    }

    /// Fill the lazily built shared tables so workers only read them.
    fn warm_shared_tables(&self) {
        let registrations = self.registered_snapshot();
        let _ = self.media_snapshot();
        self.registration_epoch.borrow_mut().bind(&registrations);
        let _ = self.style_sharing();
    }

    /// Whether `::before` and `::after` can exist for some element, so their
    /// styles are worth computing ahead of layout.
    fn prestyles_generated_pseudos(&self) -> bool {
        !self.generated_pseudo_rules.is_empty() || !self.snapshot_declarations.is_empty() || !self.effect_declarations.is_empty()
    }

    /// Style `node` in a worker's cache and record the result. `None` when
    /// the style could not be computed.
    fn style_node(&self, document: &Document, text: &(dyn TextShaper + Sync), local: &mut StyleCache,
        entries: &mut Vec<Entry>, node: NodeId, parent: &Arc<Style>) -> Option<Arc<Style>> {
        let computed = compute_node_cached_impl(document, node, Some(parent), self, local, Some(text)).ok()?;
        let Some((_, _, box_parent, style)) = local.node_entry(node) else { return Some(Arc::new(computed)) };
        let (box_parent, style) = (*box_parent, style.clone());
        let flags = local.viewport_flags.get(node.index()).copied().unwrap_or(0);
        let pseudos = style_pseudos(document, self, text, local, node, &style);
        let pseudo_viewport = !pseudos.is_empty()
            && local.viewport_flags.get(node.index()).is_some_and(|flags| flags & 2 != 0);
        entries.push(Entry { node, parent: parent.clone(), box_parent, style: style.clone(), viewport: flags & 1 != 0, pseudos, pseudo_viewport });
        Some(style)
    }

    /// Style one task. A heavy single node is styled alone and its children
    /// come back as new tasks; anything else is styled whole.
    fn style_task(&self, document: &Document, text: &(dyn TextShaper + Sync), local: &mut StyleCache,
        entries: &mut Vec<Entry>, task: &Task, weights: &[u32], budget: u32) -> Vec<Task> {
        let mut children = Vec::new();
        if task.nodes.len() == 1 && task.weight > budget {
            let node = task.nodes[0];
            let Some(style) = self.style_node(document, text, local, entries, node, &task.parent) else { return Vec::new() };
            if style.display == Display::None || task.depth >= MAX_DEPTH { return Vec::new(); }
            element_children(document, node, &mut children);
            return child_tasks(&children, weights, &style, task.depth + 1, budget);
        }
        let mut stack: Vec<(NodeId, Arc<Style>, usize)> = task.nodes.iter().map(|&node| (node, task.parent.clone(), task.depth)).collect();
        while let Some((node, parent, depth)) = stack.pop() {
            let Some(style) = self.style_node(document, text, local, entries, node, &parent) else { continue };
            if style.display == Display::None || depth >= MAX_DEPTH { continue; }
            element_children(document, node, &mut children);
            stack.extend(children.iter()
                .filter(|child| weights.get(child.index()).is_some_and(|&weight| weight > 0))
                .map(|&child| (child, style.clone(), depth + 1)));
        }
        Vec::new()
    }

    /// One worker's share of a round: pull tasks until none is queued or
    /// running. A task that asked the host something its worker could not get
    /// is left for the next round, and the worker restarts its cache so no
    /// style computed from the missing answer is reused.
    fn style_worker(&self, document: &Document, text: &(dyn TextShaper + Sync),
        round: &Round, weights: &[u32], budget: u32) {
        let gate = &document.host_gate;
        let worker = gate.current_worker();
        let mut local = StyleCache::default();
        let mut entries = Vec::new();
        let mut idle = 0u32;
        loop {
            let task = if round.queued.load(Ordering::Acquire) > 0 {
                let task = round.queue.borrow_mut().pop();
                if task.is_some() { round.queued.fetch_sub(1, Ordering::AcqRel); }
                task
            } else {
                None
            };
            let Some(task) = task else {
                if round.outstanding.load(Ordering::Acquire) == 0 { break; }
                idle += 1;
                if idle < IDLE_SPINS { core::hint::spin_loop() } else {
                    round.parker.park(|| round.queued.load(Ordering::SeqCst) > 0 || round.outstanding.load(Ordering::SeqCst) == 0);
                }
                continue;
            };
            idle = 0;
            let _finished = Finished(round);
            let before = gate.worker_misses(worker);
            let mark = entries.len();
            let spawned = self.style_task(document, text, &mut local, &mut entries, &task, weights, budget);
            if gate.worker_misses(worker) == before {
                if !spawned.is_empty() {
                    round.outstanding.fetch_add(spawned.len(), Ordering::SeqCst);
                    let count = spawned.len();
                    round.queue.borrow_mut().extend(spawned);
                    round.queued.fetch_add(count, Ordering::SeqCst);
                    round.parker.unpark();
                }
            } else {
                entries.truncate(mark);
                local = StyleCache::default();
                round.missed.borrow_mut().push(task);
            }
        }
        round.results.borrow_mut().push(entries);
    }

    /// Compute the styles layout is about to ask for in parallel and store
    /// them in `cache`. Does nothing without a provider, when most elements
    /// already have a style, or when the text shaper cannot be shared
    /// between threads. Results equal those of sequential computation:
    /// nodes whose styling needed a host callback the workers could not
    /// call are left for layout to style.
    pub(crate) fn prestyle(&self, document: &Document, cache: &mut StyleCache, text: &dyn TextShaper) {
        let Some(provider) = self.parallelism.clone() else { return };
        let threads = provider.threads();
        if threads < 2 || cache.selected.is_some() || document.host_gate.frozen() { return }
        let cached = cache.nodes.iter().filter(|entry| entry.is_some()).count();
        if cached * 2 >= document.node_count() { return; }
        let Some(root) = crate::selector::document_element(document) else { return };
        let weights = uncomputed_weights(document, root, cache);
        let uncomputed = weights.get(root.index()).copied().unwrap_or(0) as usize;
        if uncomputed < MIN_PARALLEL_ELEMENTS { return }
        let Some(view) = text.parallel_view() else { return };
        let text: &(dyn TextShaper + Sync) = &*view;
        cache.prepare_text_context(Some(text));
        self.warm_shared_tables();

        let Ok(computed) = compute_node_cached_impl(document, root, None, self, cache, Some(text)) else { return };
        let root_style = cache.node_entry(root).map(|entry| entry.3.clone()).unwrap_or_else(|| Arc::new(computed));
        if root_style.display == Display::None { return; }
        let budget = (uncomputed / (threads * UNITS_PER_THREAD)).max(16) as u32;
        let mut children = Vec::new();
        element_children(document, root, &mut children);
        let mut tasks = child_tasks(&children, &weights, &root_style, 1, budget);
        if tasks.is_empty() { return }

        document.prime_host_answers(root);
        for _ in 0..HOST_ROUNDS {
            let round = Round {
                outstanding: AtomicUsize::new(tasks.len()),
                queued: AtomicUsize::new(tasks.len()),
                queue: SyncRefCell::new(tasks.into_iter().collect()),
                missed: SyncRefCell::new(Vec::new()),
                results: SyncRefCell::new(Vec::new()),
                parker: Parker::default(),
            };
            document.freeze_host(Some(provider.clone()));
            {
                let shared = SharedDocument(document);
                let (round, weights) = (&round, &weights);
                provider.run(threads, &|_| self.style_worker(shared.get(), text, round, weights, budget));
            }
            document.thaw_host();
            let Round { missed, results, .. } = round;
            merge_entries(document, cache, results.into_inner().into_iter().flatten().collect());
            let missed = missed.into_inner();
            if missed.is_empty() || !document.answer_host_questions() { break; }
            tasks = missed;
        }
        document.forget_host_answers();
    }
}

#[cfg(test)]
mod parker_tests {
    use super::*;
    use std::sync::Arc as StdArc;

    #[test]
    fn park_returns_at_once_when_ready() {
        let parker = Parker::default();
        parker.park(|| true);
        assert_eq!(parker.sleepers.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn unpark_wakes_a_sleeper_before_the_timeout() {
        let parker = StdArc::new(Parker::default());
        let flag = StdArc::new(AtomicBool::new(false));
        let sleeper = {
            let (parker, flag) = (parker.clone(), flag.clone());
            std::thread::spawn(move || {
                let started = std::time::Instant::now();
                while !flag.load(Ordering::SeqCst) { parker.park(|| flag.load(Ordering::SeqCst)); }
                started.elapsed()
            })
        };
        std::thread::sleep(core::time::Duration::from_millis(20));
        flag.store(true, Ordering::SeqCst);
        parker.unpark();
        assert!(sleeper.join().unwrap() < core::time::Duration::from_secs(5));
        assert_eq!(parker.sleepers.load(Ordering::SeqCst), 0);
    }
}
