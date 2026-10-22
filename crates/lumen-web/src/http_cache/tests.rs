use super::*;
use crate::FetchConfig;
use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "lumen-http-cache-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    fn files(&self, suffix: &str) -> Vec<PathBuf> {
        let mut found: Vec<_> = fs::read_dir(&self.0)
            .unwrap()
            .flatten()
            .map(|item| item.path())
            .filter(|path| path.to_string_lossy().ends_with(suffix))
            .collect();
        found.sort();
        found
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Ok(listing) = fs::read_dir(&self.0) {
            for item in listing.flatten() {
                let _ = fs::remove_file(item.path());
            }
        }
        let _ = fs::remove_dir(&self.0);
    }
}

struct Server {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(mut handler: impl FnMut(usize, &str) -> Vec<u8> + Send + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen, halt) = (requests.clone(), stop.clone());
        let thread = std::thread::spawn(move || loop {
            let Ok((mut stream, _)) = listener.accept() else { return };
            if halt.load(Ordering::SeqCst) {
                return;
            }
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let end = line == "\r\n";
                request.push_str(&line);
                if end {
                    break;
                }
            }
            let index = {
                let mut seen = seen.lock().unwrap();
                seen.push(request.clone());
                seen.len() - 1
            };
            let _ = stream.write_all(&handler(index, &request));
        });
        Server { port, requests, stop, thread: Some(thread) }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn request(&self, index: usize) -> String {
        self.requests.lock().unwrap()[index].to_ascii_lowercase()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let text = if status == 304 { "Not Modified" } else { "OK" };
    let mut out = format!("HTTP/1.1 {status} {text}\r\nConnection: close\r\n");
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    if status != 304 {
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn config(dir: &TempDir) -> FetchConfig {
    let mut config = FetchConfig::default();
    config.set_http_cache_dir(&dir.0);
    config
}

fn get(url: &str, config: &FetchConfig) -> crate::http::HttpResponse {
    crate::http::request_with_config("GET", url, &[], None, config).unwrap()
}

fn get_with(url: &str, headers: &[(&str, &str)], config: &FetchConfig) -> crate::http::HttpResponse {
    let headers: Vec<_> = headers.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect();
    crate::http::request_with_config("GET", url, &headers, None, config).unwrap()
}

fn with_mode(config: &FetchConfig, mode: CacheMode) -> FetchConfig {
    let mut config = config.clone();
    config.cache_mode = mode;
    config
}

#[test]
fn fresh_entry_is_served_without_network() {
    let dir = TempDir::new();
    let server = Server::start(|_, _| response(200, &[("Cache-Control", "max-age=3600")], b"hello"));
    let config = config(&dir);
    let url = server.url("/a");
    assert_eq!(get(&url, &config).body, b"hello");
    let second = get(&url, &config);
    assert_eq!(second.body, b"hello");
    assert_eq!(second.status, 200);
    assert_eq!(server.count(), 1);
    assert_eq!(dir.files(".meta").len(), 1);
}

#[test]
fn stale_entry_revalidates_and_a_304_serves_the_stored_body() {
    let dir = TempDir::new();
    let server = Server::start(|index, request| {
        if request.to_ascii_lowercase().contains("if-none-match: \"v1\"") {
            response(304, &[("ETag", "\"v1\""), ("Cache-Control", "max-age=0"), ("X-Refreshed", "yes")], b"")
        } else {
            assert_eq!(index, 0);
            response(200, &[("ETag", "\"v1\""), ("Cache-Control", "max-age=0"), ("X-Old", "1")], b"body-one")
        }
    });
    let config = config(&dir);
    let url = server.url("/b");
    assert_eq!(get(&url, &config).body, b"body-one");
    for _ in 0..2 {
        let again = get(&url, &config);
        assert_eq!(again.status, 200);
        assert_eq!(again.body, b"body-one");
        assert!(again.headers.iter().any(|(n, v)| n.eq_ignore_ascii_case("x-refreshed") && v == "yes"));
        assert!(again.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("x-old")));
    }
    assert_eq!(server.count(), 3);
}

#[test]
fn last_modified_validator_sends_if_modified_since() {
    let dir = TempDir::new();
    let date = "Wed, 21 Oct 2015 07:28:00 GMT";
    let server = Server::start(move |_, request| {
        if request.to_ascii_lowercase().contains(&format!("if-modified-since: {}", date.to_ascii_lowercase())) {
            response(304, &[], b"")
        } else {
            response(200, &[("Last-Modified", date), ("Cache-Control", "no-cache")], b"lm")
        }
    });
    let config = config(&dir);
    let url = server.url("/lm");
    assert_eq!(get(&url, &config).body, b"lm");
    assert_eq!(get(&url, &config).body, b"lm");
    assert_eq!(server.count(), 2);
}

#[test]
fn a_200_on_revalidation_replaces_the_entry() {
    let dir = TempDir::new();
    let server = Server::start(|index, request| {
        let request = request.to_ascii_lowercase();
        match index {
            0 => response(200, &[("ETag", "\"v1\""), ("Cache-Control", "max-age=0")], b"first"),
            1 => {
                assert!(request.contains("if-none-match: \"v1\""));
                response(200, &[("ETag", "\"v2\""), ("Cache-Control", "max-age=0")], b"second!")
            }
            _ => {
                assert!(request.contains("if-none-match: \"v2\""));
                response(304, &[("ETag", "\"v2\"")], b"")
            }
        }
    });
    let config = config(&dir);
    let url = server.url("/c");
    assert_eq!(get(&url, &config).body, b"first");
    assert_eq!(get(&url, &config).body, b"second!");
    assert_eq!(get(&url, &config).body, b"second!");
    assert_eq!(dir.files(".body").len(), 1);
}

#[test]
fn no_store_responses_are_not_stored() {
    let dir = TempDir::new();
    let server = Server::start(|_, _| response(200, &[("Cache-Control", "no-store, max-age=100")], b"x"));
    let config = config(&dir);
    let url = server.url("/d");
    get(&url, &config);
    get(&url, &config);
    assert_eq!(server.count(), 2);
    assert!(dir.files(".meta").is_empty());
    assert!(dir.files(".body").is_empty());
}

#[test]
fn responses_without_freshness_or_validators_are_not_stored() {
    let dir = TempDir::new();
    let server = Server::start(|_, _| response(200, &[], b"plain"));
    let config = config(&dir);
    let url = server.url("/e");
    get(&url, &config);
    get(&url, &config);
    assert_eq!(server.count(), 2);
    assert!(dir.files(".meta").is_empty());
}

#[test]
fn vary_selects_the_stored_response() {
    let dir = TempDir::new();
    let server = Server::start(|_, request| {
        let lang = if request.to_ascii_lowercase().contains("x-lang: b") { "bee" } else { "aye" };
        response(200, &[("Cache-Control", "max-age=3600"), ("Vary", "X-Lang")], lang.as_bytes())
    });
    let config = config(&dir);
    let url = server.url("/v");
    assert_eq!(get_with(&url, &[("X-Lang", "a")], &config).body, b"aye");
    assert_eq!(get_with(&url, &[("X-Lang", "a")], &config).body, b"aye");
    assert_eq!(server.count(), 1);
    assert_eq!(get_with(&url, &[("X-Lang", "b")], &config).body, b"bee");
    assert_eq!(server.count(), 2);
    assert_eq!(get_with(&url, &[("X-Lang", "b")], &config).body, b"bee");
    assert_eq!(server.count(), 2);
    assert_eq!(get_with(&url, &[("X-Lang", "a")], &config).body, b"aye");
    assert_eq!(server.count(), 3);
}

#[test]
fn vary_star_is_not_stored() {
    let dir = TempDir::new();
    let server = Server::start(|_, _| response(200, &[("Cache-Control", "max-age=3600"), ("Vary", "*")], b"s"));
    let config = config(&dir);
    let url = server.url("/vs");
    get(&url, &config);
    get(&url, &config);
    assert_eq!(server.count(), 2);
}

#[test]
fn request_cache_modes() {
    let dir = TempDir::new();
    let server = Server::start(|index, request| {
        let request = request.to_ascii_lowercase();
        if request.contains("if-none-match") {
            return response(304, &[("ETag", "\"t\"")], b"");
        }
        let body = format!("gen{index}");
        response(200, &[("ETag", "\"t\""), ("Cache-Control", "max-age=3600")], body.as_bytes())
    });
    let base = config(&dir);
    let url = server.url("/m");

    let only = with_mode(&base, CacheMode::OnlyIfCached);
    assert!(crate::http::request_with_config("GET", &url, &[], None, &only).is_err());
    assert_eq!(server.count(), 0);

    assert_eq!(get(&url, &base).body, b"gen0");
    assert_eq!(get(&url, &with_mode(&base, CacheMode::OnlyIfCached)).body, b"gen0");
    assert_eq!(get(&url, &with_mode(&base, CacheMode::ForceCache)).body, b"gen0");
    assert_eq!(server.count(), 1);

    assert_eq!(get(&url, &with_mode(&base, CacheMode::NoStore)).body, b"gen1");
    assert_eq!(server.count(), 2);
    assert!(server.request(1).contains("cache-control: no-cache"));
    assert_eq!(get(&url, &base).body, b"gen0", "no-store neither reads nor updates the cache");
    assert_eq!(server.count(), 2);

    assert_eq!(get(&url, &with_mode(&base, CacheMode::Reload)).body, b"gen2");
    assert_eq!(server.count(), 3);
    assert_eq!(get(&url, &base).body, b"gen2", "reload updates the stored response");

    assert_eq!(get(&url, &with_mode(&base, CacheMode::NoCache)).body, b"gen2");
    assert_eq!(server.count(), 4);
    assert!(server.request(3).contains("if-none-match: \"t\""));
    assert!(server.request(3).contains("cache-control: max-age=0"));
}

#[test]
fn force_cache_serves_a_stale_entry_and_fetches_when_absent() {
    let dir = TempDir::new();
    let server = Server::start(|index, _| {
        response(200, &[("ETag", "\"s\""), ("Cache-Control", "max-age=0")], format!("g{index}").as_bytes())
    });
    let base = config(&dir);
    let forced = with_mode(&base, CacheMode::ForceCache);
    let url = server.url("/f");
    assert_eq!(get(&url, &forced).body, b"g0");
    assert_eq!(get(&url, &forced).body, b"g0");
    assert_eq!(server.count(), 1);
}

#[test]
fn embedder_default_mode_revalidates_ordinary_requests() {
    let dir = TempDir::new();
    let server = Server::start(|_, request| {
        if request.to_ascii_lowercase().contains("if-none-match") {
            response(304, &[], b"")
        } else {
            response(200, &[("ETag", "\"d\""), ("Cache-Control", "max-age=3600")], b"dd")
        }
    });
    let mut config = config(&dir);
    config.set_http_cache_default_mode(CacheMode::NoCache);
    let url = server.url("/dm");
    assert_eq!(get(&url, &config).body, b"dd");
    assert_eq!(get(&url, &config).body, b"dd");
    assert_eq!(server.count(), 2);
    assert!(!server.request(1).contains("cache-control"), "the embedder default adds no request headers");
}

#[test]
fn conditional_request_headers_from_the_caller_bypass_the_cache() {
    let dir = TempDir::new();
    let server = Server::start(|_, _| response(200, &[("Cache-Control", "max-age=3600")], b"c"));
    let config = config(&dir);
    let url = server.url("/cond");
    get_with(&url, &[("If-None-Match", "\"zzz\"")], &config);
    assert!(dir.files(".meta").is_empty());
    assert_eq!(
        effective_mode(CacheMode::Default, CacheMode::Default, &[("If-Modified-Since".into(), "x".into())]),
        CacheMode::NoStore
    );
    assert_eq!(
        effective_mode(CacheMode::Default, CacheMode::Default, &[("Cache-Control".into(), "no-cache".into())]),
        CacheMode::NoCache
    );
    assert_eq!(
        effective_mode(CacheMode::Default, CacheMode::ForceCache, &[("Pragma".into(), "no-cache".into())]),
        CacheMode::NoCache
    );
    assert_eq!(effective_mode(CacheMode::Default, CacheMode::NoCache, &[]), CacheMode::NoCache);
}

#[test]
fn unsafe_methods_invalidate_the_entry() {
    let dir = TempDir::new();
    let server = Server::start(|_, _| response(200, &[("Cache-Control", "max-age=3600")], b"u"));
    let config = config(&dir);
    let url = server.url("/inv");
    get(&url, &config);
    assert_eq!(dir.files(".meta").len(), 1);
    crate::http::request_with_config("POST", &url, &[], Some(b"x"), &config).unwrap();
    assert!(dir.files(".meta").is_empty());
    assert!(dir.files(".body").is_empty());
}

#[test]
fn corrupt_entries_are_ignored_and_replaced() {
    let dir = TempDir::new();
    let server = Server::start(|index, _| {
        response(200, &[("Cache-Control", "max-age=3600")], format!("payload-{index:04}").as_bytes())
    });
    let config = config(&dir);
    let url = server.url("/x");
    assert_eq!(get(&url, &config).body, b"payload-0000");

    fs::write(&dir.files(".meta")[0], b"not a cache entry").unwrap();
    assert_eq!(get(&url, &config).body, b"payload-0001");
    assert_eq!(get(&url, &config).body, b"payload-0001");

    let body = dir.files(".body").last().unwrap().clone();
    let mut bytes = fs::read(&body).unwrap();
    bytes[3] ^= 0xff;
    fs::write(&body, &bytes).unwrap();
    assert_eq!(get(&url, &config).body, b"payload-0002", "a flipped byte fails the CRC");

    fs::write(dir.files(".body").last().unwrap(), b"short").unwrap();
    assert_eq!(get(&url, &config).body, b"payload-0003", "a truncated body is ignored");
    assert_eq!(get(&url, &config).body, b"payload-0003");
    assert_eq!(server.count(), 4);
}

#[test]
fn large_streamed_bodies_are_stored_and_served_whole() {
    let dir = TempDir::new();
    let payload: Vec<u8> = (0..3_000_000u32).map(|n| (n % 251) as u8).collect();
    let served = payload.clone();
    let server = Server::start(move |_, _| response(200, &[("Cache-Control", "max-age=3600")], &served));
    let config = config(&dir);
    let url = server.url("/big");
    assert!(get(&url, &config).body == payload);
    assert!(get(&url, &config).body == payload);
    assert_eq!(server.count(), 1);
}

#[test]
fn an_abandoned_download_leaves_nothing_behind() {
    let dir = TempDir::new();
    let payload = vec![9u8; 2_000_000];
    let server = Server::start(move |_, _| response(200, &[("Cache-Control", "max-age=3600")], &payload));
    let config = config(&dir);
    let url = server.url("/part");
    let cancellation = lumen_os::net::TcpCancellation::default();
    let mut open = crate::http::open_request_cancellable_with_config("GET", &url, &[], None, &cancellation, &config).unwrap();
    assert!(open.body.read_chunk().unwrap().is_some());
    drop(open);
    assert!(dir.files(".meta").is_empty());
    assert!(dir.files(".body").is_empty());
    assert!(dir.files(".tmp").is_empty());
}

#[test]
fn the_size_cap_evicts_the_least_recently_used_entry() {
    let dir = TempDir::new();
    let server = Server::start(|_, request| {
        let path = request.split(' ').nth(1).unwrap_or("").to_owned();
        response(200, &[("Cache-Control", "max-age=3600")], format!("{path:-<1000}").as_bytes())
    });
    let mut config = config(&dir);
    config.set_http_cache_max_bytes(3200);
    let pause = || std::thread::sleep(Duration::from_millis(30));
    get(&server.url("/1"), &config);
    pause();
    get(&server.url("/2"), &config);
    pause();
    get(&server.url("/1"), &config);
    pause();
    get(&server.url("/3"), &config);
    assert_eq!(server.count(), 3);
    get(&server.url("/1"), &config);
    assert_eq!(server.count(), 3, "the recently used entry survived");
    get(&server.url("/2"), &config);
    assert_eq!(server.count(), 4, "the least recently used entry was evicted");
}

#[test]
fn a_large_body_is_read_in_large_chunks() {
    let payload = vec![5u8; 8 << 20];
    let served = payload.clone();
    let server = Server::start(move |_, _| response(200, &[], &served));
    let cancellation = lumen_os::net::TcpCancellation::default();
    let mut open = crate::http::open_request_cancellable_with_config(
        "GET",
        &server.url("/chunks"),
        &[],
        None,
        &cancellation,
        &FetchConfig::default(),
    )
    .unwrap();
    let (mut total, mut reads, mut largest) = (0usize, 0usize, 0usize);
    while let Some(chunk) = open.body.read_chunk().unwrap() {
        total += chunk.len();
        reads += 1;
        largest = largest.max(chunk.len());
    }
    assert_eq!(total, payload.len());
    assert!(largest > 8192, "largest chunk was {largest}");
    assert!(reads < payload.len() / 8192, "{reads} reads for {total} bytes");
}

#[test]
fn freshness_follows_max_age_expires_and_the_heuristic() {
    let meta = |headers: &[(&str, &str)], stored_at: u64| Meta {
        url: "http://x/".into(),
        status: 200,
        status_text: "OK".into(),
        stored_at,
        headers: headers.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect(),
        vary: Vec::new(),
        body_file: "b".into(),
        body_len: 0,
        body_crc: 0,
    };
    let now = now_secs();
    assert!(meta(&[("Cache-Control", "max-age=60")], now).fresh(now + 30));
    assert!(!meta(&[("Cache-Control", "max-age=60")], now).fresh(now + 61));
    assert!(!meta(&[("Cache-Control", "max-age=60, no-cache")], now).fresh(now));
    assert!(!meta(&[("Expires", "Thu, 01 Jan 1970 00:00:00 GMT")], now).fresh(now));
    assert!(!meta(&[("Expires", "0")], now).fresh(now));
    assert!(!meta(&[("Cache-Control", "max-age=60"), ("Age", "120")], now).fresh(now));
    let old = "Wed, 21 Oct 2015 07:28:00 GMT";
    assert!(meta(&[("Last-Modified", old)], now).fresh(now), "no Date: the store time stands in");
    let date = "Thu, 22 Oct 2015 07:28:00 GMT";
    let stored = parse_http_date(date).unwrap() as u64;
    let heuristic = meta(&[("Last-Modified", "Mon, 12 Oct 2015 07:28:00 GMT"), ("Date", date)], stored);
    assert_eq!(heuristic.lifetime(), 24 * 3600);
    assert!(heuristic.fresh(stored + 3600));
    assert!(!heuristic.fresh(stored + 25 * 3600));
}

#[test]
fn metadata_round_trips_and_rejects_damage() {
    let meta = Meta {
        url: "http://h/p?q=1".into(),
        status: 200,
        status_text: "OK".into(),
        stored_at: 42,
        headers: vec![("ETag".into(), "W/\"1\"".into()), ("X-Empty".into(), String::new())],
        vary: vec![("accept".into(), "a/b".into())],
        body_file: "k.n.body".into(),
        body_len: 7,
        body_crc: 99,
    };
    let text = meta.serialize();
    let parsed = Meta::parse(&text).unwrap();
    assert_eq!(parsed.url, meta.url);
    assert_eq!(parsed.headers, meta.headers);
    assert_eq!(parsed.vary, meta.vary);
    assert_eq!((parsed.body_len, parsed.body_crc, parsed.stored_at), (7, 99, 42));
    assert!(Meta::parse(&text[..text.len() - 4]).is_none(), "missing end marker");
    assert!(Meta::parse(&text.replace("k.n.body", "../escape")).is_none(), "path traversal in the body name");
    assert!(Meta::parse("garbage").is_none());
}

fn validating_server() -> Server {
    Server::start(|index, request| {
        let request = request.to_ascii_lowercase();
        if request.contains("if-none-match: \"v1\"") {
            response(304, &[("ETag", "\"v1\"")], b"")
        } else if index < 2 && !request.contains("if-none-match") {
            response(200, &[("ETag", "\"v1\""), ("Cache-Control", "max-age=3600")], b"body-one")
        } else {
            response(200, &[("ETag", "\"v2\""), ("Cache-Control", "max-age=3600")], b"body-two")
        }
    })
}

fn once_config(dir: &TempDir) -> FetchConfig {
    let mut config = config(dir);
    config.set_http_cache_default_mode(CacheMode::NoCache);
    config.set_http_cache_revalidate_once(true);
    config
}

#[test]
fn revalidate_once_reuses_a_confirmed_entry_for_the_rest_of_the_process() {
    let dir = TempDir::new();
    let server = validating_server();
    let config = once_config(&dir);
    let url = server.url("/once");
    assert_eq!(get(&url, &config).body, b"body-one");
    assert_eq!(get(&url, &config).body, b"body-one");
    assert_eq!(get(&url, &config).body, b"body-one");
    assert_eq!(server.count(), 1);
}

#[test]
fn a_new_process_revalidates_once_then_reuses_the_entry() {
    let dir = TempDir::new();
    let server = validating_server();
    let url = server.url("/restart");
    assert_eq!(get(&url, &once_config(&dir)).body, b"body-one");
    let next_process = once_config(&dir);
    assert_eq!(get(&url, &next_process).body, b"body-one");
    assert_eq!(server.count(), 2);
    assert!(server.request(1).contains("if-none-match"));
    assert_eq!(get(&url, &next_process).body, b"body-one");
    assert_eq!(server.count(), 2);
}

#[test]
fn requests_that_ask_to_revalidate_still_do() {
    let dir = TempDir::new();
    let server = validating_server();
    let config = once_config(&dir);
    let url = server.url("/forced");
    assert_eq!(get(&url, &config).body, b"body-one");
    assert_eq!(get_with(&url, &[("Cache-Control", "no-cache")], &config).body, b"body-one");
    assert_eq!(server.count(), 2);
    assert_eq!(get(&url, &with_mode(&config, CacheMode::NoCache)).body, b"body-one");
    assert_eq!(server.count(), 3);
    assert_eq!(get(&url, &config).body, b"body-one");
    assert_eq!(server.count(), 3);
}

#[test]
fn only_confirmed_urls_skip_revalidation() {
    let dir = TempDir::new();
    let server = validating_server();
    let seed = config(&dir);
    let (first, second) = (server.url("/one"), server.url("/two"));
    get(&first, &seed);
    get(&second, &seed);
    let config = once_config(&dir);
    assert_eq!(crate::revalidate_cached_resource(&first, &config), Ok(CacheValidation::Unchanged));
    let before = server.count();
    get(&first, &config);
    assert_eq!(server.count(), before);
    get(&second, &config);
    assert_eq!(server.count(), before + 1);
}

#[test]
fn revalidate_confirms_a_304_without_reading_the_body() {
    let dir = TempDir::new();
    let server = validating_server();
    let url = server.url("/probe");
    assert_eq!(get(&url, &config(&dir)).body, b"body-one");
    let config = once_config(&dir);
    assert_eq!(crate::revalidate_cached_resource(&url, &config), Ok(CacheValidation::Unchanged));
    assert_eq!(server.count(), 2);
    assert!(server.request(1).contains("if-none-match: \"v1\""));
    let body = dir.files(".body").remove(0);
    fs::write(&body, b"damaged!").unwrap();
    assert_eq!(
        crate::revalidate_cached_resource(&url, &config),
        Ok(CacheValidation::Unchanged),
        "the body is only verified when it is served"
    );
}

#[test]
fn revalidate_stores_a_changed_response_and_a_missing_one() {
    let dir = TempDir::new();
    let server = Server::start(|index, request| {
        let request = request.to_ascii_lowercase();
        if index == 0 {
            response(200, &[("ETag", "\"v1\""), ("Cache-Control", "max-age=3600")], b"first")
        } else {
            assert!(request.contains("if-none-match: \"v1\"") || index > 1);
            response(200, &[("ETag", "\"v2\""), ("Cache-Control", "max-age=3600")], b"second!")
        }
    });
    let config = once_config(&dir);
    let (changed, missing) = (server.url("/changed"), server.url("/missing"));
    assert_eq!(get(&changed, &config).body, b"first");
    let seeded = once_config(&dir);
    assert_eq!(crate::revalidate_cached_resource(&changed, &seeded), Ok(CacheValidation::Updated));
    assert_eq!(crate::revalidate_cached_resource(&missing, &seeded), Ok(CacheValidation::Updated));
    let requests = server.count();
    assert_eq!(get(&changed, &seeded).body, b"second!");
    assert_eq!(get(&missing, &seeded).body, b"second!");
    assert_eq!(server.count(), requests);
    assert_eq!(dir.files(".body").len(), 2);
}

#[test]
fn revalidate_reports_uncacheable_responses() {
    let dir = TempDir::new();
    let server = Server::start(|_, _| response(200, &[("Cache-Control", "no-store")], b"x"));
    let config = once_config(&dir);
    assert_eq!(crate::revalidate_cached_resource(&server.url("/ns"), &config), Ok(CacheValidation::NotCacheable));
    assert!(dir.files(".meta").is_empty());
}
