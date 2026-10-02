use std::rc::Rc;
#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    pub line: u32,
    /// The parse failed because the input ended too soon (error at the EOF token, or the lexer ran
    /// out mid-construct). A REPL uses this to keep reading lines instead of reporting the error.
    pub at_eof: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsxRuntime { Automatic, Development, Classic, Preserve }

#[derive(Debug, Clone)]
pub struct JsxOptions {
    pub runtime: JsxRuntime,
    pub import_source: String,
    pub factory: String,
    pub fragment_factory: String,
    pub filename: String,
}

impl Default for JsxOptions {
    fn default() -> Self {
        Self { runtime: JsxRuntime::Automatic, import_source: "react".into(), factory: "React.createElement".into(), fragment_factory: "React.Fragment".into(), filename: String::new() }
    }
}

#[derive(Default)]
pub(crate) struct TemplateSites {
    state: std::cell::RefCell<(
        crate::fasthash::FastMap<(usize, u32, u64), (std::rc::Weak<str>, u32)>,
        u32,
    )>,
}
// The driver and its coroutine access this registry exclusively under the existing handoff;
// like GcState, the Rc/Weak handles must never be touched by both threads concurrently.
unsafe impl Send for TemplateSites {}
unsafe impl Sync for TemplateSites {}
thread_local! {
    static TEMPLATE_SITES: std::cell::RefCell<std::sync::Arc<TemplateSites>> =
        std::cell::RefCell::new(std::sync::Arc::new(TemplateSites::default()));
}
pub(crate) fn template_sites_handle() -> std::sync::Arc<TemplateSites> {
    TEMPLATE_SITES.with(|current| current.borrow().clone())
}
pub(crate) fn enter_template_sites(
    state: std::sync::Arc<TemplateSites>,
) -> std::sync::Arc<TemplateSites> {
    TEMPLATE_SITES.with(|current| std::mem::replace(&mut *current.borrow_mut(), state))
}

pub(crate) fn template_site(src: &Rc<str>, offset: u32, quasis: &[(Option<String>, String)]) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (_, raw) in quasis {
        raw.hash(&mut h);
    }
    let key = (Rc::as_ptr(src) as *const u8 as usize, offset, h.finish());
    TEMPLATE_SITES.with(|t| {
        let registry = t.borrow();
        let (map, next) = &mut *registry.state.borrow_mut();
        match map.get(&key) {
            Some((live, id)) if live.strong_count() != 0 => *id,
            _ => {
                *next += 1;
                map.insert(key, (Rc::downgrade(src), *next));
                *next
            }
        }
    })
}

/// A template-site id no parsed site shares (snapshot-decoded templates, whose bodies are never
/// released).
pub(crate) fn fresh_template_site() -> u32 {
    TEMPLATE_SITES.with(|t| {
        let registry = t.borrow();
        let (_, next) = &mut *registry.state.borrow_mut();
        *next += 1;
        *next
    })
}

