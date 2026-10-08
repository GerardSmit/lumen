//! ES module loading, linking, and evaluation.
//!
//! lumen links modules in two passes, mirroring the spec's `Link` (Instantiate) and `Evaluate`
//! phases:
//!
//!   * **Instantiate** parses every reachable module, builds its export tables, creates its scope
//!     and (frozen) namespace object, hoists its top-level bindings, and wires each import to a
//!     *live* binding cell in the exporting module's scope. No user code runs.
//!   * **Evaluate** runs module bodies depth-first (dependencies before dependents, each once),
//!     so a live import observes the exporter's latest value even across circular dependencies.
//!
//! Specifier resolution + source fetching is delegated to a host loader (`Interp::module_loader`)
//! so the engine stays filesystem-agnostic.

use crate::ast::*;
use crate::builtins::make_bound_len;
use crate::interpreter::{new_scope, Abrupt, Binding, Env, Interp};
use crate::value::Gc;
use crate::value::{Object, Property, Value};
use std::collections::HashMap;
use std::rc::Rc;

/// The module map belongs to an environment settings object, not to the VM.
/// Graph operations use the explicitly entered realm; delayed entry points must
/// enter their captured settings before accessing this map.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct ModuleKey { url: Rc<str>, module_type: Option<Rc<str>> }
impl ModuleKey {
    pub(crate) fn module_type(&self) -> Option<&str> { self.module_type.as_deref() }
    fn typed(url: String, module_type: Option<&str>) -> Self {
        Self { url: url.into(), module_type: module_type.map(Rc::from) }
    }
}
impl From<String> for ModuleKey { fn from(url: String) -> Self { Self::typed(url, None) } }
impl From<&str> for ModuleKey { fn from(url: &str) -> Self { Self::from(url.to_owned()) } }
impl std::ops::Deref for ModuleKey { type Target = str; fn deref(&self)->&str { &self.url } }
impl std::fmt::Display for ModuleKey { fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result { std::fmt::Display::fmt(self.url.as_ref(),f) } }
impl PartialEq<str> for ModuleKey { fn eq(&self,other:&str)->bool { self.module_type.is_none() && self.url.as_ref()==other } }
impl PartialEq<&str> for ModuleKey { fn eq(&self,other:&&str)->bool { self.module_type.is_none() && self.url.as_ref()==*other } }
pub(crate) trait ModuleLookup {
    fn url(&self) -> &str;
    fn module_type(&self) -> Option<&str> { None }
}
impl ModuleLookup for str { fn url(&self)->&str { self } }
impl ModuleLookup for String { fn url(&self)->&str { self } }
impl ModuleLookup for Rc<str> { fn url(&self)->&str { self } }
impl<K:ModuleLookup+?Sized> ModuleLookup for &K {
    fn url(&self)->&str { (*self).url() }
    fn module_type(&self)->Option<&str> { (*self).module_type() }
}
impl ModuleLookup for ModuleKey {
    fn url(&self)->&str { &self.url }
    fn module_type(&self)->Option<&str> { ModuleKey::module_type(self) }
}
pub(crate) struct ModuleBucket<T> {
    javascript: Option<(ModuleKey,T)>,
    typed: HashMap<String,(ModuleKey,T)>,
}
impl<T> Default for ModuleBucket<T> {
    fn default()->Self { Self { javascript:None,typed:HashMap::new() } }
}
impl<T> ModuleBucket<T> {
    fn entries(&self)->impl Iterator<Item=&(ModuleKey,T)> { self.javascript.iter().chain(self.typed.values()) }
    fn get(&self,kind:Option<&str>)->Option<&T> {
        match kind { None=>self.javascript.as_ref(),Some(kind)=>self.typed.get(kind) }.map(|(_,value)|value)
    }
    fn get_mut(&mut self,kind:Option<&str>)->Option<&mut T> {
        match kind { None=>self.javascript.as_mut(),Some(kind)=>self.typed.get_mut(kind) }.map(|(_,value)|value)
    }
}
type ModuleEntries<T> = HashMap<Rc<str>, ModuleBucket<T>>;
pub(crate) struct SettingsModuleMap<T> { settings: usize, maps: HashMap<usize, ModuleEntries<T>> }

impl<T> Default for SettingsModuleMap<T> {
    fn default() -> Self {
        Self { settings: 0, maps: HashMap::new() }
    }
}

impl<T> SettingsModuleMap<T> {
    pub(crate) fn select(&mut self, settings: usize) { self.settings = settings; }
    pub(crate) fn for_settings(&self, settings: usize) -> impl Iterator<Item = &T> {
        self.maps.get(&settings).into_iter().flat_map(|urls|urls.values()).flat_map(ModuleBucket::entries).map(|(_,value)|value)
    }
    pub(crate) fn all_values(&self) -> impl Iterator<Item = &T> {
        self.maps.values().flat_map(|urls|urls.values()).flat_map(ModuleBucket::entries).map(|(_,value)|value)
    }
    pub(crate) fn remove_settings(&mut self, settings: usize) -> Option<ModuleEntries<T>> {
        self.maps.remove(&settings)
    }
    pub(crate) fn get<K: ModuleLookup + ?Sized>(&self, key:&K)->Option<&T> {
        self.maps.get(&self.settings)?.get(key.url())?.get(key.module_type())
    }
    pub(crate) fn get_mut<K: ModuleLookup + ?Sized>(&mut self,key:&K)->Option<&mut T> {
        self.maps.get_mut(&self.settings)?.get_mut(key.url())?.get_mut(key.module_type())
    }
    pub(crate) fn contains_key<K: ModuleLookup + ?Sized>(&self,key:&K)->bool { self.get(key).is_some() }
    pub(crate) fn insert(&mut self,key:impl Into<ModuleKey>,value:T)->Option<T> {
        let key=key.into();
        let bucket = self.maps.entry(self.settings).or_default().entry(key.url.clone()).or_default();
        match key.module_type.clone() {
            None=>bucket.javascript.replace((key,value)),
            Some(kind)=>bucket.typed.insert(kind.to_string(),(key,value)),
        }.map(|(_,value)|value)
    }
    pub(crate) fn remove<K: ModuleLookup + ?Sized>(&mut self,key:&K)->Option<T> {
        let bucket = self.maps.get_mut(&self.settings)?.get_mut(key.url())?;
        match key.module_type() { None=>bucket.javascript.take(),Some(kind)=>bucket.typed.remove(kind) }.map(|(_,value)|value)
    }
    pub(crate) fn values(&self)->impl Iterator<Item=&T> { self.for_settings(self.settings) }
    #[cfg(feature = "compiler")]
    pub(crate) fn iter(&self)->impl Iterator<Item=(&ModuleKey,&T)> {
        self.maps.get(&self.settings).into_iter().flat_map(|urls|urls.values()).flat_map(ModuleBucket::entries).map(|(key,value)|(key,value))
    }
    pub(crate) fn len(&self)->usize { self.values().count() }
    #[cfg(test)]
    pub(crate) fn clear(&mut self) { self.maps.remove(&self.settings); }
}
impl<T,K:ModuleLookup+?Sized> std::ops::Index<&K> for SettingsModuleMap<T> {
    type Output=T;
    fn index(&self,key:&K)->&T { self.get(key).expect("registered module") }
}

#[derive(Clone)]
pub(crate) struct DeferredModuleKey {
    pub(crate) settings: usize,
    pub(crate) key: ModuleKey,
}
pub(crate) type ModuleSyntheticFactory = Rc<dyn Fn(&mut Interp,&str)->Result<Value,Value>>;

/// How a module-namespace property reads its current value.
#[derive(Clone)]
pub enum NsBinding {
    /// A live binding: read `local` from the exporting module's scope each time.
    Live(Env, String),
    /// A stable value (a star-as namespace re-export).
    Static(Value),
}

/// A parsed + linked module: its body, scope, namespace object, resolved export tables, and
/// evaluation status. Keyed by canonical specifier in `Interp::module_recs`.
pub(crate) struct ModuleRec {
    body: Rc<Vec<Stmt>>,
    /// Evaluation metadata survives retirement of the one-shot initialization AST.
    has_tla: bool,
    retired_eager_named: Option<Vec<ModuleKey>>,
    retired_eager_order: Option<Vec<ModuleKey>>,
    /// The source the body was parsed from, for its stack-trace frame (see `stack_trace`).
    src: Option<Rc<str>>,
    env: Env,
    pub ns: Value,
    meta: Value,
    /// Dependency keys in source order (drives depth-first evaluation).
    dep_keys: Vec<ModuleKey>,
    /// Specifier → resolved canonical key, for this module's own import/export-from clauses.
    resolved: HashMap<ModuleKey, ModuleKey>,
    /// `export name → local name` for names declared/defined in this module.
    local_exports: HashMap<String, String>,
    /// Local names that are themselves imports, so a re-export resolves through to the origin
    /// binding (spec: `import * as x; export {x}` resolves to the imported module's namespace).
    imports: HashMap<String, ImportOrigin>,
    /// `export name → (dependency key, imported name)` for `export { x } from 'dep'`.
    indirect: HashMap<String, (ModuleKey, String)>,
    /// `export name → dependency key` for `export * as name from 'dep'`.
    star_as: HashMap<String, ModuleKey>,
    /// Dependency keys of `export * from 'dep'` clauses.
    stars: Vec<ModuleKey>,
    linked: bool,
    evaluated: bool,
    evaluating: bool,
    eval_error: Option<Value>,
    /// Dependency keys imported *only* via `import defer` — skipped during this module's
    /// evaluation phase (they evaluate on first namespace access instead).
    deferred_deps: Vec<ModuleKey>,
    /// For a dynamically-imported module evaluating in a coroutine (top-level await): the promise
    /// that settles when the body finishes.
    top_promise: Option<Value>,
    /// This batch has visited the module (spec [[Status]] >= evaluating).
    started: bool,
    /// Count of not-yet-finished async-evaluating dependencies ([[PendingAsyncDependencies]]).
    pending_async: usize,
    /// Position in the async execution queue ([[AsyncEvaluationOrder]]).
    async_order: Option<u64>,
    /// Importers waiting on this module ([[AsyncParentModules]]).
    async_parents: Vec<ModuleKey>,
    /// Tarjan bookkeeping for the evaluation DFS ([[DFSIndex]] / [[DFSAncestorIndex]]).
    dfs_index: Option<usize>,
    dfs_anc: usize,
    on_stack: bool,
    /// The root of this module's strongly-connected component ([[CycleRoot]]).
    cycle_root: Option<ModuleKey>,
}

impl ModuleRec {
    pub(crate) fn visit_values(&self, mut visit: impl FnMut(&Value)) {
        visit(&self.ns);
        visit(&self.meta);
        if let Some(value) = &self.eval_error { visit(value); }
        if let Some(value) = &self.top_promise { visit(value); }
    }
    pub(crate) fn environment(&self) -> &Env { &self.env }
}

#[cfg(feature = "compiler")]
impl ModuleRec {
    pub(crate) fn snapshot_ready(&self) -> bool {
        self.evaluated && !self.evaluating && self.eval_error.is_none() && self.pending_async == 0
    }
    pub(crate) fn snapshot_env(&self) -> Env {
        self.env.clone()
    }
}

/// The origin of a local name that is an import binding (so re-exports resolve to the source).
enum ImportOrigin {
    /// `import * as x from 'dep'` — resolves to `dep`'s namespace object.
    Namespace(ModuleKey),
    /// `import defer * as x from 'dep'` — resolves to `dep`'s DEFERRED namespace object.
    DeferNamespace(ModuleKey),
    /// `import { y as x } from 'dep'` / `import x from 'dep'` — resolves to `dep`'s export `y`.
    Named(ModuleKey, String),
}

/// The result of resolving an export name (spec ResolveExport).
enum Resolution {
    /// A concrete binding: `local` in the given module scope.
    Local(Env, String),
    /// A re-exported `import defer * as ns` binding: the dep's DEFERRED namespace.
    DeferNs(ModuleKey),
    /// A namespace object (a star-as re-export target).
    Ns(Value),
    /// Two star re-exports provide the name with different bindings.
    Ambiguous,
    /// The name is not exported.
    NotFound,
}

/// What a runtime's module hook decided for one `import` (see `Interp::esm_hook`).
enum EsmHook {
    Default,
    Redirect(String),
    Module(String, String),
}

impl Interp {
    pub fn install_module_api_base_for_host(&mut self,base:Rc<dyn Fn()->String>) {
        self.module_api_bases.insert(Gc::as_ptr(&self.global) as usize,base);
    }
    fn module_api_base_for_host(&self)->String {
        self.module_api_bases.get(&(Gc::as_ptr(&self.global) as usize))
            .map_or_else(||self.import_base.clone(),|base|base())
    }
    /// Import maps belong to the Window's settings, including retained module functions.
    pub fn ensure_import_map_for_host(&mut self) -> lumen_common::import_maps::SharedImportMap {
        self.import_maps.entry(Gc::as_ptr(&self.global) as usize)
            .or_insert_with(lumen_common::import_maps::ImportMapState::shared).clone()
    }
    pub fn import_map_for_host(&self) -> Option<lumen_common::import_maps::SharedImportMap> {
        self.import_maps.get(&(Gc::as_ptr(&self.global) as usize)).cloned()
    }
    pub fn resolve_module_specifier_for_host(&mut self, specifier: &str, base: &str)
        -> Result<Option<lumen_common::import_maps::Resolution>, Abrupt> {
        let Some(map)=self.import_map_for_host() else { return Ok(None); };
        let result=map.lock().map_err(|_| "import-map state is unavailable".to_owned())
            .and_then(|mut map|map.resolve(specifier,base).map_err(|error|error.to_string()));
        result.map(Some).map_err(|error|self.throw("TypeError",error))
    }
    /// Install a plain host loader on the actually entered settings object.
    pub fn install_module_fetch_loader(&mut self,
        loader: Rc<dyn Fn(crate::ModuleFetchRequest) -> Option<crate::ModuleFetchResult>>) {
        self.module_fetch_loaders.insert(Gc::as_ptr(&self.global) as usize, loader);
    }
    /// Capture the entered settings object's host fetch service. Worklets can
    /// reuse the embedder's policy, routing and response decoding in an isolated
    /// realm without inheriting Window globals or the Window's module map.
    pub fn module_fetch_loader_for_host(&self)
        -> Option<Rc<dyn Fn(crate::ModuleFetchRequest) -> Option<crate::ModuleFetchResult>>> {
        self.module_fetch_loaders.get(&(Gc::as_ptr(&self.global) as usize)).cloned()
    }
    pub fn install_module_synthetic_factory(&mut self,kind:&str,
        factory:Rc<dyn Fn(&mut Interp,&str)->Result<Value,Value>>) {
        self.module_synthetic_factories.entry(Gc::as_ptr(&self.global) as usize).or_default().insert(kind.into(),factory);
    }
    #[cfg(feature = "embed")]
    pub fn install_async_module_import_handler(&mut self, handler: Rc<dyn Fn(crate::AsyncModuleImportRequest)>) {
        self.async_module_import_handlers.insert(Gc::as_ptr(&self.global) as usize, handler);
    }
    #[cfg(feature = "embed")]
    pub fn cancel_async_module_imports_for_realm(&mut self, realm: &crate::embed::RealmHandle) {
        let key = realm.key();
        self.pending_async_module_imports.retain(|_, (request, _, _)|request.settings_key != key);
        self.async_module_import_handlers.remove(&key);
        self.import_maps.remove(&key);
        self.module_api_bases.remove(&key);
        self.module_synthetic_factories.remove(&key);
        // Retained author functions may still name this retired settings
        // object. Keep a root-free rejecting host registration so they cannot
        // fall through to the process's unrelated legacy Node loader.
        self.module_fetch_loaders.insert(key,Rc::new(|_|None));
    }
    #[cfg(feature = "embed")]
    pub fn complete_prepared_module_import_for_host(&mut self, id: u64) -> Result<crate::ModuleEvaluationHandle, String> {
        let (namespace, promise) = self.complete_async_module_import(id, |_,_,_|None)?;
        Ok(crate::ModuleEvaluationHandle { namespace, promise })
    }
    #[cfg(feature = "embed")]
    pub fn reject_prepared_module_import_for_host(&mut self, id: u64, reason: &str) -> Result<(), String> {
        self.reject_async_module_import(id, reason)
    }
    #[cfg(feature = "embed")]
    pub fn has_pending_module_import_for_host(&self, id: u64) -> bool {
        self.pending_async_module_imports.contains_key(&id)
    }
    #[cfg(feature = "embed")]
    pub fn run_prepared_module_for_host(&mut self, source: &str, record_key: &str, module_url: &str,
        resolution_url: &str, context: Option<Rc<crate::ClassicScriptContext>>)
        -> Result<crate::ModuleEvaluationHandle, crate::ParseError> {
        if crate::native_ops::dynamic_code_disabled() {
            return Err(crate::ParseError { message: "dynamic code is unavailable in native execution".into(), line: 0, at_eof: false });
        }
        let previous = std::mem::replace(&mut self.classic_script_context, context);
        let result = self.start_module_with_base(record_key, source, module_url, resolution_url, true);
        self.classic_script_context = previous;
        let (namespace, promise) = match result {
            Ok(pair) => pair,
            Err(error) => {
                let promise = self.new_promise();
                self.observe_promise_for_host(&promise);
                self.reject_promise(&promise, crate::interpreter::abrupt_value(error));
                (Value::Undefined, promise)
            }
        };
        Ok(crate::ModuleEvaluationHandle { promise, namespace })
    }
    /// Load, link, and evaluate the module identified by canonical `key` (with initial `src`),
    /// returning its namespace object.
    pub(crate) fn load_module(&mut self, key: &str, src: &str) -> Result<Value, Abrupt> {
        let (namespace, top) = self.start_module(key, src, key, false)?;
        self.run_agent_event_loop();
        if let Some((crate::eval::promise_fast::REJECTED, reason)) =
            crate::eval::promise_fast::promise_state(&top)
        {
            return Err(Abrupt::Throw(reason));
        }
        Ok(namespace)
    }

    /// Start loading and evaluating a module graph, returning the root namespace and its real
    /// evaluation promise without running the agent event loop. `record_key` identifies the
    /// module in the engine's module map; `module_url` is the URL used for import resolution and
    /// `import.meta.url`. They differ for inline HTML module scripts, which have distinct module
    /// records while sharing their document URL.
    pub(crate) fn start_module(
        &mut self,
        record_key: &str,
        src: &str,
        module_url: &str,
        observe_for_host: bool,
    ) -> Result<(Value, Value), Abrupt> {
        self.start_module_with_base(record_key, src, module_url, module_url, observe_for_host)
    }

    /// Start a module whose import-resolution base differs from its observable `import.meta.url`.
    /// HTML inline modules use the document base URL for static imports while exposing the
    /// document URL as `import.meta.url`.
    pub(crate) fn start_module_with_base(
        &mut self,
        record_key: &str,
        src: &str,
        module_url: &str,
        resolution_url: &str,
        observe_for_host: bool,
    ) -> Result<(Value, Value), Abrupt> {
        let identity = ModuleKey::from(record_key);
        let record_key = &identity;
        // Phase 1: parse the whole graph (so every module's export tables exist before any linking).
        // Phase 2: link (hoist bindings, wire live imports, build namespaces) depth-first. A graph
        // that fails to parse or link registers nothing, so importing it again fails again.
        self.parse_and_link_at_with_base(
            record_key,
            Some(src.to_string()),
            module_url,
            resolution_url,
        )?;
        // Parsed module failures retain their original exception. Incomplete
        // linked graphs are pruned before another attempt.
        // Phase 3: evaluate module bodies depth-first. A graph containing top-level await
        // evaluates through the async machinery (awaits interleave with the job queue, an async
        // module doesn't block siblings, ancestors run in [[AsyncEvaluationOrder]]); a fully
        // synchronous graph runs directly.
        // Pre-create every graph member's evaluation promise, so a dynamic import of a
        // not-yet-executed batch member waits for the batch instead of executing it early.
        let mut graph = Vec::new();
        self.collect_eager_graph(record_key, &mut graph);
        for m in &graph {
            if !self.module_recs[m].evaluated && self.module_recs[m].top_promise.is_none() {
                let p = self.new_promise();
                self.module_recs.get_mut(m).unwrap().top_promise = Some(p);
            }
        }
        let top = self.module_recs[record_key].top_promise.clone().unwrap();
        if observe_for_host {
            self.observe_promise_for_host(&top);
        }
        let mut stack = Vec::new();
        self.inner_module_evaluation_async(record_key, &mut stack, &mut 0);
        Ok((self.module_recs[record_key].ns.clone(), top))
    }

    /// Every module in `key`'s dependency graph (including itself), depth-first.
    fn collect_graph(&self, key: &ModuleKey, out: &mut Vec<ModuleKey>) {
        if out.iter().any(|k| k == key) {
            return;
        }
        out.push(key.clone());
        if let Some(rec) = self.module_recs.get(key) {
            for d in rec.dep_keys.clone() {
                self.collect_graph(&d, out);
            }
        }
    }

    /// Like collect_graph, but follows only eager (non-deferred) requests — exactly the modules a
    /// batch Evaluate() will execute, and therefore the ones whose evaluation promises may be
    /// pre-created. A deferred-only member must NOT get an orphan pending promise, or a later
    /// dynamic import of it would wait forever instead of evaluating it.
    fn collect_eager_graph(&self, key: &ModuleKey, out: &mut Vec<ModuleKey>) {
        if out.iter().any(|k| k == key) {
            return;
        }
        out.push(key.clone());
        if let Some(rec) = self.module_recs.get(key) {
            let deferred = rec.deferred_deps.clone();
            let eager_named = self.eager_named_deps(key);
            for d in rec.dep_keys.clone() {
                // A dep imported both eagerly and with defer still evaluates with the batch.
                if deferred.contains(&d) && !eager_named.contains(&d) {
                    continue;
                }
                self.collect_eager_graph(&d, out);
            }
        }
    }

    /// Dependencies named by at least one non-defer import/export-from clause.
    fn eager_named_deps(&self, key: &ModuleKey) -> Vec<ModuleKey> {
        let rec = match self.module_recs.get(key) {
            Some(r) => r,
            None => return Vec::new(),
        };
        if let Some(eager) = &rec.retired_eager_named {
            return eager.clone();
        }
        let mut eager: Vec<ModuleKey> = Vec::new();
        for stmt in rec.body.iter() {
            let (spec, defers) = match stmt {
                Stmt::Import(decl) => (
                    dep_map_key(&decl.source, decl.attr_type.as_deref()),
                    !decl.specs.is_empty()
                        && decl
                            .specs
                            .iter()
                            .all(|s| matches!(s, crate::ast::ImportSpec::DeferNamespace(_))),
                ),
                Stmt::ExportNamed {
                    source: Some(src), ..
                }
                | Stmt::ExportAll { source: src, .. } => (ModuleKey::from(src.as_ref()), false),
                _ => continue,
            };
            if defers {
                continue;
            }
            if let Some(k) = rec.resolved.get(&spec) {
                if !eager.iter().any(|x| x == k) {
                    eager.push(k.clone());
                }
            }
        }
        eager
    }

    // --- Parse phase --------------------------------------------------------------------------

    /// Parse `key` and, transitively, every dependency — registering each module's environment,
    /// namespace object, and export tables. No user code runs and no linking happens yet, so a later
    /// ResolveExport can see the whole graph. Idempotent per key.
    #[cfg(test)]
    fn parse_and_register(&mut self, key: &str, src: Option<String>) -> Result<(), Abrupt> {
        let mut added = Vec::new();
        let result = self.parse_and_register_graph(&ModuleKey::from(key), key, key, src, &mut added);
        let result = result.and_then(|()| match self.module_recs.get(key).and_then(|record|record.eval_error.clone()) {
            Some(error)=>Err(Abrupt::Throw(error)),None=>Ok(()),
        });
        if result.is_err() {
            self.discard_modules(added);
        }
        result
    }

    /// Parse and link as one unit. Cache actual parse errors, but discard incomplete
    /// graph records so no importer remains linked to bindings that were never wired.
    fn parse_and_link_at(
        &mut self,
        key: &ModuleKey,
        src: Option<String>,
        module_url: &str,
    ) -> Result<(), Abrupt> {
        self.parse_and_link_at_with_base(key, src, module_url, module_url)
    }

    fn parse_and_link_at_with_base(
        &mut self,
        key: &ModuleKey,
        src: Option<String>,
        module_url: &str,
        resolution_url: &str,
    ) -> Result<(), Abrupt> {
        let mut added = Vec::new();
        let result = self
            .parse_and_register_graph(key, module_url, resolution_url, src, &mut added)
            .and_then(|()| self.link_module(key));
        if result.is_err() {
            self.discard_modules(added);
        }
        result
    }

    /// A record is registered before its dependencies are parsed (so cycles resolve), so a
    /// failure deeper in the graph would otherwise leave records naming unregistered
    /// dependencies, and a later import of them would link against missing keys.
    fn discard_modules(&mut self, keys: Vec<ModuleKey>) {
        for k in keys {
            if self.module_recs.get(&k).is_some_and(|record|record.eval_error.is_some()) { continue; }
            self.module_recs.remove(&k);
            self.modules.remove(&k);
        }
    }

    fn parse_and_register_graph(
        &mut self,
        key: &ModuleKey,
        module_url: &str,
        resolution_url: &str,
        src: Option<String>,
        added: &mut Vec<ModuleKey>,
    ) -> Result<(), Abrupt> {
        if self.module_recs.contains_key(key) {
            return Ok(());
        }
        let mut src = match src {
            Some(s) => s,
            None => return Err(self.throw("TypeError", format!("module not found: {key}"))),
        };
        // A module from a loaded precompiled bundle decodes its AST instead of parsing.
        let t_parse = std::time::Instant::now();
        let source_len=src.len();
        let parsed = (|| -> Result<_,Abrupt> {
        let synthetic_default = if key.module_type() == Some("text") {
            Some(Value::from_string(std::mem::take(&mut src)))
        } else if key.module_type() == Some("json") {
            Some(crate::builtins::parse_json_module_default(self,&src).map_err(Abrupt::Throw)?)
        } else if key.module_type() == Some("css") {
            let factory = self.module_synthetic_factories.get(&(Gc::as_ptr(&self.global) as usize))
                .and_then(|factories|factories.get("css")).cloned()
                .ok_or_else(||self.throw("TypeError","CSS module factory is not installed in these settings"))?;
            Some(factory(self,&src).map_err(Abrupt::Throw)?)
        } else { None };
        let body = if synthetic_default.is_some() { Vec::new() } else { match crate::precompiled::module_body(self, key) {
            Some(decoded) => decoded.map_err(|e| self.throw("SyntaxError", e))?,
            None if crate::parser::jsx_path(key).is_some()
                || self.jsx_module_keys.contains_key(key.url.as_ref()) =>
            {
                let ts = self
                    .jsx_module_keys
                    .get(key.url.as_ref())
                    .copied()
                    .or_else(|| crate::parser::jsx_path(key))
                    .unwrap_or(false);
                let mut options = self.jsx_options.clone();
                if let Some(loader) = self.jsx_options_loader.clone() {
                    options = loader(key, &options)
                        .map_err(|message| self.throw("SyntaxError", message))?;
                }
                options.filename = key.to_string();
                crate::parser::parse_module_jsx(&src, ts, &options).map_err(|e| {
                    let (start, _) = crate::parser::last_error_span();
                    let column = src
                        .get(..start as usize)
                        .unwrap_or("")
                        .rsplit('\n')
                        .next()
                        .unwrap_or("")
                        .encode_utf16()
                        .count()
                        + 1;
                    self.throw(
                        "SyntaxError",
                        format!("{key}:{}:{column}: {}", e.line, e.message),
                    )
                })?
            }
            // A `.ts`/`.mts`/`.cts` module is TypeScript (the parser's strip-only mode).
            None if crate::typescript::is_ts_path(key) => crate::parser::parse_module_ts(&src)
                .map_err(|e| self.throw_ts_syntax(e, &file_url(key), &src))?,
            None => crate::parser::parse_module(&src)
                .map_err(|e| self.throw("SyntaxError", e.message))?,
        } };
        Ok((synthetic_default,body))
        })();
        let (synthetic_default,body,parse_error)=match parsed {
            Ok((default,body))=>(default,body,None),
            Err(error)=>(None,Vec::new(),Some(crate::interpreter::abrupt_value(error))),
        };
        load_stats::add(&load_stats::PARSE, t_parse, source_len);
        let module_src = if synthetic_default.is_some() || parse_error.is_some() { None } else { self.adopt_parsed_source(
            Some(&crate::interpreter::stack_trace::module_display_name(
                module_url,
            )),
            0,
        ) };
        if let (Some(source), Some(context)) = (&module_src, &self.classic_script_context) {
            let mut context = context.as_ref().clone();
            context.base_url = resolution_url.to_owned();
            self.set_source_script_context(source, Rc::new(context));
        }
        let body = Rc::new(body);

        // Resolve every dependency specifier to a canonical key up front (fetching its source), so
        // the export tables can name dependencies by key. Duplicate specifiers resolve once.
        let mut resolved: HashMap<ModuleKey, ModuleKey> = HashMap::new();
        let mut dep_keys: Vec<ModuleKey> = Vec::new();
        let mut dep_srcs: Vec<(ModuleKey, String, Option<Rc<crate::ClassicScriptContext>>)> = Vec::new();
        // A dependency evaluates lazily only if *every* clause naming it is an `import defer`.
        let mut defer_specs: HashMap<ModuleKey, bool> = HashMap::new();
        for stmt in body.iter() {
            let (spec, defers) = match stmt {
                Stmt::Import(decl) => (
                    dep_map_key(&decl.source, decl.attr_type.as_deref()),
                    !decl.specs.is_empty()
                        && decl
                            .specs
                            .iter()
                            .all(|s| matches!(s, ImportSpec::DeferNamespace(_))),
                ),
                Stmt::ExportNamed {
                    source: Some(src), ..
                }
                | Stmt::ExportAll { source: src, .. } => (ModuleKey::from(src.as_ref()), false),
                _ => continue,
            };
            let e = defer_specs.entry(spec).or_insert(true);
            *e = *e && defers;
        }
        for (spec, attr_type) in module_dependency_specifiers(&body) {
            // The map key carries the attribute: the same specifier imported plainly AND with
            // `with { type: ... }` is two different dependencies.
            let map_key = dep_map_key(&spec, attr_type.as_deref());
            if resolved.contains_key(&map_key) {
                continue;
            }
            // The host-defined '<module source>' specifier resolves only in the source phase: it
            // gets a ModuleSource object but no module record.
            if spec == "<module source>" {
                resolved.insert(map_key, ModuleKey::from(spec));
                continue;
            }
            let fetched = self.fetch_module(&spec, resolution_url, attr_type.as_deref())?;
            let canon = fetched.key;
            let dsrc = fetched.source;
            // A `with { type: ... }` dependency synthesizes a wrapper module (JSON/text/bytes) —
            // keyed separately from any ordinary module of the same file.
            let canon = ModuleKey::typed(canon, attr_type.as_deref());
            let dsrc = typed_module_source(dsrc, attr_type.as_deref());
            resolved.insert(map_key, canon.clone());
            if !dep_keys.contains(&canon) {
                dep_keys.push(canon.clone());
                if !self.module_recs.contains_key(&canon) {
                    dep_srcs.push((canon, dsrc, fetched.script_context));
                }
            }
        }

        // Create the scope + namespace object, then build the export tables. This record must exist
        // before we recurse into dependencies so an import cycle resolves back to it.
        let env = new_scope(Some(self.global_env.clone()));
        // Top-level `this` in a module is `undefined` (not the global object).
        env.borrow_mut().vars.insert(
            "this".to_string(),
            Binding::data(Value::Undefined, false, true),
        );
        let ns_obj = Object::new(None);
        let ns = Value::Obj(ns_obj.clone());
        self.modules.insert(key.clone(), ns.clone());
        let meta = self.build_import_meta_with_base(module_url, resolution_url);
        // `import.meta` resolves lexically: a function exported from this module keeps seeing
        // this module's object no matter who calls it.
        crate::eval::bind(&env, "%importmeta%", meta.clone());

        let mut tables = build_export_tables(&body, &resolved);
        if let Some(value)=synthetic_default {
            env.borrow_mut().vars.insert("*default*",Binding::data(value,false,true));
            tables.local_exports.insert("default".into(),"*default*".into());
        }
        let deferred_deps: Vec<ModuleKey> = resolved
            .iter()
            .filter(|(spec, _)| defer_specs.get(*spec).copied().unwrap_or(false))
            .map(|(_, canon)| canon.clone())
            .collect();
        let failed=parse_error.is_some();
        self.module_recs.insert(
            key.clone(),
            ModuleRec {
                body: body.clone(),
                has_tla: body_has_tla(&body),
                retired_eager_named: None,
                retired_eager_order: None,
                src: module_src,
                env: env.clone(),
                ns,
                meta,
                dep_keys,
                resolved,
                local_exports: tables.local_exports,
                imports: tables.imports,
                indirect: tables.indirect,
                star_as: tables.star_as,
                stars: tables.stars,
                linked: false,
                evaluated: failed,
                evaluating: false,
                eval_error: parse_error,
                deferred_deps,
                top_promise: None,
                started: false,
                pending_async: 0,
                async_order: None,
                async_parents: Vec::new(),
                dfs_index: None,
                dfs_anc: 0,
                on_stack: false,
                cycle_root: None,
            },
        );

        added.push(key.clone());

        // Recurse: parse each dependency (fetched during specifier resolution above).
        for (canon, dsrc, context) in dep_srcs {
            let module_url = context.as_ref().map_or_else(||canon.to_string(), |context|context.base_url.clone());
            let previous = std::mem::replace(&mut self.classic_script_context, context);
            let result = self.parse_and_register_graph(&canon, &module_url, &module_url, Some(dsrc), added);
            self.classic_script_context = previous;
            result?;
        }
        Ok(())
    }

    // --- Link phase ---------------------------------------------------------------------------

    /// Link `key` and its dependencies depth-first (once each): instantiate top-level bindings
    /// (functions initialized, lexicals in their temporal dead zone), wire imports to live cells,
    /// validate indirect exports, and build the namespace object.
    fn link_module(&mut self, key: &ModuleKey) -> Result<(), Abrupt> {
        if self.module_recs[key].linked {
            return Ok(());
        }
        if let Some(error)=self.module_recs[key].eval_error.clone() { return Err(Abrupt::Throw(error)); }
        self.module_recs.get_mut(key).unwrap().linked = true;
        let result = self.link_module_graph(key);
        if result.is_err() {
            if let Some(rec) = self.module_recs.get_mut(key) {
                rec.linked = false;
            }
        }
        result
    }

    fn link_module_graph(&mut self, key: &ModuleKey) -> Result<(), Abrupt> {
        let (body, env, ns) = {
            let rec = &self.module_recs[key];
            (rec.body.clone(), rec.env.clone(), rec.ns.clone())
        };
        let dep_keys = self.module_recs[key].dep_keys.clone();
        for dep in &dep_keys {
            self.link_module(dep)?;
        }

        self.hoist(&body, &env, &[]);
        self.declare_block_lexicals(&body, &env, false);
        self.declare_default_placeholder(&body, &env);
        self.validate_indirect_exports(key)?;
        self.link_imports(key, &body, &env)?;
        if let Value::Obj(ns_obj) = ns {
            self.build_namespace(key, &ns_obj)?;
        }
        Ok(())
    }

    /// Every `export { x } from 'dep'` entry must resolve to a single binding (spec: unresolvable or
    /// ambiguous indirect exports are a link-time SyntaxError).
    fn validate_indirect_exports(&mut self, key: &ModuleKey) -> Result<(), Abrupt> {
        let names: Vec<String> = self.module_recs[key].indirect.keys().cloned().collect();
        for name in names {
            match self.resolve_export(key, &name, &mut Vec::new()) {
                Resolution::Local(..) | Resolution::Ns(..) | Resolution::DeferNs(..) => {}
                Resolution::Ambiguous => {
                    return Err(self.throw(
                        "SyntaxError",
                        format!("the requested module provides an ambiguous export named '{name}'"),
                    ));
                }
                Resolution::NotFound => {
                    return Err(self.throw(
                        "SyntaxError",
                        format!("the requested module does not provide an export named '{name}'"),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Fetch a dependency's `(canonical_key, source)` via the host loader.
    fn fetch_module(
        &mut self,
        specifier: &str,
        referrer: &str,
        attr_type: Option<&str>,
    ) -> Result<crate::ModuleFetchResult, Abrupt> {
        let inherited = self.classic_script_context.clone();
        let legacy = |key, source| crate::ModuleFetchResult { key, source, script_context: inherited.clone() };
        let settings_key = Gc::as_ptr(&self.global) as usize;
        if let Some(loader) = self.module_fetch_loaders.get(&settings_key).cloned() {
            if attr_type==Some("css") && !self.module_synthetic_factories.get(&settings_key).is_some_and(|factories|factories.contains_key("css")) {
                return Err(self.throw("TypeError","CSS modules are not exposed in these settings"));
            }
            let resolution=self.resolve_module_specifier_for_host(specifier,referrer)?;
            let request = crate::ModuleFetchRequest {
                resolution, settings_key, specifier: specifier.to_owned(), referrer: referrer.to_owned(),
                attribute_type: attr_type.map(str::to_owned), script_context: inherited.clone(),
            };
            return loader(request).ok_or_else(|| self.throw("TypeError", format!("module fetch failed: {specifier} (imported from {referrer})")));
        }
        if self.import_maps.contains_key(&settings_key) {
            return Err(self.throw("TypeError","no HTML module fetch loader is installed for these settings"));
        }
        #[cfg(feature = "parallel")]
        if specifier == "lumen:parallel" {
            if !self.host_state.has::<crate::parallel::api::Realm>() {
                return Err(self.throw("TypeError", "parallel host is not installed"));
            }
            if attr_type.is_some() {
                return Err(self.throw(
                    "TypeError",
                    "lumen:parallel does not accept import attributes",
                ));
            }
            return Ok(legacy(
                specifier.into(),
                "export const run = Lumen.parallel.run; export const spawn = Lumen.parallel.spawn;"
                    .into(),
            ));
        }
        // A specifier naming a module of a loaded precompiled bundle resolves inside the bundle
        // (its AST is decoded in `parse_and_register`); no host loader is consulted.
        if let Some(key) = crate::precompiled::resolve(self, specifier, referrer, attr_type) {
            return Ok(legacy(key, String::new()));
        }
        let loader = match &self.module_loader {
            Some(l) => l.clone(),
            None => return Err(self.throw("TypeError", "no module loader configured")),
        };
        // Runtime-level customization hooks (Node's `module.registerHooks`) get the first say:
        // they can redirect the specifier or synthesize the module outright.
        let mut specifier = specifier.to_string();
        match self.esm_hook(&specifier, referrer, attr_type)? {
            EsmHook::Default => {}
            EsmHook::Redirect(s) => specifier = s,
            EsmHook::Module(key, source) => return Ok(legacy(key, source)),
        }
        let t_fetch = std::time::Instant::now();
        let r = loader(&specifier, referrer, attr_type);
        load_stats::add(&load_stats::FETCH, t_fetch, 0);
        match r {
            Some((key, source)) => Ok(legacy(key, source)),
            None => Err(self.throw(
                "TypeError",
                format!("module not found: {specifier} (imported from {referrer})"),
            )),
        }
    }

    /// Consult `globalThis.__lumenEsmHook(specifier, referrer, attrType)` when a runtime installs
    /// one. It answers `undefined` (resolve normally), `{ specifier }` (resolve this instead) or
    /// `{ key, source }` (an ES module synthesized by the hooks).
    fn esm_hook(
        &mut self,
        specifier: &str,
        referrer: &str,
        attr_type: Option<&str>,
    ) -> Result<EsmHook, Abrupt> {
        let global = Value::Obj(self.global.clone());
        let hook = self.get_member(&global, "__lumenEsmHook")?;
        if !hook.is_callable() {
            return Ok(EsmHook::Default);
        }
        let attr = attr_type.map_or(Value::Undefined, |t| Value::from_string(t.to_string()));
        let args = [
            Value::from_string(specifier.to_string()),
            Value::from_string(referrer.to_string()),
            attr,
        ];
        let result = self.call(hook, Value::Undefined, &args)?;
        if !matches!(result, Value::Obj(_)) {
            return Ok(EsmHook::Default);
        }
        let string_field = |i: &mut Self, name: &str| -> Result<Option<String>, Abrupt> {
            Ok(match i.get_member(&result, name)? {
                Value::Str(s) => Some(s.to_string()),
                _ => None,
            })
        };
        if let Some(redirect) = string_field(self, "specifier")? {
            return Ok(EsmHook::Redirect(redirect));
        }
        match (string_field(self, "key")?, string_field(self, "source")?) {
            (Some(key), Some(source)) => Ok(EsmHook::Module(key, source)),
            _ => Ok(EsmHook::Default),
        }
    }

    /// Build a module's `import.meta` object (`{ url, filename?, dirname? }`, extensible, no
    /// prototype). For a file-backed module the `url` is a `file://` URL (as Node's is, so
    /// `new URL(rel, import.meta.url)` resolves), and `filename`/`dirname` (Node 20.11+) are the
    /// plain paths.
    pub(crate) fn build_import_meta(&mut self,key:&str)->Value {
        self.build_import_meta_with_base(key,key)
    }
    fn build_import_meta_with_base(&mut self, key: &str, resolution_url: &str) -> Value {
        let meta = Object::new(None);
        let is_path = key.starts_with('/');
        let url = if is_path {
            format!("file://{key}")
        } else {
            key.to_string()
        };
        {
            let mut b = meta.borrow_mut();
            b.props.insert(
                "url",
                Property::data(Value::from_string(url), true, true, true),
            );
            if is_path {
                b.props.insert(
                    "filename",
                    Property::data(Value::from_string(key.to_string()), true, true, true),
                );
                let dir = std::path::Path::new(key)
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                b.props.insert(
                    "dirname",
                    Property::data(Value::from_string(dir), true, true, true),
                );
            }
        }
        let map=self.import_map_for_host();
        if map.is_some() || self.module_fetch_loaders.contains_key(&(Gc::as_ptr(&self.global) as usize)) {
            let base=resolution_url.to_owned();
            let resolve=self.new_native_fn("resolve",1,Rc::new(move |ctx,_,args| {
                let specifier=ctx.to_string(args.first().unwrap_or(&Value::Undefined)).map_err(crate::interpreter::abrupt_value)?;
                let result=if let Some(map)=&map {
                    map.lock().map_err(|_| "import-map state is unavailable".to_owned())
                        .and_then(|mut map|map.resolve(specifier.as_str(),&base).map_err(|error|error.to_string()))
                } else { lumen_common::import_maps::resolve_without_map(specifier.as_str(),&base).map_err(|error|error.to_string()) };
                result.map(|resolution|Value::from_string(resolution.url)).map_err(|error|crate::interpreter::abrupt_value(ctx.throw("TypeError",error)))
            }));
            meta.borrow_mut().props.insert("resolve",Property::data(resolve,true,true,true));
        }
        Value::Obj(meta)
    }

    /// Declare an uninitialized `*default*` binding for `export default <expression>` (and anonymous
    /// default function/class), so an importer can wire a live binding to it during linking.
    fn declare_default_placeholder(&mut self, body: &[Stmt], env: &Env) {
        for stmt in body {
            let Stmt::ExportDefault(inner) = stmt else {
                continue;
            };
            // `*default*` gets an uninitialized (TDZ) binding for an `export default <expression>` or
            // anonymous `class`. A named function/class binds its own name; an anonymous function is a
            // hoistable declaration already bound (initialized) by `hoist`.
            let needs_tdz = matches!(&**inner, Stmt::Expr(_))
                || matches!(&**inner, Stmt::ClassDecl(c) if c.name.is_none());
            if needs_tdz {
                env.borrow_mut().vars.insert(
                    "*default*".to_string(),
                    Binding {
                        value: Value::Undefined,
                        mutable: false,
                        strict_immutable: true,
                        initialized: false,
                        import: false,
                        deletable: false,
                    },
                );
            }
        }
    }

    /// Wire every `import` binding in `body` to a live cell (or namespace object) of its dependency.
    fn link_imports(&mut self, key: &ModuleKey, body: &[Stmt], env: &Env) -> Result<(), Abrupt> {
        for stmt in body {
            let Stmt::Import(decl) = stmt else { continue };
            let dep = self.resolved_key(key, &dep_map_key(&decl.source, decl.attr_type.as_deref()));
            for spec in &decl.specs {
                match spec {
                    ImportSpec::Namespace(local) => {
                        let ns = self.module_recs[&dep].ns.clone();
                        self.bind_local(env, local, ns);
                    }
                    ImportSpec::DeferNamespace(local) => {
                        let dns = self.make_deferred_ns(&dep);
                        self.bind_local(env, local, dns);
                    }
                    ImportSpec::Source(local) => {
                        // GetModuleSource of a Source Text Module Record throws a SyntaxError:
                        // only the host-defined '<module source>' module has a ModuleSource.
                        if dep != "<module source>" {
                            return Err(self.throw(
                                "SyntaxError",
                                "source-text modules have no module source",
                            ));
                        }
                        let src_obj = self.module_source_of(&dep);
                        self.bind_local(env, local, src_obj);
                    }
                    ImportSpec::Default(local) => {
                        self.link_named(env, local, &dep, "default")?;
                    }
                    ImportSpec::Named { imported, local } => {
                        self.link_named(env, local, &dep, imported)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Wire `local` to the binding that `dep` exports as `name` (a link-time SyntaxError if the
    /// export is missing or ambiguous).
    fn link_named(&mut self, env: &Env, local: &str, dep: &ModuleKey, name: &str) -> Result<(), Abrupt> {
        match self.resolve_export(dep, name, &mut Vec::new()) {
            Resolution::Local(src_env, src_local) => {
                env.borrow_mut().link_import(local, src_env, src_local);
                Ok(())
            }
            Resolution::Ns(ns) => {
                self.bind_local(env, local, ns);
                Ok(())
            }
            Resolution::DeferNs(dep) => {
                let dns = self.make_deferred_ns(&dep);
                self.bind_local(env, local, dns);
                Ok(())
            }
            Resolution::Ambiguous => Err(self.throw(
                "SyntaxError",
                format!("the requested module provides an ambiguous export named '{name}'"),
            )),
            Resolution::NotFound => Err(self.throw(
                "SyntaxError",
                format!("the requested module '{dep}' does not provide an export named '{name}'"),
            )),
        }
    }

    /// The (cached, per canonical key) ModuleSource object a source-phase import binds: an
    /// ordinary object whose prototype is the host's concrete module-source prototype, itself
    /// inheriting from %AbstractModuleSource%.prototype.
    pub(crate) fn module_source_of(&mut self, dep: &str) -> Value {
        if let Some(v) = self.module_source_objs.get(dep) {
            return v.clone();
        }
        let proto = self.extra_protos.get("%ModuleSourceProto%").cloned();
        let obj = Object::new(proto);
        let v = Value::Obj(obj);
        self.module_source_objs.insert(dep.to_string(), v.clone());
        v
    }

    fn bind_local(&self, env: &Env, local: &str, value: Value) {
        env.borrow_mut()
            .vars
            .insert(local.to_string(), Binding::data(value, false, true));
    }

    fn resolved_key(&self, referrer: &ModuleKey, specifier: &ModuleKey) -> ModuleKey {
        self.module_recs[referrer]
            .resolved
            .get(specifier)
            .cloned()
            .unwrap_or_else(|| specifier.clone())
    }

    // --- Export resolution (spec ResolveExport / GetExportedNames) -----------------------------

    /// Resolve `name` exported by module `key` to a concrete binding, following indirect and star
    /// re-exports. `seen` guards against cyclic re-export chains.
    fn resolve_export(
        &self,
        key: &ModuleKey,
        name: &str,
        seen: &mut Vec<(ModuleKey, String)>,
    ) -> Resolution {
        let pair = (key.clone(), name.to_string());
        if seen.contains(&pair) {
            return Resolution::NotFound;
        }
        seen.push(pair);
        // A source-phase re-export resolves to the requested module's ModuleSource object.
        if name == "~source~" {
            return match self.module_source_objs.get(key.url()) {
                Some(v) => Resolution::Ns(v.clone()),
                None => Resolution::NotFound,
            };
        }
        let rec = match self.module_recs.get(key) {
            Some(r) => r,
            None => return Resolution::NotFound,
        };
        if let Some(local) = rec.local_exports.get(name) {
            // A re-exported import resolves to its origin binding (so a namespace re-exported by two
            // paths compares equal, and a named re-export follows through to the real module).
            if let Some(origin) = rec.imports.get(local) {
                return match origin {
                    ImportOrigin::Namespace(dep) => match self.module_recs.get(dep) {
                        Some(d) => Resolution::Ns(d.ns.clone()),
                        None => Resolution::NotFound,
                    },
                    ImportOrigin::DeferNamespace(dep) => {
                        if self.module_recs.contains_key(dep) {
                            Resolution::DeferNs(dep.clone())
                        } else {
                            Resolution::NotFound
                        }
                    }
                    ImportOrigin::Named(dep, imported) => {
                        let (dep, imported) = (dep.clone(), imported.clone());
                        self.resolve_export(&dep, &imported, seen)
                    }
                };
            }
            return Resolution::Local(rec.env.clone(), local.clone());
        }
        if let Some((dep, imported)) = rec.indirect.get(name) {
            let (dep, imported) = (dep.clone(), imported.clone());
            return self.resolve_export(&dep, &imported, seen);
        }
        if let Some(dep) = rec.star_as.get(name) {
            if let Some(drec) = self.module_recs.get(dep) {
                return Resolution::Ns(drec.ns.clone());
            }
            return Resolution::NotFound;
        }
        if name == "default" {
            // `default` is never provided by a `export *` re-export.
            return Resolution::NotFound;
        }
        let stars = rec.stars.clone();
        let mut star_resolution = Resolution::NotFound;
        for dep in &stars {
            match self.resolve_export(dep, name, seen) {
                Resolution::Ambiguous => return Resolution::Ambiguous,
                Resolution::NotFound => {}
                r => match &star_resolution {
                    Resolution::NotFound => star_resolution = r,
                    existing => {
                        if !same_binding(existing, &r) {
                            return Resolution::Ambiguous;
                        }
                    }
                },
            }
        }
        star_resolution
    }

    /// All names module `key` exports (spec GetExportedNames), excluding `default` from stars.
    fn exported_names(&self, key: &ModuleKey, seen: &mut Vec<ModuleKey>) -> Vec<String> {
        if seen.contains(key) {
            return Vec::new();
        }
        seen.push(key.clone());
        let rec = match self.module_recs.get(key) {
            Some(r) => r,
            None => return Vec::new(),
        };
        let mut names: Vec<String> = Vec::new();
        let push = |n: &str, names: &mut Vec<String>| {
            if !names.iter().any(|x| x == n) {
                names.push(n.to_string());
            }
        };
        for n in rec.local_exports.keys() {
            push(n, &mut names);
        }
        for n in rec.indirect.keys() {
            push(n, &mut names);
        }
        for n in rec.star_as.keys() {
            push(n, &mut names);
        }
        let stars = rec.stars.clone();
        for dep in &stars {
            for n in self.exported_names(dep, seen) {
                if n != "default" {
                    push(&n, &mut names);
                }
            }
        }
        names
    }

    /// Populate `ns` with one entry per unambiguously-resolvable export, sorted by name, and record
    /// how each reads its live value. Namespace objects are frozen and prototype-less.
    fn build_namespace(&mut self, key: &ModuleKey, ns: &crate::value::Gc) -> Result<(), Abrupt> {
        let mut names = self.exported_names(key, &mut Vec::new());
        names.sort();
        let mut live: crate::fasthash::FastMap<String, NsBinding> = Default::default();
        for name in &names {
            match self.resolve_export(key, name, &mut Vec::new()) {
                Resolution::Local(env, local) => {
                    live.insert(name.clone(), NsBinding::Live(env, local));
                    // A placeholder own property makes the name enumerable / own-key-visible; reads
                    // go through the live map (see `get_member`).
                    ns.borrow_mut().props.insert(
                        name.as_str(),
                        Property::data(Value::Undefined, true, true, false),
                    );
                }
                Resolution::Ns(v) => {
                    live.insert(name.clone(), NsBinding::Static(v.clone()));
                    ns.borrow_mut()
                        .props
                        .insert(name.as_str(), Property::data(v, true, true, false));
                }
                Resolution::DeferNs(dep) => {
                    let v = self.make_deferred_ns(&dep);
                    live.insert(name.clone(), NsBinding::Static(v.clone()));
                    ns.borrow_mut()
                        .props
                        .insert(name.as_str(), Property::data(v, true, true, false));
                }
                // Ambiguous / unresolvable star names are omitted from the namespace.
                _ => {}
            }
        }
        // @@toStringTag = "Module": non-writable, non-enumerable, non-configurable.
        if let Some(tag) = crate::builtins::to_string_tag_key(self) {
            ns.borrow_mut().props.insert(
                tag,
                Property::data(
                    Value::from_string("Module".to_string()),
                    false,
                    false,
                    false,
                ),
            );
        }
        ns.borrow_mut().extensible = false;
        ns.borrow().ic_plain.set(false);
        // Pointer-keyed metadata owns a pin until collection evicts both together.
        self.gc_pin(ns);
        self.module_ns.insert(Gc::as_ptr(ns) as usize, live);
        Ok(())
    }

    /// Build the distinct deferred-namespace object for `import defer * as ns`: same exports and
    /// live bindings as the module's ordinary namespace, but a separate identity, a
    /// "Deferred Module" @@toStringTag, and evaluation-on-first-string-keyed-access.
    fn make_deferred_ns(&mut self, dep: &ModuleKey) -> Value {
        // One deferred namespace per module: every `import defer` of the same module (and every
        // re-export of such a binding) observes the same object.
        if let Some(v) = self.deferred_ns_objs.get(dep) {
            return v.clone();
        }
        let base = self.module_recs[dep].ns.clone();
        let Value::Obj(base_o) = &base else {
            return base;
        };
        let dns = Object::new(None);
        {
            let src = base_o.borrow();
            let mut dst = dns.borrow_mut();
            for (k, p) in src.props.iter() {
                let mut p = p.clone();
                if Interp::is_sym_key(&k) {
                    if let Value::Str(tag) = p.value() {
                        if &*tag == "Module" {
                            p.set_value(Value::from_string("Deferred Module".to_string()));
                        }
                    }
                }
                dst.props.insert(k.clone(), p);
            }
            dst.extensible = false;
        }
        if let Some(live) = self.module_ns.get(&(Gc::as_ptr(base_o) as usize)).cloned() {
            dns.borrow().ic_plain.set(false);
            self.gc_pin(&dns);
            self.module_ns.insert(Gc::as_ptr(&dns) as usize, live);
        }
        dns.borrow().ic_plain.set(false);
        self.deferred_ns
            .insert(Gc::as_ptr(&dns) as usize, DeferredModuleKey {
                settings: Gc::as_ptr(&self.global) as usize,
                key: dep.clone(),
            });
        self.deferred_ns_objs
            .insert(dep.clone(), Value::Obj(dns.clone()));
        Value::Obj(dns)
    }

    // --- Evaluate phase -----------------------------------------------------------------------

    /// Evaluate module `key` and its dependencies depth-first (each body runs at most once). A body
    /// that throws poisons the module so later imports observe the same error.
    /// Deferred-namespace trigger: evaluate a module on first access of its namespace.
    pub(crate) fn evaluate_deferred(&mut self, key: &ModuleKey) -> Result<(), Abrupt> {
        if self.module_recs.contains_key(key) {
            // ReadyForSyncExecution: touching a deferred namespace while its module — or any
            // module in its dependency graph — is still evaluating is a TypeError.
            if !self.ready_for_sync(key, &mut Vec::new()) {
                return Err(self.throw(
                    "TypeError",
                    "cannot access a deferred namespace while its module graph is evaluating",
                ));
            }
            self.evaluate_module(key)?;
            // A stub minted mid-link for a cyclic dependency copied an empty export table —
            // hydrate it from the real namespace now that the module is linked and evaluated.
            if let Some(Value::Obj(stub)) = self.deferred_ns_objs.get(key).cloned() {
                let hydrated = self.module_ns.contains_key(&(Gc::as_ptr(&stub) as usize));
                if !hydrated {
                    let base = self.module_recs[key].ns.clone();
                    if let Value::Obj(base_o) = &base {
                        {
                            let src = base_o.borrow();
                            let mut dst = stub.borrow_mut();
                            for (k, p) in src.props.iter() {
                                if dst.props.get(&k).is_none() {
                                    let mut p = p.clone();
                                    if Interp::is_sym_key(&k) {
                                        if let Value::Str(tag) = p.value() {
                                            if &*tag == "Module" {
                                                p.set_value(Value::from_string(
                                                    "Deferred Module".to_string(),
                                                ));
                                            }
                                        }
                                    }
                                    dst.props.insert(k.clone(), p);
                                }
                            }
                        }
                        if let Some(live) =
                            self.module_ns.get(&(Gc::as_ptr(base_o) as usize)).cloned()
                        {
                            stub.borrow().ic_plain.set(false);
                            self.gc_pin(&stub);
                            self.module_ns.insert(Gc::as_ptr(&stub) as usize, live);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether `key`'s whole (non-deferred) dependency graph is free of mid-evaluation modules.
    fn ready_for_sync(&self, key: &ModuleKey, seen: &mut Vec<ModuleKey>) -> bool {
        if seen.iter().any(|k| k == key) {
            return true;
        }
        seen.push(key.clone());
        let rec = match self.module_recs.get(key) {
            Some(r) => r,
            None => return true,
        };
        if rec.evaluated && rec.eval_error.is_none() {
            return true;
        }
        // evaluating, evaluating-async (started and not finished), or parked at an await.
        if rec.evaluating || (rec.started && !rec.evaluated) {
            return false;
        }
        if !rec.evaluated && rec.has_tla {
            return false;
        }
        // Every requested module counts — deferred requests included (a deferred dep that is
        // itself mid-evaluation still blocks synchronous evaluation).
        let deps = rec.dep_keys.clone();
        deps.iter().all(|d| self.ready_for_sync(d, seen))
    }

    fn evaluate_module(&mut self, key: &ModuleKey) -> Result<(), Abrupt> {
        {
            let rec = &self.module_recs[key];
            if let Some(err) = &rec.eval_error {
                return Err(Abrupt::Throw(err.clone()));
            }
            if rec.evaluated || rec.evaluating {
                return Ok(());
            }
        }
        self.module_recs.get_mut(key).unwrap().evaluating = true;

        let dep_keys = self.module_recs[key].dep_keys.clone();
        let deferred = self.module_recs[key].deferred_deps.clone();
        // Evaluate dependencies at the position of their first NON-defer clause (an
        // `import defer` earlier in the file must not pull the module's evaluation forward).
        let body_order = self.eager_dep_order(key);
        // Only deps in dep_keys evaluate here (source-phase pseudo-modules are excluded there).
        let order: Vec<ModuleKey> = if body_order.is_empty() {
            dep_keys
                .iter()
                .filter(|d| !deferred.contains(*d))
                .cloned()
                .collect()
        } else {
            body_order
                .into_iter()
                .filter(|d| dep_keys.contains(d))
                .collect()
        };
        for dep in &order {
            if deferred.contains(dep) {
                // A dependency imported only via `import defer` evaluates lazily — but its
                // ASYNC (top-level-await) transitive dependencies still evaluate eagerly.
                self.evaluate_async_subgraph(dep, &mut Vec::new())?;
                continue;
            }
            self.evaluate_module(dep)?;
        }
        for dep in &dep_keys {
            if deferred.contains(dep) && !order.contains(dep) {
                self.evaluate_async_subgraph(dep, &mut Vec::new())?;
            }
        }

        let (body, env, meta, src) = {
            let rec = &self.module_recs[key];
            (
                rec.body.clone(),
                rec.env.clone(),
                rec.meta.clone(),
                rec.src.clone(),
            )
        };
        let saved_meta = self.import_meta.take();
        let saved_strict = self.strict;
        self.import_meta = Some(meta);
        self.strict = true;
        let result = self.with_script_frame(src, false, |i| i.run_stmt_list(&body, &env));
        self.import_meta = saved_meta;
        self.strict = saved_strict;

        let rec = self.module_recs.get_mut(key).unwrap();
        rec.evaluating = false;
        let result = match result {
            Ok(_) => {
                rec.evaluated = true;
                Ok(())
            }
            Err(a) => {
                let v = crate::interpreter::abrupt_value(a);
                rec.eval_error = Some(v.clone());
                rec.evaluated = true;
                Err(Abrupt::Throw(v))
            }
        };
        self.retire_module_body(key);
        result
    }

    /// Exports retain their own function/class nodes and environment. Completed module
    /// initialization statements cannot execute again, but graph metadata is still needed
    /// by subsequent imports, including deferred imports through an evaluated module.
    fn retire_module_body(&mut self, key: &ModuleKey) {
        let Some(rec) = self.module_recs.get(key) else {
            return;
        };
        if !rec.evaluated || rec.retired_eager_order.is_some() {
            return;
        }
        let named = self.eager_named_deps(key);
        let order = self.eager_dep_order(key);
        let rec = self.module_recs.get_mut(key).unwrap();
        rec.retired_eager_named = Some(named);
        rec.retired_eager_order = Some(order);
        rec.body = Rc::new(Vec::new());
    }

    /// Numeric-only diagnostic descriptors; statement slots exclude nested AST storage.
    pub(crate) fn module_program_memory(&self) -> [usize; 7] {
        let mut result = [0; 7];
        let mut sources = std::collections::HashSet::new();
        for rec in self.module_recs.all_values() {
            if !rec.body.is_empty() {
                result[0] += 1;
                result[1] += usize::from(!rec.evaluated);
                result[2] += usize::from(rec.evaluating);
                result[3] += usize::from(rec.evaluated);
                result[4] += rec.body.len();
                result[5] += rec.body.capacity();
            }
            if let Some(source) = &rec.src {
                if sources.insert(Rc::as_ptr(source) as *const u8 as usize) {
                    result[6] += source.len();
                }
            }
        }
        result
    }

    /// This module's dependencies in evaluation order: each dep at the position of its first
    /// non-defer import/export-from clause; defer-only deps at their first (defer) position.
    fn eager_dep_order(&self, key: &ModuleKey) -> Vec<ModuleKey> {
        let rec = match self.module_recs.get(key) {
            Some(r) => r,
            None => return Vec::new(),
        };
        if let Some(order) = &rec.retired_eager_order {
            return order.clone();
        }
        let body = rec.body.clone();
        let resolved = rec.resolved.clone();
        let mut eager: Vec<ModuleKey> = Vec::new();
        let mut defer_seen: Vec<ModuleKey> = Vec::new();
        let push = |list: &mut Vec<ModuleKey>, k: &ModuleKey| {
            if !list.iter().any(|x| x == k) {
                list.push(k.clone());
            }
        };
        for stmt in body.iter() {
            let (spec, defers) = match stmt {
                Stmt::Import(decl) => (
                    dep_map_key(&decl.source, decl.attr_type.as_deref()),
                    !decl.specs.is_empty()
                        && decl
                            .specs
                            .iter()
                            .all(|s| matches!(s, crate::ast::ImportSpec::DeferNamespace(_))),
                ),
                Stmt::ExportNamed {
                    source: Some(src), ..
                }
                | Stmt::ExportAll { source: src, .. } => (ModuleKey::from(src.as_ref()), false),
                _ => continue,
            };
            let Some(k) = resolved.get(&spec) else {
                continue;
            };
            if defers {
                push(&mut defer_seen, k);
            } else {
                push(&mut eager, k);
            }
        }
        // A defer-only dep (never named eagerly) keeps its SOURCE position — its async
        // subgraph evaluates there. A dep imported both ways evaluates at its first EAGER
        // position (the defer clause never pulls it forward).
        let mut merged: Vec<ModuleKey> = Vec::new();
        for stmt in body.iter() {
            let (spec, defers) = match stmt {
                Stmt::Import(decl) => (
                    dep_map_key(&decl.source, decl.attr_type.as_deref()),
                    !decl.specs.is_empty()
                        && decl
                            .specs
                            .iter()
                            .all(|s| matches!(s, crate::ast::ImportSpec::DeferNamespace(_))),
                ),
                Stmt::ExportNamed {
                    source: Some(src), ..
                }
                | Stmt::ExportAll { source: src, .. } => (ModuleKey::from(src.as_ref()), false),
                _ => continue,
            };
            if let Some(k) = resolved.get(&spec) {
                let is_defer_only = defer_seen.contains(k) && !eager.contains(k);
                let place = if is_defer_only { true } else { !defers };
                if place && !merged.iter().any(|x| x == k) {
                    merged.push(k.clone());
                }
            }
        }
        merged
    }

    /// InnerModuleEvaluation for a graph containing top-level await. Dependencies process in
    /// source order; a module executes once its [[PendingAsyncDependencies]] hits zero — in a
    /// coroutine for a TLA body (each await interleaves with the job queue), synchronously
    /// otherwise. Completion cascades through [[AsyncParentModules]] in [[AsyncEvaluationOrder]].
    fn inner_module_evaluation_async(
        &mut self,
        key: &ModuleKey,
        stack: &mut Vec<ModuleKey>,
        index: &mut usize,
    ) {
        if self.module_recs[key].started || self.module_recs[key].evaluated {
            return;
        }
        {
            let rec = self.module_recs.get_mut(key).unwrap();
            rec.started = true;
            rec.dfs_index = Some(*index);
            rec.dfs_anc = *index;
            rec.on_stack = true;
        }
        *index += 1;
        stack.push(key.clone());

        let dep_keys = self.module_recs[key].dep_keys.clone();
        let deferred = self.module_recs[key].deferred_deps.clone();
        let body_order = self.eager_dep_order(key);
        let order: Vec<ModuleKey> = if body_order.is_empty() {
            dep_keys
                .iter()
                .filter(|d| !deferred.contains(*d))
                .cloned()
                .collect()
        } else {
            body_order
                .into_iter()
                .filter(|d| dep_keys.contains(d))
                .collect()
        };
        for dep in &order {
            if deferred.contains(dep) {
                self.defer_async_deps(key, dep, stack, index);
                continue;
            }
            self.inner_module_evaluation_async(dep, stack, index);
            if self.module_recs[&dep].on_stack {
                // Still on the DFS stack: same strongly-connected component. An in-cycle
                // dependency that already parked at a top-level await still counts as pending.
                let danc = self.module_recs[&dep].dfs_anc;
                {
                    let rec = self.module_recs.get_mut(key).unwrap();
                    rec.dfs_anc = rec.dfs_anc.min(danc);
                }
                let dep_parked = {
                    let d = &self.module_recs[&dep];
                    d.async_order.is_some() && !d.evaluated
                };
                if dep_parked {
                    self.module_recs.get_mut(key).unwrap().pending_async += 1;
                    self.module_recs
                        .get_mut(&dep)
                        .unwrap()
                        .async_parents
                        .push(key.clone());
                }
                continue;
            }
            // Completed (or async-parked) dependency: waiting attaches to its CYCLE ROOT.
            let root = self.module_recs[&dep]
                .cycle_root
                .clone()
                .unwrap_or_else(|| dep.clone());
            let (dep_err, dep_done, dep_async) = {
                let d = &self.module_recs[&root];
                (d.eval_error.clone(), d.evaluated, d.async_order.is_some())
            };
            if let Some(err) = dep_err {
                self.finish_scc(key, stack);
                self.async_module_rejected(key, err);
                return;
            }
            if !dep_done && dep_async {
                self.module_recs.get_mut(key).unwrap().pending_async += 1;
                self.module_recs
                    .get_mut(&root)
                    .unwrap()
                    .async_parents
                    .push(key.clone());
            }
        }
        for dep in &dep_keys {
            if deferred.contains(dep) && !order.contains(dep) {
                self.defer_async_deps(key, dep, stack, index);
            }
        }
        let has_tla = self.module_recs[key].has_tla;
        if self.module_recs[key].pending_async > 0 || has_tla {
            self.module_async_seq += 1;
            self.module_recs.get_mut(key).unwrap().async_order = Some(self.module_async_seq);
        }
        if self.module_recs[key].pending_async == 0 {
            self.module_execute_async(key);
        }
        self.finish_scc(key, stack);
    }

    /// `import defer`: the deferred module's own evaluation waits for a namespace access, but
    /// every module in its subgraph with top-level await evaluates NOW — and the deferring
    /// parent pends on each (so the later synchronous evaluation meets no pending async dep).
    fn defer_async_deps(
        &mut self,
        parent: &ModuleKey,
        dep: &ModuleKey,
        stack: &mut Vec<ModuleKey>,
        index: &mut usize,
    ) {
        let mut graph = Vec::new();
        self.collect_graph(dep, &mut graph);
        for m in graph {
            if !self.module_recs[&m].has_tla {
                continue;
            }
            if !self.module_recs[&m].evaluated && self.module_recs[&m].top_promise.is_none() {
                let p = self.new_promise();
                self.module_recs.get_mut(&m).unwrap().top_promise = Some(p);
            }
            self.inner_module_evaluation_async(&m, stack, index);
            if self.module_recs[&m].on_stack {
                continue;
            }
            let root = self.module_recs[&m]
                .cycle_root
                .clone()
                .unwrap_or_else(|| m.clone());
            let (dep_err, dep_done, dep_async) = {
                let d = &self.module_recs[&root];
                (d.eval_error.clone(), d.evaluated, d.async_order.is_some())
            };
            if dep_err.is_some() || dep_done || !dep_async {
                continue;
            }
            let already = self.module_recs[&root]
                .async_parents
                .iter()
                .any(|p| p == parent);
            if !already {
                self.module_recs.get_mut(parent).unwrap().pending_async += 1;
                self.module_recs
                    .get_mut(&root)
                    .unwrap()
                    .async_parents
                    .push(parent.clone());
            }
        }
    }

    /// If `key` is its component's root, pop the SCC off the DFS stack and stamp each member's
    /// [[CycleRoot]].
    fn finish_scc(&mut self, key: &ModuleKey, stack: &mut Vec<ModuleKey>) {
        let (di, danc, on_stack) = {
            let r = &self.module_recs[key];
            (r.dfs_index, r.dfs_anc, r.on_stack)
        };
        if !on_stack {
            return;
        }
        let Some(di) = di else { return };
        if danc != di {
            return;
        }
        while let Some(m) = stack.pop() {
            let rec = self.module_recs.get_mut(&m).unwrap();
            rec.on_stack = false;
            rec.cycle_root = Some(key.clone());
            if &m == key {
                break;
            }
        }
    }

    /// Execute `key`'s own body (all dependencies done): a TLA body parks in a coroutine whose
    /// completion runs the ancestor cascade; a synchronous body runs now.
    fn module_execute_async(&mut self, key: &ModuleKey) {
        let top = self.module_recs[key].top_promise.clone();
        let (body, env, meta, src) = {
            let rec = &self.module_recs[key];
            (
                rec.body.clone(),
                rec.env.clone(),
                rec.meta.clone(),
                rec.src.clone(),
            )
        };
        if body_has_tla(&body) {
            let resume_src = src.clone();
            self.module_recs.get_mut(key).unwrap().evaluating = true;
            let module_key = key.clone();
            let module_settings = Gc::as_ptr(&self.global) as usize;
            let closure: Box<dyn FnOnce(&mut Interp) -> crate::coroutine::Suspend> =
                Box::new(move |i| {
                    let saved_meta = i.import_meta.take();
                    let saved_strict = i.strict;
                    i.import_meta = Some(meta);
                    i.strict = true;
                    let result = i.run_stmt_list(&body, &env);
                    i.import_meta = saved_meta;
                    i.strict = saved_strict;
                    let completion = match result {
                        Ok(_) => {
                            i.finish_dynamic_module(&module_key, None);
                            crate::coroutine::Suspend::Done(Value::Undefined)
                        }
                        Err(a) => {
                            let v = crate::interpreter::abrupt_value(a);
                            i.finish_dynamic_module(&module_key, Some(v.clone()));
                            crate::coroutine::Suspend::Throw(v)
                        }
                    };
                    completion
                });
            let ptr = self as *mut Interp;
            let top = match top {
                Some(t) => t,
                None => self.new_promise(),
            };
            let mut coro =
                match crate::coroutine::spawn_coroutine(ptr, crate::coroutine::SendBody(closure)) {
                    Ok(c) => c,
                    Err(_) => {
                        let e = self.make_error("Error", crate::coroutine::UNSUPPORTED_MSG);
                        self.finish_dynamic_module(key, Some(e.clone()));
                        self.reject_promise(&top, e);
                        return;
                    }
                };
            // Every step of the body, the first included, runs under the module's frame.
            coro.set_frame(Interp::resume_frame_script(resume_src,module_settings));
            self.park_async_coro(&top, coro);
            self.drive_async(top, crate::coroutine::Resume::Next(Value::Undefined));
            return;
        }
        let saved_meta = self.import_meta.take();
        let saved_strict = self.strict;
        self.import_meta = Some(meta);
        self.strict = true;
        self.module_recs.get_mut(key).unwrap().evaluating = true;
        let result = self.with_script_frame(src, false, |i| i.run_stmt_list(&body, &env));
        self.import_meta = saved_meta;
        self.strict = saved_strict;
        self.module_recs.get_mut(key).unwrap().evaluating = false;
        match result {
            Ok(_) => {
                self.module_recs.get_mut(key).unwrap().evaluated = true;
                self.retire_module_body(key);
                if let Some(t) = self.module_recs[key].top_promise.clone() {
                    self.resolve_promise(&t, Value::Undefined);
                }
            }
            Err(a) => {
                let v = crate::interpreter::abrupt_value(a);
                self.async_module_rejected(key, v);
            }
        }
    }

    /// AsyncModuleExecutionFulfilled: run every ancestor whose pending count reaches zero, in
    /// [[AsyncEvaluationOrder]] (ascending).
    pub(crate) fn async_module_fulfilled(&mut self, key: &ModuleKey) {
        // Spec step 7: the fulfilled module's own capability resolves before any ancestor
        // executes — leaf-to-root fulfilment order is observable through dynamic import.
        if let Some(t) = self.module_recs[key].top_promise.clone() {
            self.resolve_promise(&t, Value::Undefined);
        }
        let mut exec: Vec<(u64, ModuleKey)> = Vec::new();
        self.gather_available_ancestors(key, &mut exec);
        exec.sort_by_key(|(o, _)| *o);
        for (_, m) in exec {
            if self.module_recs[&m].evaluated || self.module_recs[&m].eval_error.is_some() {
                continue;
            }
            self.module_execute_async(&m);
        }
    }

    fn gather_available_ancestors(&mut self, key: &ModuleKey, out: &mut Vec<(u64, ModuleKey)>) {
        let parents = self.module_recs[key].async_parents.clone();
        for parent in parents {
            let (done, err, pending) = {
                let p = &self.module_recs[&parent];
                (p.evaluated, p.eval_error.is_some(), p.pending_async)
            };
            if done || err || pending == 0 {
                continue;
            }
            let p = self.module_recs.get_mut(&parent).unwrap();
            p.pending_async -= 1;
            if p.pending_async == 0 {
                let order = p.async_order.unwrap_or(u64::MAX);
                out.push((order, parent.clone()));
                // A synchronous ancestor completes as part of this cascade, so its own parents
                // become available too (GatherAvailableAncestors' recursion).
                if !self.module_recs[&parent].has_tla {
                    self.gather_available_ancestors(&parent, out);
                }
            }
        }
    }

    /// AsyncModuleExecutionRejected: the error propagates to every waiting ancestor.
    pub(crate) fn async_module_rejected(&mut self, key: &ModuleKey, err: Value) {
        {
            let rec = self.module_recs.get_mut(key).unwrap();
            if rec.evaluated || rec.eval_error.is_some() {
                return;
            }
            rec.eval_error = Some(err.clone());
            rec.evaluated = true;
            rec.evaluating = false;
        }
        self.retire_module_body(key);
        if let Some(t) = self.module_recs[key].top_promise.clone() {
            self.reject_promise(&t, err.clone());
        }
        for parent in self.module_recs[key].async_parents.clone() {
            self.async_module_rejected(&parent, err.clone());
        }
    }

    /// Evaluate every module with top-level await (plus its own dependencies) in `key`'s graph —
    /// the eager part of an `import defer`.
    fn evaluate_async_subgraph(&mut self, key: &ModuleKey, seen: &mut Vec<ModuleKey>) -> Result<(), Abrupt> {
        if seen.iter().any(|k| k == key) {
            return Ok(());
        }
        seen.push(key.clone());
        let rec = match self.module_recs.get(key) {
            Some(r) => r,
            None => return Ok(()),
        };
        if rec.evaluated || rec.evaluating {
            return Ok(());
        }
        if rec.has_tla {
            // The async module (and its own graph) evaluates through the batch machinery — the
            // deferring parent does NOT wait on it.
            let mut graph = Vec::new();
            self.collect_eager_graph(key, &mut graph);
            for m in &graph {
                if !self.module_recs[m].evaluated && self.module_recs[m].top_promise.is_none() {
                    let p = self.new_promise();
                    self.module_recs.get_mut(m).unwrap().top_promise = Some(p);
                }
            }
            self.inner_module_evaluation_async(key, &mut Vec::new(), &mut 0);
            return Ok(());
        }
        let deps = rec.dep_keys.clone();
        let deferred = rec.deferred_deps.clone();
        for dep in deps {
            if deferred.contains(&dep) {
                continue;
            }
            self.evaluate_async_subgraph(&dep, seen)?;
        }
        Ok(())
    }

    // --- Namespace exotic-object behaviour ----------------------------------------------------

    /// Whether `ptr` (an object pointer) is a module namespace exotic object.
    pub(crate) fn is_namespace(&self, ptr: usize) -> bool {
        self.module_ns.contains_key(&ptr)
    }

    /// A module namespace's `[[GetOwnProperty]]` for a string key: `Some(property)` with the export's
    /// *current* value (writable, enumerable, non-configurable) if `key` is an exported name, or
    /// `Some(Err(ReferenceError))` if the underlying binding is still uninitialized. `None` means the
    /// key is not an export (the caller falls back to ordinary lookup — e.g. `@@toStringTag`).
    pub(crate) fn namespace_own_property(
        &mut self,
        ptr: usize,
        key: &str,
    ) -> Option<Result<Property, Abrupt>> {
        let binding = self.module_ns.get(&ptr)?.get(key)?.clone();
        let value = match binding {
            NsBinding::Live(env, local) => match self.get_var(&local, &env) {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            },
            NsBinding::Static(v) => v,
        };
        Some(Ok(Property::data(value, true, true, false)))
    }

    /// `import(specifier)`: synchronously load the module and return an already-resolved promise of
    /// its namespace (or a rejected promise if loading throws).
    pub(crate) fn dynamic_import(
        &mut self,
        specifier: &str,
        attr_type: Option<&str>,
        defer: bool,
        referrer: Option<Value>,
    ) -> Value {
        let promise = self.new_promise();
        let settings_key=Gc::as_ptr(&self.global) as usize;
        if attr_type==Some("css") && self.module_fetch_loaders.contains_key(&settings_key)
            && !self.module_synthetic_factories.get(&settings_key).is_some_and(|factories|factories.contains_key("css")) {
            let error=crate::interpreter::abrupt_value(self.throw("TypeError","CSS modules are not exposed in these settings"));
            self.reject_promise(&promise,error);
            return promise;
        }
        // The referrer is the importing module's `import.meta.url`. Prefer the value captured
        // lexically at the call site (passed in) — it survives `await`, unlike `self.import_meta`,
        // which is only set during a module's synchronous body.
        let meta = referrer.or_else(|| self.import_meta.clone());
        let referrer = match meta {
            Some(m) => match self.get_member(&m, "url") {
                Ok(Value::Str(s)) => self.current_classic_script_context().map_or_else(||s.to_string(),|context|context.base_url.clone()),
                _ => self.current_classic_script_context().map_or_else(|| self.module_api_base_for_host(), |context| context.base_url.clone()),
            },
            None => self.current_classic_script_context().map_or_else(|| self.module_api_base_for_host(), |context| context.base_url.clone()),
        };
        let resolution = match self.resolve_module_specifier_for_host(specifier,&referrer) {
            Ok(resolution)=>resolution,
            Err(error)=>{ self.reject_promise(&promise,crate::interpreter::abrupt_value(error)); return promise; }
        };
        if let Some(handler) = self.async_module_import_handlers.get(&(Gc::as_ptr(&self.global) as usize)).cloned() {
            let id = self.next_async_module_import_id;
            self.next_async_module_import_id = id.wrapping_add(1).max(1);
            let request = crate::AsyncModuleImportRequest {
                id,
                resolution,
                settings_key: Gc::as_ptr(&self.global) as usize,
                specifier: specifier.to_owned(),
                referrer,
                attribute_type: attr_type.map(str::to_owned),
                defer,
                script_context: self.current_classic_script_context(),
            };
            self.pending_async_module_imports
                .insert(id, (request.clone(), promise.clone(), self.global.clone()));
            handler(request);
            return promise;
        }
        let previous_context = self.classic_script_context.clone();
        self.classic_script_context = self.current_classic_script_context();
        let result = (|| {
            let fetched = self.fetch_module(specifier, &referrer, attr_type)?;
            self.classic_script_context = fetched.script_context;
            let canon = fetched.key;
            let src = fetched.source;
            let canon = ModuleKey::typed(canon, attr_type);
            let src = typed_module_source(src, attr_type);
            let module_url = self.classic_script_context.as_ref().map_or_else(||canon.to_string(), |context|context.base_url.clone());
            self.parse_and_link_at(&canon, Some(src), &module_url)?;
            Ok(canon)
        })();
        self.classic_script_context = previous_context;
        match result {
            Ok(canon) if defer => {
                // import.defer: link only; resolve with the (shared) deferred namespace. The
                // async subgraph still evaluates eagerly.
                let r = self.evaluate_async_subgraph(&canon, &mut Vec::new());
                match r {
                    Ok(()) => {
                        let dns = self.make_deferred_ns(&canon);
                        self.resolve_promise(&promise, dns);
                    }
                    Err(e) => {
                        let reason = crate::interpreter::abrupt_value(e);
                        self.reject_promise(&promise, reason);
                    }
                }
            }
            Ok(canon) => {
                // Evaluation may suspend at a top-level await: chain the import promise onto the
                // module's evaluation promise, resolving with the namespace.
                let top = self.evaluate_module_dynamic(&canon);
                let ns = self.module_recs[&canon].ns.clone();
                let on_f =
                    make_bound_len(self, dynamic_import_fulfil, vec![promise.clone(), ns], 1.0);
                let on_r = make_bound_len(self, dynamic_import_reject, vec![promise.clone()], 1.0);
                self.promise_then(&top, on_f, on_r);
            }
            Err(e) => {
                let reason = crate::interpreter::abrupt_value(e);
                self.reject_promise(&promise, reason);
            }
        }
        promise
    }

    /// Finish a dynamic import whose host fetch was completed asynchronously.
    /// The host loader is a prepared, in-memory graph reader, so linking still
    /// runs through the canonical module parser and graph cache here.
    #[cfg(feature = "embed")]
    pub(crate) fn complete_async_module_import(
        &mut self,
        id: u64,
        loader: impl Fn(&str, &str, Option<&str>) -> Option<(String, String)> + 'static,
    ) -> Result<(Value, Value), String> {
        let Some((request, import_promise, owner)) = self.pending_async_module_imports.remove(&id) else {
            return Err("unknown or already completed async module import".into());
        };
        let saved = self.snapshot_realm();
        if request.settings_key != Gc::as_ptr(&self.global) as usize {
            let realm = self.realms.get(&request.settings_key)
                .map(crate::interpreter::RealmState::snapshot_clone)
                .ok_or_else(|| "async module import settings no longer exist".to_string())?;
            self.restore_realm(&realm);
        }
        // Hold the actual owner throughout linking/evaluation, even if a host
        // completion arrived while another document was active.
        let _owner = owner;
        let previous_loader = self.module_loader.replace(Rc::new(loader));
        let previous_context = std::mem::replace(&mut self.classic_script_context, request.script_context.clone());
        let result = (|| -> Result<(Value, Value), Abrupt> {
            let fetched = self.fetch_module(
                &request.specifier,
                &request.referrer,
                request.attribute_type.as_deref(),
            )?;
            self.classic_script_context = fetched.script_context;
            let canon = fetched.key;
            let source = fetched.source;
            let canon = ModuleKey::typed(canon, request.attribute_type.as_deref());
            let source = typed_module_source(source, request.attribute_type.as_deref());
            let module_url = self.classic_script_context.as_ref().map_or_else(||canon.to_string(), |context|context.base_url.clone());
            self.parse_and_link_at(&canon, Some(source), &module_url)?;
            if request.defer {
                self.evaluate_async_subgraph(&canon, &mut Vec::new())?;
                let namespace = self.make_deferred_ns(&canon);
                self.resolve_promise(&import_promise, namespace.clone());
                return Ok((namespace, import_promise.clone()));
            }
            let evaluation = self.evaluate_module_dynamic(&canon);
            let namespace = self.module_recs[&canon].ns.clone();
            let on_f = make_bound_len(
                self,
                dynamic_import_fulfil,
                vec![import_promise.clone(), namespace.clone()],
                1.0,
            );
            let on_r = make_bound_len(
                self,
                dynamic_import_reject,
                vec![import_promise.clone()],
                1.0,
            );
            self.promise_then(&evaluation, on_f, on_r);
            Ok((namespace, evaluation))
        })();
        self.classic_script_context = previous_context;
        self.module_loader = previous_loader;
        let result = match result {
            Ok(pair) => Ok(pair),
            Err(error) => {
                let reason = crate::interpreter::abrupt_value(error);
                self.reject_promise(&import_promise, reason);
                Err("async dynamic module import failed to link".into())
            }
        };
        self.restore_realm(&saved);
        result
    }

    #[cfg(feature = "embed")]
    pub(crate) fn reject_async_module_import(
        &mut self,
        id: u64,
        message: &str,
    ) -> Result<(), String> {
        let Some((request, promise, _owner)) = self.pending_async_module_imports.remove(&id) else {
            return Err("unknown or already completed async module import".into());
        };
        let saved = self.snapshot_realm();
        if request.settings_key != Gc::as_ptr(&self.global) as usize {
            let realm = self.realms.get(&request.settings_key)
                .map(crate::interpreter::RealmState::snapshot_clone)
                .ok_or_else(|| "async module import settings no longer exist".to_string())?;
            self.restore_realm(&realm);
        }
        let reason = crate::interpreter::abrupt_value(self.throw("TypeError", message));
        self.reject_promise(&promise, reason);
        self.restore_realm(&saved);
        Ok(())
    }

    /// Evaluate a dynamically-imported module, returning a promise that settles when its body
    /// (which may use top-level await) completes. Dependencies evaluate synchronously first; the
    /// module's own body runs in a coroutine so a top-level await parks it.
    fn evaluate_module_dynamic(&mut self, key: &ModuleKey) -> Value {
        // Evaluate() step: an evaluating-async or evaluated module defers to its [[CycleRoot]],
        // so a member that finished before its cycle failed reports the cycle's error.
        let key = {
            let rec = &self.module_recs[key];
            match &rec.cycle_root {
                Some(root) if rec.evaluated || rec.async_order.is_some() => root.clone(),
                _ => key.clone(),
            }
        };
        let key = &key;
        let (top, err, settled) = {
            let rec = &self.module_recs[key];
            (
                rec.top_promise.clone(),
                rec.eval_error.clone(),
                rec.evaluated || rec.evaluating,
            )
        };
        // An existing top promise means the module belongs to an in-flight batch (or already
        // finished): the batch's DFS will execute it in order, so wait rather than preempt.
        if let Some(top) = top {
            return top;
        }
        if let Some(err) = err {
            let p = self.new_promise();
            self.reject_promise(&p, err);
            return p;
        }
        if settled {
            let p = self.new_promise();
            self.resolve_promise(&p, Value::Undefined);
            return p;
        }
        // Same batch machinery as a top-level Evaluate(): pre-create the graph's promises and
        // run InnerModuleEvaluation (awaits park; siblings continue; ancestors cascade).
        let mut graph = Vec::new();
        self.collect_eager_graph(key, &mut graph);
        for m in &graph {
            if !self.module_recs[m].evaluated && self.module_recs[m].top_promise.is_none() {
                let p = self.new_promise();
                self.module_recs.get_mut(m).unwrap().top_promise = Some(p);
            }
        }
        self.inner_module_evaluation_async(key, &mut Vec::new(), &mut 0);
        self.module_recs[key]
            .top_promise
            .clone()
            .unwrap_or_else(|| {
                let p = self.new_promise();
                match self.module_recs[key].eval_error.clone() {
                    Some(e) => self.reject_promise(&p, e),
                    None => self.resolve_promise(&p, Value::Undefined),
                }
                p
            })
    }

    /// Record a dynamically-evaluated module's completion (run from inside its coroutine).
    fn finish_dynamic_module(&mut self, key: &ModuleKey, error: Option<Value>) {
        match error {
            None => {
                if let Some(rec) = self.module_recs.get_mut(key) {
                    rec.evaluated = true;
                    rec.evaluating = false;
                }
                self.retire_module_body(key);
                // The module's own promise settles first (the coroutine driver resolves it when
                // this returns), then ancestors execute in [[AsyncEvaluationOrder]].
                self.async_module_fulfilled(key);
            }
            Some(err) => {
                if let Some(rec) = self.module_recs.get_mut(key) {
                    rec.evaluated = false; // async_module_rejected records the error
                }
                self.async_module_rejected(key, err);
            }
        }
    }
}

/// Whether two export resolutions denote the same binding (so a name re-exported by two star paths
/// is not ambiguous).
fn same_binding(a: &Resolution, b: &Resolution) -> bool {
    match (a, b) {
        (Resolution::Local(e1, l1), Resolution::Local(e2, l2)) => Rc::ptr_eq(e1, e2) && l1 == l2,
        (Resolution::Ns(Value::Obj(o1)), Resolution::Ns(Value::Obj(o2))) => Gc::ptr_eq(o1, o2),
        (Resolution::DeferNs(d1), Resolution::DeferNs(d2)) => d1 == d2,
        _ => false,
    }
}

/// The `resolved`-map key for a dependency: the plain specifier, or — for an attribute import
/// (`with { type: ... }`) — the specifier qualified by the type, so the same file imported both
/// ways in one module resolves to two distinct records, without encoding types
/// into URL fragments or specifier strings.
pub(crate) fn dep_map_key(spec: &str, attr: Option<&str>) -> ModuleKey {
    ModuleKey::typed(spec.to_owned(), attr)
}

/// Every module specifier this body imports/re-exports from (with duplicates).
pub(crate) fn module_dependency_specifiers(body: &[Stmt]) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for stmt in body {
        match stmt {
            Stmt::Import(decl) => out.push((decl.source.to_string(), decl.attr_type.clone())),
            Stmt::ExportNamed {
                source: Some(src), ..
            }
            | Stmt::ExportAll { source: src, .. } => out.push((src.to_string(), None)),
            _ => {}
        }
    }
    out
}

/// Reaction for a dynamic import: the module evaluated — resolve the import promise (`args[0]`)
/// with the namespace (`args[1]`).
fn dynamic_import_fulfil(i: &mut Interp, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let p = args.first().cloned().unwrap_or(Value::Undefined);
    let ns = args.get(1).cloned().unwrap_or(Value::Undefined);
    i.resolve_promise(&p, ns);
    Ok(Value::Undefined)
}

/// Reaction for a dynamic import: evaluation failed — reject the import promise (`args[0]`) with
/// the reason (`args[1]`).
fn dynamic_import_reject(i: &mut Interp, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let p = args.first().cloned().unwrap_or(Value::Undefined);
    let reason = args.get(1).cloned().unwrap_or(Value::Undefined);
    i.reject_promise(&p, reason);
    Ok(Value::Undefined)
}

/// HTML synthetic defaults retain raw JSON/text/CSS until canonical record
/// creation. Node's legacy byte module still uses its typed-array wrapper.
pub(crate) fn typed_module_source(text: String, attr_type: Option<&str>) -> String {
    match attr_type {
        Some("json" | "text") => text,
        Some("bytes") => {
            // The text was decoded latin-1 style (char == byte) when non-UTF-8; a UTF-8 source
            // re-encodes to its original bytes either way.
            let bytes: Vec<String> = if text.chars().all(|c| (c as u32) < 0x100) {
                text.chars().map(|c| (c as u32).to_string()).collect()
            } else {
                text.bytes().map(|b| b.to_string()).collect()
            };
            format!(
                "export default new Uint8Array(new Uint8Array([{}]).buffer.sliceToImmutable());",
                bytes.join(",")
            )
        }
        _ => text,
    }
}

/// Whether a module body contains top-level `await` (a for-await head, an `await using`, or an
/// Await expression outside any function/class body) — the async-module test for `import defer`.
pub(crate) fn body_has_tla(body: &[Stmt]) -> bool {
    fn stmt(s: &Stmt) -> bool {
        match s {
            Stmt::Expr(e) | Stmt::Throw(e) => expr(e),
            Stmt::Return(v) => v.as_ref().map(expr).unwrap_or(false),
            Stmt::VarDecl { kind, decls } => {
                matches!(kind, crate::ast::DeclKind::AwaitUsing)
                    || decls
                        .iter()
                        .any(|(_, init)| init.as_ref().map(expr).unwrap_or(false))
            }
            Stmt::If { test, cons, alt } => {
                expr(test) || stmt(cons) || alt.as_ref().map(|a| stmt(a)).unwrap_or(false)
            }
            Stmt::Block(b) => b.iter().any(stmt),
            Stmt::While { test, body } | Stmt::DoWhile { body, test } => expr(test) || stmt(body),
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                init.as_ref()
                    .map(|i| match i.as_ref() {
                        crate::ast::ForInit::VarDecl { decls, .. } => decls
                            .iter()
                            .any(|(_, e)| e.as_ref().map(expr).unwrap_or(false)),
                        crate::ast::ForInit::Expr(e) => expr(e),
                    })
                    .unwrap_or(false)
                    || test.as_deref().map(expr).unwrap_or(false)
                    || update.as_deref().map(expr).unwrap_or(false)
                    || stmt(body)
            }
            Stmt::ForInOf {
                right,
                body,
                is_await,
                ..
            } => *is_await || expr(right) || stmt(body),
            Stmt::Try {
                block,
                handler,
                finalizer,
                ..
            } => {
                block.iter().any(stmt)
                    || handler
                        .as_ref()
                        .map(|h| &h.1)
                        .map(|h| h.iter().any(stmt))
                        .unwrap_or(false)
                    || finalizer
                        .as_ref()
                        .map(|f| f.iter().any(stmt))
                        .unwrap_or(false)
            }
            Stmt::Switch { disc, cases } => {
                expr(disc)
                    || cases.iter().any(|c| {
                        c.test.as_ref().map(expr).unwrap_or(false) || c.body.iter().any(stmt)
                    })
            }
            Stmt::Labeled { body, .. } => stmt(body),
            Stmt::With { obj, body } => expr(obj) || stmt(body),
            Stmt::ExportDefault(inner) | Stmt::ExportDecl(inner) => stmt(inner),
            _ => false,
        }
    }
    fn expr(e: &Expr) -> bool {
        match e {
            Expr::Await(_) => true,
            Expr::ToStr(x) | Expr::OptionalChain(x) => expr(x),
            Expr::Unary { arg, .. } | Expr::Update { arg, .. } => expr(arg),
            Expr::Binary { left, right, .. }
            | Expr::Logical { left, right, .. }
            | Expr::Assign {
                target: left,
                value: right,
                ..
            } => expr(left) || expr(right),
            Expr::Cond { test, cons, alt } => expr(test) || expr(cons) || expr(alt),
            Expr::Call { callee, args, .. } => {
                expr(callee)
                    || args.iter().any(|a| match a {
                        crate::ast::ArrayElem::Item(e) | crate::ast::ArrayElem::Spread(e) => {
                            expr(e)
                        }
                        _ => false,
                    })
            }
            Expr::New { callee, args, .. } => {
                expr(callee)
                    || args.iter().any(|a| match a {
                        crate::ast::ArrayElem::Item(e) | crate::ast::ArrayElem::Spread(e) => {
                            expr(e)
                        }
                        _ => false,
                    })
            }
            Expr::Member { obj, .. } => expr(obj),
            Expr::Index { obj, index, .. } => expr(obj) || expr(index),
            Expr::Seq(v) => v.iter().any(expr),
            Expr::Array(elems) => elems.iter().any(|a| match a {
                crate::ast::ArrayElem::Item(e) | crate::ast::ArrayElem::Spread(e) => expr(e),
                _ => false,
            }),
            Expr::Object(props) => props.iter().any(|p| match p {
                crate::ast::PropDef::KeyValue { value, .. } => expr(value),
                crate::ast::PropDef::Spread(e) => expr(e),
                _ => false,
            }),
            Expr::Yield { arg, .. } => arg.as_ref().map(|a| expr(a)).unwrap_or(false),
            Expr::TaggedTemplate { tag, .. } => expr(tag),
            Expr::ImportCall { spec, options, .. } => {
                expr(spec) || options.as_ref().map(|o| expr(o)).unwrap_or(false)
            }
            _ => false,
        }
    }
    body.iter().any(stmt)
}

/// A module's parsed export/import tables.
struct ExportTables {
    local_exports: HashMap<String, String>,
    imports: HashMap<String, ImportOrigin>,
    indirect: HashMap<String, (ModuleKey, String)>,
    star_as: HashMap<String, ModuleKey>,
    stars: Vec<ModuleKey>,
}

/// Build a module's export/import tables from its parsed body and resolved specifier→key map.
fn build_export_tables(body: &[Stmt], resolved: &HashMap<ModuleKey, ModuleKey>) -> ExportTables {
    let mut local_exports: HashMap<String, String> = HashMap::new();
    let mut imports: HashMap<String, ImportOrigin> = HashMap::new();
    let mut indirect: HashMap<String, (ModuleKey, String)> = HashMap::new();
    let mut star_as: HashMap<String, ModuleKey> = HashMap::new();
    let mut stars: Vec<ModuleKey> = Vec::new();
    let key_of = |src: &ModuleKey| {
        resolved
            .get(src)
            .cloned()
            .unwrap_or_else(|| src.clone())
    };

    for stmt in body {
        match stmt {
            Stmt::Import(decl) => {
                let dep = key_of(&dep_map_key(&decl.source, decl.attr_type.as_deref()));
                for spec in &decl.specs {
                    match spec {
                        ImportSpec::Namespace(local) => {
                            imports.insert(local.clone(), ImportOrigin::Namespace(dep.clone()));
                        }
                        ImportSpec::DeferNamespace(local) => {
                            imports
                                .insert(local.clone(), ImportOrigin::DeferNamespace(dep.clone()));
                        }
                        ImportSpec::Source(local) => {
                            imports.insert(
                                local.clone(),
                                ImportOrigin::Named(dep.clone(), "~source~".to_string()),
                            );
                        }
                        ImportSpec::Default(local) => {
                            imports.insert(
                                local.clone(),
                                ImportOrigin::Named(dep.clone(), "default".to_string()),
                            );
                        }
                        ImportSpec::Named { imported, local } => {
                            imports.insert(
                                local.clone(),
                                ImportOrigin::Named(dep.clone(), imported.clone()),
                            );
                        }
                    }
                }
            }
            Stmt::ExportDecl(inner) => {
                for name in exported_decl_names(inner) {
                    local_exports.insert(name.clone(), name);
                }
            }
            Stmt::ExportDefault(inner) => {
                let local = match &**inner {
                    Stmt::FuncDecl(f) if f.name.is_some() => f.name.clone().unwrap(),
                    Stmt::ClassDecl(c) if c.name.is_some() => c.name.clone().unwrap(),
                    _ => "*default*".to_string(),
                };
                local_exports.insert("default".to_string(), local);
            }
            Stmt::ExportNamed { specs, source } => {
                for spec in specs {
                    match source {
                        Some(src) => {
                            indirect
                                .insert(spec.exported.clone(), (key_of(&ModuleKey::from(src.as_ref())), spec.local.clone()));
                        }
                        None => {
                            local_exports.insert(spec.exported.clone(), spec.local.clone());
                        }
                    }
                }
            }
            Stmt::ExportAll { source, exported } => match exported {
                Some(name) => {
                    star_as.insert(name.clone(), key_of(&ModuleKey::from(source.as_ref())));
                }
                None => stars.push(key_of(&ModuleKey::from(source.as_ref()))),
            },
            _ => {}
        }
    }
    ExportTables {
        local_exports,
        imports,
        indirect,
        star_as,
        stars,
    }
}

/// Names introduced by an `export <decl>` statement's inner declaration.
fn exported_decl_names(inner: &Stmt) -> Vec<String> {
    match inner {
        Stmt::VarDecl { decls, .. } => {
            let mut out = Vec::new();
            for (pat, _) in decls {
                crate::interpreter::pattern_idents(pat, &mut out);
            }
            out
        }
        Stmt::FuncDecl(f) => f.name.clone().into_iter().collect(),
        Stmt::ClassDecl(c) => c.name.clone().into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Module-load counters (tooling: where a program's startup goes). Each is (nanoseconds, count,
/// bytes) accumulated process-wide.
#[doc(hidden)]
pub mod load_stats {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    pub struct Counter(pub AtomicU64, pub AtomicU64, pub AtomicU64);
    pub static PARSE: Counter = Counter(AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    pub static FETCH: Counter = Counter(AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    pub(crate) fn add(c: &Counter, t: std::time::Instant, bytes: usize) {
        c.0.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
        c.1.fetch_add(1, Relaxed);
        c.2.fetch_add(bytes as u64, Relaxed);
    }
    pub fn get(c: &Counter) -> (f64, u64, u64) {
        (
            c.0.load(Relaxed) as f64 / 1e6,
            c.1.load(Relaxed),
            c.2.load(Relaxed),
        )
    }
}

/// A module key as the `file://` URL Node names an ES module by (`C:\a\b.mts` →
/// `file:///C:/a/b.mts`); a key that already has a scheme is returned as is.
fn file_url(key: &str) -> String {
    let has_scheme = key
        .split_once(':')
        .is_some_and(|(s, _)| s.len() > 1 && s.chars().all(|c| c.is_ascii_alphanumeric()));
    if has_scheme {
        return key.to_string();
    }
    let path = key.strip_prefix(r"\\?\").unwrap_or(key).replace('\\', "/");
    if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    }
}

#[cfg(test)]
mod body_retirement_tests {
    use super::*;

    #[test]
    fn specification_module_identity_keeps_settings_types_and_url_fragments_distinct() {
        let mut map = SettingsModuleMap::default();
        let plain = ModuleKey::from("https://example.test/data#json");
        let json = ModuleKey::typed("https://example.test/data".into(), Some("json"));
        let text = ModuleKey::typed("https://example.test/data".into(), Some("text"));
        let bytes = ModuleKey::typed("https://example.test/data".into(), Some("bytes"));
        map.select(1);
        for (key,value) in [(plain.clone(),1),(json.clone(),2),(text.clone(),3),(bytes.clone(),4)] { map.insert(key,value); }
        assert_eq!(map.get(&plain),Some(&1));
        assert_eq!(map.get(&json),Some(&2));
        assert_eq!(map.get(&text),Some(&3));
        assert_eq!(map.get(&bytes),Some(&4));
        let empty_type = ModuleKey::typed("https://example.test/data#json".into(),Some(""));
        map.insert(empty_type.clone(),6);
        assert_eq!(map.get(&empty_type),Some(&6));
        assert_eq!(map.get(&plain),Some(&1),"even an unsupported empty type cannot alias JavaScript identity");
        map.select(2);
        assert!(map.get(&plain).is_none());
        map.insert(plain.clone(),5);
        map.select(1);
        assert_eq!(map.get(&plain),Some(&1));
        assert_eq!(map.remove_settings(2).expect("second settings").len(),1);

        let mut interp = Interp::new();
        let global=Value::Obj(interp.global.clone());
        let json_object=interp.get_member(&global,"JSON").ok().expect("JSON intrinsic");
        interp.set_member(&json_object,"parse",Value::Undefined).ok().expect("author replacement");
        interp.module_loader = Some(Rc::new(|specifier,_,kind| {
            let source = match (specifier,kind) {
                ("data#json",None) => "export default 11;",
                ("data",Some("json")) => "{\"value\":22}",
                ("data",Some("text")) => "text payload",
                ("data",Some("bytes")) => "ABC",
                _ => return None,
            };
            Some((specifier.into(),source.into()))
        }));
        interp.parse_and_register("root",Some("import plain from 'data#json'; import json from 'data' with {type:'json'}; import text from 'data' with {type:'text'}; import bytes from 'data' with {type:'bytes'}; export const value = plain + json.value + text.length + bytes.length;".into())).ok().expect("typed graph parses");
        let root = ModuleKey::from("root");
        interp.link_module(&root).ok().expect("typed graph links");
        interp.evaluate_module(&root).ok().expect("typed graph evaluates");
        let namespace = interp.module_recs[&root].ns.clone();
        assert_eq!(number(&mut interp,&namespace,"value"),48.0);
        assert_eq!(interp.module_recs.len(),5);
        let invalid=ModuleKey::typed("invalid.json".into(),Some("json"));
        let first=interp.parse_and_link_at(&invalid,Some("{invalid}".into()),"invalid.json").err().map(crate::interpreter::abrupt_value).expect("invalid JSON parse error");
        let second=interp.parse_and_link_at(&invalid,Some("{}".into()),"invalid.json").err().map(crate::interpreter::abrupt_value).expect("cached parse error");
        assert_eq!(first.object_identity(),second.object_identity(),"module parse exceptions retain their original identity");
        interp.module_loader = Some(Rc::new(|specifier,_,_| match specifier {
            "cycle-a"=>Some((specifier.into(),"import {readA} from 'cycle-b'; export const value = 7; export function read() { return readA(); }".into())),
            "cycle-b"=>Some((specifier.into(),"import {value} from 'cycle-a'; export function readA() { return value; }".into())),
            _=>None,
        }));
        interp.parse_and_register("cycle-a",Some("import {readA} from 'cycle-b'; export const value = 7; export function read() { return readA(); }".into())).ok().expect("cyclic graph parses");
        let cycle = ModuleKey::from("cycle-a");
        interp.link_module(&cycle).ok().expect("cyclic graph links");
        interp.evaluate_module(&cycle).ok().expect("cyclic graph evaluates");
        let namespace = interp.module_recs[&cycle].ns.clone();
        let read = interp.get_member(&namespace,"read").ok().expect("live cyclic export");
        assert!(matches!(interp.call(read,Value::Undefined,&[]),Ok(Value::Num(7.0))));
        assert_eq!(interp.module_recs.len(),8,"the back edge reuses the original graph identity, alongside the cached parse failure");
        let creations = Rc::new(std::cell::Cell::new(0));
        let calls = creations.clone();
        interp.install_module_synthetic_factory("css",Rc::new(move |_,source| {
            assert_eq!(source,"body { color: red; }");
            calls.set(calls.get()+1);
            Ok(Value::Num(41.0))
        }));
        interp.module_loader=Some(Rc::new(|specifier,_,kind| {
            (specifier == "sheet.css" && kind == Some("css")).then(||(specifier.into(),"body { color: red; }".into()))
        }));
        interp.parse_and_register("css-root",Some("import sheet from 'sheet.css' with {type:'css'}; export const value = sheet;".into())).ok().expect("native synthetic graph parses");
        let css_root=ModuleKey::from("css-root");
        interp.link_module(&css_root).ok().expect("synthetic graph links");
        interp.evaluate_module(&css_root).ok().expect("synthetic graph evaluates");
        let namespace=interp.module_recs[&css_root].ns.clone();
        assert_eq!(number(&mut interp,&namespace,"value"),41.0);
        assert_eq!(creations.get(),1,"a default export factory runs once per actual URL/type record");
    }

    fn number(interp: &mut Interp, namespace: &Value, name: &str) -> f64 {
        match interp.get_member(namespace, name).ok().expect("export") {
            Value::Num(value) => value,
            _ => panic!("numeric export required"),
        }
    }

    #[test]
    fn completed_body_is_released_but_exports_and_closures_remain_live() {
        let mut interp = Interp::new();
        interp
            .parse_and_register(
                "counter",
                Some("export let value = 3; export function inc() { return ++value; }".into()),
            )
            .ok()
            .unwrap();
        let body = Rc::downgrade(&interp.module_recs["counter"].body);
        interp.link_module(&ModuleKey::from("counter")).ok().unwrap();
        interp.evaluate_module(&ModuleKey::from("counter")).ok().unwrap();
        assert!(
            body.upgrade().is_none(),
            "initialization AST is no longer needed"
        );
        let namespace = interp.module_recs["counter"].ns.clone();
        let inc = interp.get_member(&namespace, "inc").ok().unwrap();
        assert!(matches!(
            interp.invoke(inc, Value::Undefined, &[]).ok(),
            Some(Value::Num(4.0))
        ));
        assert_eq!(number(&mut interp, &namespace, "value"), 4.0);
        interp.evaluate_module(&ModuleKey::from("counter")).ok().unwrap();
        assert_eq!(
            number(&mut interp, &namespace, "value"),
            4.0,
            "imports do not repeat initialization"
        );
    }

    #[test]
    fn top_level_await_retires_only_after_settlement_and_keeps_tla_metadata() {
        let mut interp = Interp::new();
        let source = "export const value = await Promise.resolve(7); export function read() { return value; }";
        interp
            .parse_and_register("awaited", Some(source.into()))
            .ok()
            .unwrap();
        let body = Rc::downgrade(&interp.module_recs["awaited"].body);
        assert!(body.upgrade().is_some());
        let namespace = interp.load_module("awaited", source).ok().unwrap();
        assert!(interp.module_recs["awaited"].evaluated);
        assert!(interp.module_recs["awaited"].has_tla);
        assert!(body.upgrade().is_none());
        assert_eq!(number(&mut interp, &namespace, "value"), 7.0);
        let read = interp.get_member(&namespace, "read").ok().unwrap();
        assert!(matches!(
            interp.invoke(read, Value::Undefined, &[]).ok(),
            Some(Value::Num(7.0))
        ));
        assert!(interp
            .load_module("awaited", "throw new Error('must not run')")
            .is_ok());
    }

    #[test]
    fn terminal_error_is_cached_after_body_retirement() {
        let mut interp = Interp::new();
        interp
            .parse_and_register(
                "failed",
                Some("throw new Error('original failure');".into()),
            )
            .ok()
            .unwrap();
        let body = Rc::downgrade(&interp.module_recs["failed"].body);
        interp.link_module(&ModuleKey::from("failed")).ok().unwrap();
        assert!(interp.evaluate_module(&ModuleKey::from("failed")).is_err());
        assert!(body.upgrade().is_none());
        let error = interp.module_recs["failed"].eval_error.clone().unwrap();
        match interp.evaluate_module(&ModuleKey::from("failed")) {
            Err(Abrupt::Throw(second)) => assert!(interp.values_strict_equal(&error, &second)),
            _ => panic!("must return the same cached error"),
        }
    }

    #[test]
    fn pending_await_keeps_its_body_until_the_exported_resolver_settles_it() {
        let mut interp = Interp::new();
        let source = "export let release; export const value = await new Promise(resolve => { release = resolve; });";
        interp
            .parse_and_register("pending", Some(source.into()))
            .ok()
            .unwrap();
        let body = Rc::downgrade(&interp.module_recs["pending"].body);
        let namespace = interp.load_module("pending", source).ok().unwrap();
        assert!(!interp.module_recs["pending"].evaluated);
        assert!(
            body.upgrade().is_some(),
            "a parked module still needs its statements"
        );
        let release = interp.get_member(&namespace, "release").ok().unwrap();
        interp
            .invoke(release, Value::Undefined, &[Value::Num(9.0)])
            .ok()
            .unwrap();
        interp.run_agent_event_loop();
        assert!(interp.module_recs["pending"].evaluated);
        assert!(body.upgrade().is_none());
        assert_eq!(number(&mut interp, &namespace, "value"), 9.0);
    }

    #[test]
    fn mixed_eager_and_deferred_imports_keep_graph_and_live_reexports() {
        let mut interp = Interp::new();
        interp.module_loader = Some(Rc::new(|specifier, _, _| match specifier {
            "dep" => Some((
                "dep".into(),
                "export let value = 3; export function inc() { return ++value; }".into(),
            )),
            _ => None,
        }));
        let namespace = interp.load_module("mixed", "import defer * as lazy from 'dep'; import { value, inc } from 'dep'; export { value, inc }; export function read() { return lazy.value; }").ok().unwrap();
        assert!(interp.module_recs["mixed"].body.is_empty());
        let mixed_key = ModuleKey::from("mixed");
        let dep_key = ModuleKey::from("dep");
        assert_eq!(interp.eager_named_deps(&mixed_key), vec![dep_key.clone()]);
        assert_eq!(interp.eager_dep_order(&mixed_key), vec![dep_key.clone()]);
        let mut graph = Vec::new();
        interp.collect_eager_graph(&mixed_key, &mut graph);
        assert_eq!(graph, vec![mixed_key, dep_key]);
        let inc = interp.get_member(&namespace, "inc").ok().unwrap();
        interp.invoke(inc, Value::Undefined, &[]).ok().unwrap();
        assert_eq!(number(&mut interp, &namespace, "value"), 4.0);
        let read = interp.get_member(&namespace, "read").ok().unwrap();
        assert!(matches!(
            interp.invoke(read, Value::Undefined, &[]).ok(),
            Some(Value::Num(4.0))
        ));
    }
}

#[cfg(test)]
mod namespace_lifetime_tests {
    use super::*;

    #[test]
    fn namespace_metadata_pins_until_collection_then_retires() {
        let mut interp = Interp::new();
        interp
            .parse_and_register("lifetime", Some("export const answer = 42;".into()))
            .ok()
            .expect("parse module");
        interp.link_module(&ModuleKey::from("lifetime")).ok().expect("link module");
        let ns = interp.module_recs["lifetime"].ns.clone();
        let ptr = Gc::as_ptr(ns.as_obj().unwrap()) as usize;
        assert!(interp.is_namespace(ptr));
        assert!(
            interp.gc_pins.contains_key(&ptr),
            "pointer metadata must keep its object alive until sweep"
        );
        interp.module_recs.remove("lifetime");
        interp.modules.remove("lifetime");
        drop(ns);
        interp.gc_collect();
        assert!(
            !interp.is_namespace(ptr),
            "dead namespace metadata must be evicted before address reuse"
        );
        assert!(!interp.gc_pins.contains_key(&ptr));
        let ordinary = Value::Obj(Object::new(None));
        interp.strict = true;
        assert!(interp
            .set_member(&ordinary, "PATH", Value::str("fixture"))
            .is_ok());
    }

    fn live_scopes() -> usize {
        crate::value::scope_registry_with(|registry| registry.len())
    }

    #[test]
    fn dead_modules_importing_each_other_are_collected() {
        let mut interp = Interp::new();
        interp.module_loader = Some(Rc::new(|specifier, _, _| match specifier {
            "a" => Some((
                "a".into(),
                "import { b } from 'b'; export const a = 1; export const readB = () => b;".into(),
            )),
            "b" => Some((
                "b".into(),
                "import { a } from 'a'; export const b = 2; export const readA = () => a;".into(),
            )),
            _ => None,
        }));
        interp.gc_collect();
        let base = live_scopes();
        let ns = interp
            .load_module(
                "a",
                "import { b } from 'b'; export const a = 1; export const readB = () => b;",
            )
            .ok()
            .expect("load cyclic graph");
        let read_b = interp.get_member(&ns, "readB").ok().unwrap();
        assert!(matches!(
            interp.invoke(read_b, Value::Undefined, &[]).ok(),
            Some(Value::Num(2.0))
        ));
        drop(ns);
        interp.gc_collect();
        assert!(live_scopes() > base, "registered modules stay live");
        interp.module_recs.clear();
        interp.modules.clear();
        for _ in 0..2 {
            interp.gc_collect();
        }
        assert_eq!(
            live_scopes(),
            base,
            "module scopes that import from each other must not outlive their records"
        );
    }

    #[test]
    fn retained_namespace_survives_collection_and_stays_read_only() {
        let mut interp = Interp::new();
        let ns = interp
            .load_module("retained", "export let answer = 42;")
            .ok()
            .expect("load module");
        let ptr = Gc::as_ptr(ns.as_obj().unwrap()) as usize;
        interp.gc_collect();
        assert!(interp.is_namespace(ptr));
        assert!(matches!(
            interp.get_member(&ns, "answer").ok().expect("read export"),
            Value::Num(42.0)
        ));
        interp.strict = true;
        assert!(interp.set_member(&ns, "answer", Value::Num(7.0)).is_err());
    }
}
