//! Font preload fetches use the normal asynchronous transport. They do not
//! create a FontFace: successful preload means the resource bytes were fetched.
use super::*;
use lumen_host::net::{self, Credentials, Mode, Redirect, RequestControl, RequestSpec};
use lumen_common::csp::{Destination, PolicySet};
use std::sync::Arc;

const MAX_REQUESTS:usize=4096;
const MAX_BYTES:usize=16*1024*1024;
const MAX_ACTIVE:usize=1;
const MAX_CACHED_BYTES:usize=16*1024*1024;

#[derive(Clone,PartialEq,Eq,Hash)]
struct Selection {url:String,crossorigin:Option<String>,integrity:String}
struct Entry {selection:Selection,generation:u64,control:Option<RequestControl>,body:Option<net::ResourceBodyControl>,root:Option<ResourceRequestRoot>}
struct Cached {bytes:Arc<[u8]>,url:String,redirected:bool}
pub(crate) struct State {
    dirty:Cell<bool>,entries:RefCell<HashMap<NodeId,Entry>>,next:Cell<u64>,
    violations:RefCell<Vec<lumen_common::csp::Violation>>,overflow:Cell<bool>,
    cache:RefCell<HashMap<Selection,Result<Cached,String>>>,cached_bytes:Cell<usize>,
}
impl Default for State {
    fn default()->Self {Self{dirty:Cell::new(true),entries:RefCell::new(HashMap::new()),next:Cell::new(0),violations:RefCell::new(Vec::new()),overflow:Cell::new(false),cache:RefCell::new(HashMap::new()),cached_bytes:Cell::new(0)}}
}
impl State {pub(crate) fn needs_pump(&self)->bool {self.dirty.get()}}
pub(crate) fn retire(ctx:&mut Ctx,realm:&Rc<DomRealm>) {
    let entries=std::mem::take(&mut *realm.font_preloads.entries.borrow_mut());
    for entry in entries.into_values(){if let Some(control)=entry.control{control.abort(ctx);}if let Some(body)=entry.body{body.cancel(ctx);}}
    realm.font_preloads.dirty.set(false);
    realm.font_preloads.cache.borrow_mut().clear();realm.font_preloads.cached_bytes.set(0);
}
pub(crate) fn preload_reader(realm:&Rc<DomRealm>)->Rc<dyn Fn(&str)->Option<Result<Arc<[u8]>,String>>> {
    let weak=Rc::downgrade(realm);
    Rc::new(move|url|{
        let realm=weak.upgrade()?;
        let key=realm.font_preloads.cache.borrow().keys().find(|key|key.url==url&&key.crossorigin.as_deref()==Some("")).cloned()?;
        let value=realm.font_preloads.cache.borrow_mut().remove(&key)?;
        let value=match value {
            Ok(cached)=>{
                realm.font_preloads.cached_bytes.set(realm.font_preloads.cached_bytes.get().saturating_sub(cached.bytes.len()));
                let policies=match realm.module_fetch_policy_snapshot(){Ok(policies)=>policies,Err(_)=>return Some(Err("Font policy budget exhausted".into()))};
                let document_url=realm.document_url().unwrap_or_else(||"about:blank".into());
                let decision=match policies.check_resource_response(url,&cached.url,&document_url,Destination::Font,"","",false,u32::from(cached.redirected)){Ok(decision)=>decision,Err(_)=>return Some(Err("Invalid cached font response URL".into()))};
                let blocked=decision.blocked;
                let mut queue=realm.font_preloads.violations.borrow_mut();
                if decision.violations.len()>64usize.saturating_sub(queue.len()){realm.font_preloads.overflow.set(true);return Some(Err("Font policy reporting budget exhausted".into()))}
                queue.extend(decision.violations);
                if blocked {Err("Cached font response blocked by document policy".into())}else{Ok(cached.bytes)}
            },
            Err(error)=>Err(error),
        };
        Some(value)
    })
}
fn cache_result(realm:&Rc<DomRealm>,selection:Selection,result:Result<Vec<u8>,String>,url:String,redirected:bool)->bool {
    if realm.font_preloads.cache.borrow().len()>=MAX_REQUESTS && !realm.font_preloads.cache.borrow().contains_key(&selection) {
        realm.font_preloads.overflow.set(true);return false;
    }
    if let Some(Ok(cached))=realm.font_preloads.cache.borrow_mut().remove(&selection) {
        realm.font_preloads.cached_bytes.set(realm.font_preloads.cached_bytes.get().saturating_sub(cached.bytes.len()));
    }
    let success=result.is_ok();
    let value=match result {
        Ok(bytes)=>{
            while bytes.len()>MAX_CACHED_BYTES.saturating_sub(realm.font_preloads.cached_bytes.get()) {
                let key=realm.font_preloads.cache.borrow().iter().find(|(_,value)|value.is_ok()).map(|(key,_)|key.clone());
                let Some(key)=key else{realm.font_preloads.overflow.set(true);return false};
                if let Some(Ok(old))=realm.font_preloads.cache.borrow_mut().remove(&key) {
                    realm.font_preloads.cached_bytes.set(realm.font_preloads.cached_bytes.get().saturating_sub(old.bytes.len()));
                }
            }
            realm.font_preloads.cached_bytes.set(realm.font_preloads.cached_bytes.get()+bytes.len());Ok(Cached{bytes:Arc::<[u8]>::from(bytes),url,redirected})
        },
        Err(error)=>Err(error),
    };
    if let Some(Ok(cached))=realm.font_preloads.cache.borrow_mut().insert(selection,value) {
        realm.font_preloads.cached_bytes.set(realm.font_preloads.cached_bytes.get().saturating_sub(cached.bytes.len()));
    }
    success
}
pub(crate) fn provider_policy(realm:&Rc<DomRealm>)->Rc<dyn Fn(&str)->bool> {
    let weak=Rc::downgrade(realm);
    Rc::new(move|url|{
        let Some(realm)=weak.upgrade()else{return false};
        if realm.csp_overflow.get()||realm.font_preloads.overflow.get(){return false}
        let document_url=realm.document_url().unwrap_or_else(||"about:blank".into());
        let Ok(policies)=realm.module_fetch_policy_snapshot()else{return false};
        let Ok(decision)=policies.check(url,&document_url,Destination::Font)else{return false};
        let mut queue=realm.font_preloads.violations.borrow_mut();
        if decision.violations.len()>64usize.saturating_sub(queue.len()){realm.font_preloads.overflow.set(true);return false}
        queue.extend(decision.violations);
        !decision.blocked
    })
}
pub(crate) fn drain_violations(ctx:&mut Ctx,realm:&Rc<DomRealm>)->OpResult<()> {
    if realm.font_preloads.overflow.get(){return Err(OpError::new("QuotaExceededError","Font policy reporting budget exhausted"))}
    let violations=std::mem::take(&mut *realm.font_preloads.violations.borrow_mut());
    for violation in violations{realm.queue_csp_violation(ctx,violation,None)?;}
    Ok(())
}
pub(crate) fn install(realm:&Rc<DomRealm>) {
    let weak=Rc::downgrade(realm);
    realm.add_mutation_sink(Rc::new(move|_,_|{
        if let Some(realm)=weak.upgrade(){realm.font_preloads.dirty.set(true);}
    }));
}

fn report_decision(ctx:&mut Ctx,realm:&Rc<DomRealm>,decision:Result<lumen_common::csp::Decision,lumen_common::csp::Error>,node:NodeId)->bool {
    let Ok(decision)=decision else{return false};
    let allowed=!decision.blocked;
    for violation in decision.violations {
        if realm.queue_csp_violation(ctx,violation,Some(node)).is_err(){return false;}
    }
    allowed
}

fn complete(ctx:&mut Ctx,weak:std::rc::Weak<DomRealm>,node:NodeId,generation:u64,success:bool) {
    let failure_owner=weak.clone();
    if scheduling::queue_task(ctx,move|ctx|{
        let Some(realm)=weak.upgrade()else{return Ok(())};
        let current={
            let base=lumen_common::url::parse_url(&realm.base_url(),None);
            let session=realm.session.borrow();
            select(session.document(),node,base.as_ref())?
        };
        let root={
            let mut entries=realm.font_preloads.entries.borrow_mut();
            let Some(entry)=entries.get_mut(&node)else{return Ok(())};
            if entry.generation!=generation{return Ok(())}
            entry.control=None;
            entry.body=None;
            let root=entry.root.take();
            if current.as_ref()!=Some(&entry.selection){return Ok(())}
            root
        };
        if root.is_some(){realm.dispatch_user_agent(ctx,node,if success{"load"}else{"error"},false,false,&[])?;}
        Ok(())
    }).is_err() {
        if let Some(realm)=failure_owner.upgrade(){realm.font_preloads.overflow.set(true);}
    }
}

fn select(document:&lumen_html::Document,node:NodeId,base:Option<&lumen_common::url::Url>)->OpResult<Option<Selection>> {
    if !matches!(document.kind(node),Ok(NodeKind::Element{name,namespace:Namespace::Html,..}) if name.eq_ignore_ascii_case("link"))
        || !script_loading::is_connected(document,node){return Ok(None)}
    let attr=|name|document.get_attribute_ns_ref(node,None,name).ok().flatten();
    if !attr("rel").unwrap_or("").split_ascii_whitespace().any(|token|token.eq_ignore_ascii_case("preload"))
        || !attr("as").unwrap_or("").eq_ignore_ascii_case("font"){return Ok(None)}
    let Some(href)=attr("href").filter(|href|!href.is_empty())else{return Ok(None)};
    if href.len()>8192{return Err(OpError::new("QuotaExceededError","Font preload URL exceeds bounds"))}
    let Some(mut url)=lumen_common::url::parse_url(href,base)else{return Ok(None)};url.set_hash("");
    let crossorigin=attr("crossorigin").map(|value|if value.eq_ignore_ascii_case("use-credentials"){"use-credentials".to_owned()}else{String::new()});
    let integrity=attr("integrity").unwrap_or("").to_owned();
    if integrity.len()>8192{return Err(OpError::new("QuotaExceededError","Font preload integrity exceeds bounds"))}
    Ok(Some(Selection{url:url.href(),crossorigin,integrity}))
}

pub(crate) fn pump(ctx:&mut Ctx,realm:&Rc<DomRealm>)->OpResult<usize> {
    if !realm.font_preloads.dirty.replace(false){return Ok(0)}
    let base=realm.base_url();
    let base=lumen_common::url::parse_url(&base,None);
    let selections={
        let session=realm.session.borrow();let document=session.document();
        let mut selections=HashMap::new();let mut node=document.root();
        while let Ok(Some(next))=selector::next_descendant(document,document.root(),node) {
            node=next;
            let Some(selection)=select(document,node,base.as_ref())? else{continue};
            if selections.len()>=MAX_REQUESTS{return Err(OpError::new("QuotaExceededError","Font preload request budget exhausted"))}
            selections.insert(node,selection);
        }
        selections
    };
    let obsolete:Vec<_>=realm.font_preloads.entries.borrow().iter().filter(|(node,entry)|selections.get(node)!=Some(&entry.selection)).map(|(node,_)|*node).collect();
    for node in obsolete {
        let entry=realm.font_preloads.entries.borrow_mut().remove(&node);
        if let Some(entry)=entry {
            if let Some(control)=entry.control {control.abort(ctx);}
            if let Some(body)=entry.body {body.cancel(ctx);}
        }
    }
    let mut count=0;
    for (node,selection) in selections {
        if realm.font_preloads.entries.borrow().contains_key(&node){continue}
        if realm.font_preloads.entries.borrow().values().filter(|entry|entry.root.is_some()).count()>=MAX_ACTIVE {
            realm.font_preloads.dirty.set(true);continue;
        }
        let generation=realm.font_preloads.next.get().checked_add(1).ok_or_else(||OpError::new("QuotaExceededError","Font preload generation exhausted"))?;
        realm.font_preloads.next.set(generation);
        let root=realm.retain_resource_request(ctx,node);
        realm.font_preloads.entries.borrow_mut().insert(node,Entry{selection:selection.clone(),generation,control:None,body:None,root:Some(root)});
        count+=1;
        let policies:Arc<PolicySet>=realm.module_fetch_policy_snapshot()?;
        let document_url=realm.document_url().unwrap_or_else(||"about:blank".into());
        let weak=Rc::downgrade(realm);let original=selection.url.clone();
        let policy={let policies=policies.clone();let document_url=document_url.clone();Rc::new(move|ctx:&mut Ctx,url:&str,redirected:bool|{
            let Some(realm)=weak.upgrade()else{return false};
            report_decision(ctx,&realm,policies.check_resource_redirect(&original,url,&document_url,Destination::Font,"","",false,u32::from(redirected)),node)
        })};
        let (mode,credentials)=match selection.crossorigin.as_deref() {
            None=>(Mode::NoCors,Credentials::Include),
            Some(value) if value.eq_ignore_ascii_case("use-credentials")=>(Mode::Cors,Credentials::Include),
            Some(_)=>(Mode::Cors,Credentials::SameOrigin),
        };
        let spec=RequestSpec{method:"GET".into(),url:selection.url.clone(),headers:Vec::new(),body:None,mode,credentials,redirect:Redirect::Follow,observe_upload:false,force_preflight:false};
        let weak=Rc::downgrade(realm);let original=selection.url.clone();
        let control=net::start_resource(ctx,&document_url.clone(),spec,policy,move|ctx,result|{
            let Some(realm)=weak.upgrade()else{if let Ok(response)=result {response.body.cancel(ctx);}return};
            let Ok(response)=result else{cache_result(&realm,selection,Err("Font preload network failure".into()),String::new(),false);complete(ctx,weak,node,generation,false);return};
            let current=realm.font_preloads.entries.borrow().get(&node).is_some_and(|entry|entry.generation==generation);
            if !current {response.body.cancel(ctx);return}
            let allowed=report_decision(ctx,&realm,policies.check_resource_response(&original,&response.url,&document_url,Destination::Font,"","",false,u32::from(response.redirected)),node);
            if !allowed {response.body.cancel(ctx);cache_result(&realm,selection,Err("Font preload blocked by document policy".into()),String::new(),false);complete(ctx,weak,node,generation,false);return}
            let integrity_eligible=response.kind!=net::ResponseKind::Opaque || selection.integrity.is_empty();
            let final_url=response.url;let redirected=response.redirected;
            let body_weak=weak.clone();
            let body=net::consume_resource_body(ctx,response.body,MAX_BYTES,move|ctx,result|{
                let Some(realm)=weak.upgrade()else{return};
                if !realm.font_preloads.entries.borrow().get(&node).is_some_and(|entry|entry.generation==generation){return}
                let result=match result {
                    Ok(bytes) if integrity_eligible&&lumen_common::integrity::matches(&bytes,&selection.integrity)=>Ok(bytes),
                    Ok(_)=>Err("Font preload integrity verification failed".into()),
                    Err(_)=>Err("Font preload response body failed".into()),
                };
                let success=cache_result(&realm,selection,result,final_url,redirected);complete(ctx,weak,node,generation,success);
            });
            if let Some(realm)=body_weak.upgrade() {
                if let Some(entry)=realm.font_preloads.entries.borrow_mut().get_mut(&node) {
                    if entry.generation==generation {entry.body=body;}
                }
            }
        });
        if let Some(entry)=realm.font_preloads.entries.borrow_mut().get_mut(&node){entry.control=Some(control);}
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn drain(runtime:&mut lumen_runtime::Runtime,realm:&Rc<DomRealm>) {
        for _ in 0..5 {
            realm.queue_font_tasks(runtime.engine().ctx()).expect("font tasks");
            runtime.run_until_idle();
            assert!(scheduling::run_tasks(runtime.engine(),64).is_empty());
        }
    }
    #[test]
    fn font_preloads_fetch_real_bytes_gate_redirects_and_dispatch_generation_events() {
        let mut runtime=lumen_runtime::Runtime::new_browser();
        let realm=crate::install(runtime.engine().ctx(),"<head></head><body></body>",256).unwrap();
        realm.set_document_url("https://example.test/page");
        realm.set_content_security_policy_headers(&[("Content-Security-Policy".into(),"font-src https://allowed.test".into())]).unwrap();
        let transport=runtime.engine().eval_value(r#"globalThis.requests=[];globalThis.events=[];globalThis.violations=[];
          document.addEventListener('securitypolicyviolation',e=>violations.push(e.blockedURI));
          ({request(method,url,headers,body,resolve,reject,redirect,options){
            requests.push({url,options});
            if(url.endsWith('/redirect'))resolve({status:302,url,headers:[['location','https://denied.test/font.ttf']],body:new Uint8Array()});
            else resolve({status:200,url,headers:[],body:new Uint8Array([1,2,3])});
            return {abort(){}};
          }})"#).unwrap().ok().expect("transport");
        net::Transport::install(runtime.engine().ctx(),transport,Value::Undefined,Value::Undefined).unwrap();
        assert!(matches!(runtime.engine().eval_value(r#"
          for (const [id,url,cors] of [['allowed','https://allowed.test/font.ttf',false],['blocked','https://denied.test/font.ttf',false],['redirect','https://allowed.test/redirect',false],['cors','https://allowed.test/cors.ttf',true]]) {
            let link=document.createElement('link');link.id=id;link.rel='preload';link.as='font';link.href=url;
            if(cors)link.crossOrigin='anonymous';
            link.onload=()=>events.push(id+':load');link.onerror=()=>events.push(id+':error');document.head.append(link);
          }
          document.getElementById('allowed').as==='font'
        "#),Ok(Ok(Value::Bool(true)))));
        drain(&mut runtime,&realm);
        let verdict=runtime.engine().eval_value(r#"requests.length===3 && !requests.some(r=>r.url.startsWith('https://denied.test/')) &&
          requests.find(r=>r.url.endsWith('/font.ttf')).options.mode==='no-cors' &&
          requests.find(r=>r.url.endsWith('/cors.ttf')).options.mode==='cors' &&
          events.includes('allowed:load') && events.includes('blocked:error') && events.includes('redirect:error') && events.includes('cors:error') && events.length===4 && violations.length>=2"#);
        let details=match runtime.engine().eval_value("JSON.stringify({requests,events,violations})"){Ok(Ok(Value::Str(value)))=>value.to_string(),_=>String::new()};
        assert!(matches!(verdict,Ok(Ok(Value::Bool(true)))),"{details}");
        drain(&mut runtime,&realm);
        assert!(matches!(runtime.engine().eval_value("events.length===4 && requests.length===3"),Ok(Ok(Value::Bool(true)))));
    }
    #[test]
    fn internal_resource_body_budget_cancels_before_unbounded_accumulation() {
        let mut runtime=lumen_runtime::Runtime::new_browser();let result=Rc::new(Cell::new(false));
        let observed=result.clone();
        let _=net::consume_resource_body(runtime.engine().ctx(),net::ResponseBody::Bytes(vec![0;5]),4,move|_,value|observed.set(value.is_err()));
        assert!(result.get());
    }
}
