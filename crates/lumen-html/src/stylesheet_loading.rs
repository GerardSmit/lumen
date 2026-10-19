//! Shared bounded CSS import graph construction. Transport and HTTP policy are
//! supplied by the host; CSS syntax, URL bases and decoding remain authoritative.
use alloc::{boxed::Box, collections::BTreeSet, string::String, sync::Arc, vec::Vec};
use crate::css::{self, LoadedImport, StylesheetSource};

pub const MAX_GRAPH_DEPTH: usize = 24;
pub const MAX_GRAPH_SOURCES: usize = 512;
pub const MAX_GRAPH_BYTES: usize = 16 * 1024 * 1024;

pub struct Response {
    pub final_url: String,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
    pub referrer_policy: Option<lumen_common::referrer::ReferrerPolicy>,
}
#[derive(Clone, Copy)]
pub struct FetchContext<'a> {
    pub referrer: &'a str,
    pub referrer_policy: lumen_common::referrer::ReferrerPolicy,
    pub root: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error { ResourceLimit, InvalidResponseUrl, Decode, Syntax }
pub struct SourceContext {pub path:Vec<usize>,pub encoding:&'static str,pub referrer_policy:lumen_common::referrer::ReferrerPolicy}
#[derive(Default)]
pub struct GraphBudget { pub source_count: usize, pub byte_count: usize, pub critical_failed: bool, pub failed_import_paths:Vec<Vec<usize>>, pub source_contexts:Vec<SourceContext> }

fn record_source_context(budget:&mut GraphBudget,path:&[usize],encoding:&'static str,referrer_policy:lumen_common::referrer::ReferrerPolicy)->Result<(),Error> {
    if budget.source_contexts.len()>=MAX_GRAPH_SOURCES {return Err(Error::ResourceLimit)}
    budget.source_contexts.try_reserve(1).map_err(|_|Error::ResourceLimit)?;
    let mut captured=Vec::new();captured.try_reserve_exact(path.len()).map_err(|_|Error::ResourceLimit)?;captured.extend_from_slice(path);
    budget.source_contexts.push(SourceContext{path:captured,encoding,referrer_policy});Ok(())
}

/// Failed imports are absent edges; cycles retain only the original graph node.
/// The callback completes body consumption before this graph is published.
pub fn load(
    url: &str, environment_encoding: &str, budget: &mut GraphBudget,
    fetch: &mut impl FnMut(&str) -> Result<Option<Response>, Error>,
) -> Result<Option<StylesheetSource>, Error> {
    load_with_context(url,environment_encoding,budget,FetchContext {referrer:url,
        referrer_policy:lumen_common::referrer::ReferrerPolicy::default(),root:true},&mut |url,_|fetch(url))
}
pub fn load_with_context(
    url:&str,environment_encoding:&str,budget:&mut GraphBudget,context:FetchContext<'_>,
    fetch:&mut impl FnMut(&str,FetchContext<'_>)->Result<Option<Response>,Error>,
)->Result<Option<StylesheetSource>,Error> {
    load_inner(url,environment_encoding,budget,context,fetch,&mut BTreeSet::new(),&mut Vec::new(),0)
}
fn load_inner(
    url: &str, encoding: &str, budget: &mut GraphBudget,
    context:FetchContext<'_>,
    fetch: &mut impl FnMut(&str,FetchContext<'_>) -> Result<Option<Response>, Error>,
    active: &mut BTreeSet<String>, path:&mut Vec<usize>, depth: usize,
) -> Result<Option<StylesheetSource>, Error> {
    if depth >= MAX_GRAPH_DEPTH || budget.source_count >= MAX_GRAPH_SOURCES { return Err(Error::ResourceLimit); }
    let requested=without_fragment(url);
    if active.contains(requested) { return Ok(None); }
    budget.source_count += 1;
    active.insert(requested.into());
    let result=(|| {
        let Some(response)=fetch(requested,context)? else {
            budget.critical_failed=true;
            if !path.is_empty() {record_failed_import(budget,path)?;}
            return Ok(None);
        };
        let final_url=lumen_common::url::parse(&response.final_url,None).map_err(|_|Error::InvalidResponseUrl)?.href();
        let final_key=without_fragment(&final_url);
        if final_key!=requested && active.contains(final_key) { return Ok(None); }
        if final_key!=requested { active.insert(final_key.into()); }
        let result=(|| {
            let charset=response.content_type.as_deref().and_then(|value|value.split(';').skip(1).find_map(|parameter| {
                let (name,value)=parameter.trim().split_once('=')?;
                name.trim().eq_ignore_ascii_case("charset").then(||value.trim().trim_matches(['"','\'']))
            }));
            let (payload,selected)=lumen_common::encoding::stylesheet_encoding(&response.bytes,charset,Some(encoding));
            record_source_context(budget,path,selected,response.referrer_policy.unwrap_or(context.referrer_policy))?;
            let remaining=MAX_GRAPH_BYTES.checked_sub(budget.byte_count).ok_or(Error::ResourceLimit)?;
            let mut decoder=lumen_common::encoding::TextDecoder::new_document_bounded(selected,remaining).map_err(|_|Error::Decode)?;
            let text=decoder.decode(payload,false).map_err(|error|if matches!(error,lumen_common::encoding::DecodeError::ResourceLimit){Error::ResourceLimit}else{Error::Decode})?;
            budget.byte_count=budget.byte_count.checked_add(text.len()).filter(|&size|size<=MAX_GRAPH_BYTES).ok_or(Error::ResourceLimit)?;
            build_source(&final_url,Arc::from(text),selected,budget,FetchContext {
                referrer:&final_url,referrer_policy:response.referrer_policy.unwrap_or(context.referrer_policy),root:false,
            },fetch,active,path,depth).map(Some)
        })();
        if final_key!=requested { active.remove(final_key); }
        result
    })();
    active.remove(requested);
    result
}
/// Inline source is already decoded Unicode. The same graph traversal fetches
/// only its critical imports; the owning root is never a synthetic HTTP request.
pub fn load_inline_with_context(
    base:&str,text:Arc<str>,environment_encoding:&str,budget:&mut GraphBudget,context:FetchContext<'_>,
    fetch:&mut impl FnMut(&str,FetchContext<'_>)->Result<Option<Response>,Error>,
)->Result<StylesheetSource,Error> {
    if budget.source_count>=MAX_GRAPH_SOURCES{return Err(Error::ResourceLimit)}
    budget.source_count+=1;
    budget.byte_count=budget.byte_count.checked_add(text.len()).filter(|size|*size<=MAX_GRAPH_BYTES).ok_or(Error::ResourceLimit)?;
    let base=lumen_common::url::parse(base,None).map_err(|_|Error::InvalidResponseUrl)?.href();
    let selected=lumen_common::encoding::canonical_document_label(environment_encoding).map_err(|_|Error::Decode)?;
    record_source_context(budget,&[],selected,context.referrer_policy)?;
    // The inline root has no resource URL and must not suppress an import of
    // the document/base URL as an apparent stylesheet cycle.
    build_source(&base,text,environment_encoding,budget,FetchContext {referrer:context.referrer,
        referrer_policy:context.referrer_policy,root:false},fetch,&mut BTreeSet::new(),&mut Vec::new(),0)
}
fn build_source(
    base:&str,text:Arc<str>,encoding:&str,budget:&mut GraphBudget,context:FetchContext<'_>,
    fetch:&mut impl FnMut(&str,FetchContext<'_>)->Result<Option<Response>,Error>,
    active:&mut BTreeSet<String>,path:&mut Vec<usize>,depth:usize,
)->Result<StylesheetSource,Error> {
    let rules=css::imports(&text).map_err(|_|Error::Syntax)?;
    let mut imports=Vec::new();imports.try_reserve(rules.len()).map_err(|_|Error::ResourceLimit)?;
    for (ordinal,rule) in rules.into_iter().enumerate() {
        path.try_reserve(1).map_err(|_|Error::ResourceLimit)?;path.push(ordinal);
        let source=if rule.supports.as_deref().is_some_and(|condition|!css::supports_condition(condition)){None}
            else if let Some(resolved)=css::resolve_import_url(&rule,base) {
                load_inner(&resolved,encoding,budget,context,fetch,active,path,depth+1)?.map(Box::new)
            }else{budget.critical_failed=true;record_failed_import(budget,path)?;None};
        path.pop();
        imports.push(LoadedImport{rule,source});
    }
    Ok(StylesheetSource{disabled:false, url:Arc::from(base),text,imports})
}
fn record_failed_import(budget:&mut GraphBudget,path:&[usize])->Result<(),Error> {
    if budget.failed_import_paths.len()>=MAX_GRAPH_SOURCES || path.len()>MAX_GRAPH_DEPTH {return Err(Error::ResourceLimit)}
    budget.failed_import_paths.try_reserve(1).map_err(|_|Error::ResourceLimit)?;
    let mut captured=Vec::new();captured.try_reserve_exact(path.len()).map_err(|_|Error::ResourceLimit)?;captured.extend_from_slice(path);
    budget.failed_import_paths.push(captured);Ok(())
}

fn without_fragment(url:&str)->&str { url.split_once('#').map_or(url,|(url,_)|url) }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_inline_stylesheet_graph_reuses_unicode_root_and_fetches_only_critical_edges() {
        let text:Arc<str>=Arc::from("@import 'doc.html'; @import 'ignored.css' supports(unknown:value);p{color:green}");
        let original=text.clone();let mut requested=Vec::new();
        let source=load_inline_with_context("https://inline.test/doc.html",text,"UTF-8",&mut GraphBudget::default(),
            FetchContext{referrer:"https://inline.test/doc.html",referrer_policy:lumen_common::referrer::ReferrerPolicy::default(),root:true},
            &mut |url,context|{assert!(!context.root);requested.push(String::from(url));
                Ok(Some(Response{final_url:url.into(),content_type:Some("text/css".into()),bytes:b"a{color:red}".to_vec(),referrer_policy:None}))}).unwrap();
        assert!(Arc::ptr_eq(&original,&source.text),"decoded inline root is not cloned through a byte decoder");
        assert_eq!(requested,["https://inline.test/doc.html"],"inline base URL is not itself an active fetched resource");
        assert!(source.imports[0].source.is_some());assert!(source.imports[1].source.is_none());
    }

    #[test]
    fn specification_stylesheet_graph_uses_response_bases_encoding_and_bounded_cycle_edges() {
        let mut requested=Vec::new();
        let source=load("https://sheet.test/original.css#fragment","UTF-8",&mut GraphBudget::default(),&mut |url| {
            requested.push(String::from(url));
            let (final_url,source)=if url=="https://sheet.test/original.css" {
                ("https://sheet.test/dir/final.css","@import 'child.css';p{color:red}")
            } else { ("https://sheet.test/dir/child.css","@import 'final.css';q{color:blue}") };
            Ok(Some(Response{final_url:final_url.into(),content_type:Some("text/css;charset=utf-8".into()),bytes:source.as_bytes().into(),referrer_policy:None}))
        }).unwrap().unwrap();
        assert_eq!(requested,["https://sheet.test/original.css","https://sheet.test/dir/child.css"]);
        assert_eq!(source.url.as_ref(),"https://sheet.test/dir/final.css");
        assert!(source.imports[0].source.as_ref().unwrap().imports[0].source.is_none());
        let mut budget=GraphBudget{source_count:MAX_GRAPH_SOURCES,byte_count:0,..Default::default()};
        assert!(matches!(load("https://sheet.test/x","UTF-8",&mut budget,&mut |_|panic!("resource admission precedes transport")),Err(Error::ResourceLimit)));
    }
}
