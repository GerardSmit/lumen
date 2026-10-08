//! Document policy ingress, typed violation events and host request decisions.
use super::*;
use lumen_common::csp::{PolicySet, PolicySource, PolicyDisposition, Destination, Violation, InlineCheckType};
use events::DomEvent;
use lumen_common::limits::{ByteBudget, ByteLease};
use std::sync::Arc;
pub(crate) struct State {
    policies: std::sync::Arc<PolicySet>, meta: HashSet<NodeId>,
    inline_attributes: Vec<PendingInlineAttribute>,
    inline_attribute_budget: Arc<ByteBudget>,
}
struct PendingInlineAttribute {
    target: NodeRetention,
    violations: Vec<Violation>,
    lease: ByteLease,
}
impl Default for State {
    fn default() -> Self {
        Self { policies: Default::default(), meta: Default::default(), inline_attributes: Vec::new(),
            inline_attribute_budget: ByteBudget::new(lumen_html::html::MAX_HTML_BYTES) }
    }
}
/// A pure, shared policy-container snapshot; it never retains a DOM realm.
#[derive(Clone, Default)]
pub(crate) struct PolicyContainer { policies: std::sync::Arc<PolicySet>, overflow: bool, referrer_policy: lumen_common::referrer::ReferrerPolicy }
impl std::fmt::Debug for PolicyContainer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyContainer").field("bytes", &self.policies.retained_bytes()).field("overflow", &self.overflow).field("referrer_policy", &self.referrer_policy).finish()
    }
}
impl PartialEq for PolicyContainer {
    fn eq(&self, other: &Self) -> bool { self.overflow == other.overflow && self.referrer_policy == other.referrer_policy && std::sync::Arc::ptr_eq(&self.policies, &other.policies) }
}
impl Eq for PolicyContainer {}
impl PolicyContainer { pub(crate) fn retained_bytes(&self) -> usize { self.policies.retained_bytes() } }
impl State {
    /// Pure policy evaluation for callers already holding the actual Document.
    pub(crate) fn check_inline(&self,source:&str,nonce:Option<&str>,kind:InlineCheckType)
        ->Result<lumen_common::csp::Decision,lumen_common::csp::Error> {
        self.policies.check_inline(source,nonce,kind)
    }
}
#[lumen_bind::class(name = "SecurityPolicyViolationEvent", extends = DomEvent, hint(js(webidl)))]
pub(crate) struct SecurityPolicyViolationEvent {
    base: DomEvent,
    document_uri: String, referrer: String, blocked_uri: String, effective_directive: String, violated_directive: String,
    original_policy: String, source_file: String, sample: String, disposition: String,
    status_code: u16, line_number: u32, column_number: u32,
}
#[lumen_bind::methods]
impl SecurityPolicyViolationEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
        let string=|ctx:&mut Ctx,name:&str|ui_events::dictionary_string(ctx,&options,name,"",false);
        let number=|ctx:&mut Ctx,name:&str|ui_events::dictionary_unsigned(ctx,&options,name,0,32);
        let disposition=ui_events::dictionary_string(ctx,&options,"disposition","enforce",false)?;
        if !matches!(disposition.as_str(),"enforce"|"report") {return Err(OpError::type_error("invalid CSP disposition"));}
        Ok(Self { base:DomEvent::new(ctx,kind,options.clone())?,document_uri:string(ctx,"documentURI")?,referrer:string(ctx,"referrer")?,blocked_uri:string(ctx,"blockedURI")?,effective_directive:string(ctx,"effectiveDirective")?,violated_directive:string(ctx,"violatedDirective")?,original_policy:string(ctx,"originalPolicy")?,source_file:string(ctx,"sourceFile")?,sample:string(ctx,"sample")?,disposition,status_code:number(ctx,"statusCode")? as u16,line_number:number(ctx,"lineNumber")?,column_number:number(ctx,"columnNumber")? })
    }
    #[getter(name="documentURI")] fn document_uri(&self)->String{self.document_uri.clone()}
    #[getter] fn referrer(&self)->String{self.referrer.clone()}
    #[getter(name="blockedURI")] fn blocked_uri(&self)->String{self.blocked_uri.clone()}
    #[getter(name="effectiveDirective")] fn effective_directive(&self)->String{self.effective_directive.clone()}
    #[getter(name="violatedDirective")] fn violated_directive(&self)->String{self.violated_directive.clone()}
    #[getter(name="originalPolicy")] fn original_policy(&self)->String{self.original_policy.clone()}
    #[getter(name="sourceFile")] fn source_file(&self)->String{self.source_file.clone()}
    #[getter] fn sample(&self)->String{self.sample.clone()}
    #[getter] fn disposition(&self)->String{self.disposition.clone()}
    #[getter(name="statusCode")] fn status_code(&self)->u16{self.status_code}
    #[getter(name="lineNumber")] fn line_number(&self)->u32{self.line_number}
    #[getter(name="columnNumber")] fn column_number(&self)->u32{self.column_number}
}
fn policy_error(error:lumen_common::csp::Error)->OpError { match error { lumen_common::csp::Error::Capacity=>OpError::new("QuotaExceededError","CSP policy budget exhausted"),lumen_common::csp::Error::InvalidUrl=>OpError::type_error("invalid CSP request URL") } }
impl DomRealm {
    fn capture_style_attribute(self: &Rc<Self>, node: NodeId, source: &str) -> Result<bool, lumen_html::Error> {
        if self.csp_overflow.get() { return Err(lumen_html::Error::LimitExceeded); }
        let mut state = self.csp.borrow_mut();
        let decision = state.policies.check_inline(source, None, InlineCheckType::StyleAttribute)
            .map_err(|_| lumen_html::Error::LimitExceeded)?;
        let allowed = !decision.blocked;
        if !decision.violations.is_empty() {
            // Include a conservative allowance for pending-vector spare
            // capacity and the native node-retention map entry. The same lease
            // stays alive through delivery of every policy's queued event.
            let mut bytes = 2 * std::mem::size_of::<PendingInlineAttribute>() + 256;
            bytes = bytes.checked_add(decision.violations.capacity()
                .checked_mul(std::mem::size_of::<Violation>()).ok_or(lumen_html::Error::LimitExceeded)?)
                .ok_or(lumen_html::Error::LimitExceeded)?;
            for violation in &decision.violations {
                for string in [&violation.directive, &violation.original_policy, &violation.blocked_uri, &violation.sample] {
                    bytes = bytes.checked_add(string.capacity()).ok_or(lumen_html::Error::LimitExceeded)?;
                }
            }
            let lease = state.inline_attribute_budget.reserve(bytes).ok_or(lumen_html::Error::LimitExceeded)?;
            state.inline_attributes.try_reserve(1).map_err(|_| lumen_html::Error::LimitExceeded)?;
            state.inline_attributes.push(PendingInlineAttribute {
                target: NodeRetention::new(self, node), violations: decision.violations, lease,
            });
        }
        Ok(allowed)
    }

    pub(crate) fn queue_style_attribute_violations(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<()> {
        let pending = std::mem::take(&mut self.csp.borrow_mut().inline_attributes);
        for pending in pending {
            let lease = Rc::new(pending.lease);
            for violation in pending.violations {
                self.queue_csp_violation_retained(ctx, violation, Some(pending.target.node), Some(lease.clone()))?;
            }
        }
        Ok(())
    }

    pub(crate) fn policy_container(&self) -> PolicyContainer {
        let mut state = self.csp.borrow_mut();
        if state.policies.has_unbound_self_origin() {
            let url = self.document_url().unwrap_or_else(|| "about:blank".into());
            if std::sync::Arc::make_mut(&mut state.policies).bind_unbound_self_origin(&url).is_err() { self.csp_overflow.set(true); }
        }
        PolicyContainer { policies: state.policies.clone(), overflow: self.csp_overflow.get(), referrer_policy: self.referrer_policy.get() }
    }
    pub(crate) fn inherit_policy_container(&self, container: &PolicyContainer) {
        self.csp.borrow_mut().policies = container.policies.clone();
        self.csp_overflow.set(container.overflow);
        self.referrer_policy.set(container.referrer_policy);
    }
    /// Actual navigation response metadata for CSP and Reporting bodies.
    pub fn set_reporting_response_metadata(&self,status:u16,user_agent:&str)->OpResult<()> {
        if user_agent.len()>8192{return Err(OpError::new("QuotaExceededError","Reporting user-agent exceeds bounds"))}
        self.reporting.status.set(status);*self.reporting.user_agent.borrow_mut()=user_agent.to_owned();Ok(())
    }
    /// Prepare an actual child navigation in its embedding document's policy.
    /// Report-only decisions queue violations while allowing the host request.
    pub fn prepare_frame_csp(self:&Rc<Self>,ctx:&mut Ctx,url:&str)->OpResult<bool> {
        self.prepare_embedded_csp(ctx, url, Destination::IFrame)
    }
    pub fn prepare_embedded_container_csp(self:&Rc<Self>,ctx:&mut Ctx,owner:NodeId,url:&str)->OpResult<bool> {
        let destination=match lumen_html::object::kind(self.session.borrow().document(),owner) {Some(lumen_html::object::Kind::Object)=>Destination::Object,Some(lumen_html::object::Kind::Embed)=>Destination::Embed,None=>Destination::IFrame};
        self.prepare_embedded_csp(ctx,url,destination)
    }
    fn prepare_embedded_csp(self:&Rc<Self>,ctx:&mut Ctx,url:&str,destination:Destination)->OpResult<bool> {
        if self.csp_overflow.get(){return Err(policy_error(lumen_common::csp::Error::Capacity))}
        let document_url=self.document_url().unwrap_or_else(||"about:blank".into());
        let mut decision=self.csp.borrow().policies.check(url,&document_url,destination).map_err(policy_error)?;
        let document=lumen_common::url::parse_url(&document_url,None).ok_or_else(||policy_error(lumen_common::csp::Error::InvalidUrl))?;
        let target=lumen_common::url::parse_url(url,None).ok_or_else(||policy_error(lumen_common::csp::Error::InvalidUrl))?;
        // CSP's frame reporting exception censors a cross-origin path even
        // though ordinary resource reports retain their original request URL.
        if destination==Destination::IFrame && matches!(target.scheme.as_str(),"http"|"https") && document.origin()!=target.origin(){for violation in &mut decision.violations{violation.blocked_uri=target.origin();}}
        for violation in decision.violations{self.queue_csp_violation(ctx,violation,None)?;}
        Ok(!decision.blocked)
    }
    /// Captured client policy for off-thread script fetches; no native/JS owners.
    pub fn module_fetch_policy_snapshot(&self) -> OpResult<std::sync::Arc<PolicySet>> {
        if self.csp_overflow.get() { return Err(OpError::new("QuotaExceededError", "CSP policy budget exhausted")); }
        Ok(self.csp.borrow().policies.clone())
    }
    pub fn script_fetch_integrity(&self, node: NodeId) -> String {
        self.session.borrow().document().get_attribute_ns_ref(node, None, "integrity").ok().flatten().unwrap_or("").to_owned()
    }
    pub fn report_module_fetch_violations(self: &Rc<Self>, ctx: &mut Ctx, violations: Vec<Violation>) -> OpResult<()> {
        for violation in violations { self.queue_csp_violation(ctx, violation, None)?; }
        Ok(())
    }
    pub(crate) fn prepare_timer_string(self: &Rc<Self>, ctx: &mut Ctx, source: &str, repeat: bool) -> OpResult<()> {
        if self.csp_overflow.get() { return Err(OpError::new("QuotaExceededError", "CSP policy budget exhausted")); }
        let sink = if repeat { "Window setInterval" } else { "Window setTimeout" };
        let decision = self.csp.borrow().policies.check_timer_string(source, sink);
        for violation in decision.violations { self.queue_csp_violation(ctx, violation, None)?; }
        if decision.blocked { return Err(OpError::type_error("TrustedScript is required by this timer sink's policy")); }
        Ok(())
    }
    pub(crate) fn prepare_timer_compilation(self: &Rc<Self>, ctx: &mut Ctx, source: &str) -> OpResult<()> {
        if self.csp_overflow.get() { return Err(OpError::new("QuotaExceededError", "CSP policy budget exhausted")); }
        let decision = self.csp.borrow().policies.check_string_compilation(source);
        for violation in decision.violations { self.queue_csp_violation(ctx, violation, None)?; }
        if decision.blocked { return Err(OpError::new("EvalError", "timer string compilation is blocked by Content Security Policy")); }
        Ok(())
    }
    /// Response policies precede parser metadata and author script.
    pub fn set_content_security_policy_headers(&self,headers:&[(String,String)])->OpResult<()> {
        self.reporting.configure(&self.document_url().unwrap_or_else(||"about:blank".into()),headers).map_err(policy_error)?;
        for (name,value) in headers {
            let disposition=if name.eq_ignore_ascii_case("content-security-policy") {PolicyDisposition::Enforce}
                else if name.eq_ignore_ascii_case("content-security-policy-report-only") {PolicyDisposition::Report}
                else {continue;};
            let header_policy={
                let mut state=self.csp.borrow_mut();
                std::sync::Arc::make_mut(&mut state.policies).append(value,PolicySource::Header,disposition).map_err(policy_error)?;
                state.policies.has_header_policy()
            };
            if self.has_browsing_context {self.session.borrow_mut().document_mut().set_connected_nonce_hiding(header_policy).map_err(dom_error)?;}
        }
        Ok(())
    }
    /// Enforce policy during real script preparation, before fetching or executing it.
    pub fn prepare_script_csp(self:&Rc<Self>,ctx:&mut Ctx,node:NodeId,source:&str,url:Option<&str>,parser_inserted:bool)->OpResult<bool>{
        if let Some(allowed)=self.scripts.borrow().csp_decision(node){return Ok(allowed);}
        if self.csp_overflow.get(){return Err(OpError::new("QuotaExceededError","CSP policy budget exhausted"));}
        let(nonce,integrity)={let session=self.session.borrow();let document=session.document();
            let nonce=document.cryptographic_nonce(node).ok().filter(|value|!value.is_empty()).map(str::to_owned);
            let nonce=nonce.filter(|_|document.kind(node).is_ok_and(|kind|match kind{NodeKind::Element{attributes,..}=>!attributes.iter().any(|(name,value)|[name.as_str(),value.as_str()].iter().any(|value|{let value=value.to_ascii_lowercase();value.contains("<link")||value.contains("<script")||value.contains("<style")})),_=>false}));
            (nonce,document.get_attribute_ns_ref(node,None,"integrity").ok().flatten().unwrap_or("").to_owned())};
        let decision=if let Some(url)=url{self.csp.borrow().policies.check_script_request(url,&self.document_url().unwrap_or_else(||"about:blank".into()),nonce.as_deref().unwrap_or(""),&integrity,parser_inserted)}else{self.csp.borrow().policies.check_inline(source,nonce.as_deref(),InlineCheckType::Script)}.map_err(policy_error)?;
        self.scripts.borrow_mut().record_csp_decision(node,!decision.blocked);
        for violation in decision.violations{self.queue_csp_violation(ctx,violation,Some(node))?;}
        if decision.blocked && url.is_some(){self.queue_script_preparation_error(ctx,node)?;}
        Ok(!decision.blocked)
    }
    pub(crate) fn prepare_handler_csp(self:&Rc<Self>,ctx:&mut Ctx,node:NodeId,source:&str)->OpResult<bool>{
        if self.csp_overflow.get(){return Err(OpError::new("QuotaExceededError","CSP policy budget exhausted"));}
        let decision=self.csp.borrow().policies.check_inline(source,None,InlineCheckType::ScriptAttribute).map_err(policy_error)?;
        for violation in decision.violations{self.queue_csp_violation(ctx,violation,Some(node))?;}
        Ok(!decision.blocked)
    }
    pub(crate) fn prepare_navigation_script_csp(self:&Rc<Self>,ctx:&mut Ctx,url:&str)->OpResult<bool>{
        if self.csp_overflow.get(){return Err(OpError::new("QuotaExceededError","CSP policy budget exhausted"));}
        let decision=self.csp.borrow().policies.check_inline(url,None,InlineCheckType::Navigation).map_err(policy_error)?;
        for violation in decision.violations {self.queue_csp_violation(ctx,violation,None)?;}
        Ok(!decision.blocked)
    }
    fn check_csp_worker(self:&Rc<Self>,ctx:&mut Ctx,url:&str,shared:bool)->OpResult<bool> {
        let self_url=self.document_url().unwrap_or_else(||"about:blank".into());
        let decision=self.csp.borrow().policies.check(url,&self_url,if shared{Destination::SharedWorker}else{Destination::Worker}).map_err(policy_error)?;
        for violation in decision.violations { self.queue_csp_violation(ctx,violation,None)?; }
        Ok(!decision.blocked)
    }
    pub(crate) fn queue_csp_violation(self:&Rc<Self>,ctx:&mut Ctx,violation:Violation,target:Option<NodeId>)->OpResult<()> {
        self.queue_csp_violation_retained(ctx,violation,target,None)
    }
    fn queue_csp_violation_retained(self:&Rc<Self>,ctx:&mut Ctx,violation:Violation,target:Option<NodeId>,lease:Option<Rc<ByteLease>>)->OpResult<()> {
        let weak=Rc::downgrade(self);let document_uri=self.document_url().unwrap_or_default();let referrer=self.document_referrer.borrow().clone();let location=ctx.current_execution_location();let source_file=location.as_ref().map(|(file,_,_)|file.clone());let(line_number,column_number)=location.as_ref().map(|(_,line,column)|(*line,*column)).unwrap_or((0,0));let status_code=self.reporting.status.get();let retention=target.map(|node|NodeRetention::new(self,node));
        let enqueue=move|ctx:&mut Ctx|scheduling::queue_task(ctx,move|ctx|{
            let Some(realm)=weak.upgrade()else{return Ok(())};let _retention=retention;let _lease=lease;
            let event=ctx.new_instance(SecurityPolicyViolationEvent{base:DomEvent::from_init("securitypolicyviolation",lumen_host::events::EventInit{bubbles:true,composed:true,..Default::default()}),document_uri:lumen_common::csp_report::reporting_url(&document_uri),referrer:lumen_common::csp_report::reporting_url(&referrer),blocked_uri:violation.blocked_uri.clone(),effective_directive:violation.directive.clone(),violated_directive:violation.directive.clone(),original_policy:violation.original_policy.clone(),source_file:source_file.as_deref().map(lumen_common::csp_report::reporting_url).unwrap_or_default(),sample:violation.sample.clone(),disposition:if violation.report_only{"report"}else{"enforce"}.into(),status_code,line_number,column_number});
            let node={let session=realm.session.borrow();target.filter(|node|session.document().is_connected_element(*node)).unwrap_or_else(||session.document().root())};let target=realm.wrap(ctx,node);realm.dispatch_event_to_target(ctx,node,target,event,true)?;csp_reports::deliver(&realm,ctx,&violation,&document_uri,&referrer,source_file.as_deref(),status_code,line_number,column_number);Ok(())
        });
        if let Some(handle)=self.child_realm_handle()? {ctx.with_host_realm(&handle,enqueue).map_err(browsing_context::host_realm_error)??;}else{enqueue(ctx)?;}
        Ok(())
    }
}
fn process_meta(realm:&DomRealm,document:&lumen_html::Document,node:NodeId) {
    realm.stylesheet_links.process_default_style_meta(document,node);
    if !document.is_connected_element(node)||lumen_html::forms::html_element_local_name(document,node)!=Some("meta")||!document.get_attribute_ns_ref(node,None,"http-equiv").ok().flatten().is_some_and(|value|value.eq_ignore_ascii_case("content-security-policy")){return;}
    let mut ancestor=node;let mut head=false;
    while let Ok(Some(parent))=document.parent(ancestor){head|=lumen_html::forms::html_element_local_name(document,parent)==Some("head");ancestor=parent;}
    if !head||ancestor!=document.root(){return;}
    let Some(value)=document.get_attribute_ns_ref(node,None,"content").ok().flatten()else{return};
    let mut state=realm.csp.borrow_mut();if state.meta.contains(&node){return;}if state.meta.len()>=64{realm.csp_overflow.set(true);return;}state.meta.insert(node);
    // Resource bounds fail closed: reject workers if a policy cannot be admitted.
    if std::sync::Arc::make_mut(&mut state.policies).append(value,PolicySource::Meta,PolicyDisposition::Enforce).is_err(){realm.csp_overflow.set(true);}
}
struct WorkerPolicy;
impl lumen_host::workers::WorkerRequestPolicy for WorkerPolicy {
    fn check(&self,ctx:&mut Ctx,url:&str,shared:bool)->OpResult<bool> {
        let Some(realm)=crate::window_globals::current_dom_realm(ctx)else{return Ok(true)};
        if realm.csp_overflow.get(){return Err(OpError::new("QuotaExceededError","CSP policy budget exhausted"));}
        realm.check_csp_worker(ctx,url,shared)
    }
}
pub(crate) fn install(ctx:&mut Ctx,realm:&Rc<DomRealm>)->OpResult<()> {
    let weak = Rc::downgrade(realm);
    realm.session.borrow_mut().document_mut().set_inline_style_attribute_policy(Some(Rc::new(move |_, node, source| {
        let Some(realm) = weak.upgrade() else { return Ok(false); };
        realm.capture_style_attribute(node, source)
    })));
    font_preload::install(realm);
    reporting::install(ctx,realm.reporting.clone())?;
    let constructor=ctx.class_constructor::<SecurityPolicyViolationEvent>();let global=ctx.global_object();ctx.member_set(&global,"SecurityPolicyViolationEvent",constructor).map_err(OpError::thrown)?;
    lumen_host::workers::set_request_policy(ctx,Rc::new(WorkerPolicy));
    let weak=Rc::downgrade(realm);
    realm.images.set_policy(Rc::new(move|source| {
        let Some(realm)=weak.upgrade()else{return Ok(lumen_common::csp::Decision{blocked:true,violations:Vec::new()})};
        if realm.csp_overflow.get(){return Err(lumen_common::csp::Error::Capacity)}
        let document_url=realm.document_url().unwrap_or_else(||"about:blank".into());
        let decision=realm.csp.borrow().policies.check(source,&document_url,Destination::Image);
        decision
    }));
    let weak=Rc::downgrade(realm);realm.images.set_response_policy(Rc::new(move||{
        let captured=weak.upgrade().map(|realm|(realm.csp.borrow().policies.clone(),realm.document_url().unwrap_or_else(||"about:blank".into()),realm.csp_overflow.get()));
        Rc::new(move|original:&str,final_url:&str,redirect_count:u32|{
            let Some((policies,document_url,overflow))=&captured else{return Ok(lumen_common::csp::Decision{blocked:true,violations:Vec::new()})};
            if *overflow{return Err(lumen_common::csp::Error::Capacity)}
            policies.check_resource_response(original,final_url,document_url,Destination::Image,"","",false,redirect_count)
        })
    }));
    let weak=Rc::downgrade(realm);realm.add_mutation_sink(Rc::new(move|document,mutation|{let Some(realm)=weak.upgrade()else{return};for root in mutation.kind.added_nodes(){process_meta(&realm,document,root);let mut node=root;while let Ok(Some(next))=selector::next_descendant(document,root,node){process_meta(&realm,document,next);node=next;}}}));
    let session=realm.session.borrow();let document=session.document();let mut node=document.root();while let Ok(Some(next))=selector::next_descendant(document,document.root(),node){process_meta(realm,document,next);node=next;}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_style_attribute_node_mutations_use_the_same_policy_and_identity() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let realm = crate::install(runtime.engine().ctx(), "<head></head><body><div id=target></div></body>", 128).unwrap();
        realm.set_document_url("https://style.test/page");
        realm.set_content_security_policy_headers(&[("Content-Security-Policy".into(), "style-src 'none'".into())]).unwrap();
        let source = r#"
            globalThis.attributeReports=[];
            addEventListener('securitypolicyviolation',e=>attributeReports.push(e.target.id));
            globalThis.target=document.getElementById('target');
            const attribute=document.createAttribute('style');attribute.value='width:21px';
            target.setAttributeNode(attribute);
            if(getComputedStyle(target).width==='21px')throw Error('setAttributeNode bypassed policy');
            target.style.width='13px';
            attribute.value='width:29px';
            if(target.getAttribute('style')!=='width:29px' || getComputedStyle(target).width==='29px')throw Error('Attr.value bypassed policy');
            target.removeAttributeNode(attribute);
            if(target.hasAttribute('style'))throw Error('attribute removal failed');
            target.style.width='7px';
            if(getComputedStyle(target).width!=='7px')throw Error('CSSOM update after removal blocked');true
        "#;
        assert!(matches!(runtime.engine().eval_value(source), Ok(Ok(Value::Bool(true)))));
        realm.queue_stylesheet_tasks(runtime.engine().ctx()).unwrap();
        assert!(scheduling::run_tasks(runtime.engine(), 128).is_empty());
        assert!(matches!(runtime.engine().eval_value("attributeReports.length===2 && attributeReports.every(id=>id==='target')"), Ok(Ok(Value::Bool(true)))));
    }
    #[test]
    fn specification_style_attribute_admission_is_atomic_and_releases_retired_storage() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let realm = crate::install(runtime.engine().ctx(), "<head></head><body><div id=target style='width:11px'></div></body>", 128).unwrap();
        realm.set_document_url("https://style.test/page");
        realm.set_content_security_policy_headers(&[("Content-Security-Policy".into(), "style-src 'none'".into())]).unwrap();
        let node = selector::query_selector(realm.session.borrow().document(), realm.session.borrow().document().root(), "#target").unwrap().unwrap();
        realm.csp.borrow_mut().inline_attribute_budget = ByteBudget::new(0);
        {
            let mut session = realm.session.borrow_mut();
            let document = session.document_mut();
            let count = document.node_count();
            assert_eq!(document.clone_node(node, false), Err(lumen_html::Error::LimitExceeded));
            assert_eq!(document.node_count(), count, "failed admission must reclaim its new detached node");
            assert_eq!(document.set_attribute(node, "style", "width:22px"), Err(lumen_html::Error::LimitExceeded));
            assert_eq!(document.get_attribute_ns(node, None, "style").unwrap().as_deref(), Some("width:11px"));
            assert!(document.inline_style_attribute_allowed(node), "failed mutation preserves the previous admitted declaration");
        }
        assert!(realm.csp.borrow().inline_attributes.is_empty());
        let budget = ByteBudget::new(lumen_html::html::MAX_HTML_BYTES);
        realm.csp.borrow_mut().inline_attribute_budget = budget.clone();
        realm.session.borrow_mut().document_mut().set_attribute(node, "style", "width:22px").unwrap();
        assert!(budget.reserved() > 0);
        let weak = Rc::downgrade(&realm);
        drop(realm);
        drop(runtime);
        assert!(weak.upgrade().is_none(), "pending native reports must not retain their realm in a cycle");
        assert_eq!(budget.reserved(), 0, "retiring a realm releases undelivered report storage");
    }
    #[test]
    fn specification_style_attribute_policy_blocks_rendering_and_reports_real_mutations() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let realm = crate::install(runtime.engine().ctx(), "<head></head><body></body>", 256).unwrap();
        realm.set_document_url("https://style.test/page");
        realm.set_content_security_policy_headers(&[("Content-Security-Policy".into(), "style-src 'none'".into())]).unwrap();
        let source = r#"
            globalThis.attributeEvents=[];
            document.addEventListener('securitypolicyviolation',e=>attributeEvents.push([e.target.id,e.effectiveDirective,e.blockedURI,e.isTrusted]));
            globalThis.target=document.createElement('div');target.id='target';
            target.setAttribute('style','width:31px');document.body.append(target);
            if(target.getAttribute('style')!=='width:31px' || target.style.width!=='31px')throw Error('authored attribute/declaration lost');
            if(getComputedStyle(target).width==='31px')throw Error('denied attribute applied');
            target.style.width='17px';
            if(getComputedStyle(target).width!=='17px')throw Error('CSSOM property update blocked');
            target.style='width:23px';
            if(getComputedStyle(target).width!=='23px')throw Error('PutForwards cssText update blocked');
            target.setAttributeNS('urn:custom','style','width:99px');
            if(getComputedStyle(target).width!=='23px')throw Error('namespaced style reached cascade');
            if(attributeEvents.length!==0)throw Error('violation dispatched synchronously');true
        "#;
        assert!(matches!(runtime.engine().eval_value(source), Ok(Ok(Value::Bool(true)))));
        realm.queue_stylesheet_tasks(runtime.engine().ctx()).unwrap();
        assert!(scheduling::run_tasks(runtime.engine(), 128).is_empty());
        assert!(matches!(runtime.engine().eval_value("attributeEvents.length===1 && attributeEvents[0][0]==='target' && attributeEvents[0][1]==='style-src-attr' && attributeEvents[0][2]==='inline' && attributeEvents[0][3]"), Ok(Ok(Value::Bool(true)))));
        assert_eq!(realm.csp.borrow().inline_attribute_budget.reserved(), 0,
            "queued violation storage releases at actual event delivery");
    }

    #[test]
    fn specification_style_attribute_checks_parser_position_and_stable_layout_decision() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let source = "<head></head><body><div id=before style='background:green'></div><script>first</script><meta http-equiv='Content-Security-Policy' content=\"style-src 'none'\"><div id=after style='background:green'></div></body>";
        let realm = crate::install_live_html(runtime.engine().ctx(), source, 256).unwrap();
        realm.set_document_url("https://style.test/page");
        let first = realm.next_document_parser_script(runtime.engine().ctx()).unwrap().unwrap();
        assert_eq!(first.text, "first");
        // The late body meta is ignored. The intervening host policy applies
        // to the later attribute, without rehashing the earlier admission.
        realm.set_content_security_policy_headers(&[("Content-Security-Policy".into(), "style-src 'none'".into())]).unwrap();
        while realm.next_document_parser_script(runtime.engine().ctx()).unwrap().is_some() {}
        assert!(matches!(runtime.engine().eval_value("getComputedStyle(document.getElementById('before')).backgroundColor==='rgb(0, 128, 0)' && getComputedStyle(document.getElementById('after')).backgroundColor!=='rgb(0, 128, 0)'"), Ok(Ok(Value::Bool(true)))));
    }

    #[test]
    fn specification_style_attribute_hash_and_report_only_use_actual_cascade_and_delivery() {
        for (header, policy, allowed, reports) in [
            ("Content-Security-Policy", "style-src 'unsafe-hashes' 'sha256-S0VSqEOmzmyOifPfat2sJ7ELOgkldAEbaXlvi5iMqjc='", true, 0),
            ("Content-Security-Policy", "style-src 'sha256-S0VSqEOmzmyOifPfat2sJ7ELOgkldAEbaXlvi5iMqjc='", false, 1),
            ("Content-Security-Policy", "style-src 'unsafe-hashes' 'sha256-UI8QfroYhb0WX073XBuM+RTPntpjZfkyFLsMw5vQfd0='", false, 1),
            ("Content-Security-Policy-Report-Only", "style-src 'none'", true, 1),
        ] {
            let mut runtime = lumen_runtime::Runtime::new_browser();
            let realm = crate::install(runtime.engine().ctx(), "<head></head><body></body>", 128).unwrap();
            realm.set_document_url("https://style.test/page");
            realm.set_content_security_policy_headers(&[(header.into(), policy.into())]).unwrap();
            assert!(matches!(runtime.engine().eval_value("globalThis.styleReports=0;addEventListener('securitypolicyviolation',()=>styleReports++);globalThis.target=document.createElement('div');target.setAttribute('style','background: green');document.body.append(target);true"), Ok(Ok(Value::Bool(true)))));
            let expected = format!("(getComputedStyle(target).backgroundColor==='rgb(0, 128, 0)')==={allowed}");
            assert!(matches!(runtime.engine().eval_value(&expected), Ok(Ok(Value::Bool(true)))), "{policy}");
            realm.queue_stylesheet_tasks(runtime.engine().ctx()).unwrap();
            assert!(scheduling::run_tasks(runtime.engine(), 128).is_empty());
            assert!(matches!(runtime.engine().eval_value(&format!("styleReports==={reports}")), Ok(Ok(Value::Bool(true)))), "{policy}");
            assert_eq!(realm.csp.borrow().inline_attribute_budget.reserved(), 0);
        }
    }
    struct CountingBackend(Rc<Cell<u32>>);
    impl lumen_host::workers::WorkerBackend for CountingBackend {
        fn spawn_dedicated(&self,_ctx:&mut Ctx,_spec:lumen_host::workers::DedicatedSpec)->lumen_bind::NativeResult<u64>{self.0.set(self.0.get()+1);Err(lumen_bind::NativeError::runtime("test backend reached"))}
        fn connect_shared(&self,_ctx:&mut Ctx,_spec:lumen_host::workers::SharedSpec)->lumen_bind::NativeResult<u64>{self.0.set(self.0.get()+1);Err(lumen_bind::NativeError::runtime("test shared backend reached"))}
    }
    #[test]
    fn csp_handler_violation_is_composed_and_retargets_through_closed_shadow() {
        let mut runtime=lumen_runtime::Runtime::new_browser();let engine=runtime.engine();
        let realm=crate::install(engine.ctx(),"<head></head><body><div id=host></div></body>",128).unwrap();realm.set_document_url("https://example.test/page");
        realm.set_content_security_policy_headers(&[("Content-Security-Policy".into(),"script-src 'none'".into())]).unwrap();
        let source=r#"globalThis.shadowRan=false;globalThis.shadowViolations=[];const host=document.getElementById('host');const shadow=host.attachShadow({mode:'closed'});shadow.innerHTML='<button id=inside onclick=\"globalThis.shadowRan=true\"></button>';const inside=shadow.querySelector('button');inside.addEventListener('securitypolicyviolation',e=>shadowViolations.push(e.target===inside && e.composed));document.addEventListener('securitypolicyviolation',e=>shadowViolations.push(e.target===host));inside.click();!shadowRan && shadowViolations.length===0"#;
        assert!(matches!(engine.eval_value(source),Ok(Ok(Value::Bool(true)))));
        assert!(scheduling::run_tasks(runtime.engine(),16).is_empty());
        assert!(matches!(runtime.engine().eval_value("!shadowRan && shadowViolations.length===2 && shadowViolations[0] && shadowViolations[1]"),Ok(Ok(Value::Bool(true)))));
    }
    #[test]
    fn csp_real_inline_scripts_handlers_nonce_hash_and_external_failure() {
        let mut runtime=lumen_runtime::Runtime::new_browser();let engine=runtime.engine();
        let realm=crate::install(engine.ctx(),"<head></head><body></body>",256).unwrap();
        realm.set_document_url("https://example.test/page");
        realm.set_content_security_policy_headers(&[("Content-Security-Policy".into(),"script-src 'nonce-abc' 'unsafe-hashes' 'sha256-jzgBGA4UWFFmpOBq0JpdsySukE1FrEN5bUpoK8Z29fY=' 'report-sample'".into())]).unwrap();
        let result=engine.eval_value(r#"globalThis.submits=0;globalThis.inlineDenied=false;globalThis.inlineAllowed=false;globalThis.scriptViolations=[];globalThis.doSubmit=()=>submits++;
            document.addEventListener('securitypolicyviolation',e=>scriptViolations.push([e.effectiveDirective,e.blockedURI,e.sample,e.target.id]));
            const deniedScript=document.createElement('script');deniedScript.id='blocked-script';deniedScript.textContent='globalThis.inlineDenied=true';document.head.append(deniedScript);
            const nonceScript=document.createElement('script');nonceScript.setAttribute('nonce','abc');nonceScript.textContent='globalThis.inlineAllowed=true';document.head.append(nonceScript);
            const hashScript=document.createElement('script');hashScript.textContent='doSubmit()';document.head.append(hashScript);
            // Apply the response policy before initializing these handlers.
            // Existing compiled handlers are not retroactively disabled.
            document.body.innerHTML='<button id=allowed onclick="doSubmit()"></button><button id=denied onclick="doSubmit();"></button>';
            document.getElementById('allowed').click();document.getElementById('denied').click();
            !inlineDenied && inlineAllowed && submits===2 && scriptViolations.length===0"#);
        let diagnostic=match engine.eval_value("JSON.stringify({inlineDenied,inlineAllowed,submits,scriptViolations})"){Ok(Ok(Value::Str(value)))=>value.to_string(),_=>"diagnostic failed".into()};
        assert!(matches!(result,Ok(Ok(Value::Bool(true)))),"inline CSP state {diagnostic}");
        let external=engine.eval_value("globalThis.resourceErrors=0;const external=document.createElement('script');external.id='external';external.src='https://example.test/blocked.js';external.onerror=()=>resourceErrors++;document.head.append(external);true");assert!(matches!(external,Ok(Ok(Value::Bool(true)))));
        let node=selector::query_selector(realm.session.borrow().document(),realm.session.borrow().document().root(),"#external").unwrap().unwrap();
        assert!(!realm.prepare_script_csp(engine.ctx(),node,"",Some("https://example.test/blocked.js"),true).unwrap());
        assert!(scheduling::run_tasks(runtime.engine(),32).is_empty());
        let result=runtime.engine().eval_value("resourceErrors===1 && scriptViolations.length===3 && scriptViolations[0][0]==='script-src-elem' && scriptViolations[0][1]==='inline' && scriptViolations[0][3]==='blocked-script' && scriptViolations[1][0]==='script-src-attr' && scriptViolations[1][2]==='doSubmit();' && scriptViolations[2][1]==='https://example.test/blocked.js'");
        assert!(matches!(result,Ok(Ok(Value::Bool(true)))));
    }
    #[test]
    fn csp_metadata_applies_at_parser_position_and_headers_precede_it() {
        let mut runtime=lumen_runtime::Runtime::new_browser();let engine=runtime.engine();
        let realm=crate::install_live_html(engine.ctx(),"<head><script>first</script><meta http-equiv=Content-Security-Policy content=\"child-src 'none'\"><script>second</script></head><body></body>",128).unwrap();
        realm.set_document_url("https://example.test/page");
        realm.set_content_security_policy_headers(&[("Content-Security-Policy-Report-Only".into(),"worker-src 'none'".into())]).unwrap();
        assert_eq!(realm.next_document_parser_script(engine.ctx()).unwrap().unwrap().text,"first");
        let first=realm.csp.borrow().policies.check("https://example.test/worker.js","https://example.test/page",Destination::Worker).unwrap();
        assert!(!first.blocked);assert_eq!(first.violations.len(),1);assert!(first.violations[0].report_only);
        assert_eq!(realm.next_document_parser_script(engine.ctx()).unwrap().unwrap().text,"second");
        let second=realm.csp.borrow().policies.check("https://example.test/worker.js","https://example.test/page",Destination::Worker).unwrap();
        assert!(second.blocked);assert_eq!(second.violations.len(),2);
        engine.eval_value("document.querySelector('meta').content=\"worker-src *\";document.querySelector('meta').remove()").unwrap().unwrap_or(Value::Undefined);
        assert!(realm.csp.borrow().policies.check("https://example.test/worker.js","https://example.test/page",Destination::Worker).unwrap().blocked,"changing/removing admitted meta cannot weaken policy");
    }
    #[test]
    fn csp_worker_blocks_before_backend_and_queues_native_bubbling_violation() {
        let mut runtime=lumen_runtime::Runtime::new_browser();let engine=runtime.engine();
        let realm=crate::install(engine.ctx(),"<head><meta http-equiv=Content-Security-Policy content=\"child-src 'none'; script-src 'self'\"></head><body></body>",128).unwrap();
        realm.set_document_url("https://example.test/page");
        let count=Rc::new(Cell::new(0));lumen_host::workers::set_backend(engine.ctx(),Rc::new(CountingBackend(count.clone())));
        let result=engine.eval_value("globalThis.cspEvents=[];document.addEventListener('securitypolicyviolation',e=>cspEvents.push([e.blockedURI,e.violatedDirective,e.isTrusted,e instanceof SecurityPolicyViolationEvent]));window.addEventListener('securitypolicyviolation',()=>cspEvents.push('window'));globalThis.sharedFailed=false;const shared=new SharedWorker('https://example.test/shared-worker.js');shared.onerror=e=>{e.preventDefault();sharedFailed=e.isTrusted};globalThis.workerFailed=false;const worker=new Worker('https://example.test/worker.js');worker.onerror=e=>{e.preventDefault();workerFailed=e.isTrusted};worker instanceof Worker && !workerFailed && cspEvents.length===0");
        assert!(matches!(result,Ok(Ok(Value::Bool(true)))));assert_eq!(count.get(),0);
        assert!(matches!(runtime.engine().eval_value("const ownEvent=new SecurityPolicyViolationEvent(\"securitypolicyviolation\",{effectiveDirective:\"worker-src\",violatedDirective:\"child-src\"});ownEvent.effectiveDirective===\"worker-src\" && ownEvent.violatedDirective===\"child-src\""),Ok(Ok(Value::Bool(true)))));
        runtime.run_until_idle();
        assert!(scheduling::run_tasks(runtime.engine(),16).is_empty());
        assert!(matches!(runtime.engine().eval_value("workerFailed && sharedFailed && cspEvents.length===4 && cspEvents[2][0]==='https://example.test/worker.js' && cspEvents[2][1]==='worker-src' && cspEvents[2][2] && cspEvents[2][3] && cspEvents[3]==='window'"),Ok(Ok(Value::Bool(true)))));
    }
}
