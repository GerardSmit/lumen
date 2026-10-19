//! HTML import-map normalization, registration and settings-local module resolution.
use std::collections::{BTreeMap,BTreeSet};
use std::sync::{Arc, Mutex};
use crate::{json, smuggle, url};

const MAX_ENTRIES: usize = 16_384;
const MAX_RESOLUTIONS: usize = 65_536;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_SOURCE: usize = 1024 * 1024;
type Specifiers = BTreeMap<String, Option<String>>;
/// Plain Rust state may be captured by the existing asynchronous module graph loader.
pub type SharedImportMap = Arc<Mutex<ImportMapState>>;
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error { Syntax(String), Type(String), Limit }
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self { Self::Syntax(s) | Self::Type(s) => f.write_str(s), Self::Limit => f.write_str("import-map resource limit exceeded") }
    }
}
impl std::error::Error for Error {}
#[derive(Default, Debug)]
pub struct ImportMap {
    imports: Specifiers,
    scopes: BTreeMap<String, Specifiers>,
    integrity: BTreeMap<String, String>,
}
#[derive(Default, Debug)]
pub struct ImportMapState {
    map: ImportMap,
    // Base-qualified records, deduplicated before admission. True means bare or special URL.
    resolved: BTreeMap<String, BTreeMap<String, bool>>,
    resolved_count: usize,
    resolved_specifiers: BTreeSet<Vec<u16>>,
    retained_bytes: usize,
    map_bytes: usize,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution { pub url: String, pub integrity: String }
fn type_error(s: &str) -> Error { Error::Type(s.into()) }
fn url_like(value: &str, base: &str) -> Option<url::Url> {
    let relative = value.starts_with('/') || value.starts_with("./") || value.starts_with("../");
    // URL parsing consumes scalar values; bare specifier keys retain their UTF-16 spelling.
    let value = scalar_text(value);
    url::parse(&value, relative.then_some(base)).ok()
}
pub fn resolve_without_map(specifier:&str,base:&str)->Result<Resolution,Error> {
    let url=url_like(specifier,base).ok_or_else(||type_error("unmapped bare module specifier"))?.href();
    Ok(Resolution {url,integrity:String::new()})
}
fn scalar_text(value: &str) -> std::borrow::Cow<'_, str> {
    if !smuggle::may_contain(value) { return std::borrow::Cow::Borrowed(value); }
    std::borrow::Cow::Owned(String::from_utf16_lossy(&smuggle::utf16_units(value)))
}
fn insert_last<V>(map: &mut BTreeMap<String, V>, key: String, value: V) -> Result<(), Error> {
    if map.len() >= MAX_ENTRIES && !map.contains_key(&key) { return Err(Error::Limit); }
    map.insert(key, value); Ok(())
}
fn object(value: &json::Value) -> Result<Vec<(&str, &json::Value)>, Error> {
    let json::Value::Obj(entries)=value else { return Err(type_error("import-map member must be a JSON object")); };
    if entries.len()>MAX_ENTRIES {return Err(Error::Limit);}
    // JSON.parse overwrites a duplicate's value without changing its first insertion position.
    let mut positions:BTreeMap<&str,usize>=BTreeMap::new();
    let mut result:Vec<(&str,&json::Value)>=Vec::with_capacity(entries.len());
    for (key,value) in entries {
        if let Some(&position)=positions.get(key.as_str()) { result[position]=(key.as_str(),value); }
        else {positions.insert(key.as_str(),result.len());result.push((key.as_str(),value));}
    }
    Ok(result)
}

fn specifiers(value: &json::Value, base: &str) -> Result<Specifiers, Error> {
    let mut result = BTreeMap::new();
    for (key, value) in object(value)? {
        if key.is_empty() { continue; }
        let normalized = url_like(key, base).map_or_else(|| key.to_owned(), |u| u.href());
        let address = value.as_str().and_then(|s|url_like(s, base)).map(|u|u.href())
            .filter(|address| !key.ends_with('/') || address.ends_with('/'));
        insert_last(&mut result, normalized, address)?;
    }
    Ok(result)
}
impl ImportMap {
    pub fn parse(source: &str, base: &str) -> Result<Self, Error> {
        if source.len() > MAX_SOURCE { return Err(Error::Limit); }
        let parsed = json::parse_with_spelling(source, smuggle::Spelling::Utf16)
            .map_err(|e|Error::Syntax(e.to_string()))?;
        object(&parsed)?;
        let mut result = Self::default();
        if let Some(value) = parsed.get("imports") { result.imports = specifiers(value, base)?; }
        if let Some(value) = parsed.get("scopes") {
            for (prefix, entries) in object(value)? {
                object(entries)?;
                if let Ok(scope) = url::parse(&scalar_text(prefix), Some(base)) {
                    insert_last(&mut result.scopes, scope.href(), specifiers(entries, base)?)?;
                }
            }
        }
        if let Some(value) = parsed.get("integrity") {
            for (key, value) in object(value)? {
                if let (Some(url), Some(integrity)) = (url_like(key, base), value.as_str()) {
                    insert_last(&mut result.integrity, url.href(), integrity.to_owned())?;
                }
            }
        }
        result.bytes()?;
        Ok(result)
    }
    fn bytes(&self) -> Result<usize, Error> {
        let mut bytes = std::mem::size_of::<Self>();
        let mut entries = 0usize;
        fn add(bytes: &mut usize, value: usize) -> Result<(), Error> {
            *bytes = bytes.checked_add(value).filter(|n|*n<=MAX_BYTES).ok_or(Error::Limit)?; Ok(())
        }
        for (scope_bytes, map) in std::iter::once((0, &self.imports)).chain(self.scopes.iter().map(|(s,m)|(s.capacity(),m))) {
            add(&mut bytes, scope_bytes)?;add(&mut bytes,128)?;
            for (key, address) in map {
                entries = entries.checked_add(1).filter(|n|*n<=MAX_ENTRIES).ok_or(Error::Limit)?;
                add(&mut bytes,key.capacity())?;add(&mut bytes,address.as_ref().map_or(0,String::capacity))?;add(&mut bytes,128)?;
            }
        }
        for (key, integrity) in &self.integrity {add(&mut bytes,key.capacity())?;add(&mut bytes,integrity.capacity())?;add(&mut bytes,128)?;}
        Ok(bytes)
    }
}
fn matching(map: &Specifiers, specifier: &str, prefix_allowed: bool) -> Result<Option<String>, Error> {
    if let Some(address) = map.get(specifier) {
        return address.clone().map(Some).ok_or_else(||type_error("module resolution blocked by a null import-map entry"));
    }
    if !prefix_allowed { return Ok(None); }
    // Only slash-terminated prefixes can match; walk the requested key once, longest first.
    for (at, byte) in specifier.bytes().enumerate().rev() {
        if byte != b'/' { continue; }
        let key = &specifier[..at+1];
        let Some(address) = map.get(key) else { continue; };
        let address = address.as_ref().ok_or_else(||type_error("module resolution blocked by a null import-map prefix"))?;
        let resolved = url::parse(&scalar_text(&specifier[at+1..]), Some(address))
            .map_err(|_|type_error("invalid import-map prefix remainder"))?.href();
        if !resolved.starts_with(address) { return Err(type_error("import-map prefix resolution backtracks above its mapped address")); }
        return Ok(Some(resolved));
    }
    Ok(None)
}
impl ImportMapState {
    fn globally_resolved_prefix(&self,units:&[u16])->bool {
        use std::ops::Bound::{Included,Unbounded};
        self.resolved_specifiers.range::<[u16],_>((Unbounded,Included(units)))
            .next_back().is_some_and(|prefix|units.starts_with(prefix))
    }
    pub fn shared() -> SharedImportMap { Arc::new(Mutex::new(Self::default())) }
    pub fn integrity(&self, url: &str) -> String { self.map.integrity.get(url).cloned().unwrap_or_default() }
    pub fn register(&mut self, mut incoming: ImportMap) -> Result<(), Error> {
        // Preflight the conservative maximum before mutating either map; discarded entries cost no persistent storage.
        let incoming_bytes = incoming.bytes()?;
        let map_bytes = self.map_bytes;
        if self.retained_bytes.checked_add(map_bytes).and_then(|n|n.checked_add(incoming_bytes)).and_then(|n|n.checked_add(std::mem::size_of::<Self>())).is_none_or(|n|n>MAX_BYTES) { return Err(Error::Limit); }
        for (scope, map) in &mut incoming.scopes {
            let conflicts=|key:&str,records:&BTreeMap<String,bool>|records.contains_key(key)
                || key.ends_with('/') && records.range(key.to_owned()..).take_while(|(s,_)|s.starts_with(key)).any(|(_,allowed)|*allowed);
            map.retain(|key,_| {
                if scope.ends_with('/') {
                    !self.resolved.range(scope.clone()..).take_while(|(base,_)|base.starts_with(scope.as_str())).any(|(_,records)|conflicts(key,records))
                } else { !self.resolved.get(scope).is_some_and(|records|conflicts(key,records)) }
            });
        }
        incoming.imports.retain(|key,_| {
            // HTML merge step 6 deliberately tests new key starts with already resolved specifier.
            !self.globally_resolved_prefix(&smuggle::utf16_units(key))
        });
        let new_entries = incoming.imports.len() + incoming.scopes.len() + incoming.integrity.len() + incoming.scopes.values().map(BTreeMap::len).sum::<usize>() + self.map.imports.len() + self.map.scopes.len() + self.map.integrity.len() + self.map.scopes.values().map(BTreeMap::len).sum::<usize>();
        if new_entries > MAX_ENTRIES { return Err(Error::Limit); }
        for (scope, entries) in incoming.scopes { let destination=self.map.scopes.entry(scope).or_default(); for (key,address) in entries { destination.entry(key).or_insert(address); } }
        for (key, address) in incoming.imports { self.map.imports.entry(key).or_insert(address); }
        for (url, integrity) in incoming.integrity { self.map.integrity.entry(url).or_insert(integrity); }
        self.map_bytes=self.map.bytes()?;
        Ok(())
    }
    pub fn resolve(&mut self, specifier: &str, base: &str) -> Result<Resolution, Error> {
        if specifier.len().checked_add(base.len()).is_none_or(|bytes|bytes>MAX_BYTES) {return Err(Error::Limit);}
        let as_url = url_like(specifier, base);
        let normalized = as_url.as_ref().map_or_else(||specifier.to_owned(),url::Url::href);
        let prefix_allowed = as_url.as_ref().is_none_or(url::Url::is_special);
        let mut result = if let Some(map)=self.map.scopes.get(base) { matching(map,&normalized,prefix_allowed)? } else { None };
        if result.is_none() {
            for (at,byte) in base.bytes().enumerate().rev() {
                if byte != b'/' || at+1==base.len() { continue; }
                if let Some(map)=self.map.scopes.get(&base[..at+1]) {
                    result=matching(map,&normalized,prefix_allowed)?;
                    if result.is_some() { break; }
                }
            }
        }
        if result.is_none() { result=matching(&self.map.imports,&normalized,prefix_allowed)?; }
        let url = result.or_else(||as_url.map(|u|u.href())).ok_or_else(||type_error("unmapped bare module specifier"))?;
        let exists=self.resolved.get(base).is_some_and(|entries|entries.contains_key(&normalized));
        if !exists {
            let estimate=base.len().checked_add(normalized.capacity()).and_then(|n|n.checked_add(normalized.len().checked_mul(2)?)).and_then(|n|n.checked_add(256)).ok_or(Error::Limit)?;
            if self.resolved_count>=MAX_RESOLUTIONS || self.retained_bytes.checked_add(estimate).and_then(|n|n.checked_add(self.map_bytes)).and_then(|n|n.checked_add(std::mem::size_of::<Self>())).is_none_or(|n|n>MAX_BYTES) { return Err(Error::Limit); }
            let units=smuggle::utf16_units(&normalized);
            let bytes=base.len().checked_add(normalized.capacity()).and_then(|n|n.checked_add(units.capacity().checked_mul(std::mem::size_of::<u16>())?)).and_then(|n|n.checked_add(256)).ok_or(Error::Limit)?;
            if self.retained_bytes.checked_add(bytes).and_then(|n|n.checked_add(self.map_bytes)).and_then(|n|n.checked_add(std::mem::size_of::<Self>())).is_none_or(|n|n>MAX_BYTES) {return Err(Error::Limit);}
            if !self.globally_resolved_prefix(&units) {
                let mut following=self.resolved_specifiers.split_off(&units);
                while following.first().is_some_and(|key|key.starts_with(&units)) {following.pop_first();}
                self.resolved_specifiers.append(&mut following);
                self.resolved_specifiers.insert(units);
            }
            self.resolved.entry(base.to_owned()).or_default().insert(normalized,prefix_allowed);
            self.resolved_count+=1; self.retained_bytes+=bytes;
        }
        Ok(Resolution { integrity:self.integrity(&url), url })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const BASE:&str="https://example.test/app/page.html";
    fn register(state:&mut ImportMapState,source:&str) {state.register(ImportMap::parse(source,BASE).unwrap()).unwrap();}
    #[test]
    fn specification_import_maps_normalization_scopes_blocks_and_prefix_backtracking() {
        let mut state=ImportMapState::default();
        register(&mut state,r#"{"imports":{"x":"./global.js","y":"./global-y.js","pkg/":"./pkg/","bad/":"./file.js","empty":false},"scopes":{"./":{"x":"./scope.js"},"./deep/":{"other":"./other.js"}}}"#);
        assert_eq!(state.resolve("x","https://example.test/app/deep/main.js").unwrap().url,"https://example.test/app/scope.js");
        assert_eq!(state.resolve("pkg/a.js",BASE).unwrap().url,"https://example.test/app/pkg/a.js");
        assert!(state.resolve("pkg/../escape.js",BASE).is_err());
        assert!(state.resolve("bad/a.js",BASE).is_err());
        assert!(state.resolve("empty",BASE).is_err());
        assert!(state.resolve("unmapped",BASE).is_err());
        assert_eq!(state.resolve("./relative.js",BASE).unwrap().url,"https://example.test/app/relative.js");
        register(&mut state,r#"{"scopes":{"./deep/":{"y":null}}}"#);
        assert!(state.resolve("y","https://example.test/app/deep/next.js").is_err(),"a null in a matching scope terminates global fallback");
    }
    #[test]
    fn specification_import_maps_registration_resolution_and_integrity_are_settings_owned() {
        let mut first=ImportMapState::default();let mut second=ImportMapState::default();
        register(&mut first,r#"{"imports":{"x":"./one.js"},"integrity":{"./one.js":"first"}}"#);
        assert_eq!(first.resolve("x",BASE).unwrap().integrity,"first");
        register(&mut first,r#"{"imports":{"x":"./two.js","x-more":"./more.js","new":"./new.js"},"integrity":{"./one.js":"second"}}"#);
        assert_eq!(first.resolve("x",BASE).unwrap().url,"https://example.test/app/one.js");
        assert!(first.resolve("x-more",BASE).is_err(),"new global prefix extension cannot alter a successful prior resolution");
        assert!(first.resolve("new",BASE).is_ok());
        assert!(second.resolve("x",BASE).is_err());
        register(&mut second,r#"{"imports":{"x":"./two.js"}}"#);
        assert_eq!(second.resolve("x",BASE).unwrap().url,"https://example.test/app/two.js");
    }
    #[test]
    fn specification_import_maps_merge_code_unit_prefixes_include_half_surrogate_pairs() {
        let mut state=ImportMapState::default();
        register(&mut state,r#"{"imports":{"\ud800":"./high.js"}}"#);
        let high=smuggle::utf16_from_units(&[0xd800]);
        assert!(state.resolve(&high,BASE).is_ok());
        register(&mut state,r#"{"imports":{"\ud800\udc00":"./pair.js"}}"#);
        assert!(state.resolve("\u{10000}",BASE).is_err(),"HTML merge uses UTF-16 code-unit prefixes, including a prefix ending inside a supplementary scalar");
        register(&mut state,r#"{"imports":{"other":"./other.js"}}"#);
        assert!(state.resolve("other",BASE).is_ok());
        let mut provenance=ImportMapState::default();
        register(&mut provenance,r#"{"imports":{"ab":"./short.js","abc":"./long.js"}}"#);
        assert!(provenance.resolve("abc",BASE).is_ok());
        assert!(provenance.resolve("ab",BASE).is_ok());
        assert_eq!(provenance.resolved_specifiers.len(),1,"a shorter successful global record subsumes its longer global index entries");
        register(&mut provenance,r#"{"scopes":{"./":{"abc":null}}}"#);
        assert_eq!(provenance.resolve("abc",BASE).unwrap().url,"https://example.test/app/long.js","index compression preserves exact scoped resolution provenance");
    }
    #[test]
    fn specification_import_maps_json_duplicate_order_utf16_and_structural_errors() {
        let mut state=ImportMapState::default();
        register(&mut state,r#"{"imports":{"./same":"./a","https://example.test/app/same":"./b","./same":"./c","\ud800":"./lone.js"}}"#);
        assert_eq!(state.resolve("./same",BASE).unwrap().url,"https://example.test/app/b");
        let lone=smuggle::utf16_from_units(&[0xd800]);
        assert_eq!(state.resolve(&lone,BASE).unwrap().url,"https://example.test/app/lone.js");
        let mixed=format!("{{\"imports\":{{\"{}\\udc00\":\"./mixed.js\"}}}}",lone);
        let mut mixed_state=ImportMapState::default();register(&mut mixed_state,&mixed);
        assert_eq!(mixed_state.resolve("\u{10000}",BASE).unwrap().url,"https://example.test/app/mixed.js","literal and escaped halves use JSON.parse's canonical UTF-16 equality");
        for source in ["null","[]",r#"{"imports":[]}"#,r#"{"scopes":{"bad":null}}"#,r#"{"integrity":false}"#] {
            assert!(matches!(ImportMap::parse(source,BASE),Err(Error::Type(_))));
        }
        assert!(matches!(ImportMap::parse("{",BASE),Err(Error::Syntax(_))));
        assert_eq!(smuggle::cmp_utf16("\u{10000}","\u{e000}"),std::cmp::Ordering::Less);
    }
}
