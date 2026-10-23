//! The `Origin` interface: an opaque or tuple origin that script can create, derive from a
//! string, `URL`, link element or window, and compare.
use super::*;
use crate::browsing_context::Origin;

#[lumen_bind::class(name = "Origin", hint(js(webidl)))]
pub struct DomOrigin {
    origin: Origin,
}

fn type_error(message: &str) -> OpError {
    OpError::type_error(message.to_owned())
}

fn origin_of_text(text: &str) -> OpResult<Origin> {
    if lumen_common::url::parse(text, None).is_err() {
        return Err(type_error("Origin.from: the string is not a valid URL"));
    }
    Ok(Origin::from_url(text))
}

fn origin_of_link(node: &DomNode) -> OpResult<Option<Origin>> {
    let namespace = node.namespace_uri()?.0;
    let local = node.local_name()?.unwrap_or_default();
    let session = node.realm.session.borrow();
    let document = session.document();
    let raw = match (namespace.as_deref(), local.as_str()) {
        (Some("http://www.w3.org/1999/xhtml"), "a" | "area") | (Some("http://www.w3.org/1998/Math/MathML"), "a") => {
            document.get_attribute_ns_ref(node.id, None, "href").map_err(dom_error)?
        }
        (Some("http://www.w3.org/2000/svg"), "a") => {
            let plain = document.get_attribute_ns_ref(node.id, None, "href").map_err(dom_error)?;
            match plain {
                Some(value) => Some(value),
                None => document
                    .get_attribute_ns_ref(node.id, Some("http://www.w3.org/1999/xlink"), "href")
                    .map_err(dom_error)?,
            }
        }
        _ => return Ok(None),
    };
    let Some(raw) = raw else { return Err(type_error("Origin.from: the element has no href")) };
    let url = node
        .realm
        .parse_document_url(raw)
        .map_err(|_| type_error("Origin.from: the element's href is not a valid URL"))?;
    Ok(Some(Origin::from_url(&url.href())))
}

#[lumen_bind::methods]
impl DomOrigin {
    #[constructor]
    fn new() -> Self {
        Self { origin: Origin::opaque() }
    }

    #[method(name = "from", hint(js(length = "1")))]
    fn from_value(ctx: &mut Ctx, #[default(Value::Undefined)] value: Value) -> OpResult<Value> {
        let origin = if matches!(value, Value::Undefined | Value::Null) {
            return Err(type_error("Origin.from: the value is not an origin source"));
        } else if let Value::Str(text) = &value {
            origin_of_text(text.as_str())?
        } else if let Some(url) = lumen_host::url::bindings::url_of_instance(ctx, &value) {
            Origin::from_url(&url.href())
        } else if let Ok(origin) = ctx.with_instance::<DomOrigin, _>(&value, |origin| origin.origin.clone()) {
            origin
        } else if let Some(realm) = crate::window_globals::window_realm_of(ctx, &value) {
            realm.document_origin().ok_or_else(|| type_error("Origin.from: the window has no origin"))?
        } else if let Some(link) = ctx.with_instance::<DomNode, _>(&value, origin_of_link).ok().transpose()?.flatten() {
            link
        } else {
            return Err(type_error("Origin.from: the value is not an origin source"));
        };
        Ok(ctx.new_instance(DomOrigin { origin }))
    }

    #[getter]
    fn opaque(&self) -> bool {
        matches!(self.origin, Origin::Opaque(_))
    }

    #[method(name = "isSameOrigin", hint(js(length = "1")))]
    fn is_same_origin(&self, ctx: &mut Ctx, #[default(Value::Undefined)] other: Value) -> OpResult<bool> {
        let other = other_origin(ctx, &other)?;
        Ok(self.origin.same_origin(&other))
    }

    #[method(name = "isSameSite", hint(js(length = "1")))]
    fn is_same_site(&self, ctx: &mut Ctx, #[default(Value::Undefined)] other: Value) -> OpResult<bool> {
        let other = other_origin(ctx, &other)?;
        Ok(match (&self.origin, &other) {
            (Origin::Tuple { scheme: a, host: ha, .. }, Origin::Tuple { scheme: b, host: hb, .. }) => {
                lumen_common::cookies::same_site_host(a, ha, b, hb)
            }
            _ => self.origin == other,
        })
    }
}

fn other_origin(ctx: &mut Ctx, value: &Value) -> OpResult<Origin> {
    ctx.with_instance::<DomOrigin, _>(value, |origin| origin.origin.clone())
        .map_err(|_| type_error("Argument 1 is not an Origin"))
}

pub(crate) fn install(ctx: &mut Ctx) -> OpResult<()> {
    let global = ctx.global_object();
    let constructor = ctx.class_constructor::<DomOrigin>();
    crate::install_interface(ctx, &global, "Origin", constructor)
        .map_err(|_| OpError::new("Error", "Origin installation failed"))
}
