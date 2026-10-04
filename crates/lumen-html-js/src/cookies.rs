//! Typed document.cookie adapter. The host owns the shared HTTP cookie jar.
use crate::*;

pub trait CookieHost {
    fn read(&self, document_url: &str) -> String;
    fn write(&self, document_url: &str, assignment: &str);
}

impl DomRealm {
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
    let url = realm.document_url().unwrap_or_else(|| "about:blank".into());
    let parsed = lumen_common::url::parse(&url, None)
        .map_err(|_| OpError::new("SecurityError", "document has no cookie origin"))?;
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
