//! Shared Referrer Policy request and redirect processing over URL records.
use crate::url::{self, Url};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReferrerPolicy {
    NoReferrer,
    NoReferrerWhenDowngrade,
    SameOrigin,
    Origin,
    StrictOrigin,
    OriginWhenCrossOrigin,
    #[default]
    StrictOriginWhenCrossOrigin,
    UnsafeUrl,
}

impl ReferrerPolicy {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "no-referrer" => Some(Self::NoReferrer),
            "no-referrer-when-downgrade" => Some(Self::NoReferrerWhenDowngrade),
            "same-origin" => Some(Self::SameOrigin),
            "origin" => Some(Self::Origin),
            "strict-origin" => Some(Self::StrictOrigin),
            "origin-when-cross-origin" => Some(Self::OriginWhenCrossOrigin),
            "strict-origin-when-cross-origin" => Some(Self::StrictOriginWhenCrossOrigin),
            "unsafe-url" => Some(Self::UnsafeUrl),
            _ => None,
        }
    }

    /// Header lists use the last recognized comma-separated token; content
    /// attributes use `parse` instead and do not accept a policy list.
    pub fn parse_header(value: &str) -> Option<Self> {
        value.split(',').filter_map(|token| Self::parse(token.trim_matches(|c| matches!(c, ' ' | '\t')))).last()
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::NoReferrer => "no-referrer",
            Self::NoReferrerWhenDowngrade => "no-referrer-when-downgrade",
            Self::SameOrigin => "same-origin",
            Self::Origin => "origin",
            Self::StrictOrigin => "strict-origin",
            Self::OriginWhenCrossOrigin => "origin-when-cross-origin",
            Self::StrictOriginWhenCrossOrigin => "strict-origin-when-cross-origin",
            Self::UnsafeUrl => "unsafe-url",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Referrer {
    pub source: String,
    pub policy: ReferrerPolicy,
}

impl Referrer {
    pub fn for_url(&self, destination: &Url) -> Option<String> {
        let mut source = url::parse(&self.source, None).ok()?;
        if matches!(source.scheme.as_str(), "about" | "blob" | "data") { return None; }
        source.username.clear();
        source.password.clear();
        source.fragment = None;
        let mut origin = source.clone();
        origin.path = "/".into();
        origin.query = None;
        let origin = origin.href();
        let full = source.href();
        let full = if full.len() > 4096 { origin.clone() } else { full };
        let same_origin = source.origin() != "null" && source.origin() == destination.origin();
        let downgrade = source.is_potentially_trustworthy() && !destination.is_potentially_trustworthy();
        match self.policy {
            ReferrerPolicy::NoReferrer => None,
            ReferrerPolicy::Origin => Some(origin),
            ReferrerPolicy::UnsafeUrl => Some(full),
            ReferrerPolicy::SameOrigin => same_origin.then_some(full),
            ReferrerPolicy::NoReferrerWhenDowngrade => (!downgrade).then_some(full),
            ReferrerPolicy::StrictOrigin => (!downgrade).then_some(origin),
            ReferrerPolicy::OriginWhenCrossOrigin => Some(if same_origin { full } else { origin }),
            ReferrerPolicy::StrictOriginWhenCrossOrigin => if same_origin { Some(full) } else { (!downgrade).then_some(origin) },
        }
    }

    pub fn apply_redirect_policy(&mut self, headers: &[(String,String)]) {
        for (name,value) in headers {
            if name.eq_ignore_ascii_case("referrer-policy") {
                if let Some(policy) = ReferrerPolicy::parse_header(value) { self.policy = policy; }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_window_referrer_policy_strips_credentials_and_handles_origins_downgrades_and_redirects() {
        let mut request=Referrer {source:"https://user:secret@example.test:443/path?q=1#private".into(),policy:ReferrerPolicy::default()};
        let same=url::parse("https://example.test/other",None).unwrap();
        let foreign=url::parse("https://foreign.test/",None).unwrap();
        let insecure=url::parse("http://foreign.test/",None).unwrap();
        assert_eq!(request.for_url(&same).as_deref(),Some("https://example.test/path?q=1"));
        assert_eq!(request.for_url(&foreign).as_deref(),Some("https://example.test/"));
        assert_eq!(request.for_url(&insecure),None);
        request.apply_redirect_policy(&[("Referrer-Policy".into(),"unknown, unsafe-url".into())]);
        assert_eq!(request.for_url(&insecure).as_deref(),Some("https://example.test/path?q=1"));
        request.apply_redirect_policy(&[("referrer-policy".into(),"origin, no-referrer".into())]);
        assert_eq!(request.for_url(&same),None);
        assert!(ReferrerPolicy::parse("origin, no-referrer").is_none());
        request.source="about:srcdoc".into();request.policy=ReferrerPolicy::UnsafeUrl;
        assert_eq!(request.for_url(&same),None);
        request.source=format!("https://example.test/{}","x".repeat(4100));
        assert_eq!(request.for_url(&same).as_deref(),Some("https://example.test/"));
    }
}
