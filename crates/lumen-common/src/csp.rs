//! Bounded policy containers over the maintained CSP implementation. No host or engine calls.
use content_security_policy as csp;
use alloc::{string::String, vec::Vec};
pub use csp::{Destination, PolicyDisposition, PolicySource, InlineCheckType};
const MAX_BYTES: usize = 65_536;
const MAX_POLICIES: usize = 64;
const MAX_TOKENS: usize = 4096;
#[derive(Clone, Default)]
pub struct PolicySet { policies: Vec<(csp::Policy, String)>, self_urls: Vec<Option<csp::Url>>, bytes: usize }
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Violation { pub directive: String, pub original_policy: String, pub blocked_uri: String, pub report_only: bool, pub sample: String }
#[derive(Clone, Debug)]
pub struct Decision { pub blocked: bool, pub violations: Vec<Violation> }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error { Capacity, InvalidUrl }
impl PolicySet {
    /// Bind only newly received policies. Inherited policies retain their
    /// protected resource's self-origin when the new document is opaque.
    pub fn bind_unbound_self_origin(&mut self, url: &str) -> Result<(), Error> {
        if self.self_urls.iter().all(Option::is_some) { return Ok(()); }
        let parsed = csp::Url::parse(url).map_err(|_| Error::InvalidUrl)?;
        let origin = parsed.origin().ascii_serialization();
        if origin.len() > 8192 { return Err(Error::Capacity); }
        let url = csp::Url::parse(if origin == "null" { "about:blank" } else { &origin }).map_err(|_| Error::InvalidUrl)?;
        let retained = self.self_urls.iter().flatten().map(|url|url.as_str().len()).sum::<usize>();
        let added = self.self_urls.iter().filter(|origin|origin.is_none()).count().saturating_mul(url.as_str().len());
        if retained.saturating_add(added) > MAX_BYTES { return Err(Error::Capacity); }
        for origin in &mut self.self_urls { if origin.is_none() { *origin = Some(url.clone()); } }
        Ok(())
    }
    pub fn has_unbound_self_origin(&self) -> bool { self.self_urls.iter().any(Option::is_none) }
    pub fn retained_bytes(&self) -> usize {
        self.bytes.saturating_add(self.self_urls.iter().flatten().map(|url|url.as_str().len()).sum::<usize>())
    }
    pub fn has_header_policy(&self)->bool {self.policies.iter().any(|(policy,_)|policy.source==PolicySource::Header)}
    fn native_list(&self) -> csp::CspList {
        csp::CspList(self.policies.iter().map(|(policy, _)| policy.clone()).collect())
    }
    /// Native string timers currently accept DOMString, with no fabricated
    /// TrustedScript brand/default policy. The canonical CSP sink check still
    /// enforces/report-only observes every require-trusted-types-for policy.
    pub fn check_timer_string(&self, source: &str, sink: &str) -> Decision {
        let list = self.native_list();
        let (allowed, violations) = list.should_sink_type_mismatch_violation_be_blocked_by_csp(sink, "'script'", "");
        Decision { blocked: allowed == csp::CheckResult::Blocked,
            violations: violations.into_iter().map(|violation| Violation {
                directive: "require-trusted-types-for".into(), original_policy: violation.policy.to_string(),
                blocked_uri: "trusted-types-sink".into(), report_only: violation.policy.disposition == PolicyDisposition::Report,
                sample: alloc::format!("{sink}|{}", source.chars().take(40).collect::<String>()),
            }).collect() }
    }
    /// Reuse the maintained CSP compilation decision. Samples are reconstructed
    /// on Unicode boundaries, avoiding the dependency's byte-truncation path.
    pub fn check_string_compilation(&self, source: &str) -> Decision {
        let (allowed, violations) = self.native_list().is_js_evaluation_allowed("");
        Decision { blocked: allowed == csp::CheckResult::Blocked,
            violations: violations.into_iter().map(|violation| Violation {
                directive: "script-src".into(), original_policy: violation.policy.to_string(), blocked_uri: "eval".into(),
                report_only: violation.policy.disposition == PolicyDisposition::Report,
                sample: if violation.directive.value.iter().any(|token| token == "'report-sample'") {
                    source.chars().take(40).collect()
                } else { String::new() },
            }).collect() }
    }
    pub fn append(&mut self, source: &str, kind: PolicySource, disposition: PolicyDisposition) -> Result<(), Error> {
        if self.bytes.saturating_add(source.len()) > MAX_BYTES || source.split_ascii_whitespace().count() > MAX_TOKENS { return Err(Error::Capacity); }
        let parsed = csp::CspList::parse(source, kind, disposition);
        if self.policies.len().saturating_add(parsed.0.len()) > MAX_POLICIES { return Err(Error::Capacity); }
        self.policies.try_reserve(parsed.0.len()).map_err(|_|Error::Capacity)?;
        self.self_urls.try_reserve(parsed.0.len()).map_err(|_|Error::Capacity)?;
        self.bytes += source.len();
        for mut policy in parsed.0 {
            if kind == PolicySource::Meta {
                policy.directive_set.retain(|directive| !matches!(directive.name.as_str(), "report-uri" | "frame-ancestors" | "sandbox"));
            }
            let serialized=policy.to_string();
            self.policies.push((policy, serialized));
            self.self_urls.push(None);
        }
        Ok(())
    }
    pub fn check_inline(&self, source:&str, nonce:Option<&str>, kind:InlineCheckType)->Result<Decision,Error>{
        let expressions=self.policies.iter().flat_map(|(policy,_)|&policy.directive_set).map(|directive|directive.value.len()).sum::<usize>();
        // Hash checks must not amplify one large inline source into unbounded work.
        if source.len()>8*1024*1024 || source.len().saturating_mul(expressions)>128*1024*1024{return Err(Error::Capacity);}
        let source=if crate::smuggle::may_contain(source){alloc::borrow::Cow::Owned(String::from_utf16_lossy(&crate::smuggle::utf16_units(source)))}else{alloc::borrow::Cow::Borrowed(source)};
        let source=source.as_ref();
        let element=csp::Element{nonce:nonce.map(alloc::borrow::Cow::Borrowed)};
        let mut decision=Decision{blocked:false,violations:Vec::new()};
        for(policy,raw)in &self.policies{
            for directive in &policy.directive_set{
                if directive.inline_check(&element,kind,policy,source)==csp::CheckResult::Allowed{continue;}
                let report_only=policy.disposition==PolicyDisposition::Report;decision.blocked|=!report_only;
                let name=match kind{InlineCheckType::Script|InlineCheckType::Navigation=>"script-src-elem",InlineCheckType::ScriptAttribute=>"script-src-attr",InlineCheckType::Style=>"style-src-elem",InlineCheckType::StyleAttribute=>"style-src-attr"};
                // Reporting metadata is assembled here so UTF-8 samples cannot be cut inside
                // a codepoint by the dependency's aggregate reporting convenience method.
                let sample=if directive.value.iter().any(|value|value=="'report-sample'"){source.chars().take(40).collect()}else{String::new()};
                decision.violations.push(Violation{directive:name.into(),original_policy:raw.clone(),blocked_uri:"inline".into(),report_only,sample});
            }
        }
        Ok(decision)
    }
    pub fn check(&self, target: &str, self_url: &str, destination: Destination) -> Result<Decision, Error> {
        self.check_request(target,self_url,destination,"","",false)
    }
    pub fn check_script_request(&self,target:&str,self_url:&str,nonce:&str,integrity:&str,parser_inserted:bool)->Result<Decision,Error>{
        self.check_request(target,self_url,Destination::Script,nonce,integrity,parser_inserted)
    }
    fn check_request(&self, target: &str, self_url: &str, destination: Destination, nonce:&str,integrity:&str,parser_inserted:bool) -> Result<Decision, Error> {
        self.check_request_redirect(target, target, self_url, destination, nonce, integrity, parser_inserted, 0)
    }
    /// CSP fetch checks use both the original URL and the current redirect URL.
    pub fn check_script_redirect(&self, original: &str, current: &str, self_url: &str,
        nonce: &str, integrity: &str, parser_inserted: bool, redirect_count: u32) -> Result<Decision, Error> {
        self.check_request_redirect(original, current, self_url, Destination::Script, nonce, integrity, parser_inserted, redirect_count)
    }
    pub fn check_resource_redirect(&self, original:&str,current:&str,self_url:&str,destination:Destination,
        nonce:&str,integrity:&str,parser_inserted:bool,redirect_count:u32)->Result<Decision,Error> {
        self.check_request_redirect(original,current,self_url,destination,nonce,integrity,parser_inserted,redirect_count)
    }
    pub fn check_script_response(&self, original: &str, final_url: &str, self_url: &str,
        nonce: &str, integrity: &str, parser_inserted: bool, redirect_count: u32) -> Result<Decision, Error> {
        self.check_resource_response(original,final_url,self_url,Destination::Script,nonce,integrity,parser_inserted,redirect_count)
    }
    pub fn check_resource_response(&self, original: &str, final_url: &str, self_url: &str,destination:Destination,
        nonce: &str, integrity: &str, parser_inserted: bool, redirect_count: u32) -> Result<Decision, Error> {
        if self.policies.is_empty() { return Ok(Decision { blocked: false, violations: Vec::new() }); }
        let mut url = csp::Url::parse(original).map_err(|_|Error::InvalidUrl)?;
        url.set_fragment(None);
        let mut current_url = csp::Url::parse(final_url).map_err(|_|Error::InvalidUrl)?;
        current_url.set_fragment(None);
        let response = csp::Response { url: current_url.clone(), redirect_count };
        let request = csp::Request { url, current_url, origin: csp::Url::parse(self_url).map_err(|_|Error::InvalidUrl)?.origin(),
            redirect_count, destination, initiator: csp::Initiator::None,
            nonce: nonce.into(), integrity_metadata: integrity.into(),
            parser_metadata: if parser_inserted { csp::ParserMetadata::ParserInserted } else { csp::ParserMetadata::NotParserInserted } };
        let mut decision = Decision { blocked: false, violations: Vec::new() };
        for (index, (policy, _)) in self.policies.iter().enumerate() {
        let mut request = request.clone();
        if let Some(url) = &self.self_urls[index] { request.origin = url.origin(); }
        let (result, violations) = csp::CspList(alloc::vec![policy.clone()]).should_response_to_request_be_blocked(&request, &response);
        decision.blocked |= result == csp::CheckResult::Blocked;
        decision.violations.extend(violations.into_iter().map(|violation| Violation {
            directive: violation.directive.name, original_policy: violation.policy.to_string(),
            blocked_uri: crate::csp_report::reporting_url(request.url.as_str()),
            report_only: violation.policy.disposition == PolicyDisposition::Report, sample: String::new(),
        }));
        }
        Ok(decision)
    }
    fn check_request_redirect(&self, original: &str, target: &str, self_url: &str, destination: Destination, nonce:&str,integrity:&str,parser_inserted:bool, redirect_count:u32) -> Result<Decision, Error> {
        if self.policies.is_empty() { return Ok(Decision { blocked: false, violations: Vec::new() }); }
        let mut url = csp::Url::parse(target).map_err(|_|Error::InvalidUrl)?;
        url.set_fragment(None);
        let origin = csp::Url::parse(self_url).map_err(|_|Error::InvalidUrl)?.origin();
        let mut original = csp::Url::parse(original).map_err(|_|Error::InvalidUrl)?;
        original.set_fragment(None);
        let request = csp::Request { current_url: url, url: original, origin, redirect_count, destination,
            initiator: csp::Initiator::None, nonce: nonce.into(), integrity_metadata: integrity.into(), parser_metadata: if parser_inserted {csp::ParserMetadata::ParserInserted}else{csp::ParserMetadata::NotParserInserted} };
        let mut decision = Decision { blocked: false, violations: Vec::new() };
        for (index, (policy, raw)) in self.policies.iter().enumerate() {
            let mut request = request.clone();
            if let Some(url) = &self.self_urls[index] { request.origin = url.origin(); }
            if let csp::Violates::Directive(_) = policy.does_request_violate_policy(&request) {
                let report_only = policy.disposition == PolicyDisposition::Report;
                decision.blocked |= !report_only;
                // The effective directive is independent of the fallback policy directive.
                let directive = match destination { Destination::Worker | Destination::SharedWorker | Destination::ServiceWorker => "worker-src", Destination::Json | Destination::Text | Destination::None => "connect-src", Destination::Script => "script-src-elem",Destination::Style=>"style-src-elem", Destination::Image=>"img-src", Destination::Font=>"font-src", Destination::Frame | Destination::IFrame=>"frame-src", Destination::Object | Destination::Embed=>"object-src", _ => return Err(Error::InvalidUrl) };
                let blocked_uri = crate::csp_report::reporting_url(request.url.as_str());
                decision.violations.push(Violation { directive: directive.into(), original_policy: raw.clone(), blocked_uri, report_only, sample:String::new() });
            }
        }
        Ok(decision)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inherited_policies_keep_self_origin_and_new_policies_bind_independently() {
        let mut policies = PolicySet::default();
        policies.append("img-src 'self'", PolicySource::Header, PolicyDisposition::Enforce).unwrap();
        policies.bind_unbound_self_origin("https://creator.test/path").unwrap();
        assert!(!policies.check("https://creator.test/image", "data:text/html,child", Destination::Image).unwrap().blocked);
        assert!(policies.check("https://other.test/image", "https://other.test/", Destination::Image).unwrap().blocked);
        assert!(!policies.check_resource_response("https://creator.test/image", "https://creator.test/image", "data:text/html,child", Destination::Image, "", "", false, 0).unwrap().blocked);
        policies.append("img-src 'self'", PolicySource::Meta, PolicyDisposition::Report).unwrap();
        policies.bind_unbound_self_origin("https://other.test/new").unwrap();
        let decision = policies.check("https://creator.test/image", "data:text/html,child", Destination::Image).unwrap();
        assert!(!decision.blocked);
        assert_eq!(decision.violations.len(), 1);
        assert!(decision.violations[0].report_only);
        assert!(policies.retained_bytes() < 1024);
        let mut images_only = PolicySet::default();
        images_only.append("img-src 'none'", PolicySource::Header, PolicyDisposition::Enforce).unwrap();
        assert!(!images_only.check("data:text/html,child", "https://creator.test/", Destination::IFrame).unwrap().blocked);
    }
    #[test]
    fn specification_module_fetch_nonce_redirect_and_response_checks_keep_report_only_separate() {
        let mut policies = PolicySet::default();
        policies.append("script-src 'nonce-captured'",PolicySource::Header,PolicyDisposition::Enforce).unwrap();
        policies.append("script-src 'none'",PolicySource::Header,PolicyDisposition::Report).unwrap();
        let before = policies.check_script_redirect("https://first.test/start.js","https://first.test/start.js","https://page.test/","captured","",false,0).unwrap();
        assert!(!before.blocked);
        assert!(!before.violations.is_empty());
        assert!(before.violations.iter().all(|violation|violation.report_only));
        let after = policies.check_script_redirect("https://first.test/start.js","https://second.test/end.js","https://page.test/","captured","",false,1).unwrap();
        assert!(!after.blocked);
        let response = policies.check_script_response("https://first.test/start.js","https://second.test/end.js","https://page.test/","captured","",false,1).unwrap();
        assert!(!response.blocked);
        assert!(policies.check_script_redirect("https://first.test/start.js","https://second.test/end.js","https://page.test/","other","",false,1).unwrap().blocked);
    }
    #[test]
    fn specification_timer_string_compilation_and_trusted_sink_use_primary_policy_checks() {
        let mut policies = PolicySet::default();
        policies.append("script-src 'none' 'report-sample'", PolicySource::Header, PolicyDisposition::Report).unwrap();
        let source = "ä".repeat(50);
        let decision = policies.check_string_compilation(&source);
        assert!(!decision.blocked);
        assert_eq!(decision.violations[0].sample, "ä".repeat(40));
        assert_eq!(decision.violations[0].blocked_uri, "eval");
        policies.append("script-src 'unsafe-eval'", PolicySource::Header, PolicyDisposition::Enforce).unwrap();
        assert!(!policies.check_string_compilation(&source).blocked);
        policies.append("require-trusted-types-for 'script'", PolicySource::Header, PolicyDisposition::Report).unwrap();
        let sink = policies.check_timer_string(&source, "Window setTimeout");
        assert!(!sink.blocked);
        assert_eq!(sink.violations[0].sample, alloc::format!("Window setTimeout|{}", "ä".repeat(40)));
        policies.append("require-trusted-types-for 'script'", PolicySource::Header, PolicyDisposition::Enforce).unwrap();
        assert!(policies.check_timer_string("anything", "Window setInterval").blocked);
        let mut policies = PolicySet::default();
        policies.append("script-src-elem 'none'; default-src 'unsafe-eval'", PolicySource::Header, PolicyDisposition::Enforce).unwrap();
        assert!(!policies.check_string_compilation("script source").blocked, "string compilation uses script-src/default-src, never script-src-elem");
    }
    #[test]
    fn inline_nonce_hash_unsafe_hashes_and_unicode_samples() {
        let mut policy=PolicySet::default();
        policy.append("script-src 'nonce-abc' 'sha256-jzgBGA4UWFFmpOBq0JpdsySukE1FrEN5bUpoK8Z29fY=' 'report-sample'",PolicySource::Header,PolicyDisposition::Enforce).unwrap();
        assert!(!policy.check_inline("doSubmit()",None,InlineCheckType::Script).unwrap().blocked);
        assert!(!policy.check_inline("other()",Some("abc"),InlineCheckType::Script).unwrap().blocked);
        assert!(policy.check_inline("doSubmit()",Some("abc"),InlineCheckType::ScriptAttribute).unwrap().blocked,"nonces never authorize handlers; hashes need unsafe-hashes");
        let source="ä".repeat(50);assert_eq!(policy.check_inline(&source,None,InlineCheckType::Script).unwrap().violations[0].sample,"ä".repeat(40));
        let mut policy=PolicySet::default();policy.append("script-src-attr 'unsafe-hashes' 'sha256-jzgBGA4UWFFmpOBq0JpdsySukE1FrEN5bUpoK8Z29fY='",PolicySource::Header,PolicyDisposition::Enforce).unwrap();
        assert!(!policy.check_inline("doSubmit()",None,InlineCheckType::ScriptAttribute).unwrap().blocked);
    }
    #[test]
    fn worker_fallback_multiple_policies_and_report_only() {
        let mut policy=PolicySet::default();
        policy.append("child-src 'none'; script-src 'self'",PolicySource::Meta,PolicyDisposition::Enforce).unwrap();
        let decision=policy.check("https://example.test/worker.js","https://example.test/page",Destination::Worker).unwrap();
        assert!(decision.blocked);assert_eq!(decision.violations[0].directive,"worker-src");assert_eq!(decision.violations[0].blocked_uri,"https://example.test/worker.js");
        let mut policy=PolicySet::default();policy.append("worker-src 'none'",PolicySource::Header,PolicyDisposition::Report).unwrap();
        assert!(!policy.check("https://example.test/worker.js","https://example.test/page",Destination::Worker).unwrap().blocked);
        policy.append("worker-src 'self'; child-src 'none'",PolicySource::Header,PolicyDisposition::Enforce).unwrap();
        assert!(!policy.check("https://example.test/worker.js","https://example.test/page",Destination::Worker).unwrap().blocked);
        let decision=policy.check("https://other.test/worker.js#fragment","https://example.test/page",Destination::Worker).unwrap();
        assert!(decision.blocked);assert_eq!(decision.violations[0].blocked_uri,"https://other.test/worker.js");
    }
}

#[cfg(test)]
mod image_policy_tests {
    use super::*;
    #[test]
    fn images_use_actual_img_default_fallback_and_original_url_metadata() {
        let mut policy=PolicySet::default();
        policy.append("default-src 'none'",PolicySource::Header,PolicyDisposition::Enforce).unwrap();
        let decision=policy.check("https://other.test/image.png?version=1#fragment","https://example.test/page",Destination::Image).unwrap();
        assert!(decision.blocked);assert_eq!(decision.violations[0].directive,"img-src");
        assert_eq!(decision.violations[0].blocked_uri,"https://other.test/image.png?version=1");
        let mut report=PolicySet::default();report.append("img-src 'none'",PolicySource::Header,PolicyDisposition::Report).unwrap();
        let decision=report.check("https://example.test/image.png","https://example.test/page",Destination::Image).unwrap();
        assert!(!decision.blocked);assert!(decision.violations[0].report_only);
    }
}
