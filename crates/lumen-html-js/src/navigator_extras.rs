//! `Navigator` members beyond the identity strings: the legacy plugin and MIME type lists,
//! protocol handler registration checks, and `windowControlsOverlay`.
use super::*;
use crate::browsing_context::Origin;

#[lumen_bind::class(name = "PluginArray", hint(js(webidl)))]
pub struct DomPluginArray;

#[lumen_bind::methods]
impl DomPluginArray {
    fn refresh(&self) {}
    #[proto(len)]
    fn length(&self) -> usize {
        0
    }
    #[proto(getitem)]
    fn indexed(&self, _index: usize) -> Value {
        Value::Undefined
    }
    fn item(&self, _index: u32) -> Value {
        Value::Null
    }
    #[method(name = "namedItem")]
    fn named_item(&self, _name: &str) -> Value {
        Value::Null
    }
}

#[lumen_bind::class(name = "MimeTypeArray", hint(js(webidl)))]
pub struct DomMimeTypeArray;

#[lumen_bind::methods]
impl DomMimeTypeArray {
    #[proto(len)]
    fn length(&self) -> usize {
        0
    }
    #[proto(getitem)]
    fn indexed(&self, _index: usize) -> Value {
        Value::Undefined
    }
    fn item(&self, _index: u32) -> Value {
        Value::Null
    }
    #[method(name = "namedItem")]
    fn named_item(&self, _name: &str) -> Value {
        Value::Null
    }
}

#[lumen_bind::class(name = "Plugin", hint(js(webidl)))]
pub struct DomPlugin;

#[lumen_bind::methods]
impl DomPlugin {
    #[getter]
    fn name(&self) -> &'static str {
        ""
    }
    #[getter]
    fn description(&self) -> &'static str {
        ""
    }
    #[getter]
    fn filename(&self) -> &'static str {
        ""
    }
    #[proto(len)]
    fn length(&self) -> usize {
        0
    }
    #[proto(getitem)]
    fn indexed(&self, _index: usize) -> Value {
        Value::Undefined
    }
    fn item(&self, _index: u32) -> Value {
        Value::Null
    }
    #[method(name = "namedItem")]
    fn named_item(&self, _name: &str) -> Value {
        Value::Null
    }
}

#[lumen_bind::class(name = "MimeType", hint(js(webidl)))]
pub struct DomMimeType;

#[lumen_bind::methods]
impl DomMimeType {
    #[getter(name = "type")]
    fn mime_type(&self) -> &'static str {
        ""
    }
    #[getter]
    fn description(&self) -> &'static str {
        ""
    }
    #[getter]
    fn suffixes(&self) -> &'static str {
        ""
    }
    #[getter(name = "enabledPlugin")]
    fn enabled_plugin(&self) -> Value {
        Value::Null
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

pub(crate) fn new_plugin_array(ctx: &mut Ctx) -> Value {
    ctx.new_instance(DomPluginArray)
}

pub(crate) fn new_mime_type_array(ctx: &mut Ctx) -> Value {
    ctx.new_instance(DomMimeTypeArray)
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
