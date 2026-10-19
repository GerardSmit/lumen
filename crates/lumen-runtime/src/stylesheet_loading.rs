//! Browser stylesheet transport on the existing blocking pool. Only captured
//! policy and byte/source data cross threads; HTML processing stays on its realm.
use std::cell::{Cell,RefCell};
use std::collections::HashMap;
use std::sync::{Arc,mpsc};
use lumen_os::net::TcpCancellation;
use lumen_html::stylesheet_loading::{self as graph,FetchContext,GraphBudget};
use lumen_html_js::stylesheet_loading::{StylesheetRequest,StylesheetResponse,StylesheetFailure,StylesheetResourceTiming,StylesheetResourceLoader};

type ResultData=Result<StylesheetResponse,StylesheetFailure>;
struct Ticket { result:mpsc::Receiver<ResultData>, cancelled:TcpCancellation }
pub(crate) struct Provider {
    pool:lumen_host::SpawnHandle,
    wake:super::RuntimeWaker,
    config:lumen_web::FetchConfig,
    next:Cell<u64>,
    tickets:RefCell<HashMap<u64,Ticket>>,
}
impl Provider {
    pub(crate) fn new(pool:lumen_host::SpawnHandle,wake:super::RuntimeWaker,config:lumen_web::FetchConfig)->Self {
        Self {pool,wake,config,next:Cell::new(0),tickets:RefCell::new(HashMap::new())}
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        for ticket in self.tickets.get_mut().values() {ticket.cancelled.cancel();}
    }
}
impl StylesheetResourceLoader for Provider {
    fn start(&self,request:StylesheetRequest)->Result<u64,String> {
        let id=self.next.get().checked_add(1).ok_or_else(||"stylesheet ticket space exhausted".to_owned())?;
        let mut tickets=self.tickets.borrow_mut();
        tickets.try_reserve(1).map_err(|_|"stylesheet ticket admission".to_owned())?;
        let (tx,result)=mpsc::channel();
        let cancelled=TcpCancellation::default();
        tickets.insert(id,Ticket {result,cancelled:cancelled.clone()});
        self.next.set(id);
        let config=self.config.clone();let wake=self.wake.clone();
        self.pool.spawn_detached(Box::new(move || {
            let result=fetch_graph(&request,&config,&cancelled);
            if !cancelled.is_cancelled() {let _=tx.send(result);wake.wake();}
        }));
        Ok(id)
    }
    fn poll(&self,id:u64)->Option<ResultData> {
        let mut tickets=self.tickets.borrow_mut();
        let ticket=tickets.get(&id)?;
        let result=match ticket.result.try_recv() {
            Ok(result)=>result,
            Err(mpsc::TryRecvError::Empty)=>return None,
            Err(mpsc::TryRecvError::Disconnected)=>Err(StylesheetFailure {message:"stylesheet transport stopped".into(),violations:Vec::new(),timings:Vec::new()}),
        };
        tickets.remove(&id);Some(result)
    }
    fn cancel(&self,id:u64) {
        if let Some(ticket)=self.tickets.borrow_mut().remove(&id) {ticket.cancelled.cancel();}
    }
}

/// Shared import traversal publishes a graph only after critical body loads.
/// Failed imported resources produce absent edges without failing a good root.
fn fetch_graph(request:&StylesheetRequest,config:&lumen_web::FetchConfig,cancelled:&TcpCancellation)->ResultData {
    if request.url.is_empty() {return Err(StylesheetFailure{message:"invalid stylesheet URL".into(),violations:Vec::new(),timings:Vec::new()})}
    let mut critical_failed=false;let mut root_type=None;let mut violations=Vec::new();let mut root_clean=false;let mut root_bytes=0u64;let mut timing_allow=false;let mut origins=Vec::new();let mut timings=Vec::new();
    let mut first_resource=true;
    let mut fetch=|url:&str,context:FetchContext<'_>| {
        if cancelled.is_cancelled() {return Ok(None)}
        let initial=core::mem::replace(&mut first_resource,false);
        let start_time=if context.root {request.start_time}else{lumen_host::perf::web_now_ms()};
        let mut metadata=lumen_web::ScriptFetchMetadata {
            destination:lumen_common::csp::Destination::Style,self_url:request.document_url.clone(),
            nonce:if context.root {request.nonce.clone()}else{String::new()},
            integrity:if context.root {request.integrity.clone()}else{String::new()},parser_inserted:context.root && request.parser_inserted,
            referrer:lumen_common::referrer::Referrer {source:context.referrer.into(),policy:context.referrer_policy},
            policies:request.policies.clone(),violations:Vec::new(),
        };
        let result=if lumen_common::url::parse(url,None).is_ok_and(|url|url.scheme=="data") {
            let decision=metadata.policies.check_resource_redirect(url,url,&metadata.self_url,metadata.destination,
                &metadata.nonce,&metadata.integrity,metadata.parser_inserted,0).map_err(|_|graph::Error::InvalidResponseUrl)?;
            metadata.violations.extend(decision.violations);
            if decision.blocked {None}else {
                let bytes=lumen_common::url::data_url_body_bounded(url,graph::MAX_GRAPH_BYTES)
                    .map_err(|_|graph::Error::ResourceLimit)?;
                let content_type=lumen_common::url::data_url_media_type(url).map(|value|value.into_owned());
                let size=bytes.len() as u64;
                if !lumen_common::integrity::matches(&bytes,&metadata.integrity) {None}else {
                    Some((lumen_common::http_body::SyncHttpResponse {status:200,status_text:"OK".into(),url:url.into(),
                        headers:content_type.clone().map(|value|vec![("Content-Type".into(),value)]).unwrap_or_default(),body:bytes},lumen_web::StylesheetResponsePolicy{origin_clean:true,timing_allowed:true,encoded_body_size:size,decoded_body_size:size}))
                }
            }
        }else {
            lumen_web::load_stylesheet_resource_with_config(url,&request.origin,if context.root {request.crossorigin}else{None},config,
                graph::MAX_GRAPH_BYTES,30_000,&mut metadata,Some(cancelled)).ok()
        };
        violations.extend(metadata.violations);
        let Some((response,policy))=result else {
            critical_failed=true;
            timings.push(StylesheetResourceTiming{initiator_type:if context.root {"link"}else{"css"},name:url.into(),start_time,end_time:lumen_host::perf::web_now_ms(),encoded_body_size:0,decoded_body_size:0,timing_allowed:false});
            return Ok(None)
        };
        let clean=policy.origin_clean;
        timings.push(StylesheetResourceTiming{initiator_type:if context.root {"link"}else{"css"},name:url.into(),start_time,end_time:lumen_host::perf::web_now_ms(),
            encoded_body_size:policy.encoded_body_size,decoded_body_size:policy.decoded_body_size,timing_allowed:policy.timing_allowed});
        if !(200..300).contains(&response.status) {critical_failed=true;return Ok(None)}
        let content_type=response.headers.iter().find(|(name,_)|name.eq_ignore_ascii_case("content-type")).map(|(_,value)|value.clone());
        let same_origin=lumen_common::url::parse(&response.url,None).is_ok_and(|url|url.origin()==request.origin)
            && lumen_common::url::parse(url,None).is_ok_and(|url|url.origin()==request.origin);
        if !lumen_common::mime::stylesheet_mime_allowed(content_type.as_deref(),request.quirks_mode,same_origin) {critical_failed=true;return Ok(None)}
        if context.root || request.import_request && initial {
            root_type=content_type.clone();root_clean=clean;root_bytes=policy.encoded_body_size;
            timing_allow=policy.timing_allowed;
        }
        origins.push((Arc::<str>::from(response.url.as_str()),clean));
        Ok(Some(graph::Response {final_url:response.url,content_type,bytes:response.body,
            referrer_policy:Some(metadata.referrer.policy)}))
    };
    let mut budget=GraphBudget::default();
    let context=FetchContext {referrer:&request.referrer,referrer_policy:request.referrer_policy,root:!request.import_request};
    let result=if let Some(source)=&request.inline_source {
        graph::load_inline_with_context(&request.url,source.clone(),request.environment_encoding,&mut budget,context,&mut fetch).map(Some)
    }else {graph::load_with_context(&request.url,request.environment_encoding,&mut budget,context,&mut fetch)};
    drop(fetch);critical_failed|=budget.critical_failed;
    if request.inline_source.is_some() {root_clean=true;}
    match result {
        Ok(Some(source))=>Ok(StylesheetResponse {critical_failed,failed_import_paths:budget.failed_import_paths,source_contexts:budget.source_contexts,location_url:request.url.clone(),content_type:root_type,source,origin_clean:root_clean,start_time:request.start_time,
            end_time:lumen_host::perf::web_now_ms(),encoded_body_size:root_bytes,timing_allow,origin_metadata:origins.into(),violations,timings}),
        Ok(None)=>Err(StylesheetFailure {message:"stylesheet resource failed".into(),violations,timings}),
        Err(error)=>Err(StylesheetFailure {message:format!("stylesheet graph: {error:?}"),violations,timings}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead,BufReader,Write};
    use std::net::TcpListener;
    #[test]
    fn specification_stylesheet_http_graph_preserves_cors_charset_redirect_bases_and_real_timing() {
        let first=TcpListener::bind("127.0.0.1:0").unwrap();let first_address=first.local_addr().unwrap();
        let second=TcpListener::bind("127.0.0.1:0").unwrap();let second_address=second.local_addr().unwrap();
        let redirect=std::thread::spawn(move || {
            let (mut stream,_)=first.accept().unwrap();let mut reader=BufReader::new(stream.try_clone().unwrap());
            let mut request=String::new();reader.read_line(&mut request).unwrap();
            loop {let mut line=String::new();reader.read_line(&mut line).unwrap();if line=="\r\n" {break}}
            assert!(request.starts_with("GET /root.css "));
            write!(stream,"HTTP/1.1 302 Found\r\nLocation: http://b.test/final/root.css\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let replies=std::thread::spawn(move || {
            let mut requests=Vec::new();
            for _ in 0..5 {
                let (mut stream,_)=second.accept().unwrap();let mut reader=BufReader::new(stream.try_clone().unwrap());
                let mut request=String::new();reader.read_line(&mut request).unwrap();let path=request.split_ascii_whitespace().nth(1).unwrap().to_owned();
                let mut headers=String::new();loop {let mut line=String::new();reader.read_line(&mut line).unwrap();if line=="\r\n" {break}headers.push_str(&line);}
                requests.push((path.clone(),headers));
                let (kind,body):(&str,&[u8])=match path.as_str() {
                    "/final/root.css"=>("text/css; charset=windows-1250",b"@import 'child.css'; .root{width:7px}"),
                    "/final/child.css"=>("text/css",b".child{content:'\x9e'}"),
                    "/wrong.css"=>("text/plain",b".wrong{width:99px}"),
                    _=>panic!("unexpected stylesheet path"),
                };
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nAccess-Control-Allow-Origin: http://a.test\r\nTiming-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();stream.write_all(body).unwrap();
            }
            requests
        });
        let mut config=lumen_web::FetchConfig::default();config.set_require_routes(true);
        config.set_route("a.test",80,first_address).unwrap();config.set_route("b.test",80,second_address).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let started=lumen_host::perf::web_now_ms();
        let request=StylesheetRequest{inline_source:None,import_request:false,url:"http://a.test/root.css".into(),document_url:"http://a.test/page".into(),referrer:"http://a.test/page".into(),origin:"http://a.test".into(),nonce:String::new(),parser_inserted:false,integrity:String::new(),crossorigin:Some(false),referrer_policy:lumen_common::referrer::ReferrerPolicy::StrictOriginWhenCrossOrigin,
            environment_encoding:"UTF-8",quirks_mode:false,policies:Arc::new(Default::default()),start_time:started};
        let response=fetch_graph(&request,&config,&TcpCancellation::default()).unwrap_or_else(|failure|panic!("stylesheet graph: {}",failure.message));
        assert!(response.origin_clean);assert!(response.timing_allow);assert_eq!(response.location_url,"http://a.test/root.css");
        assert_eq!(response.source.url.as_ref(),"http://b.test/final/root.css");
        let child=response.source.imports[0].source.as_ref().unwrap();assert_eq!(child.url.as_ref(),"http://b.test/final/child.css");
        assert!(child.text.contains('ž'),"child inherits root's effective transport encoding");
        assert_eq!(response.timings.len(),2);assert_eq!(response.timings[0].start_time,started);
        assert!(response.timings[1].start_time>=response.timings[0].start_time);assert!(response.timings.iter().all(|timing|timing.end_time>=timing.start_time));
        assert!(!response.origin_metadata[1].1,"import fetch is no-CORS even when root used CORS");
        assert_eq!(response.source_contexts.len(),2);
        assert_eq!(response.source_contexts[0].path,Vec::<usize>::new());assert_eq!(response.source_contexts[0].encoding,"windows-1250");
        assert_eq!(response.source_contexts[1].path,vec![0]);assert_eq!(response.source_contexts[1].encoding,"windows-1250");
        let mut imported=request.clone();imported.import_request=true;imported.url="http://b.test/final/root.css".into();imported.referrer="http://a.test/parent.css".into();
        // An import uses no-CORS even if the root link request used CORS.
        // Captured root nonce, integrity and credentials are not inherited.
        imported.nonce="root-only".into();imported.integrity="sha256-invalid-root-only".into();
        let loaded=fetch_graph(&imported,&config,&TcpCancellation::default()).unwrap_or_else(|failure|panic!("CSSOM import graph: {}",failure.message));
        assert!(!loaded.origin_clean);assert!(loaded.timings.iter().all(|timing|timing.initiator_type=="css"));
        assert!(loaded.source.imports[0].source.as_ref().unwrap().text.contains('ž'));

        let mut wrong=request.clone();wrong.url="http://b.test/wrong.css".into();wrong.quirks_mode=true;
        let failure=fetch_graph(&wrong,&config,&TcpCancellation::default()).err().expect("cross-origin clean response is not a same-origin quirks MIME exception");
        assert_eq!(failure.timings.len(),1);assert!(failure.timings[0].timing_allowed);
        redirect.join().unwrap();let requests=replies.join().unwrap();
        assert!(requests.iter().find(|(path,_)|path=="/final/root.css").unwrap().1.to_ascii_lowercase().contains("origin: http://a.test"));
        let imports=requests.iter().filter(|(path,_)|path=="/final/root.css").collect::<Vec<_>>();
        assert_eq!(imports.len(),2);assert!(!imports[1].1.to_ascii_lowercase().contains("origin:"));

        assert!(requests.iter().find(|(path,_)|path=="/final/child.css").unwrap().1.to_ascii_lowercase().contains("referer: http://b.test/final/root.css"));
    }    #[test]
    fn specification_inline_stylesheet_http_failure_keeps_good_root_and_reports_only_real_import_timing() {
        let server=TcpListener::bind("127.0.0.1:0").unwrap();let address=server.local_addr().unwrap();
        let response=std::thread::spawn(move || {
            let (mut stream,_)=server.accept().unwrap();let mut reader=BufReader::new(stream.try_clone().unwrap());
            let mut request=String::new();reader.read_line(&mut request).unwrap();assert!(request.starts_with("GET /missing.css "));
            loop {let mut line=String::new();reader.read_line(&mut line).unwrap();if line=="\r\n" {break}}
            write!(stream,"HTTP/1.1 404 Not Found\r\nContent-Type: text/css\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx").unwrap();
        });
        let mut config=lumen_web::FetchConfig::default();config.set_require_routes(true);config.set_route("inline.test",80,address).unwrap();
        let original:Arc<str>=Arc::from("@import '/missing.css';p{width:19px}");
        let request=StylesheetRequest{inline_source:Some(original.clone()),import_request:false,url:"http://inline.test/page".into(),document_url:"http://inline.test/page".into(),referrer:"http://inline.test/page".into(),origin:"http://inline.test".into(),nonce:String::new(),parser_inserted:true,integrity:String::new(),crossorigin:None,referrer_policy:Default::default(),environment_encoding:"UTF-8",quirks_mode:false,policies:Arc::new(Default::default()),start_time:lumen_host::perf::web_now_ms()};
        let loaded=fetch_graph(&request,&config,&TcpCancellation::default()).unwrap_or_else(|failure|panic!("inline root failed: {}",failure.message));
        assert!(loaded.critical_failed);assert!(loaded.origin_clean);assert!(Arc::ptr_eq(&original,&loaded.source.text));
        assert!(loaded.source.imports[0].source.is_none());assert_eq!(loaded.failed_import_paths,vec![vec![0]]);assert_eq!(loaded.timings.len(),1);
        assert_eq!(loaded.timings[0].name,"http://inline.test/missing.css");assert_eq!(loaded.timings[0].initiator_type,"css");
        assert!(loaded.timings[0].start_time>=request.start_time);assert_eq!(loaded.timings[0].encoded_body_size,1);
        response.join().unwrap();
    }

}
