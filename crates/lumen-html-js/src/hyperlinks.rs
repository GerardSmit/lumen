//! Hyperlink URL reflection uses the shared URL parser and the live document base.
use super::*;
use lumen_common::url::{self, Url};

fn hyperlink_url(node: &DomNode) -> OpResult<Option<Url>> {
    let session = node.realm.session.borrow();
    let raw = session
        .document()
        .get_attribute_ns_ref(node.id, None, "href")
        .map_err(dom_error)?;
    Ok(raw.and_then(|href| url::parse(href, Some(&node.realm.base_url())).ok()))
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
    ($ty:ident, $name:literal) => {
        #[lumen_bind::class(name = $name, extends = DomHtmlElement, hint(js(webidl)))]
        pub(crate) struct $ty {
            pub(crate) base: DomHtmlElement,
        }

        #[lumen_bind::methods]
        impl $ty {
            #[getter]
            fn href(&self) -> OpResult<String> {
                href(&self.base.base.base)
            }
            #[setter(coerce)]
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
            #[setter(coerce)]
            fn set_protocol(&self, value: &str) -> OpResult<()> {
                update_url(&self.base.base.base, |url| url.set_protocol(value))
            }
            #[getter]
            fn username(&self) -> OpResult<String> {
                Ok(hyperlink_url(&self.base.base.base)?
                    .map(|url| url.username)
                    .unwrap_or_default())
            }
            #[setter(coerce)]
            fn set_username(&self, value: &str) -> OpResult<()> {
                update_url(&self.base.base.base, |url| url.set_username(value))
            }
            #[getter]
            fn password(&self) -> OpResult<String> {
                Ok(hyperlink_url(&self.base.base.base)?
                    .map(|url| url.password)
                    .unwrap_or_default())
            }
            #[setter(coerce)]
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
            #[setter(coerce)]
            fn set_host(&self, value: &str) -> OpResult<()> {
                update_url(&self.base.base.base, |url| url.set_host(value))
            }
            #[getter]
            fn hostname(&self) -> OpResult<String> {
                Ok(hyperlink_url(&self.base.base.base)?
                    .and_then(|url| url.host)
                    .unwrap_or_default())
            }
            #[setter(coerce)]
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
            #[setter(coerce)]
            fn set_port(&self, value: &str) -> OpResult<()> {
                update_url(&self.base.base.base, |url| url.set_port(value))
            }
            #[getter]
            fn pathname(&self) -> OpResult<String> {
                Ok(hyperlink_url(&self.base.base.base)?
                    .map(|url| url.path)
                    .unwrap_or_default())
            }
            #[setter(coerce)]
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
            #[setter(coerce)]
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
            #[setter(coerce)]
            fn set_hash(&self, value: &str) -> OpResult<()> {
                update_url(&self.base.base.base, |url| {
                    url.set_hash(value);
                    true
                })
            }
        }
    };
}

hyperlink_element!(DomAnchorElement, "HTMLAnchorElement");
hyperlink_element!(DomAreaElement, "HTMLAreaElement");

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
