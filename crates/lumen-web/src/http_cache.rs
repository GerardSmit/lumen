//! Browser-style HTTP disk cache (an RFC 9111 subset) under the shared transport, so `fetch`,
//! `XMLHttpRequest` and the embedder's resource loaders all see it.
//!
//! - Only `GET` 200 responses are stored, as `<key>.meta` (status, headers, `Vary` request values,
//!   body file name, length and CRC) plus `<key>.<nonce>.body`; both are written to a temporary
//!   file and renamed, the meta last. Bodies are teed to disk while they stream to the consumer and
//!   committed when the body ends cleanly. The body is stored as the transport delivered it
//!   (transfer-decoded; `Content-Encoding` is not removed here, so the stored headers stay true).
//! - Freshness: `max-age`, else `Expires`, else a heuristic 10 % of `Date - Last-Modified` (capped
//!   at a day). `no-store` responses are never stored, `no-cache` ones always revalidate;
//!   `private`/`public` are ignored (single user).
//! - Stale entries with an `ETag` or `Last-Modified` are revalidated with `If-None-Match` /
//!   `If-Modified-Since`; a 304 refreshes the stored metadata and serves the stored body.
//! - Request cache modes follow Fetch: `default`, `no-store`, `reload`, `no-cache`, `force-cache`,
//!   `only-if-cached`.
//! - The directory is capped; least recently used entries (meta modification time, touched on
//!   every hit) are evicted. Unparsable entries, missing or short bodies and CRC mismatches are
//!   ignored and deleted.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lumen_common::cookies::parse_http_date;
use lumen_os::http_body::Framing;

use crate::http::{OpenHttpBody, OpenHttpResponse};
use crate::url::Url;

pub(crate) const DEFAULT_MAX_BYTES: u64 = 1 << 30;
const META_LIMIT: u64 = 1 << 20;
const MAGIC: &str = "LUMEN-HTTP-CACHE 1";
const HEURISTIC_CAP_SECS: u64 = 24 * 3600;
const ORPHAN_AGE: Duration = Duration::from_secs(3600);
const READ_BUFFER: usize = 256 << 10;

/// The `RequestCache` mode of a request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CacheMode {
    #[default]
    Default,
    NoStore,
    Reload,
    NoCache,
    ForceCache,
    OnlyIfCached,
}

impl CacheMode {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "default" => Self::Default,
            "no-store" => Self::NoStore,
            "reload" => Self::Reload,
            "no-cache" => Self::NoCache,
            "force-cache" => Self::ForceCache,
            "only-if-cached" => Self::OnlyIfCached,
            _ => return None,
        })
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn header_date(headers: &[(String, String)], name: &str) -> Option<u64> {
    header(headers, name)
        .and_then(parse_http_date)
        .map(|seconds| seconds.max(0) as u64)
}

#[derive(Default)]
struct CacheControl {
    max_age: Option<u64>,
    no_store: bool,
    no_cache: bool,
}

fn cache_control(headers: &[(String, String)]) -> CacheControl {
    let mut result = CacheControl::default();
    for (_, value) in headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("cache-control"))
    {
        for directive in value.split(',') {
            let directive = directive.trim();
            let (name, argument) = directive
                .split_once('=')
                .map_or((directive, ""), |(name, argument)| (name.trim(), argument.trim()));
            match name.to_ascii_lowercase().as_str() {
                "max-age" if result.max_age.is_none() => {
                    result.max_age = Some(argument.trim_matches('"').parse().unwrap_or(0));
                }
                "no-store" => result.no_store = true,
                "no-cache" => result.no_cache = true,
                _ => {}
            }
        }
    }
    result
}

#[derive(Clone, Debug)]
struct Meta {
    url: String,
    status: u16,
    status_text: String,
    stored_at: u64,
    headers: Vec<(String, String)>,
    vary: Vec<(String, String)>,
    body_file: String,
    body_len: u64,
    body_crc: u32,
}

impl Meta {
    fn serialize(&self) -> String {
        let mut out = format!(
            "{MAGIC}\nurl {}\nstatus {} {}\nstored {}\nbody {} {} {}\n",
            self.url, self.status, self.status_text, self.stored_at, self.body_file, self.body_len,
            self.body_crc
        );
        for (name, value) in &self.vary {
            out.push_str(&format!("vary {name}\t{value}\n"));
        }
        for (name, value) in &self.headers {
            out.push_str(&format!("h {name}\t{value}\n"));
        }
        out.push_str("end\n");
        out
    }

    fn parse(text: &str) -> Option<Meta> {
        let mut lines = text.lines();
        if lines.next()? != MAGIC {
            return None;
        }
        let mut meta = Meta {
            url: String::new(),
            status: 0,
            status_text: String::new(),
            stored_at: 0,
            headers: Vec::new(),
            vary: Vec::new(),
            body_file: String::new(),
            body_len: 0,
            body_crc: 0,
        };
        let mut ended = false;
        for line in lines {
            let (tag, rest) = line.split_once(' ').unwrap_or((line, ""));
            match tag {
                "url" => meta.url = rest.to_owned(),
                "status" => {
                    let (code, text) = rest.split_once(' ').unwrap_or((rest, ""));
                    meta.status = code.parse().ok()?;
                    meta.status_text = text.to_owned();
                }
                "stored" => meta.stored_at = rest.parse().ok()?,
                "body" => {
                    let mut parts = rest.split(' ');
                    meta.body_file = parts.next()?.to_owned();
                    meta.body_len = parts.next()?.parse().ok()?;
                    meta.body_crc = parts.next()?.parse().ok()?;
                }
                "vary" => {
                    let (name, value) = rest.split_once('\t')?;
                    meta.vary.push((name.to_owned(), value.to_owned()));
                }
                "h" => {
                    let (name, value) = rest.split_once('\t')?;
                    meta.headers.push((name.to_owned(), value.to_owned()));
                }
                "end" => {
                    ended = true;
                    break;
                }
                _ => return None,
            }
        }
        let safe_name = !meta.body_file.is_empty()
            && meta.body_file.bytes().all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte));
        (ended && meta.status == 200 && !meta.url.is_empty() && safe_name).then_some(meta)
    }

    fn lifetime(&self) -> u64 {
        let control = cache_control(&self.headers);
        if let Some(max_age) = control.max_age {
            return max_age;
        }
        let date = header_date(&self.headers, "date").unwrap_or(self.stored_at);
        if let Some(expires) = header(&self.headers, "expires") {
            return parse_http_date(expires)
                .map_or(0, |expires| (expires.max(0) as u64).saturating_sub(date));
        }
        match header_date(&self.headers, "last-modified") {
            Some(modified) => (date.saturating_sub(modified) / 10).min(HEURISTIC_CAP_SECS),
            None => 0,
        }
    }

    fn age(&self, now: u64) -> u64 {
        let date = header_date(&self.headers, "date").unwrap_or(self.stored_at);
        let apparent = self.stored_at.saturating_sub(date);
        let reported = header(&self.headers, "age")
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0);
        apparent.max(reported) + now.saturating_sub(self.stored_at)
    }

    fn fresh(&self, now: u64) -> bool {
        !cache_control(&self.headers).no_cache && self.lifetime() > self.age(now)
    }

    fn has_validator(&self) -> bool {
        header(&self.headers, "etag").is_some() || header(&self.headers, "last-modified").is_some()
    }
}

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "set-cookie",
    "set-cookie2",
];

fn stored_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| !HOP_BY_HOP.iter().any(|hop| name.eq_ignore_ascii_case(hop)))
        .cloned()
        .collect()
}

fn varied_value(request: &[(String, String)], name: &str) -> String {
    request
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The request header values a response varies on, or `None` for `Vary: *`.
fn vary_values(
    response: &[(String, String)],
    request: &[(String, String)],
) -> Option<Vec<(String, String)>> {
    let mut vary = Vec::new();
    for (_, value) in response
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("vary"))
    {
        for name in value.split(',').map(str::trim).filter(|name| !name.is_empty()) {
            if name == "*" {
                return None;
            }
            let name = name.to_ascii_lowercase();
            let value = varied_value(request, &name);
            vary.push((name, value));
        }
    }
    Some(vary)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

static NONCE: AtomicU64 = AtomicU64::new(0);

fn nonce() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!(
        "{:x}{:x}{:x}",
        std::process::id(),
        nanos,
        NONCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = path.with_extension(format!("{}.tmp", nonce()));
    let result = File::create(&temp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// The embedder-configured cache directory with its size cap.
#[derive(Clone, Debug)]
pub(crate) struct HttpCache {
    dir: PathBuf,
    max_bytes: u64,
    /// URLs whose stored response was confirmed current by the origin during this process.
    validated: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

/// What [`revalidate`] found out about a stored response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheValidation {
    /// The origin answered `304`: the stored response is current and its body was not read.
    Unchanged,
    /// The origin sent a new response (or none was stored); it is now stored.
    Updated,
    /// The response cannot be stored (not a `200`, `no-store`, no freshness or validator).
    NotCacheable,
}

struct Entry {
    meta: Meta,
    meta_path: PathBuf,
    body_path: PathBuf,
}

impl HttpCache {
    pub(crate) fn new(dir: PathBuf, max_bytes: u64) -> Self {
        Self { dir, max_bytes, validated: Default::default() }
    }

    fn mark_validated(&self, url: &str) {
        if let Ok(mut set) = self.validated.lock() {
            set.insert(url.to_owned());
        }
    }

    fn is_validated(&self, url: &str) -> bool {
        self.validated.lock().is_ok_and(|set| set.contains(url))
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    fn key(url: &str) -> String {
        let digest = lumen_common::hash::digest(lumen_common::hash::Algo::Sha256, url.as_bytes());
        hex(&digest[..20])
    }

    fn meta_path(&self, url: &str) -> PathBuf {
        self.dir.join(format!("{}.meta", Self::key(url)))
    }

    fn read_meta(&self, path: &Path) -> Option<Meta> {
        let mut text = String::new();
        File::open(path)
            .ok()?
            .take(META_LIMIT)
            .read_to_string(&mut text)
            .ok()?;
        Meta::parse(&text)
    }

    pub(crate) fn remove(&self, url: &str) {
        let path = self.meta_path(url);
        if let Some(meta) = self.read_meta(&path) {
            let _ = fs::remove_file(self.dir.join(&meta.body_file));
        }
        let _ = fs::remove_file(&path);
    }

    fn lookup(&self, url: &str, request: &[(String, String)]) -> Option<Entry> {
        let meta_path = self.meta_path(url);
        if !meta_path.exists() {
            return None;
        }
        let Some(meta) = self.read_meta(&meta_path) else {
            let _ = fs::remove_file(&meta_path);
            return None;
        };
        if meta.url != url {
            return None;
        }
        if meta
            .vary
            .iter()
            .any(|(name, value)| varied_value(request, name) != *value)
        {
            return None;
        }
        let body_path = self.dir.join(&meta.body_file);
        if fs::metadata(&body_path).map(|info| info.len()).ok() != Some(meta.body_len) {
            self.remove(url);
            return None;
        }
        Some(Entry { meta, meta_path, body_path })
    }

    fn touch(entry: &Entry) {
        if let Ok(file) = OpenOptions::new().write(true).open(&entry.meta_path) {
            let _ = file.set_modified(SystemTime::now());
        }
    }

    /// Open the stored body after verifying its CRC.
    fn open_verified(&self, entry: &Entry) -> io::Result<File> {
        let mut file = File::open(&entry.body_path)?;
        let mut crc = 0u32;
        let mut length = 0u64;
        let mut buffer = vec![0u8; 1 << 20];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            crc = lumen_common::crc32::crc32_from(crc, &buffer[..count]);
            length += count as u64;
        }
        if length != entry.meta.body_len || crc != entry.meta.body_crc {
            return Err(io::Error::other("cache entry is corrupt"));
        }
        file.rewind()?;
        Ok(file)
    }

    fn serve(
        &self,
        entry: &Entry,
        headers: Vec<(String, String)>,
        cancellation: &lumen_os::net::TcpCancellation,
        limit: u64,
    ) -> Option<OpenHttpResponse> {
        let file = match self.open_verified(entry) {
            Ok(file) => file,
            Err(_) => {
                self.remove(&entry.meta.url);
                return None;
            }
        };
        let source: Box<dyn BufRead + Send> = Box::new(BufReader::with_capacity(READ_BUFFER, file));
        let body = OpenHttpBody::from_source(
            source,
            Framing::Length(entry.meta.body_len),
            limit,
            cancellation.clone(),
        )
        .ok()?;
        let mut headers: Vec<_> = headers
            .into_iter()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("content-length"))
            .collect();
        headers.push(("Content-Length".into(), entry.meta.body_len.to_string()));
        Self::touch(entry);
        Some(OpenHttpResponse {
            status: entry.meta.status,
            status_text: entry.meta.status_text.clone(),
            headers,
            body,
            url: String::new(),
        })
    }

    fn refresh_meta(&self, entry: &Entry, not_modified: &[(String, String)]) -> Entry {
        const KEEP: &[&str] = &["content-encoding", "content-length", "content-range"];
        let mut meta = entry.meta.clone();
        for (name, value) in stored_headers(not_modified) {
            if KEEP.iter().any(|keep| name.eq_ignore_ascii_case(keep)) {
                continue;
            }
            meta.headers.retain(|(known, _)| !known.eq_ignore_ascii_case(&name));
            meta.headers.push((name, value));
        }
        meta.stored_at = now_secs();
        let _ = write_atomic(&entry.meta_path, meta.serialize().as_bytes());
        self.mark_validated(&meta.url);
        Entry {
            meta,
            meta_path: entry.meta_path.clone(),
            body_path: entry.body_path.clone(),
        }
    }

    fn refresh(
        &self,
        entry: &Entry,
        not_modified: &[(String, String)],
        cancellation: &lumen_os::net::TcpCancellation,
        limit: u64,
    ) -> Option<OpenHttpResponse> {
        let refreshed = self.refresh_meta(entry, not_modified);
        let mut headers = refreshed.meta.headers.clone();
        headers.extend(
            not_modified
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
                .cloned(),
        );
        self.serve(&refreshed, headers, cancellation, limit)
    }

    /// Whether `response` (a fresh network answer to a `GET`) may be stored; if so, the tee that
    /// stores it as its body is read.
    fn begin_store(
        &self,
        url: &str,
        request: &[(String, String)],
        response: &OpenHttpResponse,
    ) -> Option<Tee> {
        if response.status != 200 || cache_control(&response.headers).no_store {
            return None;
        }
        let vary = vary_values(&response.headers, request)?;
        let meta = Meta {
            url: url.to_owned(),
            status: response.status,
            status_text: response.status_text.clone(),
            stored_at: now_secs(),
            headers: stored_headers(&response.headers),
            vary,
            body_file: String::new(),
            body_len: 0,
            body_crc: 0,
        };
        if meta.lifetime() == 0 && !meta.has_validator() {
            return None;
        }
        fs::create_dir_all(&self.dir).ok()?;
        let key = Self::key(url);
        let body_file = format!("{key}.{}.body", nonce());
        let temp_path = self.dir.join(format!("{body_file}.tmp"));
        let file = File::create(&temp_path).ok()?;
        Some(Tee {
            cache: self.clone(),
            meta: Meta { body_file, ..meta },
            temp_path,
            file: Some(file),
            crc: 0,
            written: 0,
            max_entry: self.max_bytes / 2,
        })
    }

    fn evict(&self) {
        let Ok(listing) = fs::read_dir(&self.dir) else { return };
        struct Group {
            files: Vec<(PathBuf, u64)>,
            used: SystemTime,
            has_meta: bool,
        }
        let mut groups: std::collections::HashMap<String, Group> = Default::default();
        let mut total = 0u64;
        for item in listing.flatten() {
            let Ok(info) = item.metadata() else { continue };
            if !info.is_file() {
                continue;
            }
            let name = item.file_name().to_string_lossy().into_owned();
            let key = name.split('.').next().unwrap_or("").to_owned();
            let modified = info.modified().unwrap_or(UNIX_EPOCH);
            let group = groups.entry(key).or_insert_with(|| Group {
                files: Vec::new(),
                used: UNIX_EPOCH,
                has_meta: false,
            });
            if name.ends_with(".meta") {
                group.has_meta = true;
                group.used = modified;
            }
            total += info.len();
            group.files.push((item.path(), info.len()));
        }
        let now = SystemTime::now();
        let mut ordered: Vec<_> = groups.into_values().collect();
        ordered.sort_by_key(|group| (group.has_meta, group.used));
        for group in ordered {
            let remove = if group.has_meta {
                total > self.max_bytes
            } else {
                group.files.iter().all(|(path, _)| {
                    fs::metadata(path)
                        .and_then(|info| info.modified())
                        .ok()
                        .and_then(|modified| now.duration_since(modified).ok())
                        .is_some_and(|age| age > ORPHAN_AGE)
                })
            };
            if !remove {
                continue;
            }
            for (path, size) in group.files {
                if fs::remove_file(&path).is_ok() {
                    total = total.saturating_sub(size);
                }
            }
        }
    }
}

/// Writes a streaming response body to disk and commits the entry when the body ends cleanly.
/// Dropping it uncommitted discards the partial file.
pub(crate) struct Tee {
    cache: HttpCache,
    meta: Meta,
    temp_path: PathBuf,
    file: Option<File>,
    crc: u32,
    written: u64,
    max_entry: u64,
}

impl Tee {
    /// Append a body chunk; `false` abandons the entry (write error, or past the entry-size cap).
    pub(crate) fn write(&mut self, chunk: &[u8]) -> bool {
        let Some(file) = self.file.as_mut() else { return false };
        self.written += chunk.len() as u64;
        if self.written > self.max_entry || file.write_all(chunk).is_err() {
            self.file = None;
            return false;
        }
        self.crc = lumen_common::crc32::crc32_from(self.crc, chunk);
        true
    }

    pub(crate) fn commit(mut self) {
        let Some(mut file) = self.file.take() else { return };
        if file.flush().is_err() {
            return;
        }
        drop(file);
        let cache = &self.cache;
        let body_path = cache.dir.join(&self.meta.body_file);
        let meta_path = cache.meta_path(&self.meta.url);
        let previous = cache.read_meta(&meta_path).map(|old| old.body_file);
        if fs::rename(&self.temp_path, &body_path).is_err() {
            return;
        }
        self.meta.body_len = self.written;
        self.meta.body_crc = self.crc;
        if write_atomic(&meta_path, self.meta.serialize().as_bytes()).is_err() {
            let _ = fs::remove_file(&body_path);
            return;
        }
        if let Some(previous) = previous.filter(|old| *old != self.meta.body_file) {
            let _ = fs::remove_file(cache.dir.join(previous));
        }
        cache.mark_validated(&self.meta.url);
        cache.evict();
    }
}

impl Drop for Tee {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temp_path);
    }
}

fn effective_mode(mode: CacheMode, embedder_default: CacheMode, headers: &[(String, String)]) -> CacheMode {
    if mode != CacheMode::Default {
        return mode;
    }
    const CONDITIONAL: &[&str] = &[
        "if-modified-since",
        "if-none-match",
        "if-unmodified-since",
        "if-match",
        "if-range",
    ];
    if headers
        .iter()
        .any(|(name, _)| CONDITIONAL.iter().any(|known| name.eq_ignore_ascii_case(known)))
    {
        return CacheMode::NoStore;
    }
    let control = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("cache-control"))
        .flat_map(|(_, value)| value.split(',').map(|part| part.trim().to_ascii_lowercase()))
        .collect::<Vec<_>>();
    let pragma = header(headers, "pragma").is_some_and(|value| value.to_ascii_lowercase().contains("no-cache"));
    if pragma || control.iter().any(|part| part == "no-cache" || part == "max-age=0") {
        return CacheMode::NoCache;
    }
    embedder_default
}

fn set_missing(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    if header(headers, name).is_none() {
        headers.push((name.to_owned(), value.to_owned()));
    }
}

fn cache_url(url: &Url) -> String {
    let href = url.href();
    href.split('#').next().unwrap_or("").to_owned()
}

pub(crate) struct Settings<'a> {
    pub cache: &'a HttpCache,
    pub mode: CacheMode,
    pub embedder_default: CacheMode,
    pub revalidate_once: bool,
    pub cancellation: &'a lumen_os::net::TcpCancellation,
    pub body_limit: u64,
}

type Network<'a> = dyn FnMut(&[(String, String)]) -> Result<OpenHttpResponse, String> + 'a;

/// One request hop through the cache; `network` performs the real request with the given headers.
pub(crate) fn request(
    settings: &Settings<'_>,
    method: &str,
    url: &Url,
    headers: &[(String, String)],
    network: &mut Network<'_>,
) -> Result<OpenHttpResponse, String> {
    let _network = lumen_common::memcat::enter(lumen_common::memcat::CategoryTag::NETWORK);
    let cache = settings.cache;
    let key_url = cache_url(url);
    if !method.eq_ignore_ascii_case("GET") {
        let response = network(headers)?;
        let safe = matches!(method.to_ascii_uppercase().as_str(), "HEAD" | "OPTIONS" | "TRACE");
        if !safe && response.status < 500 {
            cache.remove(&key_url);
        }
        return Ok(response);
    }
    if header(headers, "range").is_some() {
        return network(headers);
    }
    let explicit = settings.mode != CacheMode::Default;
    let mode = effective_mode(settings.mode, settings.embedder_default, headers);
    let mut plain = headers.to_vec();
    if explicit {
        match mode {
            CacheMode::NoStore | CacheMode::Reload => {
                set_missing(&mut plain, "Pragma", "no-cache");
                set_missing(&mut plain, "Cache-Control", "no-cache");
            }
            CacheMode::NoCache => set_missing(&mut plain, "Cache-Control", "max-age=0"),
            _ => {}
        }
    }
    let store = |response: OpenHttpResponse| -> OpenHttpResponse {
        match cache.begin_store(&key_url, headers, &response) {
            Some(tee) => response.with_tee(tee),
            None => {
                if response.status == 200 {
                    cache.remove(&key_url);
                }
                response
            }
        }
    };
    if mode == CacheMode::NoStore {
        return network(&plain);
    }
    if mode == CacheMode::Reload {
        return network(&plain).map(store);
    }
    let entry = cache.lookup(&key_url, headers);
    let now = now_secs();
    let reuse_validated = settings.revalidate_once
        && settings.mode == CacheMode::Default
        && effective_mode(CacheMode::Default, CacheMode::Default, headers) == CacheMode::Default
        && cache.is_validated(&key_url);
    let serve = |entry: &Entry| {
        cache.serve(
            entry,
            entry.meta.headers.clone(),
            settings.cancellation,
            settings.body_limit,
        )
    };
    match (mode, &entry) {
        (CacheMode::OnlyIfCached | CacheMode::ForceCache, Some(entry)) => {
            if let Some(response) = serve(entry) {
                return Ok(response);
            }
        }
        (CacheMode::Default, Some(entry)) if entry.meta.fresh(now) => {
            if let Some(response) = serve(entry) {
                return Ok(response);
            }
        }
        (CacheMode::NoCache, Some(entry)) if reuse_validated => {
            if let Some(response) = serve(entry) {
                return Ok(response);
            }
        }
        _ => {}
    }
    if mode == CacheMode::OnlyIfCached {
        return Err("fetch: only-if-cached request has no stored response".into());
    }
    if let Some(entry) = entry.as_ref().filter(|entry| entry.meta.has_validator()) {
        let mut conditional = plain.clone();
        if let Some(etag) = header(&entry.meta.headers, "etag") {
            set_missing(&mut conditional, "If-None-Match", etag);
        }
        if let Some(modified) = header(&entry.meta.headers, "last-modified") {
            set_missing(&mut conditional, "If-Modified-Since", modified);
        }
        let response = network(&conditional)?;
        if response.status == 304 {
            if let Some(served) =
                cache.refresh(entry, &response.headers, settings.cancellation, settings.body_limit)
            {
                return Ok(served);
            }
            cache.remove(&key_url);
            return network(&plain).map(store);
        }
        return Ok(store(response));
    }
    network(&plain).map(store)
}

/// Confirm with the origin that the stored response for `url` is current, without reading its
/// body when the origin answers `304`; a changed (or missing) response is fetched and stored.
/// Either way the entry counts as validated for [`Settings::revalidate_once`].
pub(crate) fn revalidate(
    settings: &Settings<'_>,
    url: &Url,
    headers: &[(String, String)],
    network: &mut Network<'_>,
) -> Result<CacheValidation, String> {
    let _network = lumen_common::memcat::enter(lumen_common::memcat::CategoryTag::NETWORK);
    let cache = settings.cache;
    let key_url = cache_url(url);
    let entry = cache.lookup(&key_url, headers);
    let mut plain = headers.to_vec();
    set_missing(&mut plain, "Cache-Control", "max-age=0");
    let mut conditional = plain.clone();
    if let Some(entry) = entry.as_ref().filter(|entry| entry.meta.has_validator()) {
        if let Some(etag) = header(&entry.meta.headers, "etag") {
            set_missing(&mut conditional, "If-None-Match", etag);
        }
        if let Some(modified) = header(&entry.meta.headers, "last-modified") {
            set_missing(&mut conditional, "If-Modified-Since", modified);
        }
    }
    let mut response = network(&conditional)?;
    if response.status == 304 {
        if let Some(entry) = &entry {
            cache.refresh_meta(entry, &response.headers);
            return Ok(CacheValidation::Unchanged);
        }
        response = network(&plain)?;
    }
    if response.status != 200 {
        return Ok(CacheValidation::NotCacheable);
    }
    let Some(tee) = cache.begin_store(&key_url, headers, &response) else {
        return Ok(CacheValidation::NotCacheable);
    };
    let mut response = response.with_tee(tee);
    while response.body.read_chunk().map_err(|error| error.to_string())?.is_some() {}
    Ok(CacheValidation::Updated)
}

#[cfg(test)]
mod tests;
