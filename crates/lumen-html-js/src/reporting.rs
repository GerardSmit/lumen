//! Realm-scoped Reporting observers. Endpoint parsing and report bodies are shared.
use super::*;
use lumen_bind::{CtorRet,Host};
use lumen::embed::{JsFunction,JsHost};
use lumen_common::reporting::{CspBody,Endpoints};
use std::{cell::Cell,collections::VecDeque,rc::Weak,time::Instant};

const MAX_OBSERVERS:usize=64;
const MAX_REPORTS:usize=100;
const MAX_BYTES:usize=65_536;
struct Record {body:Rc<CspBody>,bytes:usize}
pub(crate) struct State {
    pub endpoints:RefCell<Endpoints>,
    pub status:Cell<u16>,
    pub user_agent:RefCell<String>,
    clock:Instant,
    observers:RefCell<Vec<Weak<ObserverData>>>,
    buffered:RefCell<VecDeque<Record>>,
    buffered_bytes:Cell<usize>,
    owner_slot:RefCell<Option<String>>,
}
impl Default for State {
    fn default()->Self {Self{endpoints:RefCell::new(Endpoints::default()),status:Cell::new(0),user_agent:RefCell::new(String::new()),clock:Instant::now(),observers:RefCell::new(Vec::new()),buffered:RefCell::new(VecDeque::new()),buffered_bytes:Cell::new(0),owner_slot:RefCell::new(None)}}
}
impl State {
    pub fn elapsed_ms(&self)->u64{self.clock.elapsed().as_millis().min(u64::MAX as u128)as u64}
    pub fn configure(&self,url:&str,headers:&[(String,String)])->Result<(),lumen_common::csp::Error>{self.endpoints.borrow_mut().configure(url,headers,self.elapsed_ms())}
    fn refresh_owners(&self,ctx:&mut Ctx)->OpResult<()> {
        let values=self.observers.borrow().iter().filter_map(Weak::upgrade).filter(|data|data.registered.get()).filter_map(|data|data.wrapper.borrow().as_ref().and_then(WeakValue::upgrade)).collect();
        let array=ctx.make_array(values);let global=ctx.global_object();
        let existing_slot=self.owner_slot.borrow().clone();
        if let Some(slot)=existing_slot.as_ref(){ctx.set_native_internal_value_slot(&global,slot,array).map_err(OpError::thrown)?;}
        else {let slot=ctx.allocate_native_private_slot_name();ctx.define_native_internal_value_slot(&global,&slot,array).map_err(OpError::thrown)?;*self.owner_slot.borrow_mut()=Some(slot);}
        Ok(())
    }
    pub fn record(self:&Rc<Self>,ctx:&mut Ctx,body:CspBody)->OpResult<()> {
        let bytes=lumen_common::reporting::csp_envelope(&body,"",0).map_err(|_|OpError::new("QuotaExceededError","report exceeds bounded storage"))?.len();
        let body=Rc::new(body);
        {let mut buffered=self.buffered.borrow_mut();while buffered.len()>=MAX_REPORTS||self.buffered_bytes.get().saturating_add(bytes)>MAX_BYTES {let Some(old)=buffered.pop_front()else{break};self.buffered_bytes.set(self.buffered_bytes.get().saturating_sub(old.bytes));}
        buffered.push_back(Record{body:body.clone(),bytes});self.buffered_bytes.set(self.buffered_bytes.get()+bytes);}
        let observers=self.observers.borrow().iter().filter_map(Weak::upgrade).filter(|data|data.registered.get()).collect::<Vec<_>>();
        for data in observers {data.enqueue(ctx,self,body.clone(),bytes)?;}
        Ok(())
    }
    fn notify(&self,ctx:&mut Ctx)->OpResult<()> {
        // The task captures the ordered registered list, not just the observer
        // whose queue became nonempty. All callbacks in this notification run
        // before the task's normal Promise checkpoint.
        let observers=self.observers.borrow().iter().filter_map(Weak::upgrade).filter(|data|data.registered.get()).filter_map(|data|{
            let wrapper=data.wrapper.borrow().as_ref().and_then(WeakValue::upgrade)?;Some((data,wrapper))
        }).collect::<Vec<_>>();
        scheduling::queue_task(ctx,move|ctx|{
            for(data,wrapper)in observers {
                if data.pending.borrow().is_empty(){continue}
                let records=data.take(ctx)?;let array=ctx.make_array(records);
                let callback_realm=ctx.function_host_realm(&data.callback).map_err(OpError::thrown)?;
                ctx.with_host_realm(&callback_realm,|ctx|{
                    if let Err(error)=data.callback.call(ctx,wrapper.clone(),&[array,wrapper]){let exception=error.to_value(ctx);DomRealm::report_exception(ctx,exception);}
                }).map_err(browsing_context::host_realm_error)?;
            }
            Ok(())
        })
    }
}
struct ObserverData {
    callback:JsFunction,types:Vec<String>,buffered:Cell<bool>,registered:Cell<bool>,
    wrapper:RefCell<Option<WeakValue>>,pending:RefCell<VecDeque<Record>>,bytes:Cell<usize>,
}
impl ObserverData {
    fn enqueue(self:&Rc<Self>,ctx:&mut Ctx,state:&Rc<State>,body:Rc<CspBody>,bytes:usize)->OpResult<()> {
        if !self.types.is_empty()&&!self.types.iter().any(|kind|kind=="csp-violation"){return Ok(())}
        let was_empty=self.pending.borrow().is_empty();
        {let mut pending=self.pending.borrow_mut();while pending.len()>=MAX_REPORTS||self.bytes.get().saturating_add(bytes)>MAX_BYTES {let Some(old)=pending.pop_front()else{break};self.bytes.set(self.bytes.get().saturating_sub(old.bytes));}
        pending.push_back(Record{body,bytes});self.bytes.set(self.bytes.get()+bytes);}
        if was_empty {state.notify(ctx)?;}
        Ok(())
    }
    fn take(&self,ctx:&mut Ctx)->OpResult<Vec<Value>> {
        self.bytes.set(0);self.pending.borrow_mut().drain(..).map(|record|report_value(ctx,&record.body)).collect()
    }
}
fn report_value(ctx:&mut Ctx,body:&CspBody)->OpResult<Value> {
    let value=Value::Obj(ctx.new_object());let report_body=Value::Obj(ctx.new_object());
    let text=|value:&str|Value::Str(value.to_string().into());
    let nullable=|value:&Option<String>|value.as_ref().map_or(Value::Null,|value|text(value));
    let number=|value:Option<u32>|value.map_or(Value::Null,|value|Value::Num(value as f64));
    for(key,field)in[("documentURL",text(&body.document_url)),("referrer",text(&body.referrer)),("blockedURL",text(&body.blocked_url)),("effectiveDirective",text(&body.effective_directive)),("originalPolicy",text(&body.original_policy)),("sourceFile",nullable(&body.source_file)),("sample",text(&body.sample)),("disposition",text(if body.report_only{"report"}else{"enforce"})),("statusCode",Value::Num(body.status_code as f64)),("lineNumber",number(body.line_number)),("columnNumber",number(body.column_number))]{ctx.create_data_property(&report_body,key,field).map_err(OpError::thrown)?;}
    for(key,field)in[("type",text("csp-violation")),("url",text(&body.document_url)),("body",report_body)]{ctx.create_data_property(&value,key,field).map_err(OpError::thrown)?;}
    Ok(value)
}
#[lumen_bind::class(name="ReportingObserver",hint(js(webidl)))]
pub struct DomReportingObserver {state:Rc<State>,data:Rc<ObserverData>}
impl lumen::embed::NativeIdentityOwner for DomReportingObserver {
    const TRACES_NATIVE_VALUES:bool=true;
    fn trace_native_values(&self,visit:&mut dyn FnMut(&Value)){visit(self.data.callback.value());}
    fn trace_native_identities(&self,_epoch:u64,_visit:&mut dyn FnMut(&Value)){}
}
struct ConstructorResult(DomReportingObserver);
impl CtorRet<JsHost,DomReportingObserver> for ConstructorResult {
    fn into_ctor(self,cx:&<JsHost as Host>::Cx<'_>)->Result<Value,Value>{
        let data=self.0.data.clone();let value=<JsHost as Host>::construct(cx,self.0)?;
        <JsHost as Host>::with_ctx(cx,|ctx|{*data.wrapper.borrow_mut()=ctx.weak_value(&value);ctx.set_native_identity_owner::<DomReportingObserver>(&value).expect("ReportingObserver native brand");});Ok(value)
    }
}
#[lumen_bind::methods]
impl DomReportingObserver {
    #[constructor]
    fn new(ctx:&mut Ctx,callback:JsFunction,options:Option<Value>)->OpResult<ConstructorResult>{
        if options.as_ref().is_some_and(|value|!matches!(value,Value::Null|Value::Undefined|Value::Obj(_))){return Err(OpError::type_error("ReportingObserver options must be a dictionary"))}
        let state=realm_services::RealmServices::<State>::current(ctx).ok_or_else(||OpError::new("Error","Reporting is not installed"))?;
        let types=match ui_events::dictionary_member(ctx,&options,"types")?{Some(value)=>ctx.convert_iterable(&value,64,|ctx,value|ctx.coerce_string(&value).map(|value|value.to_string()).map_err(OpError::thrown))?,None=>Vec::new()};
        if types.iter().map(String::len).sum::<usize>()>8192{return Err(OpError::new("QuotaExceededError","ReportingObserver types exceed bounds"))}
        let buffered=ui_events::dictionary_boolean(ctx,&options,"buffered",false)?;
        let data=Rc::new(ObserverData{callback,types,buffered:Cell::new(buffered),registered:Cell::new(false),wrapper:RefCell::new(None),pending:RefCell::new(VecDeque::new()),bytes:Cell::new(0)});
        Ok(ConstructorResult(Self{state,data}))
    }
    fn observe(&self,ctx:&mut Ctx)->OpResult<()> {
        if !self.data.registered.get(){
            let mut observers=self.state.observers.borrow_mut();observers.retain(|entry|entry.upgrade().is_some_and(|data|data.registered.get()));
            if observers.len()>=MAX_OBSERVERS{return Err(OpError::new("QuotaExceededError","too many ReportingObservers"))}
            observers.push(Rc::downgrade(&self.data));self.data.registered.set(true);drop(observers);self.state.refresh_owners(ctx)?;
        }
        if self.data.buffered.replace(false){let buffered=self.state.buffered.borrow().iter().map(|record|(record.body.clone(),record.bytes)).collect::<Vec<_>>();for(body,bytes)in buffered{self.data.enqueue(ctx,&self.state,body,bytes)?;}}
        Ok(())
    }
    fn disconnect(&self,ctx:&mut Ctx)->OpResult<()> {self.data.registered.set(false);self.state.refresh_owners(ctx)}
    #[method(name="takeRecords")]
    fn take_records(&self,ctx:&mut Ctx)->OpResult<Vec<Value>>{self.data.take(ctx)}
}
pub(crate) fn install(ctx:&mut Ctx,state:Rc<State>)->OpResult<()> {
    realm_services::RealmServices::replace_shared_current(ctx,state);
    let constructor=ctx.class_constructor::<DomReportingObserver>();let global=ctx.global_object();install_interface(ctx,&global,"ReportingObserver",constructor).map_err(OpError::thrown)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reporting_notification_snapshots_registered_observers_before_microtasks_and_gc(){
        let mut runtime=lumen_runtime::Runtime::new_browser();let engine=runtime.engine();
        let realm=crate::install(engine.ctx(),"<head></head><body></body>",128).unwrap();realm.set_document_url("https://example.test/page");
        realm.set_content_security_policy_headers(&[("Content-Security-Policy-Report-Only".into(),"script-src 'none'; report-to unavailable".into())]).unwrap();
        assert!(matches!(engine.eval_value("globalThis.order=[];new ReportingObserver(()=>{order.push('first');Promise.resolve().then(()=>order.push('microtask'));}).observe();new ReportingObserver(()=>order.push('second')).observe();true"),Ok(Ok(Value::Bool(true)))));
        engine.ctx().collect_garbage_for_host();
        let node=realm.session.borrow().document().root();assert!(realm.prepare_handler_csp(engine.ctx(),node,"blocked()").unwrap());
        assert!(scheduling::run_tasks(runtime.engine(),16).is_empty());assert!(scheduling::run_tasks(runtime.engine(),16).is_empty());
        assert!(matches!(runtime.engine().eval_value("order.join(',')==='first,second,microtask'"),Ok(Ok(Value::Bool(true)))));
    }
    #[test]
    fn csp_reporting_observers_buffer_filter_disconnect_and_take_records(){
        let mut runtime=lumen_runtime::Runtime::new_browser();let engine=runtime.engine();
        let realm=crate::install(engine.ctx(),"<head></head><body></body>",128).unwrap();realm.set_document_url("https://example.test/page");
        realm.set_reporting_response_metadata(200,"Lumen test").unwrap();
        realm.set_content_security_policy_headers(&[("Content-Security-Policy-Report-Only".into(),"script-src 'none'; report-to unavailable".into())]).unwrap();
        assert!(matches!(engine.eval_value("globalThis.observed=[];globalThis.filtered=0;globalThis.ro=new ReportingObserver(function(reports,observer){observed.push([reports,this===observer,observer===ro]);},{types:['csp-violation']});ro.observe();globalThis.other=new ReportingObserver(()=>filtered++,{types:['deprecation']});other.observe();true"),Ok(Ok(Value::Bool(true)))));
        let node=realm.session.borrow().document().root();assert!(realm.prepare_handler_csp(engine.ctx(),node,"blocked()").unwrap());
        assert!(scheduling::run_tasks(runtime.engine(),16).is_empty());
        assert!(matches!(runtime.engine().eval_value("observed.length===0"),Ok(Ok(Value::Bool(true)))),"observer notification follows the violation task in the next turn");
        assert!(scheduling::run_tasks(runtime.engine(),16).is_empty());
        let verdict=runtime.engine().eval_value("observed.length===1 && observed[0][1] && observed[0][2] && filtered===0 && observed[0][0][0].type==='csp-violation' && observed[0][0][0].body.statusCode===200 && observed[0][0][0].body.sourceFile===null && ro.takeRecords().length===0");
        let details=match runtime.engine().eval_value("JSON.stringify(observed)+':filtered='+filtered"){Ok(Ok(Value::Str(value)))=>value.to_string(),_=>"diagnostic evaluation failed".into()};
        assert!(matches!(verdict,Ok(Ok(Value::Bool(true)))),"observer details {details}");
        assert!(matches!(runtime.engine().eval_value("ro.disconnect();globalThis.replay=new ReportingObserver(()=>{}, {buffered:true});replay.observe();replay.takeRecords().length===1 && (replay.observe(),replay.takeRecords().length===0)"),Ok(Ok(Value::Bool(true)))));
        assert!(scheduling::run_tasks(runtime.engine(),16).is_empty());
    }
    #[test]
    fn reporting_endpoint_delivery_uses_real_post_and_removes_gone_endpoint(){
        let mut runtime=lumen_runtime::Runtime::new_browser();let engine=runtime.engine();
        let realm=crate::install(engine.ctx(),"<head><base href='https://bad.test/'></head><body></body>",128).unwrap();realm.set_document_url("https://example.test/path/page");realm.set_reporting_response_metadata(200,"Lumen test").unwrap();
        realm.set_content_security_policy_headers(&[("Reporting-Endpoints".into(),"group=\"../reports\"".into()),("Content-Security-Policy-Report-Only".into(),"script-src 'none'; report-to group; report-uri /ignored".into())]).unwrap();
        let transport=engine.eval_value("globalThis.reports=[];({request(method,url,headers,body,resolve,reject,redirect,options){reports.push({method,url,headers,body:JSON.parse(String.fromCharCode(...body)),options});resolve({status:410,url,headers:[],body:new Uint8Array()});return {abort(){}};}})").unwrap().ok().expect("transport");lumen_host::net::Transport::install(engine.ctx(),transport,Value::Undefined,Value::Undefined).expect("install transport");
        let node=realm.session.borrow().document().root();assert!(realm.prepare_handler_csp(engine.ctx(),node,"blocked()").unwrap());assert!(scheduling::run_tasks(runtime.engine(),16).is_empty());runtime.run_until_idle();
        assert!(matches!(runtime.engine().eval_value("reports.length===1 && reports[0].url==='https://example.test/reports' && reports[0].method==='POST' && reports[0].options.mode==='cors' && reports[0].options.credentials==='same-origin' && reports[0].headers.some(h=>h[0]==='content-type'&&h[1]==='application/reports+json') && reports[0].body[0].user_agent==='Lumen test' && reports[0].body[0].body.disposition==='report'"),Ok(Ok(Value::Bool(true)))));
        assert!(realm.reporting.endpoints.borrow().get("group",realm.reporting.elapsed_ms()).is_none());
    }
}
