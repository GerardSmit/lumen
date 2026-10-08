//! The ES-module loader for the runtime: resolves an `import` specifier to a canonical key +
//! source, which the engine's `eval_module` consults for every dependency. The engine already
//! runs the module graph (linking, top-level await); this is just resolution + the
//! CommonJS/builtin interop bridge.
//!
//! Resolution:
//! - `node:x` or a bare builtin name -> a synthetic re-export module (precomputed in JS, so
//!   named imports like `import { readFileSync } from "node:fs"` work).
//! - relative/absolute -> a file on disk (`.mjs`/`.js`/`.json`/`.cjs`, directory index,
//!   `package.json` `main`). `.js`/`.mjs` load as real ESM; `.json` and `.cjs` get a synthetic
//!   default-export wrapper (`.cjs` bridges through the global CommonJS `require`).
//! - bare package -> the `node_modules` walk; an ESM entry (`.mjs`, or `package.json`
//!   `type:module` / `exports` import condition / `module`) loads as real ESM, else it's
//!   default-only CJS interop via `require`. A bare subpath (`hono/logger`) resolves through
//!   its package's `exports` map first (`"./logger"` -> its `import`/`default` target); a
//!   literal file is considered only when there is no map.
//!
//! - `aot:/…` referrer (a module of a precompiled blob) -> the engine resolves the blob's own
//!   units and bundled packages first; only what the blob lacks arrives here, and a bare
//!   package then resolves from the current directory's `node_modules`.
//!
//! Deferred (documented, not silently wrong): named imports from a CommonJS *package* (Node
//! uses additional source static-analysis patterns), import maps.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lumen_host::sysfs::PathExt as _;

/// The named exports of each builtin (`"node:fs"` -> `"appendFile appendFileSync …"`, from the
/// node glue's `esm_exports.js`). A builtin's synthetic ESM source is built from its list when it
/// is first imported ([`builtin_source`]), not for every builtin at startup.
pub struct BuiltinModules(pub HashMap<String, &'static str>);

/// The synthetic ESM form of a builtin: the module object as the default export, and each listed
/// name that is a plain identifier as a named export read from it at import time.
pub fn builtin_source(name: &str, exports: &str) -> String {
    // Builtin names (`fs/promises`) and identifiers need no escaping inside a string literal.
    let mut src =
        format!("const __m = globalThis.__esmBuiltin(\"{name}\");\nexport default __m;\n");
    for k in exports.split(' ') {
        let mut chars = k.chars();
        let ident = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
        if ident && k != "default" {
            src.push_str(&format!("export const {k} = __m[\"{k}\"];\n"));
        }
    }
    src
}

const EXTENSIONS: [&str; 9] = [
    ".mjs", ".js", ".jsx", ".tsx", ".json", ".cjs", ".ts", ".mts", ".cts",
];

/// Build the loader closure `eval_module` wants, plus a handle on its [`LoaderCache`] so the
/// caller can drop the cached sources once the module graph they were fetched for has loaded. It
/// owns everything (`'static`); the engine caches results by the canonical key we return, so
/// returning a stable realpath per file is what dedupes shared dependencies.
#[cfg(target_arch = "wasm32")]
pub fn make_cached_loader(
    builtins: BuiltinModules,
) -> (
    impl Fn(&str, &str, Option<&str>) -> Option<(String, String)>,
    std::rc::Rc<LoaderCache>,
) {
    make_cached_loader_with_resource_loader(builtins, default_network_module_resource)
}

/// A loader bound to one immutable native fetch-routing and trust snapshot. This keeps network
/// module requests on the same per-runtime routes as `fetch()` without consulting process-global
/// DNS or trust configuration after the loader is created.
#[cfg(all(test, not(target_arch = "wasm32")))]
pub fn make_loader_with_fetch_config(
    builtins: BuiltinModules,
    fetch_config: lumen_web::FetchConfig,
) -> impl Fn(&str, &str, Option<&str>) -> Option<(String, String)> {
    make_cached_loader_with_fetch_config(builtins, fetch_config).0
}

/// [`make_loader_with_fetch_config`], with a cache handle for callers that load a complete graph.
#[cfg(not(target_arch = "wasm32"))]
pub fn make_cached_loader_with_fetch_config(
    builtins: BuiltinModules,
    fetch_config: lumen_web::FetchConfig,
) -> (
    impl Fn(&str, &str, Option<&str>) -> Option<(String, String)>,
    std::rc::Rc<LoaderCache>,
) {
    make_cached_loader_with_fetch_config_and_prefetched(
        builtins,
        fetch_config,
        PrefetchedModuleResources::default(),
    )
}

/// A configured ESM loader which first consults resources fetched by an embedder's asynchronous
/// document loader. Missing cache entries retain the ordinary native loader behavior. Failed
/// entries are remembered too, so a graph that was prepared asynchronously does not silently
/// issue the same failed request again on the runtime thread.
#[cfg(not(target_arch = "wasm32"))]
pub fn make_cached_loader_with_fetch_config_and_prefetched(
    builtins: BuiltinModules,
    fetch_config: lumen_web::FetchConfig,
    prefetched: PrefetchedModuleResources,
) -> (
    impl Fn(&str, &str, Option<&str>) -> Option<(String, String)>,
    std::rc::Rc<LoaderCache>,
) {
    make_cached_loader_with_resource_loader(builtins, move |url| {
        if let Some(entry) = prefetched.get(url) {
            return entry
                .ok()
                .map(|resource| (resource.final_url, resource.content_type, resource.bytes.to_vec()));
        }
        let resource = lumen_web::load_module_resource_with_config(url, &fetch_config).ok()?;
        Some((resource.url, resource.content_type, resource.bytes))
    })
}

type NetworkModuleResource = (String, Option<String>, Vec<u8>);

const PREFETCHED_MODULE_BYTES: usize = 32 * 1024 * 1024;
const PREFETCHED_MODULE_ENTRIES: usize = 512;

/// A successfully fetched module resource before ESM MIME, redirect-origin, and import-attribute
/// policy are applied by the canonical ESM resolver.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrefetchedModuleResource {
    pub final_url: String,
    pub content_type: Option<String>,
    pub bytes: Arc<[u8]>,
    pub script_context: Option<lumen::ClassicScriptContext>,
}

#[derive(Default)]
struct PrefetchedModuleState {
    entries: HashMap<String, PrefetchedModuleTypes>,
    count: usize,
    pending: HashMap<(String,Option<String>),Arc<ModuleResourceFlight>>,
    bytes: usize,
}
#[derive(Default)]
struct PrefetchedModuleTypes {
    javascript: Option<Result<PrefetchedModuleResource,String>>,
    typed: HashMap<String,Result<PrefetchedModuleResource,String>>,
}
impl PrefetchedModuleTypes {
    fn get(&self,kind:Option<&str>)->Option<&Result<PrefetchedModuleResource,String>> {
        match kind { None=>self.javascript.as_ref(),Some(kind)=>self.typed.get(kind) }
    }
    fn insert(&mut self,kind:Option<&str>,resource:Result<PrefetchedModuleResource,String>) {
        match kind { None=>self.javascript=Some(resource),Some(kind)=>{self.typed.insert(kind.into(),resource);} }
    }
}

/// Bounded, shareable resource snapshot populated by a browser's asynchronous module graph
/// fetcher and consumed by the normal ESM resolver on the runtime thread.
#[derive(Clone, Default)]
pub struct PrefetchedModuleResources {
    state: Arc<Mutex<PrefetchedModuleState>>,
}
#[derive(Default)]
struct ModuleResourceFlight {
    result: Mutex<Option<Result<PrefetchedModuleResource,String>>>,
    changed: std::sync::Condvar,
}

impl PrefetchedModuleResources {
    /// Record one request URL/type and its actual final response. Browser request
    /// fragments remain part of identity even though HTTP does not transmit them.
    pub fn insert(
        &self,
        request_url: String,
        result: Result<PrefetchedModuleResource, String>,
    ) -> Result<(), String> {
        self.insert_for_type(request_url, None, result)
    }
    pub fn insert_for_type(&self, request_url: String, module_type: Option<&str>, result: Result<PrefetchedModuleResource, String>) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "prefetched module resource cache is poisoned".to_owned())?;
        Self::insert_locked(&mut state, request_url, module_type, result)
    }
    fn insert_locked(state: &mut PrefetchedModuleState, request_url: String, module_type: Option<&str>, result: Result<PrefetchedModuleResource, String>) -> Result<(), String> {
        if state.entries.get(&request_url).is_some_and(|types|types.get(module_type).is_some()) {
            return Ok(());
        }
        if state.count >= PREFETCHED_MODULE_ENTRIES {
            return Err("module graph exceeded the prefetched resource count limit".into());
        }
        let weight = request_url.len().saturating_add(module_type.map_or(0,str::len)).saturating_add(result.as_ref().map_or_else(|error|error.len(), |resource| {
            resource.final_url.len()
                .saturating_add(resource.content_type.as_ref().map_or(0, String::len))
                .saturating_add(resource.script_context.as_ref().map_or(0, |context| context.base_url.len() + context.nonce.len() + context.credentials_mode.len() + context.referrer_policy.len()))
                .saturating_add(resource.bytes.len())
        }));
        if state.bytes.saturating_add(weight) > PREFETCHED_MODULE_BYTES {
            return Err("module graph exceeded the prefetched resource byte limit".into());
        }
        state.bytes += weight;
        state.count += 1;
        state.entries.entry(request_url).or_default().insert(module_type, result);
        Ok(())
    }

    /// First request wins for a URL/type. Only I/O workers wait; no engine or
    /// document handle is held by this shared resource map.
    pub fn fetch_once_for_type(&self, request_url: &str, module_type: Option<&str>,
        fetch: impl FnOnce() -> Result<PrefetchedModuleResource, String>) -> Result<PrefetchedModuleResource, String> {
        let key = (request_url.to_owned(), module_type.map(str::to_owned));
        let mut state = self.state.lock().map_err(|_|"prefetched module resource cache is poisoned".to_owned())?;
        if let Some(result) = state.entries.get(request_url).and_then(|types|types.get(module_type)) { return result.clone(); }
        if let Some(flight)=state.pending.get(&key).cloned() {
            drop(state);
            let mut result=flight.result.lock().map_err(|_|"module resource flight is poisoned".to_owned())?;
            while result.is_none() {
                result=flight.changed.wait(result).map_err(|_|"module resource flight is poisoned".to_owned())?;
            }
            return result.as_ref().expect("completed flight").clone();
        }
        if state.count + state.pending.len() >= PREFETCHED_MODULE_ENTRIES { return Err("module graph exceeded the resource count limit".into()); }
        let flight=Arc::new(ModuleResourceFlight::default());
        state.pending.insert(key.clone(),flight.clone());
        drop(state);
        let result = fetch();
        let mut state = self.state.lock().map_err(|_|"prefetched module resource cache is poisoned".to_owned())?;
        state.pending.remove(&key);
        // HTML removes failed module-map entries. Existing callers of insert()
        // retain its explicit failure snapshot contract; browser flights retry.
        let result=match result {
            Ok(resource)=>Self::insert_locked(&mut state,request_url.to_owned(),module_type,Ok(resource.clone())).map(|_|resource),
            Err(error)=>Err(error),
        };
        *flight.result.lock().map_err(|_|"module resource flight is poisoned".to_owned())?=Some(result.clone());
        drop(state);
        flight.changed.notify_all();
        result
    }

    /// Read a cached response without changing its response URL or bytes.
    pub fn get(&self, request_url: &str) -> Option<Result<PrefetchedModuleResource, String>> {
        self.get_for_type(request_url, None)
    }
    pub fn get_for_type(&self, request_url: &str, module_type: Option<&str>) -> Option<Result<PrefetchedModuleResource, String>> {
        self.state.lock().ok()?.entries.get(request_url)?.get(module_type).cloned()
    }
}

/// Resolve a static network module request to the exact fragment-free URL used on the wire.
/// This shares the ESM loader's scheme and same-origin rules with an embedder that prepares
/// several module graphs concurrently. Non-network requests (builtins, data URLs, bare names)
/// return `None` and remain the canonical resolver's responsibility.
pub fn resolve_network_module_request(specifier: &str, referrer: &str) -> Option<String> {
    let remote_relative = is_http_url(referrer)
        && (specifier.starts_with("./")
            || specifier.starts_with("../")
            || specifier.starts_with('/')
            || specifier.starts_with('?')
            || specifier.starts_with('#'));
    if !is_http_url(specifier) && !remote_relative {
        return None;
    }
    if !is_http_url(referrer) {
        return None;
    }
    let target = lumen_common::url::parse(specifier, Some(referrer)).ok()?;
    if !matches!(target.scheme.as_str(), "http" | "https") {
        return None;
    }
    let requested_url = strip_url_fragment(&target.href());
    (url_origin(&requested_url)? == url_origin(referrer)?).then_some(requested_url)
}

/// Browser URL resolution permits cross-origin URLs; canonical Fetch enforces
/// CORS rather than the Node loader's same-origin restriction.
pub fn resolve_browser_module_request(specifier: &str, referrer: &str) -> Option<String> {
    let target = match lumen_common::url::parse(specifier, None) {
        Ok(target) => target,
        Err(_) if specifier.starts_with('/') || specifier.starts_with("./") || specifier.starts_with("../") =>
            lumen_common::url::parse(specifier, Some(referrer)).ok()?,
        _ => return None,
    };
    matches!(target.scheme.as_str(), "http" | "https").then(|| target.href())
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub struct BrowserModuleClient {
    pub self_url: String,
    pub origin: String,
    pub referrer_policy: lumen_common::referrer::ReferrerPolicy,
    pub policies: Arc<lumen_common::csp::PolicySet>,
    pub import_map: Option<lumen_common::import_maps::SharedImportMap>,
}

/// Plain captured data only: safe to move onto the host's existing I/O pool.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub struct BrowserModuleFetch {
    pub client: BrowserModuleClient,
    pub referrer: String,
    pub nonce: String,
    pub credentials: String,
    pub referrer_policy: lumen_common::referrer::ReferrerPolicy,
    pub integrity: String,
    pub parser_inserted: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl BrowserModuleFetch {
    pub fn descendant(client: BrowserModuleClient, referrer: &str, context: Option<&lumen::ClassicScriptContext>) -> Self {
        Self {
            referrer: referrer.into(), nonce: context.map_or_else(String::new, |context|context.nonce.clone()),
            credentials: context.map_or_else(|| "same-origin".into(), |context|context.credentials_mode.clone()),
            referrer_policy: context.and_then(|context|lumen_common::referrer::ReferrerPolicy::parse(&context.referrer_policy))
                .unwrap_or(client.referrer_policy),
            integrity: String::new(), parser_inserted: false, client,
        }
    }
    pub fn context_after_response(&self, resource: &PrefetchedModuleResource) -> std::rc::Rc<lumen::ClassicScriptContext> {
        if let Some(context) = &resource.script_context { return std::rc::Rc::new(context.clone()); }
        std::rc::Rc::new(lumen::ClassicScriptContext {
            base_url: resource.final_url.clone(), nonce: self.nonce.clone(), credentials_mode: self.credentials.clone(),
            referrer_policy: self.referrer_policy.name().into(),
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl BrowserModuleClient {
    pub fn resolve(&self, specifier:&str, base:&str) -> Result<lumen_common::import_maps::Resolution,String> {
        if let Some(map)=&self.import_map {
            return map.lock().map_err(|_|"import-map state is unavailable".to_owned())?
                .resolve(specifier,base).map_err(|error|error.to_string());
        }
        lumen_common::import_maps::resolve_without_map(specifier,base).map_err(|error|error.to_string())
    }
    pub fn module_integrity(&self,url:&str)->String {
        self.import_map.as_ref().and_then(|map|map.lock().ok().map(|map|map.integrity(url))).unwrap_or_default()
    }
}

/// Fetch-policy failures retain their violation metadata for the client realm.
#[cfg(not(target_arch = "wasm32"))]
pub fn fetch_browser_module_resource(url: &str, module_type:Option<&str>, fetch: &BrowserModuleFetch, config: &lumen_web::FetchConfig)
    -> (Result<PrefetchedModuleResource, String>, Vec<lumen_common::csp::Violation>) {
    let destination=match module_type {
        None=>lumen_common::csp::Destination::Script,
        Some("json")=>lumen_common::csp::Destination::Json,
        Some("text")=>lumen_common::csp::Destination::Text,
        Some("css")=>lumen_common::csp::Destination::Style,
        _=>return (Err("module type is not supported by the HTML module loader".into()),Vec::new()),
    };
    let validate = |mut resource:PrefetchedModuleResource| {
        if let (Ok(requested),Ok(mut response))=(lumen_common::url::parse(url,None),lumen_common::url::parse(&resource.final_url,None)) {
            if response.fragment.is_none() {
                response.fragment=requested.fragment;
                resource.final_url=response.href();
                if let Some(context)=&mut resource.script_context {context.base_url=resource.final_url.clone();}
            }
        }
        let allowed=match module_type {
            None=>lumen_common::mime::is_javascript_module_mime(resource.content_type.as_deref()),
            Some("json")=>lumen_common::mime::is_json_mime(resource.content_type.as_deref()),
            Some("text")=>true,
            Some("css")=>lumen_common::mime::is_css_module_mime(resource.content_type.as_deref()),
            _=>false,
        };
        if allowed {Ok(resource)}else {Err("module response has an unsupported MIME type".into())}
    };
    if lumen_common::url::parse(url,None).is_ok_and(|url|url.scheme == "data") {
        let mut reports = Vec::new();
        let result = (|| {
            let decision = fetch.client.policies.check_resource_redirect(url,url,&fetch.client.self_url,destination,&fetch.nonce,&fetch.integrity,fetch.parser_inserted,0)
                .map_err(|error|format!("data module policy: {error:?}"))?;
            reports.extend(decision.violations);
            if decision.blocked { return Err("data module blocked by Content Security Policy".into()); }
            let mime = lumen_common::url::data_url_mime_essence(url).ok_or_else(||"invalid data module URL".to_owned())?;
            let bytes = lumen_common::url::data_url_body_bounded(url,PREFETCHED_MODULE_BYTES)
                .map_err(|error|format!("data module body: {error:?}"))?;
            if !lumen_common::integrity::matches(&bytes,&fetch.integrity) {
                return Err("data module body failed Subresource Integrity".into());
            }
            Ok(PrefetchedModuleResource { final_url:url.into(),content_type:Some(mime.into()),bytes:bytes.into(),
                script_context:Some(lumen::ClassicScriptContext { base_url:url.into(),nonce:fetch.nonce.clone(),
                    credentials_mode:fetch.credentials.clone(),referrer_policy:fetch.referrer_policy.name().into() }) })
        })();
        return (result.and_then(validate),reports);
    }
    let mut metadata = lumen_web::ScriptFetchMetadata {
        destination,
        self_url: fetch.client.self_url.clone(), nonce: fetch.nonce.clone(), integrity: fetch.integrity.clone(),
        parser_inserted: fetch.parser_inserted,
        referrer: lumen_common::referrer::Referrer { source: fetch.referrer.clone(), policy: fetch.referrer_policy },
        policies: fetch.client.policies.clone(), violations: Vec::new(),
    };
    let wire_url=strip_url_fragment(url);
    let result = lumen_web::load_script_resource_with_config(&wire_url, &fetch.client.origin, &fetch.credentials,
        config, PREFETCHED_MODULE_BYTES, 30_000, &mut metadata).and_then(|response| {
        if !(200..300).contains(&response.status) { return Err(format!("module response returned HTTP {}", response.status)); }
        Ok(PrefetchedModuleResource { script_context: Some(lumen::ClassicScriptContext {
                base_url: response.url.clone(), nonce: fetch.nonce.clone(), credentials_mode: fetch.credentials.clone(),
                referrer_policy: metadata.referrer.policy.name().into() }), final_url: response.url,
            content_type: response.headers.iter().find(|(name,_)|name.eq_ignore_ascii_case("content-type")).map(|(_,value)|value.clone()),
            bytes: response.body.into() })
    });
    (result.and_then(validate), metadata.violations)
}

/// Reuse the existing resource snapshot and canonical decoding. The map key is
/// the requested URL (including its fragment) and module type, not response URL
/// or fetch options. The response URL is the source/import-resolution base.
#[cfg(not(target_arch = "wasm32"))]
pub fn make_browser_module_loader(
    client: impl Fn() -> Option<BrowserModuleClient> + 'static, config: lumen_web::FetchConfig,
    prefetched: PrefetchedModuleResources,
    violations: Arc<Mutex<Vec<lumen_common::csp::Violation>>>,
) -> impl Fn(lumen::ModuleFetchRequest) -> Option<lumen::ModuleFetchResult> {
    move |request| {
        if !matches!(request.attribute_type.as_deref(), None | Some("json" | "text" | "css")) { return None; }
        let client = client()?;
        let resolution=request.resolution.clone().map(Ok).unwrap_or_else(||client.resolve(&request.specifier,&request.referrer)).ok()?;
        let Some(_) = resolve_browser_module_request(&resolution.url, &request.referrer) else {
            // Bare names require the document import-map algorithm. Never
            // reinterpret them as Node packages or host filesystem paths.
            let requested = lumen_common::url::parse(&resolution.url,None).ok()?;
            if requested.scheme != "data" { return None; }
            let key = requested.href();
            let mut fetch = BrowserModuleFetch::descendant(client,&request.referrer,request.script_context.as_deref());
            fetch.integrity=resolution.integrity;
            let resource = prefetched.fetch_once_for_type(&key,request.attribute_type.as_deref(),|| {
                let (result,reports)=fetch_browser_module_resource(&key,request.attribute_type.as_deref(),&fetch,&config);
                if let Ok(mut pending)=violations.lock() { pending.extend(reports); }
                result
            }).ok()?;
            let allowed = match request.attribute_type.as_deref() {
                None=>lumen_common::mime::is_javascript_module_mime(resource.content_type.as_deref()),
                Some("json")=>lumen_common::mime::is_json_mime(resource.content_type.as_deref()),
                Some("text")=>true,
                Some("css")=>lumen_common::mime::is_css_module_mime(resource.content_type.as_deref()),
                _=>false,
            };
            if !allowed { return None; }
            let context = fetch.context_after_response(&resource);
            let text = String::from_utf8_lossy(&resource.bytes);
            return Some(lumen::ModuleFetchResult { key:key.clone(),source:text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned(),
                script_context:Some(std::rc::Rc::new(lumen::ClassicScriptContext { base_url:key,..context.as_ref().clone() })) });
        };
        let requested = lumen_common::url::parse(&resolution.url, None).ok()?;
        let mut fetch = BrowserModuleFetch::descendant(client, &request.referrer, request.script_context.as_deref());
        fetch.integrity=resolution.integrity;
        let resource = prefetched.fetch_once_for_type(&requested.href(), request.attribute_type.as_deref(), || {
                let (result, reports) = fetch_browser_module_resource(&requested.href(),request.attribute_type.as_deref(), &fetch, &config);
                if let Ok(mut pending) = violations.lock() { pending.extend(reports); }
                result
        }).ok()?;
        let allowed = match request.attribute_type.as_deref() {
            None => lumen_common::mime::is_javascript_module_mime(resource.content_type.as_deref()),
            Some("json") => lumen_common::mime::is_json_mime(resource.content_type.as_deref()),
            Some("text") => true,
            Some("css") => lumen_common::mime::is_css_module_mime(resource.content_type.as_deref()),
            // The engine's non-HTML bytes module type remains available to
            // Node's legacy loader; HTML only supports actual registered types.
            _ => false,
        };
        if !allowed { return None; }
        let mut response = lumen_common::url::parse(&resource.final_url, None).ok()?;
        if response.fragment.is_none() { response.fragment = requested.fragment.clone(); }
        let context = fetch.context_after_response(&resource);
        let text = String::from_utf8_lossy(&resource.bytes);
        Some(lumen::ModuleFetchResult { key: requested.href(),
            source: text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned(),
            script_context: Some(std::rc::Rc::new(lumen::ClassicScriptContext { base_url: response.href(), ..context.as_ref().clone() })) })
    }
}

/// Prepare a browser graph on the existing host I/O pool. Parsing requests uses
/// the engine's canonical module parser; no AST or JS handle crosses threads.
#[cfg(not(target_arch = "wasm32"))]
pub struct BrowserModuleGraphRequest {
    pub url:String,
    pub module_type:Option<String>,
    pub fetch:BrowserModuleFetch,
}
/// One I/O completion. Parsing discovers authored requests but never resolves them or touches
/// the Window's resolved-module set. It contains no engine/realm handles or JavaScript values.
#[cfg(not(target_arch = "wasm32"))]
pub struct BrowserModuleGraphResource {
    request:BrowserModuleGraphRequest,
    resource:Result<PrefetchedModuleResource,String>,
    requests:Vec<lumen::ModuleRequest>,
}
#[cfg(not(target_arch = "wasm32"))]
pub struct BrowserModuleGraph {
    seen:std::collections::HashSet<(String,Option<String>)>,
    pending:usize,
    error:Option<String>,
}
#[cfg(not(target_arch = "wasm32"))]
impl BrowserModuleGraph {
    pub fn start(url:String,module_type:Option<String>,fetch:BrowserModuleFetch)->(Self,BrowserModuleGraphRequest) {
        let graph=Self {seen:std::collections::HashSet::from([(url.clone(),module_type.clone())]),pending:1,error:None};
        (graph,BrowserModuleGraphRequest {url,module_type,fetch})
    }
    /// Deliver on the captured settings thread, after author JavaScript's current turn. Real
    /// resolution/registration precedes child fetch admission, including integrity capture.
    pub fn complete(&mut self,completion:BrowserModuleGraphResource)->Vec<BrowserModuleGraphRequest> {
        self.pending=self.pending.saturating_sub(1);
        if self.error.is_some() {return Vec::new();}
        let resource=match completion.resource {Ok(resource)=>resource,Err(error)=>{self.error=Some(error);return Vec::new();}};
        let context=completion.request.fetch.context_after_response(&resource);
        let mut children=Vec::new();
        for request in completion.requests {
            let resolved=match completion.request.fetch.client.resolve(&request.specifier,&resource.final_url) {
                Ok(resolved)=>resolved,Err(error)=>{self.error=Some(error);return Vec::new();}
            };
            if !self.seen.insert((resolved.url.clone(),request.attribute_type.clone())) {continue;}
            if self.seen.len()>PREFETCHED_MODULE_ENTRIES {self.error=Some("module graph exceeded the resource count limit".into());return Vec::new();}
            let mut fetch=BrowserModuleFetch::descendant(completion.request.fetch.client.clone(),&resource.final_url,Some(context.as_ref()));
            fetch.integrity=resolved.integrity;
            children.push(BrowserModuleGraphRequest {url:resolved.url,module_type:request.attribute_type,fetch});
        }
        self.pending+=children.len();
        children
    }
    pub fn outcome(&self)->Option<Result<(),String>> {
        (self.pending==0).then(||self.error.clone().map_or(Ok(()),Err))
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub fn fetch_browser_module_graph_resource(request:BrowserModuleGraphRequest,config:&lumen_web::FetchConfig,
    resources:&PrefetchedModuleResources)->(BrowserModuleGraphResource,Vec<lumen_common::csp::Violation>) {
    let mut reports=Vec::new();
    let mut requests=Vec::new();
    let resource=(|| {
        if !matches!(request.module_type.as_deref(),None|Some("json"|"text"|"css")) {return Err("module type is not supported by the HTML module loader".into());}
        let resource=resources.fetch_once_for_type(&request.url,request.module_type.as_deref(),|| {
            let (result,violations)=fetch_browser_module_resource(&request.url,request.module_type.as_deref(),&request.fetch,config);
            reports.extend(violations);result
        })?;
        if request.module_type.is_none() {
            if !lumen_common::mime::is_javascript_module_mime(resource.content_type.as_deref()) {return Err("module response has unsupported JavaScript MIME type".into());}
            let text=String::from_utf8_lossy(&resource.bytes);
            // Parse errors remain attached to the real module evaluation, not a host replacement.
            if let Ok(parsed)=lumen::module_requests(text.strip_prefix('\u{feff}').unwrap_or(&text)) {requests=parsed;}
        }
        Ok(resource)
    })();
    (BrowserModuleGraphResource {request,resource,requests},reports)
}
/// Synchronous hosts use the same phased algorithm on their own settings thread.
#[cfg(not(target_arch = "wasm32"))]
pub fn prepare_browser_module_graph(root:&str,module_type:Option<&str>,fetch:BrowserModuleFetch,
    config:&lumen_web::FetchConfig,resources:&PrefetchedModuleResources)->(Result<(),String>,Vec<lumen_common::csp::Violation>) {
    let (mut graph,request)=BrowserModuleGraph::start(root.to_owned(),module_type.map(str::to_owned),fetch);
    let mut pending=vec![request];let mut reports=Vec::new();
    while let Some(request)=pending.pop() {
        let (completion,violations)=fetch_browser_module_graph_resource(request,config,resources);
        reports.extend(violations);pending.extend(graph.complete(completion));
    }
    (graph.outcome().unwrap_or_else(||Err("module graph completion imbalance".into())),reports)
}

/// Fetch one static module graph resource using the same route and trust snapshot as the runtime.
/// HTTP status and content type are retained here; the resolver applies module MIME and final
/// origin rules after it reads the prefetched response.
#[cfg(not(target_arch = "wasm32"))]
pub fn fetch_network_module_resource(
    url: &str,
    fetch_config: &lumen_web::FetchConfig,
) -> Result<PrefetchedModuleResource, String> {
    let response = lumen_web::load_resource_with_config(url, fetch_config)?;
    if !(200..300).contains(&response.status) {
        return Err(format!(
            "module response returned HTTP {} {}",
            response.status, response.status_text
        ));
    }
    let content_type = response.content_type();
    Ok(PrefetchedModuleResource {
        final_url: response.url,
        content_type,
        bytes: response.body.into(),
        script_context: None,
    })
}

#[cfg(target_arch = "wasm32")]
fn default_network_module_resource(_url: &str) -> Option<NetworkModuleResource> {
    None
}

fn make_cached_loader_with_resource_loader(
    builtins: BuiltinModules,
    load_resource: impl Fn(&str) -> Option<NetworkModuleResource> + 'static,
) -> (
    impl Fn(&str, &str, Option<&str>) -> Option<(String, String)>,
    std::rc::Rc<LoaderCache>,
) {
    let cache = std::rc::Rc::new(LoaderCache::default());
    let handle = std::rc::Rc::clone(&cache);
    let loader = move |specifier: &str, referrer: &str, attr_type: Option<&str>| {
        cache.resolve(specifier, referrer, attr_type, |s, r, a| {
            resolve(s, r, &builtins, a, &load_resource)
        })
    };
    (loader, handle)
}

/// Memoized resolution for one loader. The engine asks the loader for every import clause of
/// every module it parses — also for dependencies it has already loaded, whose source it then
/// ignores — so a module graph repeats the same resolutions many times (Puppeteer: ~630 loads for
/// ~170 modules), each one filesystem probes, `package.json` reads and a file read. Resolutions
/// are cached by (specifier, referrer directory, attribute). Redundant source copies are bounded
/// by bytes/entries; evaluated modules and their exports remain cached by the engine. The file
/// system is assumed not to change under a loading module graph
/// (Node's resolver caches the same way). A failed resolution is not cached.
#[derive(Default)]
pub struct LoaderCache {
    /// (specifier, referrer directory, attribute type) -> resolved key.
    keys: std::cell::RefCell<HashMap<(String, String, Option<String>), String>>,
    /// (resolved key, attribute type) -> source.
    sources: std::cell::RefCell<SourceCache>,
    /// Package types and canonical directories, for the resolver's helpers.
    memo: std::cell::RefCell<FsMemo>,
    /// Numeric-only opt-in memory checkpoints while a graph is still loading.
    diagnostic_lookups: std::cell::Cell<usize>,
}

const SOURCE_CACHE_BYTES: usize = 8 * 1024 * 1024;
const SOURCE_CACHE_ENTRIES: usize = 512;
const KEY_CACHE_ENTRIES: usize = 4096;
type SourceKey = (String, Option<String>);

/// Dynamic imports can keep a loader alive for an entire account lifetime. Cache only
/// 8 MiB of redundant source/key bytes plus at most 512 small map records, not the graph.
#[derive(Default)]
struct SourceCache {
    entries: HashMap<SourceKey, (String, u64)>,
    bytes: usize,
    clock: u64,
}
impl SourceCache {
    fn weight(key: &SourceKey, source: &str) -> usize {
        key.0
            .len()
            .saturating_add(key.1.as_ref().map_or(0, String::len))
            .saturating_add(source.len())
    }
    fn get(&mut self, key: &SourceKey) -> Option<String> {
        let entry = self.entries.get_mut(key)?;
        self.clock = self.clock.saturating_add(1);
        entry.1 = self.clock;
        Some(entry.0.clone())
    }
    fn insert(&mut self, key: SourceKey, source: &str) {
        if let Some((old, _)) = self.entries.remove(&key) {
            self.bytes -= Self::weight(&key, &old);
        }
        let weight = Self::weight(&key, source);
        if weight > SOURCE_CACHE_BYTES {
            return;
        }
        while self.entries.len() >= SOURCE_CACHE_ENTRIES
            || self.bytes.saturating_add(weight) > SOURCE_CACHE_BYTES
        {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, (_, time))| *time)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else {
                break;
            };
            let (old, _) = self.entries.remove(&oldest).expect("cached source");
            self.bytes -= Self::weight(&oldest, &old);
        }
        self.clock = self.clock.saturating_add(1);
        self.bytes += weight;
        self.entries.insert(key, (source.to_owned(), self.clock));
    }
    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

/// Filesystem facts the resolver re-derives for nearly every module: the `"type"` of each
/// directory's `package.json` (found by walking up from every resolved file) and each
/// directory's canonical path. A [`LoaderCache`] keeps one and installs it in [`MEMO`] while it
/// resolves, since the resolver's helpers are free functions; without one (no loader resolving)
/// they query the filesystem directly.
#[derive(Default)]
struct FsMemo {
    /// Directory -> `None` when it has no package.json, else that file's `"type"` field.
    pkg_type: HashMap<PathBuf, Option<Option<String>>>,
    /// Directory -> its canonical path.
    canon_dirs: HashMap<PathBuf, PathBuf>,
}

thread_local! {
    static MEMO: std::cell::RefCell<Option<FsMemo>> = const { std::cell::RefCell::new(None) };
}

/// The `package.json` in `dir`: `None` when there is none, else its `"type"` field.
fn dir_package_type(dir: &Path) -> Option<Option<String>> {
    let lookup = || {
        let pkg = dir.join("package.json");
        if !pkg.fs_is_file() {
            return None;
        }
        Some(
            lumen_host::sysfs::read_to_string(pkg)
                .ok()
                .and_then(|t| crate::package_type_from_json(&t)),
        )
    };
    MEMO.with(|m| match m.borrow_mut().as_mut() {
        Some(memo) => memo
            .pkg_type
            .entry(dir.to_path_buf())
            .or_insert_with(lookup)
            .clone(),
        None => lookup(),
    })
}

/// A resolved file's module key: its canonical path. Under a [`FsMemo`] that is the file's
/// directory canonicalized once per directory, plus its name — unless the file itself is a
/// symlink, which is resolved in full like everything else without a memo.
fn canonical_key(file: &Path) -> String {
    let memoized = MEMO.with(|m| m.borrow().is_some());
    let via_dir = || -> Option<PathBuf> {
        let (dir, name) = (file.parent()?, file.file_name()?);
        if dir.as_os_str().is_empty() || !lumen_host::sysfs::exists_not_symlink(file) {
            return None;
        }
        MEMO.with(|m| {
            let mut m = m.borrow_mut();
            let memo = m.as_mut()?;
            if let Some(c) = memo.canon_dirs.get(dir) {
                return Some(c.join(name));
            }
            let c = lumen_host::canonicalize(dir).ok()?;
            memo.canon_dirs.insert(dir.to_path_buf(), c.clone());
            Some(c.join(name))
        })
    };
    memoized
        .then(via_dir)
        .flatten()
        .or_else(|| lumen_host::canonicalize(file).ok())
        .unwrap_or_else(|| file.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

impl LoaderCache {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        attr_type: Option<&str>,
        uncached: impl FnOnce(&str, &str, Option<&str>) -> Option<(String, String)>,
    ) -> Option<(String, String)> {
        if lumen::memstats::enabled() {
            let lookups = self.diagnostic_lookups.get().wrapping_add(1);
            self.diagnostic_lookups.set(lookups);
            if lookups % 4096 == 0 {
                let keys = self.keys.borrow();
                let sources = self.sources.borrow();
                let memo = self.memo.borrow();
                let key_bytes: usize = keys
                    .iter()
                    .map(|((specifier, base, attr), resolved)| {
                        specifier.len()
                            + base.len()
                            + attr.as_ref().map_or(0, String::len)
                            + resolved.len()
                    })
                    .sum();
                let package_bytes: usize = memo
                    .pkg_type
                    .iter()
                    .map(|(path, kind)| {
                        path.as_os_str().as_encoded_bytes().len()
                            + kind
                                .as_ref()
                                .and_then(Option::as_ref)
                                .map_or(0, String::len)
                    })
                    .sum();
                let canonical_bytes: usize = memo
                    .canon_dirs
                    .iter()
                    .map(|(path, canonical)| {
                        path.as_os_str().as_encoded_bytes().len()
                            + canonical.as_os_str().as_encoded_bytes().len()
                    })
                    .sum();
                eprintln!(
                    "[loader-memory] lookups={lookups} resolutions={} key_payload={key_bytes} sources={} source_payload={} packages={} package_payload={package_bytes} canonical_dirs={} canonical_payload={canonical_bytes}",
                    keys.len(),
                    sources.entries.len(),
                    sources.bytes,
                    memo.pkg_type.len(),
                    memo.canon_dirs.len()
                );
            }
        }
        // An `aot:` referrer resolves against the current directory, not its own location.
        let base = if referrer.starts_with("aot:") {
            None
        } else if is_http_url(referrer) {
            // A network module is identified by its complete URL, not its directory. Keeping
            // the query string here matters because it can select distinct module resources.
            Some(strip_url_fragment(referrer))
        } else {
            let r = strip_file_scheme(referrer);
            Some(
                Path::new(r.as_ref())
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
        };
        let attr = attr_type.map(str::to_owned);
        if let Some(base) = &base {
            let key = (specifier.to_owned(), base.clone(), attr.clone());
            if let Some(resolved) = self.keys.borrow().get(&key) {
                let sk = (resolved.clone(), attr.clone());
                if let Some(src) = self.sources.borrow_mut().get(&sk) {
                    return Some((resolved.clone(), src));
                }
            }
        }
        let memo = std::mem::take(&mut *self.memo.borrow_mut());
        let outer = MEMO.with(|m| m.replace(Some(memo)));
        let result = uncached(specifier, referrer, attr_type);
        let memo = MEMO.with(|m| m.replace(outer)).unwrap_or_default();
        *self.memo.borrow_mut() = memo;
        let (resolved, src) = result?;
        if let Some(base) = base {
            let mut sources = self.sources.borrow_mut();
            sources.insert((resolved.clone(), attr.clone()), &src);
            let mut keys = self.keys.borrow_mut();
            if keys.len() >= KEY_CACHE_ENTRIES {
                // A key whose source was evicted can no longer short-circuit a resolution.
                keys.retain(|(_, _, a), r| sources.entries.contains_key(&(r.clone(), a.clone())));
                if keys.len() >= KEY_CACHE_ENTRIES {
                    keys.clear();
                }
            }
            keys.insert((specifier.to_owned(), base, attr), resolved.clone());
        }
        Some((resolved, src))
    }

    /// Drop the cached sources (resolutions are kept; a later import of a forgotten module
    /// reads its file again).
    pub fn forget_sources(&self) {
        self.sources.borrow_mut().clear();
    }
}

fn resolve(
    specifier: &str,
    referrer: &str,
    builtins: &BuiltinModules,
    attr_type: Option<&str>,
    load_resource: &dyn Fn(&str) -> Option<NetworkModuleResource>,
) -> Option<(String, String)> {
    // Builtins: `node:fs` or a bare `fs`/`path`/… name.
    let bare = specifier.strip_prefix("node:").unwrap_or(specifier);
    let key = format!("node:{bare}");
    if let Some(exports) = builtins.0.get(&key) {
        let src = builtin_source(bare, exports);
        return Some((key, src));
    }

    if let Some(source) = data_url_module(specifier) {
        return Some((specifier.to_string(), source));
    }

    // A module of an ahead-of-time blob (`aot:/…`, see `Runtime::run_precompiled`). The engine
    // already resolved everything the blob holds (its own units, the packages it bundled)
    // before asking here, so what is left is not in the blob: a relative path has nothing on
    // disk to be relative to, and a bare package is looked up from the current directory's
    // `node_modules`, as a program started there would.
    if referrer.starts_with("aot:") {
        if attr_type.is_some()
            || specifier.starts_with("./")
            || specifier.starts_with("../")
            || specifier.starts_with("aot:")
        {
            return None;
        }
        if Path::new(specifier).is_absolute() || specifier.starts_with("file://") {
            return resolve(specifier, "", builtins, attr_type, load_resource);
        }
        let cwd = lumen_host::sysfs::current_dir().ok()?;
        let (file, is_esm_pkg) = resolve_node_modules(specifier, &cwd)?;
        return load_as_module(&file, !is_esm_pkg);
    }

    // Network module graphs use the same HTTP(S) transport as fetch(). Keep this branch before
    // filesystem normalization: treating an https referrer as a Path silently turns imports
    // into local paths and loses both redirect identity and URL resolution semantics.
    let remote_relative = is_http_url(referrer)
        && (specifier.starts_with("./")
            || specifier.starts_with("../")
            || specifier.starts_with('/')
            || specifier.starts_with('?')
            || specifier.starts_with('#'));
    if is_http_url(specifier) || remote_relative {
        #[cfg(target_arch = "wasm32")]
        {
            return None;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let requested_url = resolve_network_module_request(specifier, referrer)?;
            let requested_fragment = lumen_common::url::parse(specifier, Some(referrer))
                .ok()?
                .fragment;
            let (resource_url, content_type, bytes) = load_resource(&requested_url)?;
            // Module redirects use same-origin mode here. A prefetched response follows the same
            // final-origin check as the ordinary synchronous loader.
            if url_origin(&resource_url)? != url_origin(&requested_url)? {
                return None;
            }
            if attr_type.is_none() && !lumen_web::is_javascript_module_mime(content_type.as_deref())
            {
                return None;
            }
            let text = String::from_utf8_lossy(&bytes);
            let source = text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned();
            // Fragments do not go on the HTTP request, but they remain part of the module-map
            // identity and import.meta.url. A redirect-provided fragment takes precedence;
            // otherwise URL fetching inherits the request fragment.
            let final_fragment = lumen_common::url::parse(&resource_url, None)
                .ok()?
                .fragment
                .or(requested_fragment);
            let mut final_key = strip_url_fragment(&resource_url);
            if let Some(fragment) = final_fragment {
                final_key.push('#');
                final_key.push_str(&fragment);
            }
            return Some((final_key, source));
        }
    }

    // A network graph may import only URL-relative or absolute network modules (and the
    // built-ins/data URLs handled above). In particular, never reinterpret a bare or file: URL
    // as a path on the runtime host's filesystem.
    if is_http_url(referrer) {
        return None;
    }

    // A dynamic import's referrer is `import.meta.url`, a `file://` URL — reduce it (and a
    // `file://` specifier) to a plain path for the filesystem resolver.
    let referrer_path = strip_file_scheme(referrer);
    let specifier_path = strip_file_scheme(specifier);
    let referrer: &str = &referrer_path;
    let specifier: &str = &specifier_path;

    // Package-private imports belong to the nearest package scope, not node_modules.
    if specifier.starts_with('#') {
        let from = Path::new(referrer).parent()?;
        let file = resolve_package_import(specifier, from)?;
        return load_as_module(&file, !file_is_esm(&file));
    }

    // An absolute filesystem path (`C:\x\y.js` on Windows — which `starts_with('/')` misses and
    // the bare-package walk would reject — or `/x/y.js`) names the file directly; the referrer
    // plays no part. `require(esm)` hands its already-resolved filename in this form.
    if attr_type.is_none() && Path::new(specifier).is_absolute() {
        let file = resolve_file_or_dir(&normalize(Path::new(specifier)))?;
        return load_as_module(&file, !file_is_esm(&file));
    }

    // A `with { type: "json" | "text" | "bytes" }` import wants the file's RAW contents — the
    // engine synthesizes the wrapper module itself. No CJS/ESM classification, no JSX transform:
    // the attribute defines the module type, whatever the extension says (importing a `.js` file
    // as text is a spec-tested case).
    if matches!(attr_type, Some("json" | "text" | "bytes")) {
        let file = if specifier.starts_with("./")
            || specifier.starts_with("../")
            || specifier.starts_with('/')
        {
            let base = Path::new(referrer).parent()?.join(specifier);
            resolve_file_or_dir(&normalize(&base))?
        } else {
            resolve_node_modules(specifier, Path::new(referrer).parent()?)?.0
        };
        let key = canonical_key(&file);
        return Some((key, read_raw(&file, attr_type)?));
    }

    if specifier.starts_with("./") || specifier.starts_with("../") || specifier.starts_with('/') {
        let base = Path::new(referrer).parent()?.join(specifier);
        let file = resolve_file_or_dir(&normalize(&base))?;
        // A relative `.js` file is CommonJS unless its nearest package.json is `type: module` —
        // Node's own rule. (A misresolved-as-ESM CJS file would expose no named exports.)
        let cjs = !file_is_esm(&file);
        return load_as_module(&file, cjs);
    }

    // Bare package name.
    let from = Path::new(referrer).parent()?;
    let (file, is_esm_pkg) = resolve_node_modules(specifier, from)?;
    load_as_module(&file, !is_esm_pkg)
}

fn is_http_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

fn strip_url_fragment(value: &str) -> String {
    match value.find('#') {
        Some(index) => value[..index].to_owned(),
        None => value.to_owned(),
    }
}

fn url_origin(value: &str) -> Option<String> {
    let url = lumen_common::url::parse(value, None).ok()?;
    matches!(url.scheme.as_str(), "http" | "https").then(|| url.origin())
}

/// The source of a `data:text/javascript,...` (or `application/javascript`) module URL, whose
/// body is percent-encoded or, with `;base64`, base64.
fn data_url_module(specifier: &str) -> Option<String> {
    let rest = specifier.strip_prefix("data:")?;
    let (meta, body) = rest.split_once(',')?;
    let mut parts = meta.split(';');
    let mime = parts.next()?.trim().to_ascii_lowercase();
    if mime != "text/javascript" && mime != "application/javascript" {
        return None;
    }
    let base64 = parts.any(|p| p.trim().eq_ignore_ascii_case("base64"));
    let bytes = if base64 {
        lumen_common::codec::base64_decode_forgiving(body.as_bytes())?
    } else {
        lumen_common::codec::percent_decode(body.as_bytes())
    };
    String::from_utf8(bytes).ok()
}

/// Reduce a `file://` URL to a filesystem path: `file:///C:/x` -> `C:/x` on Windows (the drive
/// letter follows the URL path's leading slash), `file:///x` -> `/x` elsewhere. `%XX` escapes are
/// decoded (a path with a space arrives as `%20`). Anything else is returned unchanged.
fn strip_file_scheme(s: &str) -> std::borrow::Cow<'_, str> {
    let Some(rest) = s.strip_prefix("file://") else {
        return std::borrow::Cow::Borrowed(s);
    };
    // `file://localhost/x` is the same as `file:///x`.
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let rest = if cfg!(windows)
        && bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
    {
        &rest[1..]
    } else {
        rest
    };
    if !rest.contains('%') {
        return std::borrow::Cow::Owned(rest.to_string());
    }
    let out = lumen_common::codec::percent_decode(rest.as_bytes());
    std::borrow::Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

/// Escape `text` as a JavaScript double-quoted string literal (for synthesized module source).
fn js_string_literal(text: &str) -> String {
    let mut lit = String::with_capacity(text.len() + 2);
    lit.push('"');
    for c in text.chars() {
        match c {
            '"' => lit.push_str("\\\""),
            '\\' => lit.push_str("\\\\"),
            '\n' => lit.push_str("\\n"),
            '\r' => lit.push_str("\\r"),
            '\u{2028}' => lit.push_str("\\u2028"),
            '\u{2029}' => lit.push_str("\\u2029"),
            c if (c as u32) < 0x20 => {
                lit.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => lit.push(c),
        }
    }
    lit.push('"');
    lit
}

/// Read a file for an attribute import. `text`/`json` decode as UTF-8 the way the web platform's
/// "UTF-8 decode" does — invalid sequences become U+FFFD and a leading BOM is stripped. `bytes`
/// must round-trip exactly, so non-UTF-8 content is latin-1-decoded (one char per byte; the
/// engine re-extracts the original bytes when it builds the `Uint8Array`).
fn read_raw(file: &Path, attr_type: Option<&str>) -> Option<String> {
    let bytes = lumen_host::sysfs::read(file).ok()?;
    Some(match attr_type {
        Some("bytes") => match String::from_utf8(bytes) {
            Ok(t) => t,
            Err(e) => e.into_bytes().iter().map(|&b| b as char).collect(),
        },
        _ => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            text.strip_prefix('\u{feff}')
                .map(str::to_owned)
                .unwrap_or(text)
        }
    })
}

/// A resolved file (existing path, extension probe, or directory index/main). `None` if
/// nothing matches.
fn resolve_file_or_dir(base: &Path) -> Option<PathBuf> {
    if let Some(f) = resolve_file(base) {
        return Some(f);
    }
    if base.fs_is_dir() {
        return resolve_directory(base);
    }
    None
}

fn resolve_file(base: &Path) -> Option<PathBuf> {
    if base.fs_is_file() {
        return Some(base.to_path_buf());
    }
    for ext in EXTENSIONS {
        let mut s = base.as_os_str().to_os_string();
        s.push(ext);
        let candidate = PathBuf::from(s);
        if candidate.fs_is_file() {
            return Some(candidate);
        }
    }
    None
}

fn resolve_directory(dir: &Path) -> Option<PathBuf> {
    let pkg = dir.join("package.json");
    if pkg.fs_is_file() {
        if let Ok(text) = lumen_host::sysfs::read_to_string(&pkg) {
            if let Some(entry) = pkg_entry(&text) {
                let target = normalize(&dir.join(entry));
                if let Some(f) =
                    resolve_file(&target).or_else(|| resolve_file(&target.join("index")))
                {
                    return Some(f);
                }
            }
            // An explicit exports map owns the entry even when blocked, unmatched or absent
            // on disk. Legacy index fallback must not reopen that package root.
            if crate::tsconfig::parse_jsonc(&text)
                .ok()
                .is_some_and(|json| json.get("exports").is_some())
            {
                return None;
            }
        }
    }
    resolve_file(&dir.join("index"))
}

/// The `node_modules` walk. Returns the resolved file and whether the package is ESM (so the
/// caller loads real ESM vs. a CJS default-export wrapper).
fn resolve_node_modules(name: &str, start: &Path) -> Option<(PathBuf, bool)> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.file_name().is_some_and(|n| n == "node_modules") {
            dir = d.parent();
            continue;
        }
        // An exports map owns every package subpath, including one with a legacy file
        // of the same name. Check it before probing physical files/directories.
        if let Some(subpath) = package_subpath(name) {
            let pkg_dir = package_dir(d, name);
            if let Ok(text) = lumen_host::sysfs::read_to_string(pkg_dir.join("package.json")) {
                if crate::tsconfig::parse_jsonc(&text)
                    .ok()
                    .is_some_and(|json| json.get("exports").is_some())
                {
                    let entry = exports_subpath(&text, &subpath)?;
                    let mapped = normalize(&pkg_dir.join(entry));
                    if !mapped.fs_is_file() {
                        return None;
                    }
                    let esm = file_is_esm(&mapped);
                    return Some((mapped, esm));
                }
            }
        }
        let target = d.join("node_modules").join(name);
        if target.fs_is_dir() {
            if let Some(f) = resolve_directory(&target) {
                // The resolved file's own nearest package.json decides ESM-ness — a package can
                // ship an ESM build under `dist/es/` with its own `{"type":"module"}`.
                let esm = file_is_esm(&f);
                return Some((f, esm));
            }
            if package_subpath(name).is_none() && target.join("package.json").fs_is_file() {
                return None;
            }
        }
        // A bare specifier can also point straight at a file (`pkg/sub.js`).
        if let Some(f) = resolve_file(&target) {
            let esm = file_is_esm(&f);
            return Some((f, esm));
        }
        dir = d.parent();
    }
    None
}

fn resolve_package_import(name: &str, start: &Path) -> Option<PathBuf> {
    use crate::tsconfig::parse_jsonc;
    if name == "#" || name.starts_with("#/") {
        return None;
    }
    let mut dir = Some(start);
    while let Some(scope) = dir {
        let manifest = scope.join("package.json");
        if manifest.fs_is_file() {
            let json = parse_jsonc(&lumen_host::sysfs::read_to_string(manifest).ok()?).ok()?;
            let entry = json.get("imports")?.get(name)?;
            let entry = package_esm_target(entry).target()?;
            // Relative exact targets cover package-private ESM dependencies such as Chalk.
            // Do not reinterpret unmapped names as arbitrary filesystem paths.
            if !entry.starts_with("./")
                || entry
                    .split('/')
                    .any(|part| part == ".." || part == "node_modules")
            {
                return None;
            }
            return resolve_file(&scope.join(entry));
        }
        if scope.file_name().is_some_and(|name| name == "node_modules") {
            return None;
        }
        dir = scope.parent();
    }
    None
}

/// The package root for a bare specifier under `<parent>/node_modules`: the first path segment,
/// or the first two for a scoped `@scope/name`.
fn package_dir(parent: &Path, name: &str) -> PathBuf {
    let mut parts = name.split('/');
    let mut pkg = String::new();
    if let Some(first) = parts.next() {
        pkg.push_str(first);
        if first.starts_with('@') {
            if let Some(scope_name) = parts.next() {
                pkg.push('/');
                pkg.push_str(scope_name);
            }
        }
    }
    parent.join("node_modules").join(pkg)
}

/// Turn a resolved file into `(canonical_key, source)`. `.mjs`/`.js` are real ESM; `.json`
/// and (when `cjs_default`) `.cjs`/CJS packages get a synthetic default-export wrapper.
fn load_as_module(file: &Path, cjs_default: bool) -> Option<(String, String)> {
    let key = canonical_key(file);
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "json" => {
            // Route through JSON.parse rather than embedding the text as an expression: an
            // object literal would give `"__proto__"` keys prototype-SETTING semantics (a
            // pollution vector), where JSON.parse creates a plain own data property — the same
            // semantics as a `with { type: "json" }` import.
            let text = lumen_host::sysfs::read_to_string(file).ok()?;
            Some((
                key,
                format!("export default JSON.parse({});", js_string_literal(&text)),
            ))
        }
        // `.mjs` is always ESM regardless of package type; `.cjs` is always CommonJS.
        "mjs" => Some((key.clone(), lumen_host::sysfs::read_to_string(file).ok()?)),
        // The bare engine parses JSX/TSX natively, keeping the original source coordinates.
        "jsx" | "tsx" => Some((key, lumen_host::sysfs::read_to_string(file).ok()?)),
        "cjs" => Some((key.clone(), cjs_wrapper(&key, source_of(file)))),
        // TypeScript: the engine parses it itself by the key's extension (Node's strip-only
        // semantics, every offset kept; `require` of a `.cts`/CJS `.ts` goes through
        // node:module, which compiles it the same way). Syntax it cannot run throws Node's
        // SyntaxError when the module is parsed.
        "mts" => Some((key, lumen_host::sysfs::read_to_string(file).ok()?)),
        "cts" => Some((key.clone(), cjs_wrapper(&key, source_of(file)))),
        _ if cjs_default => {
            let text = source_of(file);
            // Node's syntax detection: a `.js`/`.ts` file outside any package "type" that only
            // parses as a module (it has `import`/`export`) is ESM after all.
            if (ext == "js" || ext == "ts")
                && package_type_of(file).is_none()
                && detect_module_syntax(&text, ext == "ts")
            {
                return Some((key, text));
            }
            Some((key.clone(), cjs_wrapper(&key, text)))
        }
        _ => {
            let text = lumen_host::sysfs::read_to_string(file).ok()?;
            Some((key, text))
        }
    }
}

/// Whether a resolved file loads as ESM: `.mjs`/`.mts` always, `.cjs`/`.cts` never, and
/// `.js`/`.ts` per the nearest enclosing `package.json` `"type"` (absent ⇒ CommonJS, Node's
/// default).
pub(crate) fn file_is_esm(file: &Path) -> bool {
    match file.extension().and_then(|e| e.to_str()) {
        Some("mjs" | "mts") => true,
        Some("cjs" | "cts") => false,
        _ => {
            let mut dir = file.parent();
            while let Some(d) = dir {
                if let Some(ty) = dir_package_type(d) {
                    return ty.as_deref() == Some("module");
                }
                dir = d.parent();
            }
            false
        }
    }
}

/// The nearest enclosing package.json's `"type"` field (`None` when absent — or no package).
fn package_type_of(file: &Path) -> Option<String> {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if let Some(ty) = dir_package_type(d) {
            return ty;
        }
        dir = d.parent();
    }
    None
}

/// Whether `src` is ESM by Node's detection rule: it has module syntax and does not parse as a
/// CommonJS body. The textual pre-check — a line that starts with an `import`/`export`
/// declaration or uses `import.meta` — keeps ordinary CommonJS to no extra parse at all; only a
/// candidate is test-parsed as a script. (A file that is neither is left to the module parser,
/// which reports its syntax error.)
fn detect_module_syntax(src: &str, ts: bool) -> bool {
    let candidate = src.lines().any(|line| {
        let l = line.trim_start();
        let after = |kw: &str| l.strip_prefix(kw).and_then(|r| r.chars().next());
        matches!(
            after("import"),
            Some(' ' | '\t' | '{' | '*' | '"' | '\'' | '.')
        ) || matches!(after("export"), Some(' ' | '\t' | '{' | '*'))
    });
    candidate
        && if ts {
            !lumen::typescript::parses_as_commonjs(src)
        } else {
            lumen::compile_snapshot(src).is_err()
        }
}

/// Read a file's source (empty on failure — the wrapper still yields a working default export).
fn source_of(file: &Path) -> String {
    lumen_host::sysfs::read_to_string(file).unwrap_or_default()
}

/// A synthetic ESM module bridging a CommonJS file: `require(path)`'s result is the default
/// export, and each export name statically discovered in the source becomes a live named export.
/// This is the cjs-module-lexer interop that lets `import { x } from './cjs-file'` link.
fn cjs_wrapper(abs_path: &str, source: String) -> String {
    let mut out = format!(
        "const __m = globalThis.require({});\nexport default __m;\n",
        crate::js_source_string(abs_path)
    );
    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    collect_cjs_exports(&source, Path::new(abs_path), 0, &mut names, &mut seen);
    for name in names {
        // `export const NAME = __m["NAME"];` — a live binding onto the CJS export (undefined if
        // the static scan over-approximated, which is harmless).
        out.push_str(&format!(
            "export const {name} = __m[{}];\n",
            crate::js_source_string(&name)
        ));
    }
    out
}

/// Discover a CJS module's export names, following `module.exports = require('./x')` /
/// `Object.assign(module.exports, require('./x'))` re-exports transitively (bounded depth) — the
/// pattern that indirection files like `react-dom/server.js` use. `file` is the module being
/// scanned, so relative re-export targets can be resolved and read.
fn collect_cjs_exports(
    src: &str,
    file: &Path,
    depth: u32,
    names: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    for name in scan_cjs_exports(src) {
        add_export(names, seen, &name);
    }
    if depth >= 4 {
        return; // guard against cycles / pathological chains
    }
    for spec in reexport_requires(src) {
        if !spec.starts_with('.') {
            continue; // only follow relative re-exports (a bare package is its own module)
        }
        if let Some(target) = file
            .parent()
            .and_then(|d| resolve_relative_cjs(&d.join(&spec)))
        {
            if let Ok(sub) = lumen_host::sysfs::read_to_string(&target) {
                collect_cjs_exports(&sub, &target, depth + 1, names, seen);
            }
        }
    }
}

/// Resolve a relative `require` target to a file (exact, then `.js`/`.cjs`/`.json` *appended*,
/// then `index.js`), for re-export following. Extensions are appended, not substituted, so
/// `require('./server.node')` resolves to `server.node.js` rather than `server.js`.
fn resolve_relative_cjs(base: &Path) -> Option<PathBuf> {
    let base = normalize(base);
    if base.fs_is_file() {
        return Some(base);
    }
    for ext in [".js", ".cjs", ".json"] {
        let mut s = base.as_os_str().to_os_string();
        s.push(ext);
        let cand = PathBuf::from(s);
        if cand.fs_is_file() {
            return Some(cand);
        }
    }
    let index = base.join("index.js");
    index.fs_is_file().then_some(index)
}

/// The relative specifiers a module re-exports wholesale: `module.exports = require('X')`,
/// `Object.assign(module.exports, require('X'))` and TypeScript's `__exportStar(require('X'),
/// exports)` / `__export(require('X'))` (also through `tslib`), as cjs-module-lexer detects them.
fn reexport_requires(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in src.match_indices("require(") {
        // Look back for a `module.exports =`, `Object.assign(module.exports,` or a TypeScript
        // star-export helper call just before.
        let before = src[..i].trim_end();
        let is_reexport = before.ends_with("module.exports =")
            || before.ends_with("module.exports=")
            || before.ends_with("Object.assign(module.exports,")
            || before.ends_with("Object.assign(exports,")
            || before.ends_with("__exportStar(")
            || before.ends_with("__export(");
        if !is_reexport {
            continue;
        }
        let after = &src[i + "require(".len()..];
        if let Some(spec) = leading_string_literal(after.trim_start()) {
            out.push(spec);
        }
    }
    out
}

/// Statically discover a CommonJS module's export names — the cjs-module-lexer heuristic. Handles
/// the two dominant shapes: direct assignment (`exports.X =` / `module.exports.X =`) and the
/// `Object.defineProperty(exports, "X", …)` that transpiled ESM emits. A miss just means a name
/// isn't re-exported (same as before); a false positive is a harmless `undefined` export.
fn scan_cjs_exports(src: &str) -> Vec<String> {
    let bytes = src.as_bytes();
    let mut names: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // `exports.NAME =` and `module.exports.NAME =` (the `module.` prefix is subsumed).
    for (i, _) in src.match_indices("exports.") {
        let start = i + "exports.".len();
        let name = read_ident(bytes, start);
        if !name.is_empty() && next_is_assignment(bytes, start + name.len()) {
            add_export(&mut names, &mut seen, &name);
        }
    }

    // `Object.defineProperty(exports, "NAME", …)` — the transpiler pattern.
    for (i, _) in src.match_indices("defineProperty(") {
        let rest = &src[i + "defineProperty(".len()..];
        let rest = rest.trim_start();
        let rest = rest
            .strip_prefix("module.exports")
            .or_else(|| rest.strip_prefix("exports"));
        if let Some(rest) = rest {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix(',') {
                if let Some(name) = leading_string_literal(rest.trim_start()) {
                    add_export(&mut names, &mut seen, &name);
                }
            }
        }
    }

    // `module.exports = { a, b: … }` — including the `0 && (module.exports = { … })` hint that
    // bundlers (esbuild, tsc) emit specifically for CJS export lexers.
    for (i, _) in src.match_indices("module.exports") {
        let rest = src[i + "module.exports".len()..].trim_start();
        if let Some(rest) = rest.strip_prefix('=') {
            if let Some(rest) = rest.trim_start().strip_prefix('{') {
                for key in object_key_list(rest) {
                    add_export(&mut names, &mut seen, &key);
                }
            }
        }
    }

    // `__export(target, { name: () => name, … })` — the esbuild/tsc re-export helper.
    for (i, _) in src.match_indices("__export(") {
        let rest = &src[i + "__export(".len()..];
        if let Some(comma) = rest.find(',') {
            if let Some(obj) = rest[comma + 1..].trim_start().strip_prefix('{') {
                for key in object_key_list(obj) {
                    add_export(&mut names, &mut seen, &key);
                }
            }
        }
    }

    names
}

/// The top-level keys of an object literal, given the text just past its opening `{`. Handles
/// shorthand (`{ a, b }`), `key:` pairs, string keys, and nested braces/brackets/parens/strings.
fn object_key_list(after_brace: &str) -> Vec<String> {
    let b = after_brace.as_bytes();
    let mut keys = Vec::new();
    let mut depth = 1usize; // already inside the outer `{`
    let mut expect_key = true;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'{' | b'[' | b'(' => {
                depth += 1;
                expect_key = false;
                i += 1;
            }
            b'}' | b']' | b')' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    break;
                }
            }
            b'"' | b'\'' | b'`' => {
                let quote = c;
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j] != quote {
                    if b[j] == b'\\' {
                        j += 1;
                    }
                    j += 1;
                }
                if expect_key && depth == 1 {
                    keys.push(String::from_utf8_lossy(&b[start..j.min(b.len())]).into_owned());
                    expect_key = false;
                }
                i = j + 1;
            }
            b',' if depth == 1 => {
                expect_key = true;
                i += 1;
            }
            b':' if depth == 1 => {
                expect_key = false;
                i += 1;
            }
            _ => {
                if expect_key && depth == 1 && (c.is_ascii_alphabetic() || c == b'_' || c == b'$') {
                    let name = read_ident(b, i);
                    i += name.len();
                    keys.push(name);
                    expect_key = false;
                } else {
                    i += 1;
                }
            }
        }
    }
    keys
}

fn add_export(names: &mut Vec<String>, seen: &mut std::collections::HashSet<String>, name: &str) {
    if is_export_ident(name) && seen.insert(name.to_string()) {
        names.push(name.to_string());
    }
}

/// Read a JS identifier starting at `pos`.
fn read_ident(bytes: &[u8], pos: usize) -> String {
    let mut end = pos;
    while end < bytes.len() {
        let c = bytes[end];
        let ok = c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
        if !ok {
            break;
        }
        end += 1;
    }
    String::from_utf8_lossy(&bytes[pos..end]).into_owned()
}

/// Whether the next non-space token at `pos` is a plain `=` (assignment) rather than `==`/`=>`.
fn next_is_assignment(bytes: &[u8], mut pos: usize) -> bool {
    while pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b'\t') {
        pos += 1;
    }
    pos < bytes.len()
        && bytes[pos] == b'='
        && bytes.get(pos + 1) != Some(&b'=')
        && bytes.get(pos + 1) != Some(&b'>')
}

/// The contents of a leading `"…"`/`'…'` string literal (no escapes handled — export names are
/// plain identifiers in practice).
fn leading_string_literal(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let quote = *bytes.first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let rest = &s[1..];
    let end = rest.find(quote as char)?;
    Some(rest[..end].to_string())
}

/// A valid, non-reserved identifier usable as an `export const` name (excludes `default` and
/// the transpiler marker `__esModule`).
fn is_export_ident(name: &str) -> bool {
    if name.is_empty() || name == "default" || name == "__esModule" {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    {
        return false;
    }
    !is_reserved_word(name)
}

/// ES reserved words that cannot be `export const` binding names.
fn is_reserved_word(name: &str) -> bool {
    matches!(
        name,
        "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "import"
            | "in"
            | "instanceof"
            | "new"
            | "null"
            | "return"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
            | "let"
            | "static"
            | "await"
    )
}

/// Relative targets only; external private targets remain unsupported.
#[derive(Debug, PartialEq)]
enum PackageEsmTarget<'a> {
    Target(&'a str),
    Blocked,
    NoMatch,
    Unsupported,
}

impl<'a> PackageEsmTarget<'a> {
    fn target(self) -> Option<&'a str> {
        match self {
            Self::Target(target) => Some(target),
            _ => None,
        }
    }
}

/// Preserve declaration order; unmatched nested conditions allow later active siblings,
/// while an explicit null blocks the target rather than falling through.
fn package_esm_target(value: &crate::tsconfig::Json) -> PackageEsmTarget<'_> {
    use crate::tsconfig::Json;
    match value {
        Json::Str(value) if relative_package_target(value) => PackageEsmTarget::Target(value),
        Json::Null => PackageEsmTarget::Blocked,
        Json::Arr(targets) => {
            if targets.is_empty() {
                return PackageEsmTarget::Blocked;
            }
            // Node arrays skip unmatched conditions, nulls and invalid targets in order.
            // A null resets the remembered invalid result; without a valid target, the
            // last null/invalid result wins. Missing files do not trigger array fallback.
            let mut last = PackageEsmTarget::NoMatch;
            for target in targets {
                match package_esm_target(target) {
                    resolved @ PackageEsmTarget::Target(_) => return resolved,
                    PackageEsmTarget::NoMatch => {}
                    resolved => last = resolved,
                }
            }
            last
        }
        Json::Obj(conditions) => {
            for (condition, value) in conditions {
                if matches!(condition.as_str(), "node" | "import" | "default") {
                    let resolved = package_esm_target(value);
                    if resolved != PackageEsmTarget::NoMatch {
                        return resolved;
                    }
                }
            }
            PackageEsmTarget::NoMatch
        }
        _ => PackageEsmTarget::Unsupported,
    }
}

/// Legacy root module/main fields apply only when the package has no exports field.
fn pkg_entry(pkg_json: &str) -> Option<String> {
    let json = crate::tsconfig::parse_jsonc(pkg_json).ok()?;
    if let Some(exports) = json.get("exports") {
        return package_esm_target(exports.get(".").unwrap_or(exports))
            .target()
            .map(str::to_string);
    }
    ["module", "main"]
        .into_iter()
        .find_map(|field| match json.get(field)? {
            crate::tsconfig::Json::Str(value) => Some(value.clone()),
            _ => None,
        })
}

/// The subpath of a bare specifier, if any: `hono/logger` -> `logger`, `@sc/pkg/a/b` -> `a/b`,
/// and `hono` / `@sc/pkg` (bare package roots) -> `None`.
fn package_subpath(name: &str) -> Option<String> {
    let mut parts = if name.starts_with('@') {
        name.splitn(3, '/')
    } else {
        name.splitn(2, '/')
    };
    parts.next()?; // package (or scope)
    if name.starts_with('@') {
        parts.next()?; // scoped name
    }
    parts.next().map(str::to_string)
}

/// Exact subpaths take precedence; single-star patterns prefer the longest static prefix,
/// then the longest key (Node's pattern-key precedence). Star captures can contain slashes.
fn exports_subpath(pkg_json: &str, subpath: &str) -> Option<String> {
    use crate::tsconfig::Json;
    let json = crate::tsconfig::parse_jsonc(pkg_json).ok()?;
    let exports = json.get("exports")?;
    let key = format!("./{subpath}");
    if let Some(target) = exports.get(&key) {
        return package_esm_target(target).target().map(str::to_string);
    }
    let Json::Obj(entries) = exports else {
        return None;
    };
    let mut matched = None;
    for (pattern, target) in entries {
        let Some(star) = pattern.find('*') else {
            continue;
        };
        if pattern[star + 1..].contains('*') {
            continue;
        }
        let (prefix, suffix) = (&pattern[..star], &pattern[star + 1..]);
        if key.len() < prefix.len() + suffix.len()
            || !key.starts_with(prefix)
            || !key.ends_with(suffix)
        {
            continue;
        }
        let rank = (prefix.len(), pattern.len());
        if matched.as_ref().is_some_and(|(best, _, _)| *best >= rank) {
            continue;
        }
        matched = Some((rank, target, &key[prefix.len()..key.len() - suffix.len()]));
    }
    let (_, target, capture) = matched?;
    let resolved = package_esm_target(target).target()?.replace('*', capture);
    relative_package_target(&resolved).then_some(resolved)
}

fn relative_package_target(value: &str) -> bool {
    value.starts_with("./")
        && !value.split('/').skip(1).any(|part| {
            let lower = part.to_ascii_lowercase();
            let dots = lower.replace("%2e", ".");
            dots == "."
                || dots == ".."
                || lower == "node_modules"
                || lower.contains("%2f")
                || lower.contains("%5c")
                || lower.contains('\\')
        })
}

/// Lexically resolve `.`/`..` without touching the filesystem (the target may not exist yet).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn specification_browser_module_response_options_and_typed_resource_identity_survive_reuse() {
        use super::*;
        use std::sync::atomic::{AtomicUsize,Ordering};
        let resources = PrefetchedModuleResources::default();
        let fetches = AtomicUsize::new(0);
        let response_context = lumen::ClassicScriptContext {
            base_url:"https://cdn.test/final.js".into(), nonce:"first".into(),
            credentials_mode:"include".into(), referrer_policy:"no-referrer".into(),
        };
        let resource = PrefetchedModuleResource {
            final_url:response_context.base_url.clone(),content_type:Some("text/javascript".into()),
            bytes:Arc::from(&b"export const value = 1;"[..]),script_context:Some(response_context.clone()),
        };
        resources.fetch_once_for_type("https://page.test/data#identity",None,|| {
            fetches.fetch_add(1,Ordering::Relaxed); Ok(resource.clone())
        }).unwrap();
        resources.fetch_once_for_type("https://page.test/data#identity",None,|| {
            panic!("a later graph cannot replace the first effective response options")
        }).unwrap();
        resources.insert_for_type("https://page.test/data#identity".into(),Some("json"),Ok(PrefetchedModuleResource {
            final_url:"https://cdn.test/data.json".into(), content_type:Some("application/json".into()),
            bytes:Arc::from(&b"{\"value\":2}"[..]),script_context:Some(response_context.clone()),
        })).unwrap();
        resources.insert_for_type("https://page.test/data#identity".into(),Some("text"),Ok(PrefetchedModuleResource {
            final_url:"https://cdn.test/text".into(),content_type:Some("image/png".into()),
            bytes:Arc::from(&b"text payload"[..]),script_context:Some(response_context.clone()),
        })).unwrap();
        let client = BrowserModuleClient {
            self_url:"https://page.test/document".into(),origin:"https://page.test".into(),
            referrer_policy:lumen_common::referrer::ReferrerPolicy::Origin,
            policies:Arc::new(lumen_common::csp::PolicySet::default()),import_map:None,
        };
        let loader = make_browser_module_loader(move ||Some(client.clone()),lumen_web::FetchConfig::default(),resources.clone(),Arc::new(Mutex::new(Vec::new())));
        let request = |kind:Option<&str>|lumen::ModuleFetchRequest {
            settings_key:1,resolution:None,specifier:"https://page.test/data#identity".into(),referrer:"https://page.test/source.js".into(),
            attribute_type:kind.map(str::to_owned),script_context:Some(std::rc::Rc::new(lumen::ClassicScriptContext {
                base_url:"https://page.test/source.js".into(),nonce:"later".into(),credentials_mode:"omit".into(),referrer_policy:"origin".into(),
            })),
        };
        let javascript = loader(request(None)).expect("JavaScript response");
        assert_eq!(javascript.key,"https://page.test/data#identity");
        let context = javascript.script_context.expect("response context");
        assert_eq!(context.base_url,"https://cdn.test/final.js#identity");
        assert_eq!(context.nonce,"first");
        assert_eq!(context.credentials_mode,"include");
        assert_eq!(context.referrer_policy,"no-referrer");
        let json = loader(request(Some("json"))).expect("separate JSON response");
        assert_eq!(json.key,javascript.key);
        assert_eq!(json.source,"{\"value\":2}");
        assert_eq!(loader(request(Some("text"))).expect("HTML text module ignores MIME type").source,"text payload");
        assert!(loader(request(Some("bytes"))).is_none(),"Node byte modules are not an HTML module type");
        let mut data_request = request(Some("json"));
        data_request.specifier = "data:application/json,%7B%22value%22%3A3%7D#identity".into();
        let data = loader(data_request).expect("canonical bounded data-URL JSON decoding");
        assert_eq!(data.source,"{\"value\":3}");
        assert!(data.key.ends_with("#identity"));
        assert_eq!(fetches.load(Ordering::Relaxed),1);
        assert!(resources.fetch_once_for_type("https://page.test/retry",None,||Err("network failure".into())).is_err());
        assert!(resources.get("https://page.test/retry").is_none(),"failed browser flights are removed");
        assert!(resources.fetch_once_for_type("https://page.test/retry",None,||Ok(resource.clone())).is_ok(),"later requests retry after a failed flight");
        resources.insert_for_type("https://page.test/data#identity".into(),Some(""),Ok(PrefetchedModuleResource {
            final_url:"https://page.test/empty-type".into(),content_type:None,bytes:Arc::from(&b"empty"[..]),script_context:None,
        })).unwrap();
        assert_eq!(resources.get_for_type("https://page.test/data#identity",Some("")).unwrap().unwrap().bytes.as_ref(),b"empty");
        assert_eq!(resources.get("https://page.test/data#identity").unwrap().unwrap().bytes.as_ref(),resource.bytes.as_ref(),"absent and empty types are distinct without sentinels");
        assert!(resources.get("https://page.test/data#other").is_none(),"fragments select distinct response-option identities");
        let integrity_client=BrowserModuleClient {self_url:"https://page.test/".into(),origin:"https://page.test".into(),
            referrer_policy:lumen_common::referrer::ReferrerPolicy::Origin,policies:Arc::new(lumen_common::csp::PolicySet::default()),import_map:None};
        let mut integrity_fetch=BrowserModuleFetch::descendant(integrity_client,"https://page.test/main.js",None);
        integrity_fetch.integrity="sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=".into();
        assert!(fetch_browser_module_resource("data:text/plain,abc",Some("text"),&integrity_fetch,&lumen_web::FetchConfig::default()).0.is_ok());
        assert!(fetch_browser_module_resource("data:text/plain,abcd",Some("text"),&integrity_fetch,&lumen_web::FetchConfig::default()).0.is_err());
        assert!(resolve_browser_module_request("package","https://page.test/source.js").is_none());
        assert_eq!(resolve_browser_module_request("https://other.test/module.js#fragment","https://page.test/source.js").as_deref(),Some("https://other.test/module.js#fragment"));
    }

    #[test]
    fn specification_import_maps_graph_io_returns_requests_before_settings_thread_resolution() {
        let map=lumen_common::import_maps::ImportMapState::shared();
        let client=BrowserModuleClient {self_url:"https://page.test/document".into(),origin:"https://page.test".into(),
            referrer_policy:lumen_common::referrer::ReferrerPolicy::Origin,policies:Arc::new(lumen_common::csp::PolicySet::default()),import_map:Some(map.clone())};
        let root="data:text/javascript,import%20%27late%27%3B";
        let resources=PrefetchedModuleResources::default();
        let (mut graph,request)=BrowserModuleGraph::start(root.into(),None,BrowserModuleFetch::descendant(client,"https://page.test/document",None));
        let worker_resources=resources.clone();
        let (completion,_)=std::thread::spawn(move||fetch_browser_module_graph_resource(request,&lumen_web::FetchConfig::default(),&worker_resources)).join().unwrap();
        // An author turn may register a map after the response arrives but before its task runs.
        map.lock().unwrap().register(lumen_common::import_maps::ImportMap::parse(
            r#"{"imports":{"late":"data:text/javascript,export%20const%20value%3D1%3B"}}"#,"https://page.test/document").unwrap()).unwrap();
        let mut children=graph.complete(completion);
        assert_eq!(children.len(),1);
        assert_eq!(children[0].url,"data:text/javascript,export%20const%20value%3D1%3B");
        assert!(graph.outcome().is_none());
        let (completion,_)=fetch_browser_module_graph_resource(children.pop().unwrap(),&lumen_web::FetchConfig::default(),&resources);
        assert!(graph.complete(completion).is_empty());
        assert!(matches!(graph.outcome(),Some(Ok(()))));
    }

    #[test]
    fn specification_import_maps_actual_async_graph_resolution_integrity_and_typed_cache() {
        let state=lumen_common::import_maps::ImportMapState::shared();
        state.lock().unwrap().register(lumen_common::import_maps::ImportMap::parse(
            r#"{"imports":{"mapped":"data:text/javascript,export%20const%20value%3D9%3B","blocked":null}}"#,"https://page.test/document").unwrap()).unwrap();
        let client=BrowserModuleClient {self_url:"https://page.test/document".into(),origin:"https://page.test".into(),
            referrer_policy:lumen_common::referrer::ReferrerPolicy::Origin,
            policies:Arc::new(lumen_common::csp::PolicySet::default()),import_map:Some(state.clone())};
        let resources=PrefetchedModuleResources::default();
        let root="data:text/javascript,import%20%27mapped%27%3B";
        let fetch=BrowserModuleFetch::descendant(client.clone(),"https://page.test/document",None);
        assert!(prepare_browser_module_graph(root,None,fetch,&lumen_web::FetchConfig::default(),&resources).0.is_ok());
        let mapped=client.resolve("mapped",root).unwrap();
        assert!(resources.get(&mapped.url).is_some(),"the real graph loader fetched mapped dependencies through canonical resource routing");
        let loader=make_browser_module_loader(move||Some(client.clone()),lumen_web::FetchConfig::default(),resources.clone(),Arc::new(Mutex::new(Vec::new())));
        let module=loader(lumen::ModuleFetchRequest {settings_key:1,resolution:Some(mapped.clone()),specifier:"mapped".into(),referrer:root.into(),attribute_type:None,script_context:None}).expect("same typed requested-URL module cache");
        assert_eq!(module.key,mapped.url);
        assert_eq!(module.source,"export const value=9;");
        assert!(state.lock().unwrap().resolve("blocked",root).is_err());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn specification_browser_module_repeated_typed_imports_settle_and_retry_failed_mime() {
        use super::*;
        use std::io::{Read,Write};
        use std::rc::Rc;
        use std::cell::RefCell;
        use std::sync::atomic::{AtomicBool,Ordering};
        let listener=std::net::TcpListener::bind("127.0.0.1:0").expect("module server");
        let address=listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stopping=Arc::new(AtomicBool::new(false));let stop=stopping.clone();
        let server=std::thread::spawn(move|| {
            let mut requests=HashMap::<String,usize>::new();
            let deadline=std::time::Instant::now()+std::time::Duration::from_secs(5);
            while !stop.load(Ordering::Relaxed) && std::time::Instant::now()<deadline {
                let (mut stream,_)=match listener.accept() {
                    Ok(value)=>value,Err(error) if error.kind()==std::io::ErrorKind::WouldBlock=>{std::thread::sleep(std::time::Duration::from_millis(1));continue;},Err(error)=>panic!("accept: {error}"),
                };
                // Accepted sockets can inherit nonblocking mode on macOS.
                // The bounded fixture reads use their actual blocking timeout.
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
                let mut request=Vec::new();let mut chunk=[0;1024];
                while !request.windows(4).any(|window|window==b"\r\n\r\n") {
                    let read=stream.read(&mut chunk).unwrap();assert_ne!(read,0);request.extend_from_slice(&chunk[..read]);
                }
                let text=String::from_utf8_lossy(&request);let path=text.split_ascii_whitespace().nth(1).expect("path").to_owned();
                let count=requests.entry(path.clone()).or_default();
                let javascript=match path.as_str() {"/retry"|"/firsttext"=>*count!=0,"/firstjs"=>*count==0,_=>false};
                *count+=1;
                let (mime,body)=if javascript {("text/javascript","export default 'world';")}else {("text/plain","hello")};
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
            requests
        });
        let origin=format!("http://{address}");
        let client=BrowserModuleClient {self_url:format!("{origin}/document"),origin:origin.clone(),referrer_policy:lumen_common::referrer::ReferrerPolicy::Origin,
            policies:Arc::new(lumen_common::csp::PolicySet::default()),import_map:None};
        let resources=PrefetchedModuleResources::default();let config=lumen_web::FetchConfig::default();
        let loader=make_browser_module_loader({let client=client.clone();move||Some(client.clone())},config.clone(),resources.clone(),Arc::new(Mutex::new(Vec::new())));
        let mut engine=lumen::Engine::new();
        engine.ctx().install_module_fetch_loader(Rc::new(loader));
        engine.ctx().install_module_api_base_for_host(Rc::new(move||format!("{origin}/document")));
        let queued=Rc::new(RefCell::new(Vec::new()));let queue=queued.clone();
        engine.ctx().install_async_module_import_handler(Rc::new(move|request|queue.borrow_mut().push(request)));
        let result=engine.eval(r#"
            globalThis.repeatedDone=false;globalThis.repeatedError='';
            (async()=>{
                const rejected=async specifier=>{try {await import(specifier);throw new Error('accepted wrong MIME');}catch(error){if(!(error instanceof TypeError))throw error;}};
                await rejected('./file');
                if((await import('./file',{with:{type:'text'}})).default!=='hello')throw new Error('text after failure');
                if((await import('./file#2',{with:{type:'text'}})).default!=='hello')throw new Error('fragment');
                await rejected('./file#2');
                const first=await import('./firsttext',{with:{type:'text'}});
                if(first.default!=='hello'||(await import('./firsttext')).default!=='world')throw new Error('different type');
                if(first!==(await import('./firsttext',{with:{type:'text'}})))throw new Error('text namespace identity');
                await rejected('./retry');
                if((await import('./retry')).default!=='world')throw new Error('retry after failed MIME');
                const js=await import('./firstjs');
                if(js.default!=='world'||(await import('./firstjs',{with:{type:'text'}})).default!=='hello')throw new Error('reverse type');
                if(js!==(await import('./firstjs')))throw new Error('JavaScript namespace identity');
                repeatedDone=true;
            })().catch(error=>{repeatedError=String(error);});
        "#,false).expect("authored repeated imports");
        assert!(matches!(result,lumen::Completion::Value(_)),"authored imports must start without throwing");
        let mut completed=0;
        for _ in 0..32 {
            let requests=std::mem::take(&mut *queued.borrow_mut());
            if requests.is_empty() {break;}
            for request in requests {
                let resolved=request.resolution.clone().unwrap_or_else(||client.resolve(&request.specifier,&request.referrer).unwrap());
                let mut fetch=BrowserModuleFetch::descendant(client.clone(),&request.referrer,request.script_context.as_deref());fetch.integrity=resolved.integrity;
                let (outcome,_)=prepare_browser_module_graph(&resolved.url,request.attribute_type.as_deref(),fetch,&config,&resources);
                match outcome {
                    Ok(())=>{let _=engine.ctx().complete_prepared_module_import_for_host(request.id);},
                    Err(error)=>engine.ctx().reject_prepared_module_import_for_host(request.id,&error).expect("real import rejection"),
                }
                completed+=1;
            }
            let _=engine.eval("void 0",false).expect("real Promise job checkpoint");
        }
        stopping.store(true,Ordering::Relaxed);
        let requests=server.join().expect("module server completion");
        assert_eq!(completed,12,"every authored dynamic import receives one real completion");
        assert!(matches!(engine.eval("repeatedDone && repeatedError === ''",false),Ok(lumen::Completion::Value(ref result)) if result=="true"),"authored imports must finish; requests={requests:?}");
        assert_eq!(requests.get("/retry"),Some(&2),"failed MIME admission must permit a later fetch");
        assert_eq!(requests.get("/firsttext"),Some(&2),"type isolates cache and successful text reuse makes no request");
        assert_eq!(requests.get("/firstjs"),Some(&2),"JavaScript reuse preserves its own namespace and response");
        assert_eq!(requests.get("/file"),Some(&4),"fragment and type both contribute to real module identity");
    }

    #[test]
    fn typescript_star_exports_are_followed_as_reexports() {
        use super::*;
        let src = r#"__exportStar(require("./convenience/constants.js"), exports);
tslib_1.__exportStar(require('./b'), exports);
__export(require("./c"));
const x = require("./not-a-reexport");"#;
        assert_eq!(
            reexport_requires(src),
            ["./convenience/constants.js", "./b", "./c"]
        );
    }

    #[test]
    fn network_request_resolution_preserves_query_and_drops_fragment() {
        assert_eq!(
            super::resolve_network_module_request(
                "../dep.js?variant=slow#identity",
                "http://web-platform.test:8000/a/b/main.js?root=1"
            )
            .as_deref(),
            Some("http://web-platform.test:8000/a/dep.js?variant=slow")
        );
        assert_eq!(
            super::resolve_network_module_request(
                "/dep.js?x=1",
                "https://web-platform.test:8443/a/main.js"
            )
            .as_deref(),
            Some("https://web-platform.test:8443/dep.js?x=1")
        );
        assert!(super::resolve_network_module_request(
            "https://other.test/dep.js",
            "https://web-platform.test/main.js"
        )
        .is_none());
        assert!(super::resolve_network_module_request("./dep.js", "file:///wpt/main.js").is_none());
    }

    #[test]
    fn prefetched_module_resources_are_bounded_and_remember_failures() {
        use super::{PrefetchedModuleResource, PrefetchedModuleResources};

        let resources = PrefetchedModuleResources::default();
        resources
            .insert(
                "http://example.test/a.js?x=1".into(),
                Ok(PrefetchedModuleResource {
                    final_url: "http://example.test/final.js?x=1".into(),
                    content_type: Some("text/javascript".into()),
                    bytes: b"export {};".to_vec().into(),
                    script_context: None,
                }),
            )
            .unwrap();
        resources
            .insert(
                "http://example.test/missing.js".into(),
                Err("HTTP 404".into()),
            )
            .unwrap();
        assert_eq!(
            resources
                .get("http://example.test/a.js?x=1")
                .unwrap()
                .unwrap()
                .final_url,
            "http://example.test/final.js?x=1"
        );
        assert_eq!(
            resources.get("http://example.test/missing.js").unwrap(),
            Err("HTTP 404".into())
        );
        let oversized = "x".repeat(super::PREFETCHED_MODULE_BYTES + 1);
        assert!(resources
            .insert(
                "http://example.test/large.js".into(),
                Ok(PrefetchedModuleResource {
                    final_url: "http://example.test/large.js".into(),
                    content_type: Some("text/javascript".into()),
                    bytes: oversized.into_bytes().into(),
                    script_context: None,
                }),
            )
            .is_err());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn canonical_esm_loader_consumes_prefetched_redirected_resource_without_network_fallback() {
        use super::{
            make_cached_loader_with_fetch_config_and_prefetched, BuiltinModules,
            PrefetchedModuleResource, PrefetchedModuleResources,
        };
        use std::collections::HashMap;

        let mut config = lumen_web::FetchConfig::default();
        config.set_require_routes(true);
        let cache = PrefetchedModuleResources::default();
        cache
            .insert(
                "http://example.test:8000/dep.js?variant=one".into(),
                Ok(PrefetchedModuleResource {
                    final_url: "http://example.test:8000/final/dep.js?variant=one".into(),
                    content_type: Some("text/javascript".into()),
                    bytes: b"export const answer = 42;".to_vec().into(),
                    script_context: None,
                }),
            )
            .unwrap();
        let (loader, _cache_handle) = make_cached_loader_with_fetch_config_and_prefetched(
            BuiltinModules(HashMap::new()),
            config,
            cache,
        );
        let (key, source) = loader(
            "./dep.js?variant=one#module-fragment",
            "http://example.test:8000/page.html",
            None,
        )
        .expect("the normal ESM resolver consumes the prefetched graph edge");
        assert_eq!(
            key,
            "http://example.test:8000/final/dep.js?variant=one#module-fragment"
        );
        assert_eq!(source, "export const answer = 42;");
    }

    #[test]
    fn malformed_jsx_dependency_reports_native_parse_error() {
        let file = std::env::temp_dir().join(format!("lumen-malformed-{}.jsx", std::process::id()));
        std::fs::write(&file, "export default <div>").unwrap();
        let (key, source) = super::load_as_module(&file, false).unwrap();
        std::fs::remove_file(&file).unwrap();
        assert_eq!(source, "export default <div>");
        let mut engine = lumen::Engine::new();
        let result = engine.eval_module(&source, &key, |_, _| None).unwrap();
        match result {
            lumen::Completion::Throw { name, message } => {
                assert_eq!(name, "SyntaxError");
                assert!(message.contains(&key), "{message}");
                assert!(message.contains(":1:21:"), "{message}");
                assert!(message.contains("unterminated JSX element"), "{message}");
            }
            lumen::Completion::Value(_) => panic!("malformed JSX evaluated"),
        }
    }

    #[test]
    fn source_cache_bounds_bytes_and_keeps_recent_sources() {
        use super::*;
        let mut cache = SourceCache::default();
        let a = ("a".to_owned(), None);
        let b = ("b".to_owned(), None);
        let c = ("c".to_owned(), None);
        let source = "x".repeat(SOURCE_CACHE_BYTES / 2 - 16);
        cache.insert(a.clone(), &source);
        cache.insert(b.clone(), &source);
        assert!(cache.get(&a).is_some());
        cache.insert(c.clone(), &"y".repeat(64));
        assert!(cache.get(&a).is_some());
        assert!(cache.get(&b).is_none());
        assert_eq!(cache.get(&c).as_deref(), Some("y".repeat(64).as_str()));
        assert!(cache.bytes <= SOURCE_CACHE_BYTES);
        cache.insert(a.clone(), &"z".repeat(SOURCE_CACHE_BYTES + 1));
        assert!(
            cache.get(&a).is_none(),
            "oversized replacement must not leave stale source"
        );
        cache.clear();
        assert_eq!(cache.bytes, 0);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn source_cache_bounds_empty_records_and_separates_attributes() {
        use super::*;
        let mut cache = SourceCache::default();
        for index in 0..SOURCE_CACHE_ENTRIES + 1 {
            cache.insert((index.to_string(), None), "");
        }
        assert_eq!(cache.entries.len(), SOURCE_CACHE_ENTRIES);
        assert!(cache.get(&("0".into(), None)).is_none());
        cache.insert(("file".into(), None), "plain");
        cache.insert(("file".into(), Some("json".into())), "json");
        assert_eq!(cache.get(&("file".into(), None)).as_deref(), Some("plain"));
        assert_eq!(
            cache.get(&("file".into(), Some("json".into()))).as_deref(),
            Some("json")
        );
    }

    #[test]
    fn source_cache_misses_reread_while_hits_preserve_resolution() {
        use super::*;
        let cache = LoaderCache::default();
        let calls = std::cell::Cell::new(0);
        let read = |_: &str, _: &str, _: Option<&str>| {
            calls.set(calls.get() + 1);
            Some(("canonical".to_owned(), calls.get().to_string()))
        };
        assert_eq!(
            cache
                .resolve("./a.mjs", "/root/main.mjs", None, read)
                .unwrap()
                .1,
            "1"
        );
        assert_eq!(
            cache
                .resolve("./a.mjs", "/root/main.mjs", None, read)
                .unwrap()
                .1,
            "1"
        );
        cache.forget_sources();
        assert_eq!(
            cache
                .resolve("./a.mjs", "/root/main.mjs", None, read)
                .unwrap()
                .1,
            "2"
        );
        assert_eq!(calls.get(), 2);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn configured_network_module_loader_uses_its_route_snapshot() {
        use super::*;
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("local module listener");
        let address = listener.local_addr().expect("listener address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("module request");
            let mut request = Vec::new();
            let mut chunk = [0; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).expect("read module request");
                assert_ne!(read, 0, "module connection closed before request headers");
                request.extend_from_slice(&chunk[..read]);
            }
            let request = String::from_utf8_lossy(&request);
            assert!(
                request.starts_with("GET /dependency.mjs?flavor=blue HTTP/1.1\r\n"),
                "the fragment must stay off the wire while the query is preserved: {request}"
            );
            assert!(
                request
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("Host: 127.0.0.1:1")),
                "logical Host header was not preserved: {request}"
            );

            let body = b"export const answer = 42;";
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(header.as_bytes())
                .expect("write response headers");
            stream.write_all(body).expect("write module body");
        });

        // The logical module URL uses port 1, while the captured route sends the socket to the
        // ephemeral listener. Mutating the caller's config after loader creation must not retarget
        // the loader; clones are immutable snapshots and preserve the URL's Host header.
        let mut config = lumen_web::FetchConfig::default();
        config.set_require_routes(true);
        config
            .set_route("127.0.0.1", 1, address)
            .expect("configure module route");
        let loader = make_loader_with_fetch_config(BuiltinModules(HashMap::new()), config.clone());
        config
            .set_route("127.0.0.1", 1, "127.0.0.1:2".parse().unwrap())
            .expect("mutate caller config");

        let referrer = "http://127.0.0.1:1/main.mjs";
        let (key, source) = loader(
            "./dependency.mjs?flavor=blue#module-fragment",
            referrer,
            None,
        )
        .expect("configured network dependency resolves");
        assert_eq!(
            key,
            "http://127.0.0.1:1/dependency.mjs?flavor=blue#module-fragment"
        );
        assert_eq!(source, "export const answer = 42;");
        server.join().expect("module fixture server exits");
    }

    #[test]
    fn export_patterns_preserve_exact_blocks_specificity_and_boundaries() {
        let manifest = r#"{"exports":{"./*":{"import":"./dist/esm/*","require":"./dist/cjs/*"},"./feature/*":"./feature/*","./feature/*.js":"./specific/*.mjs","./blocked.js":null,"./internal/*":null}}"#;
        assert_eq!(
            super::exports_subpath(manifest, "types.js").as_deref(),
            Some("./dist/esm/types.js")
        );
        assert_eq!(
            super::exports_subpath(manifest, "feature/a.js").as_deref(),
            Some("./specific/a.mjs")
        );
        for subpath in [
            "blocked.js",
            "internal/private.js",
            "../secret.js",
            "%2e%2e/secret.js",
            "node_modules/secret.js",
            "a%2fb.js",
        ] {
            assert_eq!(super::exports_subpath(manifest, subpath), None, "{subpath}");
        }
    }

    #[test]
    fn explicit_root_exports_do_not_fall_back_to_legacy_entries() {
        for exports in ["null", r#"{".":null}"#, r#"{".":{"require":"./cjs.js"}}"#] {
            let manifest =
                format!(r#"{{"exports":{exports},"module":"./legacy.mjs","main":"./legacy.js"}}"#);
            assert_eq!(super::pkg_entry(&manifest), None);
        }
        assert_eq!(
            super::pkg_entry(r#"{"main":"./legacy.js"}"#).as_deref(),
            Some("./legacy.js")
        );
    }

    #[test]
    fn nested_unmatched_condition_continues_but_null_blocks() {
        let manifest =
            r#"{"exports":{"node":{"require":"./wrong.cjs"},"default":"./fallback.mjs"}}"#;
        assert_eq!(
            super::pkg_entry(manifest).as_deref(),
            Some("./fallback.mjs")
        );
        let blocked =
            r#"{"exports":{"node":{"import":null},"default":"./wrong.mjs"},"main":"./legacy.js"}"#;
        assert_eq!(super::pkg_entry(blocked), None);
        let ordered = r#"{"exports":{"default":"./first.mjs","node":{"import":"./second.mjs"}}}"#;
        assert_eq!(super::pkg_entry(ordered).as_deref(), Some("./first.mjs"));
        assert_eq!(
            super::pkg_entry(r#"{"exports":["./array.mjs"],"main":"./legacy.js"}"#),
            Some("./array.mjs".to_string())
        );
    }

    #[test]
    fn exports_arrays_use_ordered_fallback_for_acorn_and_nulls() {
        let acorn = r#"{"exports":{".":[{"import":"./dist/acorn.mjs","require":"./dist/acorn.js","default":"./dist/acorn.js"},"./dist/acorn.js"]}}"#;
        assert_eq!(super::pkg_entry(acorn).as_deref(), Some("./dist/acorn.mjs"));
        let fallback = r#"{"exports":[{"require":"./wrong.cjs"},null,false,"../outside.js",[null,"./first.mjs"],"./second.mjs"]}"#;
        assert_eq!(super::pkg_entry(fallback).as_deref(), Some("./first.mjs"));
        let blocked_branch = r#"{"exports":{"node":[null,{"require":"./unused.cjs"}],"default":"./wrong.mjs"},"main":"./legacy.js"}"#;
        assert_eq!(super::pkg_entry(blocked_branch), None);
        let unmatched =
            r#"{"exports":{"node":[{"require":"./unused.cjs"}],"default":"./fallback.mjs"}}"#;
        assert_eq!(
            super::pkg_entry(unmatched).as_deref(),
            Some("./fallback.mjs")
        );
    }

    #[test]
    fn exports_array_terminal_status_matches_node() {
        use super::{package_esm_target, PackageEsmTarget};
        for (source, expected) in [
            ("[]", PackageEsmTarget::Blocked),
            ("[false,null]", PackageEsmTarget::Blocked),
            ("[null,false]", PackageEsmTarget::Unsupported),
            (r#"[{"require":"./unused.cjs"}]"#, PackageEsmTarget::NoMatch),
        ] {
            let json = crate::tsconfig::parse_jsonc(source).unwrap();
            assert_eq!(package_esm_target(&json), expected, "{source}");
        }
    }

    #[test]
    fn package_root_export_ignores_earlier_subpaths() {
        let manifest = r#"{"repository":{"type":"git"},"type":"module","exports":{
            "./compile":{"import":"./compile.mjs"},
            ".":{"import":"./index.mjs"},
            "./blocked":{"node":null,"default":"./wrong.mjs"}
        }}"#;
        assert_eq!(super::pkg_entry(manifest).as_deref(), Some("./index.mjs"));
        assert_eq!(
            super::exports_subpath(manifest, "compile").as_deref(),
            Some("./compile.mjs")
        );
        assert_eq!(super::exports_subpath(manifest, "blocked"), None);
    }
    use super::*;

    #[test]
    fn package_type_reads_only_the_root_field() {
        assert_eq!(
            crate::package_type_from_json(r#"{"repository":{"type":"git"},"type":"module"}"#)
                .as_deref(),
            Some("module")
        );
        assert_eq!(
            crate::package_type_from_json(r#"{"repository":{"type":"module"}}"#),
            None
        );
        assert_eq!(crate::package_type_from_json(r#"{"type":null}"#), None);
    }

    #[test]
    fn pkg_entry_prefers_exports_import_condition() {
        // hono-shaped: `main` is CJS, but the ESM entry lives under exports' `import` condition.
        let pkg = r#"{
            "main": "dist/cjs/index.js",
            "type": "module",
            "module": "dist/index.js",
            "exports": { ".": {
                "types": "./dist/types/index.d.ts",
                "import": "./dist/index.js",
                "require": "./dist/cjs/index.js"
            } }
        }"#;
        assert_eq!(pkg_entry(pkg).as_deref(), Some("./dist/index.js"));
    }

    #[test]
    fn pkg_entry_exports_string_then_module_then_main() {
        assert_eq!(
            pkg_entry(r#"{ "exports": "./e.js", "main": "./m.js" }"#).as_deref(),
            Some("./e.js")
        );
        assert_eq!(
            pkg_entry(r#"{ "module": "./mod.js", "main": "./m.js" }"#).as_deref(),
            Some("./mod.js")
        );
        assert_eq!(
            pkg_entry(r#"{ "main": "./m.js" }"#).as_deref(),
            Some("./m.js")
        );
    }

    #[test]
    fn package_subpath_splits_plain_and_scoped() {
        assert_eq!(package_subpath("hono"), None);
        assert_eq!(package_subpath("hono/logger").as_deref(), Some("logger"));
        assert_eq!(
            package_subpath("hono/dist/x.js").as_deref(),
            Some("dist/x.js")
        );
        assert_eq!(package_subpath("@scope/pkg"), None);
        assert_eq!(package_subpath("@scope/pkg/sub").as_deref(), Some("sub"));
    }

    #[test]
    fn exports_subpath_reads_condition_and_string_forms() {
        // hono-shaped middleware subpath: an object with an `import` condition.
        let pkg = r#"{ "exports": {
            ".": { "import": "./dist/index.js" },
            "./logger": {
                "types": "./dist/types/middleware/logger/index.d.ts",
                "import": "./dist/middleware/logger/index.js",
                "require": "./dist/cjs/middleware/logger/index.js"
            }
        } }"#;
        assert_eq!(
            exports_subpath(pkg, "logger").as_deref(),
            Some("./dist/middleware/logger/index.js")
        );
        assert_eq!(exports_subpath(pkg, "cors"), None);
        // Bare-string subpath form.
        assert_eq!(
            exports_subpath(r#"{ "exports": { "./x": "./lib/x.js" } }"#, "x").as_deref(),
            Some("./lib/x.js")
        );
    }

    #[test]
    fn package_dir_handles_plain_and_scoped_subpaths() {
        let root = Path::new("/app");
        assert_eq!(
            package_dir(root, "hono"),
            PathBuf::from("/app/node_modules/hono")
        );
        assert_eq!(
            package_dir(root, "hono/dist/index.js"),
            PathBuf::from("/app/node_modules/hono")
        );
        assert_eq!(
            package_dir(root, "@scope/pkg/sub.js"),
            PathBuf::from("/app/node_modules/@scope/pkg")
        );
    }

    #[test]
    fn normalize_dotdot() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
    }

    #[test]
    fn js_string_escapes() {
        assert_eq!(crate::js_source_string(r#"a"b\c"#), r#""a\"b\\c""#);
    }
    #[test]
    fn subpath_exports_own_legacy_files_and_refuse_blocked_disk_fallback() {
        let dir = std::env::temp_dir().join(format!(
            "lumen-exports-shadow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let pkg = dir.join("node_modules/shadow");
        std::fs::create_dir_all(pkg.join("dist")).unwrap();
        std::fs::write(pkg.join("package.json"),r#"{"type":"module","exports":{"./stream":{"import":"./dist/stream.js","require":"./dist/stream.cjs"},"./cjs":"./dist/stream.cjs","./blocked":null}}"#).unwrap();
        for path in [
            "stream.js",
            "blocked.js",
            "dist/stream.js",
            "dist/stream.cjs",
        ] {
            std::fs::write(pkg.join(path), "").unwrap();
        }
        let (path, esm) =
            super::resolve_node_modules("shadow/stream", &dir).expect("mapped module");
        assert_eq!(path, pkg.join("dist/stream.js"));
        assert!(esm);
        let (path, esm) = super::resolve_node_modules("shadow/cjs", &dir).expect("mapped cjs");
        assert_eq!(path, pkg.join("dist/stream.cjs"));
        assert!(!esm);
        assert!(super::resolve_node_modules("shadow/blocked", &dir).is_none());
        assert!(super::resolve_node_modules("shadow/stream.js", &dir).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
