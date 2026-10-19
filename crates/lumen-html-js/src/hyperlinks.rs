//! Hyperlink URL reflection uses the shared URL parser and the live document base.
use super::*;
use lumen_common::url::{self, Url};

/// Capture the activation element from the dispatch path, then read its live
/// document, attributes and URL after listeners have run. Retaining the actual
/// wrapper lets ordinary adoption retarget the default action.
pub(crate) fn activation_behavior(ctx: &mut Ctx, receiver: &Value, event: &Value) -> OpResult<Option<lumen_host::events::PreparedActivation>> {
    let Some((realm, id)) = ctx.with_instance::<DomNode, _>(receiver, |node| (node.realm.clone(), node.id)).ok() else { return Ok(None); };
    let is_link = {
        let session = realm.session.borrow();
        let document = session.document();
        matches!(lumen_html::forms::html_element_local_name(document, id), Some("a" | "area"))
    };
    if !is_link { return Ok(None); }
    let Some(coordinates) = ctx.with_instance::<super::ui_events::DomMouseEvent, _>(event, |event| event.activation_coordinates()).ok() else { return Ok(None); };
    let Some((kind, trusted)) = ctx.with_instance::<DomEvent, _>(event, |event| (event.kind(), event.trusted_dispatch())).ok() else { return Ok(None); };
    if kind != "click" { return Ok(None); }
    let element = receiver.clone();
    let event = event.clone();
    Ok(Some(Box::new(move |ctx: &mut Ctx, accepted: bool| {
        if !accepted { return Ok(()); }
        let Some((realm, id)) = ctx.with_instance::<DomNode, _>(&element, |node| (node.realm.clone(), node.id)).ok() else { return Ok(()); };
        let Some(source) = realm.browsing_context().filter(|context| super::browsing_context::is_active_document(context, &realm)) else { return Ok(()); };
        let (href, mut target, is_anchor, download) = {
            let session = realm.session.borrow();
            let document = session.document();
            let name = lumen_html::forms::html_element_local_name(document, id);
            if !matches!(name, Some("a" | "area")) || (name == Some("area") && !document.is_connected_element(id)) { return Ok(()); }
            let Some(href) = document.get_attribute_ns_ref(id, None, "href").map_err(dom_error)? else { return Ok(()); };
            let target = if let Some(target) = document.get_attribute_ns_ref(id, None, "target").map_err(dom_error)? { target.to_owned() } else {
                let mut node = document.root();
                let mut target = String::new();
                while let Some(next) = lumen_html::selector::next_descendant(document, document.root(), node).map_err(dom_error)? {
                    node = next;
                    if lumen_html::forms::html_element_local_name(document, node) == Some("base") {
                        if let Some(value) = document.get_attribute_ns_ref(node, None, "target").map_err(dom_error)? { target = value.to_owned(); break; }
                    }
                }
                target
            };
            (href.to_owned(), target, name == Some("a"), document.get_attribute_ns_ref(id, None, "download").map_err(dom_error)?.is_some())
        };
        if target.contains('<') && target.bytes().any(|byte| matches!(byte, b'\t' | b'\n' | b'\r')) { target = "_blank".into(); }
        // The embedded host does not offer a download destination. A requested
        // download therefore cannot be converted into a document navigation.
        if download { return Ok(()); }
        let Ok(parsed) = url::parse(&href, Some(&realm.base_url())) else { return Ok(()); };
        let mut destination = parsed.href();
        if is_anchor {
            let original_target = ctx.with_instance::<DomEvent, _>(&event, |event| event.target_for_retarget())
                .map_err(|_| OpError::type_error("activation event is not an Event"))?;
            if let Some((image_realm, image)) = ctx.with_instance::<DomNode, _>(&original_target, |node| (node.realm.clone(), node.id)).ok() {
                let is_map = {
                    let session = image_realm.session.borrow();
                    let document = session.document();
                    lumen_html::forms::html_element_local_name(document, image) == Some("img") && document.get_attribute_ns_ref(image, None, "ismap").map_err(dom_error)?.is_some()
                };
                if is_map {
                    let (mut x, mut y) = (0, 0);
                    if trusted {
                        image_realm.flush_layout()?;
                        if let Some(geometry) = super::geometry::snapshot(&mut image_realm.session.borrow_mut(), image) {
                            x = (coordinates.0 as f32 - geometry.bounding_client_rect.x).max(0.0) as i32;
                            y = (coordinates.1 as f32 - geometry.bounding_client_rect.y).max(0.0) as i32;
                        }
                    }
                    destination.push_str(&format!("?{},{}", x, y));
                }
            }
        }
        let blank_target=target.eq_ignore_ascii_case("_blank");
        if let Some(target) = source.choose_navigation_target(ctx, &target)? {
            let mut referrer = realm.navigation_referrer();
            {
                let session = realm.session.borrow();
                let document = session.document();
                let rel=document.get_attribute_ns_ref(id,None,"rel").map_err(dom_error)?.unwrap_or("");
                let opener=rel.split_ascii_whitespace().any(|token|token.eq_ignore_ascii_case("opener"));
                let noopener=rel.split_ascii_whitespace().any(|token|matches!(token.to_ascii_lowercase().as_str(),"noopener"|"noreferrer"));
                if noopener || (blank_target && !opener) {target.disown_opener();}
                if let Some(policy) = document.get_attribute_ns_ref(id, None, "referrerpolicy").map_err(dom_error)?
                    .and_then(lumen_common::referrer::ReferrerPolicy::parse) { referrer.policy = policy; }
                if document.get_attribute_ns_ref(id, None, "rel").map_err(dom_error)?.is_some_and(|rel|
                    rel.split_ascii_whitespace().any(|token| token.eq_ignore_ascii_case("noreferrer"))) {
                    referrer.policy = lumen_common::referrer::ReferrerPolicy::NoReferrer;
                }
            }
            target.request_hyperlink_navigation(ctx, &destination, &realm, super::browsing_context::NavigationMetadata {
                policy_container: realm.policy_container(),
                cookie_source_url: realm.cookie_source_url(),
                referrer, inherited_referrer_policy: realm.referrer_policy.get(), source_element: Some(id), user_involvement: if trusted {
                    super::browsing_context::UserNavigationInvolvement::Activation
                } else { super::browsing_context::UserNavigationInvolvement::None },
            })?;
        }
        Ok(())
    })))
}

fn hyperlink_url(node: &DomNode) -> OpResult<Option<Url>> {
    let session = node.realm.session.borrow();
    let raw = session
        .document()
        .get_attribute_ns_ref(node.id, None, "href")
        .map_err(dom_error)?;
    Ok(raw.and_then(|href| url::parse(href, Some(&node.realm.base_url())).ok()))
}

#[cfg(test)]
mod activation_tests {
    use super::*;
    #[test]
    fn specification_window_hyperlink_activation_uses_dispatch_path_cancellation_and_live_document() {
        let mut engine = lumen::Engine::new();
        let realm = crate::install(engine.ctx(), "<!doctype html><base target=_self><a id=link href=#first><span id=child></span></a><div id=first></div><div id=second></div>", 256).unwrap();
        realm.set_document_url("https://links.test/path/page.html");
        let value = engine.eval_value(r#"(() => {
            const check=(value,message)=>{if(!value)throw new Error(message);};
            const link=document.getElementById('link'),child=document.getElementById('child');
            child.dispatchEvent(new MouseEvent('click',{bubbles:true,cancelable:true}));
            check(location.hash==='#first','ancestor activation uses actual dispatch path');
            link.onclick=event=>{link.href='#second';event.preventDefault();};
            child.click();check(location.hash==='#first','canceled click does not navigate');
            link.onclick=null;
            child.dispatchEvent(new Event('click',{bubbles:true}));
            check(location.hash==='#first','ordinary Event has no MouseEvent activation');
            link.removeAttribute('href');link.onclick=()=>{link.href='#second';};
            child.click();check(location.hash==='#second','activation reads href after listeners');
            link.onclick=null;link.remove();link.href='#first';link.click();
            check(location.hash==='#first','disconnected anchor still uses its fully active document');
            link.target='_blank';link.href='#second';link.click();
            check(location.hash==='#first','host configured to decline new auxiliary navigables');
            link.target='_self';link.download='file';link.click();
            check(location.hash==='#first','download does not become a document navigation');
            return true;
        })()"#).unwrap().unwrap_or_else(|exception| {
            let message=engine.ctx().member_get(&exception,"stack").ok().and_then(|value|engine.ctx().coerce_string(&value).ok()).map(|value|value.to_string()).unwrap_or_default();
            panic!("hyperlink guard: {message}");
        });
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn specification_window_hyperlink_ismap_captures_source_base_and_real_pending_navigation() {
        let mut engine=lumen::Engine::new();
        let realm=crate::install(engine.ctx(), "<!doctype html><base href='https://links.test/assets/'><a id=link href=map><img id=image ismap></a>",256).unwrap();
        realm.set_document_url("https://links.test/path/page.html");
        assert!(matches!(engine.eval_value("document.getElementById('image').click();true").unwrap().ok(),Some(Value::Bool(true))));
        let request=realm.navigation_context().unwrap().navigation_request();
        assert_eq!(request.source,super::super::browsing_context::FrameSource::Url("https://links.test/assets/map?0,0".into()));
        assert_eq!(request.base_url,"https://links.test/assets/");
    }
}

fn href(node: &DomNode) -> OpResult<String> {
    let session = node.realm.session.borrow();
    let Some(raw) = session
        .document()
        .get_attribute_ns_ref(node.id, None, "href")
        .map_err(dom_error)?
    else {
        return Ok(String::new());
    };
    Ok(url::parse(raw, Some(&node.realm.base_url()))
        .map(|url| url.href())
        .unwrap_or_else(|_| raw.to_owned()))
}

fn write_href(node: &DomNode, value: &str) -> OpResult<()> {
    node.realm
        .session
        .borrow_mut()
        .document_mut()
        .set_attribute_ns(node.id, None, "href", value)
        .map_err(dom_error)
}

fn update_url(node: &DomNode, change: impl FnOnce(&mut Url) -> bool) -> OpResult<()> {
    if let Some(mut url) = hyperlink_url(node)? {
        if change(&mut url) {
            write_href(node, &url.href())?;
        }
    }
    Ok(())
}

macro_rules! hyperlink_element {
    ($ty:ident, $name:literal, { $($members:tt)* }) => {
        #[lumen_bind::class(name = $name, extends = DomHtmlElement, hint(js(webidl)))]
        pub(crate) struct $ty {
            pub(crate) base: DomHtmlElement,
        }

        #[lumen_bind::methods]
        impl $ty {
            #[getter(name = "ping")]
            fn ping(&self) -> OpResult<String> { crate::html_interfaces::reflected_usv_value(&self.base.base.base, "ping") }
            #[setter(name = "ping", hint(js(ce_reactions)))]
            fn set_ping(&self, value: lumen_host::webidl::Usv) -> OpResult<()> { (&self.base.base.base).set_attribute_core("ping", &value.0) }
            #[getter(name = "hreflang")]
            fn hreflang(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("hreflang")?.unwrap_or_default()) }
            #[setter(name = "hreflang", coerce, hint(js(ce_reactions)))]
            fn set_hreflang(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("hreflang", value) }
            #[getter(name = "type")]
            fn kind(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("type")?.unwrap_or_default()) }
            #[setter(name = "type", coerce, hint(js(ce_reactions)))]
            fn set_kind(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("type", value) }
            $($members)*
            #[constructor]
            fn new(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<crate::custom_elements::HtmlElementCtor> {
                crate::custom_elements::construct_customized_class::<Self>(ctx, this.0)
            }
            #[getter]
            fn target(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("target")?.unwrap_or_default()) }
            #[setter(coerce, hint(js(ce_reactions)))]
            fn set_target(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("target", value) }
            #[getter]
            fn download(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("download")?.unwrap_or_default()) }
            #[setter(coerce, hint(js(ce_reactions)))]
            fn set_download(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("download", value) }
            #[getter]
            fn rel(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("rel")?.unwrap_or_default()) }
            #[setter(coerce, hint(js(ce_reactions)))]
            fn set_rel(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("rel", value) }
            #[getter(rename(js = "referrerPolicy"))]
            fn referrer_policy(&self) -> OpResult<String> {
                Ok(self.base.base.base.get_null_attribute("referrerpolicy")?
                    .and_then(|value| lumen_common::referrer::ReferrerPolicy::parse(&value))
                    .map_or(String::new(), |policy| policy.name().to_owned()))
            }
            #[setter(coerce, rename(js = "referrerPolicy"), hint(js(ce_reactions)))]
            fn set_referrer_policy(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("referrerpolicy", value) }
            #[getter]
            fn href(&self) -> OpResult<String> {
                href(&self.base.base.base)
            }
            #[setter(coerce, hint(js(ce_reactions)))]
            fn set_href(&self, value: &str) -> OpResult<()> {
                write_href(&self.base.base.base, value)
            }
            #[method(name = "toString")]
            fn to_string(&self) -> OpResult<String> {
                self.href()
            }
            #[getter]
            fn origin(&self) -> OpResult<String> {
                Ok(hyperlink_url(&self.base.base.base)?
                    .map(|url| url.origin())
                    .unwrap_or_default())
            }
            #[getter]
            fn protocol(&self) -> OpResult<String> {
                Ok(hyperlink_url(&self.base.base.base)?
                    .map(|url| format!("{}:", url.scheme))
                    .unwrap_or_else(|| ":".into()))
            }
            #[setter(coerce, hint(js(ce_reactions)))]
            fn set_protocol(&self, value: &str) -> OpResult<()> {
                update_url(&self.base.base.base, |url| url.set_protocol(value))
            }
            #[getter]
            fn username(&self) -> OpResult<String> {
                Ok(hyperlink_url(&self.base.base.base)?
                    .map(|url| url.username)
                    .unwrap_or_default())
            }
            #[setter(coerce, hint(js(ce_reactions)))]
            fn set_username(&self, value: &str) -> OpResult<()> {
                update_url(&self.base.base.base, |url| url.set_username(value))
            }
            #[getter]
            fn password(&self) -> OpResult<String> {
                Ok(hyperlink_url(&self.base.base.base)?
                    .map(|url| url.password)
                    .unwrap_or_default())
            }
            #[setter(coerce, hint(js(ce_reactions)))]
            fn set_password(&self, value: &str) -> OpResult<()> {
                update_url(&self.base.base.base, |url| url.set_password(value))
            }
            #[getter]
            fn host(&self) -> OpResult<String> {
                Ok(hyperlink_url(&self.base.base.base)?
                    .map(|url| {
                        let mut host = url.host.unwrap_or_default();
                        if let Some(port) = url.port {
                            host.push(':');
                            host.push_str(&port.to_string());
                        }
                        host
                    })
                    .unwrap_or_default())
            }
            #[setter(coerce, hint(js(ce_reactions)))]
            fn set_host(&self, value: &str) -> OpResult<()> {
                update_url(&self.base.base.base, |url| url.set_host(value))
            }
            #[getter]
            fn hostname(&self) -> OpResult<String> {
                Ok(hyperlink_url(&self.base.base.base)?
                    .and_then(|url| url.host)
                    .unwrap_or_default())
            }
            #[setter(coerce, hint(js(ce_reactions)))]
            fn set_hostname(&self, value: &str) -> OpResult<()> {
                update_url(&self.base.base.base, |url| url.set_hostname(value))
            }
            #[getter]
            fn port(&self) -> OpResult<String> {
                Ok(hyperlink_url(&self.base.base.base)?
                    .and_then(|url| url.port)
                    .map(|port| port.to_string())
                    .unwrap_or_default())
            }
            #[setter(coerce, hint(js(ce_reactions)))]
            fn set_port(&self, value: &str) -> OpResult<()> {
                update_url(&self.base.base.base, |url| url.set_port(value))
            }
            #[getter]
            fn pathname(&self) -> OpResult<String> {
                Ok(hyperlink_url(&self.base.base.base)?
                    .map(|url| url.path)
                    .unwrap_or_default())
            }
            #[setter(coerce, hint(js(ce_reactions)))]
            fn set_pathname(&self, value: &str) -> OpResult<()> {
                update_url(&self.base.base.base, |url| url.set_pathname(value))
            }
            #[getter]
            fn search(&self) -> OpResult<String> {
                Ok(hyperlink_url(&self.base.base.base)?
                    .and_then(|url| url.query)
                    .filter(|value| !value.is_empty())
                    .map(|value| format!("?{value}"))
                    .unwrap_or_default())
            }
            #[setter(coerce, hint(js(ce_reactions)))]
            fn set_search(&self, value: &str) -> OpResult<()> {
                update_url(&self.base.base.base, |url| {
                    url.set_search(value);
                    true
                })
            }
            #[getter]
            fn hash(&self) -> OpResult<String> {
                Ok(hyperlink_url(&self.base.base.base)?
                    .and_then(|url| url.fragment)
                    .filter(|value| !value.is_empty())
                    .map(|value| format!("#{value}"))
                    .unwrap_or_default())
            }
            #[setter(coerce, hint(js(ce_reactions)))]
            fn set_hash(&self, value: &str) -> OpResult<()> {
                update_url(&self.base.base.base, |url| {
                    url.set_hash(value);
                    true
                })
            }
        }
    };
}

hyperlink_element!(DomAnchorElement, "HTMLAnchorElement", {
            #[getter(name = "coords")]
            fn coords(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("coords")?.unwrap_or_default()) }
            #[setter(name = "coords", coerce, hint(js(ce_reactions)))]
            fn set_coords(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("coords", value) }
            #[getter(name = "charset")]
            fn charset(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("charset")?.unwrap_or_default()) }
            #[setter(name = "charset", coerce, hint(js(ce_reactions)))]
            fn set_charset(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("charset", value) }
            #[getter(name = "name")]
            fn name(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("name")?.unwrap_or_default()) }
            #[setter(name = "name", coerce, hint(js(ce_reactions)))]
            fn set_name(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("name", value) }
            #[getter(name = "rev")]
            fn rev(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("rev")?.unwrap_or_default()) }
            #[setter(name = "rev", coerce, hint(js(ce_reactions)))]
            fn set_rev(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("rev", value) }
            #[getter(name = "shape")]
            fn shape(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("shape")?.unwrap_or_default()) }
            #[setter(name = "shape", coerce, hint(js(ce_reactions)))]
            fn set_shape(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("shape", value) }
});
hyperlink_element!(DomAreaElement, "HTMLAreaElement", {
            #[getter(name = "alt")]
            fn alt(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("alt")?.unwrap_or_default()) }
            #[setter(name = "alt", coerce, hint(js(ce_reactions)))]
            fn set_alt(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("alt", value) }
            #[getter(name = "coords")]
            fn coords(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("coords")?.unwrap_or_default()) }
            #[setter(name = "coords", coerce, hint(js(ce_reactions)))]
            fn set_coords(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("coords", value) }
            #[getter(name = "shape")]
            fn shape(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("shape")?.unwrap_or_default()) }
            #[setter(name = "shape", coerce, hint(js(ce_reactions)))]
            fn set_shape(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("shape", value) }
            #[getter(name = "noHref")]
            fn no_href(&self) -> OpResult<bool> { Ok((&self.base.base.base).get_null_attribute("nohref")?.is_some()) }
            #[setter(name = "noHref", coerce, hint(js(ce_reactions)))]
            fn set_no_href(&self, value: bool) -> OpResult<()> { (&self.base.base.base).set_nullable_attribute_core("nohref", value.then_some("")) }
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_location_shares_the_host_window_address_and_base_missing_href_fallback() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        let realm = install(engine.ctx(), "<base><main></main>", 64).unwrap();
        realm.set_document_url("https://origin.test/dir/page.html");
        let result = engine.eval_value(r#"(() => {
            const base=document.querySelector('base');
            return document.location===window.location && document.location.href==='https://origin.test/dir/page.html' &&
                base.href===document.location.href && !base.hasAttribute('href') &&
                document.implementation.createHTMLDocument('detached').location===null;
        })()"#).unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn hyperlink_reflection_uses_live_bases_and_shared_url_setters() {
        let mut engine = lumen::Engine::new();
        let realm = install(engine.ctx(), "<base href='https://example.test/a/'><a href='../page?q=1#old'>link</a><area href='child'>", 128).unwrap();
        realm.set_document_url("https://origin.test/root/index");
        let result = engine.eval_value(r#"(() => {
            const a = document.querySelector('a'), area = document.querySelector('area');
            if (!(a instanceof HTMLAnchorElement) || !(area instanceof HTMLAreaElement) || a.href !== 'https://example.test/page?q=1#old' || String(a) !== a.href || area.href !== 'https://example.test/a/child') return false;
            a.setAttributeNS('https://attributes.test/', 'href', 'https://wrong.test/');
            if (a.href !== 'https://example.test/page?q=1#old') return false;
            document.querySelector('base').href = 'https://changed.test/b/';
            if (a.href !== 'https://changed.test/page?q=1#old') return false;
            a.port = '443'; a.username = 'a b'; a.password = 'c d'; a.pathname = '/two words'; a.search = '?new=2'; a.hash = '#new value';
            if (a.port !== '' || a.username !== 'a%20b' || a.password !== 'c%20d' || a.pathname !== '/two%20words' || a.search !== '?new=2' || a.hash !== '#new%20value' || a.origin !== 'https://changed.test') return false;
            const blank = document.createElement('a');
            if (blank.href !== '' || blank.protocol !== ':' || blank.origin !== '') return false;
            blank.pathname = '/ignored';
            if (blank.hasAttribute('href')) return false;
            blank.href = 'http://[bad';
            return blank.href === 'http://[bad' && blank.hostname === '';
        })()"#).unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }
}
