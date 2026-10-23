//! Parallel style resolution computes exactly the styles sequential
//! resolution does, and never calls host callbacks off the owning thread.

extern crate std;

use super::*;
use alloc::{rc::Rc, string::String, sync::Arc, vec::Vec};
use crate::paint::{ShapedRun, TextShaper};
use core::sync::atomic::{AtomicUsize, Ordering};
use std::thread::ThreadId;

struct Threads {
    threads: usize,
    runs: AtomicUsize,
    units: AtomicUsize,
}

std::thread_local! {
    static WORKER: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

unsafe impl css::StyleParallelism for Threads {
    fn threads(&self) -> usize { self.threads }
    fn run(&self, units: usize, job: &(dyn Fn(usize) + Sync)) {
        self.runs.fetch_add(1, Ordering::Relaxed);
        self.units.fetch_add(units, Ordering::Relaxed);
        let next = AtomicUsize::new(0);
        let work = || loop {
            let unit = next.fetch_add(1, Ordering::Relaxed);
            if unit >= units { break; }
            job(unit);
        };
        std::thread::scope(|scope| {
            for index in 1..self.threads {
                let work = &work;
                scope.spawn(move || {
                    WORKER.with(|worker| worker.set(index));
                    work();
                });
            }
            work();
        });
    }
    fn worker(&self) -> usize { WORKER.with(|worker| worker.get()) }
}

struct Text;

impl TextShaper for Text {
    fn parallel_view(&self) -> Option<crate::paint::ParallelText<'_>> {
        Some(crate::paint::ParallelText::Borrowed(self))
    }
    fn shape(&self, text: &str, size: f32) -> Result<ShapedRun, ()> {
        let glyphs: Vec<crate::paint::Glyph> = text.char_indices()
            .map(|(at, _)| crate::paint::Glyph { id: 1, face: 1, cluster: at as u32, caps_expansion: 0, x_offset: 0.0, x: at as f32 * size * 0.5, y: 0.0, size_scale: 1.0 })
            .collect();
        Ok(ShapedRun { glyphs: Arc::from(glyphs), width: text.chars().count() as f32 * size * 0.5 })
    }
    fn ascent(&self, size: f32) -> f32 { size * 0.8 }
    fn line_height(&self, size: f32) -> f32 { size * 1.2 }
}

const STYLES: &str = "<style>
@property --gap { syntax: '<length>'; inherits: true; initial-value: 2px }
:root { --accent: rgb(1 2 3); --gap: 3px; font: 16px sans-serif }
section { container-type: inline-size; padding: var(--gap) }
@container (min-width: 100px) { .card p { color: green } }
.card { --accent: rgb(4 5 6); border: 1px solid var(--accent); margin: calc(var(--gap) * 2) }
.card:has(> em) { background: rgb(7 8 9) }
p:nth-child(2n+1) { font-size: 1.25em; line-height: 1.5 }
p:nth-child(3n) { letter-spacing: 1ch; width: 10em }
li:first-child::before { content: '>' }
li::marker { color: red }
em { --gap: 5px; padding: var(--gap) }
x-widget:state(open) { display: block; color: rgb(10 11 12) }
input:checked + label { font-weight: bold }
input:invalid { outline: 1px solid red }
:target { color: rgb(13 14 15) }
</style>";

fn fixture(cards: usize) -> String {
    let mut source = String::from("<!doctype html>");
    source.push_str(STYLES);
    source.push_str("<body>");
    for card in 0..cards {
        source.push_str(&alloc::format!(
            "<div><section><div class=card id=card{card}><p>a</p><p><em>b</em></p><p>c</p><ul><li>d</li><li>e</li></ul>\
             <x-widget id=w{card}><template shadowrootmode=open><style>:host {{ color: rgb(20 21 22) }} b {{ margin: 1em }}</style><b>s</b><slot></slot></template><i>t</i></x-widget>\
             <input type=checkbox id=c{card}><label for=c{card}>l</label><input type=checkbox required></div></section></div>"
        ));
    }
    source
}

fn session_from(source: &str) -> RenderSession {
    let options = crate::html::ParseOptions { allow_declarative_shadow_roots: true, ..crate::html::ParseOptions::default() };
    RenderSession::new(crate::html::parse_with_options(source, 1 << 16, options).unwrap())
}

fn session(cards: usize) -> RenderSession {
    session_from(&fixture(cards))
}

fn elements(document: &Document) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut stack = alloc::vec![document.root()];
    while let Some(node) = stack.pop() {
        if matches!(document.kind(node), Ok(NodeKind::Element { .. })) { out.push(node); }
        if let Ok(children) = document.composed_children(node) { stack.extend(children); }
    }
    out
}

/// Lays `session` out sequentially, then prestyles a fresh cache in parallel
/// and checks every style it produced against the sequential one.
fn assert_parallel_matches_with_pseudos(session: &mut RenderSession) -> (usize, usize, Arc<Threads>) {
    session.display_list(800, 600, &Text).unwrap();
    let threads = Arc::new(Threads { threads: 4, runs: AtomicUsize::new(0), units: AtomicUsize::new(0) });
    let rules = session.rules.as_mut().unwrap();
    unsafe { rules.set_parallelism(Some(threads.clone())); }
    let mut fresh = css::StyleCache::default();
    rules.prestyle(&session.document, &mut fresh, &Text);
    unsafe { rules.set_parallelism(None); }
    let sequential = session.style_cache.borrow();
    let mut compared = 0;
    let compared_pseudos = core::cell::Cell::new(0usize);
    for node in elements(&session.document) {
        let (Some(parallel), Some(expected)) = (fresh.node_style(node), sequential.node_style(node)) else { continue };
        assert!(*parallel == *expected, "style of {node:?} differs");
        compared += 1;
        for pseudo in [css::PseudoElement::Before, css::PseudoElement::After, css::PseudoElement::Marker] {
            let (Some(parallel), Some(expected)) = (fresh.pseudo_style(node, pseudo), sequential.pseudo_style(node, pseudo)) else { continue };
            match (parallel, expected) {
                (None, None) => {}
                (Some(parallel), Some(expected)) => {
                    assert!(parallel.style == expected.style && parallel.content == expected.content, "{pseudo:?} of {node:?} differs");
                }
                _ => panic!("{pseudo:?} of {node:?} exists in only one of the caches"),
            }
            compared_pseudos.set(compared_pseudos.get() + usize::from(parallel.is_some()));
        }
    }
    (compared, compared_pseudos.get(), threads)
}

fn assert_parallel_matches(session: &mut RenderSession) -> (usize, Arc<Threads>) {
    let (compared, _, threads) = assert_parallel_matches_with_pseudos(session);
    (compared, threads)
}

#[test]
fn parallel_prestyle_equals_sequential_styles() {
    let mut session = session(40);
    let (compared, threads) = assert_parallel_matches(&mut session);
    assert!(threads.runs.load(Ordering::Relaxed) >= 1, "prestyle ran on the threads");
    assert!(threads.units.load(Ordering::Relaxed) >= 4, "the tree split into several units");
    assert!(compared > 400, "workers styled most elements, compared {compared}");
}

#[test]
fn parallel_prestyle_asks_host_callbacks_only_on_the_owning_thread() {
    let owner: ThreadId = std::thread::current().id();
    let calls = Rc::new(core::cell::Cell::new(0usize));
    let mut session = session(40);
    let document = session.document_mut();
    let open = crate::selector::query_selector(document, document.root(), "#w3").unwrap().unwrap();
    let checked = crate::selector::query_selector(document, document.root(), "#c5").unwrap().unwrap();
    let target = crate::selector::query_selector(document, document.root(), "#card7").unwrap().unwrap();
    let counter = calls.clone();
    document.set_custom_state_resolver(Rc::new(move |node, name| {
        assert_eq!(std::thread::current().id(), owner);
        counter.set(counter.get() + 1);
        node == open && name == "open"
    }));
    let counter = calls.clone();
    document.set_form_selector_state_resolver(Rc::new(move |_, node| {
        assert_eq!(std::thread::current().id(), owner);
        counter.set(counter.get() + 1);
        Some(crate::forms::FormSelectorState { checkedness: Some(node == checked), ..Default::default() })
    }));
    let counter = calls.clone();
    document.set_validity_resolver(
        Rc::new(move |_, _| {
            assert_eq!(std::thread::current().id(), owner);
            counter.set(counter.get() + 1);
            Some(crate::forms::ValidityState { value_missing: true, ..Default::default() })
        }),
        Rc::new(|| 1),
    );
    let counter = calls.clone();
    document.set_target_element_resolver(Some(Rc::new(move |_| {
        assert_eq!(std::thread::current().id(), owner);
        counter.set(counter.get() + 1);
        Some(target)
    })));
    document.set_mutation_sink(Some(Rc::new(move |_, _| assert_eq!(std::thread::current().id(), owner))));
    let (compared, threads) = assert_parallel_matches(&mut session);
    assert!(threads.runs.load(Ordering::Relaxed) >= 2, "units that needed host answers ran again");
    assert!(compared > 200, "answered units were kept, compared {compared}");
    assert!(calls.get() > 0);
    assert!(!session.document.host_gate.frozen());
    let open = crate::selector::query_selector(&session.document, session.document.root(), "#w3").unwrap().unwrap();
    let style = session.computed_style(open).unwrap();
    assert_eq!(style.display, css::Display::Block);
}

#[test]
fn a_question_the_host_cannot_answer_leaves_the_unit_to_layout() {
    let mut session = session(40);
    session.document_mut().set_mutation_sink(Some(Rc::new(|_, _| {})));
    session.display_list(800, 600, &Text).unwrap();
    let document = &session.document;
    document.freeze_host(None);
    assert!(document.mutation_sink.get(&document.host_gate).is_none());
    assert!(!document.host_gate.exact());
    assert!(!document.answer_host_questions(), "a sink call is no question");
    document.thaw_host();
    document.forget_host_answers();
    assert!(document.host_gate.exact());
}

#[test]
fn small_documents_and_warm_caches_stay_sequential() {
    let mut session = session(2);
    let (compared, threads) = assert_parallel_matches(&mut session);
    assert_eq!((compared, threads.runs.load(Ordering::Relaxed)), (0, 0));
    let mut session = self::session(40);
    session.display_list(800, 600, &Text).unwrap();
    let threads = Arc::new(Threads { threads: 4, runs: AtomicUsize::new(0), units: AtomicUsize::new(0) });
    let rules = session.rules.as_mut().unwrap();
    unsafe { rules.set_parallelism(Some(threads.clone())); }
    rules.prestyle(&session.document, &mut session.style_cache.borrow_mut(), &Text);
    assert_eq!(threads.runs.load(Ordering::Relaxed), 0, "a warm cache is not restyled");
}


#[test]
fn a_miss_belongs_to_the_worker_that_asked() {
    let session = session(2);
    let document = &session.document;
    let threads = Arc::new(Threads { threads: 4, runs: AtomicUsize::new(0), units: AtomicUsize::new(0) });
    document.freeze_host(Some(threads));
    let gate = &document.host_gate;
    assert_eq!(gate.current_worker(), 0);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            WORKER.with(|worker| worker.set(2));
            assert_eq!(gate.current_worker(), 2);
            assert!(gate.exact());
            gate.target();
            assert!(!gate.exact(), "the asking worker is no longer exact");
        }).join().unwrap();
    });
    assert_eq!((gate.worker_misses(2), gate.worker_misses(0), gate.worker_misses(1)), (1, 0, 0));
    assert!(gate.exact(), "another worker's miss leaves this worker exact");
    gate.target();
    assert_eq!(gate.worker_misses(0), 1);
    assert!(!gate.exact());
    document.thaw_host();
    document.forget_host_answers();
    assert!(gate.exact());
}

fn nested_fixture(depth: usize, siblings: usize, leaves: usize) -> String {
    let mut source = String::from("<!doctype html>");
    source.push_str(STYLES);
    source.push_str("<body>");
    for _ in 0..depth { source.push_str("<div class=card><p>x</p><ul><li>y</li></ul>"); }
    for _ in 0..siblings {
        source.push_str("<section>");
        for _ in 0..leaves { source.push_str("<p><em>b</em></p>"); }
        source.push_str("</section>");
    }
    for _ in 0..depth { source.push_str("</div>"); }
    source
}

#[test]
fn a_deep_heavy_chain_splits_into_the_sequential_styles() {
    on_large_stack(|| {
        let mut session = session_from(&nested_fixture(120, 0, 0));
        let (compared, threads) = assert_parallel_matches(&mut session);
        assert!(compared > 400, "workers styled the chain, compared {compared}");
        assert!(threads.runs.load(Ordering::Relaxed) >= 1);
    });
}

/// Unoptimized layout frames are large; deep chains need more than the test thread's stack.
fn on_large_stack(test: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new().stack_size(64 << 20).spawn(test).unwrap().join().unwrap();
}

#[test]
fn many_heavy_siblings_split_into_the_sequential_styles() {
    let mut session = session_from(&nested_fixture(0, 24, 20));
    let (compared, threads) = assert_parallel_matches(&mut session);
    assert!(compared > 800, "workers styled the siblings, compared {compared}");
    assert!(threads.units.load(Ordering::Relaxed) >= 4);
}

#[test]
fn heavy_chains_with_heavy_siblings_at_every_level_match_sequential_styles() {
    let mut source = String::from("<!doctype html>");
    source.push_str(STYLES);
    source.push_str("<body>");
    for _ in 0..30 {
        source.push_str("<div class=card>");
        for _ in 0..3 { source.push_str("<section><p><em>b</em></p><p>c</p><ul><li>d</li><li>e</li></ul></section>"); }
    }
    for _ in 0..30 { source.push_str("</div>"); }
    on_large_stack(move || {
        let mut session = session_from(&source);
        let (compared, _) = assert_parallel_matches(&mut session);
        assert!(compared > 600, "compared {compared}");
    });
}

#[test]
fn pseudo_element_styles_computed_by_workers_equal_the_sequential_ones() {
    let mut session = session(40);
    let (compared, pseudos, _) = assert_parallel_matches_with_pseudos(&mut session);
    assert!(compared > 400, "compared {compared}");
    assert!(pseudos >= 40, "generated pseudo-element styles were prestyled, compared {pseudos}");
}

fn custom_fixture(groups: usize) -> String {
    let mut source = String::from(
        "<!doctype html><style>:root { --a: 1px } .x { --c: var(--a); --d: 2px; padding: var(--c) } .y { --e: 3px } .s2 { --a: 9px }</style><body>");
    for _ in 0..groups {
        source.push_str("<section><div class=x><i>a</i></div><div class=x><i>a</i></div><div class='x y'><i>b</i></div><div class=x></div></section>\
                         <section class=s2><div class=x><i>c</i></div></section>");
    }
    source
}

#[test]
fn siblings_with_identical_custom_property_rules_share_values_and_others_differ() {
    let mut session = session_from(&custom_fixture(40));
    let (compared, _) = assert_parallel_matches(&mut session);
    assert!(compared > 300, "compared {compared}");
    let root = session.document.root();
    let value = |session: &mut RenderSession, selector: &str, name: &str| {
        let node = crate::selector::query_selector(&session.document, root, selector).unwrap().unwrap();
        let style = session.computed_style(node).unwrap();
        style.custom_properties().get(name).and_then(Option::as_ref).map(|value| String::from(value.as_str()))
    };
    let one = |text: &str| Some(String::from(text));
    assert_eq!(value(&mut session, "section:not(.s2) > .x", "--c"), one("1px"));
    assert_eq!(value(&mut session, "section:not(.s2) > .x + .x", "--c"), one("1px"));
    assert_eq!(value(&mut session, "section:not(.s2) > .x + .x", "--d"), one("2px"));
    assert_eq!(value(&mut session, "section:not(.s2) > .x", "--e"), None);
    assert_eq!(value(&mut session, "section:not(.s2) > .y", "--e"), one("3px"));
    assert_eq!(value(&mut session, "section:not(.s2) > .y", "--c"), one("1px"));
    assert_eq!(value(&mut session, ".s2 > .x", "--c"), one("9px"));
    assert_eq!(value(&mut session, ".s2 > .x", "--e"), None);
}
