use lumen_runtime::{Completion, Runtime};
use std::{io::{Read, Write}, net::TcpListener, thread, time::Duration};

fn evaluate(runtime: &mut Runtime, source: &str) {
    match runtime.eval(source).expect("script parses") {
        Completion::Value(_) => (),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

fn assert_script(runtime: &mut Runtime, expression: &str) {
    evaluate(runtime, &format!("if (!({expression})) throw new Error('browser contract failed: {}')", expression.replace('\'', "\\'")));
}

#[test]
fn browser_rejection_caught_before_delivery_has_no_handled_notification() {
    let mut runtime = Runtime::new();
    runtime.enable_browser_rejection_events();
    evaluate(&mut runtime, "globalThis.deliveryPromise = Promise.reject({marker: 1});");
    let pending = runtime.take_browser_rejection_events();
    assert_eq!(pending.len(), 1, "checkpoint collects the rejected promise");
    evaluate(&mut runtime, "deliveryPromise.catch(() => {});");
    assert!(runtime.take_browser_rejection_events().is_empty(),
        "a promise that has never delivered unhandledrejection cannot emit rejectionhandled");
    drop(pending);
}

fn pending_browser_rejection(runtime: &mut Runtime) -> (lumen::embed::RealmHandle, lumen::embed::Value, lumen::embed::Value) {
    let mut pending = runtime.take_browser_rejection_events();
    assert_eq!(pending.len(), 1);
    match pending.pop().unwrap() {
        lumen_runtime::BrowserRejectionEvent::Unhandled { owner, promise, reason, .. } => (owner, promise, reason),
        _ => panic!("expected pending unhandled notification"),
    }
}

#[test]
fn browser_rejection_later_catch_reports_delivered_identity_and_reason_once() {
    let mut runtime = Runtime::new();
    runtime.enable_browser_rejection_events();
    evaluate(&mut runtime, "globalThis.deliveryReason = {marker: 2}; globalThis.deliveryPromise = Promise.reject(deliveryReason);");
    let (owner, promise, reason) = pending_browser_rejection(&mut runtime);
    let delivery = runtime.browser_rejection_delivery();
    assert!(delivery.should_dispatch(runtime.engine().ctx(), &owner, true, &promise));
    delivery.did_dispatch_unhandled(runtime.engine().ctx(), &owner, &promise);
    evaluate(&mut runtime, "deliveryPromise.catch(() => {});");
    let mut notifications = runtime.take_browser_rejection_events();
    assert_eq!(notifications.len(), 1);
    match notifications.pop().unwrap() {
        lumen_runtime::BrowserRejectionEvent::Handled { owner: handled_owner, promise: handled, reason: actual } => {
            assert!(handled_owner.same_realm(&owner));
            assert_eq!(handled.object_identity(), promise.object_identity());
            assert_eq!(actual.object_identity(), reason.object_identity());
        }
        _ => panic!("expected genuine handled notification"),
    }
    evaluate(&mut runtime, "deliveryPromise.catch(() => {});");
    assert!(runtime.take_browser_rejection_events().is_empty());
}

#[test]
fn browser_rejection_catch_during_delivery_does_not_publish_outstanding() {
    let mut runtime = Runtime::new();
    runtime.enable_browser_rejection_events();
    evaluate(&mut runtime, "globalThis.deliveryPromise = Promise.reject({marker: 3});");
    let (owner, promise, _) = pending_browser_rejection(&mut runtime);
    let delivery = runtime.browser_rejection_delivery();
    assert!(delivery.should_dispatch(runtime.engine().ctx(), &owner, true, &promise));
    evaluate(&mut runtime, "deliveryPromise.catch(() => {});");
    delivery.did_dispatch_unhandled(runtime.engine().ctx(), &owner, &promise);
    evaluate(&mut runtime, "0;");
    assert!(runtime.take_browser_rejection_events().is_empty());
}

#[test]
fn browser_rejection_delivered_weak_tracking_releases_reason_back_reference() {
    let mut runtime = Runtime::new();
    runtime.enable_browser_rejection_events();
    evaluate(&mut runtime, "globalThis.deliveryReason = {}; globalThis.deliveryPromise = Promise.reject(deliveryReason); deliveryReason.promise = deliveryPromise;");
    let (owner, promise, reason) = pending_browser_rejection(&mut runtime);
    let weak_promise = runtime.engine().ctx().weak_value(&promise).unwrap();
    let weak_reason = runtime.engine().ctx().weak_value(&reason).unwrap();
    let delivery = runtime.browser_rejection_delivery();
    assert!(delivery.should_dispatch(runtime.engine().ctx(), &owner, true, &promise));
    delivery.did_dispatch_unhandled(runtime.engine().ctx(), &owner, &promise);
    drop(promise);
    drop(reason);
    evaluate(&mut runtime, "deliveryPromise = null; deliveryReason = null;");
    runtime.engine().collect_garbage();
    runtime.engine().collect_garbage();
    assert!(weak_promise.upgrade().is_none(), "outstanding tracking must be genuinely weak");
    assert!(weak_reason.upgrade().is_none(), "a reason back-reference must not retain the promise graph");
}

#[test]
fn browser_rejection_retirement_invalidates_already_taken_delivery() {
    let mut runtime = Runtime::new();
    runtime.enable_browser_rejection_events();
    evaluate(&mut runtime, "globalThis.deliveryPromise = Promise.reject({marker: 4});");
    let (owner, promise, _) = pending_browser_rejection(&mut runtime);
    let delivery = runtime.browser_rejection_delivery();
    runtime.cancel_browser_rejection_events_for_realm(&owner);
    assert!(!delivery.should_dispatch(runtime.engine().ctx(), &owner, true, &promise));
    delivery.did_dispatch_unhandled(runtime.engine().ctx(), &owner, &promise);
    evaluate(&mut runtime, "deliveryPromise.catch(() => {});");
    assert!(runtime.take_browser_rejection_events().is_empty());
}

fn server(delay: Duration) -> (String, thread::JoinHandle<String>) {
    payload_server(delay, "application/json", b"{\"answer\":42}".to_vec())
}

fn payload_server(delay: Duration, mime: &str, body: Vec<u8>) -> (String, thread::JoinHandle<String>) {
    let mime = mime.to_string();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/data", listener.local_addr().unwrap());
    let task = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut received = Vec::new();
        let mut byte = [0; 1];
        while !received.ends_with(b"\r\n\r\n") {
            assert_eq!(stream.read(&mut byte).unwrap(), 1);
            received.push(byte[0]);
        }
        let request_headers = String::from_utf8(received.clone()).unwrap();
        let length = request_headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
        }).unwrap_or(0);
        let mut request_body = vec![0; length];
        stream.read_exact(&mut request_body).unwrap();
        received.extend(request_body);
        thread::sleep(delay);
        let headers = format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nX-Test: yes\r\nSet-Cookie: secret=1\r\nConnection: close\r\n\r\n", body.len());
        let _ = stream.write_all(headers.as_bytes());
        let _ = stream.write_all(&body);
        String::from_utf8(received).unwrap()
    });
    (url, task)
}

#[test]
fn xhr_upload_events_follow_written_bytes_and_finish_before_response_headers() {
    let (url, task) = server(Duration::from_millis(150));
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, &format!(r#"
        var uploadXhr=new XMLHttpRequest(),uploadEvents=[];
        uploadXhr.open('POST','{url}');
        for(const type of ['loadstart','progress','load','loadend'])
          uploadXhr.upload.addEventListener(type,e=>uploadEvents.push(type+':'+e.loaded+':'+e.total+':'+e.lengthComputable));
        uploadXhr.onreadystatechange=()=>{{if(uploadXhr.readyState===2)uploadEvents.push('headers')}};
        uploadXhr.send('héllo');
    "#));
    assert_script(&mut runtime, "uploadEvents.join(',')==='loadstart:0:6:true,progress:6:6:true,load:6:6:true,loadend:6:6:true,headers' && uploadXhr.status===200");
    assert!(task.join().unwrap().ends_with("héllo"));
}

#[test]
fn xhr_completed_upload_does_not_emit_upload_timeout_for_a_slow_response() {
    let (url, task) = server(Duration::from_millis(350));
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, &format!(r#"
        var slowUpload=new XMLHttpRequest(),slowEvents=[];
        slowUpload.open('POST','{url}');slowUpload.timeout=200;
        for(const type of ['load','timeout','error','loadend'])
          slowUpload.upload.addEventListener(type,()=>slowEvents.push('upload:'+type));
        slowUpload.ontimeout=()=>slowEvents.push('request:timeout');slowUpload.send('bytes');
    "#));
    assert_script(&mut runtime, "slowEvents.join(',')==='upload:load,upload:loadend,request:timeout' && slowUpload.status===0");
    assert!(task.join().unwrap().ends_with("bytes"));
}

#[test]
fn xhr_abort_from_upload_loadstart_cancels_before_transport_admission() {
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, r#"
        var cancelledUpload=new XMLHttpRequest(),cancelledEvents=[];
        cancelledUpload.open('POST','http://127.0.0.1:9/unreachable');
        cancelledUpload.upload.onloadstart=()=>{cancelledEvents.push('start');cancelledUpload.abort()};
        cancelledUpload.upload.onabort=()=>cancelledEvents.push('upload:abort');
        cancelledUpload.upload.onloadend=()=>cancelledEvents.push('upload:end');
        cancelledUpload.onabort=()=>cancelledEvents.push('request:abort');
        cancelledUpload.onloadend=()=>cancelledEvents.push('request:end');
        cancelledUpload.send('bytes');
    "#);
    assert_script(&mut runtime, "cancelledEvents.join(',')==='start,upload:abort,upload:end,request:abort,request:end' && cancelledUpload.readyState===0 && cancelledUpload.status===0");
}

#[test]
fn xhr_xml_mime_charset_override_and_parse_failure_use_real_dom_parser() {
    let mut runtime = Runtime::new();
    let _realm = lumen_html_js::install(runtime.engine().ctx(), "<body></body>", 64).unwrap();
    let (url, task) = payload_server(Duration::ZERO, "application/example+xml;charset=windows-1252", b"<root>caf\xe9</root>".to_vec());
    evaluate(&mut runtime, &format!("var xml=new XMLHttpRequest();xml.open('GET','{url}');xml.send()"));
    assert_script(&mut runtime, "xml.responseXML.documentElement.textContent==='café' && xml.responseXML===xml.responseXML");
    task.join().unwrap();
    let (url, task) = payload_server(Duration::ZERO, "text/plain;charset=windows-1252", b"<root>caf\xe9</root>".to_vec());
    evaluate(&mut runtime, &format!("xml.open('GET','{url}');xml.overrideMimeType('application/xml');xml.send()"));
    assert_script(&mut runtime, "xml.responseText==='<root>café</root>' && xml.responseXML.documentElement.textContent==='café'");
    task.join().unwrap();
    let (url, task) = payload_server(Duration::ZERO, "application/xml", b"<root><broken></root>".to_vec());
    evaluate(&mut runtime, &format!("xml.open('GET','{url}');xml.send()"));
    assert_script(&mut runtime, "xml.status===200 && xml.responseXML===null && xml.responseText==='<root><broken></root>'");
    task.join().unwrap();
}

#[test]
fn xhr_state_headers_json_and_real_transport() {
    let (url, task) = server(Duration::ZERO);
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, &format!(r#"
        var xhr = new XMLHttpRequest(); var events = []; var headers;
        xhr.onreadystatechange = () => {{ events.push(xhr.readyState); if(xhr.readyState===2) headers=xhr.getResponseHeader('X-Test'); }};
        xhr.onload = e => {{ events.push('load:'+e.loaded); }};
        xhr.onloadend = () => events.push('end');
        xhr.open('get', '{url}'); xhr.responseType='json';
        xhr.setRequestHeader('X-Request','first'); xhr.setRequestHeader('X-Request','second');
        xhr.setRequestHeader('Cookie','ignored'); xhr.send();
    "#));
    evaluate(&mut runtime, "if(events.join(',')!=='1,2,3,4,load:13,end'||xhr.status!==200)throw new Error('XHR events: '+events.join(',')+' status '+xhr.status+' text '+JSON.stringify(xhr._bytes))");
    assert_script(&mut runtime, "headers==='yes' && xhr.response.answer===42 && xhr.response===xhr.response && xhr.getResponseHeader('set-cookie')===null && !xhr.getAllResponseHeaders().includes('secret')");
    let request = task.join().unwrap().to_lowercase();
    assert!(request.contains("x-request: first, second\r\n"), "{request}");
    assert!(!request.contains("cookie: ignored"));
}

#[test]
fn xhr_timeout_ignores_late_transport_completion() {
    let (url, task) = server(Duration::from_millis(60));
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, &format!(r#"
        var xhr=new XMLHttpRequest();var events=[];
        xhr.open('GET','{url}');xhr.timeout=5;
        for(const type of ['timeout','load','error','loadend'])xhr.addEventListener(type,()=>events.push(type));
        xhr.send();
    "#));
    assert_script(&mut runtime, "events.join(',')==='timeout,loadend' && xhr.readyState===4 && xhr.status===0 && xhr.responseText===''");
    task.join().unwrap();
}

#[test]
fn xhr_abort_and_reopen_are_safe_during_event_delivery() {
    let (url, task) = server(Duration::ZERO);
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, &format!(r#"
        var xhr=new XMLHttpRequest();var events=[];
        xhr.open('GET','{url}');
        xhr.onreadystatechange=()=>{{if(xhr.readyState===2)xhr.abort()}};
        for(const type of ['abort','load','loadend'])xhr.addEventListener(type,()=>events.push(type));
        xhr.send();
    "#));
    assert_script(&mut runtime, "events.join(',')==='abort,loadend' && xhr.readyState===0 && xhr.status===0");
    task.join().unwrap();
}

#[test]
fn xhr_validation_and_progress_interfaces() {
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, r#"
        var xhr=new XMLHttpRequest();var errors=[];
        for(const fn of [()=>xhr.send(),()=>xhr.open('TRACE','http://localhost')]) {
          try{fn()}catch(e){errors.push(e.name)}
        }
        xhr.open('GET','http://localhost');xhr.responseType='arraybuffer';
        try{void xhr.responseText}catch(e){errors.push(e.name)}
        var p=new ProgressEvent('progress',{loaded:12,total:20,lengthComputable:true});
    "#);
    assert_script(&mut runtime, "errors.join(',')==='InvalidStateError,SecurityError,InvalidStateError' && p instanceof Event && p.loaded===12 && p.total===20 && p.lengthComputable && xhr.upload instanceof XMLHttpRequestUpload && xhr.DONE===XMLHttpRequest.DONE");
}

#[test]
fn synchronous_xhr_returns_binary_body_before_send_returns() {
    let (url, task) = payload_server(Duration::ZERO, "application/octet-stream", vec![0, 255, 65]);
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, &format!(r#"
        var syncXhr=new XMLHttpRequest(),syncEvents=[];
        syncXhr.open('POST','{url}',false);syncXhr.responseType='arraybuffer';
        syncXhr.onload=e=>syncEvents.push('load:'+e.loaded+':'+e.total+':'+e.lengthComputable);
        syncXhr.onloadend=()=>syncEvents.push('end');
        syncXhr.upload.onload=()=>syncEvents.push('upload');
        syncXhr.send(new Blob(['héllo']));
        var syncReturned=syncXhr.readyState===4 && syncXhr.status===200;
    "#));
    assert_script(&mut runtime, "syncReturned && Array.from(new Uint8Array(syncXhr.response)).join(',')==='0,255,65' && syncEvents.join(',')==='load:3:3:true,end'");
    assert!(task.join().unwrap().ends_with("héllo"));
}

#[test]
fn synchronous_xhr_timeout_throws_without_dispatching_completion_events() {
    let (url, task) = server(Duration::from_millis(150));
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, &format!(r#"
        var syncTimed=new XMLHttpRequest(),syncFailure='',syncFailureEvents=[];
        syncTimed.open('GET','{url}',false);syncTimed.timeout=50;
        for(const type of ['load','error','timeout','loadend'])syncTimed.addEventListener(type,()=>syncFailureEvents.push(type));
        try{{syncTimed.send()}}catch(e){{syncFailure=e.name}}
    "#));
    assert_script(&mut runtime, "syncFailure==='TimeoutError' && syncTimed.readyState===4 && syncTimed.status===0 && syncFailureEvents.length===0");
    task.join().unwrap();
}

#[test]
fn response_body_consumption_waits_for_async_chunks_and_releases_reader() {
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, r#"
        var source = new ReadableStream({start(controller) {
          controller.enqueue(new Uint8Array([65]));
          setTimeout(()=>{controller.enqueue(new Uint8Array([66]));controller.close()}, 5);
        }});
        var response = new Response(source), bodyResult='pending';
        response.text().then(text=>bodyResult=text);
    "#);
    assert_script(&mut runtime, "bodyResult==='AB' && response.bodyUsed && !source.locked");
    evaluate(&mut runtime, r#"
        var broken = new ReadableStream({start(controller){controller.enqueue('invalid');controller.close()}});
        var failure='pending';new Response(broken).bytes().catch(error=>failure=error.name);
    "#);
    assert_script(&mut runtime, "failure==='TypeError' && !broken.locked");
}

#[test]
fn fetch_headers_arrive_before_body_and_stream_clones_preserve_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/stream", listener.local_addr().unwrap());
    let (release, ready) = std::sync::mpsc::channel();
    let task = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut request = Vec::new();
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") { assert_eq!(stream.read(&mut byte).unwrap(), 1); request.push(byte[0]); }
        stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        stream.write_all(b"2\r\nAB\r\n").unwrap();
        thread::sleep(Duration::from_millis(15));
        stream.write_all(b"2\r\nCD\r\n0\r\nX-Trailer: yes\r\n\r\n").unwrap();
    });
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, &format!("var streamed;fetch('{url}').then(response=>streamed=response)"));
    assert_script(&mut runtime, "streamed.status===200 && !streamed.bodyUsed && !streamed.body.locked");
    release.send(()).unwrap();
    evaluate(&mut runtime, "var branch=streamed.clone(), originalText, clonedText;Promise.all([streamed.text(),branch.text()]).then(texts=>{originalText=texts[0];clonedText=texts[1]})");
    assert_script(&mut runtime, "originalText==='ABCD' && clonedText==='ABCD' && streamed.bodyUsed && branch.bodyUsed");
    task.join().unwrap();
    evaluate(&mut runtime, r#"
        var direct=new Response('abc'),reader=direct.body.getReader(),cloneFailure='';
        try{direct.clone()}catch(error){cloneFailure=error.name}
        var directChunk;reader.read().then(result=>directChunk=new TextDecoder().decode(result.value));
        var empty=new Response(),emptyTexts;Promise.all([empty.text(),empty.text()]).then(values=>emptyTexts=values.join('|'));
    "#);
    assert_script(&mut runtime, "cloneFailure==='TypeError' && direct.bodyUsed && directChunk==='abc' && !empty.bodyUsed && emptyTexts==='|'");
}

#[test]
fn browser_host_upload_uses_native_progress_and_preserves_binary_response() {
    let mut runtime = Runtime::new();
    let source = include_str!("../../lumen-wasm/js/host.js")
        .replace("export async function createRuntime", "async function createRuntime");
    evaluate(&mut runtime, &format!("{source}\nglobalThis.createUploadHost = createRuntime;"));
    evaluate(&mut runtime, r#"
        var uploadHost,nativeUpload,uploadHostEvents=[],completeSnapshot;
        class UploadSession {
          constructor(host){uploadHost=host}
          pushEvent(id,kind,args){completeSnapshot=uploadHost.fetchUploadProgress(id);uploadHostEvents.push([id,kind,args]);return {status:{idle:true,halted:false,nextTimerMs:null,pendingTasks:false}}}
        }
        class NativeUpload {
          constructor(){nativeUpload=this;this.upload={};this.response=new Uint8Array([0,255]).buffer}
          open(method,url){this.method=method;this.url=url}
          setRequestHeader(name,value){}
          getAllResponseHeaders(){return 'content-type: application/octet-stream\r\nx-result: yes\r\n'}
          send(body){this.sent=body}
          abort(){this.aborted=true}
        }
        createUploadHost({RuntimeSession:UploadSession,XMLHttpRequestImpl:NativeUpload,
          fetchImpl:()=>{throw new Error('Fetch cannot report upload progress')}});
        uploadHost.fetch(17,'POST','https://example.test/upload',[],new Uint8Array([65,66,67]),
          {mode:'cors',credentials:'include',redirect:'follow',uploadProgress:true});
        nativeUpload.upload.onprogress({loaded:2,total:3,lengthComputable:true});
    "#);
    assert_script(&mut runtime, "uploadHost.fetchUploadProgress(17).join(',')==='2,3,false' && nativeUpload.responseType==='arraybuffer' && nativeUpload.withCredentials && nativeUpload.sent[2]===67 && uploadHostEvents.length===0");
    evaluate(&mut runtime, "nativeUpload.upload.onload({loaded:3,total:3,lengthComputable:true});nativeUpload.status=200;nativeUpload.statusText='OK';nativeUpload.responseURL='https://example.test/upload';nativeUpload.onload()");
    assert_script(&mut runtime, "completeSnapshot.join(',')==='3,3,true' && uploadHostEvents[0][1]==='ok' && uploadHostEvents[0][2][3][0]===0 && uploadHostEvents[0][2][3][1]===255");
    evaluate(&mut runtime, "uploadHost.fetch(18,'POST','https://example.test/upload',[],new Uint8Array([65]),{mode:'cors',redirect:'follow',uploadProgress:true});uploadHost.fetchAbort(18)");
    assert_script(&mut runtime, "nativeUpload.aborted && uploadHostEvents.length===1");
}

#[test]
fn browser_host_reads_response_body_only_on_guest_demand() {
    let mut runtime = Runtime::new();
    let source = include_str!("../../lumen-wasm/js/host.js")
        .replace("export async function createRuntime", "async function createRuntime");
    evaluate(&mut runtime, &format!("{source}\nglobalThis.createStreamingHost = createRuntime;"));
    evaluate(&mut runtime, r#"
        var bridgeHost,bridgeEvents=[],bodyPulls=0;
        class StreamingSession {
          constructor(host){bridgeHost=host}
          pushEvent(id,kind,args){bridgeEvents.push([id,kind,args]);return {status:{idle:true,halted:false,nextTimerMs:null,pendingTasks:false}}}
        }
        createStreamingHost({RuntimeSession:StreamingSession,fetchImpl:()=>Promise.resolve({
          status:200,statusText:'OK',url:'http://example.test/',headers:new Headers(),
          body:new ReadableStream({pull(c){bodyPulls++;c.enqueue(new Uint8Array([65]));c.close()}},{highWaterMark:0})
        })});
        bridgeHost.fetch(7,'GET','http://example.test/',[],null);
    "#);
    assert_script(&mut runtime, "bodyPulls===0 && bridgeEvents.length===1 && bridgeEvents[0][1]==='ok' && bridgeEvents[0][2][3]===7");
    evaluate(&mut runtime, "bridgeHost.fetchRead(7,8)");
    assert_script(&mut runtime, "bodyPulls===1 && bridgeEvents[1][0]===8 && bridgeEvents[1][1]==='chunk' && bridgeEvents[1][2][0][0]===65");
    evaluate(&mut runtime, "bridgeHost.fetchRead(7,9)");
    assert_script(&mut runtime, "bridgeEvents[2][0]===9 && bridgeEvents[2][1]==='end'");
}

#[test]
fn fetch_prepares_async_request_body_and_cancels_pending_source() {
    let (url, task) = server(Duration::ZERO);
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, &format!(r#"
        var uploadResult='pending';
        var upload=new ReadableStream({{start(c){{c.enqueue(new Uint8Array([65]));setTimeout(()=>{{c.enqueue(new Uint8Array([66]));c.close()}},5)}}}});
        fetch('{url}',{{method:'POST',body:upload}}).then(r=>r.json()).then(value=>uploadResult=value.answer);
    "#));
    assert_script(&mut runtime, "uploadResult===42 && !upload.locked");
    assert!(task.join().unwrap().ends_with("\r\n\r\nAB"));
    evaluate(&mut runtime, r#"
        var pendingController=new AbortController(),cancelledSource=false,abortResult='pending';
        var pendingSource=new ReadableStream({cancel(reason){cancelledSource=reason.name==='AbortError'}});
        fetch('http://127.0.0.1:1/',{method:'POST',body:pendingSource,signal:pendingController.signal}).catch(error=>abortResult=error.name);
        setTimeout(()=>pendingController.abort(),5);
    "#);
    assert_script(&mut runtime, "abortResult==='AbortError' && cancelledSource && !pendingSource.locked");
}

/// The peer deliberately never responds. Cancellation must close its connection,
/// rather than merely dropping JavaScript callbacks while a worker waits for I/O.
fn stalled_server() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/stalled", listener.local_addr().unwrap());
    let task = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut request = Vec::new();
        let mut byte = [0; 1];
        while !request.ends_with(b"\r\n\r\n") {
            assert_eq!(stream.read(&mut byte).unwrap(), 1, "request must reach the server before abort");
            request.push(byte[0]);
        }
        assert_eq!(stream.read(&mut byte).unwrap(), 0, "abort must shut down the active socket");
    });
    (url, task)
}

#[test]
fn fetch_abort_closes_transport_and_removes_pending_task() {
    let (url, task) = stalled_server();
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, &format!(r#"
        var controller = new AbortController(), result = '';
        fetch('{url}', {{signal: controller.signal}}).then(
          () => result = 'unexpected response', error => result = error.name === 'AbortError' ? error.name : String(error) + '\n' + error.stack);
        setTimeout(() => controller.abort(), 100);
    "#));
    evaluate(&mut runtime, "if (result !== 'AbortError') throw new Error('abort result: ' + result)");
    task.join().unwrap();
}

#[test]
fn xhr_timeout_closes_transport() {
    let (url, task) = stalled_server();
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, &format!(r#"
        var xhr = new XMLHttpRequest(), result = [];
        xhr.open('GET', '{url}'); xhr.timeout = 100;
        xhr.ontimeout = () => result.push('timeout');
        xhr.onloadend = () => result.push('loadend');
        xhr.send();
    "#));
    assert_script(&mut runtime, "result.join(',') === 'timeout,loadend' && xhr.status === 0");
    task.join().unwrap();
}

#[test]
fn browser_host_abort_propagates_signal_and_discards_late_body() {
    let mut runtime = Runtime::new();
    let source = include_str!("../../lumen-wasm/js/host.js")
        .replace("export async function createRuntime", "async function createRuntime");
    evaluate(&mut runtime, &format!("{source}\nglobalThis.createTestBrowserRuntime = createRuntime;"));
    evaluate(&mut runtime, r#"
        var browserHost, hostEvents = [], hostSignal, finishBody;
        class FakeSession {
          constructor(host) { browserHost = host; }
          pushEvent(id, kind, args) { hostEvents.push([id, kind, args]); return {status:{idle:true,halted:false,nextTimerMs:null,pendingTasks:false}}; }
        }
        createTestBrowserRuntime({RuntimeSession:FakeSession, fetchImpl:(_url, init) => {
          hostSignal = init.signal;
          return Promise.resolve({status:200,statusText:'OK',url:'http://example.test/',headers:new Headers(),
            arrayBuffer:() => new Promise(resolve => finishBody = resolve)});
        }});
    "#);
    evaluate(&mut runtime, "browserHost.fetch(7, 'GET', 'http://example.test/', [], null)");
    assert_script(&mut runtime, "typeof finishBody === 'function' && !hostSignal.aborted");
    evaluate(&mut runtime, "browserHost.fetchAbort(7); finishBody(new ArrayBuffer(4))");
    assert_script(&mut runtime, "hostSignal.aborted && hostEvents.length === 0");
}

#[test]
fn browser_host_forwards_fetch_policy_and_preserves_opaque_metadata() {
    let mut runtime = Runtime::new();
    let source = include_str!("../../lumen-wasm/js/host.js")
        .replace("export async function createRuntime", "async function createRuntime");
    evaluate(&mut runtime, &format!("{source}\nglobalThis.createPolicyHost = createRuntime;"));
    evaluate(&mut runtime, r#"
        var policyHost, policyInit, policyEvents=[];
        class PolicySession {
          constructor(host){policyHost=host}
          pushEvent(id,kind,args){policyEvents.push([id,kind,args]);return {status:{idle:true,halted:false,nextTimerMs:null,pendingTasks:false}}}
        }
        createPolicyHost({RuntimeSession:PolicySession,fetchImpl:(_url,init)=>{
          policyInit=init;
          return Promise.resolve({status:0,statusText:'',url:'',redirected:false,type:'opaque',headers:new Headers(),body:null});
        }});
        policyHost.fetch(7,'GET','https://outside.test/',[],null,{mode:'no-cors',credentials:'omit',redirect:'manual'});
    "#);
    assert_script(&mut runtime,"policyInit.mode==='no-cors' && policyInit.credentials==='omit' && policyInit.redirect==='manual' && policyEvents.length===1 && policyEvents[0][2][0]===0 && policyEvents[0][2][2]==='' && policyEvents[0][2][4]===false && policyEvents[0][2][5]==='opaque'");
}

#[test]
fn text_codec_globals_keep_web_idl_shape() {
    let mut runtime = Runtime::new_browser();
    for expression in [
        "Object.getOwnPropertyDescriptor(globalThis, 'TextEncoder').writable && !Object.getOwnPropertyDescriptor(globalThis, 'TextEncoder').enumerable",
        "TextEncoder.length === 0 && TextDecoder.length === 0",
        "new TextEncoder().encoding === 'utf-8'",
        "Object.getOwnPropertyDescriptor(TextEncoder.prototype, 'encoding').enumerable",
        "Object.getOwnPropertyDescriptor(TextEncoder.prototype, 'encode').enumerable",
        "new TextEncoder().encode().length === 0 && new TextEncoder().encode(undefined).length === 0",
        "new TextEncoder().encode('h\\u00e9').join() === '104,195,169'",
        "(() => { const out = new Uint8Array(5); const r = new TextEncoder().encodeInto('a\\u{1F600}b', out); const short = new Uint8Array(4); const s = new TextEncoder().encodeInto('a\\u{1F600}b', short); return r.read === 3 && r.written === 5 && out[0] === 97 && s.read === 1 && s.written === 1; })()",
        "(() => { try { new TextEncoder().encodeInto('a', new Uint16Array(1)); } catch (e) { return e instanceof TypeError; } return false; })()",
        "new TextDecoder().decode(new Uint8Array([0xEF, 0xBB, 0xBF, 0x68])) === 'h'",
        "new TextDecoder('utf-8', { ignoreBOM: true }).decode(new Uint8Array([0xEF, 0xBB, 0xBF, 0x68])) === '\\uFEFFh'",
        "new TextDecoder('utf-8', { ignoreBOM: true }).ignoreBOM === true && new TextDecoder().fatal === false",
        "(() => { const d = new TextDecoder(); const a = d.decode(new Uint8Array([0xE2, 0x82]), { stream: true }); return a === '' && d.decode(new Uint8Array([0xAC])) === '\\u20AC'; })()",
        "(() => { try { new TextDecoder('utf-8', { fatal: true }).decode(new Uint8Array([0xFF])); } catch (e) { return e instanceof TypeError && e.code === 'ERR_ENCODING_INVALID_ENCODED_DATA'; } return false; })()",
        "(() => { try { new TextDecoder('nope'); } catch (e) { return e instanceof RangeError && e.code === 'ERR_ENCODING_NOT_SUPPORTED'; } return false; })()",
        "(() => { try { Object.getOwnPropertyDescriptor(TextDecoder.prototype, 'encoding').get.call({}); } catch (e) { return e.code === 'ERR_INVALID_THIS'; } return false; })()",
        "btoa('abc') === 'YWJj' && atob('YWJj') === 'abc' && btoa.length === 1 && atob.length === 1",
        "(() => { try { btoa('\\u0100'); } catch (e) { return e.name === 'InvalidCharacterError'; } return false; })()",
        "(() => { try { atob('*'); } catch (e) { return e.name === 'InvalidCharacterError'; } return false; })()",
        "typeof new TextDecoder()[Symbol.for('nodejs.util.inspect.custom')] === 'function'",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn web_crypto_globals_keep_web_idl_shape() {
    let mut runtime = Runtime::new_browser();
    for expression in [
        "crypto instanceof Crypto && Object.getOwnPropertyDescriptor(globalThis, 'crypto').enumerable",
        "crypto.subtle === crypto.subtle && crypto.subtle instanceof SubtleCrypto",
        "/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(crypto.randomUUID())",
        "(() => { const a = new Uint8Array(32); return crypto.getRandomValues(a) === a && a.some((b) => b !== 0); })()",
        "(() => { try { crypto.getRandomValues(new Float32Array(1)); } catch (e) { return e instanceof TypeError; } return false; })()",
        "(() => { try { crypto.getRandomValues(new Uint8Array(65537)); } catch (e) { return e.name === 'QuotaExceededError'; } return false; })()",
        "crypto.getRandomValues(new Uint8Array(65536)).length === 65536",
        "(() => { try { new Crypto(); } catch (e) { return e instanceof TypeError; } return false; })()",
        "(() => { try { new SubtleCrypto(); } catch (e) { return e instanceof TypeError; } return false; })()",
        "(() => { try { new CryptoKey(); } catch (e) { return e instanceof TypeError; } return false; })()",
        "crypto.subtle.digest('SHA-256', new Uint8Array(0)) instanceof Promise",
        "(() => { globalThis.digestResult = null; crypto.subtle.digest({ name: 'sha-256' }, new Uint8Array([97, 98, 99])).then((buffer) => { globalThis.digestResult = [...new Uint8Array(buffer)].map((b) => b.toString(16).padStart(2, '0')).join(''); }); return true; })()",
        "(() => { globalThis.digestError = null; crypto.subtle.digest('MD5', new Uint8Array(1)).catch((e) => { globalThis.digestError = e.name; }); return true; })()",
    ] {
        assert_script(&mut runtime, expression);
    }
    runtime.run_until_idle();
    assert_script(
        &mut runtime,
        "digestResult === 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad'",
    );
    assert_script(&mut runtime, "digestError === 'NotSupportedError'");
}
