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
        for(const fn of [()=>xhr.send(),()=>xhr.open('TRACE','http://localhost'),()=>xhr.open('GET','http://localhost',false)]) {
          try{fn()}catch(e){errors.push(e.name)}
        }
        xhr.open('GET','http://localhost');xhr.responseType='arraybuffer';
        try{void xhr.responseText}catch(e){errors.push(e.name)}
        var p=new ProgressEvent('progress',{loaded:12,total:20,lengthComputable:true});
    "#);
    assert_script(&mut runtime, "errors.join(',')==='InvalidStateError,SecurityError,NotSupportedError,InvalidStateError' && p instanceof Event && p.loaded===12 && p.total===20 && p.lengthComputable && xhr.upload instanceof XMLHttpRequestUpload && xhr.DONE===XMLHttpRequest.DONE");
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
