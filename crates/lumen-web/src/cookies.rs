//! Native browser cookie storage; parsing and policy stay in lumen-common.
use lumen_common::cookies::{Context, CookieJar};
use lumen_common::url::Url;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct BrowserCookies(Arc<Mutex<CookieJar>>);

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs().min(i64::MAX as u64) as i64)
}

impl BrowserCookies {
    pub fn delete_associated(&self, url: &Url, context: &Context) {
        self.0.lock().unwrap_or_else(|error| error.into_inner()).delete_associated(url, context, now());
    }

    pub fn read(&self, url: &Url, context: &Context, http: bool) -> String {
        self.0.lock().unwrap_or_else(|error| error.into_inner()).header(url, http, context, now())
    }

    pub fn write(&self, url: &Url, assignment: &str, context: &Context, http: bool) -> bool {
        self.0.lock().unwrap_or_else(|error| error.into_inner()).store(url, assignment, http, context, now())
    }
}
