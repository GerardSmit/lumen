//! Captured stylesheet-link requests and generation-qualified completion.
//! The provider owns bytes/I/O; this module owns the HTML element algorithm.
use super::*;
use lumen_html::observe::{ObservedKind, ObservedMutation};
use std::sync::Arc;

const LINK_REQUEST_LIMIT: usize = 4096;

/// Plain captured request data; no engine, Document or JS values cross I/O.
#[derive(Clone)]
pub struct StylesheetRequest {
    /// Already decoded inline root; only its critical import edges use transport.
    pub inline_source: Option<Arc<str>>,
    pub import_request:bool,
    pub url: String,
    pub document_url: String,
    pub referrer: String,
    pub origin: String,
    pub nonce: String,
    pub parser_inserted:bool,
    pub integrity: String,
    pub crossorigin: Option<bool>,
    pub referrer_policy: lumen_common::referrer::ReferrerPolicy,
    pub environment_encoding: &'static str,
    pub quirks_mode: bool,
    pub policies: Arc<lumen_common::csp::PolicySet>,
    pub start_time:f64,
}

/// Plain completion timing for each actual graph edge, including failed MIME
/// and HTTP status responses. No fabricated DNS/TLS/transfer phases are stored.
pub struct StylesheetResourceTiming {
    pub initiator_type: &'static str,
    pub name:String,
    pub start_time:f64,
    pub end_time:f64,
    pub encoded_body_size:u64,
    pub decoded_body_size:u64,
    pub timing_allowed:bool,
}
pub struct StylesheetResponse {
    pub critical_failed: bool,
    pub failed_import_paths: Vec<Vec<usize>>,
    pub source_contexts:Vec<lumen_html::stylesheet_loading::SourceContext>,

    pub location_url:String,
    pub content_type:Option<String>,
    pub source: lumen_html::css::StylesheetSource,
    pub origin_clean: bool,
    pub start_time: f64,
    pub end_time: f64,
    pub encoded_body_size: u64,
    pub timing_allow:bool,
    pub origin_metadata:Arc<[(Arc<str>,bool)]>,
    pub violations: Vec<lumen_common::csp::Violation>,
    pub timings:Vec<StylesheetResourceTiming>,
}
pub struct StylesheetFailure {
    pub message:String,
    pub violations:Vec<lumen_common::csp::Violation>,
    pub timings:Vec<StylesheetResourceTiming>,
}

/// The same provider contract is used by the native browser and fixture transport.
/// Cancellation invalidates the ticket, including any later I/O completion.
pub trait StylesheetResourceLoader {
    fn start(&self, request: StylesheetRequest) -> Result<u64, String>;
    fn poll(&self, ticket: u64) -> Option<Result<StylesheetResponse, StylesheetFailure>>;
    fn cancel(&self, ticket: u64);
}

/// Installed by the browser host before DOM creation, including child realms.
/// Only weak document registrations are retained by the host task pump.
pub struct StylesheetEnvironment {
    factory:Rc<dyn Fn(&mut Ctx)->Rc<dyn StylesheetResourceLoader>>,
    pub documents:Rc<RefCell<Vec<std::rc::Weak<DomRealm>>>>,
}
impl StylesheetEnvironment {
    pub fn new(factory:impl Fn(&mut Ctx)->Rc<dyn StylesheetResourceLoader>+'static,
        documents:Rc<RefCell<Vec<std::rc::Weak<DomRealm>>>>)->Self {
        Self{factory:Rc::new(factory),documents}
    }
}
pub(crate) fn register_document(ctx:&mut Ctx,realm:&Rc<DomRealm>) {
    let weak=Rc::downgrade(realm);
    realm.session.borrow_mut().document_mut().set_parser_style_block_sink(Some(Rc::new(move |document,node| {
        if let Some(realm)=weak.upgrade() {
            if lumen_html::xml_stylesheet::is_candidate(document,node) {realm.stylesheet_links.record_parser_birth(document,node);}
            else {realm.capture_inline_stylesheet(document,node,true)?;}
        }Ok(())
    })));
    let weak=Rc::downgrade(realm);
    realm.session.borrow_mut().document_mut().set_inline_stylesheet_policy(Some(Rc::new(move |document,node,text| {
        let Some(realm)=weak.upgrade() else{return false};
        if let Some(entry)=realm.stylesheet_links.entries.borrow().get(&node) {
            if entry.parser_open {return false}
            if let Some(inline)=&entry.inline {return inline.source.is_some() && !entry.sheet_disabled.unwrap_or(false)}
        }
        let kind=document.get_attribute_ns_ref(node,None,"type").ok().flatten().unwrap_or("");
        script_loading::is_connected(document,node) && lumen_common::mime::inline_stylesheet_type_supported(kind)
            && !realm.csp_overflow.get() && realm.csp.borrow().check_inline(text,Some(document.cryptographic_nonce(node).unwrap_or("")),
                lumen_common::csp::InlineCheckType::Style).is_ok_and(|decision|!decision.blocked)
    })));

    if realm.stylesheet_links.created_at.get().is_none() {realm.stylesheet_links.created_at.set(Some(lumen_host::perf::web_now_ms()));}
    let Some((factory,documents))=ctx.op_state().get::<StylesheetEnvironment>()
        .map(|environment|(environment.factory.clone(),environment.documents.clone())) else {return};
    if !realm.has_stylesheet_resource_loader() {realm.set_stylesheet_resource_loader(factory(ctx));}
    let mut documents=documents.borrow_mut();
    documents.retain(|entry|entry.strong_count()!=0);
    if !documents.iter().any(|entry|entry.upgrade().is_some_and(|entry|Rc::ptr_eq(&entry,realm))) {
        documents.push(Rc::downgrade(realm));
    }
}

#[derive(Clone, Eq, PartialEq)]
struct Selection {
    href: String,
    crossorigin: Option<bool>,
    kind: String,
    alternate: bool,
    disabled: bool,
    connected: bool,
}
struct Pending {
    ticket: Option<u64>,
    generation: u64,
    _root: ResourceRequestRoot,
    _blocking: Option<focus::ScriptBlockingStylesheet>,
    _render:Option<RenderLease>,
    _load: LoadLease,
    imports:Vec<Rc<lumen_html::session::StylesheetImportLease>>,
}
struct ImportPending {ticket:u64,_root:ResourceRequestRoot,_load:LoadLease}
struct ImportWork {
    owner:NodeId,
    lease:Rc<lumen_html::session::StylesheetImportLease>,
    covered_by_root:bool,
    completed:bool,
    failed:bool,
    pending:Option<ImportPending>,
    source_context:Option<(&'static str,lumen_common::referrer::ReferrerPolicy)>,
}

struct RenderLease {owner:std::rc::Weak<DomRealm>, active:Rc<Cell<bool>>}
impl Drop for RenderLease {
    fn drop(&mut self) {
        self.active.set(false);
        if let Some(owner)=self.owner.upgrade() {
            owner.stylesheet_links.render_count.set(owner.stylesheet_links.render_count.get()-1);
        }
    }
}
struct LoadLease { owner: std::rc::Weak<DomRealm> }
impl Drop for LoadLease {
    fn drop(&mut self) {
        if let Some(owner)=self.owner.upgrade() {
            owner.stylesheet_links.load_count.set(owner.stylesheet_links.load_count.get()-1);
        }
    }
}
#[derive(Default)]
struct Entry {
    inline: Option<InlineBlock>,
    parser_open: bool,
    change_token: Option<Rc<Cell<u64>>>,
    selection: Option<Selection>,
    generation: u64,
    explicitly_enabled: bool,
    pending: Option<Pending>,
    completed_generation: Option<u64>,
    source_context:Option<(&'static str,lumen_common::referrer::ReferrerPolicy)>,
    ready:Option<Box<(Pending,Result<StylesheetResponse,StylesheetFailure>)>>,
    parser_created: bool,
    script_blocking_eligible:Option<bool>,
    origin_metadata:Arc<[(Arc<str>,bool)]>,
    sheet_disabled:Option<bool>,
    flag_lease:Option<Rc<lumen_html::session::StylesheetGraphLease>>,
    sheet_added:bool,
    render_pending:std::rc::Weak<Cell<bool>>,
    obtained_type:Option<String>,
    refetch:bool,
    location:Option<Arc<str>>,
}
fn collect_source_urls(source:&lumen_html::css::StylesheetSource,out:&mut HashSet<Arc<str>>)->OpResult<()> {
    if out.len()>=lumen_html::stylesheet_loading::MAX_GRAPH_SOURCES {return Err(OpError::new("QuotaExceededError","CSS graph origin provenance"))}
    out.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","CSS graph origin allocation"))?;out.insert(source.url.clone());
    for import in &source.imports {if let Some(source)=import.source.as_deref() {collect_source_urls(source,out)?;}}
    Ok(())
}

fn collect_missing_imports(source:&lumen_html::css::StylesheetSource,path:&mut Vec<usize>,active:&mut Vec<Arc<str>>,
    out:&mut Vec<(Vec<usize>,Option<String>,String,bool)>,resource:bool)->OpResult<()> {
    if path.len()>=lumen_html::stylesheet_loading::MAX_GRAPH_DEPTH {return Err(OpError::new("QuotaExceededError","CSS import depth"))}
    active.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","CSS import ancestry"))?;if resource {active.push(source.url.clone());}
    for (ordinal,import) in source.imports.iter().enumerate() {
        path.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","CSS import path"))?;path.push(ordinal);
        if let Some(child)=import.source.as_deref() {collect_missing_imports(child,path,active,out,true)?;}
        else {
            if out.len()>=lumen_html::css::MAX_CSS_GRAPH_IMPORTS {return Err(OpError::new("QuotaExceededError","CSS import occurrences"))}
            out.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","CSS import occurrence list"))?;
            let supported=import.rule.supports.as_deref().is_none_or(lumen_html::css::supports_condition);
            let resolved=lumen_html::css::resolve_import_url(&import.rule,&source.url);
            let invalid=supported && resolved.is_none();
            let url=resolved.filter(|url|supported && !active.iter().any(|ancestor|ancestor.split('#').next()==url.split('#').next()));
            let mut captured=Vec::new();captured.try_reserve_exact(path.len()).map_err(|_|OpError::new("QuotaExceededError","CSS import path capture"))?;captured.extend_from_slice(path);
            out.push((captured,url.map(|url| url.to_string()),source.url.to_string(),invalid));
        }
        path.pop();
    }
    if resource {active.pop();}Ok(())
}

impl Entry {
    fn inline_bytes(&self)->usize {self.inline.as_ref().and_then(|inline|inline.source.as_ref()).map_or(0,|source|source.len())}
}
struct InlineBlock {
    source: Option<Arc<str>>,
    policy_blocked: bool,
    base: String,
    violations: Vec<lumen_common::csp::Violation>,
    installed: bool,
}
#[derive(Clone, Copy)]
pub(crate) struct PendingParserScript {pub node:NodeId,pub generation:u64,pub document_write:bool,pub insertion_parent:Option<NodeId>}
#[derive(Default)]
pub(crate) struct StylesheetLinks {
    provider: RefCell<Option<Rc<dyn StylesheetResourceLoader>>>,
    entries: RefCell<HashMap<NodeId, Entry>>,
    dirty: RefCell<HashSet<NodeId>>,
    import_dirty:RefCell<HashSet<NodeId>>,
    imports:RefCell<HashMap<usize,ImportWork>>,
    scan: Cell<bool>,
    processing_failed:Cell<bool>,
    inline_publication_pending:Cell<bool>,
    generation: Cell<u64>,
    preferred_title: RefCell<String>,
    last_title: RefCell<Option<String>>,
    flags_dirty:Cell<bool>,
    load_count: Cell<usize>,
    inline_bytes:Cell<usize>,
    render_count:Cell<usize>,
    created_at:Cell<Option<f64>>,
    deferred_complete: Cell<bool>,
    deferred_load: Cell<bool>,
    pub(crate) parser_script:Cell<Option<PendingParserScript>>,
}

impl StylesheetLinks {
    pub(crate) fn prepare_cssom_import_update(&self,owner:NodeId)->OpResult<()> {
        let mut dirty=self.import_dirty.borrow_mut();
        if !dirty.contains(&owner) {
            if dirty.len()>=LINK_REQUEST_LIMIT {return Err(OpError::new("QuotaExceededError","CSSOM import owner admission"))}
            dirty.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","CSSOM import owner allocation"))?;
        }
        Ok(())
    }
    pub(crate) fn cssom_imports_changed(&self,owner:NodeId) {self.import_dirty.borrow_mut().insert(owner);}
    fn admit_owner_update(&self,node:NodeId)->Result<(),lumen_html::Error> {
        let mut entries=self.entries.borrow_mut();
        if !entries.contains_key(&node) {
            if entries.len()>=LINK_REQUEST_LIMIT {return Err(lumen_html::Error::LimitExceeded)}
            entries.try_reserve(1).map_err(|_|lumen_html::Error::LimitExceeded)?;
        }
        let mut dirty=self.dirty.borrow_mut();
        if !dirty.contains(&node) {
            if dirty.len()>=LINK_REQUEST_LIMIT {return Err(lumen_html::Error::LimitExceeded)}
            dirty.try_reserve(1).map_err(|_|lumen_html::Error::LimitExceeded)?;
        }
        Ok(())
    }

    pub(crate) fn location(&self,node:NodeId)->Option<String> {
        self.entries.borrow().get(&node)?.location.as_deref().map(str::to_owned)
    }
    pub(crate) fn origin_metadata(&self,node:NodeId)->Arc<[(Arc<str>,bool)]> {
        self.entries.borrow().get(&node).map_or_else(||Arc::from([]),|entry|entry.origin_metadata.clone())
    }
    pub(crate) fn explicitly_enable(&self, node: NodeId)->OpResult<()> {
        self.admit_owner_update(node).map_err(dom_error)?;
        let mut entries=self.entries.borrow_mut();let entry=entries.entry(node).or_default();
        entry.explicitly_enabled = true;entry.sheet_disabled=Some(false);if let Some(lease)=entry.flag_lease.as_ref().filter(|lease|!lease.is_detached()) {lease.set_disabled(false);}
        self.dirty.borrow_mut().insert(node);
        Ok(())
    }
    fn enable_set(&self,document:&lumen_html::Document,name:&str)->Result<(),lumen_html::Error> {
        let mut entries=self.entries.borrow_mut();
        let mut plan=Vec::new();plan.try_reserve_exact(entries.len()).map_err(|_|lumen_html::Error::LimitExceeded)?;
        for (&node,entry) in entries.iter() {
            if !entry.sheet_added && !entry.inline.as_ref().is_some_and(|inline|inline.source.is_some()) {continue;}
            let instruction=lumen_html::xml_stylesheet::descriptor(document,node)?;
            let title=instruction.as_ref().map_or_else(||inline_stylesheet_title(document,node).unwrap_or(""),|instruction|instruction.title());
            if !title.is_empty() && entry.sheet_disabled!=Some(title!=name) {plan.push((node,title!=name));}
        }
        let changed=!plan.is_empty();
        let mut dirty=self.dirty.borrow_mut();dirty.try_reserve(plan.len()).map_err(|_|lumen_html::Error::LimitExceeded)?;
        for (node,disabled) in plan {let entry=entries.get_mut(&node).expect("admitted sheet");entry.sheet_disabled=Some(disabled);if let Some(lease)=entry.flag_lease.as_ref().filter(|lease|!lease.is_detached()) {lease.set_disabled(disabled);}dirty.insert(node);}
        if changed {self.flags_dirty.set(true);}Ok(())
    }
    pub(crate) fn process_default_style_meta(&self,document:&lumen_html::Document,node:NodeId) {
        if document.root_node(node,false).ok()!=Some(document.root())
            || lumen_html::forms::html_element_local_name(document,node)!=Some("meta")
            || !document.get_attribute_ns_ref(node,None,"http-equiv").ok().flatten().is_some_and(|value|value.eq_ignore_ascii_case("default-style")) {return;}
        let Some(name)=document.get_attribute_ns_ref(node,None,"content").ok().flatten().filter(|value|!value.is_empty()) else {return;};
        if self.change_preferred(document,name).is_err() {self.processing_failed.set(true);}
    }
    fn change_preferred(&self,document:&lumen_html::Document,name:&str)->Result<(),lumen_html::Error> {
        if name.len()>lumen_html::html::MAX_HTML_BYTES {return Err(lumen_html::Error::LimitExceeded);}
        let changed=self.preferred_title.borrow().as_str()!=name;
        if !changed {return Ok(());}
        let mut stored=String::new();stored.try_reserve_exact(name.len()).map_err(|_|lumen_html::Error::LimitExceeded)?;stored.push_str(name);
        if changed && self.last_title.borrow().is_none() {self.enable_set(document,name)?;}
        *self.preferred_title.borrow_mut()=stored;self.flags_dirty.set(true);Ok(())
    }
    pub(crate) fn set_sheet_disabled(&self,node:NodeId,value:bool)->OpResult<()> {
        self.admit_owner_update(node).map_err(dom_error)?;
        let mut entries=self.entries.borrow_mut();let entry=entries.entry(node).or_default();entry.sheet_disabled=Some(value);if let Some(lease)=entry.flag_lease.as_ref().filter(|lease|!lease.is_detached()) {lease.set_disabled(value);}
        Ok(())
    }
    pub(crate) fn record_parser_birth(&self, document: &lumen_html::Document, node: NodeId) {
        if (is_link(document,node)||is_style(document,node)||lumen_html::xml_stylesheet::is_candidate(document,node)) && self.admit_owner_update(node).is_err() {
            self.processing_failed.set(true);return;
        }
        if is_link(document, node) || lumen_html::xml_stylesheet::is_candidate(document,node) {
            self.entries.borrow_mut().entry(node).or_default().parser_created = true;
            self.dirty.borrow_mut().insert(node);
        }
        if is_style(document,node) {
            let mut entries=self.entries.borrow_mut();
            let entry=entries.entry(node).or_default();entry.parser_created=true;entry.parser_open=true;
        }
    }
    pub(crate) fn install(&self, provider: Rc<dyn StylesheetResourceLoader>) {
        let mut old=self.provider.borrow_mut();
        if old.as_ref().is_some_and(|current|Rc::ptr_eq(current,&provider)) { return; }
        if let Some(old)=old.as_ref() {
            for entry in self.entries.borrow_mut().values_mut() {
                if let Some(pending)=entry.pending.take() { if let Some(ticket)=pending.ticket {old.cancel(ticket);} }
                entry.selection=None;
            }
        }
        *old=Some(provider);
        self.scan.set(true);
    }
    pub(crate) fn retire(&self) {
        if let Some(provider)=self.provider.borrow().as_ref() {
            for entry in self.entries.borrow_mut().values_mut() {
                if let Some(pending)=entry.pending.take() { if let Some(ticket)=pending.ticket {provider.cancel(ticket);} }
            }
        }
        if let Some(provider)=self.provider.borrow().as_ref() {
            for work in self.imports.borrow_mut().values_mut() {if let Some(pending)=work.pending.take() {provider.cancel(pending.ticket);}}
        }
        self.imports.borrow_mut().clear();self.import_dirty.borrow_mut().clear();
        self.entries.borrow_mut().clear();
        self.inline_bytes.set(0);
        self.dirty.borrow_mut().clear();
        self.scan.set(false);
        self.preferred_title.borrow_mut().clear();
        self.last_title.borrow_mut().take();
        self.flags_dirty.set(false);
        self.deferred_complete.set(false);
        self.deferred_load.set(false);
        self.parser_script.set(None);self.processing_failed.set(false);
    }
    pub(crate) fn reclaim_nodes(&self,document:&lumen_html::Document) {
        let provider=self.provider.borrow();
        self.entries.borrow_mut().retain(|node,entry|{
            if document.kind(*node).is_ok() {return true}
            self.inline_bytes.set(self.inline_bytes.get()-entry.inline_bytes());
            if let Some(pending)=entry.pending.take() {
                if let Some(provider)=provider.as_ref() {if let Some(ticket)=pending.ticket {provider.cancel(ticket);}}
            }
            false
        });
        self.dirty.borrow_mut().retain(|node|document.kind(*node).is_ok());
    }
    pub(crate) fn pending(&self)->bool { self.load_count.get()!=0 }
    pub(crate) fn defer_complete(&self)->bool {
        if !self.pending() {return false}
        self.deferred_complete.set(true);true
    }
    pub(crate) fn defer_window_load(&self)->bool {
        if !self.pending() {return false}
        self.deferred_load.set(true);true
    }
    pub(crate) fn current(&self,node:NodeId,generation:u64)->bool {
        self.entries.borrow().get(&node).is_some_and(|entry|entry.generation==generation && entry.completed_generation==Some(generation))
    }
    pub(crate) fn mutation(&self,document:&lumen_html::Document,mutation:&ObservedMutation) {
        if matches!(&mutation.kind,ObservedKind::Attribute{name,namespace_uri,..} if namespace_uri.is_none()
            && ((is_link(document,mutation.target) && matches!(name.as_str(),"href"|"rel"|"type"|"disabled"|"crossorigin"|"media"|"title"|"blocking"))
                || (is_style(document,mutation.target) && matches!(name.as_str(),"media"|"title"|"blocking"))))
            && self.admit_owner_update(mutation.target).is_err() {self.processing_failed.set(true);return;}
        match &mutation.kind {
            ObservedKind::CharacterData{..} if lumen_html::xml_stylesheet::is_candidate(document,mutation.target)=>{
                if self.admit_owner_update(mutation.target).is_err() {self.processing_failed.set(true);return;}
                self.dirty.borrow_mut().insert(mutation.target);
                if let Some(entry)=self.entries.borrow_mut().get_mut(&mutation.target) {
                    entry.refetch=true;entry.completed_generation=None;
                    if let Some(token)=&entry.change_token {token.set(token.get().wrapping_add(1));}
                    if let Some(pending)=entry.pending.take() {if let (Some(provider),Some(ticket))=(self.provider.borrow().as_ref(),pending.ticket) {provider.cancel(ticket);}drop(pending);}
                }
            },
            ObservedKind::Attribute{name,namespace_uri,..} if namespace_uri.is_none() && is_link(document,mutation.target)=>{
                if name=="blocking" {
                    let explicit=document.get_attribute_ns_ref(mutation.target,None,"blocking").ok().flatten()
                        .is_some_and(|tokens|tokens.split_ascii_whitespace().any(|token|token.eq_ignore_ascii_case("render")));
                    if !explicit {if let Some(entry)=self.entries.borrow_mut().get_mut(&mutation.target) {
                        if !entry.parser_created {if let Some(pending)=entry.pending.as_mut() {pending._render.take();}}
                    }}
                }
                if name=="disabled" && document.get_attribute_ns_ref(mutation.target,None,"disabled").ok().flatten().is_none() {
                    // The bit survives subsequent disabling and affects alternate selection.
                    self.entries.borrow_mut().entry(mutation.target).or_default().explicitly_enabled=true;
                }
                if matches!(name.as_str(),"href"|"rel"|"type"|"disabled"|"crossorigin"|"media"|"title") {
                    self.dirty.borrow_mut().insert(mutation.target);
                }
                // A matching hint does not obtain an already obtained resource
                // again. Removal also leaves that response authoritative.
                let matching_obtained_type=if name=="type" {
                    let raw=document.get_attribute_ns_ref(mutation.target,None,"type").ok().flatten();
                    let mut entries=self.entries.borrow_mut();
                    entries.get_mut(&mutation.target).is_some_and(|entry| {
                        let matches=entry.obtained_type.as_deref().is_some_and(|obtained|
                            raw.is_none_or(|raw|lumen_common::mime::mime_essence(raw).is_some_and(|hint|hint.eq_ignore_ascii_case(obtained))));
                        if matches {if let Some(selection)=entry.selection.as_mut() {selection.kind=raw.unwrap_or("").into();}}
                        matches
                    })
                }else{false};
                if !matching_obtained_type && matches!(name.as_str(),"href"|"rel"|"type"|"disabled"|"crossorigin") {
                    let mut entries=self.entries.borrow_mut();
                    let entry=entries.entry(mutation.target).or_default();
                    entry.completed_generation=None;entry.ready=None;
                    entry.refetch=true;
                    if name=="disabled" {let disabled=document.get_attribute_ns_ref(mutation.target,None,"disabled").ok().flatten().is_some();entry.sheet_disabled=Some(disabled);if let Some(lease)=entry.flag_lease.as_ref().filter(|lease|!lease.is_detached()) {lease.set_disabled(disabled);}}
                }
                if name=="title" {self.scan.set(true);}
            }
            ObservedKind::Attribute{name,namespace_uri,..} if namespace_uri.is_none() && is_style(document,mutation.target)
                && matches!(name.as_str(),"media"|"title"|"blocking")=>{self.dirty.borrow_mut().insert(mutation.target);},
            ObservedKind::Attribute{name,namespace_uri,..} if namespace_uri.is_none() && name=="title" =>self.scan.set(true),

            ObservedKind::ChildList{..}|ObservedKind::ChildListMany{..}|ObservedKind::ChildListReplacement{..}=>{
                self.scan.set(true);
                for (&node,entry) in self.entries.borrow_mut().iter_mut() {
                    if lumen_html::xml_stylesheet::is_candidate(document,node) && entry.selection.as_ref().is_some_and(|selected|selected.connected!=lumen_html::xml_stylesheet::in_prolog(document,node).unwrap_or(false)) {
                        entry.refetch=true;entry.completed_generation=None;
                        if let Some(token)=&entry.change_token {token.set(token.get().wrapping_add(1));}
                        if let Some(pending)=entry.pending.take() {if let (Some(provider),Some(ticket))=(self.provider.borrow().as_ref(),pending.ticket) {provider.cancel(ticket);}drop(pending);}
                    }
                    if !script_loading::is_connected(document,node) {if let Some(pending)=entry.pending.as_mut() {pending._render.take();}}
                }
            },
            _=>{}
        }
    }
    fn next_generation(&self)->OpResult<u64> {
        let generation=self.generation.get().checked_add(1)
            .ok_or_else(||OpError::new("QuotaExceededError","stylesheet generation exhausted"))?;
        self.generation.set(generation);
        Ok(generation)
    }
    pub(crate) fn adopt_nodes(&self,target:&StylesheetLinks,mapping:&[(NodeId,NodeId)])->OpResult<()> {
        let count=mapping.iter().filter(|(old,_)|self.entries.borrow().contains_key(old)).count();
        if !core::ptr::eq(self,target) && target.entries.borrow().len().checked_add(count).is_none_or(|count|count>LINK_REQUEST_LIMIT) {
            return Err(OpError::new("QuotaExceededError","stylesheet owner adoption limit"))
        }
        if !core::ptr::eq(self,target) && target.dirty.borrow().len().checked_add(count).is_none_or(|count|count>LINK_REQUEST_LIMIT) {
            return Err(OpError::new("QuotaExceededError","stylesheet dirty adoption limit"))
        }
        target.dirty.borrow_mut().try_reserve(count).map_err(|_|OpError::new("QuotaExceededError","stylesheet dirty adoption admission"))?;
        target.entries.borrow_mut().try_reserve(count)
            .map_err(|_|OpError::new("QuotaExceededError","stylesheet metadata adoption"))?;
        for &(old,new) in mapping {
            let Some(mut entry)=self.entries.borrow_mut().remove(&old) else {continue};
            self.inline_bytes.set(self.inline_bytes.get()-entry.inline_bytes());
            if let Some(pending)=entry.pending.take() {
                if let Some(provider)=self.provider.borrow().as_ref() {if let Some(ticket)=pending.ticket {provider.cancel(ticket);}}
            }
            entry.selection=None;
            entry.completed_generation=None;entry.ready=None;
            entry.parser_created=false;entry.script_blocking_eligible=None;
            entry.inline=None;entry.parser_open=false;entry.change_token=None;
            entry.sheet_disabled=None;
            entry.origin_metadata=Arc::from([]);
            entry.location=None;
            target.entries.borrow_mut().insert(new,entry);
            target.dirty.borrow_mut().insert(new);
        }
        Ok(())
    }
}

pub(crate) fn is_style(document:&lumen_html::Document,node:NodeId)->bool {
    matches!(document.kind(node),Ok(NodeKind::Element{name,namespace,..})
        if matches!(namespace,Namespace::Html|Namespace::Svg) && lumen_html::svg::local_name(name)=="style")
}
pub(crate) fn inline_stylesheet_title(document:&lumen_html::Document,node:NodeId)->Option<&str> {
    if document.root_node(node,false).ok().is_some_and(|root|matches!(document.kind(root),Ok(NodeKind::Document))) {
        document.get_attribute_ns_ref(node,None,"title").ok().flatten()
    }else {Some("")}
}
impl DomRealm {
    /// Pure parser/mutation phase: capture the actual style-block creation
    /// decision before a later script runs, without borrowing its Session.
    pub(crate) fn capture_inline_stylesheet(&self,document:&lumen_html::Document,node:NodeId,popped:bool)->Result<(),lumen_html::Error> {
        if !is_style(document,node) {return Ok(())}
        self.stylesheet_links.admit_owner_update(node)?;
        let mut entries=self.stylesheet_links.entries.borrow_mut();
        if !entries.contains_key(&node) {entries.try_reserve(1).map_err(|_|lumen_html::Error::LimitExceeded)?;}
        let entry=entries.entry(node).or_default();
        if popped {entry.parser_open=false;}
        if entry.parser_open {return Ok(())}
        let generation=self.stylesheet_links.generation.get().checked_add(1).ok_or(lumen_html::Error::LimitExceeded)?;
        let mut violations=Vec::new();
        let mut policy_blocked=false;
        let kind=document.get_attribute_ns_ref(node,None,"type").ok().flatten().unwrap_or("");
        let connected=script_loading::is_connected(document,node);
        let source=if connected && lumen_common::mime::inline_stylesheet_type_supported(kind) {
            if self.csp_overflow.get() {return Err(lumen_html::Error::LimitExceeded)}
            let previous=self.stylesheet_links.inline_bytes.get().checked_sub(entry.inline_bytes()).ok_or(lumen_html::Error::LimitExceeded)?;
            let available=lumen_html::html::MAX_HTML_BYTES.checked_sub(previous).ok_or(lumen_html::Error::LimitExceeded)?;
            let text=script_loading::script_child_text_bounded(document,node,lumen_html::css::MAX_CSS_BYTES.min(available))?;
            let decision=self.csp.borrow().check_inline(&text,Some(document.cryptographic_nonce(node).unwrap_or("")),
                lumen_common::csp::InlineCheckType::Style).map_err(|_|lumen_html::Error::LimitExceeded)?;
            violations=decision.violations;
            policy_blocked=decision.blocked;
            (!decision.blocked).then(||Arc::<str>::from(text))
        }else {None};
        let retained=self.stylesheet_links.inline_bytes.get().checked_sub(entry.inline_bytes())
            .and_then(|bytes|bytes.checked_add(source.as_ref().map_or(0,|source|source.len())))
            .filter(|bytes|*bytes<=lumen_html::html::MAX_HTML_BYTES).ok_or(lumen_html::Error::LimitExceeded)?;
        if let Some(token)=&entry.change_token {token.set(token.get().wrapping_add(1));}
        self.recompute_document_base_url(document,false);
        self.stylesheet_links.inline_bytes.set(retained);
        entry.inline=Some(InlineBlock{
source,policy_blocked,base:self.effective_base_url_from_cache().unwrap_or_else(||self.fallback_base_url()),violations,installed:false});
                let title=inline_stylesheet_title(document,node).unwrap_or("");
        entry.generation=generation;entry.completed_generation=None;entry.ready=None;entry.render_pending=std::rc::Weak::new();
        let preferred=self.stylesheet_links.preferred_title.borrow();let last=self.stylesheet_links.last_title.borrow();
        entry.sheet_disabled=Some(!title.is_empty() && title!=last.as_deref().unwrap_or(preferred.as_str()));entry.sheet_added=false;entry.flag_lease=None;drop(last);drop(preferred);

        let choose_preferred=entry.inline.as_ref().is_some_and(|inline|inline.source.is_some())
            && self.stylesheet_links.preferred_title.borrow().is_empty();
        drop(entries);
        if choose_preferred {
            let title=inline_stylesheet_title(document,node).unwrap_or("");
            if !title.is_empty() {self.stylesheet_links.change_preferred(document,title)?;}
        }
        self.stylesheet_links.generation.set(generation);
        self.stylesheet_links.dirty.borrow_mut().insert(node);
        self.stylesheet_links.inline_publication_pending.set(true);
        Ok(())
    }
    pub(crate) fn inline_stylesheet_associated(&self,node:NodeId)->bool {
        self.stylesheet_links.entries.borrow().get(&node)
            .is_some_and(|entry|!entry.parser_open && entry.inline.as_ref().is_some_and(|inline|inline.source.is_some()))
    }
    pub(crate) fn capture_inline_stylesheet_mutation(&self,document:&lumen_html::Document,mutation:&ObservedMutation) {
        let update=|node| {
            if self.capture_inline_stylesheet(document,node,false).is_err() {self.stylesheet_links.processing_failed.set(true);}
        };
        match &mutation.kind {
            ObservedKind::CharacterData{..}=>{if let Ok(Some(parent))=document.parent(mutation.target) {if is_style(document,parent) {update(parent);}}},
            ObservedKind::ChildList{..}|ObservedKind::ChildListMany{..}|ObservedKind::ChildListReplacement{..}=>{
                if is_style(document,mutation.target) {update(mutation.target);}
                for root in mutation.kind.added_nodes().chain(mutation.kind.removed_nodes()) {
                    let mut node=root;
                    loop {
                        if is_style(document,node) {update(node);}
                        let Some(next)=lumen_html::selector::next_shadow_including_descendant(document,root,node).ok().flatten() else {break};node=next;
                    }
                }
            },
            _=>{}
        }
    }
    /// Complete already captured style-block updates at a safe native boundary.
    /// The mutation/parser sink cannot borrow its Session. Only the root inline
    /// sheet is created here; links, import fetching and terminal events keep
    /// their normal asynchronous preparation and task phases.
    pub(crate) fn flush_inline_stylesheet_updates(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<()> {
        if self.stylesheet_links.processing_failed.get() {
            return Err(OpError::new("QuotaExceededError", "inline stylesheet processing admission"));
        }
        if !self.stylesheet_links.inline_publication_pending.get() { return Ok(()); }
        let mut candidates = {
            let entries = self.stylesheet_links.entries.borrow();
            let mut candidates = Vec::new();
            let count = entries.values().filter(|entry| !entry.parser_open
                && entry.inline.as_ref().is_some_and(|inline| !inline.installed)).count();
            candidates.try_reserve_exact(count).map_err(|_| OpError::new("QuotaExceededError", "inline stylesheet publication admission"))?;
            candidates.extend(entries.iter().filter_map(|(&node, entry)| (!entry.parser_open
                && entry.inline.as_ref().is_some_and(|inline| !inline.installed)).then_some((entry.generation,node))));
            candidates
        };
        candidates.sort_unstable_by_key(|(generation,_)| *generation);
        for (_,node) in candidates {
            self.ensure_inline_stylesheet(ctx, node)?;
        }
        self.stylesheet_links.inline_publication_pending.set(false);
        Ok(())
    }

    /// CSSOM reads create the root sheet synchronously; import completion must
    /// update that same sheet, preserving its source epoch and root rule identity.
    pub(crate) fn ensure_inline_stylesheet(self:&Rc<Self>,ctx:&mut Ctx,node:NodeId)->OpResult<bool> {
        let missing=self.stylesheet_links.entries.borrow().get(&node).is_none_or(|entry|entry.inline.is_none() && !entry.parser_open);
        if missing {
            let session=self.session.borrow();self.capture_inline_stylesheet(session.document(),node,false).map_err(dom_error)?;
        }
        let pending={let mut entries=self.stylesheet_links.entries.borrow_mut();let Some(entry)=entries.get_mut(&node) else{return Ok(false)};
            let Some(inline)=entry.inline.as_mut() else{return Ok(false)};
            if inline.installed {return Ok(inline.source.is_some())}
            (inline.source.clone(),inline.base.clone(),core::mem::take(&mut inline.violations),inline.policy_blocked)};
        for violation in pending.2 {self.queue_csp_violation(ctx,violation,Some(node))?;}
        let mut session=self.session.borrow_mut();
        // A host can attach the fetched graph before the first CSSOM access.
        // Preserve that accepted current-generation graph; a real authored
        // update already invalidates it through the shared mutation token.
        let installed_graph = pending.0.as_ref().is_some_and(|text|
            session.stylesheet_source(node).is_some_and(|source|source.text.as_ref()==text.as_ref()));
        if !installed_graph {
            session.set_stylesheet_source(node,None).map_err(|error|OpError::new("InvalidStateError",format!("inline stylesheet removal: {error:?}")))?;
        }
        let Some(text)=pending.0 else {
            drop(session);
            if pending.3 {self.queue_inline_style_failure(ctx,node)?;}
            if let Some(inline)=self.stylesheet_links.entries.borrow_mut().get_mut(&node).and_then(|entry|entry.inline.as_mut()) {inline.installed=true;}
            return Ok(false)
        };
        if !installed_graph {
            let imports=lumen_html::css::imports(&text).map_err(|error|OpError::new("SyntaxError",format!("inline CSS parse: {error:?}")))?;
            let source=lumen_html::css::StylesheetSource{disabled:false, url:Arc::from(pending.1),text,imports:imports.into_iter().map(|rule|lumen_html::css::LoadedImport{rule,source:None}).collect()};
            session.set_stylesheet_source(node,Some(source)).map_err(|error|OpError::new("InvalidStateError",format!("inline stylesheet creation: {error:?}")))?;
        }
        session.set_stylesheet_disabled(node,false).map_err(|error|OpError::new("InvalidStateError",format!("inline stylesheet enable: {error:?}")))?;
        let token=session.stylesheet_change_token(node).map_err(|error|OpError::new("QuotaExceededError",format!("inline stylesheet identity: {error:?}")))?;
        let flag_lease=session.stylesheet_root_lease(node).map_err(|error|OpError::new("QuotaExceededError",format!("stylesheet flag identity: {error:?}")))?;
        drop(session);
        if let Some(entry)=self.stylesheet_links.entries.borrow_mut().get_mut(&node) {entry.change_token=Some(token);entry.flag_lease=Some(flag_lease);}
        self.add_inline_stylesheet_set(node)?;
        if let Some(inline)=self.stylesheet_links.entries.borrow_mut().get_mut(&node).and_then(|entry|entry.inline.as_mut()) {inline.installed=true;}
        Ok(true)
    }
    fn queue_inline_style_failure(self:&Rc<Self>,ctx:&mut Ctx,node:NodeId)->OpResult<()> {
        // Match browser style-block processing failure notifications. This is
        // an actual enforced CSP rejection, not an unsupported MIME type or a
        // disconnected block, and must not install a stylesheet.
        let count=self.stylesheet_links.load_count.get().checked_add(1)
            .filter(|count|*count<=LINK_REQUEST_LIMIT)
            .ok_or_else(||OpError::new("QuotaExceededError","stylesheet failure task admission"))?;
        let root=self.retain_resource_request(ctx,node);
        self.stylesheet_links.load_count.set(count);
        let lease=LoadLease{owner:Rc::downgrade(self)};
        let realm=self.clone();
        scheduling::queue_task(ctx,move |ctx| {
            let _root=root;
            if !realm.is_document_destroyed() {
                realm.dispatch_user_agent(ctx,node,"error",false,false,&[])?;
            }
            drop(lease);
            realm.finish_stylesheet_load(ctx)
        })
    }
    fn add_inline_stylesheet_set(&self,node:NodeId)->OpResult<()> {
        let session=self.session.borrow();let document=session.document();
        let title=inline_stylesheet_title(document,node).unwrap_or("");
        let preferred=self.stylesheet_links.preferred_title.borrow();
        let last=self.stylesheet_links.last_title.borrow();
        let selected=last.as_deref().unwrap_or(preferred.as_str());
        let disabled=self.stylesheet_links.entries.borrow().get(&node).and_then(|entry|entry.sheet_disabled).unwrap_or_else(||!title.is_empty() && title!=selected);let preferred=preferred.clone();drop(last);drop(session);
        if let Some(entry)=self.stylesheet_links.entries.borrow_mut().get_mut(&node) {entry.sheet_added=true;entry.sheet_disabled=Some(disabled);if let Some(lease)=entry.flag_lease.as_ref().filter(|lease|!lease.is_detached()) {lease.set_disabled(disabled);}}
        let mut session=self.session.borrow_mut();session.set_preferred_stylesheet_set(&preferred).map_err(|error|OpError::new("InvalidStateError",format!("inline preferred set: {error:?}")))?;
        session.set_stylesheet_disabled(node,disabled).map_err(|error|OpError::new("InvalidStateError",format!("inline sheet applicability: {error:?}")))?;Ok(())
    }
}

fn admit_candidate(candidates:&mut HashSet<NodeId>,node:NodeId)->OpResult<()> {
    if !candidates.contains(&node) {
        if candidates.len()>=LINK_REQUEST_LIMIT {return Err(OpError::new("QuotaExceededError","stylesheet candidate limit"))}
        candidates.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","stylesheet candidate admission"))?;
        candidates.insert(node);
    }
    Ok(())
}

impl DomRealm {
    fn allows_render_blocking(&self)->bool {
        if self.content_type!="text/html" || self.lifecycle.destroyed.get() {return false}
        let session=self.session.borrow();let document=session.document();
        let html=named_child(document,document.root(),&["html"]).ok().flatten();
        html.and_then(|html|named_child(document,html,&["body","frameset"]).ok().flatten()).is_none()
    }
    /// Called only at an actual host rendering opportunity; resource and task
    /// pumping continues while rendering is blocked. Ten seconds is the UA's
    /// bounded render-blocking timeout, independent of resource cancellation.
    pub fn rendering_blocked(&self)->bool {
        self.rendering_blocked_at(lumen_host::perf::web_now_ms())
    }
    pub(crate) fn rendering_blocked_at(&self,now:f64)->bool {
        self.stylesheet_links.created_at.get().is_some_and(|created|now-created<10_000.)
            && (self.allows_render_blocking() || self.rendering_stylesheet_pending())
    }

    fn rendering_stylesheet_pending(&self)->bool {
        if self.stylesheet_links.render_count.get()==0 {return false}
        let session=self.session.borrow();let document=session.document();let environment=session.media_environment();
        self.stylesheet_links.entries.borrow().iter().any(|(node,entry)|
            entry.render_pending.upgrade().is_some_and(|active|active.get())
                && script_loading::is_connected(document,*node)
                && !session.stylesheet_disabled(*node)
                && (entry.parser_created || document.get_attribute_ns_ref(*node,None,"blocking").ok().flatten().is_some_and(|tokens|tokens.split_ascii_whitespace().any(|token|token.eq_ignore_ascii_case("render"))))
                && lumen_html::css::media_query_matches(document.get_attribute_ns_ref(*node,None,"media").ok().flatten().unwrap_or(""),environment))
    }
    pub fn is_document_destroyed(&self)->bool {self.lifecycle.destroyed.get()}
    pub fn has_stylesheet_resource_loader(&self)->bool { self.stylesheet_links.provider.borrow().is_some() }
    pub fn set_stylesheet_resource_loader(&self, provider: Rc<dyn StylesheetResourceLoader>) {
        self.stylesheet_links.install(provider);
    }
    pub fn stylesheet_resources_pending(&self) -> bool { self.stylesheet_links.pending() }

    /// Run preparation and consume completions on the owning main thread. No
    /// session borrow crosses provider admission, task admission or author code.
    pub fn queue_stylesheet_tasks(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<usize> {
        self.queue_style_attribute_violations(ctx)?;
        if self.stylesheet_links.processing_failed.replace(false) {return Err(OpError::new("QuotaExceededError","inline stylesheet processing admission"))}
        let provider = self.stylesheet_links.provider.borrow().clone();

        let mut candidates = core::mem::take(&mut *self.stylesheet_links.dirty.borrow_mut());
        if self.stylesheet_links.scan.replace(false) {
            let session = self.session.borrow();
            let document = session.document();
            let mut node = document.root();
            loop {
                if is_link(document,node) || lumen_html::xml_stylesheet::is_candidate(document,node) { admit_candidate(&mut candidates,node)?; }
                if is_style(document,node) {
                    admit_candidate(&mut candidates,node)?;
                    if self.stylesheet_links.entries.borrow().get(&node).is_none_or(|entry|entry.inline.is_none() && !entry.parser_open) {
                        self.capture_inline_stylesheet(document,node,false).map_err(dom_error)?;
                    }
                }
                let Some(next) = lumen_html::selector::next_shadow_including_descendant(document,document.root(),node).map_err(dom_error)? else { break; };
                node = next;
            }
            for &node in self.stylesheet_links.entries.borrow().keys() {
                admit_candidate(&mut candidates,node)?;
            }
        }
        self.synchronize_stylesheet_flags()?;
        for node in candidates {
            if is_style(self.session.borrow().document(),node) {self.prepare_inline_stylesheet(ctx,provider.as_ref(),node)?;}
            else if lumen_html::xml_stylesheet::is_candidate(self.session.borrow().document(),node) {self.prepare_stylesheet_instruction(ctx,provider.as_ref(),node)?;}
            else if let Some(provider)=provider.as_ref() {self.prepare_stylesheet_link(ctx,provider,node)?;}

        }
        let pending = self.stylesheet_links.entries.borrow().iter()
            .filter_map(|(&node,entry)|entry.pending.as_ref().and_then(|pending|pending.ticket.map(|ticket|(node,ticket,pending.generation))))
            .collect::<Vec<_>>();
        let mut completed=Vec::new();
        for (node,ticket,generation) in pending {
            let Some(result)=provider.as_ref().and_then(|provider|provider.poll(ticket)) else {continue};
            let lease = {
                let mut entries=self.stylesheet_links.entries.borrow_mut();
                let Some(entry)=entries.get_mut(&node).filter(|entry|entry.generation==generation) else {continue};
                let lease=entry.pending.take();
                entry.completed_generation=Some(generation);
                lease
            };
            let Some(lease)=lease else {continue};
            completed.push((node,generation,lease,result));
        }
        completed.sort_by(|(_,a,_,left),(_,b,_,right)| {
            let end=|result:&Result<StylesheetResponse,StylesheetFailure>|result.as_ref().map_or(f64::INFINITY,|response|response.end_time);
            end(left).total_cmp(&end(right)).then(a.cmp(b))
        });
        let mut count=0;
        for (node,generation,lease,result) in completed {
            self.queue_stylesheet_completion(ctx,node,generation,lease,result)?;
            count+=1;
        }
        count+=self.queue_cssom_import_tasks(ctx,provider.as_ref())?;
        let ready={let mut entries=self.stylesheet_links.entries.borrow_mut();entries.iter_mut().filter_map(|(node,entry)|
            (!self.import_completion_waits(*node)).then(||entry.ready.take().map(|ready|(*node,entry.generation,*ready))).flatten()).collect::<Vec<_>>()};
        for (node,generation,(lease,result)) in ready {self.queue_stylesheet_completion(ctx,node,generation,lease,result)?;count+=1;}
        self.finish_stylesheet_load(ctx)?;
        Ok(count)
    }

    fn register_import_occurrences(&self,owner:NodeId,leases:&[Rc<lumen_html::session::StylesheetImportLease>],covered:bool)->OpResult<()> {
        let mut works=self.stylesheet_links.imports.borrow_mut();
        let additional=leases.iter().filter(|lease|!works.contains_key(&(Rc::as_ptr(lease) as usize))).count();
        if works.len().checked_add(additional).is_none_or(|count|count>LINK_REQUEST_LIMIT) {return Err(OpError::new("QuotaExceededError","CSS import occurrence admission"))}
        works.try_reserve(additional).map_err(|_|OpError::new("QuotaExceededError","CSS import occurrence allocation"))?;
        for lease in leases {works.entry(Rc::as_ptr(lease) as usize).or_insert_with(||ImportWork{owner,lease:lease.clone(),covered_by_root:covered,completed:false,failed:false,pending:None,source_context:None});}
        Ok(())
    }
    fn record_import_graph_context(&self,owner:NodeId,base:Option<&Rc<lumen_html::session::StylesheetImportLease>>,contexts:Vec<lumen_html::stylesheet_loading::SourceContext>,failed:&[Vec<usize>])->OpResult<()> {
        let base_path=match base {Some(base)=>base.with_live_path(|path|path.map(Vec::from)),None=>Some(Vec::new())};
        let Some(base_path)=base_path else{return Ok(())};
        for context in contexts {
            if context.path.is_empty() && base.is_none() {
                if let Some(entry)=self.stylesheet_links.entries.borrow_mut().get_mut(&owner) {entry.source_context=Some((context.encoding,context.referrer_policy));}continue
            }
            let mut path=base_path.clone();path.try_reserve(context.path.len()).map_err(|_|OpError::new("QuotaExceededError","CSS source context path"))?;path.extend(context.path);
            let lease=self.session.borrow_mut().stylesheet_import_rule_lease(owner,path).map_err(|error|OpError::new("InvalidStateError",format!("CSS source context: {error:?}")))?;
            self.register_import_occurrences(owner,&[lease.clone()],false)?;
            if let Some(work)=self.stylesheet_links.imports.borrow_mut().get_mut(&(Rc::as_ptr(&lease) as usize)) {work.source_context=Some((context.encoding,context.referrer_policy));work.completed=true;}
        }
        // Failed leaves remain terminal for this occurrence. Deleting and
        // reinserting the same URL creates a new lease and a new attempt.
        for relative in failed {
            let mut path=base_path.clone();path.try_reserve(relative.len()).map_err(|_|OpError::new("QuotaExceededError","failed import path"))?;path.extend_from_slice(relative);
            let lease=self.session.borrow_mut().stylesheet_import_rule_lease(owner,path).map_err(|error|OpError::new("InvalidStateError",format!("failed CSS occurrence: {error:?}")))?;
            self.register_import_occurrences(owner,&[lease.clone()],false)?;
            if let Some(work)=self.stylesheet_links.imports.borrow_mut().get_mut(&(Rc::as_ptr(&lease) as usize)) {work.completed=true;work.failed=true;}
        }
        Ok(())
    }
    fn import_completion_waits(&self,owner:NodeId)->bool {
        self.stylesheet_links.import_dirty.borrow().contains(&owner) || self.stylesheet_links.imports.borrow().values()
            .any(|work|work.owner==owner && !work.covered_by_root && !work.completed)
    }
    fn queue_cssom_import_tasks(self:&Rc<Self>,ctx:&mut Ctx,provider:Option<&Rc<dyn StylesheetResourceLoader>>)->OpResult<usize> {
        // Deleted occurrences and replaced roots relinquish their real tickets,
        // roots and load delays before any new admission. A retained CSSOM
        // object keeps its detached graph, never the obsolete network operation.
        let stale={let session=self.session.borrow();self.stylesheet_links.imports.borrow().iter()
            .filter_map(|(key,work)|session.stylesheet_import_rule(work.owner,&work.lease).is_none().then_some(*key)).collect::<Vec<_>>()};
        for key in stale {if let Some(work)=self.stylesheet_links.imports.borrow_mut().remove(&key) {if let Some(pending)=work.pending {if let Some(provider)=provider {provider.cancel(pending.ticket);}}}}
        let cancelled_roots={let session=self.session.borrow();self.stylesheet_links.entries.borrow().iter().filter_map(|(owner,entry)| {
            let pending=entry.pending.as_ref()?;
            (!pending.imports.is_empty() && pending.imports.iter().all(|lease|session.stylesheet_import_rule(*owner,lease).is_none())).then_some(*owner)
        }).collect::<Vec<_>>()};
        for owner in cancelled_roots {
            let pending=self.stylesheet_links.entries.borrow_mut().get_mut(&owner).and_then(|entry|entry.pending.take());
            if let Some(pending)=pending {
                if let Some(ticket)=pending.ticket {if let Some(provider)=provider {provider.cancel(ticket);}}
                let source={let session=self.session.borrow();session.stylesheet_source(owner).map(|source|lumen_html::css::StylesheetSource{disabled:false, url:source.url.clone(),text:source.text.clone(),imports:Vec::new()})};
                if let Some(source)=source {
                    let response=StylesheetResponse{critical_failed:false,failed_import_paths:Vec::new(),source_contexts:Vec::new(),location_url:source.url.to_string(),content_type:Some("text/css".into()),source,
                        origin_clean:true,start_time:0.,end_time:0.,encoded_body_size:0,timing_allow:false,origin_metadata:Arc::from([]),violations:Vec::new(),timings:Vec::new()};
                    if let Some(entry)=self.stylesheet_links.entries.borrow_mut().get_mut(&owner) {entry.completed_generation=Some(pending.generation);entry.ready=Some(Box::new((pending,Ok(response))));}
                }
            }
        }
        let dirty=core::mem::take(&mut *self.stylesheet_links.import_dirty.borrow_mut());
        for owner in dirty {
            let candidates={let session=self.session.borrow();let mut candidates=Vec::new();
                if let Some(source)=session.stylesheet_source(owner) {collect_missing_imports(source,&mut Vec::new(),&mut Vec::new(),&mut candidates,!is_style(session.document(),owner))?;}candidates};
            for (path,url,parent_url,invalid) in candidates {
                let lease=self.session.borrow_mut().stylesheet_import_rule_lease(owner,path)
                    .map_err(|error|OpError::new("InvalidStateError",format!("CSS import occurrence: {error:?}")))?;
                let key=Rc::as_ptr(&lease) as usize;
                if self.stylesheet_links.imports.borrow().contains_key(&key) {continue}
                let Some(url)=url else {self.register_import_occurrences(owner,&[lease.clone()],false)?;if let Some(work)=self.stylesheet_links.imports.borrow_mut().get_mut(&key) {work.completed=true;work.failed=invalid;}continue};
                let parent_context=lease.with_live_path(|path|path.map(|path|path[..path.len()-1].to_vec()));
                let parent_context=if let Some(path)=parent_context.filter(|path|!path.is_empty()) {
                    let parent=self.session.borrow_mut().stylesheet_import_rule_lease(owner,path).map_err(|error|OpError::new("InvalidStateError",format!("CSS parent context: {error:?}")))?;
                    self.stylesheet_links.imports.borrow().get(&(Rc::as_ptr(&parent) as usize)).and_then(|work|work.source_context)
                }else{self.stylesheet_links.entries.borrow().get(&owner).and_then(|entry|entry.source_context)};
                let (encoding,policy)=parent_context.unwrap_or((self.document_encoding(),self.referrer_policy.get()));
                let request=StylesheetRequest{inline_source:None,import_request:true,url,document_url:self.document_url().unwrap_or_else(||"about:blank".into()),
                    referrer:parent_url,origin:self.script_fetch_origin(),nonce:String::new(),parser_inserted:false,integrity:String::new(),crossorigin:None,
                    referrer_policy:policy,environment_encoding:encoding,
                    quirks_mode:self.session.borrow().document().document_mode()==lumen_html::DocumentMode::Quirks,policies:self.module_fetch_policy_snapshot()?,start_time:lumen_host::perf::web_now_ms()};
                let count=self.stylesheet_links.load_count.get().checked_add(1).filter(|count|*count<=LINK_REQUEST_LIMIT)
                    .ok_or_else(||OpError::new("QuotaExceededError","CSS import load admission"))?;
                let root=self.retain_resource_request(ctx,owner);
                self.register_import_occurrences(owner,&[lease.clone()],false)?;
                match provider.map(|provider|provider.start(request)) {
                    Some(Ok(ticket))=>{self.stylesheet_links.load_count.set(count);if let Some(work)=self.stylesheet_links.imports.borrow_mut().get_mut(&key) {
                        work.pending=Some(ImportPending{ticket,_root:root,_load:LoadLease{owner:Rc::downgrade(self)}});
                    }else if let Some(provider)=provider {provider.cancel(ticket);}},
                    _=>{if let Some(work)=self.stylesheet_links.imports.borrow_mut().get_mut(&key) {work.completed=true;work.failed=true;}},
                }
            }
        }
        let pending=self.stylesheet_links.imports.borrow().iter().filter_map(|(key,work)|work.pending.as_ref().map(|pending|(*key,pending.ticket))).collect::<Vec<_>>();
        let mut count=0;
        for (key,ticket) in pending {
            let Some(result)=provider.and_then(|provider|provider.poll(ticket)) else {continue};
            let work={let mut works=self.stylesheet_links.imports.borrow_mut();works.get_mut(&key).and_then(|work|work.pending.take().map(|pending|(work.owner,work.lease.clone(),pending)))};
            let Some((owner,lease,pending))=work else {continue};
            let realm=self.clone();
            scheduling::queue_task(ctx,move |ctx| {
                let _pending=pending;
                if realm.lifecycle.destroyed.get() || realm.session.borrow().stylesheet_import_rule(owner,&lease).is_none() {return Ok(())}
                let failed=match result {
                    Ok(response)=>{
                        publish_timings(ctx,response.timings)?;realm.report_module_fetch_violations(ctx,response.violations)?;
                        let unqualified_failure=response.critical_failed && response.failed_import_paths.is_empty();
                        let install=realm.session.borrow_mut().complete_stylesheet_import_occurrences(owner,vec![(lease.clone(),Box::new(response.source))]);
                        if install.is_ok() {
                            realm.record_import_graph_context(owner,Some(&lease),response.source_contexts,&response.failed_import_paths)?;
                            if let Some(entry)=realm.stylesheet_links.entries.borrow_mut().get_mut(&owner) {
                                let live={let session=realm.session.borrow();let mut live=HashSet::new();if let Some(source)=session.stylesheet_source(owner) {collect_source_urls(source,&mut live)?;}live};
                                let mut metadata=entry.origin_metadata.iter().filter(|item|live.contains(&item.0)).cloned().collect::<Vec<_>>();
                                metadata.try_reserve(response.origin_metadata.len()).map_err(|_|OpError::new("QuotaExceededError","CSS origin provenance admission"))?;
                                for item in response.origin_metadata.iter() {if !metadata.iter().any(|known|known.0==item.0) {metadata.push(item.clone());}}
                                entry.origin_metadata=Arc::from(metadata);
                                cssom::update_stylesheet_origin_metadata(&realm,owner,entry.origin_metadata.clone());
                            }
                        }
                        unqualified_failure || install.is_err()
                    },
                    Err(failure)=>{publish_timings(ctx,failure.timings)?;realm.report_module_fetch_violations(ctx,failure.violations)?;true},
                };
                if let Some(work)=realm.stylesheet_links.imports.borrow_mut().get_mut(&key) {work.completed=true;work.failed=failed;}
                realm.stylesheet_links.import_dirty.borrow_mut().insert(owner);
                Ok(())
            })?;count+=1;
        }
        Ok(count)
    }

    fn queue_stylesheet_completion(self:&Rc<Self>,ctx:&mut Ctx,node:NodeId,generation:u64,
        lease:Pending,result:Result<StylesheetResponse,StylesheetFailure>)->OpResult<()> {
        if self.import_completion_waits(node) {
            if let Some(entry)=self.stylesheet_links.entries.borrow_mut().get_mut(&node).filter(|entry|entry.generation==generation) {entry.ready=Some(Box::new((lease,result)));}
            return Ok(())
        }
        let wrapper=self.wrap(ctx,node);
        let realm=self.clone();
        // The queued task is part of this attempt's load delay. Cancelling
        // a task drops both the node and script-blocking leases.
        scheduling::queue_task(ctx,move |ctx| {
            let _wrapper=wrapper;
            if !realm.stylesheet_links.current(node,generation) {drop(lease);return realm.finish_stylesheet_load(ctx)}
            if realm.import_completion_waits(node) {return realm.queue_stylesheet_completion(ctx,node,generation,lease,result)}
            let unchanged={let session=realm.session.borrow();
(is_style(session.document(),node) && realm.inline_stylesheet_associated(node)) || (is_link(session.document(),node) || lumen_html::xml_stylesheet::is_candidate(session.document(),node)) && realm.stylesheet_links.entries.borrow().get(&node)
                    .is_some_and(|entry|!entry.refetch && entry.selection.as_ref()==Some(&selection(session.document(),node)))};
            if !unchanged {drop(lease);return realm.finish_stylesheet_load(ctx)}
            let success=match result {
                Ok(response)=>{
                    publish_timings(ctx,response.timings)?;
                    realm.report_module_fetch_violations(ctx,response.violations)?;
                    if let Some(entry)=realm.stylesheet_links.entries.borrow_mut().get_mut(&node) {
                        entry.obtained_type=Some(response.content_type.as_deref().and_then(lumen_common::mime::mime_essence).unwrap_or("text/css").to_ascii_lowercase());
                        entry.origin_metadata=response.origin_metadata;
                        if !lumen_html::xml_stylesheet::is_candidate(realm.session.borrow().document(),node) {entry.location=Some(Arc::from(response.location_url));}
                        if entry.inline.is_none() {entry.sheet_disabled=None;entry.sheet_added=false;}
                    }
                    let mut session=realm.session.borrow_mut();
                    let inline=is_style(session.document(),node);
                    let installed=if inline {
                        let completed=lease.imports.iter().cloned().zip(response.source.imports).filter_map(|(lease,import)|import.source.map(|source|(lease,source))).collect();
                        session.complete_stylesheet_import_occurrences(node,completed).map(|_|())
                    }else {session.set_stylesheet_source(node,Some(response.source))};
                    let installed=installed.is_ok();
                    if installed {if let Some(descriptor)=lumen_html::xml_stylesheet::descriptor(session.document(),node).map_err(dom_error)? {
                        let title=descriptor.title().to_owned();let media=descriptor.media().to_owned();
                        session.set_stylesheet_metadata(node,&title,&media).map_err(|error|OpError::new("QuotaExceededError",format!("stylesheet metadata: {error:?}")))?;
                    }}
                    drop(session);
                    if installed && !inline {
                        // Publish the source's actual link/PI association before
                        // capturing its flag lease. In particular, a PI has no
                        // HTML href attribute from which an unassociated lease
                        // could recover resource text.
                        realm.update_stylesheet_applicability(node,response.origin_clean)?;
                        let flag_lease=realm.session.borrow_mut().stylesheet_root_lease(node)
                            .map_err(|error|OpError::new("QuotaExceededError",format!("stylesheet flag identity: {error:?}")))?;
                        if let Some(entry)=realm.stylesheet_links.entries.borrow_mut().get_mut(&node) {entry.flag_lease=Some(flag_lease);}
                    }
                    if let Some(entry)=realm.stylesheet_links.entries.borrow().get(&node) {cssom::update_stylesheet_origin_metadata(&realm,node,entry.origin_metadata.clone());}
                    for (ordinal,import) in lease.imports.iter().enumerate() {if let Some(work)=realm.stylesheet_links.imports.borrow_mut().get_mut(&(Rc::as_ptr(import) as usize)) {
                        work.completed=true;work.covered_by_root=false;
                        work.failed=response.failed_import_paths.iter().any(|path|path.as_slice()==[ordinal]);
                    }}
                    if installed && inline {
                        let mut contexts=Vec::new();
                        for context in response.source_contexts {
                            if context.path.is_empty() {contexts.push(context);continue}
                            let Some(import)=lease.imports.get(context.path[0]) else{continue};
                            let mut relative=context;relative.path.remove(0);
                            realm.record_import_graph_context(node,Some(import),vec![relative],&[])?;
                        }
                        realm.record_import_graph_context(node,None,contexts,&[])?;
                        for (ordinal,import) in lease.imports.iter().enumerate() {
                            let failed=response.failed_import_paths.iter().filter(|path|path.first()==Some(&ordinal)).map(|path|path[1..].to_vec()).collect::<Vec<_>>();
                            realm.record_import_graph_context(node,Some(import),Vec::new(),&failed)?;
                        }
                    }else if installed {realm.record_import_graph_context(node,None,response.source_contexts,&response.failed_import_paths)?;}
                    if inline {
                        let session=realm.session.borrow();
                        installed && !realm.stylesheet_links.imports.borrow().values().any(|work|work.owner==node && work.failed && session.stylesheet_import_rule(node,&work.lease).is_some())
                            && !(response.critical_failed && response.failed_import_paths.is_empty())
                    }else {installed && !response.critical_failed}
                }
                Err(failure)=>{
                    for import in &lease.imports {if let Some(work)=realm.stylesheet_links.imports.borrow_mut().get_mut(&(Rc::as_ptr(import) as usize)) {work.completed=true;work.covered_by_root=false;work.failed=true;}}
                    publish_timings(ctx,failure.timings)?;
                    realm.report_module_fetch_violations(ctx,failure.violations)?;
                    if realm.session.borrow().link_stylesheet_state(node).is_some() {
                        realm.session.borrow_mut().set_stylesheet_source(node,None)
                            .map_err(|error|OpError::new("InvalidStateError",format!("failed stylesheet removal: {error:?}")))?;
                    }
                    false
                },
            };
            realm.dispatch_user_agent(ctx,node,if success {"load"}else{"error"},false,false,&[])?;
            drop(lease);
            realm.finish_stylesheet_load(ctx)?;
            Ok(())
        })?;
        Ok(())
    }

    fn finish_stylesheet_load(self:&Rc<Self>,ctx:&mut Ctx)->OpResult<()> {
        if self.stylesheet_links.pending() || self.lifecycle.destroyed.get() {return Ok(())}
        self.resume_stylesheet_blocked_parser(ctx)?;
        if self.stylesheet_links.deferred_complete.replace(false) {
            self.set_document_ready_state(ctx,DocumentReadyState::Complete)?;
        }
        if self.stylesheet_links.deferred_load.replace(false) {
            self.dispatch_window_user_agent(ctx,"load",false,false)?;
        }
        Ok(())
    }

    fn prepare_stylesheet_instruction(self:&Rc<Self>,ctx:&mut Ctx,provider:Option<&Rc<dyn StylesheetResourceLoader>>,node:NodeId)->OpResult<()> {
        self.stylesheet_links.admit_owner_update(node).map_err(dom_error)?;
        self.session.borrow_mut().reclaim_stale_stylesheet_sources();
        let token=self.session.borrow_mut().stylesheet_change_token(node).map_err(|error|OpError::new("QuotaExceededError",format!("XML stylesheet identity: {error:?}")))?;
        let selected={let session=self.session.borrow();
            lumen_html::xml_stylesheet::descriptor(session.document(),node).map_err(dom_error)?;
            selection(session.document(),node)};
        let mut entries=self.stylesheet_links.entries.borrow_mut();
        let entry=entries.entry(node).or_default();
        entry.change_token=Some(token);
        if entry.selection.as_ref()==Some(&selected) && !core::mem::take(&mut entry.refetch) {return Ok(());}
        if let Some(pending)=entry.pending.take() {if let (Some(provider),Some(ticket))=(provider,pending.ticket) {provider.cancel(ticket);}drop(pending);}
        entry.generation=self.stylesheet_links.next_generation()?;
        entry.completed_generation=None;entry.refetch=false;entry.selection=Some(selected.clone());
        entry.sheet_disabled=None;entry.sheet_added=false;
        let generation=entry.generation;let parser_created=entry.parser_created;
        drop(entries);
        // CSSOM XML instructions replace immediately, before obtaining a new
        // resource. A retained old CSSOM wrapper keeps its detached graph.
        if self.session.borrow().link_stylesheet_state(node).is_some() {
            self.session.borrow_mut().set_stylesheet_source(node,None).map_err(|error|OpError::new("InvalidStateError",format!("XML stylesheet removal: {error:?}")))?;
        }
        if !selected.connected {return Ok(());}
        let Some(provider)=provider else {return Ok(());};
        let (href,encoding,matching)={let session=self.session.borrow();
            let Some(descriptor)=lumen_html::xml_stylesheet::descriptor(session.document(),node).map_err(dom_error)? else {return Ok(());};
            if !descriptor.css_supported() {return Ok(());}
            let Some(href)=descriptor.href() else {return Ok(());};
            (href.to_owned(),descriptor.get("charset").and_then(|label| lumen_common::encoding::canonical_document_label(label).ok()).unwrap_or(self.document_encoding()),
                lumen_html::css::media_query_matches(descriptor.media(),session.media_environment()))
        };
        let Some(url)=lumen_common::url::parse_url(&href,lumen_common::url::parse_url(&self.base_url(),None).as_ref()) else {return Ok(());};
        let location=url.href();
        if let Some(entry)=self.stylesheet_links.entries.borrow_mut().get_mut(&node) {entry.location=Some(Arc::from(location.as_str()));}
        let request=StylesheetRequest{inline_source:None,import_request:false,url:location,document_url:self.document_url().unwrap_or_else(||"about:blank".into()),
            referrer:self.document_url().unwrap_or_else(||"about:blank".into()),origin:self.script_fetch_origin(),nonce:String::new(),parser_inserted:parser_created,
            integrity:String::new(),crossorigin:None,referrer_policy:self.referrer_policy.get(),environment_encoding:encoding,
            quirks_mode:self.session.borrow().document().document_mode()==lumen_html::DocumentMode::Quirks,
            policies:self.module_fetch_policy_snapshot()?,start_time:lumen_host::perf::web_now_ms()};
        self.start_stylesheet_request(ctx,Some(provider),node,request,generation,parser_created && matching && !selected.alternate,parser_created)
    }

    fn prepare_stylesheet_link(self:&Rc<Self>,ctx:&mut Ctx,provider:&Rc<dyn StylesheetResourceLoader>,node:NodeId)->OpResult<()> {
        let selected={let session=self.session.borrow();
            if !is_link(session.document(),node) {None}else{Some(selection(session.document(),node))}};
        let initially_matching_media={let session=self.session.borrow();let raw=session.document().get_attribute_ns_ref(node,None,"media").ok().flatten().unwrap_or("");
            lumen_html::css::media_query_matches(raw,session.media_environment())};
        let mut entries=self.stylesheet_links.entries.borrow_mut();
        if selected.is_none() {
            if let Some(mut entry)=entries.remove(&node) {
                self.stylesheet_links.inline_bytes.set(self.stylesheet_links.inline_bytes.get()-entry.inline_bytes());
                if let Some(pending)=entry.pending.take() {if let Some(ticket)=pending.ticket {provider.cancel(ticket);}}
            }
            return Ok(())
        }
        if !entries.contains_key(&node) {
            if entries.len()>=LINK_REQUEST_LIMIT {return Err(OpError::new("QuotaExceededError","stylesheet owner limit"))}
            entries.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","stylesheet request admission"))?;
        }
        let entry=entries.entry(node).or_default();
        if entry.script_blocking_eligible.is_none() {
            entry.script_blocking_eligible=Some(entry.parser_created && initially_matching_media && selected.as_ref().is_some_and(|selected|
                selected.connected && !selected.disabled && !selected.alternate && !selected.href.is_empty()
                && lumen_common::mime::stylesheet_hint_supported(&selected.kind)));
        }
        if entry.selection==selected && !core::mem::take(&mut entry.refetch) {
            drop(entries);
            let loaded_state=self.session.borrow().link_stylesheet_state(node);
            if let Some(state)=loaded_state {
                self.update_stylesheet_applicability(node,state.origin_clean)?;
            }
            return Ok(())
        }
        if let Some(pending)=entry.pending.take(){if let Some(ticket)=pending.ticket {provider.cancel(ticket);}}
        entry.generation=self.stylesheet_links.next_generation()?;
        entry.completed_generation=None;entry.ready=None;
        entry.refetch=false;
        entry.selection=selected.clone();
        let generation=entry.generation;
        let parser_created=entry.parser_created;
        let script_blocking_eligible=entry.script_blocking_eligible==Some(true);
        drop(entries);
        // A new href/crossorigin attempt keeps the prior associated sheet until
        // completion. Ceasing to be a connected styling link removes it now.
        let remove=selected.as_ref().is_none_or(|selected| !selected.connected || selected.href.is_empty());
        let loaded=self.session.borrow().link_stylesheet_state(node).is_some();
        if remove && loaded {
            self.session.borrow_mut().set_stylesheet_source(node,None)
                .map_err(|error|OpError::new("InvalidStateError",format!("stylesheet removal: {error:?}")))?;
        }
        let Some(selected)=selected.filter(|selection|selection.connected && !selection.disabled && !selection.href.is_empty()
            && lumen_common::mime::stylesheet_hint_supported(&selection.kind)) else{return Ok(())};
        // An alternate without a title is not an external-resource stylesheet.
        let (title,nonce,integrity,policy,quirks,encoding)={let session=self.session.borrow();let document=session.document();
            let attr=|name|document.get_attribute_ns_ref(node,None,name).ok().flatten().unwrap_or("");
            (attr("title").to_owned(),document.cryptographic_nonce(node).unwrap_or("").to_owned(),attr("integrity").to_owned(),
             lumen_common::referrer::ReferrerPolicy::parse(attr("referrerpolicy")).unwrap_or(self.referrer_policy.get()),
             document.document_mode()==lumen_html::DocumentMode::Quirks,
             lumen_common::encoding::canonical_document_label(attr("charset")).unwrap_or(self.document_encoding()))};
        if selected.alternate && title.is_empty(){return Ok(())}
        let url=lumen_common::url::parse_url(&selected.href,lumen_common::url::parse_url(&self.base_url(),None).as_ref())
            .map(|url|url.href()).unwrap_or_default();
        let request=StylesheetRequest {inline_source:None,import_request:false,url,document_url:self.document_url().unwrap_or_else(||"about:blank".into()),referrer:self.script_fetch_referrer().source,
            origin:self.script_fetch_origin(),nonce,parser_inserted:parser_created,integrity,crossorigin:selected.crossorigin,referrer_policy:policy,
            environment_encoding:encoding,quirks_mode:quirks,policies:self.module_fetch_policy_snapshot()?,start_time:lumen_host::perf::web_now_ms()};
        self.start_stylesheet_request(ctx,Some(provider),node,request,generation,script_blocking_eligible,parser_created)
    }
    fn prepare_inline_stylesheet(self:&Rc<Self>,ctx:&mut Ctx,provider:Option<&Rc<dyn StylesheetResourceLoader>>,node:NodeId)->OpResult<()> {
        let stale={let mut entries=self.stylesheet_links.entries.borrow_mut();
            entries.get_mut(&node).and_then(|entry|if entry.pending.as_ref().is_some_and(|pending|pending.generation!=entry.generation) {entry.pending.take()}else {None})};
        if let Some(pending)=stale {if let (Some(provider),Some(ticket))=(provider,pending.ticket) {provider.cancel(ticket);}drop(pending);}
        if !self.ensure_inline_stylesheet(ctx,node)? {return Ok(())}
        let (text,base,generation,parser_created,script_blocking_eligible)={
            let matching={let session=self.session.borrow();let raw=session.document().get_attribute_ns_ref(node,None,"media").ok().flatten().unwrap_or("");
                lumen_html::css::media_query_matches(raw,session.media_environment())};
            let enabled=!self.session.borrow().stylesheet_disabled(node);
            let mut entries=self.stylesheet_links.entries.borrow_mut();let Some(entry)=entries.get_mut(&node) else{return Ok(())};
            if entry.pending.is_some() || entry.completed_generation==Some(entry.generation) {return Ok(())}
            let Some(inline)=entry.inline.as_ref() else{return Ok(())};let Some(text)=inline.source.clone() else{return Ok(())};
            if entry.script_blocking_eligible.is_none() {entry.script_blocking_eligible=Some(entry.parser_created && matching && enabled);}
            (text,inline.base.clone(),entry.generation,entry.parser_created,entry.script_blocking_eligible==Some(true))
        };
        let request=StylesheetRequest {inline_source:Some(text),import_request:false,url:base,document_url:self.document_url().unwrap_or_else(||"about:blank".into()),
            referrer:self.script_fetch_referrer().source,origin:self.script_fetch_origin(),nonce:String::new(),parser_inserted:parser_created,
            integrity:String::new(),crossorigin:None,referrer_policy:self.referrer_policy.get(),environment_encoding:self.document_encoding(),
            quirks_mode:self.session.borrow().document().document_mode()==lumen_html::DocumentMode::Quirks,
            policies:self.module_fetch_policy_snapshot()?,start_time:lumen_host::perf::web_now_ms()};
        self.start_stylesheet_request(ctx,provider,node,request,generation,script_blocking_eligible,parser_created)
    }
    fn start_stylesheet_request(self:&Rc<Self>,ctx:&mut Ctx,provider:Option<&Rc<dyn StylesheetResourceLoader>>,node:NodeId,
        request:StylesheetRequest,generation:u64,script_blocking_eligible:bool,parser_created:bool)->OpResult<()> {

        if let Some(entry)=self.stylesheet_links.entries.borrow_mut().get_mut(&node) {entry.source_context=Some((request.environment_encoding,request.referrer_policy));}
        let inline_processed=request.inline_source.as_ref().is_some_and(|text|
            lumen_html::css::imports(text).is_ok_and(|imports|imports.is_empty()));
        // Synchronously parsed import-free rules are already available; their
        // queued load notification is not unresolved styling work.
        let blocking=if !inline_processed && script_blocking_eligible && self.ready_state.get()==DocumentReadyState::Loading {
            Some(self.begin_script_blocking_stylesheet(node)?)
        }else{None};
        let explicitly_render_blocking={let session=self.session.borrow();session.document()
            .get_attribute_ns_ref(node,None,"blocking").ok().flatten()
            .is_some_and(|tokens|tokens.split_ascii_whitespace().any(|token|token.eq_ignore_ascii_case("render")))};
        let media_matches={let session=self.session.borrow();let instruction=lumen_html::xml_stylesheet::descriptor(session.document(),node).map_err(dom_error)?;
            let raw=instruction.as_ref().map_or_else(||session.document().get_attribute_ns_ref(node,None,"media").ok().flatten().unwrap_or(""),|descriptor|descriptor.media());
            lumen_html::css::media_query_matches(raw,session.media_environment())};
        let render=if self.allows_render_blocking() && media_matches && (parser_created || explicitly_render_blocking) {
            let count=self.stylesheet_links.render_count.get().checked_add(1)
                .ok_or_else(||OpError::new("QuotaExceededError","render-blocking stylesheet admission"))?;
            self.stylesheet_links.render_count.set(count);
            let active=Rc::new(Cell::new(true));
            if let Some(entry)=self.stylesheet_links.entries.borrow_mut().get_mut(&node) {entry.render_pending=Rc::downgrade(&active);}
            Some(RenderLease{owner:Rc::downgrade(self),active})
        }else{None};
        let imports=if request.inline_source.is_some() {
            let count=self.session.borrow().stylesheet_source(node).map_or(0,|source|source.imports.len());
            let mut imports=Vec::new();imports.try_reserve(count).map_err(|_|OpError::new("QuotaExceededError","inline import occurrence admission"))?;
            for index in 0..count {
                imports.push(self.session.borrow_mut().stylesheet_import_rule_lease(node,vec![index])
                    .map_err(|error|OpError::new("QuotaExceededError",format!("inline import occurrence: {error:?}")))?);
            }
            self.register_import_occurrences(node,&imports,true)?;imports
        }else {Vec::new()};
        let root=self.retain_resource_request(ctx,node);

        let count=self.stylesheet_links.load_count.get().checked_add(1)
            .ok_or_else(||OpError::new("QuotaExceededError","stylesheet load delay"))?;
        if count>LINK_REQUEST_LIMIT {return Err(OpError::new("QuotaExceededError","stylesheet concurrent request limit"))}
        if let Some(text)=request.inline_source.as_ref().filter(|_|inline_processed) {
            let source=lumen_html::css::StylesheetSource{disabled:false, url:Arc::from(request.url.as_str()),text:text.clone(),imports:Vec::new()};
            self.stylesheet_links.load_count.set(count);
            if let Some(entry)=self.stylesheet_links.entries.borrow_mut().get_mut(&node) {entry.completed_generation=Some(generation);}
            return self.queue_stylesheet_completion(ctx,node,generation,
                Pending{ticket:None,generation,_root:root,_blocking:blocking,_render:render,_load:LoadLease{owner:Rc::downgrade(self)},imports},
                Ok(StylesheetResponse{critical_failed:false,failed_import_paths:Vec::new(),source_contexts:Vec::new(),location_url:request.url,content_type:Some("text/css".into()),source,
                    origin_clean:true,start_time:request.start_time,end_time:lumen_host::perf::web_now_ms(),encoded_body_size:0,timing_allow:false,
                    origin_metadata:Arc::from([]),violations:Vec::new(),timings:Vec::new()}));
        }
        let provider=provider.ok_or_else(||OpError::new("InvalidStateError","stylesheet resource loader unavailable"))?;
        let ticket=provider.start(request).map_err(|message|OpError::new("QuotaExceededError",message))?;

        if let Some(entry)=self.stylesheet_links.entries.borrow_mut().get_mut(&node) {
            self.stylesheet_links.load_count.set(count);
            entry.pending=Some(Pending {ticket:Some(ticket),generation,_root:root,_blocking:blocking,_render:render,
                _load:LoadLease {owner:Rc::downgrade(self)},imports});
        } else {
            provider.cancel(ticket);
        }
        Ok(())
    }

    pub fn change_preferred_stylesheet_set(&self,name:&str)->OpResult<()> {
        let session=self.session.borrow();self.stylesheet_links.change_preferred(session.document(),name).map_err(dom_error)?;drop(session);
        self.synchronize_stylesheet_flags()
    }
    pub fn select_stylesheet_set(&self,name:&str)->OpResult<()> {
        if name.len()>lumen_html::html::MAX_HTML_BYTES {return Err(dom_error(lumen_html::Error::LimitExceeded));}
        let mut stored=String::new();stored.try_reserve_exact(name.len()).map_err(|_|dom_error(lumen_html::Error::LimitExceeded))?;stored.push_str(name);
        let session=self.session.borrow();self.stylesheet_links.enable_set(session.document(),name).map_err(dom_error)?;drop(session);
        *self.stylesheet_links.last_title.borrow_mut()=Some(stored);self.synchronize_stylesheet_flags()
    }
    pub fn set_stylesheet_response_headers(&self,headers:&[(String,String)])->OpResult<()> {
        for (name,value) in headers {if name.eq_ignore_ascii_case("default-style") {self.change_preferred_stylesheet_set(value)?;}}
        Ok(())
    }
    fn synchronize_stylesheet_flags(&self)->OpResult<()> {
        if !self.stylesheet_links.flags_dirty.get() {return Ok(());}
        let mut session=self.session.borrow_mut();
        session.set_preferred_stylesheet_set(&self.stylesheet_links.preferred_title.borrow()).map_err(|error|OpError::new("InvalidStateError",format!("preferred sheet set: {error:?}")))?;
        session.flush_stylesheet_flag_changes().map_err(|error|OpError::new("InvalidStateError",format!("sheet flag synchronization: {error:?}")))?;
        self.stylesheet_links.flags_dirty.set(false);Ok(())
    }
    pub(crate) fn update_stylesheet_applicability(&self,node:NodeId,origin_clean:bool)->OpResult<()> {
        let session=self.session.borrow();let document=session.document();
        let attr=|node,name|document.get_attribute_ns_ref(node,None,name).ok().flatten().unwrap_or("");
        let instruction=lumen_html::xml_stylesheet::descriptor(document,node).map_err(dom_error)?;
        let title=instruction.as_ref().map_or_else(||inline_stylesheet_title(document,node).unwrap_or(""),|descriptor|descriptor.title());
        let alternate=instruction.as_ref().map_or_else(||attr(node,"rel").split_ascii_whitespace().any(|token|token.eq_ignore_ascii_case("alternate")),|descriptor|descriptor.alternate());
        let attribute_disabled=document.get_attribute_ns_ref(node,None,"disabled").ok().flatten().is_some();
        let entries=self.stylesheet_links.entries.borrow();
        let explicitly_enabled=entries.get(&node).is_some_and(|entry|entry.explicitly_enabled);
        let manual_disabled=entries.get(&node).and_then(|entry|entry.sheet_disabled);
        let adding=entries.get(&node).is_none_or(|entry|!entry.sheet_added);
        drop(entries);
        if adding && manual_disabled!=Some(true) && self.stylesheet_links.preferred_title.borrow().is_empty()
            && !title.is_empty() && !(alternate && !explicitly_enabled) {
            self.stylesheet_links.change_preferred(document,title).map_err(dom_error)?;
        }
        let preferred=self.stylesheet_links.preferred_title.borrow();
        let last=self.stylesheet_links.last_title.borrow();
        let selected=last.as_deref().unwrap_or(preferred.as_str());
        let disabled=if adding {manual_disabled.unwrap_or(attribute_disabled || (!title.is_empty() && selected!=title))}
            else {self.stylesheet_links.entries.borrow().get(&node).and_then(|entry|entry.sheet_disabled).unwrap_or(false)};
        let preferred_title=preferred.clone();drop(preferred);drop(last);drop(session);
        if let Some(entry)=self.stylesheet_links.entries.borrow_mut().get_mut(&node) {entry.sheet_added=true;entry.sheet_disabled=Some(disabled);if let Some(lease)=entry.flag_lease.as_ref().filter(|lease|!lease.is_detached()) {lease.set_disabled(disabled);}}
        let mut session=self.session.borrow_mut();
        session.set_preferred_stylesheet_set(&preferred_title)
            .and_then(|()|session.set_link_stylesheet_state(node,lumen_html::layout::LinkStylesheetState {disabled,origin_clean,attribute_disabled}))
            .map_err(|error|OpError::new("InvalidStateError",format!("stylesheet applicability: {error:?}")))
    }
}
fn is_link(document:&lumen_html::Document,node:NodeId)->bool {
    matches!(document.kind(node),Ok(NodeKind::Element{name,namespace:Namespace::Html,..}) if name.as_ref()=="link")
}
fn selection(document:&lumen_html::Document,node:NodeId)->Selection {
    if lumen_html::xml_stylesheet::is_candidate(document,node) {
        let descriptor=lumen_html::xml_stylesheet::descriptor(document,node).ok().flatten();
        return Selection{href:descriptor.as_ref().and_then(|descriptor|descriptor.href()).unwrap_or("").into(),crossorigin:None,
            kind:descriptor.as_ref().and_then(|descriptor|descriptor.get("type")).unwrap_or("").into(),
            alternate:descriptor.as_ref().is_some_and(|descriptor|descriptor.alternate()),disabled:false,connected:descriptor.is_some()};
    }
    let attr=|name|document.get_attribute_ns_ref(node,None,name).ok().flatten().unwrap_or("");
    let rel=attr("rel");
    let stylesheet=rel.split_ascii_whitespace().any(|token|token.eq_ignore_ascii_case("stylesheet"));
    let alternate=rel.split_ascii_whitespace().any(|token|token.eq_ignore_ascii_case("alternate"));
    let crossorigin=document.get_attribute_ns_ref(node,None,"crossorigin").ok().flatten()
        .map(|value|value.eq_ignore_ascii_case("use-credentials"));
    Selection { href:if stylesheet {attr("href").into()}else{String::new()},crossorigin,
        kind:attr("type").into(),alternate,disabled:document.get_attribute_ns_ref(node,None,"disabled").ok().flatten().is_some(),
        connected:script_loading::is_connected(document,node) }
}

fn publish_timings(ctx:&mut Ctx,timings:Vec<StylesheetResourceTiming>)->OpResult<()> {
    for timing in timings {
        lumen_host::performance_timeline::record_resource(ctx,&timing.name,timing.initiator_type,
            timing.start_time,timing.end_time,timing.encoded_body_size,timing.decoded_body_size,timing.timing_allowed)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;
    #[test]
    fn specification_inline_style_policy_failure_tasks_follow_real_attempts() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<head><meta http-equiv='Content-Security-Policy' content=\"style-src 'none'\"><style id=parser>body{color:red}</style></head><body></body>",128).unwrap();
        assert!(matches!(engine.eval_value("globalThis.errors=[];document.addEventListener('error',e=>errors.push([e.target.id,e.isTrusted,e.bubbles]),true);errors.length===0"),Ok(Ok(Value::Bool(true)))));
        realm.queue_stylesheet_tasks(engine.ctx()).unwrap();
        assert!(matches!(engine.eval_value("errors.length===0"),Ok(Ok(Value::Bool(true)))),"failures are queued, not synchronously dispatched");
        assert!(scheduling::run_tasks(&mut engine,32).is_empty());
        assert!(matches!(engine.eval_value("errors.length===1 && errors[0][0]==='parser' && errors[0][1] && !errors[0][2] && document.getElementById('parser').sheet===null"),Ok(Ok(Value::Bool(true)))));
        assert!(matches!(engine.eval_value("globalThis.dynamic=document.createElement('style');dynamic.id='dynamic';dynamic.textContent='body{color:green}';document.head.append(dynamic);globalThis.ignored=document.createElement('style');ignored.type='text/not-css';ignored.textContent='body{color:blue}';document.head.append(ignored);true"),Ok(Ok(Value::Bool(true)))));
        realm.queue_stylesheet_tasks(engine.ctx()).unwrap();
        assert!(scheduling::run_tasks(&mut engine,32).is_empty());
        assert!(matches!(engine.eval_value("errors.length===2 && errors[1][0]==='dynamic' && errors[1][1] && dynamic.sheet===null"),Ok(Ok(Value::Bool(true)))));
        assert!(matches!(engine.eval_value("dynamic.textContent='body{color:blue}';true"),Ok(Ok(Value::Bool(true)))));
        realm.queue_stylesheet_tasks(engine.ctx()).unwrap();
        assert!(scheduling::run_tasks(&mut engine,32).is_empty());
        realm.queue_stylesheet_tasks(engine.ctx()).unwrap();
        assert!(scheduling::run_tasks(&mut engine,32).is_empty());
        assert!(matches!(engine.eval_value("errors.length===3 && errors[2][0]==='dynamic' && errors[2][1] && dynamic.sheet===null"),Ok(Ok(Value::Bool(true)))),"one real failure notification per processing attempt");
        assert_eq!(realm.stylesheet_links.load_count.get(),0,"failure tasks release their load-delay leases");
    }
    #[derive(Default)]
    struct ManualProvider {
        requests:RefCell<Vec<StylesheetRequest>>,
        cancelled:RefCell<Vec<u64>>,
        ready:RefCell<HashMap<u64,Result<StylesheetResponse,StylesheetFailure>>>,
    }
    impl StylesheetResourceLoader for ManualProvider {
        fn start(&self,request:StylesheetRequest)->Result<u64,String> {
            self.requests.borrow_mut().push(request);Ok(self.requests.borrow().len() as u64)
        }
        fn poll(&self,ticket:u64)->Option<Result<StylesheetResponse,StylesheetFailure>> {self.ready.borrow_mut().remove(&ticket)}
        fn cancel(&self,ticket:u64) {self.cancelled.borrow_mut().push(ticket);self.ready.borrow_mut().remove(&ticket);}
    }
    impl ManualProvider {
        fn complete(&self,ticket:u64,text:&str) {
            let url=self.requests.borrow()[ticket as usize-1].url.clone();
            self.ready.borrow_mut().insert(ticket,Ok(StylesheetResponse{critical_failed:false,failed_import_paths:Vec::new(),source_contexts:Vec::new(),
                location_url:url.clone(),content_type:Some("text/css".into()),source:lumen_html::css::StylesheetSource{disabled:false, url:Arc::from(url),text:Arc::from(text),imports:Vec::new()},
                origin_clean:true,start_time:1.,end_time:ticket as f64,encoded_body_size:text.len() as u64,timing_allow:true,
                origin_metadata:Arc::from([]),violations:Vec::new(),timings:Vec::new(),
            }));
        }
    }
    fn check(engine:&mut Engine,source:&str) {
        match engine.eval_value(source) {
            Ok(Ok(Value::Bool(true)))=>{},
            Ok(Err(error))=>match engine.describe_throw(error) {
                lumen::Completion::Throw{name,message}=>panic!("stylesheet guard: {name}: {message}"),
                lumen::Completion::Value(message)=>panic!("stylesheet guard: {message}"),
            },
            _=>panic!("stylesheet guard did not return true"),
        }
    }
    fn turn(engine:&mut Engine,realm:&Rc<DomRealm>) {
        realm.queue_stylesheet_tasks(engine.ctx()).unwrap();
        assert!(scheduling::run_tasks(engine,64).is_empty(),"stylesheet terminal task threw");
    }
    #[test]
    fn specification_stylesheet_sets_creation_order_live_titles_and_selected_preference() {
        let mut engine=Engine::new();let realm=crate::install(engine.ctx(),"<head></head><body><p>text</p></body>",256).unwrap();
        check(&mut engine,r#"globalThis.a=document.createElement('style');a.title='A';a.textContent='p{color:green}';document.head.append(a);
            globalThis.b=document.createElement('style');b.title='B';b.textContent='p{color:red}';document.head.insertBefore(b,a);
            globalThis.sa=a.sheet;globalThis.sb=b.sheet;if(sa.disabled||!sb.disabled)throw Error('creation order');
            a.title='B';b.title='A';if(a.sheet!==sa||b.sheet!==sb||sa.disabled||!sb.disabled)throw Error('live title changed actual sheet flags');
            a.media='print';if(a.sheet!==sa||sa.disabled)throw Error('media changed flag or identity');true"#);
        realm.select_stylesheet_set("A").unwrap();
        check(&mut engine,"if(!sa.disabled||sb.disabled)throw Error('selected actual title');true");
        realm.change_preferred_stylesheet_set("B").unwrap();
        check(&mut engine,r#"if(!sa.disabled||sb.disabled)throw Error('preferred overrode last selection');
            globalThis.c=document.createElement('style');c.title='B';c.textContent='p{opacity:.5}';document.head.append(c);if(!c.sheet.disabled)throw Error('new sheet ignored last name');true"#);
        realm.select_stylesheet_set("").unwrap();
        check(&mut engine,"if(!sa.disabled||!sb.disabled||!c.sheet.disabled)throw Error('empty selection');true");
    }
    #[test]
    fn specification_stylesheet_sets_default_headers_meta_insertion_and_shadow_titles() {
        let mut engine=Engine::new();let realm=crate::install(engine.ctx(),"<head></head><body></body>",256).unwrap();
        realm.set_stylesheet_response_headers(&[("Default-Style".into(),"A".into()),("default-style".into(),"B".into())]).unwrap();
        check(&mut engine,r#"globalThis.a=document.createElement('style');a.title='A';a.textContent='body{color:red}';document.head.append(a);
            globalThis.b=document.createElement('style');b.title='B';b.textContent='body{color:green}';document.head.append(b);
            if(!a.sheet.disabled||b.sheet.disabled)throw Error('ordered headers');
            globalThis.meta=document.createElement('meta');meta.httpEquiv='default-style';meta.content='A';document.head.append(meta);true"#);
        turn(&mut engine,&realm);
        check(&mut engine,r#"if(a.sheet.disabled||!b.sheet.disabled)throw Error('inserted default style');meta.content='B';true"#);
        turn(&mut engine,&realm);
        check(&mut engine,r#"if(a.sheet.disabled||!b.sheet.disabled)throw Error('attribute mutation reran insertion pragma');
            globalThis.host=document.createElement('div');document.body.append(host);globalThis.shadow=host.attachShadow({mode:'open'});
            globalThis.s=document.createElement('style');s.title='Unused';s.textContent='div{color:green}';shadow.append(s);
            if(s.sheet.title!==null||s.sheet.disabled)throw Error('shadow title enters document set');true"#);
    }
    #[test]
    fn specification_stylesheet_sets_link_response_reuse_explicit_alternate_and_new_sheet_flags() {
        let mut engine=Engine::new();let realm=crate::install(engine.ctx(),"<head></head><body></body>",256).unwrap();realm.set_document_url("https://sets.test/page");
        let provider=Rc::new(ManualProvider::default());realm.set_stylesheet_resource_loader(provider.clone());
        check(&mut engine,r#"globalThis.link=document.createElement('link');link.rel='alternate stylesheet';link.title='A';link.href='/one.css';link.disabled=false;document.head.append(link);true"#);
        turn(&mut engine,&realm);assert_eq!(provider.requests.borrow().len(),1);provider.complete(1,"body{color:green}");turn(&mut engine,&realm);
        assert_eq!(realm.stylesheet_links.preferred_title.borrow().as_str(),"A","explicitly enabled alternate creates non-alternate sheet");
        check(&mut engine,r#"globalThis.original=link.sheet;original.disabled=true;link.title='B';link.media='print';true"#);turn(&mut engine,&realm);
        assert_eq!(provider.requests.borrow().len(),1,"title/media reuse the obtained response");
        check(&mut engine,"if(link.sheet!==original||!original.disabled||original.title!=='B'||original.media.mediaText!=='print')throw Error('owner metadata or disabled flag');link.href='/two.css';true");
        turn(&mut engine,&realm);assert_eq!(provider.requests.borrow().len(),2);provider.complete(2,"body{color:red}");turn(&mut engine,&realm);
        check(&mut engine,"if(link.sheet===original||original.ownerNode!==null||!original.disabled||!link.sheet.disabled)throw Error('replacement selection and detached flag');true");
        realm.select_stylesheet_set("B").unwrap();check(&mut engine,"if(link.sheet.disabled||!original.disabled)throw Error('detached source affected by selection');true");
    }
    #[test]
    fn specification_xml_stylesheet_generations_prolog_media_and_detached_graph_identity() {
        let mut engine=Engine::new();
        let realm=crate::install_live_xml(engine.ctx(),r#"<html xmlns="http://www.w3.org/1999/xhtml"><head/><body><p id="target">text</p></body></html>"#,192,crate::XmlDocumentType::Xhtml).unwrap();
        assert!(matches!(realm.next_document_parser_step(engine.ctx()).unwrap(),DocumentParserStep::Complete));
        realm.set_document_url("https://xml-style.test/page.xhtml");
        let provider=Rc::new(ManualProvider::default());realm.set_stylesheet_resource_loader(provider.clone());
        check(&mut engine,r#"globalThis.pi=document.createProcessingInstruction('xml-stylesheet','href="/one.css?x=1&amp;y=2" title="Preferred" media="screen" charset="windows-1250"');
            globalThis.events=[];pi.addEventListener('load',()=>events.push('load'));pi.addEventListener('error',()=>events.push('error'));
            document.insertBefore(pi,document.documentElement);if(pi.sheet!==null)throw Error('pending PI sheet');true"#);
        turn(&mut engine,&realm);
        assert_eq!(provider.requests.borrow().len(),1);
        assert_eq!(provider.requests.borrow()[0].url,"https://xml-style.test/one.css?x=1&y=2");
        assert_eq!(provider.requests.borrow()[0].environment_encoding,"windows-1250");
        provider.complete(1,"p{color:green}");
        if let Some(Ok(response))=provider.ready.borrow_mut().get_mut(&1) {response.location_url="https://xml-style.test/redirected.css".into();response.source.url=Arc::from("https://xml-style.test/redirected.css");}
        turn(&mut engine,&realm);
        check(&mut engine,r#"globalThis.old=pi.sheet;globalThis.authoredData=pi.data;
            if(!old||old.ownerNode!==pi||old.href!=='https://xml-style.test/one.css?x=1&y=2'||old.title!=='Preferred'||old.media.mediaText!=='screen'||document.styleSheets[0]!==old)throw Error('PI sheet owner or metadata');
            old.media.mediaText='print';if(pi.data!==authoredData||old.media.mediaText!=='print')throw Error('sheet media rewrote PI data');
            pi.data='href="/two.css" title="Replacement" media="all"';if(pi.sheet!==null||old.ownerNode!==null||old.cssRules.length!==1||old.title!=='Preferred'||old.media.mediaText!=='print')throw Error('immediate PI replacement or retained metadata');true"#);
        turn(&mut engine,&realm);assert_eq!(provider.requests.borrow().len(),2);
        check(&mut engine,"pi.data=pi.data;true");
        assert!(provider.cancelled.borrow().contains(&2),"authored equal data replacement synchronously cancels the actual prior fetch");
        turn(&mut engine,&realm);assert_eq!(provider.requests.borrow().len(),3);
        check(&mut engine,"document.appendChild(pi);if(pi.sheet!==null)throw Error('epilog sheet');true");
        assert!(provider.cancelled.borrow().contains(&3));turn(&mut engine,&realm);
        assert_eq!(provider.requests.borrow().len(),3,"epilog cannot obtain a sheet");
        check(&mut engine,"document.insertBefore(pi,document.documentElement);true");turn(&mut engine,&realm);
        assert_eq!(provider.requests.borrow().len(),4);
        provider.complete(4,"p{color:red}");turn(&mut engine,&realm);
        check(&mut engine,r#"if(pi.sheet===old||pi.sheet.ownerNode!==pi||pi.sheet.title!=='Replacement'||events.join()!=='load,load')throw Error('reentry generation');globalThis.reentered=pi.sheet;document.removeChild(document.documentElement);document.insertBefore(document.createElement('root'),pi);document.removeChild(document.documentElement);if(reentered.ownerNode!==null)throw Error('prolog membership ABA revived the old sheet');pi.data='href="/bad.css" x="1" x="2"';if(pi.sheet!==null||document.styleSheets.length!==0)throw Error('duplicate pseudo-attribute admitted');true"#);
        turn(&mut engine,&realm);assert_eq!(provider.requests.borrow().len(),4,"invalid pseudo-attributes cannot fetch");
        check(&mut engine,r#"globalThis.other=document.createProcessingInstruction('not-stylesheet','href="/bad.css"');document.insertBefore(other,document.documentElement);if(other.sheet!==null)throw Error('non-stylesheet LinkStyle');true"#);
        assert!(!realm.stylesheet_resources_pending());
    }
    #[test]
    fn specification_xml_stylesheet_adoption_rebinds_settings_cancels_fetch_and_enforces_cors() {
        let mut engine=Engine::new();let source=crate::install_live_xml(engine.ctx(),r#"<html xmlns="http://www.w3.org/1999/xhtml"><head/><body/></html>"#,192,crate::XmlDocumentType::Xhtml).unwrap();
        assert!(matches!(source.next_document_parser_step(engine.ctx()).unwrap(),DocumentParserStep::Complete));
        source.set_document_url("https://source.test/page.xhtml");let first=Rc::new(ManualProvider::default());source.set_stylesheet_resource_loader(first.clone());
        check(&mut engine,r#"globalThis.pi=document.createProcessingInstruction('xml-stylesheet','href="relative.css" media="all"');document.insertBefore(pi,document.documentElement);globalThis.foreign=new DOMParser().parseFromString('<root/>','application/xml');true"#);
        turn(&mut engine,&source);assert_eq!(first.requests.borrow().len(),1);
        let document=match engine.eval_value("foreign") {Ok(Ok(value))=>value,_=>panic!("foreign XML Document guard")};
        let target=engine.ctx().with_instance::<DomDocument,_>(&document,|document|document.realm.clone()).unwrap();
        target.set_document_url("https://target.test/sub/document.xml");let second=Rc::new(ManualProvider::default());target.set_stylesheet_resource_loader(second.clone());
        check(&mut engine,"foreign.insertBefore(foreign.adoptNode(pi),foreign.documentElement);true");
        assert!(first.cancelled.borrow().contains(&1),"adoption aborts old settings request");
        turn(&mut engine,&target);
        assert_eq!(second.requests.borrow()[0].url,"https://target.test/sub/relative.css");
        assert_eq!(second.requests.borrow()[0].document_url,"https://target.test/sub/document.xml");
        assert!(!second.requests.borrow()[0].parser_inserted,"adoption is not a new parser-created instruction");
        second.complete(1,"root{color:green}");
        if let Some(Ok(response))=second.ready.borrow_mut().get_mut(&1) {response.origin_clean=false;}
        turn(&mut engine,&target);
        check(&mut engine,"if(pi.ownerDocument!==foreign||pi.sheet.ownerNode!==pi||foreign.styleSheets[0]!==pi.sheet)throw Error('actual adopted owner: document='+(pi.ownerDocument===foreign)+', owner='+(pi.sheet.ownerNode===pi)+', sheet='+(foreign.styleSheets[0]===pi.sheet));let denied=false;try{pi.sheet.cssRules}catch(error){denied=error.name==='SecurityError'}if(!denied)throw Error('CORS-clean flag was lost');true");
        assert!(!source.stylesheet_resources_pending());assert!(!target.stylesheet_resources_pending());
    }

    #[test]
    fn specification_xml_stylesheet_parser_critical_imports_block_until_actual_completion() {
        let mut engine=Engine::new();
        let realm=crate::install_live_xml(engine.ctx(),r#"<?xml-stylesheet href="/blocking.css" media="screen"?><html xmlns="http://www.w3.org/1999/xhtml"><head><script>globalThis.observed=getComputedStyle(document.documentElement).zIndex;</script></head><body/></html>"#,192,crate::XmlDocumentType::Xhtml).unwrap();
        realm.set_document_url("https://xml-style.test/parser.xhtml");let provider=Rc::new(ManualProvider::default());realm.set_stylesheet_resource_loader(provider.clone());
        assert!(matches!(realm.next_document_parser_step(engine.ctx()).unwrap(),DocumentParserStep::BlockedOnStylesheets));
        assert!(realm.script_blocking_stylesheets_pending());
        // HTML's render-blocking mechanism requires text/html; XML's
        // stylesheet dependency still blocks the actual parser script.
        assert!(!realm.rendering_blocked());
        assert_eq!(provider.requests.borrow().len(),1);assert!(provider.requests.borrow()[0].parser_inserted);
        let request=provider.requests.borrow()[0].clone();
        let source=lumen_html::stylesheet_loading::load_with_context(&request.url,"UTF-8",&mut Default::default(),
            lumen_html::stylesheet_loading::FetchContext{referrer:&request.referrer,referrer_policy:request.referrer_policy,root:true},
            &mut |url,_|Ok(Some(lumen_html::stylesheet_loading::Response{final_url:url.into(),content_type:Some("text/css".into()),bytes:if url.ends_with("blocking.css") {b"@import 'child.css';html{z-index:3}".to_vec()}else{b"html{color:green}".to_vec()},referrer_policy:None}))).unwrap().unwrap();
        provider.ready.borrow_mut().insert(1,Ok(StylesheetResponse{critical_failed:false,failed_import_paths:Vec::new(),source_contexts:Vec::new(),location_url:request.url,content_type:Some("text/css".into()),source,
            origin_clean:true,start_time:0.,end_time:1.,encoded_body_size:0,timing_allow:false,origin_metadata:Arc::from([]),violations:Vec::new(),timings:Vec::new()}));
        turn(&mut engine,&realm);
        assert!(!realm.script_blocking_stylesheets_pending());
        let DocumentParserStep::Script(script)=realm.next_document_parser_step(engine.ctx()).unwrap() else{panic!("XML stylesheet blocked script missing")};
        realm.execute_document_parser_script(engine.ctx(),script.node).unwrap();
        assert!(matches!(realm.next_document_parser_step(engine.ctx()).unwrap(),DocumentParserStep::Complete));
        check(&mut engine,"if(observed!=='3'||document.styleSheets.length!==1||document.styleSheets[0].cssRules[0].styleSheet.cssRules.length!==1)throw Error('XML critical graph or parser order: observed='+observed+', sheets='+document.styleSheets.length+', child rules='+document.styleSheets[0]?.cssRules[0]?.styleSheet?.cssRules.length);true");
    }
    #[test]
    fn specification_cssom_import_jobs_preserve_root_edits_cancel_deleted_occurrences_and_retry_new_rules() {
        let mut engine=Engine::new();let realm=crate::install(engine.ctx(),"<!doctype html><body><p class=box>x</p></body>",256).unwrap();
        realm.set_document_url("https://occurrence.test/page");let provider=Rc::new(ManualProvider::default());realm.set_stylesheet_resource_loader(provider.clone());
        check(&mut engine,"globalThis.s=document.createElement('style');s.textContent='p{height:3px}';document.head.append(s);globalThis.root=s.sheet;globalThis.rule=root.cssRules[0];globalThis.events=[];s.onload=()=>events.push('load');s.onerror=()=>events.push('error');true");
        realm.queue_stylesheet_tasks(engine.ctx()).unwrap();
        check(&mut engine,"rule.style.height='17px';root.insertRule('html{color:green}',0);true");
        assert!(scheduling::run_tasks(&mut engine,64).is_empty(),"edited import-free completion cannot throw or overwrite root");
        check(&mut engine,"if(s.sheet!==root||root.cssRules[1]!==rule||rule.style.height!=='17px'||events.join()!=='load')throw Error('import-free identity completion');root.insertRule('@import \"/same.css\"',0);globalThis.first=root.cssRules[0];true");
        turn(&mut engine,&realm);assert_eq!(provider.requests.borrow().len(),1);assert!(provider.requests.borrow()[0].import_request);
        check(&mut engine,"root.deleteRule(0);root.insertRule('@import \"/same.css\"',0);globalThis.second=root.cssRules[0];if(second===first)throw Error('new occurrence identity');true");
        turn(&mut engine,&realm);assert_eq!(provider.cancelled.borrow().as_slice(),&[1]);assert_eq!(provider.requests.borrow().len(),2);
        provider.complete(2,"p{width:23px}");turn(&mut engine,&realm);turn(&mut engine,&realm);
        check(&mut engine,"if(s.sheet!==root||root.cssRules[0]!==second||second.styleSheet.cssRules.length!==1||root.cssRules[2]!==rule||rule.style.height!=='17px')throw Error('import completion replaced current rules');if(events.join()!=='load')throw Error('CSSOM import fabricated element load event');root.insertRule('@import \"/again.css\"',0);true");
        turn(&mut engine,&realm);assert!(realm.stylesheet_resources_pending());
        let _retired=realm.retire_browsing_context_group(engine.ctx());assert!(!realm.stylesheet_resources_pending());
        assert_eq!(provider.cancelled.borrow().as_slice(),&[1,3]);
    }
    #[test]
    fn specification_cssom_import_completion_updates_origin_clean_provenance_on_retained_pending_roots() {
        let mut engine=Engine::new();let realm=crate::install(engine.ctx(),"<!doctype html><body></body>",128).unwrap();
        realm.set_document_url("https://origin.test/page");let provider=Rc::new(ManualProvider::default());realm.set_stylesheet_resource_loader(provider.clone());
        check(&mut engine,"globalThis.s=document.createElement('style');s.textContent='p{color:green}';document.head.append(s);globalThis.root=s.sheet;true");turn(&mut engine,&realm);
        check(&mut engine,"root.insertRule('@import \"https://cross.test/child.css\"',0);true");turn(&mut engine,&realm);
        provider.complete(1,"p{width:23px}");if let Some(Ok(response))=provider.ready.borrow_mut().get_mut(&1) {response.origin_clean=false;response.origin_metadata=Arc::from([(Arc::from("https://cross.test/child.css"),false)]);}
        turn(&mut engine,&realm);turn(&mut engine,&realm);
        check(&mut engine,"globalThis.child=root.cssRules[0].styleSheet;let blocked=false;try{child.cssRules}catch(e){blocked=e.name==='SecurityError'}if(!blocked||root.cssRules.length!==2)throw Error('late import origin provenance');root.deleteRule(0);blocked=false;try{child.cssRules}catch(e){blocked=e.name==='SecurityError'}if(!blocked)throw Error('retained detached import became origin-clean');true");
    }

    #[test]
    fn specification_pending_inline_imports_merge_occurrences_after_root_edits_and_cancel_all_removed_edges() {
        let mut engine=Engine::new();let realm=crate::install(engine.ctx(),"<!doctype html><body></body>",256).unwrap();
        realm.set_document_url("https://occurrence.test/page");let provider=Rc::new(ManualProvider::default());realm.set_stylesheet_resource_loader(provider.clone());
        check(&mut engine,"globalThis.s=document.createElement('style');s.textContent='@import \"/old.css\";p{height:3px}';document.head.append(s);globalThis.root=s.sheet;globalThis.rule=root.cssRules[1];globalThis.events=[];s.onload=()=>events.push('load');true");
        turn(&mut engine,&realm);assert_eq!(provider.requests.borrow().len(),1);
        check(&mut engine,"rule.style.height='19px';root.insertRule('@import \"/new.css\"',0);root.deleteRule(1);true");
        turn(&mut engine,&realm);assert_eq!(provider.cancelled.borrow().as_slice(),&[1]);assert_eq!(provider.requests.borrow().len(),2);
        check(&mut engine,"if(events.length)throw Error('terminal event before critical new import');true");
        provider.complete(2,"p{width:29px}");turn(&mut engine,&realm);turn(&mut engine,&realm);
        check(&mut engine,"if(s.sheet!==root||root.cssRules[1]!==rule||rule.style.height!=='19px'||root.cssRules[0].styleSheet.cssRules.length!==1||events.join()!=='load')throw Error('occurrence critical completion');true");
        // A supports-false import has no child sheet, but editing its parent
        // before processing completes must still retain the current CSSOM root.
        check(&mut engine,"s.textContent='@import \"/ignored.css\" supports(unknown:value);';globalThis.skipped=s.sheet;true");
        turn(&mut engine,&realm);let request=provider.requests.borrow().last().unwrap().clone();
        check(&mut engine,"skipped.insertRule('html{transition:normal}',1);true");
        let source=lumen_html::css::StylesheetSource{disabled:false, url:Arc::from(request.url.as_str()),text:request.inline_source.unwrap(),imports:lumen_html::css::imports(r#"@import "/ignored.css" supports(unknown:value);"#).unwrap().into_iter().map(|rule|lumen_html::css::LoadedImport{rule,source:None}).collect()};
        provider.ready.borrow_mut().insert(3,Ok(StylesheetResponse{critical_failed:false,failed_import_paths:Vec::new(),source_contexts:Vec::new(),location_url:request.url,content_type:None,source,origin_clean:true,start_time:0.,end_time:1.,encoded_body_size:0,timing_allow:false,origin_metadata:Arc::from([]),violations:Vec::new(),timings:Vec::new()}));
        turn(&mut engine,&realm);check(&mut engine,"if(s.sheet!==skipped||skipped.cssRules.length!==2||skipped.cssRules[0].styleSheet!==null)throw Error('unloaded import root edit');true");
    }

    #[test]
    fn specification_stylesheet_generations_live_metadata_and_detached_sheet_flags() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<!doctype html><link id=sheet rel=stylesheet href=/first.css type=' text/css;charset=utf-8 '><link id=unsupported rel=stylesheet href=/bad.css type=text/plain>",128).unwrap();
        realm.set_document_url("https://styles.test/page");
        let provider=Rc::new(ManualProvider::default());realm.set_stylesheet_resource_loader(provider.clone());
        check(&mut engine,"globalThis.link=document.getElementById('sheet');globalThis.events=[];link.onload=()=>events.push('load');link.onerror=()=>events.push('error');if(link.sheet!==null)throw Error('unfetched sheet');true");
        turn(&mut engine,&realm);
        assert_eq!(provider.requests.borrow().len(),1,"supported parameterized hint only");
        provider.complete(1,"div{width:10px}");turn(&mut engine,&realm);
        check(&mut engine,"link.type='text/css; charset=windows-1250';true");turn(&mut engine,&realm);
        assert_eq!(provider.requests.borrow().len(),1,"matching obtained MIME does not refetch");
        check(&mut engine,"globalThis.old=link.sheet;if(events.join()!=='load'||old.cssRules.length!==1)throw Error('terminal load');old.disabled=true;link.href='/second.css';true");
        turn(&mut engine,&realm);
        check(&mut engine,"if(link.sheet!==old||!old.disabled)throw Error('pending replacement discarded old association');true");
        provider.complete(2,"div{width:20px}");turn(&mut engine,&realm);
        check(&mut engine,"if(link.sheet===old||old.ownerNode!==null||!old.disabled||link.sheet.disabled)throw Error('replacement isolation');old.disabled=false;old.disabled=true;if(link.sheet.disabled)throw Error('detached setter affected replacement');globalThis.current=link.sheet;link.media='screen';link.title='named';if(current.media.mediaText!=='screen'||current.title!=='named')throw Error('live metadata');current.media.mediaText='print';if(link.media!=='print'||link.sheet!==current)throw Error('media reference');true");
        turn(&mut engine,&realm);assert_eq!(provider.requests.borrow().len(),2,"metadata does not refetch");
        check(&mut engine,"link.href='/third.css';true");turn(&mut engine,&realm);
        check(&mut engine,"link.href='/fourth.css';true");turn(&mut engine,&realm);
        assert!(provider.cancelled.borrow().contains(&3));
        provider.ready.borrow_mut().insert(4,Err(StylesheetFailure{message:"wrong response MIME".into(),violations:Vec::new(),timings:Vec::new()}));
        turn(&mut engine,&realm);
        check(&mut engine,"if(link.sheet!==null||events.join()!=='load,load,error')throw Error('failed replacement completion');true");
        assert!(!realm.stylesheet_resources_pending());
    }
    #[test]
    fn specification_stylesheet_preferred_set_follows_actual_addition_and_explicit_enable() {
        let mut engine=Engine::new();let realm=crate::install(engine.ctx(),"<!doctype html><body></body>",128).unwrap();
        realm.set_document_url("https://styles.test/page");let provider=Rc::new(ManualProvider::default());realm.set_stylesheet_resource_loader(provider.clone());
        check(&mut engine,"globalThis.green=document.createElement('link');green.rel='stylesheet';green.title='green';green.href='/green.css';document.head.append(green);true");turn(&mut engine,&realm);
        provider.complete(1,"div{color:green}");turn(&mut engine,&realm);
        check(&mut engine,"globalThis.red=document.createElement('link');red.rel='stylesheet';red.title='red';red.href='/red.css';document.head.insertBefore(red,green);globalThis.inline=document.createElement('style');inline.title='inline';inline.textContent='div{color:black}';document.head.insertBefore(inline,red);true");turn(&mut engine,&realm);
        provider.complete(2,"div{color:red}");turn(&mut engine,&realm);
        check(&mut engine,"if(green.sheet.disabled||!red.sheet.disabled)throw Error('tree reordering changed first added preference');red.disabled=false;if(red.disabled||red.sheet.disabled)throw Error('no-op false did not explicitly enable');true");
        turn(&mut engine,&realm);assert_eq!(provider.requests.borrow().len(),2);
    }    #[test]
    fn specification_stylesheet_parser_pause_is_distinct_from_eof_and_resumes_after_terminal_task() {
        let mut engine=Engine::new();let realm=crate::install_live_html(engine.ctx(),
            "<!doctype html><link id=sheet rel=stylesheet href=/blocking.css><script>globalThis.ran=true</script><p id=tail>tail</p>",128).unwrap();
        realm.set_document_url("https://styles.test/page");let provider=Rc::new(ManualProvider::default());realm.set_stylesheet_resource_loader(provider.clone());
        assert!(matches!(realm.next_document_parser_step(engine.ctx()).unwrap(),DocumentParserStep::BlockedOnStylesheets));
        assert!(realm.has_live_document_parser());assert!(realm.script_blocking_stylesheets_pending());
        assert!(realm.rendering_blocked());
        check(&mut engine,"globalThis.frame=false;requestAnimationFrame(()=>frame=true);true");
        assert!(scheduling::run_animation_frame(&mut engine).is_empty());
        check(&mut engine,"if(frame)throw Error('rendering ran while parser stylesheet blocked');true");
        check(&mut engine,"if(globalThis.ran||document.getElementById('tail'))throw Error('parser advanced during stylesheet wait');document.getElementById('sheet').onload=()=>globalThis.loaded=true;true");
        provider.complete(1,"p{color:green}");realm.queue_stylesheet_tasks(engine.ctx()).unwrap();
        assert!(realm.stylesheet_resources_pending(),"terminal queued task retains load lease");
        assert!(realm.script_blocking_stylesheets_pending(),"script lease survives until load dispatch");
        assert!(scheduling::run_tasks(&mut engine,64).is_empty());
        assert!(!realm.script_blocking_stylesheets_pending());
        let DocumentParserStep::Script(script)=realm.next_document_parser_step(engine.ctx()).unwrap() else {panic!("pending script was lost")};
        check(&mut engine,"if(!globalThis.loaded||globalThis.ran)throw Error('terminal task ordering');true");
        realm.execute_document_parser_script(engine.ctx(),script.node).unwrap();
        assert!(matches!(realm.next_document_parser_step(engine.ctx()).unwrap(),DocumentParserStep::Complete));
        assert!(!realm.rendering_blocked());
        assert!(scheduling::run_animation_frame(&mut engine).is_empty());
        check(&mut engine,"if(!frame||!ran||!document.getElementById('tail'))throw Error('parser/rendering did not resume');true");
    }
    #[test]
    fn specification_stylesheet_retirement_cancels_actual_pending_owners_without_late_events() {
        let mut engine=Engine::new();let _parent=crate::install(engine.ctx(),"<main></main>",128).unwrap();
        let child=engine.ctx().create_host_realm();let provider=Rc::new(ManualProvider::default());
        let (weak_document,weak_global,retired)=engine.ctx().with_host_realm(&child,|ctx| {
            let realm=crate::install(ctx,"<!doctype html><link rel=stylesheet href=/pending.css>",128).unwrap();
            realm.set_document_url("https://styles.test/page");realm.set_stylesheet_resource_loader(provider.clone());
            realm.queue_stylesheet_tasks(ctx).unwrap();assert!(realm.stylesheet_resources_pending());
            let global=ctx.global_object();
            let weak_global=ctx.weak_value(&global).expect("stylesheet origin global");
            let retired=realm.retire_browsing_context_group(ctx);
            assert!(!realm.stylesheet_resources_pending(),"actual pending load leases drop on retirement");
            (Rc::downgrade(&realm),weak_global,retired)
        }).expect("stylesheet child realm");
        assert_eq!(provider.cancelled.borrow().as_slice(),&[1]);
        for handle in retired {engine.ctx().dispose_host_realm(&handle).expect("retired stylesheet realm");}
        drop(child);engine.collect_garbage();
        assert!(weak_global.upgrade().is_none(),"cancelled provider owns only plain request data");
        assert!(weak_document.upgrade().is_none(),"pending stylesheet cannot pin retired document");
    }
    #[test]
    fn specification_inline_shadow_styles_publish_at_connection_before_same_script_cssom() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<!doctype html><body></body>",512).unwrap();
        realm.set_document_url("https://inline.test/host");
        let provider=Rc::new(ManualProvider::default());
        realm.set_stylesheet_resource_loader(provider.clone());
        check(&mut engine,r#"(() => {
            const expect=(ok,message)=>{if(!ok)throw Error(message)};
            getComputedStyle(document.body).color; // Warm the actual old rule index.
            customElements.define('inline-sync-host',class extends HTMLElement {
                constructor(){
                    super();this.i=this.attachInternals();
                    this.s=document.createElement('style');
                    this.s.textContent=':host{color:red;border:3px solid blue}:host(:state(active)){color:green}';
                    this.attachShadow({mode:'open'}).append(this.s);
                }
            });
            const h=document.createElement('inline-sync-host');
            expect(h.s.sheet===null,'disconnected style has no associated sheet');
            document.body.append(h);
            expect(getComputedStyle(h).color==='rgb(255, 0, 0)','connected constructor style color in same script');
            expect(getComputedStyle(h).borderTopStyle==='solid','connected host border in same script');
            const first=h.s.sheet;
            expect(first!==null && first.ownerNode===h.s,'synchronous native root association');
            h.i.states.add('active');
            expect(getComputedStyle(h).color==='rgb(0, 128, 0)','state mutation uses current admitted scoped rules');
            h.s.textContent=':host{color:blue;border:2px dashed green}';
            expect(getComputedStyle(h).color==='rgb(0, 0, 255)','children change publishes new source before CSSOM');
            expect(getComputedStyle(h).borderTopStyle==='dashed','children change preserves actual scoped host matching');
            expect(first.ownerNode===null && h.s.sheet!==first,'update removes the old associated sheet');
            const second=h.s.sheet;
            h.remove();
            expect(h.s.sheet===null && second.ownerNode===null,'host disconnection removes descendant sheet');
            document.body.append(h);
            expect(getComputedStyle(h).color==='rgb(0, 0, 255)','equal-byte reconnection reapplies the actual sheet');
            const doc=document.implementation.createHTMLDocument('owner');
            doc.adoptNode(h);
            expect(h.s.sheet===null,'adoption detached shadow source');
            document.body.append(h);
            expect(getComputedStyle(h).color==='rgb(0, 0, 255)','adopted shadow styles use new document owner');
            globalThis.syncStyleHost=h;
            return true;
        })()"#);
        assert!(provider.requests.borrow().is_empty(),"synchronous inline publication starts no transport or terminal tasks");
        realm.set_content_security_policy_headers(&[("Content-Security-Policy".into(),"style-src 'none'".into())]).unwrap();
        check(&mut engine,"syncStyleHost.s.textContent=':host{color:red}';if(syncStyleHost.s.sheet!==null||getComputedStyle(syncStyleHost).color==='rgb(255, 0, 0)')throw Error('new source bypassed captured CSP');true");
        assert!(provider.requests.borrow().is_empty());
    }

    #[test]
    fn specification_inline_stylesheets_create_synchronously_keep_import_identity_and_capture_csp_at_update() {
        let mut engine=Engine::new();let realm=crate::install(engine.ctx(),"<!doctype html><style id=sheet>@import '/child.css';p{color:green}</style><body><p>text</p></body>",192).unwrap();
        realm.set_document_url("https://inline.test/doc.html");let provider=Rc::new(ManualProvider::default());realm.set_stylesheet_resource_loader(provider.clone());
        check(&mut engine,"globalThis.s=document.getElementById('sheet');globalThis.events=[];s.onload=()=>events.push('load');s.onerror=()=>events.push('error');globalThis.root=s.sheet;globalThis.rule=root.cssRules[1];if(!root||root.href!==null||document.styleSheets[0]!==root)throw Error('immediate inline root');true");
        turn(&mut engine,&realm);assert_eq!(provider.requests.borrow().len(),1);
        let request=provider.requests.borrow()[0].clone();
        assert!(request.inline_source.is_some(),"no fake network request for the Unicode root");
        let source=lumen_html::stylesheet_loading::load_inline_with_context(&request.url,request.inline_source.unwrap(),"UTF-8",&mut Default::default(),
            lumen_html::stylesheet_loading::FetchContext{referrer:&request.referrer,referrer_policy:request.referrer_policy,root:true},
            &mut |url,context|{assert!(!context.root);Ok(Some(lumen_html::stylesheet_loading::Response{final_url:url.into(),content_type:Some("text/css".into()),bytes:b"p{width:13px}".to_vec(),referrer_policy:None}))}).unwrap();
        provider.ready.borrow_mut().insert(1,Ok(StylesheetResponse{critical_failed:false,failed_import_paths:Vec::new(),source_contexts:Vec::new(),location_url:request.url,content_type:None,source,origin_clean:true,start_time:0.,end_time:1.,encoded_body_size:0,timing_allow:false,origin_metadata:Arc::from([]),violations:Vec::new(),timings:Vec::new()}));
        turn(&mut engine,&realm);
        check(&mut engine,"if(s.sheet!==root||root.cssRules[1]!==rule||root.cssRules[0].styleSheet.cssRules.length!==1||events.join()!=='load')throw Error('critical completion replaced root identity');s.type='text/plain';s.media='print';s.title='named';if(s.sheet!==root||root.media.mediaText!=='print'||root.title!=='named')throw Error('metadata recreated root');root.media.mediaText='screen';if(s.media!=='screen')throw Error('live media reference');true");
        turn(&mut engine,&realm);assert_eq!(provider.requests.borrow().len(),1,"metadata never refetches inline imports");
        check(&mut engine,"s.type='text/css';if(s.sheet!==root)throw Error('type mutation changed association');globalThis.violations=[];document.addEventListener('securitypolicyviolation',e=>violations.push(e.effectiveDirective));true");
        realm.set_content_security_policy_headers(&[("Content-Security-Policy".into(),"style-src 'none'".into())]).unwrap();
        check(&mut engine,"if(s.sheet!==root)throw Error('later policy retroactively replaced root');s.textContent='p{color:red}';if(s.sheet!==null||root.ownerNode!==null||document.styleSheets.length!==0)throw Error('update did not remove old association');s.type='text/css';s.nonce='allowed';if(s.sheet!==null)throw Error('attribute alone recreated invalid association');true");
        turn(&mut engine,&realm);assert_eq!(provider.requests.borrow().len(),1,"blocked inline source cannot start critical network fetch");
        check(&mut engine,"globalThis.off=document.createElement('style');off.textContent='p{color:red}';if(off.sheet!==null)throw Error('disconnected stylesheet');true");
    }
    #[test]
    fn specification_inline_stylesheet_no_imports_use_no_transport_and_detach_on_equal_byte_reconnection() {
        let mut engine=Engine::new();let realm=crate::install(engine.ctx(),"<!doctype html><body></body>",128).unwrap();realm.set_document_url("https://inline.test/doc");
        let provider=Rc::new(ManualProvider::default());realm.set_stylesheet_resource_loader(provider.clone());
        check(&mut engine,"globalThis.s=document.createElement('style');s.textContent='p{color:green}';document.head.append(s);globalThis.first=s.sheet;globalThis.events=[];s.onload=()=>events.push('load');if(first.cssRules.length!==1)throw Error('rules were not immediately available');true");
        turn(&mut engine,&realm);assert!(provider.requests.borrow().is_empty(),"no-I/O inline processing does not admit a thread ticket");
        check(&mut engine,"if(events.join()!=='load')throw Error('no-import terminal event');s.remove();document.head.append(s);globalThis.second=s.sheet;if(second===first||first.ownerNode!==null||second.ownerNode!==s)throw Error('equal-byte ABA revived old sheet');first.disabled=true;if(second.disabled)throw Error('detached disabled leaked');true");
        turn(&mut engine,&realm);assert!(provider.requests.borrow().is_empty());
        check(&mut engine,"if(events.join()!=='load,load')throw Error('reconnection terminal event');true");
    }
    #[test]
    fn specification_inline_parser_pop_defers_open_updates_but_not_processed_style_scripts() {
        let mut engine=Engine::new();let realm=crate::install_live_html(engine.ctx(),"<!doctype html><script>globalThis.sync=true;globalThis.loads=[]</script><style id=sheet onload='loads.push(sync)'>p{color:green}</style><script>sync=false</script><body></body>",192).unwrap();
        realm.set_document_url("https://inline.test/parser");let provider=Rc::new(ManualProvider::default());realm.set_stylesheet_resource_loader(provider.clone());
        let DocumentParserStep::Script(first)=realm.next_document_parser_step(engine.ctx()).unwrap() else{panic!("initial parser script")};
        realm.execute_document_parser_script(engine.ctx(),first.node).unwrap();
        let DocumentParserStep::Script(second)=realm.next_document_parser_step(engine.ctx()).unwrap() else{panic!("processed import-free style delayed following script")};
        assert!(!realm.script_blocking_stylesheets_pending());
        check(&mut engine,"if(loads.length)throw Error('synchronous load event');true");
        realm.execute_document_parser_script(engine.ctx(),second.node).unwrap();
        assert!(scheduling::run_tasks(&mut engine,64).is_empty());
        check(&mut engine,"if(loads.length!==1||loads[0]!==false)throw Error('style pop event ordering');true");
        assert!(provider.requests.borrow().is_empty());
    }

    #[test]
    fn specification_inline_stylesheet_failed_critical_import_keeps_root_and_disabled_is_sheet_state() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<!doctype html><body></body>",192).unwrap();
        realm.set_document_url("https://inline.test/page");
        let provider=Rc::new(ManualProvider::default());realm.set_stylesheet_resource_loader(provider.clone());
        check(&mut engine,"globalThis.s=document.createElement('style');s.disabled=true;if(s.disabled||s.hasAttribute('disabled'))throw Error('no-sheet disabled must be a no-op');s.type='text/css; charset=utf-8';s.textContent='p{color:red}';document.head.append(s);if(s.sheet!==null)throw Error('style type accepts parameters');s.type='text/css';if(s.sheet!==null)throw Error('type alone triggered style update');s.textContent=\"@import '/missing.css';p{color:green}\";globalThis.root=s.sheet;globalThis.rule=root.cssRules[1];globalThis.events=[];s.onload=()=>events.push('load');s.onerror=()=>events.push('error');s.disabled=true;if(!s.disabled||!root.disabled||s.hasAttribute('disabled'))throw Error('disabled must set associated flag');s.disabled=false;true");
        turn(&mut engine,&realm);assert_eq!(provider.requests.borrow().len(),1);
        let request=provider.requests.borrow()[0].clone();
        let source=lumen_html::stylesheet_loading::load_inline_with_context(&request.url,request.inline_source.unwrap(),"UTF-8",&mut Default::default(),
            lumen_html::stylesheet_loading::FetchContext{referrer:&request.referrer,referrer_policy:request.referrer_policy,root:true},
            &mut |_,_|Ok(None)).unwrap();
        provider.ready.borrow_mut().insert(1,Ok(StylesheetResponse{critical_failed:true,failed_import_paths:vec![vec![0]],source_contexts:Vec::new(),location_url:request.url,content_type:None,source,
            origin_clean:true,start_time:0.,end_time:1.,encoded_body_size:0,timing_allow:false,origin_metadata:Arc::from([]),violations:Vec::new(),timings:Vec::new()}));
        turn(&mut engine,&realm);
        check(&mut engine,"if(s.sheet!==root||root.cssRules[1]!==rule||root.cssRules[0].styleSheet!==null||events.join()!=='error')throw Error('failed import discarded good root or fired load');true");
        assert!(!realm.stylesheet_resources_pending());
    }
    #[test]
    fn specification_inline_import_free_processing_does_not_require_a_network_provider() {
        let mut engine=Engine::new();let realm=crate::install(engine.ctx(),"<!doctype html><body></body>",128).unwrap();
        assert!(!realm.has_stylesheet_resource_loader());
        check(&mut engine,"globalThis.s=document.createElement('style');s.textContent='p{color:green}';globalThis.loads=0;s.onload=()=>loads++;document.head.append(s);if(!s.sheet||s.sheet.cssRules.length!==1||loads)throw Error('synchronous root without network');true");
        turn(&mut engine,&realm);
        check(&mut engine,"if(loads!==1)throw Error('import-free processing event requires network');true");
        check(&mut engine,"globalThis.host=document.createElement('div');document.body.append(host);globalThis.shadow=host.attachShadow({mode:'open'});globalThis.inside=document.createElement('style');inside.title='ignored-in-shadow';inside.textContent='p{color:red}';shadow.append(inside);if(inside.sheet.title!==null||inside.sheet.disabled)throw Error('shadow title entered document preferred set');true");
        turn(&mut engine,&realm);
        assert!(!realm.stylesheet_resources_pending());
    }

    #[test]
    fn specification_inline_stylesheet_admission_bounds_before_copy_and_releases_snapshot_charges() {
        let mut engine=Engine::new();let realm=crate::install(engine.ctx(),"<!doctype html><style id=sheet>p{color:red}</style><body></body>",128).unwrap();
        check(&mut engine,"globalThis.s=document.getElementById('sheet');globalThis.old=s.sheet;true");
        assert_eq!(realm.stylesheet_links.inline_bytes.get(),"p{color:red}".len());
        check(&mut engine,"s.textContent='p{color:green}';if(s.sheet===old)throw Error('root replacement');true");
        assert_eq!(realm.stylesheet_links.inline_bytes.get(),"p{color:green}".len(),"charge replacement rather than cumulative mutation history");
        let (owner,text)={let session=realm.session.borrow();let document=session.document();
            let owner=lumen_html::selector::get_element_by_id(document,document.root(),"sheet").unwrap().unwrap();let text=document.first_child(owner).unwrap().unwrap();(owner,text)};
        let oversized=" ".repeat(lumen_html::css::MAX_CSS_BYTES+1);
        realm.session.borrow_mut().document_mut().replace_data(text,&oversized).unwrap();
        assert!(realm.stylesheet_links.processing_failed.get(),"native observer preserves hard bound failure");
        let session=realm.session.borrow();
        assert!(matches!(script_loading::script_child_text_bounded(session.document(),owner,lumen_html::css::MAX_CSS_BYTES),Err(lumen_html::Error::LimitExceeded)));
        drop(session);
        assert_eq!(realm.stylesheet_links.inline_bytes.get(),"p{color:green}".len(),"failed admission cannot leak a new source charge");
        check(&mut engine,"s.remove();true");
        assert_eq!(realm.stylesheet_links.inline_bytes.get(),0,"disconnected source removed even if author retains old sheet");
        realm.stylesheet_links.retire();assert_eq!(realm.stylesheet_links.inline_bytes.get(),0);
    }

    #[test]
    fn specification_stylesheet_explicit_enable_cannot_bypass_owner_admission() {
        let mut engine=Engine::new();let realm=crate::install(engine.ctx(),"<!doctype html><body></body>",LINK_REQUEST_LIMIT*3+32).unwrap();
        let global=engine.ctx().global_object();engine.ctx().member_set(&global,"ownerLimit",Value::Num(LINK_REQUEST_LIMIT as f64)).unwrap_or_else(|_|panic!("guard owner limit global"));
        check(&mut engine,"globalThis.retained=document.createDocumentFragment();for(let i=0;i<ownerLimit;i++){let link=document.createElement('link');retained.append(link);link.disabled=false;}globalThis.extra=document.createElement('link');retained.append(extra);let threw=false;try{extra.disabled=false}catch(e){threw=e.name==='QuotaExceededError'}if(!threw)throw Error('explicit-enable bypassed owner bound');retained.firstChild.disabled=false;true");
        assert_eq!(realm.stylesheet_links.entries.borrow().len(),LINK_REQUEST_LIMIT);
        assert_eq!(realm.stylesheet_links.dirty.borrow().len(),LINK_REQUEST_LIMIT);
    }

}
