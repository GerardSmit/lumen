//! Typed document.cookie adapter. The host owns the shared HTTP cookie jar.
use crate::*;

pub trait CookieHost {
    fn read(&self, document_url: &str) -> String;
    fn write(&self, document_url: &str, assignment: &str);
    /// Privileged browser automation; never exposed by document.cookie.
    fn delete_associated(&self, _document_url: &str) -> OpResult<()> {
        Err(OpError::new("NotSupportedError", "cookie host has no automation deletion operation"))
    }
}

impl DomRealm {
    pub(crate) fn delete_associated_cookies(&self) -> OpResult<()> {
        let Some(url) = cookie_url(self)? else { return Ok(()); };
        let host = self.cookie_host.borrow().clone().ok_or_else(||
            OpError::new("NotSupportedError", "browser cookie host is absent"))?;
        host.delete_associated(&url)
    }

    pub(crate) fn cookie_source_url(&self) -> Option<String> { cookie_url(self).ok().flatten() }
    pub(crate) fn inherit_cookie_environment(&self, source: &DomRealm) {
        *self.cookie_source_url.borrow_mut() = source.cookie_source_url();
        *self.cookie_host.borrow_mut() = source.cookie_host.borrow().clone();
    }
    pub fn set_cookie_host(&self, host: Rc<dyn CookieHost>) {
        *self.cookie_host.borrow_mut() = Some(host);
    }
}

impl DomDocument {
    pub(crate) fn cookie_value(&self) -> OpResult<String> {
        if !self.realm.has_browsing_context {
            return Ok(String::new());
        }
        let Some(url) = cookie_url(&self.realm)? else {
            return Ok(String::new());
        };
        let host = self.realm.cookie_host.borrow();
        match host.as_ref() {
            Some(host) => Ok(host.read(&url)),
            None => Err(OpError::new(
                "NotSupportedError",
                "document cookies require a browser cookie host",
            )),
        }
    }

    pub(crate) fn set_cookie_value(&self, assignment: &str) -> OpResult<()> {
        if !self.realm.has_browsing_context {
            return Ok(());
        }
        let Some(url) = cookie_url(&self.realm)? else {
            return Ok(());
        };
        let host = self.realm.cookie_host.borrow();
        match host.as_ref() {
            Some(host) => {
                host.write(&url, assignment);
                Ok(())
            }
            None => Err(OpError::new(
                "NotSupportedError",
                "document cookies require a browser cookie host",
            )),
        }
    }
}

fn cookie_url(realm: &DomRealm) -> OpResult<Option<String>> {
    let origin = realm.document_origin().or_else(|| realm.browsing_context().map(|context| context.root_or_child_origin()));
    let mut url = realm.document_url().unwrap_or_else(|| "about:blank".into());
    let initial = lumen_common::url::parse(&url, None).ok();
    let initial_about = initial.as_ref().is_some_and(|parsed| parsed.scheme == "about" && matches!(parsed.path.as_str(), "blank" | "srcdoc"));
    let network_scheme = initial.as_ref().is_some_and(|parsed| matches!(parsed.scheme.as_str(), "http" | "https"));
    // Documents that are neither network-scheme nor initial about: are cookie-averse
    // and never reach the opaque-origin check.
    if initial.is_some() && !initial_about && !network_scheme {
        return Ok(None);
    }
    if matches!(origin, Some(browsing_context::Origin::Opaque(_))) {
        return Err(OpError::new("SecurityError", "document has an opaque cookie origin"));
    }
    let source = if initial_about { realm.cookie_source_url.borrow().clone() } else { None };
    let source = source.filter(|source| origin.as_ref().is_some_and(|origin| origin.same_origin(&browsing_context::Origin::from_url(source))));
    let parsed = match source {
        Some(source) => {
            url = source;
            lumen_common::url::parse(&url, None).ok()
        }
        None => initial,
    }
    .ok_or_else(|| OpError::new("SecurityError", "document has no cookie origin"))?;
    if !matches!(parsed.scheme.as_str(), "http" | "https") {
        return Ok(None);
    }
    if parsed.origin() == "null" {
        Err(OpError::new(
            "SecurityError",
            "document has an opaque cookie origin",
        ))
    } else {
        Ok(Some(url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct ForbiddenHost;
    impl CookieHost for ForbiddenHost {
        fn read(&self, _: &str) -> String { panic!("opaque document reached cookie jar") }
        fn write(&self, _: &str, _: &str) { panic!("opaque document reached cookie jar") }
    }

    #[test]
    fn opaque_document_cookies_reject_before_host_access() {
        let mut engine = lumen::Engine::new();
        let realm = crate::install(engine.ctx(), "<body></body>", 64).unwrap();
        realm.set_document_url("https://example.test/");
        realm.set_document_origin(browsing_context::Origin::opaque());
        realm.set_cookie_host(Rc::new(ForbiddenHost));
        let value = engine.eval_value("let failures=0;try{document.cookie}catch(e){if(e.name==='SecurityError')failures++}try{document.cookie='secret=1'}catch(e){if(e.name==='SecurityError')failures++}failures===2").unwrap();
        assert!(matches!(value, Ok(Value::Bool(true))));
    }
}
