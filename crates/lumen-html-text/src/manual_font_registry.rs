//! Language-neutral state for manually constructed web fonts.
//!
//! Descriptor parsing, resource fetching, font decoding, and promise
//! settlement stay with the caller. This module owns bounded font records,
//! membership, lifecycle transitions, and renderer snapshots for document
//! and worker contexts.

use alloc::{
    rc::{Rc, Weak},
    string::String,
    sync::Arc,
    vec::Vec,
};
use core::cell::RefCell;
use lumen_html::css::{FontFaceIdentity, FontFaceRule, FontFaceSource};

const MAX_MANUAL_FONTS: usize = super::MAX_REGISTERED_FONTS;
pub const MAX_MANUAL_FONT_BYTES_PER_FACE: usize = super::MAX_FONT_BYTES;
const MAX_FONT_BYTES_PER_FACE: usize = MAX_MANUAL_FONT_BYTES_PER_FACE;
const MAX_MANUAL_FONT_BYTES: usize = 16 * 1024 * 1024;
const MAX_MANUAL_METADATA_BYTES: usize = 512 * 1024;
const MAX_MANUAL_RULE_BYTES: usize = 16 * 1024;
const MAX_DOCUMENT_CSS_FACES: usize = 256;
const MAX_DOCUMENT_CSS_BYTES: usize = 1024 * 1024;
const MAX_FONT_ERROR_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FontFaceStatus {
    Unloaded,
    Loading,
    Loaded,
    Error,
}

impl FontFaceStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unloaded => "unloaded",
            Self::Loading => "loading",
            Self::Loaded => "loaded",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FontRegistryContext {
    Document,
    Worker,
}

#[derive(Clone)]
pub enum ManualFontSource {
    Url,
    Binary(Arc<[u8]>),
}

struct ManualFontFaceData {
    identity: FontFaceIdentity,
    rule: FontFaceRule,
    source: ManualFontSource,
    status: FontFaceStatus,
    decoded: Option<Arc<super::FontFace>>,
    error: Option<Arc<str>>,
}

/// A handle owned by a host `FontFace` wrapper. The registry keeps only a
/// weak reference, so detached faces do not become permanent registry roots.
#[derive(Clone)]
pub struct ManualFontFaceState {
    data: Rc<RefCell<ManualFontFaceData>>,
}

impl ManualFontFaceState {
    pub fn identity(&self) -> FontFaceIdentity {
        self.data.borrow().identity.clone()
    }

    pub fn rule(&self) -> FontFaceRule {
        self.data.borrow().rule.clone()
    }

    pub fn source(&self) -> ManualFontSource {
        self.data.borrow().source.clone()
    }

    pub fn status(&self) -> FontFaceStatus {
        self.data.borrow().status
    }

    pub fn decoded(&self) -> Option<Arc<super::FontFace>> {
        self.data.borrow().decoded.clone()
    }

    pub fn error(&self) -> Option<Arc<str>> {
        self.data.borrow().error.clone()
    }
}

/// The manual-font view consumed by existing embedder resource providers.
/// Its fields intentionally match the current `lumen-html-js` snapshot type.
#[derive(Clone)]
pub struct ManualFontFace {
    pub identity: FontFaceIdentity,
    pub rule: FontFaceRule,
    pub status: FontFaceStatus,
    pub decoded: Option<Arc<super::FontFace>>,
    pub byte_length: Option<usize>,
}

#[derive(Clone)]
pub struct FontLoadRequest {
    pub identity: FontFaceIdentity,
    pub rule: FontFaceRule,
    pub source: ManualFontSource,
}

/// A stable view for consumers that rebuild Canvas or layout font snapshots.
/// `generation` changes whenever membership, descriptors, CSS faces, or load
/// state changes. CSS faces are empty for workers.
pub struct FontRegistrySnapshot<'a> {
    pub generation: u64,
    pub document_css_faces: &'a [FontFaceRule],
    pub manual_faces: &'a [ManualFontFace],
}

#[derive(Default)]
struct FontUsage {
    faces: usize,
    font_bytes: usize,
    font_allocations: Vec<usize>,
    metadata_bytes: usize,
}

/// Context-local registry for manually created `FontFace` objects.
///
/// It stores only weak handles to face state. The host owns each live face and
/// decides when to fetch, decode, and settle language-specific promises.
pub struct ManualFontRegistry {
    context: FontRegistryContext,
    next_identity: u64,
    faces: Vec<Weak<RefCell<ManualFontFaceData>>>,
    members: Vec<FontFaceIdentity>,
    document_css_faces: Vec<FontFaceRule>,
    generation: u64,
    snapshot_generation: Option<u64>,
    manual_snapshot: Vec<ManualFontFace>,
}

impl ManualFontRegistry {
    pub fn new(context: FontRegistryContext) -> Self {
        Self {
            context,
            next_identity: 0,
            faces: Vec::new(),
            members: Vec::new(),
            document_css_faces: Vec::new(),
            generation: 0,
            snapshot_generation: None,
            manual_snapshot: Vec::new(),
        }
    }

    pub fn context(&self) -> FontRegistryContext {
        self.context
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Create state for a manual `FontFace`, assigning an identity within this
    /// document or worker. The host can keep the returned handle for detached
    /// faces and later add it to that context's font set.
    pub fn create_manual_face(
        &mut self,
        mut rule: FontFaceRule,
        source: ManualFontSource,
    ) -> Result<ManualFontFaceState, &'static str> {
        self.prune_dead_faces();
        let usage = self.font_usage()?;
        if usage.faces >= MAX_MANUAL_FONTS {
            return Err("too many live manual font faces");
        }
        let metadata_bytes = rule_metadata_bytes(&rule).ok_or("font metadata size overflow")?;
        if metadata_bytes > MAX_MANUAL_RULE_BYTES
            || usage
                .metadata_bytes
                .checked_add(metadata_bytes)
                .is_none_or(|total| total > MAX_MANUAL_METADATA_BYTES)
        {
            return Err("manual font metadata budget exceeded");
        }
        let source_bytes = match &source {
            ManualFontSource::Url => 0,
            ManualFontSource::Binary(bytes) => {
                if bytes.len() > MAX_FONT_BYTES_PER_FACE {
                    return Err("font too large");
                }
                bytes.len()
            }
        };
        let additional_bytes = match &source {
            ManualFontSource::Binary(bytes)
                if !usage
                    .font_allocations
                    .contains(&(bytes.as_ptr() as *const () as usize)) =>
            {
                source_bytes
            }
            _ => 0,
        };
        if usage
            .font_bytes
            .checked_add(additional_bytes)
            .is_none_or(|total| total > MAX_MANUAL_FONT_BYTES)
        {
            return Err("manual font byte budget exceeded");
        }

        let next = self
            .next_identity
            .checked_add(1)
            .ok_or("FontFace identity space exhausted")?;
        let identity = FontFaceIdentity::Manual(next);
        rule.identity = Some(identity.clone());
        self.faces
            .try_reserve(1)
            .map_err(|_| "font registry allocation failed")?;
        let data = Rc::new(RefCell::new(ManualFontFaceData {
            identity,
            rule,
            source,
            status: FontFaceStatus::Unloaded,
            decoded: None,
            error: None,
        }));
        self.faces.push(Rc::downgrade(&data));
        self.next_identity = next;
        Ok(ManualFontFaceState { data })
    }

    /// Add a live face to this context's `FontFaceSet`. Repeated addition is
    /// idempotent and preserves the original insertion order.
    pub fn add_manual_face(&mut self, face: &ManualFontFaceState) -> Result<bool, &'static str> {
        let identity = self.ensure_owned(face)?;
        if self.members.contains(&identity) {
            return Ok(false);
        }
        if self.members.len() >= MAX_MANUAL_FONTS {
            return Err("too many registered manual font faces");
        }
        self.members
            .try_reserve(1)
            .map_err(|_| "font registry allocation failed")?;
        self.members.push(identity);
        self.bump_generation();
        Ok(true)
    }

    pub fn delete_manual_face(&mut self, face: &ManualFontFaceState) -> Result<bool, &'static str> {
        let identity = self.ensure_owned(face)?;
        let before = self.members.len();
        self.members.retain(|member| member != &identity);
        let removed = before != self.members.len();
        if removed {
            self.bump_generation();
        }
        Ok(removed)
    }

    pub fn clear_manual_faces(&mut self) -> bool {
        if self.members.is_empty() {
            return false;
        }
        self.members.clear();
        self.bump_generation();
        true
    }

    /// Return manual members in insertion order. Callers that need the full
    /// renderer view should use `snapshot`; DOM adapters can use this compact
    /// identity list to join their language-specific wrapper roots.
    pub fn member_identities(&mut self) -> &[FontFaceIdentity] {
        self.prune_dead_faces();
        &self.members
    }

    pub fn contains_manual_face(
        &mut self,
        face: &ManualFontFaceState,
    ) -> Result<bool, &'static str> {
        let identity = self.ensure_owned(face)?;
        Ok(self.members.contains(&identity))
    }

    /// Replace a manual face's already parsed descriptors. Parsing and CSS
    /// serialization remain with the language adapter.
    pub fn update_manual_rule(
        &mut self,
        face: &ManualFontFaceState,
        mut rule: FontFaceRule,
    ) -> Result<bool, &'static str> {
        let identity = self.ensure_owned(face)?;
        rule.identity = Some(identity);
        let new_bytes = rule_metadata_bytes(&rule).ok_or("font metadata size overflow")?;
        if new_bytes > MAX_MANUAL_RULE_BYTES {
            return Err("manual font metadata budget exceeded");
        }
        let old_rule = face.data.borrow().rule.clone();
        if old_rule == rule {
            return Ok(false);
        }
        let old_bytes = rule_metadata_bytes(&old_rule).ok_or("font metadata size overflow")?;
        let usage = self.font_usage()?;
        if usage
            .metadata_bytes
            .saturating_sub(old_bytes)
            .checked_add(new_bytes)
            .is_none_or(|total| total > MAX_MANUAL_METADATA_BYTES)
        {
            return Err("manual font metadata budget exceeded");
        }
        face.data.borrow_mut().rule = rule;
        self.bump_generation();
        Ok(true)
    }

    /// Begin or resume a host-owned fetch/decode operation. A request carries
    /// only parsed rule/source data; the caller chooses the URL base and
    /// scheduler. Loaded and failed faces return `None`.
    pub fn begin_load(
        &mut self,
        face: &ManualFontFaceState,
    ) -> Result<Option<FontLoadRequest>, &'static str> {
        self.ensure_owned(face)?;
        let mut data = face.data.borrow_mut();
        match data.status {
            FontFaceStatus::Loaded | FontFaceStatus::Error => return Ok(None),
            FontFaceStatus::Unloaded => {
                data.status = FontFaceStatus::Loading;
                data.error = None;
                let request = FontLoadRequest {
                    identity: data.identity.clone(),
                    rule: data.rule.clone(),
                    source: data.source.clone(),
                };
                drop(data);
                self.bump_generation();
                Ok(Some(request))
            }
            FontFaceStatus::Loading => Ok(Some(FontLoadRequest {
                identity: data.identity.clone(),
                rule: data.rule.clone(),
                source: data.source.clone(),
            })),
        }
    }

    /// Complete a load started by `begin_load`. The registry checks the
    /// decoded face against the same per-face limit used by `FontFace` and
    /// accounts unique byte allocations across live manual records.
    pub fn complete_load(
        &mut self,
        face: &ManualFontFaceState,
        result: Result<Arc<super::FontFace>, Arc<str>>,
    ) -> Result<FontFaceStatus, &'static str> {
        self.ensure_owned(face)?;
        let status = face.status();
        if status != FontFaceStatus::Loading {
            return match status {
                FontFaceStatus::Loaded | FontFaceStatus::Error => Ok(status),
                FontFaceStatus::Unloaded => Err("font face load was not started"),
                FontFaceStatus::Loading => unreachable!(),
            };
        }

        let (decoded, error) = match result {
            Err(error) => (None, Some(bounded_error(error))),
            Ok(decoded) => {
                let decoded_bytes = decoded.bytes.len();
                let usage = self.font_usage()?;
                let additional = if usage
                    .font_allocations
                    .contains(&(decoded.bytes.as_ptr() as *const () as usize))
                {
                    0
                } else {
                    decoded_bytes
                };
                if decoded_bytes > MAX_FONT_BYTES_PER_FACE
                    || usage
                        .font_bytes
                        .checked_add(additional)
                        .is_none_or(|total| total > MAX_MANUAL_FONT_BYTES)
                {
                    (None, Some(Arc::from("manual font byte budget exceeded")))
                } else {
                    (Some(decoded), None)
                }
            }
        };
        let status = if decoded.is_some() {
            FontFaceStatus::Loaded
        } else {
            FontFaceStatus::Error
        };
        {
            let mut data = face.data.borrow_mut();
            data.decoded = decoded;
            data.error = error;
            data.status = status;
        }
        self.bump_generation();
        Ok(status)
    }

    /// Record a failure discovered before resource loading, such as invalid
    /// constructor descriptors. The language adapter remains responsible for
    /// rejecting its promise with the matching exception type.
    pub fn fail_manual_face(
        &mut self,
        face: &ManualFontFaceState,
        error: Arc<str>,
    ) -> Result<FontFaceStatus, &'static str> {
        self.ensure_owned(face)?;
        let mut data = face.data.borrow_mut();
        if matches!(data.status, FontFaceStatus::Loaded | FontFaceStatus::Error) {
            return Ok(data.status);
        }
        data.status = FontFaceStatus::Error;
        data.decoded = None;
        data.error = Some(bounded_error(error));
        drop(data);
        self.bump_generation();
        Ok(FontFaceStatus::Error)
    }

    /// Synchronize the document's current CSS `@font-face` descriptors.
    /// Workers do not have CSS and reject this operation explicitly.
    pub fn replace_document_css_faces(
        &mut self,
        faces: &[FontFaceRule],
    ) -> Result<bool, &'static str> {
        if self.context != FontRegistryContext::Document {
            return Err("worker font registries do not have CSS font faces");
        }
        if faces.len() > MAX_DOCUMENT_CSS_FACES {
            return Err("too many document CSS font faces");
        }
        if faces
            .iter()
            .any(|rule| !matches!(rule.identity.as_ref(), Some(FontFaceIdentity::Css(_))))
        {
            return Err("document CSS font face is missing its CSS identity");
        }
        let mut metadata_bytes = 0usize;
        for (index,rule) in faces.iter().enumerate() {
            if !rule.family_display.is_empty() && !faces[..index].iter().any(|old|Arc::ptr_eq(&old.family_display,&rule.family_display)) {
                metadata_bytes=metadata_bytes.checked_add(shared_feature_metadata_bytes(&rule.family_display).ok_or("font metadata size overflow")?).ok_or("font metadata size overflow")?;
            }
            let bytes = rule_owned_metadata_bytes(rule).ok_or("font metadata size overflow")?;
            metadata_bytes = metadata_bytes
                .checked_add(bytes)
                .ok_or("font metadata size overflow")?;
            if metadata_bytes > MAX_DOCUMENT_CSS_BYTES {
                return Err("document CSS font metadata budget exceeded");
            }
        }
        if self.document_css_faces.as_slice() == faces {
            return Ok(false);
        }
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(faces.len())
            .map_err(|_| "font registry allocation failed")?;
        snapshot.extend_from_slice(faces);
        self.document_css_faces = snapshot;
        self.bump_generation();
        Ok(true)
    }

    /// Return a cached, insertion-ordered snapshot for embedder font providers
    /// and Canvas font-set rebuilds. Rebuilding occurs only after a generation
    /// change, and dead detached/member handles are pruned first.
    pub fn snapshot(&mut self) -> Result<FontRegistrySnapshot<'_>, &'static str> {
        self.prune_dead_faces();
        if self.snapshot_generation != Some(self.generation) {
            let mut snapshot = Vec::new();
            snapshot
                .try_reserve_exact(self.members.len())
                .map_err(|_| "font registry allocation failed")?;
            for identity in &self.members {
                let Some(face) = self.find_face(identity) else {
                    continue;
                };
                let data = face.data.borrow();
                snapshot.push(ManualFontFace {
                    identity: data.identity.clone(),
                    rule: data.rule.clone(),
                    status: data.status,
                    decoded: data.decoded.clone(),
                    byte_length: match &data.source {
                        ManualFontSource::Binary(bytes) => Some(bytes.len()),
                        ManualFontSource::Url => None,
                    },
                });
            }
            self.manual_snapshot = snapshot;
            self.snapshot_generation = Some(self.generation);
        }
        Ok(FontRegistrySnapshot {
            generation: self.generation,
            document_css_faces: &self.document_css_faces,
            manual_faces: &self.manual_snapshot,
        })
    }

    fn ensure_owned(
        &mut self,
        face: &ManualFontFaceState,
    ) -> Result<FontFaceIdentity, &'static str> {
        self.prune_dead_faces();
        let identity = face.identity();
        let owned = self
            .faces
            .iter()
            .filter_map(Weak::upgrade)
            .any(|data| Rc::ptr_eq(&data, &face.data));
        if owned {
            Ok(identity)
        } else {
            Err("font face belongs to a different registry")
        }
    }

    fn find_face(&self, identity: &FontFaceIdentity) -> Option<ManualFontFaceState> {
        self.faces
            .iter()
            .filter_map(Weak::upgrade)
            .find_map(|data| {
                let matches = data.borrow().identity == *identity;
                matches.then_some(ManualFontFaceState { data })
            })
    }

    fn prune_dead_faces(&mut self) {
        self.faces.retain(|face| face.strong_count() != 0);
        let faces = &self.faces;
        let old_members = self.members.len();
        self.members.retain(|identity| {
            faces.iter().any(|face| {
                face.upgrade()
                    .is_some_and(|data| data.borrow().identity == *identity)
            })
        });
        if old_members != self.members.len() {
            self.bump_generation();
        }
    }

    fn font_usage(&self) -> Result<FontUsage, &'static str> {
        let mut usage = FontUsage::default();
        usage
            .font_allocations
            .try_reserve(self.faces.len().saturating_mul(2))
            .map_err(|_| "font registry allocation failed")?;
        for face in self.faces.iter().filter_map(Weak::upgrade) {
            let data = face.borrow();
            usage.faces = usage
                .faces
                .checked_add(1)
                .ok_or("font registry size overflow")?;
            let rule_bytes =
                rule_metadata_bytes(&data.rule).ok_or("font metadata size overflow")?;
            usage.metadata_bytes = usage
                .metadata_bytes
                .checked_add(rule_bytes)
                .ok_or("font metadata size overflow")?;
            if let ManualFontSource::Binary(bytes) = &data.source {
                include_font_bytes(&mut usage, bytes)?;
            }
            if let Some(decoded) = &data.decoded {
                include_font_bytes(&mut usage, &decoded.bytes)?;
            }
        }
        Ok(usage)
    }

    fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.snapshot_generation = None;
        self.manual_snapshot.clear();
    }
}

fn include_font_bytes(usage: &mut FontUsage, bytes: &Arc<[u8]>) -> Result<(), &'static str> {
    let pointer = bytes.as_ptr() as *const () as usize;
    if usage.font_allocations.contains(&pointer) {
        return Ok(());
    }
    usage
        .font_allocations
        .try_reserve(1)
        .map_err(|_| "font registry allocation failed")?;
    usage.font_allocations.push(pointer);
    usage.font_bytes = usage
        .font_bytes
        .checked_add(bytes.len())
        .ok_or("font registry size overflow")?;
    Ok(())
}

fn bounded_error(error: Arc<str>) -> Arc<str> {
    if error.len() <= MAX_FONT_ERROR_BYTES {
        return error;
    }
    let mut end = MAX_FONT_ERROR_BYTES;
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    Arc::from(&error[..end])
}

fn rule_metadata_bytes(rule:&FontFaceRule)->Option<usize> {
    rule_owned_metadata_bytes(rule)?.checked_add(shared_feature_metadata_bytes(&rule.family_display)?)
}
fn rule_owned_metadata_bytes(rule: &FontFaceRule) -> Option<usize> {
    let descriptors = &rule.descriptors;
    let mut bytes = 0usize;
    let mut add = |length: usize| -> Option<()> {
        bytes = bytes.checked_add(length)?;
        Some(())
    };
    add(descriptors.family.len())?;
    add(descriptors
        .sources
        .len()
        .checked_mul(core::mem::size_of::<FontFaceSource>())?)?;
    for source in descriptors.sources.iter() {
        add(match source {
            FontFaceSource::Url(value) | FontFaceSource::Local(value) => value.len(),
        })?;
    }
    for value in [
        descriptors.weight_keyword.as_deref(),
        descriptors.stretch_keyword.as_deref(),
        descriptors.unicode_range.as_deref(),
        Some(descriptors.variation_settings.as_ref()),
        rule.source_url.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        add(value.len())?;
    }
    if let Some(settings) = descriptors.feature_settings.as_ref() {
        add(2 * core::mem::size_of::<usize>() + core::mem::size_of_val(settings.as_ref()))?;
        add(settings.retained_bytes())?;
    }
    if let Some(expressions) = descriptors.stretch_expressions.as_ref() {
        add(2 * core::mem::size_of::<usize>() + core::mem::size_of_val(expressions.as_ref()))?;
        for expression in expressions.iter() { add(expression.retained_bytes())?; }
    }
    add(rule
        .import_path
        .len()
        .checked_mul(core::mem::size_of::<usize>())?)?;
    add(rule
        .media
        .len()
        .checked_mul(core::mem::size_of::<Arc<str>>())?)?;
    for value in rule.media.iter().chain(rule.supports.iter()) {
        add(value.len())?;
    }
    add(rule
        .supports
        .len()
        .checked_mul(core::mem::size_of::<Arc<str>>())?)?;
    for value in rule.layers.iter() {
        add(value.len())?;
    }
    add(rule
        .layers
        .len()
        .checked_mul(core::mem::size_of::<String>())?)?;
    if let Some(scope)=&rule.family_scope {add(core::mem::size_of_val(scope.as_ref()))?;}
    Some(bytes)
}
fn shared_feature_metadata_bytes(rules:&[lumen_html::css::FontFamilyDisplayRule])->Option<usize> {
    let mut bytes=core::mem::size_of_val(rules);
    for rule in rules {
        bytes=bytes.checked_add(core::mem::size_of_val(rule.families.as_ref()))?.checked_add(core::mem::size_of_val(rule.aliases.as_ref()))?;
        for family in rule.families.iter() {bytes=bytes.checked_add(family.len())?;}
        for alias in rule.aliases.iter() {bytes=bytes.checked_add(alias.name.len())?.checked_add(core::mem::size_of_val(alias.indices.as_ref()))?;}
        for condition in rule.media.iter().chain(rule.supports.iter()) {bytes=bytes.checked_add(condition.len())?;}
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FontFace, DEFAULT_FONT_BYTES};
    use lumen_html::css::FontFaceDescriptors;

    fn rule(family: &str) -> FontFaceRule {
        FontFaceDescriptors::parse(family, &[])
            .expect("fixture descriptor")
            .to_rule(None)
    }

    #[test]
    fn manual_load_and_membership_changes_advance_renderer_generation() {
        let mut registry = ManualFontRegistry::new(FontRegistryContext::Document);
        assert!(!registry.replace_document_css_faces(&[]).unwrap());
        let face = registry
            .create_manual_face(rule("registry"), ManualFontSource::Url)
            .unwrap();
        let created_generation = registry.generation();
        assert!(registry.add_manual_face(&face).unwrap());
        assert!(registry.generation() > created_generation);
        assert_eq!(registry.member_identities(), &[face.identity()]);

        let before_load = registry.generation();
        assert!(registry.begin_load(&face).unwrap().is_some());
        assert!(registry.generation() > before_load);
        let before_completion = registry.generation();
        let decoded = Arc::new(FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap());
        assert_eq!(
            registry.complete_load(&face, Ok(decoded)).unwrap(),
            FontFaceStatus::Loaded
        );
        assert!(registry.generation() > before_completion);
        assert_eq!(
            registry.snapshot().unwrap().manual_faces[0].status,
            FontFaceStatus::Loaded
        );

        assert!(registry.delete_manual_face(&face).unwrap());
        assert!(registry.member_identities().is_empty());
    }

    #[test]
    fn worker_registry_isolates_faces_and_reclaims_snapshot_bytes() {
        let mut registry = ManualFontRegistry::new(FontRegistryContext::Worker);
        let mut document = ManualFontRegistry::new(FontRegistryContext::Document);
        let foreign = document
            .create_manual_face(rule("foreign"), ManualFontSource::Url)
            .unwrap();
        let face = registry
            .create_manual_face(rule("worker"), ManualFontSource::Url)
            .unwrap();
        assert_eq!(foreign.identity(), face.identity());
        assert!(registry.add_manual_face(&foreign).is_err());
        assert!(registry.replace_document_css_faces(&[]).is_err());
        registry.add_manual_face(&face).unwrap();
        registry.begin_load(&face).unwrap();
        let decoded = Arc::new(FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap());
        let decoded_bytes = Arc::downgrade(&decoded.bytes);
        registry.complete_load(&face, Ok(decoded)).unwrap();
        drop(registry.snapshot().unwrap());
        assert!(decoded_bytes.upgrade().is_some());

        registry.delete_manual_face(&face).unwrap();
        drop(face);
        assert!(decoded_bytes.upgrade().is_none());
    }

    #[test]
    fn descriptor_updates_and_bounded_failures_invalidate_snapshots() {
        let mut registry = ManualFontRegistry::new(FontRegistryContext::Document);
        let oversized_font = Arc::<[u8]>::from(alloc::vec![0; MAX_FONT_BYTES_PER_FACE + 1]);
        assert!(registry
            .create_manual_face(
                rule("oversized font"),
                ManualFontSource::Binary(oversized_font),
            )
            .is_err());

        let mut oversized = rule("oversized");
        oversized.descriptors.family = Arc::from("x".repeat(MAX_MANUAL_RULE_BYTES + 1));
        assert!(registry
            .create_manual_face(oversized, ManualFontSource::Url)
            .is_err());

        let face = registry
            .create_manual_face(rule("before"), ManualFontSource::Url)
            .unwrap();
        registry.add_manual_face(&face).unwrap();
        let mut changed = rule("after");
        changed.identity = Some(face.identity());
        assert!(registry.update_manual_rule(&face, changed).unwrap());
        assert_eq!(face.rule().descriptors.family.as_ref(), "after");
        let long_error = Arc::<str>::from("é".repeat(MAX_FONT_ERROR_BYTES));
        registry.fail_manual_face(&face, long_error).unwrap();
        let snapshot = registry.snapshot().unwrap();
        assert_eq!(snapshot.manual_faces[0].status, FontFaceStatus::Error);
        let error = face.error().unwrap();
        assert_eq!(error.len(), MAX_FONT_ERROR_BYTES);
        assert!(error.is_char_boundary(error.len()));
    }
}
