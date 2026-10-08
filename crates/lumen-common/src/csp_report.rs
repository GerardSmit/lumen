//! CSP report destinations and the legacy report schema, independent of engine/network state.
//! Policy tokenization, URL resolution and JSON encoding use the existing shared implementations.
use alloc::{string::String, vec::Vec};
use crate::csp::{Error, PolicyDisposition, PolicySource, Violation};

pub const MAX_REPORT_BYTES: usize = 65_536;
pub const MAX_REPORT_DESTINATIONS: usize = 16;

#[derive(Debug, PartialEq, Eq)]
pub enum Destinations {
    Legacy(Vec<String>),
    /// Presence suppresses report-uri even when this group has no registered endpoint.
    Reporting(String),
}

pub fn destinations(violation: &Violation, document_url: &str) -> Result<Destinations, Error> {
    if violation.original_policy.len() > MAX_REPORT_BYTES { return Err(Error::Capacity); }
    let policy = content_security_policy::Policy::parse(&violation.original_policy,
        PolicySource::Header, if violation.report_only { PolicyDisposition::Report } else { PolicyDisposition::Enforce });
    if let Some(directive) = policy.directive_set.iter().find(|directive| directive.name == "report-to") {
        return Ok(Destinations::Reporting(directive.value.first().cloned().unwrap_or_default()));
    }
    let mut endpoints = Vec::new();
    let Some(directive) = policy.directive_set.iter().find(|directive| directive.name == "report-uri") else {
        return Ok(Destinations::Legacy(endpoints));
    };
    if directive.value.len() > MAX_REPORT_DESTINATIONS { return Err(Error::Capacity); }
    // The document URL is the CSP base; an author <base> element cannot redirect reports.
    let base = crate::url::parse_url(document_url, None).ok_or(Error::InvalidUrl)?;
    for value in &directive.value {
        let Some(mut url) = crate::url::parse_url(value, Some(&base)) else { continue };
        if !matches!(url.scheme.as_str(), "http" | "https") || !url.username.is_empty() || !url.password.is_empty() { continue; }
        url.fragment = None;
        let endpoint = url.href();
        endpoints.push(endpoint);
    }
    Ok(Destinations::Legacy(endpoints))
}

pub struct Context<'a> {
    pub document_url: &'a str,
    pub referrer: &'a str,
    pub source_file: Option<&'a str>,
    pub status_code: u16,
    pub line_number: u32,
    pub column_number: u32,
}

pub fn reporting_url(value: &str) -> String {
    let Some(mut url) = crate::url::parse_url(value, None) else { return value.into() };
    if !matches!(url.scheme.as_str(), "http" | "https") { return url.scheme; }
    url.username.clear(); url.password.clear(); url.fragment = None;
    url.href()
}

/// Admit report metadata before allocating either report representation.
pub fn validate_context(violation:&Violation,context:&Context<'_>)->Result<(),Error> {
    let input_bytes = violation.original_policy.len().saturating_add(violation.blocked_uri.len())
        .saturating_add(violation.sample.len()).saturating_add(context.document_url.len())
        .saturating_add(context.referrer.len()).saturating_add(context.source_file.map_or(0, str::len));
    if input_bytes > MAX_REPORT_BYTES { return Err(Error::Capacity); }
    Ok(())
}
/// Serialize the standards-defined legacy envelope using the maintained JSON codec.
pub fn legacy_body(violation: &Violation, context: &Context<'_>) -> Result<Vec<u8>, Error> {
    validate_context(violation,context)?;
    let mut report = serde_json::json!({
        "document-uri": reporting_url(context.document_url),
        "referrer": reporting_url(context.referrer),
        "blocked-uri": reporting_url(&violation.blocked_uri),
        "effective-directive": violation.directive,
        "violated-directive": violation.directive,
        "original-policy": violation.original_policy,
        "disposition": if violation.report_only { "report" } else { "enforce" },
        "status-code": context.status_code,
        "script-sample": violation.sample,
    });
    if let Some(source) = context.source_file {
        report["source-file"] = reporting_url(source).into();
        report["line-number"] = context.line_number.into();
        report["column-number"] = context.column_number.into();
    }
    let bytes = serde_json::to_vec(&serde_json::json!({"csp-report": report})).map_err(|_| Error::Capacity)?;
    if bytes.len() > MAX_REPORT_BYTES { return Err(Error::Capacity); }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn violation(policy: &str) -> Violation {
        Violation {directive:"script-src-elem".into(), original_policy:policy.into(),
            blocked_uri:"inline".into(), report_only:true, sample:"ä\"\n".into()}
    }
    #[test]
    fn report_destinations_use_document_url_and_report_to_suppresses_legacy() {
        let policy = violation("script-src 'none'; report-uri ../report#fragment /other");
        assert_eq!(destinations(&policy,"https://example.test/path/document").unwrap(),
            Destinations::Legacy(alloc::vec!["https://example.test/report".into(),"https://example.test/other".into()]));
        assert_eq!(destinations(&violation("report-uri /legacy; report-to reports"),"https://example.test/page").unwrap(),
            Destinations::Reporting("reports".into()));
        assert_eq!(destinations(&violation("report-uri data:text/plain,x https://u:p@example.test/report"),"https://example.test/page").unwrap(),Destinations::Legacy(Vec::new()));
    }
    #[test]
    fn legacy_report_round_trips_unicode_and_optional_source_coordinates() {
        let violation=violation("script-src 'none'; report-uri /report");
        let context=Context {document_url:"https://u:p@example.test/page#private",referrer:"",source_file:Some("https://example.test/script#fragment"),status_code:200,line_number:3,column_number:7};
        let body=legacy_body(&violation,&context).unwrap();
        let value:serde_json::Value=serde_json::from_slice(&body).unwrap();
        let report=&value["csp-report"];
        assert_eq!(report["document-uri"],"https://example.test/page");
        assert_eq!(report["source-file"],"https://example.test/script");
        assert_eq!(report["script-sample"],violation.sample);
        assert_eq!(report["disposition"],"report");
        assert_eq!(report["violated-directive"],"script-src-elem");
        assert_eq!(report["column-number"],7);
    }
}
