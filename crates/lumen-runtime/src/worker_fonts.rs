//! Bounded font I/O for worker and document owners. The owner keeps only Rust request/cache state; blocking transport
//! uses the existing configured Fetch/CORS path and returns through the normal task registry.
use lumen_host::{CompletionSender, Ctx, Value};
use lumen_html::css::{FontFaceRule, FontFaceSource};
use lumen_html_js::FontResourceLoader;
use lumen_html_text::{FontFace, MAX_MANUAL_FONT_BYTES_PER_FACE};
use std::{any::Any, cell::RefCell, rc::Rc, sync::Arc, task::Poll};

const MAX_REQUESTS: usize = 64;
const MAX_ACTIVE: usize = 4;
// FontFace's maintained decoder also caps the normalized (WOFF-expanded) font
// bytes at MAX_MANUAL_FONT_BYTES_PER_FACE. Charge each ready slot at that
// maximum, conservatively bounding retained decoded font data to 16 MiB.
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_READY: usize = MAX_CACHE_BYTES / MAX_MANUAL_FONT_BYTES_PER_FACE;

const MAX_SOURCES: usize = 32;

#[derive(Clone, PartialEq, Eq)]
struct Key {
    // Share the validated descriptors' immutable strings instead of copying source lists.
    sources: Arc<[FontFaceSource]>,
    base: Arc<str>,
    origin: Arc<str>,
    ascent: Option<u32>,
    descent: Option<u32>,
}

enum State {
    Queued,
    Pending,
    Ready(Result<Arc<FontFace>, String>),
}

struct Entry {
    key: Key,
    state: State,
}

#[derive(Clone)]
pub struct FontResourceRequests {
    config: lumen_web::FetchConfig,
    entries: Rc<RefCell<Vec<Entry>>>,
    document: Option<std::rc::Weak<lumen_html_js::DomRealm>>,
    request_budget: std::time::Duration,
}

pub(crate) type WorkerFontLoader = FontResourceRequests;

impl FontResourceRequests {
    pub(crate) fn new(config: lumen_web::FetchConfig) -> Self {
        Self { config, entries: Rc::new(RefCell::new(Vec::new())), document: None, request_budget: std::time::Duration::from_secs(15) }
    }

    /// Reuse the configured Fetch/CORS transport and native completion owner
    /// for a document. The weak reference avoids retaining a retired realm.
    pub fn for_document(config: lumen_web::FetchConfig, document: &Rc<lumen_html_js::DomRealm>, request_budget: std::time::Duration) -> Self {
        Self { config, entries: Rc::new(RefCell::new(Vec::new())), document: Some(Rc::downgrade(document)), request_budget }
    }

    fn key(rule: &FontFaceRule, base: &str) -> Key {
        Key {
            sources: rule.sources.clone(),
            base: rule.source_url.clone().unwrap_or_else(|| base.into()),
            origin: lumen_common::url::parse_url(base, None).map_or_else(|| Arc::from("null"), |url| Arc::from(url.origin())),
            ascent: rule.ascent_override.map(f32::to_bits),
            descent: rule.descent_override.map(f32::to_bits),
        }
    }

    fn result(&self, key: &Key) -> Poll<Result<Arc<FontFace>, String>> {
        let entries = self.entries.borrow();
        match entries.iter().find(|entry| entry.key == *key).map(|entry| &entry.state) {
            Some(State::Ready(result)) => Poll::Ready(result.clone()),
            _ => Poll::Pending,
        }
    }

    fn start_queued(&self, ctx: &mut Ctx) {
        let Some(sender) = ctx.op_state().get::<CompletionSender>().cloned() else { return };
        loop {
            let key = {
                let mut entries = self.entries.borrow_mut();
                if entries.iter().filter(|entry| matches!(entry.state, State::Pending)).count() >= MAX_ACTIVE { return; }
                let Some(entry) = entries.iter_mut().find(|entry| matches!(entry.state, State::Queued)) else { return };
                entry.state = State::Pending;
                entry.key.clone()
            };
            let id = lumen_host::register_native_task(ctx, complete);
            let config = self.config.clone();
            let budget = self.request_budget;
            sender.run_blocking(id, move || Box::new(Completed { result: load_with_budget(&key, &config, budget), key }));
        }
    }
}

impl FontResourceLoader for FontResourceRequests {
    fn load(&self, rule: &FontFaceRule, base: &str) -> Result<Arc<FontFace>, String> {
        match self.result(&Self::key(rule, base)) {
            Poll::Ready(result) => result,
            Poll::Pending => Err("worker font loading requires an owner turn".into()),
        }
    }

    fn poll_load(&self, rule: &FontFaceRule, base: &str) -> Poll<Result<Arc<FontFace>, String>> {
        self.result(&Self::key(rule, base))
    }

    fn poll_load_in_context(&self, ctx: &mut Ctx, rule: &FontFaceRule, base: &str) -> Poll<Result<Arc<FontFace>, String>> {
        if ctx.op_state().get::<CompletionSender>().is_none() { return Poll::Ready(Err("worker font loading requires a completion owner".into())); }
        if rule.sources.len() > MAX_SOURCES { return Poll::Ready(Err("font source list exceeds resource budget".into())); }
        let key = Self::key(rule, base);
        {
            let mut entries = self.entries.borrow_mut();
            if !entries.iter().any(|entry| entry.key == key) {
                if entries.len() >= MAX_REQUESTS {
                    if let Some(index) = entries.iter().position(|entry| matches!(entry.state, State::Ready(_))) {
                        entries.remove(index);
                    } else { return Poll::Ready(Err("font request budget exhausted".into())); }
                }
                if entries.try_reserve(1).is_err() { return Poll::Ready(Err("font request allocation failed".into())); }
                entries.push(Entry { key: key.clone(), state: State::Queued });
            }
        }
        // A completion decoder obtains this exact owner cache; it cannot mutate another worker.
        ctx.op_state().put(self.clone());
        self.start_queued(ctx);
        self.result(&key)
    }

    fn loaded(&self, rule: &FontFaceRule, base: &str) -> Option<Arc<FontFace>> {
        match self.result(&Self::key(rule, base)) {
            Poll::Ready(Ok(face)) => Some(face),
            _ => None,
        }
    }
}

struct Completed {
    key: Key,
    result: Result<Arc<FontFace>, String>,
}

fn complete(ctx: &mut Ctx, payload: Box<dyn Any + Send>) -> Result<Vec<Value>, Value> {
    let Ok(done) = payload.downcast::<Completed>() else { return Ok(Vec::new()) };
    let Some(loader) = ctx.op_state().get::<WorkerFontLoader>().cloned() else { return Ok(Vec::new()) };
    {
        let mut entries = loader.entries.borrow_mut();
        if let Some(entry) = entries.iter_mut().find(|entry| entry.key == done.key) {
            entry.state = State::Ready(done.result);
        }
    }
    // Settle promises and registry generations before evicting the temporary decoded cache.
    if let Some(document) = &loader.document {
        if let Some(document) = document.upgrade() {
            document.queue_font_tasks(ctx).map_err(|error| error.to_value(ctx))?;
        }
    } else {
        lumen_html_js::poll_worker_fonts(ctx).map_err(|error| Value::str(error.to_string()))?;
    }
    {
        let mut entries = loader.entries.borrow_mut();
        while entries.iter().filter(|entry| matches!(entry.state, State::Ready(_))).count() > MAX_READY {
            let index = entries.iter().position(|entry| matches!(entry.state, State::Ready(_))).unwrap();
            entries.remove(index);
        }
        if entries.is_empty() { entries.shrink_to_fit(); }
    }
    loader.start_queued(ctx);
    Ok(Vec::new())
}

#[cfg(test)]
fn load(key: &Key, config: &lumen_web::FetchConfig) -> Result<Arc<FontFace>, String> {
    load_with_budget(key, config, std::time::Duration::from_secs(15))
}

fn load_with_budget(key: &Key, config: &lumen_web::FetchConfig, budget: std::time::Duration) -> Result<Arc<FontFace>, String> {
    let started = std::time::Instant::now();
    let base = lumen_common::url::parse_url(&key.base, None);
    let mut last_error = "no supported font source".to_string();
    for source in key.sources.iter() {
        if started.elapsed() >= budget { return Err("font resource deadline exceeded".into()); }
        let FontFaceSource::Url(source) = source else { continue };
        let bytes = if source.get(..5).is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:")) {
            match lumen_common::url::data_url_body_bounded(source, MAX_MANUAL_FONT_BYTES_PER_FACE) {
                Ok(bytes) => bytes,
                Err(_) => { last_error = "invalid or oversized data font".into(); continue; }
            }
        } else {
            let Some(url) = lumen_common::url::parse_url(source, base.as_ref()) else { last_error = "invalid font URL".into(); continue };
            if !matches!(url.scheme.as_str(), "http" | "https") { last_error = "unsupported font URL scheme".into(); continue; }
            let remaining = budget.saturating_sub(started.elapsed()).as_millis().clamp(1, u32::MAX as u128) as u32;
            match lumen_web::load_cors_resource_with_config(&url.href(), &key.origin, config, MAX_MANUAL_FONT_BYTES_PER_FACE, remaining) {
                Ok(response) if (200..300).contains(&response.status) => response.body,
                Ok(response) => { last_error = format!("font HTTP status {}", response.status); continue; }
                Err(error) => { last_error = error; continue; }
            }
        };
        match FontFace::new(bytes.into()) {
            Ok(face) => return face.with_metric_overrides(key.ascent.map(f32::from_bits), key.descent.map(f32::from_bits)).map(Arc::new).map_err(str::to_owned),
            Err(error) => last_error = error.into(),
        }
    }
    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key(source: &str) -> Key {
        Key { sources: Arc::from([FontFaceSource::Url(source.into())]), base: "http://owner.test/redirected/worker.js".into(), origin: "http://owner.test".into(), ascent: None, descent: None }
    }

    fn accept_font_request(listener: &std::net::TcpListener) -> (std::net::TcpStream, String) {
        use std::io::Read;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline, "font fixture accept timeout");
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(error) => panic!("font fixture accept: {error}"),
            }
        };
        stream.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
        let mut request = Vec::new();
        let mut chunk = [0; 512];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = stream.read(&mut chunk).unwrap();
            assert_ne!(count, 0, "font fixture premature EOF");
            request.extend_from_slice(&chunk[..count]);
            assert!(request.len() <= 16 * 1024, "font fixture oversized headers");
        }
        (stream, String::from_utf8(request).unwrap())
    }

    #[test]
    fn worker_font_http_loader_preserves_captured_routes_cors_and_streaming_budget() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for expected in ["allow", "deny", "oversized"] {
                let (mut stream, request) = accept_font_request(&listener);
                assert!(request.starts_with(&format!("GET /{expected}.ttf HTTP/1.1\r\n")));
                assert!(request.to_ascii_lowercase().contains("host: fonts.test"));
                assert!(request.to_ascii_lowercase().contains("origin: http://owner.test"));
                let body = lumen_html_text::DEFAULT_FONT_BYTES;
                let cors = if expected == "deny" { "" } else { "Access-Control-Allow-Origin: http://owner.test\r\n" };
                let length = if expected == "oversized" { MAX_MANUAL_FONT_BYTES_PER_FACE + 1 } else { body.len() };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: font/ttf\r\nContent-Length: {length}\r\n{cors}Connection: close\r\n\r\n").unwrap();
                if expected != "oversized" { stream.write_all(body).unwrap(); }
            }
        });
        let mut config = lumen_web::FetchConfig::default();
        config.set_require_routes(true);
        config.set_route("fonts.test", 80, address).unwrap();
        let loader = WorkerFontLoader::new(config.clone());
        config.remove_route("fonts.test", 80).unwrap();
        let mut rule = lumen_html::css::parse_font_faces("@font-face{font-family: Worker;src:url(allow.ttf)}").unwrap().remove(0);
        rule.source_url = Some("http://fonts.test/sheet.css".into());
        // Resolve against the referring source but send the worker's origin, even when the
        // source URL is cross-origin. CORS must never be bypassed by treating it as the owner.
        assert!(load(&WorkerFontLoader::key(&rule, "http://owner.test/worker.js"), &loader.config).is_ok());
        assert!(load(&test_key("http://fonts.test/deny.ttf"), &loader.config).is_err());
        assert!(load(&test_key("http://fonts.test/oversized.ttf"), &loader.config).is_err());
        server.join().unwrap();
        assert!(load(&test_key("http://unrouted.test/font.ttf"), &loader.config).is_err());
    }

    #[test]
    fn worker_font_owner_completion_deduplicates_requests_without_javascript_callback() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (release, gate) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, request) = accept_font_request(&listener);
            assert!(request.starts_with("GET /redirected/font.ttf HTTP/1.1\r\n"));
            gate.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            let body = lumen_html_text::DEFAULT_FONT_BYTES;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            stream.write_all(body).unwrap();
        });
        let mut config = lumen_web::FetchConfig::default();
        config.set_require_routes(true);
        config.set_route("owner.test", 80, address).unwrap();
        let loader = WorkerFontLoader::new(config);
        let mut runtime = crate::Runtime::new_browser();
        let rule = lumen_html::css::parse_font_faces("@font-face {font-family: Worker; src: url(font.ttf);}").unwrap().remove(0);
        let base = "http://owner.test/redirected/worker.js";
        assert!(loader.poll_load_in_context(runtime.engine().ctx(), &rule, base).is_pending());
        assert!(loader.poll_load_in_context(runtime.engine().ctx(), &rule, base).is_pending());
        assert_eq!(loader.entries.borrow().len(), 1);
        assert!(matches!(loader.entries.borrow()[0].state, State::Pending));
        release.send(()).unwrap();
        assert!(runtime.wait_for_completion(std::time::Duration::from_secs(5)));
        assert!(matches!(loader.poll_load(&rule, base), Poll::Ready(Ok(_))));
        assert!(runtime.run_until_idle().idle);
        server.join().unwrap();
    }

    #[test]
    fn worker_font_resource_source_failures_preserve_the_budget_and_fallback_order() {
        let key = Key { base: "http://font.test/worker.js".into(), origin: "http://font.test".into(), ascent: None, descent: None, sources: Arc::from([
            FontFaceSource::Local("missing-font".into()),
            FontFaceSource::Url("data:font/ttf;base64,invalid!".into()),
            FontFaceSource::Url("file:///font.ttf".into()),
        ]) };
        let error = load(&key, &lumen_web::FetchConfig::default()).err().unwrap();
        assert_eq!(error, "unsupported font URL scheme");
        assert!(lumen_common::url::data_url_body_bounded("data:,abcd", 3).is_err());
    }
}
