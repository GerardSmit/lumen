//! `Navigator` members beyond the identity strings: the legacy plugin and MIME type lists,
//! protocol handler registration checks, and `windowControlsOverlay`.
use super::*;
use crate::browsing_context::Origin;

const PDF_PLUGIN_NAMES: [&str; 5] = [
    "PDF Viewer",
    "Chrome PDF Viewer",
    "Chromium PDF Viewer",
    "Microsoft Edge PDF Viewer",
    "WebKit built-in PDF",
];
const PDF_MIME_TYPES: [&str; 2] = ["application/pdf", "text/pdf"];
const PDF_DESCRIPTION: &str = "Portable Document Format";
const PDF_FILENAME: &str = "internal-pdf-viewer";
const ENABLED_PLUGIN_SLOT: &str = "#lumen_plugins\u{1}enabledPlugin";

fn named_value(items: &[Value], names: &[&str], name: &str) -> Option<Value> {
    let position = names.iter().position(|candidate| *candidate == name)?;
    items.get(position).cloned()
}

fn listed_names(items: &[Value], names: &[&str]) -> Vec<String> {
    names.iter().take(items.len()).map(|name| (*name).to_owned()).collect()
}

#[lumen_bind::class(name = "PluginArray", hint(js(webidl, named_properties)))]
pub struct DomPluginArray {
    plugins: Vec<Value>,
}

impl lumen::embed::NativeIdentityOwner for DomPluginArray {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        self.plugins.iter().for_each(visit);
    }
}

#[lumen_bind::methods]
impl DomPluginArray {
    #[method(hint(js(named_supported)))]
    fn named_supported(&self, name: &str) -> bool {
        named_value(&self.plugins, &PDF_PLUGIN_NAMES, name).is_some()
    }
    #[method(hint(js(named_names)))]
    fn named_names(&self) -> Vec<String> {
        listed_names(&self.plugins, &PDF_PLUGIN_NAMES)
    }
    #[method(hint(js(named_getter)))]
    fn named_getter(&self, name: &str) -> Value {
        self.named_item(name)
    }
    fn refresh(&self) {}
    #[proto(len)]
    fn length(&self) -> usize {
        self.plugins.len()
    }
    #[proto(getitem)]
    fn indexed(&self, index: usize) -> Value {
        self.plugins.get(index).cloned().unwrap_or(Value::Undefined)
    }
    fn item(&self, index: u32) -> Value {
        self.plugins.get(index as usize).cloned().unwrap_or(Value::Null)
    }
    #[method(name = "namedItem")]
    fn named_item(&self, name: &str) -> Value {
        named_value(&self.plugins, &PDF_PLUGIN_NAMES, name).unwrap_or(Value::Null)
    }
}

#[lumen_bind::class(name = "MimeTypeArray", hint(js(webidl, named_properties)))]
pub struct DomMimeTypeArray {
    mime_types: Vec<Value>,
}

impl lumen::embed::NativeIdentityOwner for DomMimeTypeArray {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        self.mime_types.iter().for_each(visit);
    }
}

#[lumen_bind::methods]
impl DomMimeTypeArray {
    #[method(hint(js(named_supported)))]
    fn named_supported(&self, name: &str) -> bool {
        named_value(&self.mime_types, &PDF_MIME_TYPES, name).is_some()
    }
    #[method(hint(js(named_names)))]
    fn named_names(&self) -> Vec<String> {
        listed_names(&self.mime_types, &PDF_MIME_TYPES)
    }
    #[method(hint(js(named_getter)))]
    fn named_getter(&self, name: &str) -> Value {
        self.named_item(name)
    }
    #[proto(len)]
    fn length(&self) -> usize {
        self.mime_types.len()
    }
    #[proto(getitem)]
    fn indexed(&self, index: usize) -> Value {
        self.mime_types.get(index).cloned().unwrap_or(Value::Undefined)
    }
    fn item(&self, index: u32) -> Value {
        self.mime_types.get(index as usize).cloned().unwrap_or(Value::Null)
    }
    #[method(name = "namedItem")]
    fn named_item(&self, name: &str) -> Value {
        named_value(&self.mime_types, &PDF_MIME_TYPES, name).unwrap_or(Value::Null)
    }
}

#[lumen_bind::class(name = "Plugin", hint(js(webidl, named_properties)))]
pub struct DomPlugin {
    name: &'static str,
    mime_types: Vec<Value>,
}

impl lumen::embed::NativeIdentityOwner for DomPlugin {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        self.mime_types.iter().for_each(visit);
    }
}

#[lumen_bind::methods]
impl DomPlugin {
    #[method(hint(js(named_supported)))]
    fn named_supported(&self, name: &str) -> bool {
        named_value(&self.mime_types, &PDF_MIME_TYPES, name).is_some()
    }
    #[method(hint(js(named_names)))]
    fn named_names(&self) -> Vec<String> {
        listed_names(&self.mime_types, &PDF_MIME_TYPES)
    }
    #[method(hint(js(named_getter)))]
    fn named_getter(&self, name: &str) -> Value {
        self.named_item(name)
    }
    #[getter]
    fn name(&self) -> &'static str {
        self.name
    }
    #[getter]
    fn description(&self) -> &'static str {
        PDF_DESCRIPTION
    }
    #[getter]
    fn filename(&self) -> &'static str {
        PDF_FILENAME
    }
    #[proto(len)]
    fn length(&self) -> usize {
        self.mime_types.len()
    }
    #[proto(getitem)]
    fn indexed(&self, index: usize) -> Value {
        self.mime_types.get(index).cloned().unwrap_or(Value::Undefined)
    }
    fn item(&self, index: u32) -> Value {
        self.mime_types.get(index as usize).cloned().unwrap_or(Value::Null)
    }
    #[method(name = "namedItem")]
    fn named_item(&self, name: &str) -> Value {
        named_value(&self.mime_types, &PDF_MIME_TYPES, name).unwrap_or(Value::Null)
    }
}

#[lumen_bind::class(name = "MimeType", hint(js(webidl)))]
pub struct DomMimeType {
    kind: &'static str,
}

#[lumen_bind::methods]
impl DomMimeType {
    #[getter(name = "type")]
    fn mime_type(&self) -> &'static str {
        self.kind
    }
    #[getter]
    fn description(&self) -> &'static str {
        PDF_DESCRIPTION
    }
    #[getter]
    fn suffixes(&self) -> &'static str {
        "pdf"
    }
    #[getter(name = "enabledPlugin")]
    fn enabled_plugin(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        ctx.native_private_value_slot(&this.0, ENABLED_PLUGIN_SLOT).unwrap_or(Value::Null)
    }
}

#[lumen_bind::class(name = "WindowControlsOverlay", extends = DomEventTarget, hint(js(webidl)))]
pub struct DomWindowControlsOverlay {
    base: DomEventTarget,
}

#[lumen_bind::methods]
impl DomWindowControlsOverlay {
    #[getter]
    fn visible(&self) -> bool {
        false
    }

    #[method(name = "getTitlebarAreaRect")]
    fn get_titlebar_area_rect(&self, ctx: &mut Ctx) -> Value {
        crate::geometry::rect_value(
            ctx,
            lumen_html::paint::Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
        )
    }

    #[getter]
    fn ongeometrychange(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.handler_value(ctx, &this.0, "geometrychange")
    }

    #[setter]
    fn set_ongeometrychange(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: crate::events::EventHandler,
    ) {
        self.base.set_event_handler(ctx, &this.0, "geometrychange", callback);
    }
}

/// The `navigator.plugins` and `navigator.mimeTypes` lists: empty without a PDF viewer, otherwise
/// Chrome's five PDF plugins sharing the two PDF MIME types.
pub(crate) fn new_plugin_arrays(ctx: &mut Ctx, pdf_viewer_enabled: bool) -> OpResult<(Value, Value)> {
    let mut mime_types = Vec::new();
    let mut plugins = Vec::new();
    if pdf_viewer_enabled {
        for kind in PDF_MIME_TYPES {
            mime_types.push(ctx.new_instance(DomMimeType { kind }));
        }
        for name in PDF_PLUGIN_NAMES {
            let plugin = ctx.new_instance(DomPlugin { name, mime_types: mime_types.clone() });
            ctx.set_native_identity_owner::<DomPlugin>(&plugin)?;
            plugins.push(plugin);
        }
        for mime_type in &mime_types {
            ctx.set_native_internal_value_slot(mime_type, ENABLED_PLUGIN_SLOT, plugins[0].clone())
                .map_err(OpError::thrown)?;
        }
    }
    let plugin_array = ctx.new_instance(DomPluginArray { plugins });
    ctx.set_native_identity_owner::<DomPluginArray>(&plugin_array)?;
    let mime_type_array = ctx.new_instance(DomMimeTypeArray { mime_types });
    ctx.set_native_identity_owner::<DomMimeTypeArray>(&mime_type_array)?;
    Ok((plugin_array, mime_type_array))
}

pub(crate) fn new_window_controls_overlay(ctx: &mut Ctx) -> Value {
    ctx.new_instance(DomWindowControlsOverlay {
        base: DomEventTarget::from_data(TargetData::new(None)),
    })
}

pub(crate) fn constructors(ctx: &mut Ctx) -> Vec<(&'static str, Value)> {
    vec![
        ("PluginArray", ctx.class_constructor::<DomPluginArray>()),
        ("MimeTypeArray", ctx.class_constructor::<DomMimeTypeArray>()),
        ("Plugin", ctx.class_constructor::<DomPlugin>()),
        ("MimeType", ctx.class_constructor::<DomMimeType>()),
        ("WindowControlsOverlay", ctx.class_constructor::<DomWindowControlsOverlay>()),
    ]
}

const SAFELISTED_SCHEMES: &[&str] = &[
    "bitcoin", "ftp", "ftps", "geo", "im", "irc", "ircs", "magnet", "mailto", "matrix", "mms", "news",
    "nntp", "openpgp4fpr", "sftp", "sip", "sms", "smsto", "ssh", "tel", "urn", "webcal", "wtai", "xmpp",
    "cabal", "dat", "did", "dweb", "ethereum", "hyper", "ipfs", "ipns", "ssb",
];

fn scheme_may_be_handled(scheme: &str) -> bool {
    let lower = scheme.to_ascii_lowercase();
    if SAFELISTED_SCHEMES.contains(&lower.as_str()) {
        return true;
    }
    lower
        .strip_prefix("web+")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_lowercase()))
}

/// The argument checks shared by `registerProtocolHandler` and `unregisterProtocolHandler`.
pub(crate) fn check_protocol_handler(realm: &DomRealm, scheme: &str, url: &str) -> OpResult<()> {
    if !scheme_may_be_handled(scheme) {
        return Err(OpError::new("SecurityError", "the scheme cannot be handled by a web page"));
    }
    if !url.contains("%s") {
        return Err(OpError::new("SyntaxError", "the handler URL must contain \"%s\""));
    }
    let parsed = realm
        .parse_document_url(url)
        .map_err(|_| OpError::new("SyntaxError", "the handler URL is not a valid URL"))?;
    if !matches!(parsed.scheme.as_str(), "http" | "https") {
        return Err(OpError::new("SecurityError", "the handler URL must use http or https"));
    }
    let own = realm
        .document_origin()
        .unwrap_or_else(|| Origin::from_url(&realm.base_url()));
    if !own.same_origin(&Origin::from_url(&parsed.href())) {
        return Err(OpError::new("SecurityError", "the handler URL must be same-origin with the document"));
    }
    Ok(())
}
