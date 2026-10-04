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
    paint::{FontMetric, FontSpec},
};
use lumen_html_text::FontFace;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
    sync::Arc,
    task::Poll,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FontFaceStatus {
    Unloaded,
    Loading,
    Loaded,
    Error,
}

impl FontFaceStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unloaded => "unloaded",
            Self::Loading => "loading",
            Self::Loaded => "loaded",
            Self::Error => "error",
        }
    }
}

/// A currently registered, non-CSS `document.fonts` member for the embedder's
/// renderer. The full current set is pushed on every membership or load-state
/// change, in the set's insertion order.
#[derive(Clone)]
pub struct ManualFontFace {
    pub identity: FontFaceIdentity,
    pub rule: FontFaceRule,
    pub status: FontFaceStatus,
    pub decoded: Option<Arc<FontFace>>,
    pub byte_length: Option<usize>,
}

/// The embedder supplies bounded resource loading and may expose the same
/// decoded face to layout. It never parses descriptors or manufactures fonts.
pub trait FontResourceLoader {
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

    /// Current non-CSS members of `document.fonts`. The adapter updates this
    /// snapshot on add/delete/clear and when a manual face finishes loading.
    fn replace_manual_faces(&self, _faces: &[ManualFontFace]) {}

    /// Current non-CSS members in insertion order. Renderers append decoded
    /// entries after the live CSS-connected faces in stylesheet order.
    fn manual_faces(&self) -> Vec<ManualFontFace> {
        Vec::new()
    }

    /// Metric of the first available font selected by the embedder's actual
    /// FontSet, without starting a resource request.
    fn primary_metric(&self, _font: &FontSpec, _metric: FontMetric) -> Option<f32> {
        None
    }
}

enum FaceSource {
    Url,
    Binary(Arc<[u8]>),
}

struct FaceData {
    identity: FontFaceIdentity,
    rule: RefCell<FontFaceRule>,
    descriptor_overrides: RefCell<Vec<(String, String)>>,
    source: FaceSource,
    invalid_descriptors: RefCell<HashMap<String, String>>,
    status: Cell<FontFaceStatus>,
    decoded: RefCell<Option<Arc<FontFace>>>,
    error: RefCell<Option<String>>,
    loaded: Value,
    loaded_deferred: RefCell<Option<Deferred>>,
    realm: Weak<DomRealm>,
    wrapper: RefCell<Option<WeakValue>>,
}

impl FaceData {
    fn new(
        ctx: &mut Ctx,
        realm: &Rc<DomRealm>,
        mut rule: FontFaceRule,
        source: FaceSource,
        identity: FontFaceIdentity,
    ) -> Rc<Self> {
        rule.identity = Some(identity.clone());
        let deferred = Deferred::new(ctx);
        Rc::new(Self {
            identity,
            rule: RefCell::new(rule),
            descriptor_overrides: RefCell::new(Vec::new()),
            source,
            invalid_descriptors: RefCell::new(HashMap::new()),
            status: Cell::new(FontFaceStatus::Unloaded),
            decoded: RefCell::new(None),
            error: RefCell::new(None),
            loaded: deferred.promise(),
            loaded_deferred: RefCell::new(Some(deferred)),
            realm: Rc::downgrade(realm),
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
        self.rule.borrow().clone()
    }
}

struct Batch {
    faces: Vec<Rc<FaceData>>,
    values: Vec<Value>,
    promise: Option<Deferred>,
    base: Option<String>,
}

#[derive(Default)]
pub(crate) struct FontLoading {
    provider: RefCell<Option<Rc<dyn FontResourceLoader>>>,
    dom_exception_constructor: RefCell<Option<Value>>,
    object_freeze: RefCell<Option<JsFunction>>,
    batches: RefCell<Vec<Batch>>,
    sets: RefCell<Vec<Weak<RefCell<FontFaceSetState>>>>,
    manual_faces: RefCell<Vec<ManualFontFace>>,
    next_manual: Cell<u64>,
}

impl FontLoading {
    pub(crate) fn capture_dom_exception(&self, ctx: &mut Ctx) {
        let global = ctx.global_object();
        if self.dom_exception_constructor.borrow().is_none() {
            let constructor = ctx
                .get_member(&global, "DOMException")
                .ok()
                .filter(Value::is_callable);
            if constructor.is_some() {
                *self.dom_exception_constructor.borrow_mut() = constructor;
            }
        }
        if self.object_freeze.borrow().is_none() {
            let freeze = ctx
                .get_member(&global, "Object")
                .ok()
                .and_then(|object| ctx.get_member(&object, "freeze").ok())
                .and_then(JsFunction::from_value);
            if freeze.is_some() {
                *self.object_freeze.borrow_mut() = freeze;
            }
        }
    }

    pub(crate) fn set_provider(&self, provider: Rc<dyn FontResourceLoader>) {
        provider.replace_manual_faces(&self.manual_faces.borrow());
        *self.provider.borrow_mut() = Some(provider);
    }

    pub(crate) fn primary_metric(&self, font: &FontSpec, metric: FontMetric) -> Option<f32> {
        self.provider
            .borrow()
            .as_ref()
            .and_then(|provider| provider.primary_metric(font, metric))
    }

    fn allocate_manual_id(&self) -> OpResult<u64> {
        let next = self.next_manual.get().checked_add(1).ok_or_else(|| {
            OpError::new("QuotaExceededError", "FontFace identity space exhausted")
        })?;
        self.next_manual.set(next);
        Ok(next)
    }

    fn register_set(&self, state: &Rc<RefCell<FontFaceSetState>>) {
        self.sets.borrow_mut().push(Rc::downgrade(state));
    }

    fn publish_manual_faces(&self, faces: Vec<ManualFontFace>) {
        *self.manual_faces.borrow_mut() = faces;
        if let Some(provider) = self.provider.borrow().as_ref() {
            provider.replace_manual_faces(&self.manual_faces.borrow());
        }
    }

    pub(crate) fn manual_faces(&self) -> Vec<ManualFontFace> {
        self.manual_faces.borrow().clone()
    }

    fn sync_manual_faces(&self) {
        let mut faces = Vec::new();
        let mut sets = self.sets.borrow_mut();
        sets.retain(|weak| {
            let Some(state) = weak.upgrade() else {
                return false;
            };
            let state = state.borrow();
            faces.extend(state.manual.iter().map(|(data, _)| ManualFontFace {
                identity: data.identity.clone(),
                rule: data.rule(),
                status: data.status.get(),
                decoded: data.decoded.borrow().clone(),
                byte_length: match &data.source {
                    FaceSource::Binary(bytes) => Some(bytes.len()),
                    FaceSource::Url => None,
                },
            }));
            true
        });
        drop(sets);
        self.publish_manual_faces(faces);
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
        let mut sets = self.sets.borrow_mut();
        sets.retain(|weak| {
            let Some(state) = weak.upgrade() else {
                return false;
            };
            state.borrow_mut().face_completed(face, success);
            true
        });
        drop(sets);
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
                if matches!(
                    face.status.get(),
                    FontFaceStatus::Unloaded | FontFaceStatus::Loading
                ) {
                    if face.status.replace(FontFaceStatus::Loading) != FontFaceStatus::Loading {
                        self.notify_started(ctx, face);
                        changed = true;
                    }
                    let invalid_descriptor =
                        face.invalid_descriptors.borrow().values().next().cloned();
                    let result = if let Some(message) = invalid_descriptor {
                        Err(("SyntaxError", message))
                    } else {
                        match &face.source {
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
                                Some(provider) => match provider.poll_load(&face.rule(), base) {
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
                            changed = true;
                            *face.decoded.borrow_mut() = Some(decoded);
                            face.status.set(FontFaceStatus::Loaded);
                            if let Some(deferred) = face.loaded_deferred.borrow_mut().take() {
                                deferred.resolve(ctx, value.clone());
                            }
                            self.notify_completed(face, true);
                        }
                        Err((name, message)) => {
                            changed = true;
                            *face.error.borrow_mut() = Some(message.clone());
                            face.status.set(FontFaceStatus::Error);
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
                if face.status.get() == FontFaceStatus::Error {
                    failure = face.error.borrow().clone();
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
        .and_then(|font_loading| font_loading.dom_exception_constructor.borrow().clone())
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
    let identity = data.identity.clone();
    let realm = data.realm.upgrade();
    let font_loading = realm.as_deref().map(|realm| &realm.font_loading);
    let (new, descriptors, canonical) = {
        let old = data.rule.borrow();
        let mut descriptors = old.descriptors.clone();
        descriptors
            .set(name, value)
            .map_err(|error| css_syntax_error(ctx, font_loading, error))?;
        let canonical = descriptors.get(name).unwrap_or_else(|| value.to_owned());
        let mut new = descriptors.to_rule(old.source_url.clone());
        new.identity = Some(identity.clone());
        new.source_order = old.source_order;
        new.layer = old.layer;
        new.layers = old.layers.clone();
        new.layer_path = old.layer_path;
        new.media = old.media.clone();
        new.supports = old.supports.clone();
        (new, descriptors, canonical)
    };
    let css_identity_is_live = if matches!(&identity, FontFaceIdentity::Css(_)) {
        if let Some(realm) = &realm {
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

    *data.rule.borrow_mut() = new;
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
                realm.session.borrow_mut().invalidate_fonts();
            }
        } else {
            realm.session.borrow_mut().invalidate_fonts();
            realm.font_loading.sync_manual_faces();
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
    data.rule.borrow().descriptors.get(name).unwrap_or_default()
}

fn set_descriptor_value(data: &FaceData, name: &str, ctx: &mut Ctx, value: &str) -> OpResult<()> {
    update_rule_descriptor(data, name, value, ctx)
}

fn create_css_face(ctx: &mut Ctx, realm: &Rc<DomRealm>, rule: FontFaceRule) -> Rc<FaceData> {
    let identity = rule.identity.clone().unwrap_or_else(|| {
        FontFaceIdentity::Manual(realm.font_loading.allocate_manual_id().unwrap_or(0))
    });
    FaceData::new(ctx, realm, rule, FaceSource::Url, identity)
}

fn ensure_face_wrapper(ctx: &mut Ctx, face: &Rc<FaceData>) -> Value {
    if let Some(value) = face.wrapper_value() {
        return value;
    }
    let value = ctx.new_instance(DomFontFace { data: face.clone() });
    face.set_wrapper(ctx, &value);
    value
}

#[lumen_bind::class(name = "FontFace", hint(js(webidl)))]
pub(crate) struct DomFontFace {
    data: Rc<FaceData>,
}

#[lumen_bind::methods]
impl DomFontFace {
    #[constructor(coerce)]
    fn new(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        family: String,
        source: Value,
        descriptors: Option<Value>,
    ) -> OpResult<Self> {
        let realm = active_realm(ctx)?;
        let id = realm.font_loading.allocate_manual_id()?;
        let entries = descriptor_entries(ctx, descriptors)?;
        let bytes = ctx.buffer_source_bytes(&source);
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
            .map_err(|error| css_syntax_error(ctx, Some(&realm.font_loading), error))?;
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
        let base = realm.base_url();
        let mut rule = descriptors.to_rule(Some(Arc::from(base)));
        let identity = FontFaceIdentity::Manual(id);
        rule.identity = Some(identity.clone());
        let data = FaceData::new(
            ctx,
            &realm,
            rule,
            bytes.map_or(FaceSource::Url, |bytes| {
                FaceSource::Binary(Arc::from(bytes))
            }),
            identity,
        );
        *data.invalid_descriptors.borrow_mut() = invalid_descriptors;
        data.set_wrapper(ctx, &this.0);
        if let Some(message) = data.invalid_descriptors.borrow().values().next().cloned() {
            data.status.set(FontFaceStatus::Error);
            *data.error.borrow_mut() = Some(message.clone());
            if let Some(deferred) = data.loaded_deferred.borrow_mut().take() {
                let error =
                    font_dom_exception(ctx, Some(&realm.font_loading), "SyntaxError", &message);
                deferred.reject(ctx, error);
            }
        } else if matches!(&data.source, FaceSource::Binary(_)) {
            realm.font_loading.batches.borrow_mut().push(Batch {
                faces: vec![data.clone()],
                values: vec![this.0.clone()],
                promise: None,
                base: Some(realm.base_url()),
            });
        }
        Ok(Self { data })
    }

    #[getter]
    fn family(&self) -> String {
        self.data
            .rule
            .borrow()
            .descriptors
            .get("family")
            .unwrap_or_else(|| self.data.rule.borrow().family.to_string())
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
        if matches!(&self.data.source, FaceSource::Binary(_)) {
            return Ok(self.data.loaded.clone());
        }
        if self.data.status.get() == FontFaceStatus::Unloaded {
            let realm = self.data.realm.upgrade().ok_or_else(|| {
                OpError::new("InvalidStateError", "font document is no longer available")
            })?;
            let value = this.0;
            self.data.set_wrapper(ctx, &value);
            self.data.status.set(FontFaceStatus::Loading);
            realm.font_loading.notify_started(ctx, &self.data);
            realm
                .font_loading
                .enqueue_load(ctx, vec![(self.data.clone(), value)], None);
        }
        Ok(self.data.loaded.clone())
    }

    #[getter]
    fn status(&self) -> &'static str {
        self.data.status.get().as_str()
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

fn active_realm(ctx: &mut Ctx) -> OpResult<Rc<DomRealm>> {
    let global = ctx.global_object();
    let document = member_get(ctx, &global, "document")?;
    ctx.with_instance::<DomDocument, _>(&document, |document| document.realm.clone())
        .map_err(|_| OpError::new("InvalidStateError", "FontFace requires an active document"))
}

struct ReadyState {
    promise: Value,
    deferred: Option<Deferred>,
}

struct FontFaceSetState {
    realm: Weak<DomRealm>,
    owner: Option<WeakValue>,
    css: HashMap<FontFaceIdentity, (Rc<FaceData>, Value)>,
    css_order: Vec<FontFaceIdentity>,
    manual: Vec<(Rc<FaceData>, Value)>,
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
    fn new(realm: &Rc<DomRealm>) -> Self {
        Self {
            realm: Rc::downgrade(realm),
            owner: None,
            css: HashMap::new(),
            css_order: Vec::new(),
            manual: Vec::new(),
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

    fn contains(&self, identity: &FontFaceIdentity) -> bool {
        self.css.contains_key(identity)
            || self
                .manual
                .iter()
                .any(|(data, _)| &data.identity == identity)
    }

    fn find_value(&self, identity: &FontFaceIdentity) -> Option<Value> {
        self.css
            .get(identity)
            .map(|(_, value)| value.clone())
            .or_else(|| {
                self.manual
                    .iter()
                    .find(|(face, _)| &face.identity == identity)
                    .map(|(_, value)| value.clone())
            })
    }

    fn face_started(&mut self, ctx: &mut Ctx, face: &Rc<FaceData>) {
        if !self.contains(&face.identity) {
            return;
        }
        let Some(value) = self.find_value(&face.identity) else {
            return;
        };
        self.pending.insert(face.identity.clone(), value);
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
        if !self.contains(&face.identity) {
            return;
        }
        let value = self
            .pending
            .remove(&face.identity)
            .or_else(|| self.find_value(&face.identity));
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

    fn ordered(&self) -> Vec<(Rc<FaceData>, Value)> {
        let mut entries = self
            .css_order
            .iter()
            .filter_map(|identity| self.css.get(identity).cloned())
            .collect::<Vec<_>>();
        entries.extend(self.manual.iter().cloned());
        entries
    }

    fn ordered_with_order(&mut self) -> Vec<(u64, Rc<FaceData>, Value)> {
        let entries = self.ordered();
        entries
            .into_iter()
            .map(|(face, value)| {
                let order = self.register_member(&face.identity);
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

impl DomFontFaceSet {
    pub(crate) fn new(realm: &Rc<DomRealm>) -> Self {
        let state = Rc::new(RefCell::new(FontFaceSetState::new(realm)));
        realm.font_loading.register_set(&state);
        Self {
            base: DomEventTarget::independent(realm),
            state,
        }
    }

    pub(crate) fn attach(&self, ctx: &mut Ctx, owner: &Value) {
        self.state.borrow_mut().attach(ctx, owner);
    }

    fn realm(&self) -> OpResult<Rc<DomRealm>> {
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

    fn sync_css(&self, ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
        sync_css_state(ctx, realm, &self.state)
    }

    fn snapshot_entries(&self, ctx: &mut Ctx) -> OpResult<Vec<(Rc<FaceData>, Value)>> {
        let realm = self.realm()?;
        self.sync_css(ctx, &realm)?;
        Ok(self.state.borrow().ordered())
    }

    fn publish_manual_snapshot(&self) {
        if let Some(realm) = self.state.borrow().realm.upgrade() {
            realm.font_loading.sync_manual_faces();
        }
    }

    fn queue_face_load(
        &self,
        ctx: &mut Ctx,
        realm: &Rc<DomRealm>,
        face: Rc<FaceData>,
        value: Value,
    ) {
        if matches!(
            face.status.get(),
            FontFaceStatus::Loaded | FontFaceStatus::Error
        ) {
            return;
        }
        if face.status.get() == FontFaceStatus::Unloaded {
            let cached = realm
                .font_loading
                .provider
                .borrow()
                .as_ref()
                .and_then(|provider| provider.loaded(&face.rule(), &realm.base_url()));
            if let Some(decoded) = cached {
                face.status.set(FontFaceStatus::Loading);
                realm.font_loading.notify_started(ctx, &face);
                *face.decoded.borrow_mut() = Some(decoded);
                face.status.set(FontFaceStatus::Loaded);
                if let Some(deferred) = face.loaded_deferred.borrow_mut().take() {
                    deferred.resolve(ctx, value.clone());
                }
                realm.font_loading.notify_completed(&face, true);
                return;
            }
            face.status.set(FontFaceStatus::Loading);
            realm.font_loading.notify_started(ctx, &face);
        }
    }

    fn values_for_iterator(&self, ctx: &mut Ctx, entries: bool) -> OpResult<Value> {
        self.snapshot_entries(ctx)?;
        let state = Rc::downgrade(&self.state);
        let realm = self.state.borrow().realm.clone();
        Ok(ctx.new_instance(DomFontFaceSetIterator {
            state,
            realm,
            entries,
            seen: RefCell::new(std::collections::HashSet::new()),
        }))
    }
}

fn sync_css_state(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    state: &Rc<RefCell<FontFaceSetState>>,
) -> OpResult<()> {
    let rules = DomFontFaceSet::descriptors(realm)?;
    let mut state = state.borrow_mut();
    let mut order = Vec::with_capacity(rules.len());
    let mut next = HashMap::with_capacity(rules.len());
    for rule in rules {
        let identity = rule.identity.clone().ok_or_else(|| {
            OpError::new(
                "InvalidStateError",
                "CSS font face is missing its stable identity",
            )
        })?;
        let (face, value) = if let Some((face, value)) = state.css.remove(&identity) {
            let mut rule = rule.clone();
            for (name, value) in face.descriptor_overrides.borrow().iter() {
                if let Err(error) = rule.descriptors.set(name, value) {
                    return Err(css_syntax_error(ctx, Some(&realm.font_loading), error));
                }
            }
            *face.rule.borrow_mut() = rule;
            (face, value)
        } else {
            let face = create_css_face(ctx, realm, rule.clone());
            let value = ensure_face_wrapper(ctx, &face);
            (face, value)
        };
        state.register_member(&identity);
        order.push(identity.clone());
        next.insert(identity, (face, value));
    }
    let live = next
        .keys()
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    let removed = state
        .css
        .keys()
        .filter(|identity| !live.contains(*identity))
        .cloned()
        .filter_map(|identity| {
            state
                .css
                .get(&identity)
                .map(|(_, value)| (identity, value.clone()))
        })
        .collect::<Vec<_>>();
    state.css = next;
    state.css_order = order;
    for (identity, removed_value) in removed {
        state.pending.remove(&identity);
        state
            .succeeded
            .retain(|item| !same_js_value(item, &removed_value));
        state
            .failed
            .retain(|item| !same_js_value(item, &removed_value));
        state.member_order.remove(&identity);
    }
    Ok(())
}

fn next_live_entry(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    state: &Rc<RefCell<FontFaceSetState>>,
    seen: &mut std::collections::HashSet<u64>,
) -> OpResult<Option<(u64, Value)>> {
    sync_css_state(ctx, realm, state)?;
    let mut state = state.borrow_mut();
    for (generation, _face, value) in state.ordered_with_order() {
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
            .with_instance::<DomFontFace, _>(&face, |face| face.data.identity.clone())
            .ok();
        let Some(identity) = identity else {
            return Ok(false);
        };
        let _ = self.snapshot_entries(ctx)?;
        Ok(self.state.borrow().contains(&identity))
    }

    fn add(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>, face: Value) -> OpResult<Value> {
        let data = ctx
            .with_instance::<DomFontFace, _>(&face, |face| face.data.clone())
            .map_err(|_| OpError::type_error("FontFaceSet.add requires a FontFace"))?;
        let realm = self.realm()?;
        if matches!(data.identity, FontFaceIdentity::Css(_)) {
            return Err(font_dom_exception(
                ctx,
                Some(&realm.font_loading),
                "InvalidModificationError",
                "A CSS-connected FontFace cannot be added to a FontFaceSet",
            ));
        }
        if let Some(face_realm) = data.realm.upgrade() {
            if !Rc::ptr_eq(&realm, &face_realm) {
                return Err(font_dom_exception(
                    ctx,
                    Some(&realm.font_loading),
                    "InvalidModificationError",
                    "FontFace belongs to a different document",
                ));
            }
        }
        {
            let mut state = self.state.borrow_mut();
            if !state
                .manual
                .iter()
                .any(|(existing, _)| Rc::ptr_eq(existing, &data))
            {
                state.manual.push((data.clone(), face.clone()));
                state.register_member(&data.identity);
            }
        }
        if data.status.get() == FontFaceStatus::Loading {
            self.state.borrow_mut().face_started(ctx, &data);
        }
        self.publish_manual_snapshot();
        Ok(this.0)
    }

    fn delete(&self, ctx: &mut Ctx, face: Value) -> OpResult<bool> {
        let identity = ctx
            .with_instance::<DomFontFace, _>(&face, |face| face.data.identity.clone())
            .ok();
        let Some(identity) = identity else {
            return Ok(false);
        };
        let _ = self.snapshot_entries(ctx)?;
        if matches!(identity, FontFaceIdentity::Css(_)) {
            return Ok(false);
        }
        let mut state = self.state.borrow_mut();
        let removed_value = state
            .manual
            .iter()
            .find(|(data, _)| data.identity == identity)
            .map(|(_, value)| value.clone());
        let length = state.manual.len();
        state.manual.retain(|(data, _)| data.identity != identity);
        let removed = state.manual.len() != length;
        if removed {
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
        {
            let mut state = self.state.borrow_mut();
            let identities = state
                .manual
                .iter()
                .map(|(face, value)| (face.identity.clone(), value.clone()))
                .collect::<Vec<_>>();
            state.manual.clear();
            for (identity, value) in identities {
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
        let spec = css::parse_font_shorthand(font).ok_or_else(|| {
            font_dom_exception(
                ctx,
                Some(&realm.font_loading),
                "SyntaxError",
                "Invalid font shorthand",
            )
        })?;
        let faces = self.snapshot_entries(ctx)?;
        let rules = faces
            .iter()
            .map(|(face, _)| face.rule())
            .collect::<Vec<_>>();
        let indices = css::matching_font_faces(&spec, &rules, &text);
        let provider = realm.font_loading.provider.borrow().clone();
        let base = realm.base_url();
        Ok(indices.into_iter().all(|index| {
            let face = &faces[index].0;
            face.status.get() == FontFaceStatus::Loaded
                || provider
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
        let result = (|| -> OpResult<(Rc<DomRealm>, Vec<(Rc<FaceData>, Value)>)> {
            let realm = self.realm()?;
            let spec = css::parse_font_shorthand(font).ok_or_else(|| {
                font_dom_exception(
                    ctx,
                    Some(&realm.font_loading),
                    "SyntaxError",
                    "Invalid font shorthand",
                )
            })?;
            let faces = self.snapshot_entries(ctx)?;
            let rules = faces
                .iter()
                .map(|(face, _)| face.rule())
                .collect::<Vec<_>>();
            let indices = css::matching_font_faces(&spec, &rules, &text);
            let selected = indices
                .into_iter()
                .map(|index| faces[index].clone())
                .collect::<Vec<_>>();
            Ok((realm, selected))
        })();
        match result {
            Err(error) => Promise::rejected(error),
            Ok((realm, faces)) if faces.is_empty() => Promise::resolved(Vec::<Value>::new()),
            Ok((realm, faces)) => {
                let deferred = Deferred::new(ctx);
                let promise = Promise::pending(&deferred);
                for (face, _) in &faces {
                    deferred.reject_on(ctx, &face.loaded);
                    let wrapper = ensure_face_wrapper(ctx, face);
                    self.queue_face_load(ctx, &realm, face.clone(), wrapper);
                }
                realm.font_loading.enqueue_load(ctx, faces, Some(deferred));
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
    fn onloading(&self) -> Option<JsFunction> {
        self.base.handler("loading")
    }

    #[setter]
    fn set_onloading(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        handler: Option<JsFunction>,
    ) {
        self.base.set_handler(ctx, &this.0, "loading", handler);
    }

    #[getter]
    fn onloadingdone(&self) -> Option<JsFunction> {
        self.base.handler("loadingdone")
    }

    #[setter]
    fn set_onloadingdone(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        handler: Option<JsFunction>,
    ) {
        self.base.set_handler(ctx, &this.0, "loadingdone", handler);
    }

    #[getter]
    fn onloadingerror(&self) -> Option<JsFunction> {
        self.base.handler("loadingerror")
    }

    #[setter]
    fn set_onloadingerror(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        handler: Option<JsFunction>,
    ) {
        self.base.set_handler(ctx, &this.0, "loadingerror", handler);
    }
}

#[lumen_bind::class(name = "FontFaceSetIterator")]
pub(crate) struct DomFontFaceSetIterator {
    state: Weak<RefCell<FontFaceSetState>>,
    realm: Weak<DomRealm>,
    entries: bool,
    seen: RefCell<std::collections::HashSet<u64>>,
}

#[lumen_bind::methods]
impl DomFontFaceSetIterator {
    #[proto(iter)]
    fn iter(&self, this: lumen_bind::This<Value>) -> Value {
        this.0
    }

    #[proto(next)]
    fn next(&self, ctx: &mut Ctx) -> OpResult<Option<Value>> {
        let Some(state) = self.state.upgrade() else {
            return Ok(None);
        };
        let Some(realm) = self.realm.upgrade() else {
            return Ok(None);
        };
        let next = next_live_entry(ctx, &realm, &state, &mut self.seen.borrow_mut())?;
        let Some((_generation, value)) = next else {
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

#[lumen_bind::methods]
impl DomFontFaceSetLoadEvent {
    #[constructor]
    fn new(ctx: &mut Ctx, kind: &str, init: Option<Value>) -> OpResult<Self> {
        let base = DomEvent::new(ctx, kind, init.clone())?;
        let font_faces = match init {
            Some(init) if matches!(init, Value::Obj(_)) => member_get(ctx, &init, "fontfaces")?,
            _ => Value::Undefined,
        };
        let realm = active_realm(ctx)?;
        let font_faces = fontfaces_sequence(ctx, font_faces, Some(&realm.font_loading))?;
        Ok(Self { base, font_faces })
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
    let cached = font_loading.and_then(|font_loading| font_loading.object_freeze.borrow().clone());
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
    let event = DomFontFaceSetLoadEvent::new(ctx, kind, Some(init))?;
    let event = JsObject::from_value(ctx.new_instance(event))
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
            let base = realm.base_url();
            let entries = state.borrow().ordered().into_iter().collect::<Vec<_>>();
            for (face, value) in entries {
                if face.status.get() != FontFaceStatus::Unloaded {
                    continue;
                }
                let Some(result) = provider
                    .as_ref()
                    .and_then(|provider| provider.automatic_load(&face.rule(), &base))
                else {
                    continue;
                };
                face.status.set(FontFaceStatus::Loading);
                self.notify_started(ctx, &face);
                let Poll::Ready(Ok(decoded)) = result else {
                    self.enqueue_load(ctx, vec![(face, value)], None);
                    continue;
                };
                *face.decoded.borrow_mut() = Some(decoded);
                face.status.set(FontFaceStatus::Loaded);
                if let Some(deferred) = face.loaded_deferred.borrow_mut().take() {
                    deferred.resolve(ctx, value);
                }
                self.notify_completed(&face, true);
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
        assert!(
            matches!(result, Ok(Value::Bool(true))),
            "font assertion failed: {source}"
        );
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
