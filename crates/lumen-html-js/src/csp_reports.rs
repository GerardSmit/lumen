//! Bounded user-agent report delivery over the shared host network pipeline.
use super::*;
use lumen_common::{csp::Violation, csp_report::{self, Context, Destinations}};

#[derive(Default)]
pub(crate) struct DeliveryBudget { requests: usize, bytes: usize }
const MAX_IN_FLIGHT: usize = 32;
const MAX_IN_FLIGHT_BYTES: usize = 65_536;

pub(crate) fn deliver(realm: &Rc<DomRealm>, ctx: &mut Ctx, violation: &Violation,
    document_url: &str, referrer: &str, source_file: Option<&str>,status_code:u16,line_number:u32,column_number:u32) {
    let context = Context {document_url, referrer, source_file,
        status_code, line_number, column_number};
    let endpoints = match csp_report::destinations(violation, document_url) {
        Ok(Destinations::Legacy(endpoints)) => endpoints,
        Ok(Destinations::Reporting(group)) => {
            let Ok(body)=lumen_common::reporting::CspBody::new(violation,&context)else{return};
            if let Err(error)=realm.reporting.record(ctx,body.clone()){let exception=error.to_value(ctx);DomRealm::report_exception(ctx,exception);}
            let endpoint=realm.reporting.endpoints.borrow().get(&group,realm.reporting.elapsed_ms()).map(|endpoint|endpoint.url.clone());
            let Some(endpoint)=endpoint else{return};
            let Ok(body)=lumen_common::reporting::csp_envelope(&body,&realm.reporting.user_agent.borrow(),0)else{return};
            let bytes=body.len();
            {let mut budget=realm.csp_report_budget.borrow_mut();if budget.requests>=MAX_IN_FLIGHT||budget.bytes.saturating_add(bytes)>MAX_IN_FLIGHT_BYTES{return}budget.requests+=1;budget.bytes+=bytes;}
            let weak=Rc::downgrade(realm);
            lumen_host::net::start_reporting_report(ctx,document_url,&endpoint.clone(),body,move|ctx,response|{
                let gone=if let Ok(response)=response{let gone=response.status==410;response.body.cancel(ctx);gone}else{false};
                if let Some(realm)=weak.upgrade(){if gone{realm.reporting.endpoints.borrow_mut().remove(&group,&endpoint);}let mut budget=realm.csp_report_budget.borrow_mut();budget.requests=budget.requests.saturating_sub(1);budget.bytes=budget.bytes.saturating_sub(bytes);}
            });
            return;
        },
        Err(_) => return,
    };
    if endpoints.is_empty() { return; }
    let Ok(body) = csp_report::legacy_body(violation, &context) else { return };
    for endpoint in endpoints {
        let bytes = body.len();
        {
            let mut budget = realm.csp_report_budget.borrow_mut();
            if budget.requests >= MAX_IN_FLIGHT || budget.bytes.saturating_add(bytes) > MAX_IN_FLIGHT_BYTES { break; }
            budget.requests += 1; budget.bytes += bytes;
        }
        let weak = Rc::downgrade(realm);
        lumen_host::net::start_policy_report(ctx, document_url, &endpoint, body.clone(), move |ctx, response| {
            if let Ok(response) = response { response.body.cancel(ctx); }
            if let Some(realm) = weak.upgrade() {
                let mut budget = realm.csp_report_budget.borrow_mut();
                budget.requests = budget.requests.saturating_sub(1);
                budget.bytes = budget.bytes.saturating_sub(bytes);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn report_delivery_bounds_in_flight_requests_without_suppressing_violation_events() {
        let mut runtime=lumen_runtime::Runtime::new_browser();let engine=runtime.engine();
        let realm=crate::install(engine.ctx(),"<head></head><body></body>",256).unwrap();
        realm.set_document_url("https://example.test/page");
        realm.set_content_security_policy_headers(&[("Content-Security-Policy-Report-Only".into(),"script-src 'none'; report-uri /report".into())]).unwrap();
        let transport=engine.eval_value(r#"globalThis.pendingReports=[];globalThis.budgetEvents=0;document.addEventListener('securitypolicyviolation',()=>budgetEvents++);({request(method,url,headers,body,resolve,reject){pendingReports.push({resolve,reject});return {abort(){}};}})"#).unwrap().ok().expect("script evaluation");
        lumen_host::net::Transport::install(engine.ctx(),transport,Value::Undefined,Value::Undefined).expect("install report transport");
        let node=realm.session.borrow().document().root();
        for _ in 0..40 { assert!(realm.prepare_handler_csp(engine.ctx(),node,"blocked()").unwrap()); }
        assert!(scheduling::run_tasks(runtime.engine(),64).is_empty());
        assert_eq!(realm.csp_report_budget.borrow().requests,MAX_IN_FLIGHT);
        assert!(realm.csp_report_budget.borrow().bytes<=MAX_IN_FLIGHT_BYTES);
        assert!(matches!(runtime.engine().eval_value("budgetEvents===40 && pendingReports.length===32"),Ok(Ok(Value::Bool(true)))));
        runtime.engine().eval_value("for(const request of pendingReports)request.resolve({status:204,url:'https://example.test/report',headers:[],body:new Uint8Array()})").unwrap().ok().expect("script evaluation");
        runtime.run_until_idle();assert_eq!(realm.csp_report_budget.borrow().requests,0);
    }
    #[test]
    fn report_uri_sends_real_post_after_event_with_document_base_and_credentials() {
        let mut runtime=lumen_runtime::Runtime::new_browser();
        let engine=runtime.engine();
        let realm=crate::install(engine.ctx(),"<head><base href='https://bad.test/wrong/'></head><body></body>",256).unwrap();
        realm.set_document_url("https://example.test/path/document");
        realm.set_content_security_policy_headers(&[("Content-Security-Policy-Report-Only".into(),"script-src 'none'; report-uri ../report https://other.test/collector".into())]).unwrap();
        let transport=engine.eval_value(r#"globalThis.reportOrder=[];globalThis.reportRequests=[];globalThis.reportExecuted=false;
            document.addEventListener('securitypolicyviolation',()=>reportOrder.push('event'));
            ({request(method,url,headers,body,resolve,reject,redirect,options){
                reportOrder.push('request');
                reportRequests.push({method,url,headers,body:JSON.parse(String.fromCharCode(...body)),redirect,options});
                resolve({status:204,url,headers:[],body:new Uint8Array()});return {abort(){}};
            }})"#).unwrap().ok().expect("script evaluation");
        lumen_host::net::Transport::install(engine.ctx(),transport,Value::Undefined,Value::Undefined).expect("install report transport");
        let result=engine.eval_value("const reported=document.createElement('script');reported.textContent='globalThis.reportExecuted=true';document.head.append(reported);reportExecuted && reportRequests.length===0 && reportOrder.length===0");
        assert!(matches!(result,Ok(Ok(Value::Bool(true)))));
        assert!(scheduling::run_tasks(runtime.engine(),16).is_empty());
        runtime.run_until_idle();
        let result=runtime.engine().eval_value(r#"reportRequests.length===2 && reportOrder.join(',')==='event,request,request' &&
            reportRequests[0].url==='https://example.test/report' && reportRequests[1].url==='https://other.test/collector' &&
            reportRequests.every(r=>r.method==='POST' && r.redirect==='manual' && r.options.redirect==='error' && r.options.mode==='no-cors' && r.options.credentials==='same-origin' && r.headers.some(h=>h[0]==='content-type' && h[1]==='application/csp-report')) &&
            reportRequests[0].options.cookiesAllowed && !reportRequests[1].options.cookiesAllowed &&
            reportRequests[0].body['csp-report']['disposition']==='report' && reportRequests[0].body['csp-report']['blocked-uri']==='inline'"#);
        assert!(matches!(result,Ok(Ok(Value::Bool(true)))),"report transport assertions");
        assert_eq!(realm.csp_report_budget.borrow().requests,0);
    }
}
