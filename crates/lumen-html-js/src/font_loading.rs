//! DOM Font Loading objects backed by the document's live CSS font records.
//!
//! Fetch policy and installed-font lookup belong to the embedder. Descriptor
//! parsing, matching, object identity, task settlement, and DOM set membership
//! remain in the native DOM adapter.
use super::*;
use lumen::embed::{
    Deferred, JsFunction, JsHost, JsObject, OpError, OpResult, Promise, Value, WeakValue,
};
use lumen_bind::Host;
use lumen_html::{
    css::{self, FontFaceDescriptors, FontFaceIdentity, FontFaceRule},
    paint::{FontMetric, FontSpec, TextShaper},
};
pub use lumen_html_text::{FontFaceStatus, ManualFontFace};
use lumen_html_text::{
    FontFace, FontLoadRequest, FontProvider, FontRegistryContext,
    FontRegistrySnapshot, FontSet, ManualFontFaceState, ManualFontRegistry,
    ManualFontSource,
    MAX_MANUAL_FONT_BYTES_PER_FACE,
};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
    sync::Arc,
    task::Poll,
};

/// The embedder supplies bounded resource loading and may expose the same
/// decoded face to layout. It never parses descriptors or manufactures fonts.
pub trait FontResourceLoader {
    /// Install the owning document's policy at the provider's actual URL I/O
    /// seam, shared by CSS layout demands and FontFace.load().
    fn set_request_policy(&self,_policy:Rc<dyn Fn(&str)->bool>) {}
    /// Consume a completed anonymous-CORS font preload, including its failure,
    /// before starting another request for the same resource.
    fn set_preload_reader(&self,_reader:Rc<dyn Fn(&str)->Option<Result<Arc<[u8]>,String>>>) {}
    fn load(&self, rule: &FontFaceRule, document_base: &str) -> Result<Arc<FontFace>, String>;

    /// Start or poll an owner-managed resource request. Pending leaves the
    /// FontFace and its promises loading; the host pumps them after HTTP wakes
    /// the owner. Synchronous embedders retain their existing implementation.
    fn poll_load(
        &self,
        rule: &FontFaceRule,
        document_base: &str,
    ) -> Poll<Result<Arc<FontFace>, String>> {
        Poll::Ready(self.load(rule, document_base))
    }

    /// Runtime-backed providers can schedule requests on the owning event loop
    /// and wake it on completion without polling timers or blocking JavaScript.
    fn poll_load_in_context(
        &self,
        _ctx: &mut Ctx,
        rule: &FontFaceRule,
        base: &str,
    ) -> Poll<Result<Arc<FontFace>, String>> {
        self.poll_load(rule, base)
    }

    /// Query the same cache populated by automatic layout loads, without
    /// initiating a resource request. A returned face has already decoded.
    fn loaded(&self, _rule: &FontFaceRule, _document_base: &str) -> Option<Arc<FontFace>> {
        None
    }

    /// Observe a layout-initiated request without initiating unused faces.
    /// Pending and failed requests participate in the normal FontFaceSet
    /// lifecycle, just like requests initiated by FontFace.load().
    fn automatic_load(
        &self,
        rule: &FontFaceRule,
        document_base: &str,
    ) -> Option<Poll<Result<Arc<FontFace>, String>>> {
        self.loaded(rule, document_base)
            .map(|face| Poll::Ready(Ok(face)))
    }

    /// Current non-CSS members of this context's `FontFaceSet`. The adapter
    /// updates the snapshot on membership and load-state changes.
    fn replace_manual_faces(&self, _faces: &[ManualFontFace]) {}

    /// Current non-CSS members in insertion order. Renderers append decoded
    /// entries after the live CSS-connected faces in stylesheet order.
    fn manual_faces(&self) -> Vec<ManualFontFace> {
        Vec::new()
    }

    /// Receive the context's generation-tagged font view. Existing providers
    /// keep working through the default bridge while newer renderers can use
    /// CSS descriptors and generation to rebuild their font snapshots.
    fn replace_font_registry(&self, snapshot: &FontRegistrySnapshot<'_>) {
        self.replace_manual_faces(snapshot.manual_faces);
    }

    /// Metric of the first available font selected by the embedder's actual
    /// FontSet, without starting a resource request.
    fn primary_metric(&self, _font: &FontSpec, _metric: FontMetric) -> Option<f32> {
        None
    }
}

type FaceSource = ManualFontSource;

struct CssFaceData {
    detached: std::cell::OnceCell<ManualFontFaceState>,
    identity: FontFaceIdentity,
    rule: RefCell<FontFaceRule>,
    status: Cell<FontFaceStatus>,
    decoded: RefCell<Option<Arc<FontFace>>>,
    error: RefCell<Option<String>>,
}

enum FaceBacking {
    Css(CssFaceData),
    Manual(ManualFontFaceState),
}

/// A worker owns a font registry without a document, CSS tree, or layout host.
pub struct WorkerFontContext {
    pub(crate) font_loading: FontLoading,
    base_url: String,
}

impl WorkerFontContext {
    pub(crate) fn canvas_font_set(&self, fallback: &FontSet) -> Result<Rc<FontSet>, &'static str> {
        self.font_loading.canvas_font_set(fallback, &self.base_url)
    }
}

#[derive(Clone)]
enum FontRealm {
    Document(Rc<DomRealm>),
    Worker(Rc<WorkerFontContext>),
}

#[derive(Clone)]
pub(crate) enum WeakFontRealm {
    Document(Weak<DomRealm>),
    Worker(Weak<WorkerFontContext>),
}

impl WeakFontRealm {
    pub(crate) fn attach_display_owner(&self) {
        if let Some(realm)=self.upgrade() {*realm.font_loading().display_owner.borrow_mut()=Some(self.clone());}
    }

    pub(crate) fn refresh_render_font_set(&self,fallback:&FontSet)->Result<Option<Rc<FontSet>>,&'static str> {
        let Some(realm)=self.upgrade() else {return Ok(None);};
        realm.font_loading().refresh_render_font_set(fallback)
    }

    pub(crate) fn request_rendered_font(&self,spec:&FontSpec,text:&str)->Result<(), &'static str> {
        let Some(realm)=self.upgrade() else {return Ok(());};
        if realm.document().is_some_and(|document|document.is_document_destroyed()) {return Ok(());}
        realm.font_loading().request_rendered_font(spec,text)
    }

    pub(crate) fn request_metric_font(&self,spec:&FontSpec) {
        let Some(realm)=self.upgrade() else {return;};
        if realm.document().is_some_and(|document|document.is_document_destroyed()) {return;}
        realm.font_loading().request_metric_font(spec);
    }

    pub(crate) fn queue_demand_task(&self,ctx:&mut Ctx)->OpResult<()> {
        let Some(realm)=self.upgrade() else {return Ok(());};
        if realm.document().is_some_and(|document|document.is_document_destroyed()) || !realm.font_loading().has_metric_requests() || realm.font_loading().demand_task_queued.replace(true) {return Ok(());}
        let weak=self.clone();
        let queue=|ctx:&mut Ctx|scheduling::queue_task(ctx,move |ctx| {
            let Some(realm)=weak.upgrade() else {return Ok(());};
            realm.font_loading().demand_task_queued.set(false);
            match realm {
                FontRealm::Document(document)=> {
                    document.queue_font_tasks(ctx)?;
                    document.settle_font_loading(ctx)?;
                }
                FontRealm::Worker(_)=> {poll_worker_fonts(ctx)?;}
            }
            Ok(())
        });
        let result=if let Some(document)=realm.document() {
            if let Some(owner)=document.relevant_host_realm(ctx) {ctx.with_host_realm(&owner,queue).map_err(browsing_context::host_realm_error).and_then(|result|result)} else {queue(ctx)}
        } else {queue(ctx)};
        if result.is_err() {realm.font_loading().demand_task_queued.set(false);}
        result
    }

    fn upgrade(&self) -> Option<FontRealm> {
        match self {
            Self::Document(realm) => realm.upgrade().map(FontRealm::Document),
            Self::Worker(realm) => realm.upgrade().map(FontRealm::Worker),
        }
    }
}

impl FontRealm {
    fn downgrade(&self) -> WeakFontRealm {
        match self {
            Self::Document(realm) => WeakFontRealm::Document(Rc::downgrade(realm)),
            Self::Worker(realm) => WeakFontRealm::Worker(Rc::downgrade(realm)),
        }
    }
    fn document(&self) -> Option<&Rc<DomRealm>> {
        match self { Self::Document(realm) => Some(realm), Self::Worker(_) => None }
    }
    fn font_loading(&self) -> &FontLoading {
        match self { Self::Document(realm) => &realm.font_loading, Self::Worker(realm) => &realm.font_loading }
    }
    fn base_url(&self) -> String {
        match self { Self::Document(realm) => realm.base_url(), Self::Worker(realm) => realm.base_url.clone() }
    }
    fn invalidate_fonts(&self) {
        if let Some(document) = self.document() { document.session.borrow_mut().invalidate_fonts(); }
    }
    fn same_context(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Document(a), Self::Document(b)) => Rc::ptr_eq(a, b),
            (Self::Worker(a), Self::Worker(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// FontFaceSet matching absolutizes relative descriptors against initial CSS
/// properties, independently of the document element's inherited font.
fn resolve_initial_font_selection(realm: &FontRealm, spec: &mut FontSpec) -> OpResult<()> {
    if spec.unresolved_style.is_none() && spec.unresolved_stretch.is_none() { return Ok(()); }
    let context = initial_font_context(realm)?;
    if !css::resolve_font_spec_context(spec, context) {
        return Err(OpError::new("InvalidStateError", "Font matching requires an available query or viewport context"));
    }
    Ok(())
}

fn initial_font_context(realm: &FontRealm) -> OpResult<css::FontShorthandContext> {
    let initial = FontSpec::default();
    let metrics = super::canvas::canvas_fallback_fonts().font_relative_metrics_styled(16.0, &initial);
    let viewport = realm.document().map(|realm| realm.with_session(|session| session.media_environment()));
    let query = realm.document().map(|realm| realm.with_session(|session|
        session.query_container_context(session.document().root()).map_err(|error|
            OpError::new("InvalidStateError", format!("font query context unavailable: {error:?}"))))).transpose()?;
    Ok(css::FontShorthandContext {font_size:16.0, root_font_size:16.0,
        ex:metrics.ex, ch:metrics.ch, units:Some([css::font_unit_bases(Some(super::canvas::canvas_fallback_fonts()),16.0,&initial,css::LineHeight::Normal,false,false);2]), weight:400, viewport,
        query})
}

fn resolve_initial_width_descriptors(realm: &FontRealm, rules: &mut [FontFaceRule]) -> OpResult<()> {
    if !rules.iter().any(|rule| rule.stretch_expressions.is_some()) { return Ok(()); }
    let context = initial_font_context(realm)?;
    for rule in rules {
        if !css::resolve_font_face_width_context(&mut rule.descriptors,context) {
            return Err(OpError::new("InvalidStateError", "Font width descriptors require an available query or viewport context"));
        }
    }
    Ok(())
}

/// Install the same native loading objects used by documents in an isolated
/// worker font registry. The URL is the immutable, final worker response URL.
pub fn install_worker_fonts(
    ctx: &mut Ctx,
    base_url: &str,
    provider: Rc<dyn FontResourceLoader>,
) -> OpResult<Rc<WorkerFontContext>> {
    let worker = Rc::new(WorkerFontContext {
        font_loading: FontLoading::new(FontRegistryContext::Worker),
        base_url: base_url.to_owned(),
    });
    worker.font_loading.capture_dom_exception(ctx);
    worker.font_loading.set_provider(provider);
    ctx.op_state().put(worker.clone());
    let global = ctx.global_object();
    for (name, value) in [
        ("FontFace", ctx.class_constructor::<DomFontFace>()),
        ("FontFaceSet", ctx.class_constructor::<DomFontFaceSet>()),
        ("FontFaceSetLoadEvent", ctx.class_constructor::<DomFontFaceSetLoadEvent>()),
    ] {
        crate::install_interface(ctx, &global, name, value).map_err(OpError::thrown)?;
    }
    let set = ctx.new_instance(DomFontFaceSet::for_context(&FontRealm::Worker(worker.clone())));
    ctx.instance_data::<DomFontFaceSet>(&set).expect("new FontFaceSet has native backing").borrow().attach(ctx, &set);
    let descriptor = ctx.new_object_with_proto(&Value::Null);
    for (name, value) in [
        ("value", set), ("writable", Value::Bool(false)),
        ("enumerable", Value::Bool(true)), ("configurable", Value::Bool(true)),
    ] {
        ctx.member_set(&descriptor, name, value).map_err(OpError::thrown)?;
    }
    ctx.define_property_value(&global, Value::str("fonts"), &descriptor).map_err(OpError::thrown)?;

    worker.font_loading.settle(ctx)?;
    Ok(worker)
}

/// Pump pending requests only when the owner's normal event loop runs. Pending
/// network work supplies its own wakeup; this function never schedules a poll.
pub fn poll_worker_fonts(ctx: &mut Ctx) -> OpResult<usize> {
    let Some(worker) = ctx.op_state().get::<Rc<WorkerFontContext>>().cloned() else { return Ok(0); };
    if worker.font_loading.has_metric_requests() {
        let owner=worker.font_loading.sets.borrow().iter().filter_map(Weak::upgrade)
            .find_map(|state|state.borrow().owner.as_ref().and_then(WeakValue::upgrade));
        if let Some(owner)=owner {
            let set=ctx.instance_data::<DomFontFaceSet>(&owner)
                .ok_or_else(||OpError::new("InvalidStateError","worker font demand owner"))?;
            worker.font_loading.queue_metric_requests(ctx,&set.borrow())?;
        } else {worker.font_loading.discard_metric_requests();}
    }
    worker.font_loading.update_display_clock();
    let completed = worker.font_loading.pump(ctx, &worker.base_url);
    worker.font_loading.schedule_display_deadline(ctx)?;
    worker.font_loading.settle(ctx)?;
    Ok(completed)
}

struct FaceData {
    backing: FaceBacking,
    loading_rule: RefCell<Option<FontFaceRule>>,
    detached_display:RefCell<Option<FaceDisplay>>,
    descriptor_overrides: RefCell<Vec<(String, String)>>,
    invalid_descriptors: RefCell<HashMap<String, String>>,
    loaded: Value,
    loaded_deferred: RefCell<Option<Deferred>>,
    realm: WeakFontRealm,
    wrapper: RefCell<Option<WeakValue>>,
}

impl FaceData {
    fn new_css(
        ctx: &mut Ctx,
        realm: &FontRealm,
        mut rule: FontFaceRule,
        identity: FontFaceIdentity,
    ) -> Rc<Self> {
        rule.identity = Some(identity.clone());
        let deferred = Deferred::new(ctx);
        Rc::new(Self {
            loading_rule: RefCell::new(Some(rule.clone())),
            detached_display:RefCell::new(None),
            backing: FaceBacking::Css(CssFaceData {
                detached: std::cell::OnceCell::new(),
                identity,
                rule: RefCell::new(rule),
                status: Cell::new(FontFaceStatus::Unloaded),
                decoded: RefCell::new(None),
                error: RefCell::new(None),
            }),
            descriptor_overrides: RefCell::new(Vec::new()),
            invalid_descriptors: RefCell::new(HashMap::new()),
            loaded: deferred.promise(),
            loaded_deferred: RefCell::new(Some(deferred)),
            realm: realm.downgrade(),
            wrapper: RefCell::new(None),
        })
    }

    fn new_manual(
        ctx: &mut Ctx,
        realm: &FontRealm,
        state: ManualFontFaceState,
    ) -> Rc<Self> {
        let deferred = Deferred::new(ctx);
        Rc::new(Self {
            backing: FaceBacking::Manual(state),
            loading_rule: RefCell::new(None),
            detached_display:RefCell::new(None),
            descriptor_overrides: RefCell::new(Vec::new()),
            invalid_descriptors: RefCell::new(HashMap::new()),
            loaded: deferred.promise(),
            loaded_deferred: RefCell::new(Some(deferred)),
            realm: realm.downgrade(),
            wrapper: RefCell::new(None),
        })
    }

    fn wrapper_value(&self) -> Option<Value> {
        self.wrapper.borrow().as_ref().and_then(WeakValue::upgrade)
    }

    fn set_wrapper(&self, ctx: &mut Ctx, wrapper: &Value) {
        *self.wrapper.borrow_mut() = ctx.weak_value(wrapper);
    }

    fn rule(&self) -> FontFaceRule {
        match &self.backing {
            FaceBacking::Css(data) => data.detached.get().map_or_else(||data.rule.borrow().clone(),ManualFontFaceState::rule),
            FaceBacking::Manual(data) => data.rule(),
        }
    }

    fn identity(&self) -> FontFaceIdentity {
        match &self.backing {
            FaceBacking::Css(data) => data.detached.get().map_or_else(||data.identity.clone(),ManualFontFaceState::identity),
            FaceBacking::Manual(data) => data.identity(),
        }
    }

    fn source(&self) -> FaceSource {
        match &self.backing {
            FaceBacking::Css(data) => data.detached.get().map_or(FaceSource::Url,ManualFontFaceState::source),
            FaceBacking::Manual(data) => data.source(),
        }
    }

    fn status(&self) -> FontFaceStatus {
        match &self.backing {
            FaceBacking::Css(data) => data.detached.get().map_or_else(||data.status.get(),ManualFontFaceState::status),
            FaceBacking::Manual(data) => data.status(),
        }
    }

    fn error(&self) -> Option<String> {
        match &self.backing {
            FaceBacking::Css(data) => data.detached.get().map_or_else(||data.error.borrow().clone(),|face|face.error().map(|error|error.to_string())),
            FaceBacking::Manual(data) => data.error().map(|error| error.to_string()),
        }
    }

    fn manual_state(&self) -> Option<&ManualFontFaceState> {
        match &self.backing {
            FaceBacking::Css(data) => data.detached.get(),
            FaceBacking::Manual(state) => Some(state),
        }
    }

    fn detach_css(&self,loading:&FontLoading)->Result<(),&'static str> {
        let FaceBacking::Css(data)=&self.backing else {return Ok(());};
        if data.detached.get().is_some() {return Ok(());}
        let mut registry=loading.manual_registry.borrow_mut();
        let face=registry.create_manual_face(data.rule.borrow().clone(),FaceSource::Url)?;
        match data.status.get() {
            FontFaceStatus::Unloaded=>{},
            FontFaceStatus::Loading=>{registry.begin_load(&face)?;},
            FontFaceStatus::Loaded=>{
                registry.begin_load(&face)?;
                registry.complete_load(&face,Ok(data.decoded.borrow().clone().ok_or("loaded face has no decoded font")?))?;
            },
            FontFaceStatus::Error=>{
                registry.begin_load(&face)?;
                registry.complete_load(&face,Err(Arc::from(data.error.borrow().clone().unwrap_or_else(||"font loading failed".into()))))?;
            },
        }
        data.detached.set(face).map_err(|_|"font already detached")?;
        self.preserve_display(loading,&data.identity);
        Ok(())
    }

    fn preserve_display(&self,loading:&FontLoading,identity:&FontFaceIdentity) {
        let mut entries=loading.display_faces.borrow_mut();
        if entries.get(identity).is_some_and(|entry|entry.sources==self.rule().sources) {
            *self.detached_display.borrow_mut()=entries.remove(identity);
        }
    }

    fn decoded(&self)->Option<Arc<FontFace>> {
        match &self.backing {
            FaceBacking::Css(data)=>data.detached.get().map_or_else(||data.decoded.borrow().clone(),ManualFontFaceState::decoded),
            FaceBacking::Manual(data)=>data.decoded(),
        }
    }

    fn load_rule(&self)->FontFaceRule {
        self.loading_rule.borrow_mut().get_or_insert_with(||self.rule()).clone()
    }

    fn set_css_rule(&self, rule: FontFaceRule) {
        if let FaceBacking::Css(data) = &self.backing {
            *data.rule.borrow_mut() = rule;
        }
    }

    fn set_css_status(&self, status: FontFaceStatus) {
        if let FaceBacking::Css(data) = &self.backing {
            data.status.set(status);
        }
    }

    fn set_css_decoded(&self, decoded: Option<Arc<FontFace>>) {
        if let FaceBacking::Css(data) = &self.backing {
            *data.decoded.borrow_mut() = decoded;
        }
    }

    fn set_css_error(&self, error: Option<String>) {
        if let FaceBacking::Css(data) = &self.backing {
            *data.error.borrow_mut() = error;
        }
    }
}

struct Batch {
    faces: Vec<Rc<FaceData>>,
    values: Vec<Value>,
    promise: Option<Deferred>,
    base: Option<String>,
}

struct OwnedFontRegistrySnapshot {
    generation: u64,
    document_css_faces: Vec<FontFaceRule>,
    manual_faces: Vec<ManualFontFace>,
}

#[derive(Clone)]
struct FaceDisplay {
    sources:Arc<[css::FontFaceSource]>,
    timeline:lumen_html::font_display::DisplayTimeline,
    completed_at:Option<u64>,
    failed:bool,
}

struct CanvasFontSetCache {
    registry_generation: u64,
    resource_generation: u64,
    fallback_generation:u64,
    document_base: String,
    query: lumen_html::css::ContainerUnitContext,
    fonts: Rc<FontSet>,
}

const MAX_FONT_DEMAND_SPECS: usize = 64;
const MAX_FONT_DEMAND_SCALARS: usize = 65_536;

#[derive(Clone)]
struct FontDemand {
    spec: FontSpec,
    metrics: bool,
    // Sorted unique Unicode scalars, not retained paragraph/source strings.
    scalars: Vec<char>,
}

pub(crate) struct FontLoading {
    provider: RefCell<Option<Rc<dyn FontResourceLoader>>>,
    metric_requests: RefCell<Vec<FontDemand>>,
    metric_request_memo: RefCell<Vec<(u64, FontDemand)>>,
    dom_exception_constructor: RefCell<Option<WeakValue>>,
    object_freeze: RefCell<Option<WeakValue>>,
    batches: RefCell<Vec<Batch>>,
    worker_pump_queued: Cell<bool>,
    demand_task_queued: Cell<bool>,
    sets: RefCell<Vec<Weak<RefCell<FontFaceSetState>>>>,
    manual_registry: RefCell<ManualFontRegistry>,
    resource_generation: Cell<u64>,
    canvas_font_set: RefCell<Option<CanvasFontSetCache>>,
    pub(crate) canvas_font_source_initialized: Cell<bool>,
    pub(crate) canvas_css_key: Cell<Option<(u64, lumen_html::css::MediaEnvironment)>>,
    display_owner:RefCell<Option<WeakFontRealm>>,
    pub(crate) render_fallback:RefCell<Option<Arc<FontSet>>>,
    display_clock:std::time::Instant,
    display_faces:RefCell<HashMap<FontFaceIdentity,FaceDisplay>>,
    display_timer:Cell<Option<(u64,u64)>>,
    display_timer_generation:Cell<u64>,
}

impl Default for FontLoading {
    fn default() -> Self {
        Self::new(FontRegistryContext::Document)
    }
}

impl FontLoading {
    pub(crate) fn new(context: FontRegistryContext) -> Self {
        Self {
            provider: RefCell::new(None),
            metric_requests: RefCell::new(Vec::new()),
            metric_request_memo: RefCell::new(Vec::new()),
            dom_exception_constructor: RefCell::new(None),
            object_freeze: RefCell::new(None),
            batches: RefCell::new(Vec::new()),
            worker_pump_queued: Cell::new(false),
            demand_task_queued: Cell::new(false),
            sets: RefCell::new(Vec::new()),
            manual_registry: RefCell::new(ManualFontRegistry::new(context)),
            resource_generation: Cell::new(0),
            canvas_font_set: RefCell::new(None),
            canvas_font_source_initialized: Cell::new(false),
            canvas_css_key: Cell::new(None),
            display_owner:RefCell::new(None),
            render_fallback:RefCell::new(None),
            display_clock:std::time::Instant::now(),
            display_faces:RefCell::new(HashMap::new()),
            display_timer:Cell::new(None),
            display_timer_generation:Cell::new(0),
        }
    }

    /// Rendering records an intent; only the existing owner task pump may
    /// initiate provider I/O or materialize FontFace/FontFaceSet wrappers.
    pub(crate) fn has_metric_requests(&self)->bool {!self.metric_requests.borrow().is_empty()}
    pub(crate) fn discard_metric_requests(&self) {
        self.metric_requests.borrow_mut().clear();
        self.metric_request_memo.borrow_mut().clear();
    }

    pub(crate) fn request_metric_font(&self, spec: &FontSpec) {
        let _ = self.record_font_demand(spec, "", true);
    }

    pub(crate) fn request_rendered_font(&self, spec: &FontSpec, text: &str) -> Result<(), &'static str> {
        self.record_font_demand(spec, text, false)
    }

    fn record_font_demand(&self, spec: &FontSpec, text: &str, metrics: bool) -> Result<(), &'static str> {
        if self.provider.borrow().is_none() { return Ok(()); }
        let generation=self.resource_generation.get();
        let memo=self.metric_request_memo.borrow();
        let cached=memo.iter().find(|(old,demand)| *old==generation && &demand.spec==spec).map(|(_,demand)|demand);
        let needs_metrics=metrics && cached.is_none_or(|old|!old.metrics);
        if !needs_metrics && text.chars().all(|ch|cached.is_some_and(|old|old.scalars.binary_search(&ch).is_ok())) {return Ok(());}
        let mut requests=self.metric_requests.borrow_mut();
        let scalar_count=requests.iter().map(|demand|demand.scalars.len()).sum::<usize>();
        let existing=requests.iter().position(|old| &old.spec==spec);
        let mut novel=std::collections::HashSet::new();
        for ch in text.chars() {
            if cached.is_some_and(|old|old.scalars.binary_search(&ch).is_ok()) || existing.is_some_and(|index|requests[index].scalars.binary_search(&ch).is_ok()) || novel.contains(&ch) {continue;}
            if scalar_count.saturating_add(novel.len())>=MAX_FONT_DEMAND_SCALARS {return Err("font demand scalar budget exhausted");}
            novel.try_reserve(1).map_err(|_|"font demand scalar admission failed")?;
            novel.insert(ch);
        }
        let needs_metrics=needs_metrics && existing.is_none_or(|index|!requests[index].metrics);
        if novel.is_empty() && !needs_metrics {return Ok(());}
        if let Some(index)=existing {
            requests[index].scalars.try_reserve_exact(novel.len()).map_err(|_|"font demand scalar allocation failed")?;
        } else {
            if requests.len()>=MAX_FONT_DEMAND_SPECS {return Err("font demand specification budget exhausted");}
            requests.try_reserve_exact(1).map_err(|_|"font demand allocation failed")?;
        }
        let mut new_scalars=Vec::new();
        if existing.is_none() {new_scalars.try_reserve_exact(novel.len()).map_err(|_|"font demand scalar allocation failed")?;}
        drop(memo);
        drop(requests);
        self.begin_display_demand(spec,text,metrics)?;
        let mut requests=self.metric_requests.borrow_mut();
        let mut memo=self.metric_request_memo.borrow_mut();
        let next_requests=requests.len()+usize::from(existing.is_none());
        let next_scalars=scalar_count.saturating_add(novel.len());
        // Cache eviction forgets prior satisfaction; it never marks new scalar
        // demand satisfied. Pending + memo share one aggregate metadata budget.
        while !memo.is_empty() && (memo.len().saturating_add(next_requests)>MAX_FONT_DEMAND_SPECS || memo.iter().map(|(_,old)|old.scalars.len()).sum::<usize>().saturating_add(next_scalars)>MAX_FONT_DEMAND_SCALARS) {memo.remove(0);}
        if let Some(index)=existing {
            let demand=&mut requests[index];
            demand.scalars.extend(novel);demand.scalars.sort_unstable();demand.metrics |= needs_metrics;
        } else {
            new_scalars.extend(novel);new_scalars.sort_unstable();
            requests.push(FontDemand{spec:spec.clone(),metrics:needs_metrics,scalars:new_scalars});
        }
        drop(memo);
        drop(requests);
        Ok(())
    }

    fn display_now(&self)->u64 {
        self.display_clock.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }

    fn begin_display_demand(&self,spec:&FontSpec,text:&str,metrics:bool)->Result<(),&'static str> {
        if text.is_empty() && !metrics {return Ok(());}
        let snapshot=self.copy_font_registry_snapshot()?;
        let (base,query)=self.canvas_font_set.borrow().as_ref()
            .map(|cache|(cache.document_base.clone(),cache.query)).ok_or("font display needs render context")?;
        let mut rules=snapshot.document_css_faces;
        rules.try_reserve(snapshot.manual_faces.len()).map_err(|_|"font display rule admission failed")?;
        rules.extend(snapshot.manual_faces.iter().map(|face|face.rule.clone()));
        let installed=self.render_fallback.borrow().clone();
        let fallback=installed.as_deref().unwrap_or_else(||super::canvas::canvas_fallback_fonts());
        let initial=FontSpec::default();
        let relative=fallback.font_relative_metrics_styled(16.0,&initial);
        let context=css::FontShorthandContext {font_size:16.0,root_font_size:16.0,
            ex:relative.ex,ch:relative.ch,units:Some([css::font_unit_bases(Some(fallback),16.0,&initial,css::LineHeight::Normal,false,false);2]),weight:400,viewport:Some(query.small_viewport),query:Some(query)};
        for rule in &mut rules {
            if !css::resolve_font_face_width_context(&mut rule.descriptors,context) {
                return Err("font display descriptor query context unavailable");
            }
        }
        let now=self.display_now();
        let provider=self.provider.borrow().clone();
        let mut by_identity=HashMap::new();
        by_identity.try_reserve(rules.len()).map_err(|_|"font display rule admission failed")?;
        for (index,rule) in rules.iter().enumerate() {
            if let Some(identity)=rule.identity.as_ref() {by_identity.insert(identity,index);}
        }
        let manual_for=|identity:&css::FontFaceIdentity| by_identity.get(identity)
            .and_then(|&index|index.checked_sub(rules.len()-snapshot.manual_faces.len()))
            .and_then(|index|snapshot.manual_faces.get(index));
        let mut display=self.display_faces.borrow_mut();
        // Detached source connections are not permanent page metadata.
        display.retain(|identity,entry|by_identity.get(identity).is_some_and(|&index|rules[index].sources==entry.sources));
        let input=if metrics {" "}else{text};
        let indices=css::rendering_font_faces(spec,&rules,input,|rule,cluster| {
            let Some(identity)=rule.identity.as_ref() else {return false;};
            if display.get(identity).is_some_and(|entry|entry.timeline.phase()==lumen_html::font_display::DisplayPhase::Failure) {return false;}
            let manual=manual_for(identity);
            if manual.is_some_and(|face|face.status==FontFaceStatus::Error) {return false;}
            let decoded=manual.and_then(|face|face.decoded.clone()).or_else(||provider.as_ref().and_then(|provider|provider.loaded(rule,&base)));
            metrics || decoded.is_none_or(|face|face.covers_cluster(cluster))
        });
        let additions=indices.iter().filter(|&&index|rules[index].identity.as_ref().is_some_and(|identity|!display.contains_key(identity))).count();
        if display.len().saturating_add(additions)>MAX_FONT_DEMAND_SPECS {return Err("font display face budget exhausted");}
        display.try_reserve(additions).map_err(|_|"font display allocation failed")?;
        let mut changed=false;
        for index in indices {
            let rule=&rules[index];
            let identity=rule.identity.as_ref().ok_or("font display rule identity missing")?;
            if display.contains_key(identity) {continue;}
            let manual=manual_for(identity);
            let loaded=manual.is_some_and(|face|face.status==FontFaceStatus::Loaded)
                || provider.as_ref().is_some_and(|provider|provider.loaded(rule,&base).is_some());
            let mut timeline=lumen_html::font_display::DisplayTimeline::new(rule.effective_display(query.small_viewport),now);
            timeline.update(now,loaded,false);
            display.insert(identity.clone(),FaceDisplay{sources:rule.sources.clone(),timeline,
                completed_at:loaded.then_some(now),failed:false});
            changed=true;
        }
        drop(display);
        if changed {self.advance_resource_generation();}
        Ok(())
    }

    fn refresh_display_rules(&self,css_rules:&[FontFaceRule],manual_faces:&[ManualFontFace],environment:css::MediaEnvironment) {
        let now=self.display_now();
        let mut changed=false;
        self.display_faces.borrow_mut().retain(|identity,entry| {
            let rule=css_rules.iter().find(|rule|rule.identity.as_ref()==Some(identity))
                .or_else(||manual_faces.iter().find(|face|&face.identity==identity).map(|face|&face.rule));
            let Some(rule)=rule.filter(|rule|rule.sources==entry.sources) else {changed=true;return false;};
            changed |= entry.timeline.configure(rule.effective_display(environment),now,entry.completed_at,entry.failed);
            true
        });
        if changed {self.advance_resource_generation();}
    }

    fn display_phase(&self,rule:&FontFaceRule,_loaded:bool)->lumen_html::font_display::DisplayPhase {
        use lumen_html::font_display::DisplayPhase;
        rule.identity.as_ref().and_then(|identity|self.display_faces.borrow().get(identity)
            .filter(|entry|entry.sources==rule.sources).map(|entry|entry.timeline.phase()))
            // Unused faces are not presentation registrations. The first demand starts
            // their clock before rebuilding the font source; discovery never blocks.
            .unwrap_or(DisplayPhase::Loaded)
    }

    fn complete_display(&self,face:&FaceData,success:bool) {
        let identity=face.identity();
        let rule=face.rule();
        let now=self.display_now();
        if let Some(entry)=face.detached_display.borrow_mut().as_mut().filter(|entry|entry.sources==rule.sources) {
            if success {entry.completed_at=Some(now);}else{entry.failed=true;}
            entry.timeline.update_with_completion(now,entry.completed_at,entry.failed);
        }
        let changed={
            let mut entries=self.display_faces.borrow_mut();
            entries.get_mut(&identity).filter(|entry|entry.sources==rule.sources).is_some_and(|entry| {
                if success {entry.completed_at=Some(now);}else{entry.failed=true;}
                entry.timeline.update_with_completion(now,entry.completed_at,entry.failed)
            })
        };
        if changed {self.advance_resource_generation();}
    }

    fn advance_display_at(&self,now:u64)->bool {
        let mut changed=false;
        for entry in self.display_faces.borrow_mut().values_mut() {
            changed |= entry.timeline.update_with_completion(now,entry.completed_at,entry.failed);
        }
        if changed {self.advance_resource_generation();}
        changed
    }

    fn next_display_delay(&self,now:u64)->Option<u64> {
        self.display_faces.borrow().values().filter_map(|entry|entry.timeline.next_delay_ms(now)).min()
    }

    pub(crate) fn needs_display_owner_context(&self)->bool {
        self.display_timer.get().is_some() || self.next_display_delay(self.display_now()).is_some()
    }

    pub(crate) fn next_display_deadline(&self)->Option<std::time::Duration> {
        self.next_display_delay(self.display_now()).map(std::time::Duration::from_millis)
    }

    pub(crate) fn schedule_display_deadline(&self,ctx:&mut Ctx)->OpResult<()> {
        let now=self.display_now();
        let delay=self.next_display_delay(now);
        let deadline=delay.map(|delay|now.saturating_add(delay));
        if self.display_timer.get().map(|(deadline,_)|deadline)==deadline {return Ok(());}
        if let Some((_,id))=self.display_timer.take() {lumen_timers::cancel_host_callback(ctx,id);}
        let Some(delay)=delay else {return Ok(());};
        // Source-only embedders expose the deadline through the existing font
        // pump contract; Runtime installs and drives the canonical timer heap.
        if ctx.host_mut::<lumen_timers::Timers>().is_none() {return Ok(());}
        let generation=self.display_timer_generation.get().checked_add(1)
            .ok_or_else(||OpError::new("QuotaExceededError","font deadline generation exhausted"))?;
        let owner=self.display_owner.borrow().clone().ok_or_else(||OpError::new("InvalidStateError","font display owner missing"))?;
        let ticket=ctx.new_instance(FontDisplayDeadline{owner,generation});
        let callback=ctx.bound_function(&lumen_bind::FnItem::of::<display_deadline::tick::Op>());
        let id=lumen_timers::schedule_host_callback(ctx,callback,&[ticket],
            std::time::Duration::from_millis(delay))?;
        self.display_timer_generation.set(generation);
        self.display_timer.set(Some((now.saturating_add(delay),id)));
        Ok(())
    }

    pub(crate) fn discard_display_deadlines(&self,ctx:&mut Ctx) {
        if let Some((_,id))=self.display_timer.take() {lumen_timers::cancel_host_callback(ctx,id);}
        self.display_faces.borrow_mut().clear();
    }

    pub(crate) fn update_display_clock(&self)->bool {self.advance_display_at(self.display_now())}

    pub(crate) fn queue_metric_requests(&self,ctx:&mut Ctx,set:&DomFontFaceSet)->OpResult<usize> {
        let requests=core::mem::take(&mut *self.metric_requests.borrow_mut());
        if requests.is_empty() {return Ok(0);}
        let realm=set.realm()?;
        let entries=set.snapshot_entries(ctx)?;
        let mut rules=entries.iter().map(|(face,_)|face.rule()).collect::<Vec<_>>();
        resolve_initial_width_descriptors(&realm,&mut rules)?;
        let mut by_identity=HashMap::new();
        by_identity.try_reserve(entries.len()).map_err(|_|OpError::new("QuotaExceededError","font demand face admission"))?;
        for (index,(face,_)) in entries.iter().enumerate() {by_identity.insert(face.identity(),index);}
        let provider=self.provider.borrow().clone();
        let base=realm.base_url();
        let decoded=entries.iter().map(|(face,_)|face.decoded()
            .or_else(||provider.as_ref().and_then(|provider|provider.loaded(&face.rule(),&base)))).collect::<Vec<_>>();
        let mut selected=Vec::new();
        selected.try_reserve_exact(entries.len()).map_err(|_|OpError::new("QuotaExceededError","font demand selected face admission"))?;
        let mut selected_indices=Vec::new();
        selected_indices.try_reserve_exact(entries.len()).map_err(|_|OpError::new("QuotaExceededError","font demand selection flags"))?;
        selected_indices.resize(entries.len(),false);
        for demand in &requests {
            let mut text=String::new();
            text.try_reserve_exact(demand.scalars.len().saturating_mul(4)).map_err(|_|OpError::new("QuotaExceededError","font demand scalar projection"))?;
            for ch in &demand.scalars {text.push(*ch);}
            for (text,metrics) in [(text.as_str(),false),(if demand.metrics {" "} else {""},true)] {
                for index in css::rendering_font_faces(&demand.spec,&rules,text,|rule,piece| {
                    let Some(index)=rule.identity.as_ref().and_then(|identity|by_identity.get(identity)).copied() else {return false;};
                    entries[index].0.status()!=FontFaceStatus::Error
                        && self.display_phase(rule,decoded[index].is_some())!=lumen_html::font_display::DisplayPhase::Failure
                        && (metrics || decoded[index].as_ref().is_none_or(|face|face.covers_cluster(piece)))
                }) {
                    if entries[index].0.status()==FontFaceStatus::Unloaded && !selected_indices[index] {
                        selected_indices[index]=true;selected.push(entries[index].clone());
                    }
                }
            }
        }
        let count=self.enqueue_load(ctx,selected,None);
        let generation=self.resource_generation.get();
        let mut memo=self.metric_request_memo.borrow_mut();
        memo.retain(|(old,_)|*old==generation);
        for mut demand in requests {
            if let Some(at)=memo.iter().position(|(_,old)|old.spec==demand.spec) {
                let (_,old)=memo.remove(at);
                demand.metrics |= old.metrics;
                if old.scalars.len().saturating_add(demand.scalars.len())<=MAX_FONT_DEMAND_SCALARS && demand.scalars.try_reserve_exact(old.scalars.len()).is_ok() {
                    demand.scalars.extend(old.scalars);
                    demand.scalars.sort_unstable();
                    demand.scalars.dedup();
                }
            }
            while !memo.is_empty() && (memo.len()>=MAX_FONT_DEMAND_SPECS || memo.iter().map(|(_,old)|old.scalars.len()).sum::<usize>().saturating_add(demand.scalars.len())>MAX_FONT_DEMAND_SCALARS) {memo.remove(0);}
            if memo.try_reserve_exact(1).is_ok() {memo.push((generation,demand));}
        }
        Ok(count)
    }

    pub(crate) fn has_live_sets(&self) -> bool {
        self.sets
            .borrow()
            .iter()
            .any(|state| state.strong_count() != 0)
    }

    pub(crate) fn capture_dom_exception(&self, ctx: &mut Ctx) {
        let global = ctx.global_object();
        if self.dom_exception_constructor.borrow().is_none() {
            let constructor = ctx
                .get_member(&global, "DOMException")
                .ok()
                .filter(Value::is_callable);
            if constructor.is_some() {
                *self.dom_exception_constructor.borrow_mut() = constructor.and_then(|value| crate::realm_services::capture_realm_value(ctx, value).ok());
            }
        }
        if self.object_freeze.borrow().is_none() {
            let freeze = ctx
                .get_member(&global, "Object")
                .ok()
                .and_then(|object| ctx.get_member(&object, "freeze").ok())
                .and_then(JsFunction::from_value);
            if freeze.is_some() {
                *self.object_freeze.borrow_mut() = freeze.and_then(|value| crate::realm_services::capture_realm_value(ctx, value.value().clone()).ok());
            }
        }
    }

    pub(crate) fn set_provider(&self, provider: Rc<dyn FontResourceLoader>) {
        *self.provider.borrow_mut() = Some(provider);
        self.advance_resource_generation();
        self.sync_manual_faces();
    }

    pub(crate) fn primary_metric(&self, font: &FontSpec, metric: FontMetric) -> Option<f32> {
        if self.canvas_font_set.borrow().is_some() {
            self.request_metric_font(font);
            let installed=self.render_fallback.borrow().clone();
            let fallback=installed.as_deref().unwrap_or_else(||super::canvas::canvas_fallback_fonts());
            if let Ok(Some(fonts))=self.refresh_render_font_set(fallback) {
                return fonts.first_available_metric(font,metric);
            }
        }
        let provider = self.provider.borrow().clone()?;
        provider.primary_metric(font, metric)
    }

    fn invalidate_canvas_font_set(&self) {
        self.metric_request_memo.borrow_mut().clear();
        if let Some(cache)=self.canvas_font_set.borrow_mut().as_mut() {
            cache.resource_generation=u64::MAX;
        }
    }

    fn advance_resource_generation(&self) {
        self.resource_generation
            .set(self.resource_generation.get().wrapping_add(1));
        self.invalidate_canvas_font_set();
    }

    fn register_set(&self, state: &Rc<RefCell<FontFaceSetState>>) {
        self.sets.borrow_mut().push(Rc::downgrade(state));
    }

    fn registry_error(error: &'static str) -> OpError {
        let name = if error.contains("different registry") {
            "InvalidModificationError"
        } else if error.contains("worker") || error.contains("CSS identity") {
            "InvalidStateError"
        } else {
            "QuotaExceededError"
        };
        OpError::new(name, error)
    }

    fn create_manual_face(
        &self,
        ctx: &mut Ctx,
        realm: &FontRealm,
        rule: FontFaceRule,
        source: FaceSource,
    ) -> OpResult<Rc<FaceData>> {
        let state = self
            .manual_registry
            .borrow_mut()
            .create_manual_face(rule, source)
            .map_err(Self::registry_error)?;
        self.invalidate_canvas_font_set();
        Ok(FaceData::new_manual(ctx, realm, state))
    }

    fn update_manual_rule(
        &self,
        face: &FaceData,
        rule: FontFaceRule,
    ) -> OpResult<()> {
        let state = face
            .manual_state()
            .ok_or_else(|| OpError::new("InvalidStateError", "CSS font state is not manual"))?;
        self.manual_registry
            .borrow_mut()
            .update_manual_rule(state, rule)
            .map_err(Self::registry_error)?;
        self.invalidate_canvas_font_set();
        Ok(())
    }

    fn begin_manual_load(&self, face: &FaceData) -> OpResult<Option<FontLoadRequest>> {
        let state = face
            .manual_state()
            .ok_or_else(|| OpError::new("InvalidStateError", "CSS font state is not manual"))?;
        let request = self.manual_registry
            .borrow_mut()
            .begin_load(state)
            .map_err(Self::registry_error)?;
        if request.is_some() {
            self.invalidate_canvas_font_set();
        }
        Ok(request)
    }

    fn complete_manual_load(
        &self,
        face: &FaceData,
        result: Result<Arc<FontFace>, Arc<str>>,
    ) -> OpResult<FontFaceStatus> {
        let state = face
            .manual_state()
            .ok_or_else(|| OpError::new("InvalidStateError", "CSS font state is not manual"))?;
        let status = self.manual_registry
            .borrow_mut()
            .complete_load(state, result)
            .map_err(Self::registry_error)?;
        self.invalidate_canvas_font_set();
        Ok(status)
    }

    fn fail_manual_face(&self, face: &FaceData, error: String) -> OpResult<()> {
        let state = face
            .manual_state()
            .ok_or_else(|| OpError::new("InvalidStateError", "CSS font state is not manual"))?;
        self.manual_registry
            .borrow_mut()
            .fail_manual_face(state, Arc::from(error))
            .map_err(Self::registry_error)?;
        self.invalidate_canvas_font_set();
        Ok(())
    }

    fn add_manual_face(&self, face: &FaceData) -> OpResult<bool> {
        let state = face
            .manual_state()
            .ok_or_else(|| OpError::new("InvalidStateError", "CSS font state is not manual"))?;
        if face.detached_display.borrow().is_some() {
            let mut entries=self.display_faces.borrow_mut();
            if !entries.contains_key(&face.identity()) && entries.len()>=MAX_FONT_DEMAND_SPECS {
                return Err(OpError::new("QuotaExceededError","font display face budget exhausted"));
            }
            entries.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","font display allocation failed"))?;
        }
        let added = self.manual_registry
            .borrow_mut()
            .add_manual_face(state)
            .map_err(Self::registry_error)?;
        if added {
            if let Some(mut entry)=face.detached_display.borrow_mut().take() {
                entry.timeline.configure(face.rule().display,self.display_now(),entry.completed_at,entry.failed);
                entry.timeline.update_with_completion(self.display_now(),entry.completed_at,entry.failed);
                self.display_faces.borrow_mut().insert(face.identity(),entry);
            }
            self.invalidate_canvas_font_set();
        }
        Ok(added)
    }

    fn delete_manual_face(&self, face: &FaceData) -> OpResult<bool> {
        let state = face
            .manual_state()
            .ok_or_else(|| OpError::new("InvalidStateError", "CSS font state is not manual"))?;
        let deleted = self.manual_registry
            .borrow_mut()
            .delete_manual_face(state)
            .map_err(Self::registry_error)?;
        if deleted {
            face.preserve_display(self,&face.identity());
            self.invalidate_canvas_font_set();
        }
        Ok(deleted)
    }

    fn clear_manual_faces(&self) -> bool {
        let cleared = self.manual_registry.borrow_mut().clear_manual_faces();
        if cleared {
            self.invalidate_canvas_font_set();
        }
        cleared
    }

    pub(crate) fn replace_document_css_faces(&self, faces: &[FontFaceRule]) -> OpResult<()> {
        let changed = self
            .manual_registry
            .borrow_mut()
            .replace_document_css_faces(faces)
            .map_err(Self::registry_error)?;
        if changed {
            self.invalidate_canvas_font_set();
            self.sync_manual_faces();
        }
        Ok(())
    }

    fn update_connected_css_rule(&self, rule: &FontFaceRule) -> OpResult<()> {
        // Author descriptor mutation must reach retained render sources without
        // making a renderer borrow the Session. Reuse the bounded registry's
        // validation and generation change; detached rules are never admitted.
        let mut faces = self.with_font_registry_snapshot(|snapshot| {
            let mut faces = Vec::new();
            faces.try_reserve_exact(snapshot.document_css_faces.len())
                .map_err(|_| Self::registry_error("font registry allocation failed"))?;
            faces.extend_from_slice(snapshot.document_css_faces);
            Ok::<_, OpError>(faces)
        })??;
        if let Some(current) = faces.iter_mut().find(|current| current.identity == rule.identity) {
            *current = rule.clone();
            self.replace_document_css_faces(&faces)?;
        }
        Ok(())
    }

    fn manual_member_identities(&self) -> OpResult<Vec<FontFaceIdentity>> {
        let mut registry = self.manual_registry.borrow_mut();
        let members = registry.member_identities();
        let mut identities = Vec::new();
        identities
            .try_reserve_exact(members.len())
            .map_err(|_| OpError::new("QuotaExceededError", "font registry allocation failed"))?;
        identities.extend_from_slice(members);
        Ok(identities)
    }

    pub(crate) fn with_font_registry_snapshot<R>(
        &self,
        callback: impl FnOnce(&FontRegistrySnapshot<'_>) -> R,
    ) -> OpResult<R> {
        let mut registry = self.manual_registry.borrow_mut();
        let snapshot = registry.snapshot().map_err(Self::registry_error)?;
        Ok(callback(&snapshot))
    }

    pub(crate) fn has_canvas_font_source(&self) -> Result<bool, &'static str> {
        if self.provider.borrow().is_some() {
            return Ok(true);
        }
        let has_manual_face = self.with_font_registry_snapshot(|snapshot| {
            snapshot.manual_faces.iter().any(|face| {
                face.status == FontFaceStatus::Loaded && face.decoded.is_some()
            })
        })
        .map_err(|_| "font registry snapshot failed")?;
        if !has_manual_face {
            self.invalidate_canvas_font_set();
        }
        Ok(has_manual_face)
    }

    fn copy_font_registry_snapshot(&self) -> Result<OwnedFontRegistrySnapshot, &'static str> {
        let mut registry = self.manual_registry.borrow_mut();
        let snapshot = registry.snapshot().map_err(|_| "font registry allocation failed")?;
        let mut document_css_faces = Vec::new();
        document_css_faces
            .try_reserve_exact(snapshot.document_css_faces.len())
            .map_err(|_| "font registry allocation failed")?;
        document_css_faces.extend_from_slice(snapshot.document_css_faces);
        let mut manual_faces = Vec::new();
        manual_faces
            .try_reserve_exact(snapshot.manual_faces.len())
            .map_err(|_| "font registry allocation failed")?;
        manual_faces.extend_from_slice(snapshot.manual_faces);
        Ok(OwnedFontRegistrySnapshot {
            generation: snapshot.generation,
            document_css_faces,
            manual_faces,
        })
    }

    /// Return the realm's shared renderer snapshot. Resource callbacks run
    /// only after the registry borrow has ended, and a cache hit clones only
    /// the `Rc` handle rather than face data.
    pub(crate) fn refresh_render_font_set(&self,fallback:&FontSet)->Result<Option<Rc<FontSet>>,&'static str> {
        let snapshot_generation=self.with_font_registry_snapshot(|snapshot|snapshot.generation)
            .map_err(|_|"font registry snapshot failed")?;
        let context={
            let cache=self.canvas_font_set.borrow();
            let Some(cache)=cache.as_ref() else {return Ok(None);};
            if cache.registry_generation==snapshot_generation && cache.resource_generation==self.resource_generation.get()
                && cache.fallback_generation==fallback.generation() {return Ok(Some(cache.fonts.clone()));}
            (cache.document_base.clone(),cache.query)
        };
        self.canvas_font_set_with_query(fallback,&context.0,context.1).map(Some)
    }

    pub(crate) fn canvas_font_set(
        &self,
        fallback: &FontSet,
        document_base: &str,
    ) -> Result<Rc<FontSet>, &'static str> {
        self.canvas_font_set_with_query(fallback, document_base, lumen_html::css::ContainerUnitContext::default())
    }
    pub(crate) fn canvas_font_set_with_query(
        &self,
        fallback: &FontSet,
        document_base: &str,
        query: lumen_html::css::ContainerUnitContext,
    ) -> Result<Rc<FontSet>, &'static str> {
        let registry_generation = self
            .with_font_registry_snapshot(|snapshot| snapshot.generation)
            .map_err(|_| "font registry snapshot failed")?;
        let resource_generation = self.resource_generation.get();
        if let Some(cache) = self.canvas_font_set.borrow().as_ref() {
            if cache.registry_generation == registry_generation
                && cache.resource_generation == resource_generation
                && cache.document_base == document_base
                && cache.fallback_generation==fallback.generation()
            {
                if cache.query == query { return Ok(cache.fonts.clone()); }
            }
        }
        self.invalidate_canvas_font_set();

        let snapshot = self.copy_font_registry_snapshot()?;
        self.refresh_display_rules(&snapshot.document_css_faces,&snapshot.manual_faces,query.small_viewport);
        let resource_generation=self.resource_generation.get();
        let provider = self.provider.borrow().clone();
        let mut resolved_css_faces = Vec::new();
        resolved_css_faces
            .try_reserve_exact(snapshot.document_css_faces.len())
            .map_err(|_| "font registry allocation failed")?;
        for rule in &snapshot.document_css_faces {
            let connected=rule.identity.as_ref().and_then(|identity|self.sets.borrow().iter()
                .filter_map(Weak::upgrade).find_map(|set|set.borrow().css.get(identity)
                    .filter(|(face,_)|face.rule().sources==rule.sources)
                    .and_then(|(face,_)|face.decoded())));
            resolved_css_faces.push(connected.or_else(||provider.as_ref().and_then(|provider|provider.loaded(rule,document_base))));
        }
        let fallback_registrations = fallback
            .registrations()
            .ok_or("fallback font registrations are unavailable")?;
        let css_display=snapshot.document_css_faces.iter().zip(&resolved_css_faces)
            .map(|(rule,face)|self.display_phase(rule,face.is_some())).collect::<Vec<_>>();
        let manual_display=snapshot.manual_faces.iter()
            .map(|face|self.display_phase(&face.rule,face.status==FontFaceStatus::Loaded)).collect::<Vec<_>>();
        let fonts = Rc::new(FontSet::from_font_registry_snapshot_with_display(
            &fallback_registrations,
            &snapshot.document_css_faces,
            &resolved_css_faces,
            &snapshot.manual_faces,
            query,Some(&css_display),Some(&manual_display),
        )?);
        let current_registry_generation = self
            .with_font_registry_snapshot(|snapshot| snapshot.generation)
            .map_err(|_| "font registry snapshot failed")?;
        let current_resource_generation = self.resource_generation.get();
        if current_registry_generation == snapshot.generation
            && current_resource_generation == resource_generation
        {
            let mut key = String::new();
            key.try_reserve_exact(document_base.len())
                .map_err(|_| "font registry allocation failed")?;
            key.push_str(document_base);
            *self.canvas_font_set.borrow_mut() = Some(CanvasFontSetCache {
                registry_generation: snapshot.generation,
                resource_generation,
                fallback_generation:fallback.generation(),
                document_base: key,
                query,
                fonts: fonts.clone(),
            });
        }
        Ok(fonts)
    }

    fn sync_manual_faces(&self) {
        let Some(provider) = self.provider.borrow().clone() else {
            return;
        };
        let Ok(snapshot) = self.copy_font_registry_snapshot() else {
            return;
        };
        provider.replace_font_registry(&FontRegistrySnapshot {
            generation: snapshot.generation,
            document_css_faces: &snapshot.document_css_faces,
            manual_faces: &snapshot.manual_faces,
        });
    }

    fn notify_started(&self, ctx: &mut Ctx, face: &Rc<FaceData>) {
        let mut sets = self.sets.borrow_mut();
        sets.retain(|weak| {
            let Some(state) = weak.upgrade() else {
                return false;
            };
            state.borrow_mut().face_started(ctx, face);
            true
        });
        drop(sets);
        self.sync_manual_faces();
    }

    fn notify_completed(&self, face: &Rc<FaceData>, success: bool) {
        self.complete_display(face,success);
        let mut sets = self.sets.borrow_mut();
        sets.retain(|weak| {
            let Some(state) = weak.upgrade() else {
                return false;
            };
            state.borrow_mut().face_completed(face, success);
            true
        });
        drop(sets);
        if face.manual_state().is_none() {
            self.advance_resource_generation();
        }
        self.sync_manual_faces();
    }

    /// Run actual font fetch/decoding on a user-agent task, then settle the
    /// engine's native promises. Reactions remain native engine microtasks.
    pub(crate) fn pump(&self, ctx: &mut Ctx, base: &str) -> usize {
        let batches = std::mem::take(&mut *self.batches.borrow_mut());
        let mut count = 0;
        let provider = self.provider.borrow().clone();
        for mut batch in batches {
            let base = batch.base.get_or_insert_with(|| base.to_owned());
            let mut failure = None;
            let mut pending = false;
            let mut changed = false;
            for (face, value) in batch.faces.iter().zip(&batch.values) {
                let prior_status = face.status();
                if matches!(
                    prior_status,
                    FontFaceStatus::Unloaded | FontFaceStatus::Loading
                ) {
                    let source = if face.manual_state().is_some() {
                        match self.begin_manual_load(face) {
                            Ok(Some(request)) => request.source,
                            Ok(None) => face.source(),
                            Err(error) => {
                                let message = format!("{error:?}");
                                let _ = self.fail_manual_face(face, message.clone());
                                if let Some(deferred) = face.loaded_deferred.borrow_mut().take() {
                                    deferred.reject(
                                        ctx,
                                        OpError::new("NetworkError", message.clone()),
                                    );
                                }
                                failure = Some(message);
                                changed = true;
                                continue;
                            }
                        }
                    } else {
                        if prior_status == FontFaceStatus::Unloaded {
                            face.set_css_status(FontFaceStatus::Loading);
                        }
                        face.source()
                    };
                    if prior_status == FontFaceStatus::Unloaded {
                        self.notify_started(ctx, face);
                        changed = true;
                    }
                    let invalid_descriptor =
                        face.invalid_descriptors.borrow().values().next().cloned();
                    let result = if let Some(message) = invalid_descriptor {
                        Err(("SyntaxError", message))
                    } else {
                        match &source {
                            FaceSource::Binary(bytes) => FontFace::new(bytes.clone())
                                .and_then(|font| {
                            let rule = face.rule();
                                    font.with_metric_overrides(
                                        rule.ascent_override,
                                        rule.descent_override,
                                    )
                                })
                                .map(Arc::new)
                                .map_err(|message| ("SyntaxError", message.to_owned())),
                            FaceSource::Url => match provider.as_ref() {
                                Some(provider) => match provider.poll_load_in_context(ctx, &face.load_rule(), base) {
                                    Poll::Ready(result) => {
                                        result.map_err(|message| ("NetworkError", message))
                                    }
                                    Poll::Pending => {
                                        pending = true;
                                        continue;
                                    }
                                },
                                None => Err((
                                    "NetworkError",
                                    "The embedder has not supplied a font resource loader"
                                        .to_owned(),
                                )),
                            },
                        }
                    };
                    match result {
                        Ok(decoded) => {
                            let manual = face.manual_state().is_some();
                            if manual {
                                if let Err(error) = self.complete_manual_load(face, Ok(decoded.clone())) {
                                    let message = format!("{error:?}");
                                    let _ = self.fail_manual_face(face, message);
                                }
                            } else {
                                face.set_css_decoded(Some(decoded));
                                face.set_css_status(FontFaceStatus::Loaded);
                            }
                            changed = true;
                            if face.status() == FontFaceStatus::Loaded {
                                if let Some(deferred) = face.loaded_deferred.borrow_mut().take() {
                                    deferred.resolve(ctx, value.clone());
                                }
                                self.notify_completed(face, true);
                            } else {
                                let message = face
                                    .error()
                                    .unwrap_or_else(|| "manual font byte budget exceeded".into());
                                failure = Some(message.clone());
                                if let Some(deferred) = face.loaded_deferred.borrow_mut().take() {
                                    deferred.reject(ctx, OpError::new("NetworkError", message));
                                }
                                self.notify_completed(face, false);
                            }
                        }
                        Err((name, message)) => {
                            changed = true;
                            if face.manual_state().is_some() {
                                if self
                                    .complete_manual_load(
                                        face,
                                        Err(Arc::<str>::from(message.as_str())),
                                    )
                                    .is_err()
                                {
                                    let _ = self.fail_manual_face(face, message.clone());
                                }
                            } else {
                                face.set_css_error(Some(message.clone()));
                                face.set_css_status(FontFaceStatus::Error);
                            }
                            if let Some(deferred) = face.loaded_deferred.borrow_mut().take() {
                                let error = if name == "SyntaxError" {
                                    font_dom_exception(ctx, Some(self), name, &message)
                                } else {
                                    OpError::new(name, message)
                                };
                                deferred.reject(ctx, error);
                            }
                            self.notify_completed(face, false);
                        }
                    }
                }
                if face.status() == FontFaceStatus::Error {
                    failure = face.error();
                }
            }
            count += usize::from(changed);
            if pending {
                if let Some(message) = failure {
                    if let Some(promise) = batch.promise.take() {
                        promise.reject(ctx, OpError::new("NetworkError", message));
                    }
                }
                self.batches.borrow_mut().push(batch);
                continue;
            }
            if let Some(promise) = batch.promise {
                if let Some(message) = failure {
                    promise.reject(ctx, OpError::new("NetworkError", message));
                } else {
                    promise.resolve(ctx, batch.values);
                }
            }
        }
        count
    }

    fn enqueue_load(
        &self,
        ctx: &mut Ctx,
        faces: Vec<(Rc<FaceData>, Value)>,
        promise: Option<Deferred>,
    ) -> usize {
        if faces.is_empty() {
            if let Some(promise) = promise {
                promise.resolve(ctx, Vec::<Value>::new());
            }
            return 0;
        }
        let base = faces
            .first()
            .and_then(|(face, _)| face.realm.upgrade())
            .map(|realm| realm.base_url());
        let batch_faces = faces.iter().map(|(face, _)| face.clone()).collect();
        let values = faces.into_iter().map(|(_, value)| value).collect();
        self.batches.borrow_mut().push(Batch {
            faces: batch_faces,
            values,
            promise,
            base,
        });
        // Request admission is one owner task. Pending providers wake the owner
        // through completion; never requeue a task merely because I/O is pending.
        if let Some(worker) = ctx.op_state().get::<Rc<WorkerFontContext>>().cloned() {
            if !self.worker_pump_queued.replace(true) {
                let worker = Rc::downgrade(&worker);
                let queued = scheduling::queue_task(ctx, move |ctx| {
                    if let Some(worker) = worker.upgrade() {
                        worker.font_loading.worker_pump_queued.set(false);
                        worker.font_loading.pump(ctx, &worker.base_url);
                        worker.font_loading.settle(ctx)?;
                    }
                    Ok(())
                });
                if queued.is_err() { self.worker_pump_queued.set(false); }
            }
        }
        self.batches.borrow().len()
    }
}


pub(crate) fn font_dom_exception(
    ctx: &mut Ctx,
    font_loading: Option<&FontLoading>,
    name: &'static str,
    message: &str,
) -> OpError {
    let constructor = font_loading
        .and_then(|font_loading| font_loading.dom_exception_constructor.borrow().as_ref().and_then(WeakValue::upgrade))
        .or_else(|| {
            let global = ctx.global_object();
            ctx.get_member(&global, "DOMException")
                .ok()
                .filter(Value::is_callable)
        });
    let exception = constructor
        .filter(|constructor| constructor.is_callable())
        .and_then(|constructor| {
            ctx.construct_value(constructor, &[Value::str(message), Value::str(name)])
                .ok()
        });
    match exception {
        Some(value) => OpError::thrown(value),
        None => OpError::new(name, message.to_owned()),
    }
}

fn css_syntax_error(
    ctx: &mut Ctx,
    font_loading: Option<&FontLoading>,
    error: css::CssError,
) -> OpError {
    font_dom_exception(
        ctx,
        font_loading,
        "SyntaxError",
        &format!(
            "Invalid font descriptor at {}: {}",
            error.offset, error.message
        ),
    )
}

fn member_get(ctx: &mut Ctx, target: &Value, key: &str) -> OpResult<Value> {
    JsObject::from_value(target.clone())
        .ok_or_else(|| OpError::type_error("property target must be an object"))?
        .get(ctx, key)
}

fn descriptor_entries(ctx: &mut Ctx, value: Option<Value>) -> OpResult<Vec<(String, String)>> {
    let Some(value) = value.filter(|value| !matches!(value, Value::Undefined | Value::Null)) else {
        return Ok(Vec::new());
    };
    if !matches!(value, Value::Obj(_)) {
        return Err(OpError::type_error(
            "FontFace descriptors must be an object",
        ));
    }
    let mut entries = Vec::new();
    // Web IDL dictionaries read members in declaration order. The legacy
    // width alias follows stretch, so an explicit width value takes priority.
    for name in [
        "style",
        "weight",
        "stretch",
        "unicodeRange",
        "variant",
        "featureSettings",
        "variationSettings",
        "display",
        "ascentOverride",
        "descentOverride",
        "lineGapOverride",
        "sizeAdjust",
        "width",
    ] {
        let member = member_get(ctx, &value, name)?;
        if matches!(member, Value::Undefined) {
            continue;
        }
        let text = ctx.coerce_string(&member).map_err(OpError::thrown)?;
        entries.push((name.to_owned(), text.to_string()));
    }
    Ok(entries)
}

fn update_rule_descriptor(data: &FaceData, name: &str, value: &str, ctx: &mut Ctx) -> OpResult<()> {
    let identity = data.identity();
    let realm = data.realm.upgrade();
    let font_loading = realm.as_ref().map(FontRealm::font_loading);
    let (new, descriptors, canonical) = {
        let old = data.rule();
        let mut descriptors = old.descriptors.clone();
        descriptors
            .set(name, value)
            .map_err(|error| css_syntax_error(ctx, font_loading, error))?;
        let canonical = descriptors.get(name).unwrap_or_else(|| value.to_owned());
        let mut new = descriptors.to_rule(old.source_url.clone());
        new.identity = Some(identity.clone());
        new.family_display=old.family_display.clone();
        new.family_scope=old.family_scope.clone();
        new.rule_start=old.rule_start;
        new.import_path=old.import_path.clone();
        new.source_revision=old.source_revision;
        new.source_order = old.source_order;
        new.layer = old.layer;
        new.layers = old.layers.clone();
        new.layer_path = old.layer_path;
        new.media = old.media.clone();
        new.supports = old.supports.clone();
        (new, descriptors, canonical)
    };
    let css_identity_is_live = if matches!(&identity, FontFaceIdentity::Css(_)) {
        if let Some(realm) = realm.as_ref().and_then(FontRealm::document) {
            Some(
                realm
                    .session
                    .borrow_mut()
                    .set_font_face_descriptors(&identity, descriptors)
                    .map_err(|error| {
                        OpError::new(
                            "InvalidStateError",
                            format!("could not update CSS font descriptors: {error:?}"),
                        )
                    })?,
            )
        } else {
            None
        }
    } else {
        None
    };

    if let Some(realm) = &realm {
        if matches!(&identity, FontFaceIdentity::Css(_)) {
            if css_identity_is_live == Some(true) {
                realm.font_loading().update_connected_css_rule(&new)?;
            }
            data.set_css_rule(new);
        } else {
            realm.font_loading().update_manual_rule(data, new)?;
        }
    } else if matches!(&identity, FontFaceIdentity::Css(_)) {
        data.set_css_rule(new);
    }
    {
        let mut overrides = data.descriptor_overrides.borrow_mut();
        overrides.retain(|(old_name, _)| old_name != name);
        overrides.push((name.to_owned(), canonical));
    }
    {
        let mut invalid = data.invalid_descriptors.borrow_mut();
        invalid.remove(name);
        if matches!(name, "stretch" | "width") {
            invalid.remove("stretch");
            invalid.remove("width");
        }
    }
    if let Some(realm) = realm {
        if matches!(&identity, FontFaceIdentity::Css(_)) {
            if !css_identity_is_live.unwrap_or(false) {
                // Keep the detached FontFace wrapper's descriptor state, but
                // make sure no cached layout result still reflects its old
                // CSS-connected rule.
                realm.invalidate_fonts();
            }
        } else {
            realm.invalidate_fonts();
            realm.font_loading().sync_manual_faces();
        }
    }
    Ok(())
}

fn descriptor_value(data: &FaceData, name: &str) -> String {
    let invalid = data.invalid_descriptors.borrow();
    if invalid.contains_key(name)
        || matches!(name, "stretch" | "width")
            && (invalid.contains_key("stretch") || invalid.contains_key("width"))
    {
        return String::new();
    }
    drop(invalid);
    data.rule().descriptors.get(name).unwrap_or_default()
}

fn set_descriptor_value(data: &FaceData, name: &str, ctx: &mut Ctx, value: &str) -> OpResult<()> {
    update_rule_descriptor(data, name, value, ctx)
}

fn create_css_face(ctx: &mut Ctx, realm: &FontRealm, rule: FontFaceRule) -> Rc<FaceData> {
    let identity = rule.identity.clone().unwrap_or_else(|| {
        FontFaceIdentity::Manual(0)
    });
    FaceData::new_css(ctx, realm, rule, identity)
}

fn ensure_face_wrapper(ctx: &mut Ctx, face: &Rc<FaceData>) -> Value {
    if let Some(value) = face.wrapper_value() {
        return value;
    }
    let value = ctx.new_instance(DomFontFace { data: face.clone() });
    ctx.set_native_identity_owner::<DomFontFace>(&value).expect("FontFace native owner");
    face.set_wrapper(ctx, &value);
    value
}

#[lumen_bind::class(name = "FontFace", hint(js(webidl)))]
pub(crate) struct DomFontFace {
    data: Rc<FaceData>,
}

impl lumen::embed::NativeIdentityOwner for DomFontFace {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _:u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        visit(&self.data.loaded);
        if let Some(deferred)=self.data.loaded_deferred.borrow().as_ref() { visit(deferred.promise_value()); }
    }
}

struct FontFaceConstructor { face:DomFontFace, start_load:bool }
impl lumen_bind::CtorRet<JsHost, DomFontFace> for FontFaceConstructor {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let data=self.face.data.clone();
        let value=<JsHost as Host>::construct(cx,self.face)?;
        <JsHost as Host>::with_ctx(cx,|ctx| {
            ctx.set_native_identity_owner::<DomFontFace>(&value).expect("FontFace native owner");
            data.set_wrapper(ctx,&value);
            if self.start_load {
                if let Some(realm)=data.realm.upgrade() {
                    realm.font_loading().enqueue_load(ctx,vec![(data.clone(),value.clone())],None);
                }
            }
        });
        Ok(value)
    }
}

#[lumen_bind::methods]
impl DomFontFace {
    #[constructor(coerce)]
    fn new(
        ctx: &mut Ctx,
        family: String,
        source: Value,
        descriptors: Option<Value>,
    ) -> OpResult<FontFaceConstructor> {
        let realm = active_realm(ctx)?;
        let loading = realm.font_loading();
        let base = realm.base_url();
        let entries = descriptor_entries(ctx, descriptors)?;
        let bytes = ctx.buffer_source_bytes(&source);
        let oversized_binary = bytes
            .as_ref()
            .is_some_and(|bytes| bytes.len() > MAX_MANUAL_FONT_BYTES_PER_FACE);
        let source_text = if bytes.is_some() {
            None
        } else {
            Some(
                ctx.coerce_string(&source)
                    .map_err(OpError::thrown)?
                    .to_string(),
            )
        };
        let mut descriptors = FontFaceDescriptors::parse(&family, &[])
            .map_err(|error| css_syntax_error(ctx, Some(&loading), error))?;
        // The FontFace dictionary defaults to normal; CSS @font-face uses
        // the descriptor's auto initial value. Both share the same parser.
        descriptors.set("stretch","normal")
            .map_err(|error| css_syntax_error(ctx, Some(&loading), error))?;
        let mut invalid_descriptors = HashMap::new();
        for (name, value) in &entries {
            if let Err(error) = descriptors.set(name, value) {
                invalid_descriptors.insert(
                    name.clone(),
                    format!(
                        "Invalid font descriptor at {}: {}",
                        error.offset, error.message
                    ),
                );
            }
        }
        if let Some(source_text) = source_text.as_deref() {
            if let Err(error) = descriptors.set("src", source_text) {
                invalid_descriptors.insert(
                    String::from("src"),
                    format!(
                        "Invalid font descriptor at {}: {}",
                        error.offset, error.message
                    ),
                );
            }
        }
        let rule = descriptors.to_rule(Some(Arc::from(base.clone())));
        let source = if oversized_binary {
            FaceSource::Url
        } else {
            bytes.map_or(FaceSource::Url, |bytes| {
                FaceSource::Binary(Arc::from(bytes))
            })
        };
        let initial_error = invalid_descriptors.values().next().cloned();
        let data = loading.create_manual_face(ctx, &realm, rule, source)?;
        *data.invalid_descriptors.borrow_mut() = invalid_descriptors;
        if oversized_binary {
            data.invalid_descriptors
                .borrow_mut()
                .insert("__fontTooLarge".into(), "font too large".into());
        }
        let start_load=initial_error.is_none()&&(oversized_binary||matches!(data.source(),FaceSource::Binary(_)));
        if let Some(message) = initial_error {
            loading.fail_manual_face(&data, message.clone())?;
            if let Some(deferred) = data.loaded_deferred.borrow_mut().take() {
                let error = font_dom_exception(ctx, Some(&loading), "SyntaxError", &message);
                deferred.reject(ctx, error);
            }
        }
        Ok(FontFaceConstructor {face:Self { data },start_load})
    }

    #[getter]
    fn family(&self) -> String {
        let rule = self.data.rule();
        rule
            .descriptors
            .get("family")
            .unwrap_or_else(|| rule.family.to_string())
    }

    #[setter(coerce)]
    fn set_family(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        set_descriptor_value(&self.data, "family", ctx, value)
    }

    #[getter]
    fn loaded(&self) -> Value {
        self.data.loaded.clone()
    }

    fn load(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        // BufferSource faces begin their decode in the constructor's queued
        // task. FontFace.load() only starts URL-backed faces.
        if matches!(self.data.source(), FaceSource::Binary(_)) {
            return Ok(self.data.loaded.clone());
        }
        if self.data.status() == FontFaceStatus::Unloaded {
            let realm = self.data.realm.upgrade().ok_or_else(|| {
                OpError::new("InvalidStateError", "font document is no longer available")
            })?;
            let loading = realm.font_loading();
            let value = this.0;
            self.data.set_wrapper(ctx, &value);
            let started = if self.data.manual_state().is_some() {
                loading.begin_manual_load(&self.data)?.is_some()
            } else {
                self.data.set_css_status(FontFaceStatus::Loading);
                true
            };
            if started {
                loading.notify_started(ctx, &self.data);
                loading.enqueue_load(ctx, vec![(self.data.clone(), value)], None);
            }
        }
        Ok(self.data.loaded.clone())
    }

    #[getter]
    fn status(&self) -> &'static str {
        self.data.status().as_str()
    }

    #[getter]
    fn style(&self) -> String {
        descriptor_value(&self.data, "style")
    }
    #[getter]
    fn weight(&self) -> String {
        descriptor_value(&self.data, "weight")
    }
    #[getter]
    fn stretch(&self) -> String {
        descriptor_value(&self.data, "stretch")
    }
    #[getter]
    fn width(&self) -> String {
        descriptor_value(&self.data, "width")
    }
    #[getter]
    fn unicode_range(&self) -> String {
        descriptor_value(&self.data, "unicodeRange")
    }
    #[getter]
    fn variant(&self) -> String {
        descriptor_value(&self.data, "variant")
    }
    #[getter]
    fn feature_settings(&self) -> String {
        descriptor_value(&self.data, "featureSettings")
    }
    #[getter]
    fn variation_settings(&self) -> String {
        descriptor_value(&self.data, "variationSettings")
    }
    #[getter]
    fn display(&self) -> String {
        descriptor_value(&self.data, "display")
    }
    #[getter]
    fn ascent_override(&self) -> String {
        descriptor_value(&self.data, "ascentOverride")
    }
    #[getter]
    fn descent_override(&self) -> String {
        descriptor_value(&self.data, "descentOverride")
    }
    #[getter]
    fn line_gap_override(&self) -> String {
        descriptor_value(&self.data, "lineGapOverride")
    }
    #[getter]
    fn size_adjust(&self) -> String {
        descriptor_value(&self.data, "sizeAdjust")
    }

    #[setter(coerce)]
    fn set_style(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        set_descriptor_value(&self.data, "style", ctx, value)
    }
    #[setter(coerce)]
    fn set_weight(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        set_descriptor_value(&self.data, "weight", ctx, value)
    }
    #[setter(coerce)]
    fn set_stretch(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        set_descriptor_value(&self.data, "stretch", ctx, value)
    }
    #[setter(coerce)]
    fn set_width(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        set_descriptor_value(&self.data, "width", ctx, value)
    }
    #[setter(coerce)]
    fn set_unicode_range(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        set_descriptor_value(&self.data, "unicodeRange", ctx, value)
    }
    #[setter(coerce)]
    fn set_variant(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        set_descriptor_value(&self.data, "variant", ctx, value)
    }
    #[setter(coerce)]
    fn set_feature_settings(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        set_descriptor_value(&self.data, "featureSettings", ctx, value)
    }
    #[setter(coerce)]
    fn set_variation_settings(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        set_descriptor_value(&self.data, "variationSettings", ctx, value)
    }
    #[setter(coerce)]
    fn set_display(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        set_descriptor_value(&self.data, "display", ctx, value)
    }
    #[setter(coerce)]
    fn set_ascent_override(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        set_descriptor_value(&self.data, "ascentOverride", ctx, value)
    }
    #[setter(coerce)]
    fn set_descent_override(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        set_descriptor_value(&self.data, "descentOverride", ctx, value)
    }
    #[setter(coerce)]
    fn set_line_gap_override(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        set_descriptor_value(&self.data, "lineGapOverride", ctx, value)
    }
    #[setter(coerce)]
    fn set_size_adjust(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        set_descriptor_value(&self.data, "sizeAdjust", ctx, value)
    }
}

fn active_realm(ctx: &mut Ctx) -> OpResult<FontRealm> {
    if let Some(worker) = ctx.op_state().get::<Rc<WorkerFontContext>>().cloned() {
        return Ok(FontRealm::Worker(worker));
    }
    let global = ctx.global_object();
    let document = member_get(ctx, &global, "document")?;
    ctx.with_instance::<DomDocument, _>(&document, |document| FontRealm::Document(document.realm.clone()))
        .map_err(|_| OpError::new("InvalidStateError", "FontFace requires an active document"))
}

#[lumen_bind::class(name="FontDisplayDeadline")]
struct FontDisplayDeadline {owner:WeakFontRealm,generation:u64}
#[lumen_bind::methods]
impl FontDisplayDeadline {}

#[lumen_bind::module(name="font_display_deadline")]
mod display_deadline {
    use super::*;
    #[op]
    pub fn tick(ctx:&mut Ctx,ticket:Value)->OpResult<()> {
        let (owner,generation)=ctx.with_instance::<FontDisplayDeadline,_>(&ticket,
            |ticket|(ticket.owner.clone(),ticket.generation))
            .map_err(|_|OpError::type_error("font display deadline ticket"))?;
        let Some(realm)=owner.upgrade() else {return Ok(());};
        if realm.document().is_some_and(|document|document.is_document_destroyed()) {return Ok(());}
        let loading=realm.font_loading();
        if generation!=loading.display_timer_generation.get() {return Ok(());}
        loading.display_timer.set(None);
        if loading.update_display_clock() {realm.invalidate_fonts();}
        loading.schedule_display_deadline(ctx)?;
        Ok(())
    }
}

struct ReadyState {
    promise: Value,
    deferred: Option<Deferred>,
}

struct FontFaceSetState {
    realm: WeakFontRealm,
    owner: Option<WeakValue>,
    css: HashMap<FontFaceIdentity, (Rc<FaceData>, Value)>,
    css_order: Vec<FontFaceIdentity>,
    css_sync_key: Option<(u64, lumen_html::css::MediaEnvironment)>,
    // Strong JavaScript roots for registry members. The shared registry owns
    // membership and insertion order; this map exists only to keep wrappers
    // alive while the set contains them.
    manual_roots: HashMap<FontFaceIdentity, (Rc<FaceData>, Value)>,
    member_order: HashMap<FontFaceIdentity, u64>,
    next_member_order: u64,
    status: bool,
    ready: Option<ReadyState>,
    initial_layout_pending: bool,
    pending: HashMap<FontFaceIdentity, Value>,
    succeeded: Vec<Value>,
    failed: Vec<Value>,
    lifecycle_queued: bool,
}

impl FontFaceSetState {
    fn new(realm: &FontRealm) -> Self {
        Self {
            realm: realm.downgrade(),
            owner: None,
            css: HashMap::new(),
            css_order: Vec::new(),
            css_sync_key: None,
            manual_roots: HashMap::new(),
            member_order: HashMap::new(),
            next_member_order: 0,
            status: false,
            ready: None,
            initial_layout_pending: false,
            pending: HashMap::new(),
            succeeded: Vec::new(),
            failed: Vec::new(),
            lifecycle_queued: false,
        }
    }

    fn attach(&mut self, ctx: &mut Ctx, owner: &Value) {
        self.owner = ctx.weak_value(owner);
        if self.ready.is_none() {
            let deferred = Deferred::new(ctx);
            let promise = deferred.promise();
            self.ready = Some(ReadyState {
                promise,
                deferred: Some(deferred),
            });
            self.initial_layout_pending = true;
        }
    }

    fn register_member(&mut self, identity: &FontFaceIdentity) -> u64 {
        if let Some(order) = self.member_order.get(identity) {
            return *order;
        }
        self.next_member_order = self.next_member_order.wrapping_add(1).max(1);
        let order = self.next_member_order;
        self.member_order.insert(identity.clone(), order);
        order
    }

    fn find_value(&self, identity: &FontFaceIdentity) -> Option<Value> {
        self.css
            .get(identity)
            .map(|(_, value)| value.clone())
            .or_else(|| self.manual_roots.get(identity).map(|(_, value)| value.clone()))
    }

    fn contains_face(&self,face:&Rc<FaceData>)->bool {
        let identity=face.identity();
        self.css.get(&identity).or_else(||self.manual_roots.get(&identity))
            .is_some_and(|(current,_)|Rc::ptr_eq(current,face))
    }

    fn face_started(&mut self, ctx: &mut Ctx, face: &Rc<FaceData>) {
        let identity = face.identity();
        if !self.contains_face(face) {return;}
        let Some(value) = self.find_value(&identity) else {
            return;
        };
        self.pending.insert(identity, value);
        if !self.status {
            self.status = true;
            self.initial_layout_pending = false;
            let pending_ready = self
                .ready
                .as_ref()
                .is_some_and(|ready| ready.deferred.is_some());
            if !pending_ready {
                let deferred = Deferred::new(ctx);
                self.ready = Some(ReadyState {
                    promise: deferred.promise(),
                    deferred: Some(deferred),
                });
            }
            self.lifecycle_queued = false;
            self.succeeded.clear();
            self.failed.clear();
            if let Some(owner) = self.owner.as_ref().and_then(WeakValue::upgrade) {
                let _ = scheduling::queue_task(ctx, move |ctx| {
                    dispatch_load_event(ctx, owner, "loading", Vec::new())
                });
            }
        }
    }

    fn face_completed(&mut self, face: &Rc<FaceData>, success: bool) {
        let identity = face.identity();
        if !self.contains_face(face) {return;}
        let value = self
            .pending
            .remove(&identity)
            .or_else(|| self.find_value(&identity));
        if let Some(value) = value {
            let entries = if success {
                &mut self.succeeded
            } else {
                &mut self.failed
            };
            if !entries.iter().any(|old| same_js_value(old, &value)) {
                entries.push(value);
            }
        }
        self.lifecycle_queued = false;
    }

    fn ordered(&self, manual_members: &[FontFaceIdentity]) -> Vec<(Rc<FaceData>, Value)> {
        let mut entries = self
            .css_order
            .iter()
            .filter_map(|identity| self.css.get(identity).cloned())
            .collect::<Vec<_>>();
        entries.extend(
            manual_members
                .iter()
                .filter_map(|identity| self.manual_roots.get(identity).cloned()),
        );
        entries
    }

    fn ordered_with_order(
        &mut self,
        manual_members: &[FontFaceIdentity],
    ) -> Vec<(u64, Rc<FaceData>, Value)> {
        let entries = self.ordered(manual_members);
        entries
            .into_iter()
            .map(|(face, value)| {
                let order = self.register_member(&face.identity());
                (order, face, value)
            })
            .collect()
    }
}

fn same_js_value(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Obj(a), Value::Obj(b)) => std::ptr::eq(&**a, &**b),
        _ => false,
    }
}

#[lumen_bind::class(name = "FontFaceSet", extends = DomEventTarget, hint(js(webidl)))]
pub(crate) struct DomFontFaceSet {
    base: DomEventTarget,
    state: Rc<RefCell<FontFaceSetState>>,
}

impl lumen::embed::NativeIdentityOwner for DomFontFaceSet {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _:u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        self.base.trace_callback_values(visit);
        let state=self.state.borrow();
        for (_,value) in state.css.values().chain(state.manual_roots.values()) {visit(value);}
        for value in state.pending.values().chain(state.succeeded.iter()).chain(state.failed.iter()) {visit(value);}
        if let Some(ready)=state.ready.as_ref() {
            visit(&ready.promise);
            if let Some(deferred)=ready.deferred.as_ref() {visit(deferred.promise_value());}
        }
    }
}

impl DomFontFaceSet {
    pub(crate) fn new(realm: &Rc<DomRealm>) -> Self {
        Self::for_context(&FontRealm::Document(realm.clone()))
    }

    fn for_context(realm: &FontRealm) -> Self {
        let state = Rc::new(RefCell::new(FontFaceSetState::new(realm)));
        realm.font_loading().register_set(&state);
        Self {
            base: realm.document().map(DomEventTarget::independent).unwrap_or_else(DomEventTarget::new),
            state,
        }
    }

    pub(crate) fn attach(&self, ctx: &mut Ctx, owner: &Value) {
        ctx.set_native_identity_owner::<DomFontFaceSet>(owner).expect("FontFaceSet native owner");
        self.state.borrow_mut().attach(ctx, owner);
    }

    fn realm(&self) -> OpResult<FontRealm> {
        self.state
            .borrow()
            .realm
            .upgrade()
            .ok_or_else(|| OpError::new("InvalidStateError", "font document is unavailable"))
    }

    fn descriptors(realm: &DomRealm) -> OpResult<Vec<FontFaceRule>> {
        let mut session = realm.session.borrow_mut();
        let environment = session.media_environment();
        let mut faces = session.font_faces().map_err(|error| {
            OpError::new("InvalidStateError", format!("font stylesheet: {error:?}"))
        })?;
        faces.retain(|face| face.applies(environment));
        Ok(faces)
    }

    fn sync_css(&self, ctx: &mut Ctx, realm: &FontRealm) -> OpResult<()> {
        sync_css_state(ctx, realm, &self.state)
    }

    fn snapshot_entries(&self, ctx: &mut Ctx) -> OpResult<Vec<(Rc<FaceData>, Value)>> {
        let realm = self.realm()?;
        self.sync_css(ctx, &realm)?;
        let manual_members = realm.font_loading().manual_member_identities()?;
        Ok(self.state.borrow().ordered(&manual_members))
    }

    fn publish_manual_snapshot(&self) {
        if let Some(realm) = self.state.borrow().realm.upgrade() {
            realm.font_loading().sync_manual_faces();
        }
    }

    fn queue_face_load(
        &self,
        ctx: &mut Ctx,
        realm: &FontRealm,
        face: Rc<FaceData>,
        value: Value,
    ) {
        if matches!(
            face.status(),
            FontFaceStatus::Loaded | FontFaceStatus::Error
        ) {
            return;
        }
        if face.status() == FontFaceStatus::Unloaded {
            if !face.invalid_descriptors.borrow().is_empty() {
                if face.manual_state().is_some() {
                    if realm.font_loading().begin_manual_load(&face).ok().flatten().is_none() {
                        return;
                    }
                } else {
                    face.set_css_status(FontFaceStatus::Loading);
                }
                realm.font_loading().notify_started(ctx, &face);
                return;
            }
            let cached = realm.font_loading()
                .provider
                .borrow()
                .as_ref()
                .and_then(|provider| provider.loaded(&face.rule(), &realm.base_url()));
            if let Some(decoded) = cached {
                if face.manual_state().is_some() {
                    if realm.font_loading().begin_manual_load(&face).ok().flatten().is_none() {
                        return;
                    }
                } else {
                    face.set_css_status(FontFaceStatus::Loading);
                }
                realm.font_loading().notify_started(ctx, &face);
                if face.manual_state().is_some() {
                    if realm.font_loading()
                        .complete_manual_load(&face, Ok(decoded))
                        .is_err()
                    {
                        return;
                    }
                } else {
                    face.set_css_decoded(Some(decoded));
                    face.set_css_status(FontFaceStatus::Loaded);
                }
                if face.status() == FontFaceStatus::Loaded {
                    if let Some(deferred) = face.loaded_deferred.borrow_mut().take() {
                        deferred.resolve(ctx, value.clone());
                    }
                    realm.font_loading().notify_completed(&face, true);
                } else {
                    if let Some(deferred) = face.loaded_deferred.borrow_mut().take() {
                        deferred.reject(
                            ctx,
                            OpError::new(
                                "NetworkError",
                                face.error().unwrap_or_else(|| "font load failed".into()),
                            ),
                        );
                    }
                    realm.font_loading().notify_completed(&face, false);
                }
                return;
            }
            if face.manual_state().is_some() {
                if realm.font_loading().begin_manual_load(&face).ok().flatten().is_none() {
                    return;
                }
            } else {
                face.set_css_status(FontFaceStatus::Loading);
            }
            realm.font_loading().notify_started(ctx, &face);
        }
    }

    fn values_for_iterator(&self, ctx: &mut Ctx, entries: bool) -> OpResult<Value> {
        self.snapshot_entries(ctx)?;
        let state = Rc::downgrade(&self.state);
        let realm = self.state.borrow().realm.clone();
        let owner=self.state.borrow().owner.as_ref().and_then(WeakValue::upgrade)
            .ok_or_else(||OpError::new("InvalidStateError","FontFaceSet owner is unavailable"))?;
        let value=ctx.new_instance(DomFontFaceSetIterator {
            owner:RefCell::new(Some(owner)),
            state,
            realm,
            entries,
            seen: RefCell::new(std::collections::HashSet::new()),
        });
        ctx.set_native_identity_owner::<DomFontFaceSetIterator>(&value)?;
        Ok(value)
    }
}

fn sync_css_state(
    ctx: &mut Ctx,
    realm: &FontRealm,
    set: &Rc<RefCell<FontFaceSetState>>,
) -> OpResult<()> {
    let Some(document) = realm.document() else { return Ok(()); };
    let state = set;
    let key = {
        let mut session = document.session.borrow_mut();
        let generation = session.font_face_generation().map_err(|error| {
            OpError::new("InvalidStateError", format!("font stylesheet: {error:?}"))
        })?;
        (generation, session.media_environment())
    };
    if state.borrow().css_sync_key == Some(key) {
        return Ok(());
    }
    let rules = DomFontFaceSet::descriptors(document)?;
    let mut state = state.borrow_mut();
    let mut order = Vec::new();
    order
        .try_reserve_exact(rules.len())
        .map_err(|_| OpError::new("QuotaExceededError", "font registry allocation failed"))?;
    for rule in rules {
        let identity = rule.identity.clone().ok_or_else(|| {
            OpError::new(
                "InvalidStateError",
                "CSS font face is missing its stable identity",
            )
        })?;
        if state.css.get(&identity).is_some_and(|(face,_)|face.rule().sources!=rule.sources) {
            let (old,value)=state.css.get(&identity).cloned().expect("existing connected face");
            old.detach_css(realm.font_loading()).map_err(|error|OpError::new("QuotaExceededError",error))?;
            state.css.remove(&identity);
            state.pending.remove(&identity);
            state.succeeded.retain(|entry|!same_js_value(entry,&value));
            state.failed.retain(|entry|!same_js_value(entry,&value));
            state.member_order.remove(&identity);
        }
        if let Some((face, _)) = state.css.get(&identity) {
            let mut rule = rule;
            for (name, value) in face.descriptor_overrides.borrow().iter() {
                if let Err(error) = rule.descriptors.set(name, value) {
                    return Err(css_syntax_error(ctx, Some(realm.font_loading()), error));
                }
            }
            face.set_css_rule(rule);
        } else {
            let face = create_css_face(ctx, realm, rule);
            let value = ensure_face_wrapper(ctx, &face);
            state.css.insert(identity.clone(), (face, value));
        }
        state.register_member(&identity);
        order.push(identity);
    }
    if state.css.len() > order.len() {
        let live = order
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        let removed = state
            .css
            .iter()
            .filter(|(identity, _)| !live.contains(*identity))
            .map(|(identity, (_, value))| (identity.clone(), value.clone()))
            .collect::<Vec<_>>();
        for (identity, removed_value) in removed {
            if let Some((face,_))=state.css.get(&identity) {
                face.detach_css(realm.font_loading()).map_err(|error|OpError::new("QuotaExceededError",error))?;
            }
            state.css.remove(&identity);
            state.pending.remove(&identity);
            state
                .succeeded
                .retain(|item| !same_js_value(item, &removed_value));
            state
                .failed
                .retain(|item| !same_js_value(item, &removed_value));
            state.member_order.remove(&identity);
        }
    }
    state.css_order = order;
    let mut css_faces = Vec::new();
    css_faces
        .try_reserve_exact(state.css_order.len())
        .map_err(|_| OpError::new("QuotaExceededError", "font registry allocation failed"))?;
    css_faces.extend(state.css_order.iter().filter_map(|identity| {
        state.css.get(identity).map(|(face, _)| face.rule())
    }));
    drop(state);
    realm.font_loading().replace_document_css_faces(&css_faces)?;
    realm.font_loading().canvas_css_key.set(Some(key));
    set.borrow_mut().css_sync_key = Some(key);
    Ok(())
}

fn next_live_entry(
    ctx: &mut Ctx,
    realm: &FontRealm,
    state: &Rc<RefCell<FontFaceSetState>>,
    seen: &mut std::collections::HashSet<u64>,
) -> OpResult<Option<(u64, Value)>> {
    sync_css_state(ctx, realm, state)?;
    let manual_members = realm.font_loading().manual_member_identities()?;
    let mut state = state.borrow_mut();
    for (generation, _face, value) in state.ordered_with_order(&manual_members) {
        if seen.insert(generation) {
            return Ok(Some((generation, value)));
        }
    }
    Ok(None)
}

#[lumen_bind::methods]
impl DomFontFaceSet {
    #[getter]
    fn size(&self, ctx: &mut Ctx) -> OpResult<usize> {
        Ok(self.snapshot_entries(ctx)?.len())
    }

    #[getter]
    fn status(&self) -> &'static str {
        if self.state.borrow().status {
            "loading"
        } else {
            "loaded"
        }
    }

    #[getter]
    fn ready(&self) -> Value {
        self.state
            .borrow()
            .ready
            .as_ref()
            .map(|ready| ready.promise.clone())
            .unwrap_or(Value::Undefined)
    }

    fn has(&self, ctx: &mut Ctx, face: Value) -> OpResult<bool> {
        let identity = ctx
            .with_instance::<DomFontFace, _>(&face, |face| face.data.identity())
            .ok();
        let Some(identity) = identity else {
            return Ok(false);
        };
        Ok(self
            .snapshot_entries(ctx)?
            .iter()
            .any(|(face, _)| face.identity() == identity))
    }

    fn add(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>, face: Value) -> OpResult<Value> {
        let data = ctx
            .with_instance::<DomFontFace, _>(&face, |face| face.data.clone())
            .map_err(|_| OpError::type_error("FontFaceSet.add requires a FontFace"))?;
        let realm = self.realm()?;
        let identity = data.identity();
        if matches!(identity, FontFaceIdentity::Css(_)) {
            return Err(font_dom_exception(
                ctx,
                Some(realm.font_loading()),
                "InvalidModificationError",
                "A CSS-connected FontFace cannot be added to a FontFaceSet",
            ));
        }
        if let Some(face_realm) = data.realm.upgrade() {
            if !realm.same_context(&face_realm) {
                return Err(font_dom_exception(
                    ctx,
                    Some(realm.font_loading()),
                    "InvalidModificationError",
                    "FontFace belongs to a different document",
                ));
            }
        }
        {
            let mut state = self.state.borrow_mut();
            if !state.manual_roots.contains_key(&identity) {
                state
                    .manual_roots
                    .try_reserve(1)
                    .map_err(|_| OpError::new("QuotaExceededError", "font set allocation failed"))?;
            }
            realm.font_loading().add_manual_face(&data)?;
            state
                .manual_roots
                .entry(identity.clone())
                .or_insert_with(|| (data.clone(), face.clone()));
            state.register_member(&identity);
        }
        if data.status() == FontFaceStatus::Loading {
            self.state.borrow_mut().face_started(ctx, &data);
        }
        self.publish_manual_snapshot();
        Ok(this.0)
    }

    fn delete(&self, ctx: &mut Ctx, face: Value) -> OpResult<bool> {
        let data = ctx
            .with_instance::<DomFontFace, _>(&face, |face| face.data.clone())
            .ok();
        let Some(data) = data else {
            return Ok(false);
        };
        let _ = self.snapshot_entries(ctx)?;
        let identity = data.identity();
        if matches!(identity, FontFaceIdentity::Css(_)) {
            return Ok(false);
        }
        let realm = self.realm()?;
        let Some(face_realm) = data.realm.upgrade() else {
            return Ok(false);
        };
        if !realm.same_context(&face_realm) {
            return Ok(false);
        }
        let removed = realm.font_loading().delete_manual_face(&data)?;
        let mut state = self.state.borrow_mut();
        let removed_value = state.manual_roots.remove(&identity).map(|(_, value)| value);
        if removed || removed_value.is_some() {
            state.member_order.remove(&identity);
            state.pending.remove(&identity);
            if let Some(value) = removed_value {
                state.succeeded.retain(|item| !same_js_value(item, &value));
                state.failed.retain(|item| !same_js_value(item, &value));
            }
        }
        drop(state);
        if removed {
            self.publish_manual_snapshot();
        }
        Ok(removed)
    }

    fn clear(&self, ctx: &mut Ctx) -> OpResult<()> {
        let _ = self.snapshot_entries(ctx)?;
        let realm = self.realm()?;
        for (face,_) in self.state.borrow().manual_roots.values() {
            face.preserve_display(realm.font_loading(),&face.identity());
        }
        realm.font_loading().clear_manual_faces();
        {
            let mut state = self.state.borrow_mut();
            let roots = std::mem::take(&mut state.manual_roots);
            for (identity, (_, value)) in roots {
                state.member_order.remove(&identity);
                state.pending.remove(&identity);
                state.succeeded.retain(|item| !same_js_value(item, &value));
                state.failed.retain(|item| !same_js_value(item, &value));
            }
        }
        self.publish_manual_snapshot();
        Ok(())
    }

    #[method(coerce)]
    fn check(
        &self,
        ctx: &mut Ctx,
        font: &str,
        #[default(" ".to_owned())] text: String,
    ) -> OpResult<bool> {
        let realm = self.realm()?;
        let mut spec = css::parse_font_shorthand(font).ok_or_else(|| {
            font_dom_exception(
                ctx,
                Some(realm.font_loading()),
                "SyntaxError",
                "Invalid font shorthand",
            )
        })?;
        resolve_initial_font_selection(&realm, &mut spec)?;
        let faces = self.snapshot_entries(ctx)?;
        let mut rules = faces
            .iter()
            .map(|(face, _)| face.rule())
            .collect::<Vec<_>>();
        resolve_initial_width_descriptors(&realm, &mut rules)?;
        let indices = css::matching_font_faces(&spec, &rules, &text);
        let provider = realm.font_loading().provider.borrow().clone();
        let base = realm.base_url();
        Ok(indices.into_iter().all(|index| {
            let face = &faces[index].0;
            face.status() == FontFaceStatus::Loaded
                || face.invalid_descriptors.borrow().is_empty()
                    && provider
                        .as_ref()
                        .is_some_and(|provider| provider.loaded(&rules[index], &base).is_some())
        }))
    }

    #[method(coerce)]
    fn load(
        &self,
        ctx: &mut Ctx,
        font: &str,
        #[default(" ".to_owned())] text: String,
    ) -> Promise<Vec<Value>> {
        let result = (|| -> OpResult<(FontRealm, Vec<(Rc<FaceData>, Value)>)> {
            let realm = self.realm()?;
            let mut spec = css::parse_font_shorthand(font).ok_or_else(|| {
                font_dom_exception(
                    ctx,
                    Some(realm.font_loading()),
                    "SyntaxError",
                    "Invalid font shorthand",
                )
            })?;
            resolve_initial_font_selection(&realm, &mut spec)?;
            let faces = self.snapshot_entries(ctx)?;
            let mut rules = faces
                .iter()
                .map(|(face, _)| face.rule())
                .collect::<Vec<_>>();
            resolve_initial_width_descriptors(&realm, &mut rules)?;
            let indices = css::matching_font_faces(&spec, &rules, &text);
            let selected = indices
                .into_iter()
                .map(|index| faces[index].clone())
                .collect::<Vec<_>>();
            Ok((realm, selected))
        })();
        match result {
            Err(error) => Promise::rejected(error),
            Ok((_, faces)) if faces.is_empty() => Promise::resolved(Vec::<Value>::new()),
            Ok((realm, faces)) => {
                let deferred = Deferred::new(ctx);
                let promise = Promise::pending(&deferred);
                for (face, _) in &faces {
                    deferred.reject_on(ctx, &face.loaded);
                    let wrapper = ensure_face_wrapper(ctx, face);
                    self.queue_face_load(ctx, &realm, face.clone(), wrapper);
                }
                realm.font_loading().enqueue_load(ctx, faces, Some(deferred));
                promise
            }
        }
    }

    fn keys(&self, ctx: &mut Ctx) -> OpResult<Value> {
        self.values_for_iterator(ctx, false)
    }

    fn values(&self, ctx: &mut Ctx) -> OpResult<Value> {
        self.values_for_iterator(ctx, false)
    }

    fn entries(&self, ctx: &mut Ctx) -> OpResult<Value> {
        self.values_for_iterator(ctx, true)
    }

    #[proto(iter)]
    fn iter(&self, ctx: &mut Ctx) -> OpResult<Value> {
        self.values_for_iterator(ctx, false)
    }

    fn for_each(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: JsFunction,
        this_arg: Option<Value>,
    ) -> OpResult<()> {
        let realm = self.realm()?;
        self.snapshot_entries(ctx)?;
        let mut seen = std::collections::HashSet::new();
        while let Some((_generation, value)) = next_live_entry(ctx, &realm, &self.state, &mut seen)?
        {
            callback.call(
                ctx,
                this_arg.clone().unwrap_or(Value::Undefined),
                &[value.clone(), value, this.0.clone()],
            )?;
        }
        Ok(())
    }

    #[getter]
    fn onloading(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.handler_value(ctx, &this.0, "loading")
    }

    #[setter]
    fn set_onloading(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        handler: crate::events::EventHandler,
    ) {
        self.base.set_event_handler(ctx, &this.0, "loading", handler);
    }

    #[getter]
    fn onloadingdone(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.handler_value(ctx, &this.0, "loadingdone")
    }

    #[setter]
    fn set_onloadingdone(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        handler: crate::events::EventHandler,
    ) {
        self.base.set_event_handler(ctx, &this.0, "loadingdone", handler);
    }

    #[getter]
    fn onloadingerror(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.handler_value(ctx, &this.0, "loadingerror")
    }

    #[setter]
    fn set_onloadingerror(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        handler: crate::events::EventHandler,
    ) {
        self.base.set_event_handler(ctx, &this.0, "loadingerror", handler);
    }
}

#[lumen_bind::class(name = "FontFaceSetIterator")]
pub(crate) struct DomFontFaceSetIterator {
    owner: RefCell<Option<Value>>,
    state: Weak<RefCell<FontFaceSetState>>,
    realm: WeakFontRealm,
    entries: bool,
    seen: RefCell<std::collections::HashSet<u64>>,
}

impl lumen::embed::NativeIdentityOwner for DomFontFaceSetIterator {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _:u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        if let Some(owner)=self.owner.borrow().as_ref() {visit(owner);}
    }
}

#[lumen_bind::methods]
impl DomFontFaceSetIterator {
    #[proto(iter)]
    fn iter(&self, this: lumen_bind::This<Value>) -> Value {
        this.0
    }

    #[proto(next)]
    fn next(&self, ctx: &mut Ctx) -> OpResult<Option<Value>> {
        if self.owner.borrow().is_none() {return Ok(None);}
        let Some(state) = self.state.upgrade() else {
            self.owner.borrow_mut().take();
            return Ok(None);
        };
        let Some(realm) = self.realm.upgrade() else {
            self.owner.borrow_mut().take();
            return Ok(None);
        };
        let next = next_live_entry(ctx, &realm, &state, &mut self.seen.borrow_mut())?;
        let Some((_generation, value)) = next else {
            self.owner.borrow_mut().take();
            return Ok(None);
        };
        if !self.entries {
            return Ok(Some(value));
        }
        Ok(Some(JsHost::from_list(ctx, vec![value.clone(), value])))
    }
}

#[lumen_bind::class(name = "FontFaceSetLoadEvent", extends = DomEvent, hint(js(webidl)))]
pub(crate) struct DomFontFaceSetLoadEvent {
    base: DomEvent,
    font_faces: Value,
}

impl lumen::embed::NativeIdentityOwner for DomFontFaceSetLoadEvent {
    const TRACES_NATIVE_VALUES:bool=true;
    fn trace_native_identities(&self,epoch:u64,visit:&mut dyn FnMut(&Value)) {
        lumen::embed::NativeIdentityOwner::trace_native_identities(&self.base,epoch,visit);
    }
    fn trace_native_values(&self,visit:&mut dyn FnMut(&Value)) {
        lumen::embed::NativeIdentityOwner::trace_native_values(&self.base,visit);
        visit(&self.font_faces);
    }
}

struct FontLoadEventConstructor(DomFontFaceSetLoadEvent);
impl lumen_bind::CtorRet<JsHost,DomFontFaceSetLoadEvent> for FontLoadEventConstructor {
    fn into_ctor(self,cx:&<JsHost as Host>::Cx<'_>)->Result<Value,Value> {
        let value=<JsHost as Host>::construct(cx,self.0)?;
        <JsHost as Host>::with_ctx(cx,|ctx|ctx.set_native_identity_owner::<DomFontFaceSetLoadEvent>(&value)
            .expect("FontFaceSetLoadEvent native owner"));
        Ok(value)
    }
}

impl DomFontFaceSetLoadEvent {
    fn create(ctx: &mut Ctx, kind: &str, init: Option<Value>) -> OpResult<Self> {
        let base = DomEvent::new(ctx, kind, init.clone())?;
        let font_faces = match init {
            Some(init) if matches!(init, Value::Obj(_)) => member_get(ctx, &init, "fontfaces")?,
            _ => Value::Undefined,
        };
        let realm = active_realm(ctx)?;
        let font_faces = fontfaces_sequence(ctx, font_faces, Some(realm.font_loading()))?;
        Ok(Self { base, font_faces })
    }
}

#[lumen_bind::methods]
impl DomFontFaceSetLoadEvent {
    #[constructor]
    fn new(ctx:&mut Ctx,kind:&str,init:Option<Value>)->OpResult<FontLoadEventConstructor> {
        Self::create(ctx,kind,init).map(FontLoadEventConstructor)
    }

    #[getter(rename(js = "fontfaces"))]
    fn font_faces(&self) -> Value {
        self.font_faces.clone()
    }
}

fn fontfaces_sequence(
    ctx: &mut Ctx,
    value: Value,
    font_loading: Option<&FontLoading>,
) -> OpResult<Value> {
    let faces = if matches!(value, Value::Undefined) {
        Vec::new()
    } else {
        ctx.convert_iterable(&value, 65_536, |ctx, face| {
            ctx.with_instance::<DomFontFace, _>(&face, |_| face.clone())
                .map_err(|_| OpError::type_error("fontfaces contains a non-FontFace value"))
        })?
    };
    let array = create_fontfaces_array(ctx, &faces)?;
    freeze_fontfaces_array(ctx, font_loading, array)
}

fn create_fontfaces_array(ctx: &mut Ctx, values: &[Value]) -> OpResult<Value> {
    Ok(JsHost::from_list(ctx, values.to_vec()))
}

fn freeze_fontfaces_array(
    ctx: &mut Ctx,
    font_loading: Option<&FontLoading>,
    array: Value,
) -> OpResult<Value> {
    let cached = font_loading.and_then(|font_loading| font_loading.object_freeze.borrow().as_ref().and_then(WeakValue::upgrade).and_then(JsFunction::from_value));
    let freeze = cached.or_else(|| {
        let global = ctx.global_object();
        let object = ctx.get_member(&global, "Object").ok()?;
        JsFunction::from_value(ctx.get_member(&object, "freeze").ok()?)
    });
    let freeze = freeze.ok_or_else(|| OpError::type_error("Object.freeze is unavailable"))?;
    freeze.call(ctx, Value::Undefined, &[array.clone()])?;
    Ok(array)
}

fn dispatch_load_event(
    ctx: &mut Ctx,
    owner: Value,
    kind: &'static str,
    values: Vec<Value>,
) -> OpResult<()> {
    let faces = create_fontfaces_array(ctx, &values)?;
    let init = Value::Obj(ctx.new_object());
    ctx.member_set(&init, "fontfaces", faces)
        .map_err(OpError::thrown)?;
    let event = DomFontFaceSetLoadEvent::create(ctx, kind, Some(init))?;
    let value=ctx.new_instance(event);
    ctx.set_native_identity_owner::<DomFontFaceSetLoadEvent>(&value)?;
    let event = JsObject::from_value(value)
        .ok_or_else(|| OpError::type_error("FontFaceSetLoadEvent is not an object"))?;
    crate::events::dispatch_user_agent_event(ctx, lumen_bind::This(owner), event).map(|_| ())
}

impl FontFaceSetState {
    fn settle(
        &mut self,
        ctx: &mut Ctx,
        state_weak: Weak<RefCell<FontFaceSetState>>,
    ) -> OpResult<bool> {
        if !self.status {
            if self.lifecycle_queued {
                return Ok(false);
            }
            if !self.initial_layout_pending {
                return Ok(true);
            }
            self.initial_layout_pending = false;
            if let (Some(owner), Some(ready)) = (
                self.owner.as_ref().and_then(WeakValue::upgrade),
                self.ready.as_mut().and_then(|ready| ready.deferred.take()),
            ) {
                ready.resolve(ctx, owner);
            }
            return Ok(true);
        }
        if !self.pending.is_empty() || self.lifecycle_queued {
            return Ok(false);
        }
        let Some(owner) = self.owner.as_ref().and_then(WeakValue::upgrade) else {
            self.status = false;
            if let Some(ready) = self.ready.as_mut().and_then(|ready| ready.deferred.take()) {
                ready.resolve(ctx, Value::Undefined);
            }
            return Ok(true);
        };
        self.lifecycle_queued = true;
        let success = std::mem::take(&mut self.succeeded);
        let failed = std::mem::take(&mut self.failed);
        let ready = self.ready.as_mut().and_then(|ready| ready.deferred.take());
        self.status = false;
        if let Some(ready) = ready {
            ready.resolve(ctx, owner.clone());
        }
        scheduling::queue_task(ctx, move |ctx| {
            let result: OpResult<()> = (|| {
                dispatch_load_event(ctx, owner.clone(), "loadingdone", success)?;
                if !failed.is_empty() {
                    dispatch_load_event(ctx, owner, "loadingerror", failed)?;
                }
                Ok(())
            })();
            if let Some(state) = state_weak.upgrade() {
                state.borrow_mut().lifecycle_queued = false;
            }
            result
        })?;
        Ok(false)
    }
}

impl FontLoading {
    pub(crate) fn settle(&self, ctx: &mut Ctx) -> OpResult<bool> {
        let states = {
            let mut sets = self.sets.borrow_mut();
            let mut states = Vec::new();
            sets.retain(|weak| {
                if let Some(state) = weak.upgrade() {
                    states.push(state);
                    true
                } else {
                    false
                }
            });
            states
        };
        let provider = self.provider.borrow().clone();
        for state in &states {
            let Some(realm) = state.borrow().realm.upgrade() else {
                continue;
            };
            sync_css_state(ctx, &realm, state)?;
            let manual_members = self.manual_member_identities()?;
            let base = realm.base_url();
            let entries = state
                .borrow()
                .ordered(&manual_members)
                .into_iter()
                .collect::<Vec<_>>();
            for (face, value) in entries {
                if face.status() != FontFaceStatus::Unloaded {
                    continue;
                }
                if !face.invalid_descriptors.borrow().is_empty() {
                    continue;
                }
                let Some(result) = provider
                    .as_ref()
                    .and_then(|provider| provider.automatic_load(&face.rule(), &base))
                else {
                    continue;
                };
                if face.manual_state().is_some() {
                    if self.begin_manual_load(&face)?.is_none() {
                        continue;
                    }
                } else {
                    face.set_css_status(FontFaceStatus::Loading);
                }
                self.notify_started(ctx, &face);
                let Poll::Ready(Ok(decoded)) = result else {
                    self.enqueue_load(ctx, vec![(face, value)], None);
                    continue;
                };
                if face.manual_state().is_some() {
                    self.complete_manual_load(&face, Ok(decoded))?;
                } else {
                    face.set_css_decoded(Some(decoded));
                    face.set_css_status(FontFaceStatus::Loaded);
                }
                if face.status() == FontFaceStatus::Loaded {
                    if let Some(deferred) = face.loaded_deferred.borrow_mut().take() {
                        deferred.resolve(ctx, value);
                    }
                    self.notify_completed(&face, true);
                } else {
                    let message = face
                        .error()
                        .unwrap_or_else(|| "manual font byte budget exceeded".into());
                    if let Some(deferred) = face.loaded_deferred.borrow_mut().take() {
                        deferred.reject(ctx, OpError::new("NetworkError", message));
                    }
                    self.notify_completed(&face, false);
                }
            }
        }
        let mut quiescent = true;
        for state in states {
            let state_weak = Rc::downgrade(&state);
            quiescent &= state.borrow_mut().settle(ctx, state_weak)?;
        }
        Ok(quiescent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;
    use lumen_runtime::Runtime;

    #[test]
    fn specification_font_display_first_use_deadlines_and_live_policy_share_owner_tasks() {
        struct Provider {calls:Cell<usize>}
        impl FontResourceLoader for Provider {
            fn load(&self,_:&FontFaceRule,_:&str)->Result<Arc<FontFace>,String> {Err("pending provider must be polled".into())}
            fn poll_load(&self,_:&FontFaceRule,_:&str)->Poll<Result<Arc<FontFace>,String>> {
                self.calls.set(self.calls.get()+1);Poll::Pending
            }
        }
        let mut engine=Engine::new();
        lumen_timers::install(&mut engine,64);
        let realm=install_with_layout(&mut engine,"<!doctype html><style>@font-face{font-family:Pending;src:url(pending.ttf);font-display:block}</style><span id=x style='font:20px Pending'>A </span>");
        realm.set_document_url("https://display.test/page.html");
        let provider=Rc::new(Provider{calls:Cell::new(0)});
        realm.set_font_resource_loader(provider.clone());
        let source=super::super::canvas::realm_font_source(&realm).unwrap();
        assert!(realm.font_loading.display_faces.borrow().is_empty(),"snapshot discovery cannot start a download timer");
        let spec=FontSpec{families:Some(Arc::from([lumen_html::paint::FontFamily::from("Pending")])),..FontSpec::default()};
        let hidden=source.shape_resolved("A ",20.,false,&spec).unwrap();
        assert_eq!(provider.calls.get(),0,"block shaping cannot initiate I/O");
        assert_eq!(realm.font_loading.display_faces.borrow().len(),1);
        let started=realm.font_loading.display_faces.borrow().values().next().unwrap().timeline.clone();
        assert_eq!(started.phase(),lumen_html::font_display::DisplayPhase::Block);
        assert!(hidden.glyphs.iter().all(|glyph|source.rasterize_glyph(glyph.face,glyph.id,20.).unwrap().alpha.is_empty()));
        realm.queue_font_tasks(engine.ctx()).unwrap();
        assert_eq!(provider.calls.get(),1);
        assert!(realm.font_loading.display_timer.get().is_some(),"the actual canonical timer heap owns one presentation wakeup");
        boolean(&mut engine,"globalThis.pendingFace=[...document.fonts][0];pendingFace.status==='loading'");
        boolean(&mut engine,"pendingFace.display='swap';pendingFace.status==='loading'");
        let visible=source.shape_resolved("A ",20.,false,&spec).unwrap();
        assert_eq!(hidden.width,visible.width);
        assert_eq!(hidden.glyphs.iter().map(|glyph|(glyph.id,glyph.cluster,glyph.x,glyph.y)).collect::<Vec<_>>(),
            visible.glyphs.iter().map(|glyph|(glyph.id,glyph.cluster,glyph.x,glyph.y)).collect::<Vec<_>>());
        assert!(visible.glyphs.iter().any(|glyph|!source.rasterize_glyph(glyph.face,glyph.id,20.).unwrap().alpha.is_empty()));
        realm.queue_font_tasks(engine.ctx()).unwrap();
        assert!(realm.font_loading.display_timer.get().is_none(),"infinite swap cancels the obsolete block deadline");
        boolean(&mut engine,"pendingFace.status==='loading'");
    }

    #[test]
    fn specification_font_family_display_defaults_reach_canonical_native_first_use() {
        struct Provider;
        impl FontResourceLoader for Provider {
            fn load(&self,_:&FontFaceRule,_:&str)->Result<Arc<FontFace>,String>{Err("pending".into())}
            fn poll_load(&self,_:&FontFaceRule,_:&str)->Poll<Result<Arc<FontFace>,String>>{Poll::Pending}
        }
        let mut engine=Engine::new();
        let realm=install_with_layout(&mut engine,"<!doctype html><style>@font-feature-values Omitted, Explicit {font-display:swap} @font-face{font-family:Omitted;src:url(omitted.ttf)} @font-face{font-family:Explicit;src:url(explicit.ttf);font-display:auto}</style>");
        realm.set_font_resource_loader(Rc::new(Provider));
        let source=realm.render_font_source().unwrap();
        let spec=|family:&str|FontSpec{families:Some(Arc::from([lumen_html::paint::FontFamily::from(family)])),..FontSpec::default()};
        let visible=source.shape_resolved("A",20.,false,&spec("Omitted")).unwrap();
        assert!(visible.glyphs.iter().any(|glyph|!source.rasterize_glyph(glyph.face,glyph.id,20.).unwrap().alpha.is_empty()),"family swap exposes genuine fallback ink on first use");
        let hidden=source.shape_resolved("A",20.,false,&spec("Explicit")).unwrap();
        assert_eq!(visible.width,hidden.width);
        assert!(hidden.glyphs.iter().all(|glyph|source.rasterize_glyph(glyph.face,glyph.id,20.).unwrap().alpha.is_empty()),"explicit auto keeps its own block policy");
        realm.queue_font_tasks(engine.ctx()).unwrap();
        boolean(&mut engine,"Array.from(document.fonts).every(face=>face.status==='loading')");
    }

    #[test]
    fn specification_font_display_failure_survives_manual_set_removal_and_reinsertion() {
        struct Provider {ready:Cell<bool>}
        impl FontResourceLoader for Provider {
            fn load(&self,_:&FontFaceRule,_:&str)->Result<Arc<FontFace>,String> {Err("pending provider".into())}
            fn poll_load(&self,_:&FontFaceRule,_:&str)->Poll<Result<Arc<FontFace>,String>> {if self.ready.get() {Poll::Ready(FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).map(Arc::new).map_err(str::to_owned))} else {Poll::Pending}}
        }
        let mut engine=Engine::new();
        let realm=install_with_layout(&mut engine,"<!doctype html><main></main>");
        let provider=Rc::new(Provider{ready:Cell::new(false)});
        realm.set_font_resource_loader(provider.clone());
        boolean(&mut engine,"globalThis.face=new FontFace('Optional','url(optional.ttf)',{display:'optional'});document.fonts.add(face);face.status==='unloaded'");
        let source=realm.render_font_source().unwrap();
        let spec=FontSpec{families:Some(Arc::from([lumen_html::paint::FontFamily::from("Optional")])),..FontSpec::default()};
        source.shape_resolved("A",20.,false,&spec).unwrap();
        realm.queue_font_tasks(engine.ctx()).unwrap();
        let deadline=realm.font_loading.display_now()+200;
        assert!(realm.font_loading.advance_display_at(deadline));
        let fallback=source.shape_resolved("A",20.,false,&spec).unwrap();
        assert!(fallback.glyphs.iter().any(|glyph|!source.rasterize_glyph(glyph.face,glyph.id,20.).unwrap().alpha.is_empty()));
        boolean(&mut engine,"document.fonts.delete(face)&&face.status==='loading'");
        assert!(realm.font_loading.display_faces.borrow().is_empty(),"inactive metadata is owned by the retained FontFace, not a document cache");
        boolean(&mut engine,"document.fonts.add(face);face.status==='loading'");
        assert_eq!(realm.font_loading.display_faces.borrow().values().next().unwrap().timeline.phase(),
            lumen_html::font_display::DisplayPhase::Failure,"reinsertion cannot restart a failed first-use clock");
        assert_eq!(source.shape_resolved("A",20.,false,&spec).unwrap().width,fallback.width);
        provider.ready.set(true);
        realm.queue_font_tasks(engine.ctx()).unwrap();
        boolean(&mut engine,"face.status==='loaded' && document.fonts.has(face)");
        let late=source.shape_resolved("A",20.,false,&spec).unwrap();
        assert_eq!(late.width,fallback.width);
        assert_eq!(late.glyphs.iter().map(|glyph|glyph.face).collect::<Vec<_>>(),
            fallback.glyphs.iter().map(|glyph|glyph.face).collect::<Vec<_>>(),
            "late resource success resolves loading but cannot revive the optional presentation face");
    }

    #[test]
    fn specification_font_display_src_replacement_detaches_old_face_and_preserves_pending_identity() {
        struct Provider {seen:RefCell<Vec<Arc<[css::FontFaceSource]>>>}
        impl FontResourceLoader for Provider {
            fn load(&self,_:&FontFaceRule,_:&str)->Result<Arc<FontFace>,String> {Err("pending".into())}
            fn poll_load(&self,rule:&FontFaceRule,_:&str)->Poll<Result<Arc<FontFace>,String>> {
                self.seen.borrow_mut().push(rule.sources.clone());Poll::Pending
            }
        }
        let mut engine=Engine::new();
        let realm=install_with_layout(&mut engine,"<!doctype html><style>@font-face{font-family:Pending;src:url(first.ttf);font-display:block}</style><span>A</span>");
        realm.set_document_url("https://display.test/page.html");
        let provider=Rc::new(Provider{seen:RefCell::new(Vec::new())});
        realm.set_font_resource_loader(provider.clone());
        boolean(&mut engine,"globalThis.oldFace=[...document.fonts][0];oldFace.load();oldFace.status==='loading'");
        realm.queue_font_tasks(engine.ctx()).unwrap();
        let first=provider.seen.borrow()[0].clone();
        boolean(&mut engine,"document.styleSheets[0].cssRules[0].style.setProperty('src','url(second.ttf)');globalThis.newFace=[...document.fonts][0];newFace!==oldFace && newFace.status==='unloaded' && oldFace.status==='loading'");
        boolean(&mut engine,"oldFace.display='swap';document.styleSheets[0].cssRules[0].style.getPropertyValue('font-display')==='block' && newFace.display==='block'");
        realm.queue_font_tasks(engine.ctx()).unwrap();
        assert!(provider.seen.borrow().iter().all(|source|source==&first),"in-flight detached load retains its original source request");
        boolean(&mut engine,"document.fonts.add(oldFace);document.fonts.has(oldFace) && document.fonts.has(newFace) && document.fonts.size===2");
    }

    #[test]
    fn specification_font_native_owners_preserve_iterator_faces_and_release_retired_realms() {
        let mut engine=Engine::new();
        let _parent=crate::install(engine.ctx(),"<main></main>",128).unwrap();
        let child=engine.ctx().create_host_realm();
        let (weak_document,weak_global,iterator,retired)=engine.ctx().with_host_realm(&child,|ctx| {
            let realm=crate::install(ctx,"<main></main>",128).unwrap();
            let global=ctx.global_object();
            let weak_global=ctx.weak_value(&global).expect("font origin global");
            let iterator=ctx.eval_in_realm(&global,r#"(() => {
                const face=new FontFace('RetainedNativeFont','url(missing.woff)');
                const set=document.fonts;
                set.add(face);
                return set.values();
            })()"#).ok().expect("create actual font iterator");
            let retired=realm.retire_browsing_context_group(ctx);
            (Rc::downgrade(&realm),weak_global,iterator,retired)
        }).expect("font child realm");
        for handle in retired {engine.ctx().dispose_host_realm(&handle).expect("dispose retired font realm");}
        drop(child);
        engine.collect_garbage();
        assert!(weak_document.upgrade().is_some(),"a retained set iterator preserves its associated document");
        let next=engine.ctx().member_get(&iterator,"next").ok().and_then(JsFunction::from_value).expect("font iterator next");
        let result=next.call(engine.ctx(),iterator.clone(),&[]).ok().expect("retained font iterator remains usable");
        let face=engine.ctx().member_get(&result,"value").ok().expect("font iterator member");
        let family=engine.ctx().member_get(&face,"family").ok().expect("retained font descriptor");
        assert_eq!(engine.ctx().coerce_string(&family).ok().expect("family string").to_string(),"RetainedNativeFont");
        let loaded=engine.ctx().member_get(&face,"loaded").ok().expect("retained loaded promise");
        assert!(matches!(loaded,Value::Obj(_)),"loaded retains actual Promise identity");
        drop(loaded);drop(family);drop(face);drop(result);drop(next);drop(iterator);
        engine.collect_garbage();
        assert!(weak_global.upgrade().is_none(),"font native promises and membership are internal owner edges");
        assert!(weak_document.upgrade().is_none(),"released font iterator does not pin its origin document");
    }

    #[test]
    fn font_face_set_selection_uses_initial_fonts_and_document_fallback_axes() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(),
            "<html style='font-size:64px;writing-mode:vertical-rl'><body><div style='container-type:size;width:1px;height:1px'></div></body></html>", 128).unwrap();
        let source = FontRealm::Document(realm.clone());
        let mut em = css::parse_font_shorthand("oblique calc(sign(1em - 20px)*5deg) 1cqw serif").unwrap();
        resolve_initial_font_selection(&source, &mut em).unwrap();
        assert_eq!(em.style.angle(), Some(-5.0));
        let environment = realm.with_session(|session| session.media_environment());
        let threshold = environment.height / 200.0;
        let mut query = css::parse_font_shorthand(&format!("oblique calc(sign(1cqi - {threshold}px)*5deg) 16px serif")).unwrap();
        resolve_initial_font_selection(&source, &mut query).unwrap();
        assert_eq!(query.style.angle(), Some(5.0));
        assert!(query.unresolved_style.is_none());
    }

    struct Provider {
        calls: Cell<usize>,
        fail: bool,
    }
    impl FontResourceLoader for Provider {
        fn load(&self, _rule: &FontFaceRule, _base: &str) -> Result<Arc<FontFace>, String> {
            self.calls.set(self.calls.get() + 1);
            if self.fail {
                return Err("font response is unavailable".into());
            }
            FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES))
                .map(Arc::new)
                .map_err(str::to_owned)
        }
    }

    fn boolean(engine: &mut Engine, source: &str) {
        let result = engine.eval_value(source).expect("valid font test script");
        if !matches!(&result, Ok(Value::Bool(true))) {
            let value = match result { Ok(value) | Err(value) => value };
            let diagnostic = engine.ctx().coerce_string(&value).ok()
                .map(|message| message.to_string()).unwrap_or_else(|| "unprintable result".into());
            panic!("font assertion failed: {source}; result: {diagnostic}");
        }
    }

    fn install_worker_test(engine: &mut Engine, provider: Rc<dyn FontResourceLoader>) -> Rc<WorkerFontContext> {
        canvas::install_worker(engine.ctx()).unwrap();
        install_worker_fonts(engine.ctx(), "https://example.test/redirected/worker.js", provider).unwrap()
    }

    fn worker_checkpoint(engine: &mut Engine) {
        for _ in 0..8 {
            if !scheduling::task_pending(engine.ctx()) { break; }
            assert!(scheduling::run_tasks(engine, 32).is_empty());
        }
        engine.ctx().drain_microtasks_for_host();
    }

    #[test]
    fn specification_computed_metric_font_intents_use_owner_tasks_and_shared_face_lifecycle() {
        struct MetricProvider { polls: Cell<usize>, ready: Cell<bool>, loaded: RefCell<Option<Arc<FontFace>>> }
        impl FontResourceLoader for MetricProvider {
            fn load(&self,_:&FontFaceRule,_:&str)->Result<Arc<FontFace>,String> {Err("owner task required".into())}
            fn poll_load_in_context(&self,_:&mut Ctx,rule:&FontFaceRule,base:&str)->Poll<Result<Arc<FontFace>,String>> {
                assert_eq!(rule.family.as_ref(),"Metrics");
                assert_eq!(base,"https://metrics.test/page.html");
                self.polls.set(self.polls.get()+1);
                if !self.ready.get() {return Poll::Pending;}
                let face=Arc::new(FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap());
                *self.loaded.borrow_mut()=Some(face.clone());Poll::Ready(Ok(face))
            }
            fn loaded(&self,rule:&FontFaceRule,_:&str)->Option<Arc<FontFace>> {
                (rule.family.as_ref()=="Metrics").then(||self.loaded.borrow().clone()).flatten()
            }
        }
        let mut engine=Engine::new();
        let realm=install_with_layout(&mut engine,"<!doctype html><style>@font-face{font-family:Metrics;src:url(font.ttf)}@font-face{font-family:Excluded;src:url(never.ttf);unicode-range:U+0041}#e{font:20px Metrics; border-spacing:2ex 3ex}</style><table id=e></table>");
        realm.set_document_url("https://metrics.test/page.html");
        let provider=Rc::new(MetricProvider{polls:Cell::new(0),ready:Cell::new(false),loaded:RefCell::new(None)});
        realm.set_font_resource_loader(provider.clone());
        let source=super::super::canvas::realm_font_source(&realm).unwrap();
        let spec=FontSpec{families:Some(Arc::from([lumen_html::paint::FontFamily::from("Metrics")])),..FontSpec::default()};
        source.font_relative_metrics_styled(20.,&spec);
        source.font_relative_metrics_styled(20.,&spec);
        assert_eq!(provider.polls.get(),0,"computed/renderer metric reads cannot execute provider I/O");
        assert_eq!(realm.font_loading.metric_requests.borrow().len(),1,"pending selections share actual complete specification identity");
        realm.queue_font_tasks(engine.ctx()).unwrap();
        assert_eq!(provider.polls.get(),1,"the existing owning task initiates the selected font");
        boolean(&mut engine,"Array.from(document.fonts).find(f=>f.family==='Metrics').status==='loading'");
        provider.ready.set(true);
        realm.queue_font_tasks(engine.ctx()).unwrap();
        realm.settle_font_loading(engine.ctx()).unwrap();
        worker_checkpoint(&mut engine);
        boolean(&mut engine,"Array.from(document.fonts).find(f=>f.family==='Metrics').status==='loaded' && Array.from(document.fonts).find(f=>f.family==='Excluded').status==='unloaded'");
        let source=super::super::canvas::realm_font_source(&realm).unwrap();
        let actual=source.font_relative_metrics_styled(20.,&spec);
        let expected=provider.loaded.borrow().as_ref().unwrap().font_relative_metrics_styled(20.,&spec);
        assert_eq!(actual,expected,"new registry/resource generation exposes actual decoded metrics");
        realm.queue_font_tasks(engine.ctx()).unwrap();
        let polls=provider.polls.get();
        source.font_relative_metrics_styled(20.,&spec);
        assert!(!realm.font_loading.has_metric_requests(),"warm metric reads reuse the current generation selection");
        realm.queue_font_tasks(engine.ctx()).unwrap();
        assert_eq!(provider.polls.get(),polls,"already loaded metrics cannot restart provider work");
    }

    #[test]
    fn specification_metric_font_intents_enter_child_owner_and_retirement_preserves_explicit_tasks() {
        struct OwnerProvider { origin: usize, polls: Cell<usize> }
        impl FontResourceLoader for OwnerProvider {
            fn load(&self,_:&FontFaceRule,_:&str)->Result<Arc<FontFace>,String> {Err("owner task required".into())}
            fn poll_load_in_context(&self,ctx:&mut Ctx,_:&FontFaceRule,base:&str)->Poll<Result<Arc<FontFace>,String>> {
                assert_eq!(ctx.global_object().object_identity(),Some(self.origin),"metric wrappers and provider callbacks belong to the child settings realm");
                assert_eq!(base,"https://child-metrics.test/page.html");
                self.polls.set(self.polls.get()+1);
                Poll::Ready(FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).map(Arc::new).map_err(str::to_owned))
            }
        }
        let mut engine=Engine::new();
        let _parent=install(engine.ctx(),"<main></main>",64).unwrap();
        let child=engine.ctx().create_host_realm();
        let (realm,provider)=engine.ctx().with_host_realm(&child,|ctx| {
            let realm=install(ctx,"<style>@font-face{font-family:ChildMetrics;src:url(child.ttf)}</style>",64).unwrap();
            realm.set_document_url("https://child-metrics.test/page.html");
            let provider=Rc::new(OwnerProvider{origin:ctx.global_object().object_identity().expect("child identity"),polls:Cell::new(0)});
            realm.set_font_resource_loader(provider.clone());
            (realm,provider)
        }).expect("child font settings");
        let spec=FontSpec{families:Some(Arc::from([lumen_html::paint::FontFamily::from("ChildMetrics")])),..FontSpec::default()};
        let source=super::super::canvas::realm_font_source(&realm).unwrap();
        source.font_relative_metrics_styled(20.,&spec);
        realm.queue_font_tasks(engine.ctx()).unwrap();
        assert_eq!(provider.polls.get(),1,"a parent task pump enters the real child owner for metric work");
        let retired=engine.ctx().with_host_realm(&child,|ctx| {
            let global=ctx.global_object();
            ctx.eval_in_realm(&global,"globalThis.explicitFace=new FontFace('Explicit','url(explicit.ttf)'); explicitFace.load();").ok().expect("queue explicit author font request");
            // Queue a new current-generation layout selection before destruction.
            realm.font_loading.request_metric_font(&FontSpec::default());
            assert!(realm.font_loading.has_metric_requests());
            let retired=realm.retire_browsing_context_group(ctx);
            realm.queue_font_tasks(ctx).unwrap();
            assert!(!realm.font_loading.has_metric_requests(),"destruction discards layout-only selections");
            assert!(realm.font_loading.metric_request_memo.borrow().is_empty(),"retirement releases bounded selection metadata");
            retired
        }).expect("retire actual child context");
        assert_eq!(provider.polls.get(),2,"new metric policy does not suppress the established explicit FontFace pump");
        for handle in retired {engine.ctx().dispose_host_realm(&handle).expect("dispose retired child font context");}
    }

    #[test]
    fn specification_rendered_font_demands_share_dom_canvas_selection_and_bound_metadata() {
        struct DemandProvider { calls: RefCell<Vec<String>>, fonts: RefCell<HashMap<String,Arc<FontFace>>> }
        impl FontResourceLoader for DemandProvider {
            fn load(&self,rule:&FontFaceRule,_:&str)->Result<Arc<FontFace>,String> {
                self.calls.borrow_mut().push(rule.family.to_string());
                let font=Arc::new(FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap());
                self.fonts.borrow_mut().insert(rule.family.to_string(),font.clone());Ok(font)
            }
            fn loaded(&self,rule:&FontFaceRule,_:&str)->Option<Arc<FontFace>> {self.fonts.borrow().get(rule.family.as_ref()).cloned()}
        }
        let mut engine=Engine::new();
        let realm=install_with_layout(&mut engine,"<style>@font-face{font-family:SourceA;src:url(a.ttf);unicode-range:U+0041}@font-face{font-family:SourceB;src:url(b.ttf);unicode-range:U+0042}@font-face{font-family:Excluded;src:url(unused.ttf);unicode-range:U+6C34}</style><canvas id=c></canvas>");
        let provider=Rc::new(DemandProvider{calls:RefCell::new(Vec::new()),fonts:RefCell::new(HashMap::new())});
        realm.set_font_resource_loader(provider.clone());
        let spec=css::parse_font_shorthand("20px SourceA, SourceB, Excluded").unwrap();
        let source=realm.render_font_source().unwrap();
        source.shape_resolved_segment_with_cluster_advances("BA",1..2,20.,false,&spec,None).unwrap();
        assert!(provider.calls.borrow().is_empty(),"native measurement never invokes the resource provider");
        assert_eq!(realm.font_loading.metric_requests.borrow()[0].scalars,vec!['A'],"only the shaped source slice demands glyph coverage");
        realm.queue_font_tasks(engine.ctx()).unwrap();
        assert_eq!(&*provider.calls.borrow(),&["SourceA"],"unicode-range excludes context-only and unused faces");
        boolean(&mut engine,"const cx=c.getContext('2d'); cx.font='20px SourceA, SourceB, Excluded'; cx.measureText('AB'); true");
        assert_eq!(provider.calls.borrow().len(),1,"canvas records the same owner intent without I/O");
        realm.queue_font_tasks(engine.ctx()).unwrap();
        assert_eq!(&*provider.calls.borrow(),&["SourceA","SourceB"]);
        realm.settle_font_loading(engine.ctx()).unwrap();worker_checkpoint(&mut engine);
        boolean(&mut engine,"Array.from(document.fonts).filter(f=>f.status==='loaded').length===2 && Array.from(document.fonts).find(f=>f.family==='Excluded').status==='unloaded'");
        let source=realm.render_font_source().unwrap();
        source.shape_styled("AB",20.,false,&spec).unwrap();realm.queue_font_tasks(engine.ctx()).unwrap();
        for _ in 0..8 {source.shape_resolved("AB",20.,false,&spec).unwrap();}
        assert!(!realm.font_loading.has_metric_requests(),"warm glyph demand has no new allocations or owner requests");
        assert_eq!(provider.calls.borrow().len(),2);
        let pending=realm.font_loading.metric_requests.borrow();let memo=realm.font_loading.metric_request_memo.borrow();
        assert!(pending.len()+memo.len()<=MAX_FONT_DEMAND_SPECS);
        assert!(pending.iter().map(|entry|entry.scalars.len()).sum::<usize>()+memo.iter().map(|(_,entry)|entry.scalars.len()).sum::<usize>()<=MAX_FONT_DEMAND_SCALARS);
        drop(memo);drop(pending);
        let oversized=(0..=0x10FFFF).filter_map(char::from_u32).take(MAX_FONT_DEMAND_SCALARS+1).collect::<String>();
        assert!(realm.font_loading.request_rendered_font(&FontSpec::default(),&oversized).is_err(),"unknown scalars cannot be marked satisfied on exhausted admission");
        assert!(realm.font_loading.metric_request_memo.borrow().iter().all(|(_,entry)|entry.spec!=FontSpec::default()),"failed admission does not add a satisfied cache entry");
    }

    #[test]
    fn specification_worker_canvas_font_demand_uses_original_set_and_owner_pump() {
        let mut engine=Engine::new();let provider=Rc::new(Provider{calls:Cell::new(0),fail:false});
        let worker=install_worker_test(&mut engine,provider.clone());
        boolean(&mut engine,"globalThis.workerFace=new FontFace('WorkerDemand','url(worker.ttf)'); fonts.add(workerFace); globalThis.originalFonts=fonts; globalThis.surface=new OffscreenCanvas(40,40); globalThis.workerCtx=surface.getContext('2d'); workerCtx.font='20px WorkerDemand'; Object.defineProperty(globalThis,'fonts',{value:null,configurable:true}); workerCtx.measureText('A'); workerFace.status==='unloaded'");
        assert_eq!(provider.calls.get(),0,"worker canvas cannot perform resource I/O during shaping");
        assert!(worker.font_loading.has_metric_requests());
        poll_worker_fonts(engine.ctx()).unwrap();worker_checkpoint(&mut engine);
        boolean(&mut engine,"workerFace.status==='loaded' && originalFonts.has(workerFace) && fonts===null");
        assert_eq!(provider.calls.get(),1,"the original traced set owner survives public property replacement");
    }

    #[test]
    fn specification_rendered_font_demands_retry_after_real_cmap_or_source_failure() {
        struct CoverageProvider { calls: RefCell<Vec<String>>, fonts: RefCell<HashMap<String,Arc<FontFace>>> }
        impl FontResourceLoader for CoverageProvider {
            fn load(&self,rule:&FontFaceRule,_:&str)->Result<Arc<FontFace>,String> {
                self.calls.borrow_mut().push(rule.family.to_string());
                if rule.family.as_ref()=="Broken" {return Err("actual unavailable font resource".into());}
                let bytes=if rule.family.as_ref()=="Arabic" {include_bytes!("../../lumen-html-text/tests/fixtures/shaping/NotoNaskhArabic-regular.woff2").as_slice()} else {lumen_html_text::DEFAULT_FONT_BYTES};
                let font=Arc::new(FontFace::new(Arc::from(bytes)).unwrap());
                self.fonts.borrow_mut().insert(rule.family.to_string(),font.clone());Ok(font)
            }
            fn loaded(&self,rule:&FontFaceRule,_:&str)->Option<Arc<FontFace>> {self.fonts.borrow().get(rule.family.as_ref()).cloned()}
        }
        let mut engine=Engine::new();
        let realm=install_with_layout(&mut engine,"<style>@font-face{font-family:Broken;src:url(missing.ttf)}@font-face{font-family:Latin;src:url(latin.ttf)}@font-face{font-family:Arabic;src:url(arabic.woff2)}</style>");
        let provider=Rc::new(CoverageProvider{calls:RefCell::new(Vec::new()),fonts:RefCell::new(HashMap::new())});realm.set_font_resource_loader(provider.clone());
        let spec=css::parse_font_shorthand("20px Broken, Latin, Arabic").unwrap();
        for expected in ["Broken","Latin","Arabic"] {
            realm.render_font_source().unwrap().shape_resolved("ل",20.,true,&spec).unwrap();
            realm.queue_font_tasks(engine.ctx()).unwrap();
            assert_eq!(provider.calls.borrow().last().map(String::as_str),Some(expected));
        }
        assert!(!provider.fonts.borrow()["Latin"].covers_cluster("ل"),"the first decoded resource really lacks the demanded glyph");
        assert!(provider.fonts.borrow()["Arabic"].covers_cluster("ل"),"the fallback resource supplies genuine Arabic coverage");
        realm.settle_font_loading(engine.ctx()).unwrap();worker_checkpoint(&mut engine);
        let run=realm.render_font_source().unwrap().shape_resolved("ل",20.,true,&spec).unwrap();assert!(run.glyphs.iter().all(|glyph|glyph.id!=0));
        realm.queue_font_tasks(engine.ctx()).unwrap();
        assert_eq!(provider.calls.borrow().len(),3,"failed and decoded-missing sources are not refetched");
    }

    #[test]
    fn window_offscreen_canvas_keeps_owner_font_registry_and_worker_context_brand() {
        struct CachedProvider { loaded: RefCell<Option<Arc<FontFace>>> }
        impl FontResourceLoader for CachedProvider {
            fn load(&self, _: &FontFaceRule, _: &str) -> Result<Arc<FontFace>, String> {
                let face = Arc::new(FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).map_err(str::to_owned)?);
                *self.loaded.borrow_mut() = Some(face.clone());
                Ok(face)
            }
            fn loaded(&self, _: &FontFaceRule, _: &str) -> Option<Arc<FontFace>> { self.loaded.borrow().clone() }
        }
        let mut engine = Engine::new();
        let realm = install_with_layout(&mut engine, "<style>@font-face {font-family: OwnerCSS; src:url(owner.ttf)}</style><main></main>");
        realm.set_document_url("https://example.test/page.html");
        realm.set_font_resource_loader(Rc::new(CachedProvider { loaded: RefCell::new(None) }));
        boolean(&mut engine, r#"(() => {
            globalThis.ownerCanvas = new OffscreenCanvas(128, 64);
            globalThis.ownerContext = ownerCanvas.getContext('2d');
            ownerContext.font = '20px OwnerCSS, sans-serif';
            globalThis.ownerFallback = ownerContext.measureText('iiii').width;
            globalThis.ownerCssReady = false;
            document.fonts.load('20px OwnerCSS').then(() => { ownerCssReady = true; });
            return ownerContext instanceof OffscreenCanvasRenderingContext2D &&
                !(ownerContext instanceof CanvasRenderingContext2D) && ownerContext.canvas === ownerCanvas;
        })()"#);
        assert_eq!(realm.queue_font_tasks(engine.ctx()).unwrap(), 1);
        realm.settle_font_loading(engine.ctx()).unwrap();
        worker_checkpoint(&mut engine);
        boolean(&mut engine, r#"(() => {
            if (!ownerCssReady || ownerContext.measureText('iiii').width === ownerFallback) throw new Error('Window OffscreenCanvas missed CSS-loaded owner font');
            ownerContext.fillText('iiii', 0, 20);
            ownerContext.strokeText('iiii', 0, 40);
            if (!ownerContext.getImageData(0, 0, 128, 64).data.some(byte => byte !== 0)) throw new Error('owner font did not render');
            globalThis.manualOwner = new FontFace('OwnerManual', 'url(owner.ttf)');
            document.fonts.add(manualOwner); manualOwner.load();
            return true;
        })()"#);
        realm.queue_font_tasks(engine.ctx()).unwrap();
        realm.settle_font_loading(engine.ctx()).unwrap();
        worker_checkpoint(&mut engine);
        boolean(&mut engine, r#"(() => {
            ownerContext.font = '20px OwnerManual, sans-serif';
            const loaded = ownerContext.measureText('iiii').width;
            if (loaded === ownerFallback) throw new Error('Window OffscreenCanvas missed manual owner font');
            document.fonts.delete(manualOwner);
            return ownerContext.measureText('iiii').width === ownerFallback;
        })()"#);
    }

    #[test]
    fn worker_fonts_share_native_set_lifecycle_and_live_canvas_registry() {
        let mut engine = Engine::new();
        let provider = Rc::new(Provider { calls: Cell::new(0), fail: false });
        let _worker = install_worker_test(&mut engine, provider.clone());
        boolean(&mut engine, "typeof document === 'undefined' && fonts === globalThis.fonts && fonts.size === 0");
        boolean(&mut engine, r#"(() => {
            globalThis.fontEvents = [];
            fonts.onloading = () => fontEvents.push('loading');
            fonts.onloadingdone = event => fontEvents.push('done:' + event.fontfaces.length);
            globalThis.workerFace = new FontFace('Worker Test', 'url(font.woff2)');
            globalThis.canvasContext = new OffscreenCanvas(128, 64).getContext('2d');
            canvasContext.font = '20px "Worker Test", sans-serif';
            if (canvasContext.font !== '20px "Worker Test", sans-serif') throw new Error('canvas rejected an author font-family list');
            globalThis.fallbackWidth = canvasContext.measureText('iiii').width;
            globalThis.oldReady = fonts.ready;
            fonts.add(workerFace);
            globalThis.workerLoaded = false;
            globalThis.workerLoad = workerFace.load();
            workerLoad.then(face => { workerLoaded = face === workerFace; });
            globalThis.readyResolved = false;
            fonts.ready.then(set => { readyResolved = set === fonts; });
            return fonts.has(workerFace) && fonts.size === 1 &&
                workerFace.status === 'loading' && workerLoad === workerFace.loaded &&
                fonts.ready !== oldReady && [...fonts][0] === workerFace;
        })()"#);
        worker_checkpoint(&mut engine);
        assert_eq!(provider.calls.get(), 1);
        boolean(&mut engine, r#"(() => {
            const loadedWidth = canvasContext.measureText('iiii').width;
            canvasContext.fillText('iiii', 0, 20, 40);
            canvasContext.strokeText('iiii', 0, 40, 40);
            const drawn = canvasContext.getImageData(0, 0, 128, 64).data.some(byte => byte !== 0);
            const failures = [];
            if (!workerLoaded) failures.push('FontFace.load identity/reaction');
            if (!readyResolved) failures.push('FontFaceSet.ready identity/reaction');
            if (fonts.status !== 'loaded') failures.push('set status=' + fonts.status);
            if (fontEvents.join(',') !== 'loading,done:1') failures.push('events=' + fontEvents.join(','));
            if (loadedWidth === fallbackWidth) failures.push('live font width=' + loadedWidth + ', fallback=' + fallbackWidth);
            if (!drawn) failures.push('fill/stroke produced no pixels');
            if (!fonts.delete(workerFace)) failures.push('delete returned false');
            if (canvasContext.measureText('iiii').width !== fallbackWidth) failures.push('delete did not invalidate canvas fonts');
            return failures.length ? failures.join('; ') : true;
        })()"#);
        boolean(&mut engine, r#"(() => {
            fonts.add(workerFace); fonts.clear();
            return fonts.size === 0 && canvasContext.measureText('iiii').width === fallbackWidth;
        })()"#);
    }

    #[test]
    fn worker_font_pending_requests_keep_base_and_do_not_reschedule_polling() {
        struct PendingProvider { ready: Cell<bool>, calls: Cell<usize>, bases: RefCell<Vec<String>> }
        impl FontResourceLoader for PendingProvider {
            fn load(&self, _: &FontFaceRule, _: &str) -> Result<Arc<FontFace>, String> { unreachable!() }
            fn poll_load(&self, _: &FontFaceRule, base: &str) -> Poll<Result<Arc<FontFace>, String>> {
                self.calls.set(self.calls.get() + 1);
                self.bases.borrow_mut().push(base.to_owned());
                if self.ready.get() {
                    Poll::Ready(FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).map(Arc::new).map_err(str::to_owned))
                } else { Poll::Pending }
            }
        }
        let mut engine = Engine::new();
        let provider = Rc::new(PendingProvider { ready: Cell::new(false), calls: Cell::new(0), bases: RefCell::new(Vec::new()) });
        let worker = install_worker_test(&mut engine, provider.clone());
        boolean(&mut engine, r#"(() => {
            globalThis.pendingFace = new FontFace('Pending', 'url(relative.woff2)');
            fonts.add(pendingFace); pendingFace.load();
            globalThis.pendingReady = fonts.ready;
            return pendingFace.status === 'loading';
        })()"#);
        worker_checkpoint(&mut engine);
        assert_eq!(provider.calls.get(), 1);
        assert!(!scheduling::task_pending(engine.ctx()));
        boolean(&mut engine, "fonts.status === 'loading' && fonts.ready === pendingReady && pendingFace.status === 'loading'");
        provider.ready.set(true);
        assert_eq!(poll_worker_fonts(engine.ctx()).unwrap(), 1);
        worker_checkpoint(&mut engine);
        assert!(provider.bases.borrow().iter().all(|base| base == &worker.base_url));
        boolean(&mut engine, "fonts.status === 'loaded' && pendingFace.status === 'loaded'");
        let mut other = Engine::new();
        let _other_worker = install_worker_test(&mut other, Rc::new(Provider { calls: Cell::new(0), fail: false }));
        boolean(&mut other, "fonts.size === 0 && typeof pendingFace === 'undefined'");
    }

    #[test]
    fn worker_binary_fonts_use_live_registry_without_resource_fetch() {
        let mut engine = Engine::new();
        let provider = Rc::new(Provider { calls: Cell::new(0), fail: true });
        let _worker = install_worker_test(&mut engine, provider.clone());
        let bytes = engine.ctx().make_uint8array(lumen_html_text::DEFAULT_FONT_BYTES).ok().expect("worker font bytes");
        let global = engine.ctx().global_object();
        engine.ctx().member_set(&global, "fontBytes", bytes).ok().expect("install worker font bytes");
        boolean(&mut engine, "globalThis.binaryFace = new FontFace('Binary Worker', fontBytes); fonts.add(binaryFace); fonts.has(binaryFace)");
        worker_checkpoint(&mut engine);
        assert_eq!(provider.calls.get(), 0);
        boolean(&mut engine, "binaryFace.status === 'loaded' && fonts.check('20px \"Binary Worker\"')");
    }

    fn install_with_layout(engine: &mut Engine, source: &str) -> Rc<DomRealm> {
        let realm = install(engine.ctx(), source, 64).unwrap();
        let font = Rc::new(FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap());
        let retained_font = font.clone();
        realm.set_layout_flusher(Rc::new(move |session| {
            session
                .display_list(120, 100, retained_font.as_ref())
                .map(|_| ())
                .map_err(|error| format!("{error:?}"))
        }));
        realm
    }

    #[test]
    fn font_face_width_mutations_rebuild_static_selection_without_face_leaks() {
        let mut engine = Engine::new();
        let provider = Rc::new(Provider {calls:Cell::new(0),fail:true});
        let worker = install_worker_test(&mut engine,provider.clone());
        for (name,bytes) in [("widthRegular",lumen_html_text::TEST_FONT_BYTES),("widthBold",lumen_html_text::TEST_FONT_BOLD_BYTES)] {
            let value = engine.ctx().make_uint8array(bytes).ok().expect("width fixture bytes");
            let global = engine.ctx().global_object();
            engine.ctx().member_set(&global,name,value).ok().expect("install width bytes");
        }
        boolean(&mut engine,r#"globalThis.widthFirst=new FontFace('Width Worker',widthRegular,{stretch:'75%'});globalThis.widthSecond=new FontFace('Width Worker',widthBold,{stretch:'125%'});fonts.add(widthFirst);fonts.add(widthSecond);true"#);
        worker_checkpoint(&mut engine);
        boolean(&mut engine,"widthFirst.status==='loaded' && widthSecond.status==='loaded'");
        let spec = css::parse_font_shorthand("16px \"Width Worker\"").unwrap();
        let before = worker.canvas_font_set(crate::canvas::canvas_fallback_fonts()).unwrap();
        let registrations=before.registrations().unwrap();
        let regular=registrations.iter().find(|entry| entry.font.family.as_ref()=="Width Worker" && entry.descriptors.stretch_range==[75.0,75.0]).expect("loaded regular width face").font.face.clone();
        let bold=registrations.iter().find(|entry| entry.font.family.as_ref()=="Width Worker" && entry.descriptors.stretch_range==[125.0,125.0]).expect("loaded bold width face").font.face.clone();
        let old_run = before.shape_styled("ink",32.0,false,&spec).unwrap();
        assert!(old_run.glyphs.iter().all(|glyph| glyph.face == regular.id()));
        boolean(&mut engine,"widthFirst.stretch='125%';widthSecond.stretch='75%';widthFirst.stretch==='125%' && widthSecond.stretch==='75%'");
        let after = worker.canvas_font_set(crate::canvas::canvas_fallback_fonts()).unwrap();
        assert!(!Rc::ptr_eq(&before,&after));
        let new_run = after.shape_styled("ink",32.0,false,&spec).unwrap();
        assert!(new_run.glyphs.iter().all(|glyph| glyph.face == bold.id()));
        assert_eq!(before.shape_styled("ink",32.0,false,&spec).unwrap(),old_run);
        let old = before.registrations().unwrap();
        let new = after.registrations().unwrap();
        for (a,b) in old.iter().zip(&new) { assert!(Arc::ptr_eq(&a.font.face,&b.font.face)); }
        let glyph = new_run.glyphs[0];
        assert_eq!(after.rasterize_glyph(glyph.face,glyph.id,32.0).unwrap(),bold.rasterize(glyph.id,32.0).unwrap());
        boolean(&mut engine,"(() => { try { widthSecond.stretch='-1%';return false; } catch(e) { return e.name==='SyntaxError' && widthSecond.stretch==='75%'; } })()");
        assert!(Rc::ptr_eq(&after,&worker.canvas_font_set(crate::canvas::canvas_fallback_fonts()).unwrap()));
        assert_eq!(provider.calls.get(),0);
    }

    #[test]
    fn font_face_feature_mutations_rebuild_registration_policy_without_face_leaks() {
        let mut engine = Engine::new();
        let provider = Rc::new(Provider {calls:Cell::new(0),fail:true});
        let worker = install_worker_test(&mut engine,provider.clone());
        let bytes = include_bytes!("../../lumen-html-text/tests/fixtures/caps/FontWithFancyFeatures.otf");
        let value = engine.ctx().make_uint8array(bytes).ok().expect("feature fixture bytes");
        let global = engine.ctx().global_object();
        engine.ctx().member_set(&global,"featureBytes",value).ok().expect("install feature bytes");
        boolean(&mut engine,r#"globalThis.featureFace=new FontFace('Feature Worker',featureBytes,{featureSettings:'"liga" off'}); fonts.add(featureFace); true"#);
        worker_checkpoint(&mut engine);
        boolean(&mut engine,"featureFace.status === 'loaded'");
        let baseline = FontFace::new(Arc::from(bytes.as_slice())).unwrap();
        let disabled = FontSpec {ligatures:lumen_html::paint::FontLigatures::NONE,..FontSpec::default()};
        let plain = baseline.shape_styled("C",32.0,false,&disabled).unwrap().glyphs[0].id;
        let enabled = baseline.shape_styled("C",32.0,false,&FontSpec::default()).unwrap().glyphs[0].id;
        assert_ne!(plain,enabled);
        let spec = css::parse_font_shorthand("16px \"Feature Worker\"").unwrap();
        let before = worker.canvas_font_set(crate::canvas::canvas_fallback_fonts()).unwrap();
        assert_eq!(before.shape_styled("C",32.0,false,&spec).unwrap().glyphs[0].id,plain);
        boolean(&mut engine,r#"featureFace.featureSettings='"liga" on'; featureFace.featureSettings==='"liga"'"#);
        let after = worker.canvas_font_set(crate::canvas::canvas_fallback_fonts()).unwrap();
        assert!(!Rc::ptr_eq(&before,&after));
        assert_eq!(after.shape_styled("C",32.0,false,&spec).unwrap().glyphs[0].id,enabled);
        assert_eq!(before.shape_styled("C",32.0,false,&spec).unwrap().glyphs[0].id,plain);
        let old = before.registrations().unwrap();
        let new = after.registrations().unwrap();
        assert!(Arc::ptr_eq(&old.last().unwrap().font.face,&new.last().unwrap().font.face));
        boolean(&mut engine,r#"(() => { try { featureFace.featureSettings='"invalid"'; return false; } catch(e) { return e.name==='SyntaxError' && featureFace.featureSettings==='"liga"'; } })()"#);
        assert!(Rc::ptr_eq(&after,&worker.canvas_font_set(crate::canvas::canvas_fallback_fonts()).unwrap()));
        assert_eq!(provider.calls.get(),0);
    }

    #[test]
    fn canvas_font_set_cache_tracks_registry_generation_and_reclaims_replaced_set() {
        let fallback = FontSet::new(vec![lumen_html_text::RegisteredFont {
            family: Arc::from("fallback"),
            weight: 400,
            style: lumen_html::paint::FontStyle::Normal,
            stretch: 100.0,
            face: Arc::new(FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap()),
        }])
        .unwrap();
        let loading = FontLoading::default();
        let first = loading
            .canvas_font_set(&fallback, "https://fonts.example.test/page.html")
            .unwrap();
        let cache_hit = loading
            .canvas_font_set(&fallback, "https://fonts.example.test/page.html")
            .unwrap();
        assert!(Rc::ptr_eq(&first, &cache_hit));
        let retired = Rc::downgrade(&first);
        drop(first);
        drop(cache_hit);

        let decoded = Arc::new(FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap());
        let manual_state = {
            let mut registry = loading.manual_registry.borrow_mut();
            let rule = lumen_html::css::parse_font_faces(
                "@font-face { font-family: \"Manual Canvas\"; src: url(manual.woff2); unicode-range: U+0041; }",
            )
            .unwrap()
            .remove(0);
            let state = registry
                .create_manual_face(
                    rule,
                    ManualFontSource::Binary(Arc::from(lumen_html_text::TEST_FONT_BYTES)),
                )
                .unwrap();
            assert!(registry.add_manual_face(&state).unwrap());
            assert!(registry.begin_load(&state).unwrap().is_some());
            assert_eq!(
                registry.complete_load(&state, Ok(decoded.clone())).unwrap(),
                FontFaceStatus::Loaded
            );
            state
        };

        let current = loading
            .canvas_font_set(&fallback, "https://fonts.example.test/page.html")
            .unwrap();
        assert!(retired.upgrade().is_none());
        let registrations = current.registrations().unwrap();
        assert_eq!(registrations.len(), 2);
        assert_eq!(registrations[1].font.family.as_ref(), "Manual Canvas");
        assert!(Arc::ptr_eq(&registrations[1].font.face, &decoded));
        assert_eq!(registrations[1].unicode_range.as_deref().unwrap(), &[(0x41, 0x41)]);
        drop(manual_state);
    }

    #[test]
    fn fontface_constructor_descriptors_setlike_and_binary_source_use_native_faces() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = install_with_layout(engine, "<style></style>");
        realm.set_font_resource_loader(Rc::new(Provider {
            calls: Cell::new(0),
            fail: false,
        }));
        boolean(
            engine,
            r#"(() => {
                globalThis.localFace = new FontFace('Local Face', 'url(local.ttf)', {weight:'700', width:'condensed', sizeAdjust:'90%'});
                globalThis.localFonts = document.fonts;
                localFonts.add(localFace);
                globalThis.localEntries = [...localFonts.entries()];
                globalThis.localKeys = [...localFonts.keys()];
                globalThis.directFace = new FontFace('Direct Face', 'url(direct.ttf)');
                globalThis.directPromise = directFace.load();
                globalThis.directLoadResolved = false;
                directPromise.then(() => directLoadResolved = true);
                return true;
            })()"#,
        );
        boolean(engine, "localFace.family === 'Local Face'");
        boolean(engine, "localFace.weight === '700'");
        boolean(engine, "localFace.stretch === 'condensed'");
        boolean(engine, "localFace.width === 'condensed'");
        boolean(engine, "localFace.sizeAdjust === '90%'");
        boolean(engine, "localFonts.size === 1");
        boolean(engine, "localFonts.has(localFace)");
        boolean(engine, "localEntries.length === 1");
        boolean(
            engine,
            "localEntries[0][0] === localFace && localEntries[0][1] === localFace",
        );
        boolean(engine, "localKeys[0] === localFace");
        boolean(engine, "localFonts.delete(localFace)");
        boolean(engine, "localFonts.size === 0");
        boolean(
            engine,
            "directPromise === directFace.loaded && directFace.status === 'loading'",
        );
        boolean(
            engine,
            r#"(() => {
                const bytes = new Uint8Array([0,1,2,3]).subarray(1,3);
                const face = new FontFace('Binary Face', bytes);
                document.fonts.add(face);
                const loading = face.load();
                return face.status === 'unloaded' && loading === face.loaded &&
                    document.fonts.has(face) && document.fonts.ready instanceof Promise;
            })()"#,
        );
        assert_eq!(realm.queue_font_tasks(engine.ctx()).unwrap(), 2);
        realm.settle_font_loading(engine.ctx()).unwrap();
        while scheduling::task_pending(engine.ctx()) {
            scheduling::run_tasks(engine, 8);
        }
        engine.ctx().drain_microtasks_for_host();
        boolean(
            engine,
            "directLoadResolved && directFace.status === 'loaded'",
        );
        boolean(
            engine,
            "[...document.fonts].length === 1 && [...document.fonts][0].status === 'error'",
        );
    }

    #[test]
    fn fontface_constructor_records_invalid_descriptors_as_error_faces() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = install_with_layout(engine, "<style></style>");
        boolean(
            engine,
            r#"(() => {
                const bad = new FontFace('Bad Face', 'url(bad.woff)', {ascentOverride:'10px'});
                const valid = new FontFace('Valid Face', 'url(valid.woff)');
                globalThis.badDescriptorFace = bad;
                globalThis.badDescriptorLoad = null;
                const loaded = bad.loaded;
                globalThis.sameRejectedPromise = bad.load() === loaded;
                bad.loaded.then(
                    () => badDescriptorLoad = {status: 'loaded'},
                    error => badDescriptorLoad = {name: error.name, code: error.code, dom: error instanceof DOMException}
                );
                globalThis.setterError = undefined;
                const nativeDOMException = DOMException;
                globalThis.DOMException = class PageReplacement {};
                try { valid.ascentOverride = '-50%'; } catch (error) { setterError = error; }
                finally { globalThis.DOMException = nativeDOMException; }
                globalThis.setterUsesCapturedDOMException = setterError instanceof nativeDOMException;
                return true;
            })()"#,
        );
        boolean(engine, "badDescriptorFace.status === 'error'");
        boolean(engine, "badDescriptorFace.ascentOverride === ''");
        boolean(engine, "sameRejectedPromise");
        boolean(engine, "setterError instanceof DOMException");
        boolean(engine, "setterUsesCapturedDOMException");
        boolean(engine, "setterError.name === 'SyntaxError'");
        boolean(engine, "setterError.code === 12");
        assert_eq!(realm.queue_font_tasks(engine.ctx()).unwrap(), 0);
        engine.ctx().drain_microtasks_for_host();
        boolean(engine, "badDescriptorLoad.name === 'SyntaxError'");
        boolean(engine, "badDescriptorLoad.code === 12");
        boolean(engine, "badDescriptorLoad.dom");
    }

    #[test]
    fn font_settlement_skips_unobserved_layout_but_ready_requires_a_provider() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(realm.settle_font_loading(engine.ctx()).unwrap());
        boolean(engine, "globalThis.readySettled=false;document.fonts.ready.then(()=>readySettled=true);!readySettled");
        assert!(
            realm.settle_font_loading(engine.ctx()).is_err(),
            "observed ready must wait for the host's layout provider"
        );
        let calls = Rc::new(Cell::new(0));
        let observed = calls.clone();
        realm.set_layout_flusher(Rc::new(move |_| {
            observed.set(observed.get() + 1);
            Ok(())
        }));
        assert!(realm.settle_font_loading(engine.ctx()).unwrap());
        engine.ctx().drain_microtasks_for_host();
        assert_eq!(calls.get(), 1);
        boolean(engine, "readySettled");
    }

    #[test]
    fn fontfaceset_ready_and_iteration_follow_layout_and_live_membership() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = install_with_layout(engine, "<style></style>");
        boolean(
            engine,
            "globalThis.initialReady = false; const initialSet = document.fonts; const initialPromise = initialSet.ready; initialPromise.then(() => initialReady = true); initialPromise === initialSet.ready && !initialReady",
        );
        assert!(realm.settle_font_loading(engine.ctx()).unwrap());
        engine.ctx().drain_microtasks_for_host();
        boolean(engine, "initialReady && initialSet.status === 'loaded'");
        boolean(
            engine,
            r#"(() => {
                const fonts = document.fonts;
                const first = new FontFace('first', 'local("first")');
                const removed = new FontFace('removed', 'local("removed")');
                const added = new FontFace('added', 'local("added")');
                fonts.add(first).add(removed);
                const seen = [];
                fonts.forEach(face => {
                    seen.push(face.family);
                    if (face === first) {
                        fonts.delete(removed);
                        fonts.add(added);
                    }
                });
                globalThis.liveSeen = seen;
                globalThis.livePair = [...fonts.entries()][0];
                const nativeFreeze = Object.freeze;
                Object.freeze = () => { throw new Error('page freeze called'); };
                try {
                    globalThis.loadEvent = new FontFaceSetLoadEvent('loadingdone', {fontfaces:[first]});
                } finally {
                    Object.freeze = nativeFreeze;
                }
                globalThis.emptyEvent = new FontFaceSetLoadEvent('loadingdone');
                globalThis.setEvent = new FontFaceSetLoadEvent('loadingdone', {fontfaces:new Set([first, added])});
                globalThis.generatorEvent = new FontFaceSetLoadEvent('loadingdone', {
                    fontfaces:(function* () { yield added; yield first; })()
                });
                globalThis.arrayLikeRejected = false;
                try {
                    new FontFaceSetLoadEvent('loadingdone', {fontfaces:{0:first, length:1}});
                } catch (error) {
                    arrayLikeRejected = error instanceof TypeError;
                }
                globalThis.nullSequenceRejected = false;
                try {
                    new FontFaceSetLoadEvent('loadingdone', {fontfaces:null});
                } catch (error) {
                    nullSequenceRejected = error instanceof TypeError;
                }
                globalThis.badIterableClosed = false;
                try {
                    new FontFaceSetLoadEvent('loadingdone', {
                        fontfaces:(function* () {
                            try { yield first; yield {}; }
                            finally { badIterableClosed = true; }
                        })()
                    });
                } catch (error) {
                    globalThis.badIterableTypeError = error instanceof TypeError;
                }
                return true;
            })()"#,
        );
        boolean(
            engine,
            "liveSeen.length === 2 && liveSeen[0] === 'first' && liveSeen[1] === 'added'",
        );
        boolean(
            engine,
            "Array.isArray(livePair) && livePair[0].family === 'first' && livePair[1] === livePair[0]",
        );
        boolean(
            engine,
            "Array.isArray(loadEvent.fontfaces) && loadEvent.fontfaces.length === 1 && loadEvent.fontfaces[0].family === 'first'",
        );
        boolean(
            engine,
            "Object.isFrozen(loadEvent.fontfaces) && loadEvent.fontfaces === loadEvent.fontfaces",
        );
        boolean(
            engine,
            "setEvent.fontfaces.length === 2 && setEvent.fontfaces[0].family === 'first' && setEvent.fontfaces[1].family === 'added'",
        );
        boolean(
            engine,
            "generatorEvent.fontfaces.length === 2 && generatorEvent.fontfaces[0].family === 'added' && generatorEvent.fontfaces[1].family === 'first'",
        );
        boolean(engine, "arrayLikeRejected");
        boolean(engine, "nullSequenceRejected");
        boolean(
            engine,
            "emptyEvent.fontfaces.length === 0 && Object.isFrozen(emptyEvent.fontfaces) && emptyEvent.fontfaces !== loadEvent.fontfaces",
        );
        boolean(engine, "badIterableTypeError && badIterableClosed");
    }

    #[test]
    fn manual_fontface_descriptor_mutations_are_live_before_and_after_add() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let _realm = install_with_layout(engine, "<style></style>");
        boolean(
            engine,
            r#"(() => {
                const fonts = document.fonts;
                const preAdd = new FontFace('before', 'url(before.woff)', {weight:'700', style:'oblique'});
                preAdd.family = 'after-before';
                preAdd.weight = '300';
                preAdd.style = 'italic';
                fonts.add(preAdd);

                const live = new FontFace('live-before', 'url(live.woff)', {weight:'600', style:'oblique'});
                fonts.add(live);
                live.family = 'live-after';
                live.weight = '400';
                live.style = 'normal';

                const snapshot = [...fonts];
                const families = snapshot.map(face => face.family);
                const weights = snapshot.map(face => face.weight);
                const styles = snapshot.map(face => face.style);
                return snapshot.length === 2 && snapshot[0] === preAdd && snapshot[1] === live &&
                    families.includes('after-before') && !families.includes('before') &&
                    families.includes('live-after') && !families.includes('live-before') &&
                    weights.includes('300') && weights.includes('400') &&
                    !weights.includes('700') && !weights.includes('600') &&
                    styles.includes('italic') && styles.includes('normal') &&
                    !styles.includes('oblique');
            })()"#,
        );
    }

    #[test]
    fn automatic_font_resources_hold_ready_and_dispatch_success_and_failure() {
        struct AutomaticProvider {
            requested: Cell<bool>,
            ready: Cell<bool>,
            fail: bool,
        }
        impl FontResourceLoader for AutomaticProvider {
            fn load(&self, _: &FontFaceRule, _: &str) -> Result<Arc<FontFace>, String> {
                panic!("automatic requests must use poll_load")
            }
            fn poll_load(&self, _: &FontFaceRule, _: &str) -> Poll<Result<Arc<FontFace>, String>> {
                if !self.ready.get() {
                    Poll::Pending
                } else if self.fail {
                    Poll::Ready(Err("font transport failed".into()))
                } else {
                    Poll::Ready(
                        FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES))
                            .map(Arc::new)
                            .map_err(str::to_owned),
                    )
                }
            }
            fn automatic_load(
                &self,
                rule: &FontFaceRule,
                base: &str,
            ) -> Option<Poll<Result<Arc<FontFace>, String>>> {
                self.requested.get().then(|| self.poll_load(rule, base))
            }
        }
        for fail in [false, true] {
            let mut runtime = Runtime::new();
            let engine = runtime.engine();
            let realm = install_with_layout(
                engine,
                "<style>@font-face{font-family:automatic;src:url(font.ttf)}</style><p>text</p>",
            );
            let provider = Rc::new(AutomaticProvider {
                requested: Cell::new(false),
                ready: Cell::new(false),
                fail,
            });
            realm.set_font_resource_loader(provider.clone());
            boolean(engine, "[...document.fonts][0].status === 'unloaded'");
            realm.settle_font_loading(engine.ctx()).unwrap();
            boolean(engine, "[...document.fonts][0].status === 'unloaded'");
            provider.requested.set(true);
            assert!(!realm.settle_font_loading(engine.ctx()).unwrap());
            boolean(
                engine,
                "globalThis.done = false; globalThis.events = []; document.fonts.ready.then(() => done = true); document.fonts.addEventListener('loadingdone', () => events.push('done')); document.fonts.addEventListener('loadingerror', () => events.push('error')); document.fonts.status === 'loading' && [...document.fonts][0].status === 'loading'",
            );
            realm.queue_font_tasks(engine.ctx()).unwrap();
            engine.ctx().drain_microtasks_for_host();
            boolean(engine, "!done");
            provider.ready.set(true);
            realm.queue_font_tasks(engine.ctx()).unwrap();
            while scheduling::task_pending(engine.ctx()) {
                scheduling::run_tasks(engine, 8);
            }
            realm.settle_font_loading(engine.ctx()).unwrap();
            while scheduling::task_pending(engine.ctx()) {
                scheduling::run_tasks(engine, 8);
            }
            engine.ctx().drain_microtasks_for_host();
            boolean(
                engine,
                "done && document.fonts.status === 'loaded' && events[0] === 'done'",
            );
            boolean(
                engine,
                if fail {
                    "[...document.fonts][0].status === 'error' && events[1] === 'error'"
                } else {
                    "[...document.fonts][0].status === 'loaded' && events.length === 1"
                },
            );
        }
    }

    #[test]
    fn asynchronous_font_resources_retain_promises_and_original_base_until_completion() {
        struct AsyncProvider {
            ready: Cell<bool>,
            bases: RefCell<Vec<String>>,
        }
        impl FontResourceLoader for AsyncProvider {
            fn load(&self, _: &FontFaceRule, _: &str) -> Result<Arc<FontFace>, String> {
                panic!("an asynchronous resource must use poll_load")
            }
            fn poll_load(
                &self,
                _: &FontFaceRule,
                base: &str,
            ) -> Poll<Result<Arc<FontFace>, String>> {
                self.bases.borrow_mut().push(base.to_owned());
                if self.ready.get() {
                    Poll::Ready(
                        FontFace::new(Arc::from(lumen_html_text::DEFAULT_FONT_BYTES))
                            .map(Arc::new)
                            .map_err(str::to_owned),
                    )
                } else {
                    Poll::Pending
                }
            }
        }
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = install_with_layout(engine, "<head></head><body></body>");
        realm.set_document_url("https://example.test/original/page.html");
        let provider = Rc::new(AsyncProvider {
            ready: Cell::new(false),
            bases: RefCell::new(Vec::new()),
        });
        realm.set_font_resource_loader(provider.clone());
        boolean(
            engine,
            r#"
            globalThis.pendingFace = new FontFace('async', 'url(font.ttf)');
            document.fonts.add(pendingFace);
            globalThis.faceDone = false;
            globalThis.setDone = false;
            pendingFace.load().then(() => faceDone = true);
            document.fonts.load('16px async').then(() => setDone = true);
            true
        "#,
        );
        realm.set_document_url("https://different.test/new/page.html");
        assert_eq!(realm.queue_font_tasks(engine.ctx()).unwrap(), 0);
        assert_eq!(realm.queue_font_tasks(engine.ctx()).unwrap(), 0);
        engine.ctx().drain_microtasks_for_host();
        boolean(
            engine,
            "pendingFace.status === 'loading' && !faceDone && !setDone",
        );
        assert!(!realm.settle_font_loading(engine.ctx()).unwrap());
        assert!(provider
            .bases
            .borrow()
            .iter()
            .all(|base| base == "https://example.test/original/page.html"));
        provider.ready.set(true);
        assert_eq!(realm.queue_font_tasks(engine.ctx()).unwrap(), 1);
        realm.settle_font_loading(engine.ctx()).unwrap();
        while scheduling::task_pending(engine.ctx()) {
            scheduling::run_tasks(engine, 8);
        }
        engine.ctx().drain_microtasks_for_host();
        boolean(
            engine,
            "pendingFace.status === 'loaded' && faceDone && setDone && document.fonts.status === 'loaded'",
        );
        assert_eq!(realm.queue_font_tasks(engine.ctx()).unwrap(), 0);
    }

    #[test]
    fn css_font_load_decodes_once_and_settles_native_promises_on_the_task() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = install_with_layout(
            engine,
            "<style>@font-face { font-family: testfont; src: url(font.ttf); }</style>",
        );
        let provider = Rc::new(Provider {
            calls: Cell::new(0),
            fail: false,
        });
        realm.set_font_resource_loader(provider.clone());
        boolean(
            engine,
            "document.fonts === document.fonts && !document.fonts.check('20px testfont')",
        );
        boolean(
            engine,
            "!document.fonts.check({toString() { return '20px testfont'; }} )",
        );
        boolean(
            engine,
            "document.fonts.check('20px testfont', '') && document.fonts.check('20px missingfamily')",
        );
        boolean(
            engine,
            "globalThis.empty = false; document.fonts.load('20px testfont', '').then(faces => empty = faces.length === 0); true",
        );
        assert_eq!(realm.queue_font_tasks(engine.ctx()).unwrap(), 0);
        engine.ctx().drain_microtasks_for_host();
        boolean(engine, "empty");
        assert_eq!(provider.calls.get(), 0);
        boolean(
            engine,
            "globalThis.done = false; globalThis.loadedFace = null; globalThis.lifecycle = []; globalThis.trust = []; document.fonts.addEventListener('loading', event => { lifecycle.push('loading'); trust.push(event.isTrusted); }); document.fonts.addEventListener('loadingdone', event => { lifecycle.push('done'); trust.push(event.isTrusted); }); globalThis.preLoadReady = document.fonts.ready; const pending = document.fonts.load('20px testfont'); globalThis.readyRetained = preLoadReady === document.fonts.ready; preLoadReady.then(() => lifecycle.push('ready')); pending.then(faces => { loadedFace = faces[0]; done = true; }); true",
        );
        boolean(
            engine,
            "readyRetained && document.fonts.status === 'loading' && !done && lifecycle.length === 0",
        );
        assert_eq!(realm.queue_font_tasks(engine.ctx()).unwrap(), 1);
        assert!(!realm.settle_font_loading(engine.ctx()).unwrap());
        while scheduling::task_pending(engine.ctx()) {
            scheduling::run_tasks(engine, 8);
        }
        engine.ctx().drain_microtasks_for_host();
        boolean(
            engine,
            "done && loadedFace.status === 'loaded' && document.fonts.check('20px testfont') && document.fonts.status === 'loaded'",
        );
        boolean(engine, "lifecycle.indexOf('loading') >= 0");
        boolean(
            engine,
            "lifecycle.indexOf('done') > lifecycle.indexOf('loading')",
        );
        boolean(
            engine,
            "lifecycle.indexOf('ready') >= 0 && lifecycle.indexOf('ready') < lifecycle.indexOf('done')",
        );
        boolean(engine, "trust.length === 2 && trust[0] && trust[1]");
        assert_eq!(provider.calls.get(), 1);
    }

    #[test]
    fn css_font_load_rejects_unavailable_resources_and_invalid_shorthand() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = install_with_layout(
            engine,
            "<style>@font-face { font-family: testfont; src: url(missing.ttf); }</style>",
        );
        realm.set_font_resource_loader(Rc::new(Provider {
            calls: Cell::new(0),
            fail: true,
        }));
        boolean(
            engine,
            "globalThis.failure = ''; document.fonts.load('20px testfont').catch(e => failure = e.name); true",
        );
        assert_eq!(realm.queue_font_tasks(engine.ctx()).unwrap(), 1);
        realm.settle_font_loading(engine.ctx()).unwrap();
        while scheduling::task_pending(engine.ctx()) {
            scheduling::run_tasks(engine, 8);
        }
        engine.ctx().drain_microtasks_for_host();
        boolean(
            engine,
            "failure === 'NetworkError' && !document.fonts.check('20px testfont')",
        );
        boolean(
            engine,
            "globalThis.syntax = 0; document.fonts.load('inherit').catch(e => syntax = e instanceof DOMException && e.name === 'SyntaxError' && e.code === 12); true",
        );
        engine.ctx().drain_microtasks_for_host();
        boolean(engine, "syntax === true");
    }
}
