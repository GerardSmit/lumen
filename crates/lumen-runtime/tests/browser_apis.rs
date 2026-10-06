use lumen_runtime::{Completion, Runtime};
use std::{io::{Read, Write}, net::TcpListener, thread, time::Duration};

fn evaluate(runtime: &mut Runtime, source: &str) {
    match runtime.eval(source).expect("script parses") {
        Completion::Value(_) => (),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

fn assert_script(runtime: &mut Runtime, expression: &str) {
    evaluate(runtime, &format!("if (!({expression})) throw new Error('browser contract failed: {}')", expression.replace('\\', "\\\\").replace('\'', "\\'").replace('\n', "\\n")));
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

#[test]
fn url_classes_keep_web_idl_shape() {
    let mut runtime = Runtime::new_browser();
    for expression in [
        r##"['URL', 'URLSearchParams'].every((name) => { const d = Object.getOwnPropertyDescriptor(globalThis, name); return d.writable && d.configurable && !d.enumerable; })"##,
        "URL.length === 1 && URLSearchParams.length === 0",
        "Object.prototype.toString.call(new URL('http://a/')) === '[object URL]'",
        "Object.prototype.toString.call(new URLSearchParams()) === '[object URLSearchParams]'",
        r##"['href', 'origin', 'protocol', 'username', 'password', 'host', 'hostname', 'port', 'pathname', 'search', 'searchParams', 'hash'].every((name) => { const d = Object.getOwnPropertyDescriptor(URL.prototype, name); return d.enumerable && d.configurable && typeof d.get === 'function'; })"##,
        r##"['href', 'protocol', 'username', 'password', 'host', 'hostname', 'port', 'pathname', 'search', 'hash'].every((name) => typeof Object.getOwnPropertyDescriptor(URL.prototype, name).set === 'function')"##,
        "['origin', 'searchParams'].every((name) => Object.getOwnPropertyDescriptor(URL.prototype, name).set === undefined)",
        r##"['toString', 'toJSON'].every((name) => Object.getOwnPropertyDescriptor(URL.prototype, name).enumerable)"##,
        r##"['parse', 'canParse', 'createObjectURL', 'revokeObjectURL'].every((name) => { const d = Object.getOwnPropertyDescriptor(URL, name); return d.enumerable && d.writable && d.configurable; })"##,
        r##"['append', 'delete', 'get', 'getAll', 'has', 'set', 'sort', 'entries', 'forEach', 'keys', 'values', 'toString', 'size'].every((name) => Object.getOwnPropertyDescriptor(URLSearchParams.prototype, name).enumerable)"##,
        "URLSearchParams.prototype[Symbol.iterator] === URLSearchParams.prototype.entries",
        "!Object.getOwnPropertyDescriptor(URLSearchParams.prototype, Symbol.iterator).enumerable",
        "URLSearchParams.prototype.append.length === 2 && URLSearchParams.prototype.delete.length === 1 && URLSearchParams.prototype.has.length === 1 && URLSearchParams.prototype.forEach.length === 1",
        "URL.parse.length === 1 && URL.canParse.length === 1",
        "(() => { class Mine extends URL {} const u = new Mine('http://a/x'); return u instanceof Mine && u.pathname === '/x'; })()",
        "(() => { try { URL('http://a/'); } catch (e) { return e instanceof TypeError; } return false; })()",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn url_getters_setters_and_statics() {
    let mut runtime = Runtime::new_browser();
    for expression in [
        r##"(() => { const u = new URL('HTTPS://user:pw@Example.COM:8443/a/b?x=1#frag'); return [u.href, u.origin, u.protocol, u.username, u.password, u.host, u.hostname, u.port, u.pathname, u.search, u.hash].join('|') === 'https://user:pw@example.com:8443/a/b?x=1#frag|https://example.com:8443|https:|user|pw|example.com:8443|example.com|8443|/a/b|?x=1|#frag'; })()"##,
        "String(new URL('http://a/?')) === 'http://a/?' && new URL('http://a/?').search === '' && new URL('http://a/#').hash === ''",
        "JSON.stringify({ u: new URL('http://a/b') }) === '{\"u\":\"http://a/b\"}' && new URL('http://a/b').toJSON() === 'http://a/b'",
        "new URL('../c?q', 'http://ex.com/a/b/').href === 'http://ex.com/a/c?q'",
        "new URL('about:blank').host === '' && new URL('file:///c/d').origin === 'null' && new URL('blob:https://ex.com/id').origin === 'https://ex.com'",
        "(() => { const u = new URL('http://a/'); u.protocol = 'https'; u.username = 'x y'; u.password = 'p'; u.hostname = 'b.org'; u.port = '99'; u.pathname = '/p q'; u.search = 'k=v w'; u.hash = 'h h'; return u.href === 'https://x%20y:p@b.org:99/p%20q?k=v%20w#h%20h'; })()",
        "(() => { const u = new URL('http://a:81/'); u.host = 'c.net:82'; const first = u.host; u.port = '80'; return first === 'c.net:82' && u.href === 'http://c.net/'; })()",
        "(() => { const u = new URL('http://a/'); u.port = 'bad'; u.hostname = ''; u.protocol = '1x'; return u.href === 'http://a/'; })()",
        "(() => { const u = new URL('http://a/'); try { u.href = 'not a url'; } catch (e) { return e instanceof TypeError && e.code === 'ERR_INVALID_URL' && e.input === 'not a url' && u.href === 'http://a/'; } return false; })()",
        "(() => { try { new URL('nope'); } catch (e) { return e instanceof TypeError && e.message === 'Invalid URL' && e.code === 'ERR_INVALID_URL' && e.input === 'nope' && !('base' in e); } return false; })()",
        "(() => { try { new URL('/x', 'also bad'); } catch (e) { return e.code === 'ERR_INVALID_URL' && e.input === '/x' && e.base === 'also bad'; } return false; })()",
        "(() => { try { new URL(); } catch (e) { return e instanceof TypeError && e.code === 'ERR_MISSING_ARGS' && e.message === 'The \"url\" argument must be specified'; } return false; })()",
        "(() => { try { URL.parse(); } catch (e) { return e.code === 'ERR_MISSING_ARGS'; } return false; })()",
        "(() => { try { new URL({ toString() { throw new RangeError('boom'); } }); } catch (e) { return e instanceof RangeError; } return false; })()",
        "URL.parse('nope') === null && URL.parse('/x', 'http://a/') instanceof URL && URL.parse('/x', 'http://a/').href === 'http://a/x' && URL.parse('/x', 'bad') === null",
        "URL.canParse('http://a') && !URL.canParse('/x') && URL.canParse('/x', 'http://a') && !URL.canParse('/x', 'bad')",
        "(() => { try { URL.canParse(); } catch (e) { return e.code === 'ERR_MISSING_ARGS'; } return false; })()",
        "new URL('http://a/\\ud800').pathname === '/%EF%BF%BD'",
        "(() => { const getter = Object.getOwnPropertyDescriptor(URL.prototype, 'href').get; try { getter.call({}); } catch (e) { return e instanceof TypeError && e.code === 'ERR_INVALID_THIS'; } return false; })()",
        "(() => { const setter = Object.getOwnPropertyDescriptor(URL.prototype, 'hash').set; try { setter.call(new URLSearchParams(), 'x'); } catch (e) { return e.code === 'ERR_INVALID_THIS'; } return false; })()",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn url_search_params_stay_linked_to_their_url() {
    let mut runtime = Runtime::new_browser();
    for expression in [
        "(() => { const u = new URL('http://a/?q=1'); return u.searchParams === u.searchParams && u.searchParams instanceof URLSearchParams; })()",
        "(() => { const u = new URL('http://a/?q=1'); u.searchParams.append('a', 'b c'); return u.search === '?q=1&a=b+c' && u.href === 'http://a/?q=1&a=b+c'; })()",
        "(() => { const u = new URL('http://a/?q=1&q=2&r=3'); const sp = u.searchParams; sp.delete('q'); sp.set('r', '4'); return u.search === '?r=4'; })()",
        "(() => { const u = new URL('http://a/?q=1#h'); u.searchParams.delete('q'); return u.href === 'http://a/#h' && u.search === ''; })()",
        "(() => { const u = new URL('http://a/?b=2&a=1'); u.searchParams.sort(); return u.search === '?a=1&b=2'; })()",
        "(() => { const u = new URL('http://a/?q=1'); const sp = u.searchParams; u.search = '?z=9&z=8'; return sp.get('q') === null && sp.getAll('z').join() === '9,8' && sp.size === 2; })()",
        "(() => { const u = new URL('http://a/?q=1'); const sp = u.searchParams; u.href = 'http://b/?k=v'; return sp.get('k') === 'v' && sp.get('q') === null; })()",
        "(() => { const u = new URL('http://a/?q=1'); const sp = u.searchParams; u.search = ''; return sp.size === 0 && u.searchParams === sp; })()",
        "(() => { const u = new URL('http://a/?q=1'); const sp = u.searchParams; u.pathname = '/other'; u.hash = 'x'; return sp.get('q') === '1' && u.search === '?q=1'; })()",
        "(() => { const u = new URL('http://a/?q=1'); const it = u.searchParams.keys(); u.search = '?x=1&y=2'; return [...it].join() === 'x,y'; })()",
        "(() => { const u = new URL('http://a/?q=1'); const sp = u.searchParams; const detached = new URLSearchParams(sp); detached.append('z', '1'); return u.search === '?q=1' && sp.size === 1; })()",
        "(() => { const sp = new URL('http://a/?q=1').searchParams; return sp.toString() === 'q=1'; })()",
        "(() => { const u = Object.freeze(new URL('http://a/?q=1')); u.searchParams.append('a', '1'); return u.search === '?q=1&a=1'; })()",
        "(() => { const u = new URL('http://a/?q=1'); u.searchParams.append('k', 'v'); return JSON.stringify(Object.keys(u.searchParams)) === '[]' && u.searchParams.has('k', 'v') && !u.searchParams.has('k', 'w'); })()",
    ] {
        assert_script(&mut runtime, expression);
    }
    evaluate(
        &mut runtime,
        "globalThis.weak = (() => { const u = new URL('http://a/?q=1'); const sp = u.searchParams; sp.__keep = u; return new WeakRef(sp); })();",
    );
    runtime.engine().collect_garbage();
    runtime.engine().collect_garbage();
    assert_script(&mut runtime, "weak.deref() === undefined");
}

#[test]
fn url_search_params_constructor_and_operations() {
    let mut runtime = Runtime::new_browser();
    for expression in [
        "new URLSearchParams().toString() === '' && new URLSearchParams(undefined).size === 0 && new URLSearchParams(null).size === 0",
        "new URLSearchParams('?a=1&b=2').toString() === 'a=1&b=2' && new URLSearchParams('??a=1').get('?a') === '1'",
        "(() => { const sp = new URLSearchParams('a=1&&b&=c&d=e=f&g=%E4%F6&h=x+y%20z'); return [sp.get('a'), sp.get('b'), sp.get(''), sp.get('d'), sp.get('g'), sp.get('h')].join('|') === '1||c|e=f|\\ufffd\\ufffd|x y z'; })()",
        "new URLSearchParams({ a: '1', b: 2 }).toString() === 'a=1&b=2'",
        "new URLSearchParams([['a', '1'], ['b', '2'], ['a', '3']]).toString() === 'a=1&b=2&a=3'",
        "new URLSearchParams(new Map([['a', '1']])).toString() === 'a=1'",
        "new URLSearchParams(new URLSearchParams('x=1&x=2')).getAll('x').join() === '1,2'",
        "new URLSearchParams([['a', 'b'][Symbol.iterator]()].map((it) => it)).toString() === 'a=b'",
        "(() => { const record = { b: '1' }; Object.defineProperty(record, 'hidden', { value: 'x', enumerable: false }); const plain = new URLSearchParams(record).toString() === 'b=1'; record[Symbol.for('s')] = 'y'; try { new URLSearchParams(record); } catch (e) { return plain && e instanceof TypeError && e.message === 'Cannot convert a Symbol value to a string'; } return false; })()",
        "new URLSearchParams({ a: '\\ud800' }).get('a') === '\\ufffd' && new URLSearchParams([['\\udc00', 'x']]).has('\\ufffd')",
        "(() => { const sp = new URLSearchParams('b=2&a=1&a=0&c=9'); sp.append('d', 'x y'); sp.set('a', 'z'); sp.delete('c'); return sp.toString() === 'b=2&a=z&d=x+y' && sp.size === 3; })()",
        "(() => { const sp = new URLSearchParams('a=1&a=2&b=1'); sp.delete('a', '2'); const kept = sp.toString(); sp.delete('b', undefined); return kept === 'a=1&b=1' && sp.toString() === 'a=1'; })()",
        "(() => { const sp = new URLSearchParams('a=1&a=2'); return sp.has('a') && sp.has('a', '2') && !sp.has('a', '3') && sp.has('a', undefined) && !sp.has('b') && sp.get('a') === '1' && sp.get('b') === null && sp.getAll('a').join() === '1,2' && sp.getAll('b').length === 0; })()",
        "(() => { const sp = new URLSearchParams('a=null'); return sp.has('a', null) === true && sp.set('n', null) === undefined && sp.get('n') === 'null'; })()",
        "(() => { const sp = new URLSearchParams('b=1&\\uE000=3&\\ud83d\\ude00=2&a=0&b=0'); sp.sort(); return [...sp.keys()].join() === 'a,b,b,\\ud83d\\ude00,\\uE000' && sp.getAll('b').join() === '1,0'; })()",
        "(() => { const sp = new URLSearchParams('a=1&b=2'); const seen = []; sp.forEach(function (value, key, owner) { seen.push(key + value + (owner === sp) + (this.tag)); }, { tag: 't' }); return seen.join() === 'a1truet,b2truet'; })()",
        "(() => { const sp = new URLSearchParams('a=1&b=2&c=3'); const seen = []; sp.forEach((value, key) => { seen.push(key); if (key === 'a') sp.delete('b'); }); return seen.join() === 'a,c'; })()",
        "(() => { const sp = new URLSearchParams('a=1&b=2'); return [...sp].map((p) => p.join('=')).join('&') === 'a=1&b=2' && [...sp.keys()].join() === 'a,b' && [...sp.values()].join() === '1,2' && [...sp.entries()].length === 2; })()",
        "(() => { const sp = new URLSearchParams('a=1'); const it = sp.entries(); const first = it.next(); sp.append('b', '2'); const second = it.next(); const third = it.next(); return first.value.join() === 'a,1' && second.value.join() === 'b,2' && third.done && third.value === undefined; })()",
        "(() => { const it = new URLSearchParams('a=1').keys(); return it[Symbol.iterator]() === it && Object.prototype.toString.call(it) === '[object URLSearchParams Iterator]'; })()",
        "(() => { const it = new URLSearchParams().keys(); const proto = Object.getPrototypeOf(it); const iteratorProto = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]())); return Object.getPrototypeOf(proto) === iteratorProto && Object.getOwnPropertyDescriptor(proto, 'next').enumerable && proto.hasOwnProperty('constructor') === false && proto[Symbol.toStringTag] === 'URLSearchParams Iterator'; })()",
        "(() => { const it = new URLSearchParams('a=1').keys(); try { it.next.call({}); } catch (e) { return e instanceof TypeError && e.code === 'ERR_INVALID_THIS'; } return false; })()",
        "(() => { try { URLSearchParams.prototype.get.call({}, 'a'); } catch (e) { return e instanceof TypeError && e.code === 'ERR_INVALID_THIS' && e.message === 'Value of \"this\" must be of type URLSearchParams'; } return false; })()",
        "(() => { try { Object.getOwnPropertyDescriptor(URLSearchParams.prototype, 'size').get.call(null); } catch (e) { return e.code === 'ERR_INVALID_THIS'; } return false; })()",
        "(() => { const sp = new URLSearchParams(); const errors = []; for (const run of [() => sp.append('a'), () => sp.set('a'), () => sp.get(), () => sp.getAll(), () => sp.has(), () => sp.delete()]) { try { run(); } catch (e) { errors.push(e instanceof TypeError && e.code === 'ERR_MISSING_ARGS' ? e.message : 'bad'); } } return errors.join('|') === 'The \"name\" and \"value\" arguments must be specified|The \"name\" and \"value\" arguments must be specified|The \"name\" argument must be specified|The \"name\" argument must be specified|The \"name\" argument must be specified|The \"name\" argument must be specified'; })()",
        "(() => { const codes = []; for (const init of [[['a']], [['a', 'b', 'c']], [null], [1], ['ab'], [new Set(['a'])]]) { try { new URLSearchParams(init); codes.push('none'); } catch (e) { codes.push(e instanceof TypeError ? e.code : 'bad'); } } return codes.join() === 'ERR_INVALID_TUPLE,ERR_INVALID_TUPLE,ERR_INVALID_TUPLE,ERR_INVALID_TUPLE,ERR_INVALID_TUPLE,ERR_INVALID_TUPLE'; })()",
        "(() => { try { new URLSearchParams({ [Symbol.iterator]: 1 }); } catch (e) { return e instanceof TypeError && e.code === 'ERR_ARG_NOT_ITERABLE' && e.message === 'Query pairs must be iterable'; } return false; })()",
        "(() => { try { new URLSearchParams([['a', Symbol()]]); } catch (e) { return e instanceof TypeError && e.code === undefined; } return false; })()",
        "(() => { try { new URLSearchParams().forEach(1); } catch (e) { return e instanceof TypeError && e.code === 'ERR_INVALID_ARG_TYPE' && e.message === 'The \"callback\" argument must be of type function. Received type number (1)'; } return false; })()",
        "(() => { try { new URLSearchParams().forEach(); } catch (e) { return e.code === 'ERR_INVALID_ARG_TYPE'; } return false; })()",
        "(() => { const sp = new URLSearchParams(); sp.append(1, { toString() { return 'two'; } }); return sp.toString() === '1=two'; })()",
        "(() => { const sp = new URLSearchParams('a=1'); sp.append({ toString() { sp.delete('a'); return 'k'; } }, 'v'); return sp.toString() === 'k=v'; })()",
        "(() => { const sp = new URLSearchParams(); sp.append('é €', '😀*-._~!'); return sp.toString() === '%C3%A9+%E2%82%AC=%F0%9F%98%80*-._%7E%21'; })()",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn url_inspect_hooks_and_object_urls() {
    let mut runtime = Runtime::new_browser();
    for expression in [
        "(() => { const sym = Symbol.for('nodejs.util.inspect.custom'); return typeof URL.prototype[sym] === 'function' && typeof URLSearchParams.prototype[sym] === 'function' && !Object.getOwnPropertyDescriptor(URL.prototype, sym).enumerable; })()",
        "(() => { const sym = Symbol.for('nodejs.util.inspect.custom'); const text = new URL('http://a/?x=1')[sym](2, {}, (value) => Object.keys(value).join() + '|' + String(value.searchParams.get('x')) + '|' + value.constructor.name); return text === 'URL href,origin,protocol,username,password,host,hostname,port,pathname,search,searchParams,hash|1|URL'; })()",
        "(() => { const sym = Symbol.for('nodejs.util.inspect.custom'); const url = new URL('http://a/'); return url[sym](-1, {}, () => '') === url; })()",
        "(() => { const sym = Symbol.for('nodejs.util.inspect.custom'); const options = { breakLength: 80, stylize: (text) => text }; const inspect = (value) => JSON.stringify(value); return new URLSearchParams('a=1&b=2')[sym](2, options, inspect) === 'URLSearchParams { \"a\" => \"1\", \"b\" => \"2\" }' && new URLSearchParams()[sym](2, options, inspect) === 'URLSearchParams {}' && new URLSearchParams('a=1')[sym](-1, { stylize: (text, style) => text + ':' + style }, inspect) === '[Object]:special'; })()",
        "(() => { const sym = Symbol.for('nodejs.util.inspect.custom'); const options = { breakLength: 3, stylize: (text) => text }; return new URLSearchParams('a=1&b=2')[sym](2, options, (value) => JSON.stringify(value)) === 'URLSearchParams {\\n  \"a\" => \"1\",\\n  \"b\" => \"2\" }'; })()",
        "(() => { const sym = Symbol.for('nodejs.util.inspect.custom'); const it = new URLSearchParams('a=1&b=2').entries(); it.next(); return it[sym](2, { stylize: (text) => text }, (value) => JSON.stringify(value)) === 'URLSearchParams Iterator { [\"b\",\"2\"] }'; })()",
        "(() => { const blob = new Blob(['hello'], { type: 'text/plain' }); const url = URL.createObjectURL(blob); const id = url.slice('blob:nodedata:'.length); const internals = globalThis.__lumenBlobInternals; const before = internals.resolveObjectURL(id); URL.revokeObjectURL(url); return url.startsWith('blob:nodedata:') && before instanceof Blob && before !== blob && before.size === 5 && before.type === 'text/plain' && internals.resolveObjectURL(id) === undefined; })()",
        "(() => { URL.revokeObjectURL('http://a/'); URL.revokeObjectURL('nope'); URL.revokeObjectURL('blob:nodedata:unknown'); return true; })()",
        "(() => { try { URL.createObjectURL({}); } catch (e) { return e instanceof TypeError && e.code === 'ERR_INVALID_ARG_TYPE' && e.message === 'The \"obj\" argument must be an instance of Blob. Received an instance of Object'; } return false; })()",
        "(() => { try { URL.createObjectURL(); } catch (e) { return e.code === 'ERR_INVALID_ARG_TYPE'; } return false; })()",
        "(() => { try { URL.revokeObjectURL(); } catch (e) { return e.code === 'ERR_MISSING_ARGS'; } return false; })()",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn blob_file_and_form_data_keep_web_idl_shape() {
    let mut runtime = Runtime::new_browser();
    for expression in [
        r##"['Blob', 'File', 'FormData'].every((name) => { const d = Object.getOwnPropertyDescriptor(globalThis, name); return d.writable && d.configurable && !d.enumerable; })"##,
        "Blob.length === 0 && File.length === 2 && FormData.length === 0",
        "Object.getPrototypeOf(File) === Blob && Object.getPrototypeOf(File.prototype) === Blob.prototype",
        "Object.prototype.toString.call(new Blob()) === '[object Blob]' && Object.prototype.toString.call(new File([], 'a')) === '[object File]' && Object.prototype.toString.call(new FormData()) === '[object FormData]'",
        r##"['size', 'type'].every((name) => { const d = Object.getOwnPropertyDescriptor(Blob.prototype, name); return d.enumerable && d.configurable && typeof d.get === 'function' && d.set === undefined; })"##,
        r##"['name', 'lastModified'].every((name) => { const d = Object.getOwnPropertyDescriptor(File.prototype, name); return d.enumerable && d.configurable && typeof d.get === 'function' && d.set === undefined; })"##,
        r##"['slice', 'text', 'arrayBuffer', 'bytes', 'stream'].every((name) => Object.getOwnPropertyDescriptor(Blob.prototype, name).enumerable && typeof Blob.prototype[name] === 'function')"##,
        "Blob.prototype.slice.length === 0 && Blob.prototype.text.length === 0",
        r##"['append', 'delete', 'get', 'getAll', 'has', 'set', 'entries', 'keys', 'values', 'forEach'].every((name) => Object.getOwnPropertyDescriptor(FormData.prototype, name).enumerable)"##,
        "FormData.prototype[Symbol.iterator] === FormData.prototype.entries",
        "FormData.prototype.append.length === 2 && FormData.prototype.set.length === 2 && FormData.prototype.get.length === 1 && FormData.prototype.forEach.length === 1",
        "(() => { try { Blob(); } catch (e) { return e instanceof TypeError; } return false; })()",
        "(() => { try { new File([]); } catch (e) { return e instanceof TypeError && e.code === 'ERR_MISSING_ARGS'; } return false; })()",
        "(() => { try { Object.getOwnPropertyDescriptor(Blob.prototype, 'size').get.call({}); } catch (e) { return e instanceof TypeError && e.code === 'ERR_INVALID_THIS'; } return false; })()",
        "(() => { try { FormData.prototype.get.call({}, 'a'); } catch (e) { return e instanceof TypeError && e.code === 'ERR_INVALID_THIS'; } return false; })()",
        "(() => { try { new FormData().append('a'); } catch (e) { return e.code === 'ERR_MISSING_ARGS'; } return false; })()",
        "(() => { try { new Blob([], { endings: 'bad' }); } catch (e) { return e instanceof TypeError && e.code === 'ERR_INVALID_ARG_VALUE'; } return false; })()",
        "(() => { class Mine extends Blob {} const b = new Mine(['ab']); return b instanceof Mine && b.size === 2 && b.slice(1) instanceof Blob; })()",
        "(() => { class Mine extends File {} const f = new Mine(['a'], 'n.txt'); return f instanceof Mine && f.name === 'n.txt' && f instanceof Blob; })()",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn blob_slice_type_and_part_conversion() {
    let mut runtime = Runtime::new_browser();
    for expression in [
        "new Blob(['abc', new Uint8Array([100, 101]), new Blob(['f']), new DataView(new Uint8Array([103]).buffer), new Uint8Array([104]).buffer]).size === 8",
        "new Blob(['a\\r\\nb'], { endings: 'transparent' }).size === 4",
        "new Blob(['\\ud800']).size === 3",
        "new Blob([], { type: 'Text/PLAIN' }).type === 'text/plain' && new Blob([], { type: 'bad\\u00e9' }).type === '' && new Blob([]).type === ''",
        "(() => { const b = new Blob(['0123456789']); return b.slice(2, 5).size === 3 && b.slice(-3).size === 3 && b.slice(5, 2).size === 0 && b.slice(-100, 100).size === 10 && b.slice(0, 4, 'Text/X').type === 'text/x' && b.slice().type === '' && b.slice(NaN, Infinity).size === 10; })()",
        "(() => { const b = new Blob(['x'], { type: 'a/b' }); return b.slice().type === '' && b.slice(0, 1, undefined).type === ''; })()",
        "(() => { try { new Blob('abc'); } catch (e) { return e instanceof TypeError && e.code === 'ERR_INVALID_ARG_TYPE'; } return false; })()",
        "(() => { try { new Blob([], 1); } catch (e) { return e.code === 'ERR_INVALID_ARG_TYPE'; } return false; })()",
        "new Blob(new Set(['ab', 'c'])).size === 3",
        "new File(['abc'], 'a.txt', { type: 'Text/Plain', lastModified: 42.9 }).lastModified === 42 && new File([], 'a').lastModified > 1e12 && new File(['abc'], 'a.txt', { type: 'Text/Plain' }).type === 'text/plain'",
        "(() => { const f = new File(['abcd'], 'a'); const s = f.slice(1, 3); return s instanceof Blob && !(s instanceof File) && s.size === 2; })()",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn blob_reads_resolve_promises_and_stream_chunks() {
    let mut runtime = Runtime::new_browser();
    evaluate(
        &mut runtime,
        r#"
        var results = {};
        const blob = new Blob(['h\u00e9llo ', new Uint8Array([0xf0, 0x9f, 0x98, 0x80])], { type: 'text/plain' });
        blob.text().then((value) => results.text = value);
        blob.arrayBuffer().then((value) => results.buffer = value);
        blob.bytes().then((value) => results.bytes = value);
        (async () => {
          const reader = blob.stream().getReader();
          const chunks = [];
          for (;;) { const { done, value } = await reader.read(); if (done) break; chunks.push(value); }
          results.stream = chunks;
        })();
        new Blob([new Uint8Array([0xef, 0xbb, 0xbf, 0x41, 0xff])]).text().then((value) => results.bom = value);
        new Blob([]).stream().getReader().read().then((value) => results.empty = value.done);
        var invalidThis = [];
        Blob.prototype.text.call({}).catch((error) => invalidThis.push(error.code));
        Blob.prototype.arrayBuffer.call({}).catch((error) => invalidThis.push(error.code));
        "#,
    );
    for expression in [
        "results.text === 'h\\u00e9llo \u{1f600}'",
        "results.buffer instanceof ArrayBuffer && results.buffer.byteLength === 11",
        "results.bytes instanceof Uint8Array && results.bytes.length === 11 && results.bytes[0] === 104",
        "results.stream.length === 1 && results.stream[0] instanceof Uint8Array && results.stream[0].length === 11",
        "results.bom === 'A\\ufffd'",
        "results.empty === true",
        "invalidThis.join() === 'ERR_INVALID_THIS,ERR_INVALID_THIS'",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn web_streams_glue_runs_once_on_first_access_to_any_published_global() {
    let names = "['ReadableStream', 'ReadableStreamDefaultReader', 'ReadableStreamBYOBReader', 'ReadableStreamBYOBRequest', 'ReadableByteStreamController', 'ReadableStreamDefaultController', 'WritableStream', 'WritableStreamDefaultWriter', 'WritableStreamDefaultController', 'TransformStream', 'TransformStreamDefaultController', 'ByteLengthQueuingStrategy', 'CountQueuingStrategy', 'TextEncoderStream', 'TextDecoderStream']";
    let mut runtime = Runtime::new_browser();
    evaluate(
        &mut runtime,
        &format!(
            r#"
        var streamNames = {names};
        var glueRuns = 0;
        const nativeDefine = Object.defineProperty;
        Object.defineProperty = function (target, key, descriptor) {{
          if (key === Symbol.for('lumen.cloneBody')) glueRuns++;
          return nativeDefine.call(this, target, key, descriptor);
        }};
        var presentBefore = streamNames.every((name) => name in globalThis);
        var runsBefore = glueRuns;
        "#
        ),
    );
    for expression in [
        "presentBefore && runsBefore === 0",
        "(() => { const d = Object.getOwnPropertyDescriptor(globalThis, 'WritableStream'); return glueRuns === 1 && typeof d.value === 'function' && d.get === undefined && d.set === undefined && d.writable && d.configurable && !d.enumerable; })()",
        "streamNames.every((name) => { const d = Object.getOwnPropertyDescriptor(globalThis, name); return typeof d.value === 'function' && d.get === undefined && d.writable && d.configurable && !d.enumerable && d.value.name === name; })",
        "glueRuns === 1",
        "typeof ReadableStream === 'function' && new ReadableStream() instanceof ReadableStream && glueRuns === 1",
    ] {
        assert_script(&mut runtime, expression);
    }

    let mut runtime = Runtime::new_browser();
    evaluate(
        &mut runtime,
        r#"
        var out = {};
        globalThis.TransformStream = 'page override';
        const reader = new ReadableStream({ start(c) { c.enqueue(new Uint8Array([1, 2])); c.close(); } }).getReader();
        reader.read().then((r) => out.chunk = r.value.length);
        new Blob(['abc']).stream().getReader().read().then((r) => out.blob = r.value.length);
        new TextEncoderStream();
        "#,
    );
    for expression in [
        "globalThis.TransformStream === 'page override'",
        "out.chunk === 2 && out.blob === 3",
        "typeof WritableStream === 'function' && typeof TextDecoderStream === 'function'",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn form_data_operations_iteration_and_file_entries() {
    let mut runtime = Runtime::new_browser();
    for expression in [
        "(() => { const f = new FormData(); f.append('a', '1'); f.append('b', '2'); f.append('a', '3'); return f.get('a') === '1' && f.getAll('a').join() === '1,3' && f.has('b') && !f.has('z') && f.get('z') === null && f.getAll('z').length === 0; })()",
        "(() => { const f = new FormData(); f.append('a', '1'); f.append('b', '2'); f.append('a', '3'); f.set('a', 'x'); return [...f].map((e) => e.join('=')).join('&') === 'a=x&b=2'; })()",
        "(() => { const f = new FormData(); f.set('n', 'v'); f.append('m', 'w'); f.delete('n'); return !f.has('n') && [...f.keys()].join() === 'm' && [...f.values()].join() === 'w'; })()",
        "(() => { const f = new FormData(); f.append(1, 2); return f.get('1') === '2' && typeof f.get('1') === 'string'; })()",
        "(() => { const f = new FormData(); f.append('\\ud800', '\\udc00'); return f.has('\\ufffd') && f.get('\\ufffd') === '\\ufffd'; })()",
        "(() => { const f = new FormData(); const b = new Blob(['xy'], { type: 'text/plain' }); f.append('f', b); const v = f.get('f'); return v instanceof File && v !== b && v.name === 'blob' && v.size === 2 && v.type === 'text/plain'; })()",
        "(() => { const f = new FormData(); const file = new File(['xy'], 'a.txt', { lastModified: 7 }); f.append('f', file); f.append('g', file, 'b.txt'); return f.get('f') === file && f.get('g') !== file && f.get('g').name === 'b.txt' && f.get('g').lastModified === 7 && f.get('g').size === 2; })()",
        "(() => { try { new FormData().append('a', 'b', 'c'); } catch (e) { return e instanceof TypeError; } return false; })()",
        "(() => { const f = new FormData(); f.append('a', '1'); f.append('b', '2'); const seen = []; const self = {}; f.forEach(function (value, name, form) { seen.push(name + value + (form === f) + (this === self)); }, self); return seen.join() === 'a1truetrue,b2truetrue'; })()",
        "(() => { const f = new FormData(); f.append('a', '1'); const it = f.entries(); return Object.prototype.toString.call(it) === '[object FormData Iterator]' && it[Symbol.iterator]() === it && it.next().value.join() === 'a,1' && it.next().done === true; })()",
        "(() => { const f = new FormData(); f.append('a', '1'); const seen = []; f.forEach((value, name) => { seen.push(name); if (seen.length < 3) f.append('n' + seen.length, 'x'); }); return seen.join() === 'a,n1,n2'; })()",
        "(() => { try { new FormData({}); } catch (e) { return e instanceof TypeError; } return false; })()",
        "(() => { try { new FormData(undefined, {}); return true; } catch (e) { return false; } })()",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn form_data_multipart_round_trip_through_fetch_bodies() {
    let mut runtime = Runtime::new_browser();
    evaluate(
        &mut runtime,
        r#"
        var results = {};
        const form = new FormData();
        form.append('text', 'a\nb');
        form.append('file', new File(['\u0000bytes\r\n--x'], 'f"n.bin', { type: 'application/x-test' }));
        const request = new Request('http://a.test/', { method: 'POST', body: form });
        results.contentType = request.headers.get('content-type');
        request.formData().then((parsed) => {
          results.text = parsed.get('text');
          const file = parsed.get('file');
          results.file = [file instanceof File, file.name, file.type, file.size];
          return file.text();
        }).then((text) => results.fileText = text);
        new Response(new Blob(['ab'], { type: 'text/x' })).blob().then((blob) => results.blob = [blob.type, blob.size]);
        results.blobContentType = new Request('http://a.test/', { method: 'POST', body: new Blob(['ab'], { type: 'text/x' }) }).headers.get('content-type');
        "#,
    );
    for expression in [
        "results.contentType.startsWith('multipart/form-data; boundary=----lumenFormBoundary')",
        "results.text === 'a\\r\\nb'",
        "results.file.join('|') === 'true|f\"n.bin|application/x-test|' + new Blob(['\\u0000bytes\\r\\n--x']).size",
        "results.fileText === '\\u0000bytes\\r\\n--x'",
        "results.blob.join() === 'text/x,2'",
        "results.blobContentType === 'text/x'",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn blob_and_file_survive_structured_clone_and_object_urls_follow_the_registry() {
    let mut runtime = Runtime::new_browser();
    evaluate(
        &mut runtime,
        r#"
        var results = {};
        const blob = new Blob(['abc'], { type: 'text/plain' });
        const file = new File(['de'], 'n.txt', { type: 'a/b', lastModified: 5 });
        results.blobClone = structuredClone(blob);
        results.fileClone = structuredClone({ file });
        results.url = URL.createObjectURL(file);
        results.second = URL.createObjectURL(file);
        results.resolved = globalThis.__lumenBlobInternals.resolveObjectURL(results.url.slice('blob:nodedata:'.length));
        URL.revokeObjectURL(results.url);
        results.revoked = globalThis.__lumenBlobInternals.resolveObjectURL(results.url.slice('blob:nodedata:'.length));
        "#,
    );
    for expression in [
        "results.blobClone instanceof Blob && !(results.blobClone instanceof File) && results.blobClone.size === 3 && results.blobClone.type === 'text/plain'",
        "results.fileClone.file instanceof File && results.fileClone.file.name === 'n.txt' && results.fileClone.file.lastModified === 5 && results.fileClone.file.type === 'a/b' && results.fileClone.file.size === 2",
        "results.url !== results.second && results.url.startsWith('blob:nodedata:')",
        "results.resolved instanceof Blob && results.resolved.size === 2 && results.resolved.type === 'a/b'",
        "results.revoked === undefined",
        "globalThis.__lumenBlobInternals.resolveObjectURL(results.second.slice('blob:nodedata:'.length)) instanceof Blob",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn native_event_classes_have_web_idl_shape() {
    let mut runtime = Runtime::new_browser();
    for expression in [
        "Event.length === 1 && EventTarget.length === 0 && CustomEvent.length === 1 && ErrorEvent.length === 1",
        "EventTarget.prototype.addEventListener.length === 2 && EventTarget.prototype.dispatchEvent.length === 1",
        "Event.NONE === 0 && Event.CAPTURING_PHASE === 1 && Event.AT_TARGET === 2 && Event.BUBBLING_PHASE === 3 && Event.prototype.BUBBLING_PHASE === 3",
        "Object.getOwnPropertyDescriptor(Event, 'AT_TARGET').writable === false && Object.getOwnPropertyDescriptor(Event, 'AT_TARGET').enumerable === true",
        "Object.getOwnPropertyDescriptor(Event.prototype, 'type').enumerable === true && typeof Object.getOwnPropertyDescriptor(Event.prototype, 'type').get === 'function'",
        "Object.getOwnPropertyDescriptor(EventTarget.prototype, 'addEventListener').enumerable === true",
        "Event.prototype[Symbol.toStringTag] === 'Event' && AbortSignal.prototype[Symbol.toStringTag] === 'AbortSignal' && DOMException.prototype[Symbol.toStringTag] === 'DOMException'",
        "Object.getPrototypeOf(CustomEvent.prototype) === Event.prototype && Object.getPrototypeOf(ErrorEvent.prototype) === Event.prototype && Object.getPrototypeOf(AbortSignal.prototype) === EventTarget.prototype",
        "Object.getPrototypeOf(AbortController.prototype) === Object.prototype",
        "(() => { try { new AbortSignal(); } catch (e) { return e instanceof TypeError && e.code === 'ERR_ILLEGAL_CONSTRUCTOR'; } })()",
        "(() => { try { EventTarget.prototype.addEventListener.call({}, 'x', () => {}); } catch (e) { return e instanceof TypeError && e.code === 'ERR_INVALID_THIS'; } })()",
        "(() => { try { new Event(); } catch (e) { return e instanceof TypeError && e.code === 'ERR_MISSING_ARGS'; } })()",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn native_event_dispatch_runs_listeners_in_order_with_dispatch_state() {
    let mut runtime = Runtime::new_browser();
    assert_script(&mut runtime, r#"(() => {
        const target = new EventTarget();
        const order = [];
        const event = new Event('ping', {cancelable: true});
        target.addEventListener('ping', e => {
            order.push('first:' + e.eventPhase + ':' + (e.target === target) + ':' + (e.currentTarget === target));
            e.preventDefault();
        });
        const handler = { handleEvent(e) { order.push('object:' + (this === handler)); e.stopImmediatePropagation(); } };
        target.addEventListener('ping', handler);
        target.addEventListener('ping', () => order.push('never'));
        target.addEventListener('ping', handler);
        const result = target.dispatchEvent(event);
        return result === false && event.defaultPrevented && event.currentTarget === null &&
            event.eventPhase === 0 && event.isTrusted === false &&
            order.join() === 'first:2:true:true,object:true';
    })()"#);
    assert_script(&mut runtime, r#"(() => {
        const target = new EventTarget();
        let calls = 0;
        const listener = () => calls++;
        target.addEventListener('x', listener, {once: true});
        target.dispatchEvent(new Event('x'));
        target.dispatchEvent(new Event('x'));
        target.addEventListener('x', listener, true);
        target.removeEventListener('x', listener);
        target.dispatchEvent(new Event('x'));
        target.removeEventListener('x', listener, true);
        target.dispatchEvent(new Event('x'));
        return calls === 2;
    })()"#);
    assert_script(&mut runtime, r#"(() => {
        const target = new EventTarget();
        const event = new Event('x');
        target.addEventListener('x', () => {
            try { target.dispatchEvent(event); } catch (e) { event.caught = e.code; }
        });
        target.dispatchEvent(event);
        return event.caught === 'ERR_EVENT_RECURSION';
    })()"#);
}

#[test]
fn native_event_constructors_read_init_and_expose_unforgeable_trust() {
    let mut runtime = Runtime::new_browser();
    for expression in [
        "(() => { const e = new Event('x', {bubbles: 1, cancelable: 'y', composed: {}}); return e.bubbles && e.cancelable && e.composed && e.type === 'x' && e.timeStamp >= 0; })()",
        "(() => { const e = new CustomEvent('x', {detail: {a: 1}}); return e.detail.a === 1 && new CustomEvent('y').detail === null; })()",
        "(() => { const e = new ErrorEvent('error', {message: 'm', filename: 'f', lineno: 3, colno: 4, error: 5}); return e.message === 'm' && e.filename === 'f' && e.lineno === 3 && e.colno === 4 && e.error === 5; })()",
        "(() => { const d = Object.getOwnPropertyDescriptor(new Event('x'), 'isTrusted'); return d.configurable === false && d.get === Object.getOwnPropertyDescriptor(Event.prototype, 'isTrusted').get; })()",
        "(() => { try { Object.defineProperty(new Event('x'), 'isTrusted', {value: true}); } catch (e) { return e instanceof TypeError; } })()",
        "(() => { const e = new Event('x', {isTrusted: true}); return e.isTrusted === false; })()",
        "(() => { let n = 0; try { new Event('x', {get bubbles() { throw 1; }}); } catch (e) { n = e; } return n === 1; })()",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn native_abort_signals_abort_with_dom_exceptions_and_compose() {
    let mut runtime = Runtime::new_browser();
    for expression in [
        "(() => { const s = AbortSignal.abort(); return s.aborted && s.reason instanceof DOMException && s.reason instanceof Error && s.reason.name === 'AbortError' && s.reason.code === 20; })()",
        "(() => { const c = new AbortController(); const seen = []; c.signal.addEventListener('abort', e => seen.push(e.type + ':' + e.isTrusted)); c.abort('r'); c.abort('later'); return c.signal.reason === 'r' && seen.join() === 'abort:true'; })()",
        "(() => { const a = new AbortController(), b = new AbortController(); const any = AbortSignal.any([a.signal, b.signal]); b.abort(7); return any.aborted && any.reason === 7 && !a.signal.aborted; })()",
        "(() => { const s = AbortSignal.abort(); try { s.throwIfAborted(); } catch (e) { return e === s.reason; } })()",
        "(() => { const c = new AbortController(); let n = 0; const t = new EventTarget(); t.addEventListener('x', () => n++, {signal: c.signal}); t.dispatchEvent(new Event('x')); c.abort(); t.dispatchEvent(new Event('x')); return n === 1; })()",
        "(() => { try { AbortSignal.any([{}]); } catch (e) { return e instanceof TypeError && e.code === 'ERR_INVALID_ARG_TYPE'; } })()",
    ] {
        assert_script(&mut runtime, expression);
    }
    evaluate(&mut runtime, "globalThis.timeoutSignal = AbortSignal.timeout(1); globalThis.timeoutSeen = []; timeoutSignal.addEventListener('abort', () => timeoutSeen.push(timeoutSignal.reason.name)); setTimeout(() => {}, 30);");
    runtime.run_until_idle();
    assert_script(&mut runtime, "timeoutSignal.aborted && timeoutSeen.join() === 'TimeoutError' && timeoutSignal.reason.code === 23");
}

#[test]
fn native_dom_exception_behaves_like_an_error() {
    let mut runtime = Runtime::new_browser();
    for expression in [
        "(() => { const e = new DOMException('m', 'NotFoundError'); return e instanceof Error && e instanceof DOMException && e.name === 'NotFoundError' && e.message === 'm' && e.code === 8 && typeof e.stack === 'string'; })()",
        "new DOMException().name === 'Error' && new DOMException().message === '' && new DOMException().code === 0",
        "DOMException.INDEX_SIZE_ERR === 1 && DOMException.DATA_CLONE_ERR === 25 && DOMException.prototype.TIMEOUT_ERR === 23",
        "Object.getPrototypeOf(DOMException.prototype) === Error.prototype && Object.getPrototypeOf(DOMException) === Error",
        "(() => { class Custom extends DOMException {} const e = new Custom('m', 'AbortError'); return e instanceof Custom && e.code === 20 && typeof e.stack === 'string'; })()",
        "(() => { const e = new DOMException('m', {name: 'DataError', cause: 5}); return e.name === 'DataError' && e.cause === 5; })()",
        "Error.prototype.toString.call(new DOMException('m', 'AbortError')) === 'AbortError: m'",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn xhr_classes_have_the_web_idl_shape() {
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, r#"
        var own = (target, name) => Object.getOwnPropertyDescriptor(target, name);
        var interfaces = [XMLHttpRequest, XMLHttpRequestEventTarget, XMLHttpRequestUpload, ProgressEvent];
        var globalsOk = interfaces.every(c => { const d = own(globalThis, c.name); return d.writable && !d.enumerable && d.configurable; });
        var chainOk = Object.getPrototypeOf(XMLHttpRequest) === XMLHttpRequestEventTarget
          && Object.getPrototypeOf(XMLHttpRequestUpload) === XMLHttpRequestEventTarget
          && Object.getPrototypeOf(XMLHttpRequestEventTarget) === EventTarget
          && Object.getPrototypeOf(ProgressEvent) === Event;
        var constantsOk = Object.entries({UNSENT: 0, OPENED: 1, HEADERS_RECEIVED: 2, LOADING: 3, DONE: 4}).every(([name, value]) =>
          [XMLHttpRequest, XMLHttpRequest.prototype].every(target => {
            const d = own(target, name);
            return d.value === value && d.enumerable && !d.writable && !d.configurable;
          }));
        var lengthsOk = XMLHttpRequest.length === 0 && ProgressEvent.length === 1 && XMLHttpRequest.prototype.open.length === 2
          && XMLHttpRequest.prototype.setRequestHeader.length === 2 && XMLHttpRequest.prototype.send.length === 0;
        var tags = [new XMLHttpRequest(), new XMLHttpRequest().upload, new ProgressEvent('x')].map(o => Object.prototype.toString.call(o)).join();
        var illegal = [XMLHttpRequestEventTarget, XMLHttpRequestUpload].map(c => { try { new c(); return 'constructed'; } catch (e) { return e.constructor.name; } }).join();
        var handlers = ['loadstart', 'progress', 'abort', 'error', 'load', 'timeout', 'loadend'].every(type => {
          const d = own(XMLHttpRequestEventTarget.prototype, 'on' + type);
          return d && typeof d.get === 'function' && typeof d.set === 'function' && d.enumerable && d.configurable;
        }) && own(XMLHttpRequest.prototype, 'onreadystatechange') !== undefined
          && own(XMLHttpRequestUpload.prototype, 'onload') === undefined;
        var xhr = new XMLHttpRequest(), handler = () => {};
        xhr.onload = handler;
        var roundTrip = xhr.onload === handler && (xhr.onload = 5, xhr.onload === null) && xhr.onreadystatechange === null;
        var brand = (() => { try { own(XMLHttpRequest.prototype, 'readyState').get.call({}); return 'no throw'; } catch (e) { return e.constructor.name; } })();
        var progress = new ProgressEvent('progress', {loaded: 3.7, lengthComputable: 1});
        var progressOk = progress.loaded === 3 && progress.total === 0 && progress.lengthComputable === true && progress.isTrusted === false;
        var initial = [xhr.readyState, xhr.status, xhr.statusText, xhr.responseURL, xhr.responseText, xhr.response, xhr.responseType, xhr.timeout, xhr.withCredentials].join('|');
    "#);
    assert_script(&mut runtime, "globalsOk && chainOk && constantsOk && lengthsOk && handlers && roundTrip && progressOk");
    assert_script(&mut runtime, "tags === '[object XMLHttpRequest],[object XMLHttpRequestUpload],[object ProgressEvent]'");
    assert_script(&mut runtime, "illegal === 'TypeError,TypeError' && brand === 'TypeError' && initial === '0|0||||||0|false'");
    assert_script(&mut runtime, "xhr.upload === xhr.upload && xhr instanceof EventTarget && xhr.upload instanceof XMLHttpRequestEventTarget");
}

#[test]
fn xhr_download_sequence_orders_ready_state_and_progress_events() {
    let (url, task) = server(Duration::ZERO);
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, &format!(r#"
        var xhr = new XMLHttpRequest(), order = [];
        xhr.onreadystatechange = () => order.push('rsc' + xhr.readyState);
        for (const type of ['loadstart', 'progress', 'load', 'loadend', 'error', 'abort', 'timeout'])
          xhr.addEventListener(type, e => order.push(type + ':' + e.loaded + '/' + e.total + (e.lengthComputable ? '!' : '') + (e.isTrusted ? 't' : '')));
        xhr.open('GET', '{url}');
        xhr.send();
        order.push('sent');
    "#));
    assert_script(&mut runtime, "order.join(',') === 'rsc1,loadstart:0/0t,sent,rsc2,rsc3,progress:13/13!t,rsc4,load:13/13!t,loadend:13/13!t'");
    assert_script(&mut runtime, "xhr.readyState === 4 && xhr.status === 200 && xhr.statusText === 'OK' && xhr.responseURL.startsWith('http://127.0.0.1:')");
    task.join().unwrap();
}

#[test]
fn xhr_network_failure_dispatches_error_then_loadend_with_done_state() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/refused", listener.local_addr().unwrap());
    drop(listener);
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, &format!(r#"
        var xhr = new XMLHttpRequest(), order = [];
        xhr.onreadystatechange = () => order.push('rsc' + xhr.readyState);
        for (const type of ['loadstart', 'error', 'load', 'loadend', 'abort', 'timeout'])
          xhr.addEventListener(type, () => order.push(type));
        xhr.upload.addEventListener('error', () => order.push('upload:error'));
        xhr.open('POST', '{url}');
        xhr.send('payload');
    "#));
    assert_script(&mut runtime, "order.join(',') === 'rsc1,loadstart,rsc4,upload:error,error,loadend' && xhr.readyState === 4 && xhr.status === 0 && xhr.responseText === ''");
}

#[test]
fn xhr_response_types_decode_the_same_bytes() {
    let mut runtime = Runtime::new();
    let cases: [(&str, &str, Vec<u8>, &str); 5] = [
        ("", "text/plain;charset=windows-1252", b"caf\xe9".to_vec(), "xhr.response === 'café' && xhr.responseText === 'café'"),
        ("text", "text/plain", "héllo".as_bytes().to_vec(), "xhr.response === 'héllo'"),
        ("json", "application/json", b"{\"a\":[1,2]}".to_vec(), "xhr.response.a[1] === 2 && xhr.response === xhr.response"),
        ("arraybuffer", "application/octet-stream", vec![9, 8, 7], "xhr.response instanceof ArrayBuffer && new Uint8Array(xhr.response).join() === '9,8,7'"),
        ("blob", "image/png", vec![1, 2, 3, 4], "xhr.response instanceof Blob && xhr.response.type === 'image/png' && xhr.response.size === 4"),
    ];
    for (kind, mime, body, expectation) in cases {
        let (url, task) = payload_server(Duration::ZERO, mime, body);
        evaluate(&mut runtime, &format!("var xhr = new XMLHttpRequest(); xhr.open('GET', '{url}'); xhr.responseType = '{kind}'; xhr.send();"));
        assert_script(&mut runtime, &format!("xhr.readyState === 4 && xhr.status === 200 && {expectation}"));
        task.join().unwrap();
    }
    assert_script(&mut runtime, "(() => { try { void xhr.responseText; return false; } catch (e) { return e.name === 'InvalidStateError'; } })()");
    let (url, task) = payload_server(Duration::ZERO, "application/json", b"not json".to_vec());
    evaluate(&mut runtime, &format!("var bad = new XMLHttpRequest(); bad.open('GET', '{url}'); bad.responseType = 'json'; bad.send();"));
    assert_script(&mut runtime, "bad.status === 200 && bad.response === null");
    task.join().unwrap();
}

#[test]
fn xhr_request_bodies_carry_their_default_content_type() {
    let mut runtime = Runtime::new();
    let cases: [(&str, &str); 5] = [
        ("'text'", "content-type: text/plain;charset=utf-8\r\n"),
        ("new URLSearchParams({a: '1 2'})", "content-type: application/x-www-form-urlencoded;charset=utf-8\r\n"),
        ("new Blob(['x'], {type: 'text/csv'})", "content-type: text/csv\r\n"),
        ("(() => { const f = new FormData(); f.append('k', 'v'); return f; })()", "content-type: multipart/form-data; boundary="),
        ("new Uint8Array([1, 2])", ""),
    ];
    for (body, expected) in cases {
        let (url, task) = payload_server(Duration::ZERO, "text/plain", Vec::new());
        evaluate(&mut runtime, &format!("var xhr = new XMLHttpRequest(); xhr.open('POST', '{url}'); xhr.send({body});"));
        let request = task.join().unwrap().to_lowercase();
        if expected.is_empty() {
            assert!(!request.contains("content-type:"), "{request}");
        } else {
            assert!(request.contains(expected), "{expected}: {request}");
        }
    }
    let (url, task) = payload_server(Duration::ZERO, "text/plain", Vec::new());
    evaluate(&mut runtime, &format!("var xhr = new XMLHttpRequest(); xhr.open('POST', '{url}'); xhr.setRequestHeader('Content-Type', 'application/x-custom'); xhr.send('body');"));
    let request = task.join().unwrap().to_lowercase();
    assert!(request.contains("content-type: application/x-custom\r\n") && !request.contains("text/plain"), "{request}");
}

#[test]
fn xhr_in_flight_request_survives_collection_and_idle_objects_collect() {
    let (url, task) = server(Duration::from_millis(40));
    let mut runtime = Runtime::new();
    runtime.expose_gc();
    evaluate(&mut runtime, &format!(r#"
        globalThis.log = []; globalThis.refs = [];
        (() => {{
            const xhr = new XMLHttpRequest();
            xhr.open('GET', '{url}');
            xhr.onload = () => log.push('load:' + xhr.responseText);
            refs.push(new WeakRef(xhr));
            xhr.send();
            const idle = new XMLHttpRequest();
            idle.onload = () => {{}};
            idle.open('GET', 'http://127.0.0.1:9/never');
            refs.push(new WeakRef(idle));
        }})();
        let rounds = 0;
        const timer = setInterval(() => {{ gc(); if (++rounds === 30) clearInterval(timer); }}, 2);
    "#));
    assert_script(&mut runtime, "log.join() === 'load:{\"answer\":42}'");
    evaluate(&mut runtime, "gc(); gc();");
    assert_script(&mut runtime, "refs.map(ref => ref.deref() === undefined).join() === 'true,true'");
    task.join().unwrap();
}

#[test]
fn synchronous_xhr_network_failure_throws_a_dom_exception() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/refused", listener.local_addr().unwrap());
    drop(listener);
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, &format!(r#"
        var sync = new XMLHttpRequest(), thrown = '', events = [];
        sync.open('GET', '{url}', false);
        for (const type of ['error', 'load', 'loadend']) sync.addEventListener(type, () => events.push(type));
        try {{ sync.send(); }} catch (e) {{ thrown = e.name + ':' + (e instanceof DOMException); }}
    "#));
    assert_script(&mut runtime, "thrown === 'NetworkError:true' && sync.readyState === 4 && sync.status === 0 && events.length === 0");
}

#[test]
fn xhr_open_validation_and_forbidden_headers() {
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, r#"
        var errors = [], xhr = new XMLHttpRequest();
        for (const run of [() => xhr.open('bad method', 'http://localhost/'), () => xhr.open('GET', 'http://'), () => xhr.setRequestHeader('X', '1'),
            () => { xhr.open('GET', 'http://localhost/'); xhr.setRequestHeader('X-Bad', 'a\nb'); }, () => { xhr.withCredentials = true; xhr.timeout = 'abc'; return xhr.timeout; }]) {
          try { errors.push(String(run())); } catch (e) { errors.push(e.name); }
        }
    "#);
    assert_script(&mut runtime, "errors.join() === 'SyntaxError,SyntaxError,InvalidStateError,SyntaxError,0'");
}

fn install_stub_transport(runtime: &mut Runtime, origin: Option<&str>) {
    let origin_member = origin
        .map(|origin| format!("browserOrigin() {{ return '{origin}'; }},"))
        .unwrap_or_default();
    evaluate(
        runtime,
        &format!(
            r#"
        globalThis.__stub = {{
          calls: [],
          handler: null,
          {origin_member}
          request(method, url, headers, body, resolve, reject, redirect, options) {{
            const call = {{ method, url, headers, body, redirect, options, resolve, reject, aborted: false }};
            __stub.calls.push(call);
            if (__stub.handler) __stub.handler(call);
            return {{ abort() {{ call.aborted = true; }} }};
          }},
        }};
        globalThis.bodyReader = (chunks) => {{
          let index = 0;
          return {{
            cancelled: 0,
            read() {{ return Promise.resolve(index < chunks.length ? new Uint8Array(chunks[index++]) : null); }},
            cancel() {{ this.cancelled++; }},
          }};
        }};
        globalThis.respond = (call, chunks, headers = [], status = 200) => call.resolve({{
          status, statusText: 'OK', url: call.url, headers, bodyReader: bodyReader(chunks),
        }});
        globalThis.out = {{}};
        "#
        ),
    );
    let ctx = runtime.engine().ctx();
    let global = ctx.global_object();
    let Ok(stub) = ctx.member_get(&global, "__stub") else {
        panic!("stub transport")
    };
    lumen_host::net::Transport::install(ctx, stub.clone(), stub.clone(), stub);
}

#[test]
fn fetch_classes_publish_the_webidl_shape() {
    let mut runtime = Runtime::new_browser();
    install_stub_transport(&mut runtime, Some("http://page.test"));
    evaluate(
        &mut runtime,
        "var own = (object, key) => Object.getOwnPropertyDescriptor(object, key); var tags = [new Headers(), new Request('http://a.test/'), new Response(), new Headers().entries()].map((value) => Object.prototype.toString.call(value)).join();",
    );
    for expression in [
        "[Headers.length, Request.length, Response.length, fetch.length].join() === '0,1,0,1'",
        "[Headers.prototype.append, Headers.prototype.delete, Headers.prototype.get, Headers.prototype.getSetCookie, Headers.prototype.has, Headers.prototype.set, Headers.prototype.forEach].map((method) => method.length).join() === '2,1,1,0,1,2,1'",
        "[Response.json.length, Response.redirect.length, Response.error.length, Request.prototype.clone.length, Response.prototype.clone.length].join() === '1,1,0,0,0'",
        "tags === '[object Headers],[object Request],[object Response],[object Headers Iterator]'",
        "Headers.prototype[Symbol.iterator] === Headers.prototype.entries",
        "['append', 'delete', 'get', 'getSetCookie', 'has', 'set', 'entries', 'keys', 'values', 'forEach'].every((key) => own(Headers.prototype, key).enumerable)",
        "['method', 'url', 'headers', 'destination', 'referrer', 'referrerPolicy', 'mode', 'credentials', 'cache', 'redirect', 'integrity', 'keepalive', 'isReloadNavigation', 'isHistoryNavigation', 'signal', 'duplex', 'body', 'bodyUsed'].every((key) => { const d = own(Request.prototype, key); return d.enumerable && typeof d.get === 'function' && d.set === undefined; })",
        "['type', 'url', 'redirected', 'status', 'ok', 'statusText', 'headers', 'body', 'bodyUsed'].every((key) => { const d = own(Response.prototype, key); return d.enumerable && typeof d.get === 'function' && d.set === undefined; })",
        "['clone', 'arrayBuffer', 'blob', 'bytes', 'formData', 'json', 'text'].every((key) => own(Request.prototype, key).enumerable && own(Response.prototype, key).enumerable)",
        "['json', 'redirect', 'error'].every((key) => typeof Response[key] === 'function') && Response.json !== Response.prototype.json",
        "(() => { const d = own(globalThis, 'fetch'); return d.enumerable && d.writable && d.configurable && typeof d.value === 'function'; })()",
        "(() => { try { own(Request.prototype, 'method').get.call({}); } catch (e) { return e.code === 'ERR_INVALID_THIS'; } return false; })()",
        "(() => { try { Headers(); } catch (e) { return e instanceof TypeError; } return false; })()",
        "(() => { const proto = Object.getPrototypeOf(new Headers().keys()); return own(proto, 'next').enumerable && !proto.hasOwnProperty('constructor') && proto[Symbol.toStringTag] === 'Headers Iterator'; })()",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn request_and_response_constructors_validate_and_default() {
    let mut runtime = Runtime::new_browser();
    install_stub_transport(&mut runtime, Some("http://page.test"));
    let throws = |class: &str, body: &str| {
        format!("(() => {{ try {{ {body} }} catch (e) {{ return e instanceof {class}; }} return false; }})()")
    };
    for expression in [
        "(() => { const r = new Request('http://a.test/x'); return [r.method, r.url, r.mode, r.credentials, r.cache, r.redirect, r.referrer, r.referrerPolicy, r.integrity, r.keepalive, r.destination, r.duplex, r.body, r.bodyUsed, r.isReloadNavigation, r.isHistoryNavigation].join('|') === 'GET|http://a.test/x|cors|same-origin|default|follow|about:client|||false||half||false|false|false'; })()",
        "(() => { const r = new Request('http://a.test/x'); return r.headers === r.headers && r.signal === r.signal && r.signal instanceof AbortSignal && !r.signal.aborted; })()",
        "new Request('http://a.test/', { method: 'patch' }).method === 'patch' && new Request('http://a.test/', { method: 'post' }).method === 'POST'",
        "(() => { const r = new Request('http://a.test/', { method: 'POST', body: 'q', headers: { 'x-a': '1' }, cache: 'no-store', credentials: 'include', redirect: 'manual', mode: 'same-origin', integrity: 'sha256-x', keepalive: true, referrerPolicy: 'origin' }); const c = new Request(r, { method: 'PUT' }); return [c.method, c.headers.get('x-a'), c.headers.get('content-type'), c.cache, c.credentials, c.redirect, c.mode, c.integrity, c.keepalive, c.referrerPolicy, r.bodyUsed].join() === 'PUT,1,text/plain;charset=UTF-8,no-store,include,manual,same-origin,sha256-x,true,origin,true'; })()",
        "(() => { const r = new Request('http://a.test/', { method: 'POST', body: 'q' }); return r.headers.get('content-type') === 'text/plain;charset=UTF-8' && new Request('http://a.test/', { method: 'POST', body: new URLSearchParams({ a: '1' }) }).headers.get('content-type') === 'application/x-www-form-urlencoded;charset=UTF-8' && new Request('http://a.test/', { method: 'POST', body: new Blob(['x'], { type: 'a/b' }) }).headers.get('content-type') === 'a/b' && new Request('http://a.test/', { method: 'POST', body: 'q', headers: { 'content-type': 'x/y' } }).headers.get('content-type') === 'x/y'; })()",
        &throws("TypeError", "new Request('http://a.test/', { mode: 'navigate' });"),
        &throws("TypeError", "new Request('http://a.test/', { mode: 'other' });"),
        &throws("TypeError", "new Request('http://a.test/', { method: 'CONNECT' });"),
        &throws("TypeError", "new Request('http://a.test/', { method: 'trace' });"),
        &throws("TypeError", "new Request('http://a.test/', { method: 'bad method' });"),
        &throws("TypeError", "new Request('http://user:pw@a.test/');"),
        &throws("TypeError", "new Request('http://a.test/', { window: 1 });"),
        &throws("TypeError", "new Request('http://a.test/', { body: 'x' });"),
        &throws("TypeError", "new Request('http://a.test/', { method: 'HEAD', body: 'x' });"),
        &throws("TypeError", "new Request('http://a.test/', { signal: {} });"),
        &throws("TypeError", "new Request('http://a.test/', { cache: 'bad' });"),
        &throws("TypeError", "new Request('http://a.test/', 5);"),
        &throws("TypeError", "new Request();"),
        &throws("TypeError", "new Request('http://a.test/', { method: 'POST', body: new ReadableStream() });"),
        "(() => { const r = new Request('http://a.test/', { method: 'POST', body: new ReadableStream(), duplex: 'half' }); return r.body instanceof ReadableStream && !r.bodyUsed; })()",
        "(() => { const r = new Request('http://a.test/', { method: 'POST', body: 'x' }); const moved = new Request(r); return r.bodyUsed && !moved.bodyUsed; })()",
        &throws("TypeError", "const r = new Request('http://a.test/', { method: 'POST', body: 'x' }); new Request(r); new Request(r);"),
        "(() => { const r = new Response(); return [r.status, r.statusText, r.type, r.url, r.redirected, r.ok, r.body, r.headers instanceof Headers].join('|') === '200||default||false|true||true'; })()",
        &throws("RangeError", "new Response(null, { status: 199 });"),
        &throws("RangeError", "new Response(null, { status: 600 });"),
        &throws("TypeError", "new Response('x', { status: 204 });"),
        &throws("TypeError", "new Response(null, { statusText: 'a\\nb' });"),
        &throws("TypeError", "new Response(null, 5);"),
        "new Response(null, { status: 204 }).status === 204 && new Response(null, { status: undefined }).status === 200",
        "(() => { const r = Response.error(); return r.type === 'error' && r.status === 0 && r.ok === false && r.body === null; })()",
        "(() => { const r = Response.redirect('http://a.test/b', 301); return r.status === 301 && r.headers.get('location') === 'http://a.test/b' && r.type === 'default'; })()",
        &throws("RangeError", "Response.redirect('http://a.test/', 200);"),
        &throws("TypeError", "Response.redirect('http://');"),
        &throws("TypeError", "Response.json(undefined);"),
        "(() => { const r = Response.json({ a: 1 }, { status: 201 }); return r.status === 201 && r.headers.get('content-type') === 'application/json'; })()",
        "new Response('x', { headers: { 'set-cookie': 'a=1', 'x-a': '1' } }).headers.has('set-cookie') === false",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn request_signal_follows_the_signal_it_was_given() {
    let mut runtime = Runtime::new_browser();
    install_stub_transport(&mut runtime, Some("http://page.test"));
    for expression in [
        "(() => { const ac = new AbortController(); const r = new Request('http://a.test/', { signal: ac.signal }); const before = r.signal !== ac.signal && !r.signal.aborted; ac.abort('why'); return before && r.signal.aborted && r.signal.reason === 'why'; })()",
        "new Request('http://a.test/', { signal: AbortSignal.abort() }).signal.aborted === true",
        "(() => { const ac = new AbortController(); const r = new Request('http://a.test/', { signal: ac.signal }); const c = r.clone(); ac.abort(); return c.signal.aborted && c.signal !== r.signal; })()",
        "(() => { const ac = new AbortController(); const r = new Request('http://a.test/', { signal: ac.signal }); const c = new Request(r, { signal: null }); ac.abort(); return !c.signal.aborted; })()",
        "(() => { const ac = new AbortController(); const r = new Request('http://a.test/', { signal: ac.signal }); const c = new Request(r); ac.abort(); return c.signal.aborted; })()",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn headers_guards_iteration_and_forbidden_names() {
    let mut runtime = Runtime::new_browser();
    install_stub_transport(&mut runtime, Some("http://page.test"));
    let throws = |body: &str| {
        format!("(() => {{ try {{ {body} }} catch (e) {{ return e instanceof TypeError; }} return false; }})()")
    };
    for expression in [
        "(() => { const h = new Headers([['B', '2'], ['a', '1'], ['b', '3']]); return [...h].map((pair) => pair.join(':')).join('|') === 'a:1|b:2, 3' && h.get('B') === '2, 3' && h.has('A') && h.get('zz') === null; })()",
        "(() => { const h = new Headers({ X: '1' }); h.set('x', ' v '); h.append('y', 'a'); h.append('y', 'b'); h.delete('nope'); return h.get('x') === 'v' && h.get('y') === 'a, b' && [...h.keys()].join() === 'x,y' && [...h.values()].join('|') === 'v|a, b'; })()",
        "(() => { const h = new Headers(new Headers([['a', '1']])); return h.get('a') === '1' && new Headers(new Map([['m', 'n']])).get('m') === 'n'; })()",
        "(() => { const h = new Headers(); h.append('Set-Cookie', 'a=1'); h.append('set-cookie', 'b=2'); h.append('x', '1'); return h.getSetCookie().join('|') === 'a=1|b=2' && [...h].map((pair) => pair.join(':')).join('|') === 'set-cookie:a=1|set-cookie:b=2|x:1' && h.get('set-cookie') === 'a=1, b=2'; })()",
        "(() => { const h = new Headers([['a', '1']]); const it = h.keys(); const first = it.next(); h.append('b', '2'); const second = it.next(); return first.value === 'a' && second.value === 'b' && it.next().done === true; })()",
        "(() => { const h = new Headers([['a', '1'], ['b', '2']]); const seen = []; const self = {}; h.forEach(function (value, name, headers) { seen.push(name + value + (this === self) + (headers === h)); }, self); return seen.join() === 'a1truetrue,b2truetrue'; })()",
        "(() => { const h = new Headers(); h.append('x', '\\u00ff'); return h.get('x') === '\\u00ff'; })()",
        &throws("new Headers([['a']]);"),
        &throws("new Headers([['a', 'b', 'c']]);"),
        &throws("new Headers(null);"),
        &throws("new Headers(5);"),
        &throws("new Headers().append('bad name', 'x');"),
        &throws("new Headers().append('', 'x');"),
        &throws("new Headers().append('a', 'x\\ny');"),
        &throws("new Headers().append('a', 'x\\u0100');"),
        &throws("new Headers().get('b@d');"),
        &throws("new Headers().append('a');"),
        "(() => { const r = new Request('http://a.test/x', { headers: { Cookie: 'a', 'X-Ok': '1', 'Sec-Fetch-Mode': 'x', Host: 'h', 'Proxy-Authorization': 'p' } }); return !r.headers.has('cookie') && !r.headers.has('sec-fetch-mode') && !r.headers.has('host') && !r.headers.has('proxy-authorization') && r.headers.get('x-ok') === '1'; })()",
        "(() => { const r = new Request('http://other.test/', { mode: 'no-cors', headers: { 'X-Custom': '1', Accept: 'text/html', 'Content-Type': 'application/json' } }); return !r.headers.has('x-custom') && r.headers.get('accept') === 'text/html' && !r.headers.has('content-type'); })()",
        &throws("new Request('http://other.test/', { mode: 'no-cors', method: 'PUT' });"),
        "(() => { const r = new Response('', { headers: { 'Set-Cookie': 'a=1', 'Set-Cookie2': 'b', 'X-A': '1' } }); return !r.headers.has('set-cookie') && !r.headers.has('set-cookie2') && r.headers.get('x-a') === '1'; })()",
        "(() => { const h = Response.error().headers; try { h.append('a', 'b'); } catch (e) { return e instanceof TypeError; } return false; })()",
        "(() => { const h = Response.redirect('http://a.test/', 302).headers; try { h.delete('location'); } catch (e) { return e instanceof TypeError && h.has('location'); } return false; })()",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn body_mixin_consumption_locking_and_cloning() {
    let mut runtime = Runtime::new_browser();
    install_stub_transport(&mut runtime, Some("http://page.test"));
    evaluate(
        &mut runtime,
        r#"
        (async () => {
          const attempt = async (promise) => { try { await promise; return 'resolved'; } catch (e) { return e.constructor.name; } };
          const response = new Response('{"a":1}', { headers: { 'content-type': 'application/json' } });
          out.before = response.bodyUsed;
          out.json = (await response.json()).a;
          out.after = response.bodyUsed;
          out.reuse = await attempt(response.text());
          out.text = await new Response('héllo \u{1f600}').text();
          out.bom = await new Response(new Uint8Array([0xef, 0xbb, 0xbf, 0x41])).text();
          out.buffer = (await new Response(new Uint8Array([1, 2, 3])).arrayBuffer()).byteLength;
          const bytes = await new Response('ab').bytes();
          out.bytes = bytes instanceof Uint8Array && bytes.join();
          const blob = await new Response('abc', { headers: { 'content-type': 'text/x' } }).blob();
          out.blob = blob.size + blob.type;
          const urlencoded = await new Response(new URLSearchParams({ a: '1', b: '2' })).formData();
          out.urlencoded = urlencoded.get('a') + urlencoded.get('b');
          const sent = new FormData(); sent.append('k', 'v'); sent.append('f', new Blob(['file'], { type: 'text/plain' }), 'n.txt');
          const multipart = await new Response(sent).formData();
          out.multipart = multipart.get('k') + multipart.get('f').name;
          out.unsupported = await attempt(new Response('x', { headers: { 'content-type': 'text/plain' } }).formData());
          out.badJson = await attempt(new Response('{').json());
          const empty = new Response(null);
          out.empty = empty.body === null && (await empty.text()) === '' && !empty.bodyUsed && (await empty.text()) === '';

          const streamed = new Response('xy');
          const stream = streamed.body;
          out.streamIdentity = stream instanceof ReadableStream && stream === streamed.body && !streamed.bodyUsed;
          const reader = stream.getReader();
          out.locked = streamed.body.locked;
          out.lockedRead = await attempt(streamed.text());
          out.lockedClone = (() => { try { streamed.clone(); return 'cloned'; } catch (e) { return e.constructor.name; } })();
          const chunk = await reader.read();
          out.chunk = chunk.value.join();
          out.usedAfterRead = streamed.bodyUsed;
          out.usedRead = await attempt(streamed.text());

          const original = new Response('same');
          const copy = original.clone();
          out.clones = (await original.text()) + (await copy.text());
          out.usedClone = (() => { try { original.clone(); return 'cloned'; } catch (e) { return e.constructor.name; } })();

          const piped = new Response(new ReadableStream({ start(c) { c.enqueue(new TextEncoder().encode('he')); c.enqueue(new TextEncoder().encode('llo')); c.close(); } }));
          const twin = piped.clone();
          out.teed = (await piped.text()) + (await twin.text());
          out.badChunk = await attempt(new Response(new ReadableStream({ start(c) { c.enqueue('text'); c.close(); } })).text());
          out.failing = await (async () => { try { await new Response(new ReadableStream({ start(c) { c.error(new RangeError('boom')); } })).text(); } catch (e) { return e.message; } })();

          const request = new Request('http://a.test/', { method: 'POST', body: 'q' });
          const moved = new Request(request);
          out.moved = request.bodyUsed + ':' + (await moved.text()) + ':' + (await attempt(request.text()));
          const post = new Request('http://a.test/', { method: 'POST', body: 'again' });
          const postClone = post.clone();
          out.requestClone = (await post.text()) + (await postClone.text());
          out.userStream = await (async () => {
            const body = new ReadableStream({ start(c) { c.enqueue(new TextEncoder().encode('ab')); c.close(); } });
            const r = new Request('http://a.test/', { method: 'POST', body, duplex: 'half' });
            return (await r.text()) + r.bodyUsed;
          })();
        })().catch((error) => { out.error = error.name + ': ' + error.message; });
        "#,
    );
    runtime.run_until_idle();
    for expression in [
        "out.error === undefined",
        "out.before === false && out.json === 1 && out.after === true && out.reuse === 'TypeError'",
        "out.text === 'h\\u00e9llo \\u{1f600}' && out.bom === 'A' && out.buffer === 3 && out.bytes === '97,98'",
        "out.blob === '3text/x' && out.urlencoded === '12' && out.multipart === 'vn.txt'",
        "out.unsupported === 'TypeError' && out.badJson === 'SyntaxError' && out.empty === true",
        "out.streamIdentity === true && out.locked === true && out.lockedRead === 'TypeError' && out.lockedClone === 'TypeError'",
        "out.chunk === '120,121' && out.usedAfterRead === true && out.usedRead === 'TypeError'",
        "out.clones === 'samesame' && out.usedClone === 'TypeError' && out.teed === 'hellohello'",
        "out.badChunk === 'TypeError' && out.failing === 'boom'",
        "out.moved === 'true:q:TypeError' && out.requestClone === 'againagain' && out.userStream === 'abtrue'",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn bytes_bodies_do_not_load_the_streams_glue_until_body_is_read() {
    let mut runtime = Runtime::new_browser();
    install_stub_transport(&mut runtime, Some("http://page.test"));
    evaluate(
        &mut runtime,
        r#"
        var glueRuns = 0;
        const nativeDefine = Object.defineProperty;
        Object.defineProperty = function (target, key, descriptor) {
          if (key === Symbol.for('lumen.cloneBody')) glueRuns++;
          return nativeDefine.call(this, target, key, descriptor);
        };
        var done = [];
        (async () => {
          const response = new Response('abc');
          const copy = response.clone();
          done.push(await response.text(), await copy.text(), await new Request('http://a.test/', { method: 'POST', body: 'x' }).clone().text());
          done.push(glueRuns);
        })().catch((error) => done.push(error.message));
        "#,
    );
    runtime.run_until_idle();
    assert_script(&mut runtime, "done.join() === 'abc,abc,x,0'");
    assert_script(&mut runtime, "(() => { const r = new Response('abc'); return r.body instanceof ReadableStream && glueRuns === 1; })()");
}

#[test]
fn fetch_runs_over_the_transport_and_exposes_the_response() {
    let mut runtime = Runtime::new();
    install_stub_transport(&mut runtime, None);
    evaluate(
        &mut runtime,
        r#"
        __stub.handler = (call) => respond(call, [[104, 105], [33]], [['content-type', 'text/plain'], ['x-a', '1'], ['x-a', '2']]);
        fetch('http://api.test/x?q=1#frag', { method: 'POST', body: 'data', headers: { 'X-Req': '1' } }).then(async (response) => {
          out.status = response.status + response.statusText;
          out.type = response.type;
          out.url = response.url;
          out.ok = response.ok;
          out.redirected = response.redirected;
          out.header = response.headers.get('x-a');
          out.immutable = (() => { try { response.headers.set('a', 'b'); } catch (e) { return e instanceof TypeError; } return false; })();
          out.text = await response.text();
          out.bodyUsed = response.bodyUsed;
        }).catch((error) => { out.error = error.name + ': ' + error.message; });
        "#,
    );
    runtime.run_until_idle();
    for expression in [
        "out.error === undefined",
        "out.status === '200OK' && out.type === 'basic' && out.url === 'http://api.test/x?q=1' && out.ok && out.redirected === false",
        "out.header === '1, 2' && out.immutable === true && out.text === 'hi!' && out.bodyUsed === true",
        "__stub.calls.length === 1 && __stub.calls[0].method === 'POST' && __stub.calls[0].redirect === 'follow'",
        "new TextDecoder().decode(__stub.calls[0].body) === 'data'",
        "__stub.calls[0].headers.map((pair) => pair.join(':')).join() === 'content-type:text/plain;charset=UTF-8,x-req:1'",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn fetch_reads_request_bodies_of_every_kind_and_fails_with_the_source_error() {
    let mut runtime = Runtime::new();
    install_stub_transport(&mut runtime, None);
    evaluate(
        &mut runtime,
        r#"
        __stub.handler = (call) => respond(call, []);
        (async () => {
          const stream = new ReadableStream({ start(c) { c.enqueue(new TextEncoder().encode('ab')); c.enqueue(new TextEncoder().encode('cd')); c.close(); } });
          await fetch('http://api.test/stream', { method: 'POST', body: stream, duplex: 'half' });
          const form = new FormData(); form.append('k', 'v');
          await fetch('http://api.test/form', { method: 'POST', body: form });
          await fetch('http://api.test/params', { method: 'PUT', body: new URLSearchParams({ a: '1' }) });
          await fetch(new Request('http://api.test/request', { method: 'POST', body: new Uint8Array([1, 2]) }));
          out.failure = await fetch('http://api.test/fail', { method: 'POST', body: new ReadableStream({ start(c) { c.error(new RangeError('source failed')); } }), duplex: 'half' }).then(() => 'resolved', (e) => e.message);
          out.rejected = await fetch('relative-without-base').then(() => 'resolved', (e) => e.constructor.name);
          out.noArgs = (() => { try { fetch(); } catch (e) { return e.constructor.name; } return 'no throw'; })();
        })().catch((error) => { out.error = error.name + ': ' + error.message; });
        "#,
    );
    runtime.run_until_idle();
    for expression in [
        "out.error === undefined && out.failure === 'source failed' && out.rejected === 'TypeError'",
        "new TextDecoder().decode(__stub.calls[0].body) === 'abcd'",
        "__stub.calls[1].headers.some(([name, value]) => name === 'content-type' && value.startsWith('multipart/form-data; boundary=')) && __stub.calls[1].body.length > 0",
        "new TextDecoder().decode(__stub.calls[2].body) === 'a=1' && __stub.calls[2].method === 'PUT'",
        "__stub.calls[3].url === 'http://api.test/request' && __stub.calls[3].body.join() === '1,2'",
        "__stub.calls.length === 4",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn fetch_abort_rejects_cancels_the_transport_and_errors_open_bodies() {
    let mut runtime = Runtime::new();
    install_stub_transport(&mut runtime, None);
    evaluate(
        &mut runtime,
        r#"
        globalThis.stalled = { cancelled: 0, read() { return new Promise(() => {}); }, cancel() { this.cancelled++; } };
        (async () => {
          const attempt = (promise) => promise.then(() => 'resolved', (e) => (e && e.name) || String(e));

          const early = new AbortController();
          const pending = fetch('http://api.test/pending', { signal: early.signal });
          early.abort();
          out.pending = await attempt(pending);
          out.pendingAborted = __stub.calls[0].aborted;

          const reason = new AbortController();
          const withReason = fetch('http://api.test/reason', { signal: reason.signal });
          reason.abort('custom');
          out.reason = await attempt(withReason);

          out.preAborted = await attempt(fetch('http://api.test/never', { signal: AbortSignal.abort() }));
          out.callsAfterPre = __stub.calls.length;

          __stub.handler = (call) => call.resolve({ status: 200, statusText: 'OK', url: call.url, headers: [], bodyReader: stalled });
          const reading = new AbortController();
          const response = await fetch('http://api.test/body', { signal: reading.signal });
          const text = attempt(response.text());
          reading.abort();
          out.bodyRead = await text;
          out.cancelled = stalled.cancelled;

          const streamed = new AbortController();
          const second = await fetch('http://api.test/stream', { signal: streamed.signal });
          const streamReader = second.body.getReader();
          const streamRead = attempt(streamReader.read());
          streamed.abort();
          out.streamRead = await streamRead;

          __stub.handler = (call) => respond(call, [[111, 107]]);
          const finished = new AbortController();
          const done = await fetch('http://api.test/done', { signal: finished.signal });
          const doneText = await done.text();
          finished.abort();
          out.afterBody = doneText;

          const shared = new AbortController();
          const first = await fetch('http://api.test/shared-1', { signal: shared.signal });
          await first.text();
          const sharedSecond = await fetch('http://api.test/shared-2', { signal: shared.signal });
          shared.abort();
          out.sharedSecond = await attempt(sharedSecond.text());
          out.sharedFirst = first.bodyUsed;
        })().catch((error) => { out.error = error.name + ': ' + error.message; });
        "#,
    );
    runtime.run_until_idle();
    for expression in [
        "out.error === undefined",
        "out.pending === 'AbortError' && out.pendingAborted === true && out.reason === 'custom'",
        "out.preAborted === 'AbortError' && out.callsAfterPre === 2",
        "out.bodyRead === 'AbortError' && out.cancelled >= 1",
        "out.streamRead === 'AbortError'",
        "out.afterBody === 'ok'",
        "out.sharedSecond === 'AbortError' && out.sharedFirst === true",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn fetch_redirect_modes_in_direct_mode() {
    let mut runtime = Runtime::new();
    install_stub_transport(&mut runtime, None);
    evaluate(
        &mut runtime,
        r#"
        __stub.handler = (call) => call.resolve({ status: 302, statusText: 'Found', url: call.url, headers: [['location', '/next']] });
        (async () => {
          const attempt = (promise) => promise.then((r) => r, (e) => e);
          const manual = await fetch('http://api.test/a', { redirect: 'manual' });
          out.manual = [manual.type, manual.status, manual.url, manual.body, manual.headers.has('location')].join();
          const error = await attempt(fetch('http://api.test/a', { redirect: 'error' }));
          out.error = error instanceof TypeError;
          const follow = await fetch('http://api.test/a');
          out.follow = follow.status + ':' + follow.headers.get('location');
          out.modes = __stub.calls.map((call) => call.redirect).join();
        })().catch((error) => { out.failure = error.name + ': ' + error.message; });
        "#,
    );
    runtime.run_until_idle();
    for expression in [
        "out.failure === undefined",
        "out.manual === 'opaqueredirect,0,,,false'",
        "out.error === true",
        "out.follow === '302:/next'",
        "out.modes === 'manual,manual,follow'",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn fetch_applies_cors_policy_and_follows_redirects_in_a_browsing_context() {
    let mut runtime = Runtime::new_browser();
    install_stub_transport(&mut runtime, Some("http://page.test"));
    evaluate(
        &mut runtime,
        r#"
        (async () => {
          const attempt = (promise) => promise.then((r) => r, (e) => e);
          __stub.handler = (call) => {
            if (call.url === 'http://page.test/a') call.resolve({ status: 302, statusText: 'Found', url: call.url, headers: [['location', '/b']] });
            else respond(call, [[111]], [['content-type', 'text/plain']]);
          };
          const followed = await fetch('http://page.test/a');
          out.followed = [followed.url, followed.redirected, followed.type, await followed.text(), __stub.calls.length].join();
          const refused = await attempt(fetch('http://page.test/a', { redirect: 'error' }));
          out.refused = refused instanceof TypeError;
          const manual = await fetch('http://page.test/a', { redirect: 'manual' });
          out.manual = manual.type + manual.status;

          __stub.calls.length = 0;
          __stub.handler = (call) => respond(call, [[1]], [['content-type', 'text/plain'], ['x-secret', '1']]);
          out.blocked = (await attempt(fetch('http://other.test/data'))) instanceof TypeError;
          __stub.handler = (call) => respond(call, [[1]], [['access-control-allow-origin', '*'], ['content-type', 'text/plain'], ['x-secret', '1']]);
          const cors = await fetch('http://other.test/data');
          out.cors = [cors.type, cors.headers.has('x-secret'), cors.headers.has('content-type')].join();
          const noCors = await fetch('http://other.test/data', { mode: 'no-cors' });
          out.noCors = [noCors.type, noCors.status, noCors.url, noCors.body, [...noCors.headers].length].join();
          out.sameOriginMode = (await attempt(fetch('http://other.test/data', { mode: 'same-origin' }))) instanceof TypeError;
          out.origin = __stub.calls.filter((call) => call.url.startsWith('http://other.test') && call.options.mode === 'cors').every((call) => call.headers.some(([name, value]) => name === 'origin' && value === 'http://page.test'));
        })().catch((error) => { out.failure = error.name + ': ' + error.message; });
        "#,
    );
    runtime.run_until_idle();
    for expression in [
        "out.failure === undefined",
        "out.followed === 'http://page.test/b,true,basic,o,2' && out.refused === true && out.manual === 'opaqueredirect0'",
        "out.blocked === true && out.cors === 'cors,false,true'",
        "out.noCors === 'opaque,0,,,0' && out.sameOriginMode === true && out.origin === true",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn fetch_responses_and_in_flight_fetches_are_collectable() {
    let mut runtime = Runtime::new();
    install_stub_transport(&mut runtime, None);
    evaluate(
        &mut runtime,
        r#"
        __stub.handler = (call) => respond(call, [[1, 2], [3]]);
        globalThis.weak = {};
        // Defined outside the async function: a closure created inside it may keep the
        // function's whole scope (and every local below) alive while the fetch is pending.
        const ignore = () => {};
        (async () => {
          const controller = new AbortController();
          const response = await fetch('http://api.test/gc', { signal: controller.signal });
          const stream = response.body;
          weak.response = new WeakRef(response);
          weak.stream = new WeakRef(stream);
          weak.headers = new WeakRef(response.headers);
          weak.signal = new WeakRef(controller.signal);
          await response.clone().bytes();
          const fresh = new Response('x');
          weak.fresh = new WeakRef(fresh);
          weak.freshHeaders = new WeakRef(fresh.headers);
          const request = new Request('http://api.test/', { signal: controller.signal });
          weak.request = new WeakRef(request);
          weak.followed = new WeakRef(request.signal);
          __stub.handler = null;
          fetch('http://api.test/never', { signal: controller.signal }).catch(ignore);
          out.settled = true;
        })().catch((error) => { out.error = error.name + ': ' + error.message; });
        "#,
    );
    runtime.run_until_idle();
    assert_script(&mut runtime, "out.error === undefined && out.settled === true");
    runtime.engine().collect_garbage();
    runtime.engine().collect_garbage();
    for expression in [
        "weak.response.deref() === undefined",
        "weak.stream.deref() === undefined",
        "weak.headers.deref() === undefined",
        "weak.fresh.deref() === undefined && weak.freshHeaders.deref() === undefined",
        "weak.request.deref() === undefined && weak.followed.deref() === undefined",
        "weak.signal.deref() === undefined",
    ] {
        assert_script(&mut runtime, expression);
    }
}

const WASM_PRELUDE: &str = r#"
const wasm = (...sections) => new Uint8Array([0, 0x61, 0x73, 0x6d, 1, 0, 0, 0, ...sections.flat()]);
const ADD = wasm(
  [0x01, 0x07, 0x01, 0x60, 0x02, 0x7f, 0x7f, 0x01, 0x7f],
  [0x03, 0x02, 0x01, 0x00],
  [0x07, 0x07, 0x01, 0x03, 0x61, 0x64, 0x64, 0x00, 0x00],
  [0x0a, 0x09, 0x01, 0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x6a, 0x0b]);
const IMPORTS = wasm(
  [0x01, 0x08, 0x02, 0x60, 0x01, 0x7f, 0x00, 0x60, 0x00, 0x00],
  [0x02, 0x16, 0x02, 0x03, 0x65, 0x6e, 0x76, 0x03, 0x6c, 0x6f, 0x67, 0x00, 0x00,
   0x03, 0x65, 0x6e, 0x76, 0x03, 0x6d, 0x65, 0x6d, 0x02, 0x00, 0x01],
  [0x03, 0x02, 0x01, 0x01],
  [0x07, 0x07, 0x01, 0x03, 0x72, 0x75, 0x6e, 0x00, 0x01],
  [0x0a, 0x08, 0x01, 0x06, 0x00, 0x41, 0x07, 0x10, 0x00, 0x0b]);
const GLOBAL = wasm(
  [0x06, 0x06, 0x01, 0x7f, 0x01, 0x41, 0x05, 0x0b],
  [0x07, 0x05, 0x01, 0x01, 0x67, 0x03, 0x00]);
const MEMORY = wasm(
  [0x05, 0x03, 0x01, 0x00, 0x01],
  [0x07, 0x07, 0x01, 0x03, 0x6d, 0x65, 0x6d, 0x02, 0x00]);
const TRAP_AT_START = wasm(
  [0x01, 0x04, 0x01, 0x60, 0x00, 0x00],
  [0x03, 0x02, 0x01, 0x00],
  [0x08, 0x01, 0x00],
  [0x0a, 0x05, 0x01, 0x03, 0x00, 0x00, 0x0b]);
const caught = (fn) => { try { fn(); } catch (error) { return error; } };
"#;

fn wasm_runtime() -> Runtime {
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, &format!("{WASM_PRELUDE} globalThis.wasm = wasm; globalThis.ADD = ADD; globalThis.IMPORTS = IMPORTS; globalThis.GLOBAL = GLOBAL; globalThis.MEMORY = MEMORY; globalThis.TRAP_AT_START = TRAP_AT_START; globalThis.caught = caught;"));
    runtime
}

#[test]
fn webassembly_namespace_and_classes_keep_their_shape() {
    let mut runtime = wasm_runtime();
    for expression in [
        "typeof WebAssembly === 'object' && Object.getPrototypeOf(WebAssembly) === Object.prototype",
        "(() => { const d = Object.getOwnPropertyDescriptor(globalThis, 'WebAssembly'); return d.writable && !d.enumerable && d.configurable && d.value === WebAssembly; })()",
        "['validate', 'compile', 'instantiate'].every((n) => typeof WebAssembly[n] === 'function' && Object.getOwnPropertyDescriptor(WebAssembly, n).enumerable)",
        "['Module', 'Instance', 'Memory', 'Table', 'Global', 'CompileError', 'LinkError', 'RuntimeError'].every((n) => typeof WebAssembly[n] === 'function' && WebAssembly[n].name === n)",
        "caught(() => WebAssembly.Module(ADD)) instanceof TypeError",
        "caught(() => new WebAssembly.Module()) instanceof TypeError",
        "caught(() => new WebAssembly.Module(5)) instanceof TypeError",
        "caught(() => new WebAssembly.Instance({})) instanceof TypeError",
        "caught(() => WebAssembly.validate('x')) instanceof TypeError",
        "WebAssembly.validate(ADD) === true && WebAssembly.validate(new Uint8Array(4)) === false && WebAssembly.validate(ADD.buffer) === true",
        "(() => { for (const n of ['CompileError', 'LinkError', 'RuntimeError']) { const C = WebAssembly[n]; const e = new C('boom'); if (!(e instanceof Error && e instanceof C && e.name === n && e.message === 'boom' && Object.prototype.hasOwnProperty.call(e, 'message') && !Object.prototype.propertyIsEnumerable.call(e, 'message') && C.prototype.name === n && Object.getPrototypeOf(C.prototype) === Error.prototype && String(e) === n + ': boom' && new C().message === '')) return false; } return true; })()",
        "Object.getPrototypeOf(WebAssembly.CompileError) === Error",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn webassembly_module_and_instance_expose_exports_and_call_wasm() {
    let mut runtime = wasm_runtime();
    for expression in [
        "(() => { const m = new WebAssembly.Module(ADD); const i = new WebAssembly.Instance(m); return i instanceof WebAssembly.Instance && i.exports.add(2, 3) === 5 && i.exports.add(-10, 4) === -6; })()",
        "(() => { const m = new WebAssembly.Module(ADD); const e = WebAssembly.Module.exports(m); return e.length === 1 && e[0].name === 'add' && e[0].kind === 'function' && WebAssembly.Module.imports(m).length === 0 && WebAssembly.Module.customSections(m, 'x').length === 0; })()",
        "(() => { const m = new WebAssembly.Module(IMPORTS); const i = WebAssembly.Module.imports(m); return i.length === 2 && i[0].module === 'env' && i[0].name === 'log' && i[0].kind === 'function' && i[1].name === 'mem' && i[1].kind === 'memory'; })()",
        "caught(() => WebAssembly.Module.exports({})) instanceof TypeError",
        "(() => { const e = new WebAssembly.Instance(new WebAssembly.Module(ADD)).exports; return Object.getPrototypeOf(e) === null && Object.isFrozen(e) && Object.keys(e).join() === 'add' && e.add.length === 2 && e.add === e.add; })()",
        "(() => { const i = new WebAssembly.Instance(new WebAssembly.Module(ADD)); const d = Object.getOwnPropertyDescriptor(WebAssembly.Instance.prototype, 'exports'); return typeof d.get === 'function' && d.get.call(i) === i.exports && caught(() => d.get.call({})) instanceof TypeError; })()",
        "(() => { const m = new WebAssembly.Module(ADD); return new WebAssembly.Instance(m).exports.add !== new WebAssembly.Instance(m).exports.add; })()",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn webassembly_imports_link_functions_memories_and_report_errors() {
    let mut runtime = wasm_runtime();
    for expression in [
        "(() => { const seen = []; const mem = new WebAssembly.Memory({ initial: 1 }); const i = new WebAssembly.Instance(new WebAssembly.Module(IMPORTS), { env: { log: (v) => seen.push(v), mem } }); i.exports.run(); return seen.join() === '7'; })()",
        "caught(() => new WebAssembly.Instance(new WebAssembly.Module(IMPORTS))) instanceof TypeError",
        "caught(() => new WebAssembly.Instance(new WebAssembly.Module(IMPORTS), { env: 1 })) instanceof TypeError",
        "caught(() => new WebAssembly.Instance(new WebAssembly.Module(IMPORTS), { env: { log: 1, mem: new WebAssembly.Memory({ initial: 1 }) } })) instanceof WebAssembly.LinkError",
        "caught(() => new WebAssembly.Instance(new WebAssembly.Module(IMPORTS), { env: { log() {}, mem: {} } })) instanceof WebAssembly.LinkError",
        "(() => { const e = caught(() => new WebAssembly.Instance(new WebAssembly.Module(TRAP_AT_START))); return e instanceof WebAssembly.RuntimeError && e instanceof Error && e.name === 'RuntimeError'; })()",
        "(() => { const thrown = new Error('from import'); const i = new WebAssembly.Instance(new WebAssembly.Module(IMPORTS), { env: { log() { throw thrown; }, mem: new WebAssembly.Memory({ initial: 1 }) } }); return caught(() => i.exports.run()) === thrown; })()",
        "(() => { const e = caught(() => new WebAssembly.Module(new Uint8Array([1, 2, 3, 4]))); return e instanceof WebAssembly.CompileError && e.name === 'CompileError' && e.message.length > 0; })()",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn webassembly_memory_table_and_global_validate_and_share_identity() {
    let mut runtime = wasm_runtime();
    for expression in [
        "(() => { const m = new WebAssembly.Memory({ initial: 1, maximum: 2 }); const b = m.buffer; const previous = m.grow(1); return previous === 1 && b.byteLength === 0 && m.buffer.byteLength === 131072 && m.buffer === m.buffer && caught(() => m.grow(1)) instanceof RangeError; })()",
        "caught(() => new WebAssembly.Memory()) instanceof TypeError && caught(() => new WebAssembly.Memory({})) instanceof TypeError && caught(() => new WebAssembly.Memory({ initial: -1 })) instanceof TypeError && caught(() => new WebAssembly.Memory({ initial: 2, maximum: 1 })) instanceof RangeError && caught(() => new WebAssembly.Memory({ initial: 1n })) instanceof TypeError",
        "caught(() => WebAssembly.Memory.prototype.grow.call({}, 1)) instanceof TypeError && caught(() => Object.getOwnPropertyDescriptor(WebAssembly.Memory.prototype, 'buffer').get.call({})) instanceof TypeError",
        "(() => { const i = new WebAssembly.Instance(new WebAssembly.Module(MEMORY)); return i.exports.mem instanceof WebAssembly.Memory && i.exports.mem.buffer.byteLength === 65536 && i.exports.mem === i.exports.mem; })()",
        "(() => { const m = new WebAssembly.Memory({ initial: 1 }); const i = new WebAssembly.Instance(new WebAssembly.Module(IMPORTS), { env: { log() {}, mem: m } }); return i.exports.run !== undefined; })()",
        "(() => { const t = new WebAssembly.Table({ element: 'anyfunc', initial: 2 }); const f = new WebAssembly.Instance(new WebAssembly.Module(ADD)).exports.add; t.set(0, f); return t.length === 2 && t.get(0) === f && t.get(1) === null && caught(() => t.set(1, () => {})) instanceof TypeError && caught(() => t.get(9)) instanceof RangeError && (t.set(0, null), t.get(0) === null); })()",
        "caught(() => new WebAssembly.Table()) instanceof TypeError && caught(() => new WebAssembly.Table({ element: 'externref', initial: 1 })) instanceof TypeError",
        "(() => { const g = new WebAssembly.Global({ value: 'i32', mutable: true }, 3); g.value = 9; return g.value === 9 && g.valueOf() === 9 && g instanceof WebAssembly.Global; })()",
        "(() => { const g = new WebAssembly.Global({ value: 'i64' }, 5n); return g.value === 5n && caught(() => { g.value = 1n; }) instanceof TypeError; })()",
        "caught(() => new WebAssembly.Global({ value: 'bogus' })) instanceof TypeError && caught(() => new WebAssembly.Global()) instanceof TypeError",
        "(() => { const i = new WebAssembly.Instance(new WebAssembly.Module(GLOBAL)); const g = i.exports.g; if (g.value !== 5) return false; g.value = 8; return g.value === 8 && g === i.exports.g; })()",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn webassembly_compile_and_instantiate_return_promises() {
    let mut runtime = wasm_runtime();
    evaluate(
        &mut runtime,
        r#"
        globalThis.out = {};
        const record = (name) => (value) => { out[name] = value; };
        const fail = (name) => (error) => { out[name] = error; };
        const compiled = WebAssembly.compile(ADD);
        out.promise = compiled instanceof Promise;
        compiled.then(record('compiled'), fail('compiledError'));
        WebAssembly.instantiate(ADD).then(record('pair'), fail('pairError'));
        WebAssembly.compile(ADD).then((m) => WebAssembly.instantiate(m)).then(record('instance'), fail('instanceError'));
        WebAssembly.compile(new Uint8Array(4)).then(fail('compileUnexpected'), record('compileRejected'));
        WebAssembly.compile('text').then(fail('typeUnexpected'), record('typeRejected'));
        WebAssembly.instantiate(IMPORTS, {}).then(fail('importUnexpected'), record('importRejected'));
        WebAssembly.instantiate(new WebAssembly.Module(IMPORTS), { env: { log: 1, mem: new WebAssembly.Memory({ initial: 1 }) } })
          .then(fail('linkUnexpected'), record('linkRejected'));
        "#,
    );
    runtime.run_until_idle();
    for expression in [
        "out.promise === true && out.compiled instanceof WebAssembly.Module",
        "out.pair.module instanceof WebAssembly.Module && out.pair.instance instanceof WebAssembly.Instance && out.pair.instance.exports.add(4, 5) === 9",
        "out.instance instanceof WebAssembly.Instance && out.instance.exports.add(1, 1) === 2",
        "out.compileRejected instanceof WebAssembly.CompileError && out.compileUnexpected === undefined",
        "out.typeRejected instanceof TypeError",
        "out.importRejected instanceof TypeError",
        "out.linkRejected instanceof WebAssembly.LinkError",
    ] {
        assert_script(&mut runtime, expression);
    }
}

#[test]
fn webassembly_wrappers_are_collected_and_exports_outlive_their_instance() {
    let mut runtime = wasm_runtime();
    evaluate(
        &mut runtime,
        r#"
        globalThis.weak = {};
        globalThis.kept = (() => {
          const module = new WebAssembly.Module(ADD);
          const instance = new WebAssembly.Instance(module);
          weak.module = new WeakRef(module);
          weak.instance = new WeakRef(instance);
          weak.memory = new WeakRef(new WebAssembly.Memory({ initial: 1 }));
          return instance.exports;
        })();
        "#,
    );
    runtime.engine().collect_garbage();
    runtime.engine().collect_garbage();
    for expression in [
        "weak.module.deref() === undefined && weak.instance.deref() === undefined && weak.memory.deref() === undefined",
        "kept.add(20, 22) === 42",
    ] {
        assert_script(&mut runtime, expression);
    }
}
