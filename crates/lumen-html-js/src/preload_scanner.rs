//! Speculative preload scan of the navigation source. Stylesheet fetches start
//! through the installed provider as soon as the source is known; the real
//! `<link>` adopts the in-flight ticket when its request is identical. Other
//! subresources are exposed as hints for hosts which own those fetchers.
use super::*;
use crate::stylesheet_loading::{StylesheetLinks,StylesheetRequest,StylesheetResourceLoader};
use lumen_html::preload_scan::{self, ScannedTag};
use std::sync::Arc;

const MAX_SCANNED_TAGS: usize = 1024;
const MAX_SPECULATIVE_STYLESHEETS: usize = 64;
const MAX_HINTS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreloadKind {
    ClassicScript,
    ModuleScript,
    Image,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreloadHint {
    pub url: String,
    pub kind: PreloadKind,
    pub crossorigin: Option<bool>,
    pub nonce: String,
    pub integrity: String,
}

#[derive(Default)]
pub(crate) struct SpeculativeState {
    pub(crate) stylesheets: RefCell<Vec<(StylesheetRequest, u64)>>,
    pub(crate) hints: RefCell<Vec<PreloadHint>>,
}

fn same_request(speculative: &StylesheetRequest, real: &StylesheetRequest) -> bool {
    speculative.inline_source.is_none()
        && real.inline_source.is_none()
        && !real.import_request
        && speculative.url == real.url
        && speculative.document_url == real.document_url
        && speculative.referrer == real.referrer
        && speculative.origin == real.origin
        && speculative.nonce == real.nonce
        && speculative.integrity == real.integrity
        && speculative.crossorigin == real.crossorigin
        && speculative.referrer_policy == real.referrer_policy
        && speculative.environment_encoding == real.environment_encoding
        && speculative.quirks_mode == real.quirks_mode
        && Arc::ptr_eq(&speculative.policies, &real.policies)
}

fn fetchable(url: &lumen_common::url::Url) -> bool {
    !matches!(url.scheme.as_str(), "data" | "javascript" | "about" | "blob")
}

fn attribute<'a>(tag: &'a ScannedTag, name: &str) -> &'a str {
    tag.attribute(name).unwrap_or("")
}

fn crossorigin(tag: &ScannedTag) -> Option<bool> {
    tag.attribute("crossorigin").map(|value| value.eq_ignore_ascii_case("use-credentials"))
}

impl StylesheetLinks {
    pub(crate) fn take_speculative(&self, request: &StylesheetRequest) -> Option<u64> {
        let mut list = self.speculative.stylesheets.borrow_mut();
        let index = list.iter().position(|(speculative, _)| same_request(speculative, request))?;
        Some(list.remove(index).1)
    }

    pub(crate) fn cancel_speculative(&self, provider: &dyn StylesheetResourceLoader) {
        for (_, ticket) in self.speculative.stylesheets.borrow_mut().drain(..) {
            provider.cancel(ticket);
        }
    }
}

impl DomRealm {
    /// Hints for scripts and images discovered by the preload scan, scripts first.
    pub fn take_preload_hints(&self) -> Vec<PreloadHint> {
        core::mem::take(&mut *self.stylesheet_links.speculative.hints.borrow_mut())
    }

    pub(crate) fn run_preload_scan(self: &Rc<Self>, source: &str) {
        let Some(provider) = self.stylesheet_links.provider.borrow().clone() else { return };
        let Ok(policies) = self.module_fetch_policy_snapshot() else { return };
        let Some(document_url) = self.document_url() else { return };
        let Some(parsed_document_url) = lumen_common::url::parse_url(&document_url, None) else { return };
        let scan = preload_scan::scan(source, MAX_SCANNED_TAGS);
        let mut base = lumen_common::url::parse_url(&self.base_url(), None).unwrap_or_else(|| parsed_document_url.clone());
        let mut base_seen = false;
        let referrer = self.script_fetch_referrer().source;
        let origin = self.script_fetch_origin();
        let (media_environment, mut hints) = (self.session.borrow().media_environment(), Vec::new());
        let mut started = 0;
        for tag in &scan.tags {
            if tag.name == "base" {
                if !base_seen && tag.attribute("href").is_some() {
                    base_seen = true;
                    if let Some(url) = lumen_common::url::parse_url(attribute(tag, "href"), Some(&parsed_document_url)) {
                        base = url;
                    }
                }
                continue;
            }
            let href = if tag.name == "link" { attribute(tag, "href") } else { attribute(tag, "src") };
            if href.is_empty() {
                continue;
            }
            let Some(url) = lumen_common::url::parse_url(href, Some(&base)).filter(fetchable) else { continue };
            let url = url.href();
            let nonce = attribute(tag, "nonce").to_owned();
            let integrity = attribute(tag, "integrity").to_owned();
            match tag.name {
                "link" => {
                    let rel = attribute(tag, "rel");
                    let has = |token: &str| rel.split_ascii_whitespace().any(|value| value.eq_ignore_ascii_case(token));
                    let destination = attribute(tag, "as");
                    if has("modulepreload") {
                        hints.push(PreloadHint { url, kind: PreloadKind::ModuleScript, crossorigin: crossorigin(tag), nonce, integrity });
                        continue;
                    }
                    if has("preload") && destination.eq_ignore_ascii_case("script") {
                        hints.push(PreloadHint { url, kind: PreloadKind::ClassicScript, crossorigin: crossorigin(tag), nonce, integrity });
                        continue;
                    }
                    let stylesheet = (has("stylesheet") && !has("alternate")) || (has("preload") && destination.eq_ignore_ascii_case("style"));
                    if !stylesheet
                        || tag.attribute("disabled").is_some()
                        || !lumen_common::mime::stylesheet_hint_supported(attribute(tag, "type"))
                        || !lumen_html::css::media_query_matches(attribute(tag, "media"), media_environment)
                        || started >= MAX_SPECULATIVE_STYLESHEETS
                    {
                        continue;
                    }
                    let request = StylesheetRequest {
                        inline_source: None,
                        import_request: false,
                        url,
                        document_url: document_url.clone(),
                        referrer: referrer.clone(),
                        origin: origin.clone(),
                        nonce,
                        parser_inserted: true,
                        integrity,
                        crossorigin: crossorigin(tag),
                        referrer_policy: lumen_common::referrer::ReferrerPolicy::parse(attribute(tag, "referrerpolicy"))
                            .unwrap_or(self.referrer_policy.get()),
                        environment_encoding: lumen_common::encoding::canonical_document_label(attribute(tag, "charset"))
                            .unwrap_or(self.document_encoding()),
                        quirks_mode: !scan.has_doctype,
                        policies: policies.clone(),
                        start_time: lumen_host::perf::web_now_ms(),
                    };
                    let Ok(ticket) = provider.start(request.clone()) else { break };
                    started += 1;
                    self.stylesheet_links.speculative.stylesheets.borrow_mut().push((request, ticket));
                }
                "script" => {
                    let kind = script_loading::type_from_attributes(|name| tag.attribute(name).map(str::to_owned));
                    let kind = match kind {
                        script_loading::DeclaredScriptType::Classic => PreloadKind::ClassicScript,
                        script_loading::DeclaredScriptType::Module => PreloadKind::ModuleScript,
                        _ => continue,
                    };
                    let crossorigin = if kind == PreloadKind::ModuleScript && tag.attribute("crossorigin").is_none() {
                        Some(false)
                    } else {
                        crossorigin(tag)
                    };
                    hints.push(PreloadHint { url, kind, crossorigin, nonce, integrity });
                }
                _ => {
                    if !attribute(tag, "loading").eq_ignore_ascii_case("lazy") {
                        hints.push(PreloadHint { url, kind: PreloadKind::Image, crossorigin: crossorigin(tag), nonce, integrity });
                    }
                }
            }
        }
        hints.truncate(MAX_HINTS);
        hints.sort_by_key(|hint| hint.kind == PreloadKind::Image);
        *self.stylesheet_links.speculative.hints.borrow_mut() = hints;
    }
}
