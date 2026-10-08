//! Bounded document reporting endpoints and standards report serialization.
//! Structured fields, JSON and URLs use maintained/shared parsers.
use alloc::{string::{String,ToString},vec::Vec};
use crate::{csp::{Error,Violation},csp_report::{self,Context,MAX_REPORT_BYTES}};

pub const MAX_ENDPOINTS:usize=64;
#[derive(Clone,Debug,Eq,PartialEq)]
pub struct Endpoint {pub name:String,pub url:String,pub expires_at:Option<u64>}
#[derive(Default)]
pub struct Endpoints {entries:Vec<Endpoint>}
impl Endpoints {
    /// Configure from the actual response URL, independent of author <base>.
    pub fn configure(&mut self,document_url:&str,headers:&[(String,String)],now_ms:u64)->Result<(),Error>{
        self.entries.clear();
        if headers.len()>256{return Err(Error::Capacity)}
        let base=crate::url::parse_url(document_url,None).ok_or(Error::InvalidUrl)?;
        if !base.is_potentially_trustworthy(){return Ok(())}
        let mut field=String::new();let mut modern=false;
        for(name,value)in headers {if name.eq_ignore_ascii_case("reporting-endpoints"){
            modern=true;if !field.is_empty(){field.push_str(", ")}
            if field.len().saturating_add(value.len())>MAX_REPORT_BYTES{return Err(Error::Capacity)}field.push_str(value);
        }}
        if modern {
            let Ok(dictionary)=sfv::Parser::new(&field).parse::<sfv::Dictionary>()else{return Ok(())};
            if dictionary.len()>MAX_ENDPOINTS{return Err(Error::Capacity)}
            for(name,value)in dictionary {
                let sfv::ListEntry::Item(item)=value else{continue};
                let sfv::BareItem::String(value)=item.bare_item else{continue};
                let Some(mut url)=crate::url::parse_url(value.as_str(),Some(&base))else{continue};
                if !matches!(url.scheme.as_str(),"http"|"https")||!url.is_potentially_trustworthy()||!url.username.is_empty()||!url.password.is_empty(){continue}
                url.fragment=None;
                self.entries.push(Endpoint{name:name.to_string(),url:url.href(),expires_at:None});
            }
            return Ok(())
        }
        // Compatibility ingress for legacy Report-To JSON. Modern response
        // configuration takes precedence. Endpoint selection is deterministic
        // among equal-priority endpoints, with the first endpoint retained.
        let mut legacy_bytes=0usize;
        for(name,value)in headers {if name.eq_ignore_ascii_case("report-to"){
            legacy_bytes=legacy_bytes.saturating_add(value.len());if legacy_bytes>MAX_REPORT_BYTES{return Err(Error::Capacity)}
            let Ok(parsed)=serde_json::from_str::<serde_json::Value>(value)else{continue};
            let groups=match &parsed{serde_json::Value::Array(groups)=>groups.as_slice(),_=>core::slice::from_ref(&parsed)};
            for group in groups {
                let Some(max_age)=group.get("max_age").and_then(|value|value.as_u64())else{continue};
                let name=group.get("group").and_then(|value|value.as_str()).unwrap_or("default");
                self.entries.retain(|entry|entry.name!=name);if max_age==0{continue}
                let Some(endpoints)=group.get("endpoints").and_then(|value|value.as_array())else{continue};
                if endpoints.len()>MAX_ENDPOINTS{return Err(Error::Capacity)}
                let mut chosen=None;let mut priority=u64::MAX;
                for endpoint in endpoints {
                    let Some(value)=endpoint.get("url").and_then(|value|value.as_str())else{continue};
                    let Some(mut url)=crate::url::parse_url(value,Some(&base))else{continue};
                    if !matches!(url.scheme.as_str(),"http"|"https")||!url.is_potentially_trustworthy()||!url.username.is_empty()||!url.password.is_empty(){continue}
                    let current=endpoint.get("priority").and_then(|value|value.as_u64()).unwrap_or(1);
                    if current<priority{url.fragment=None;chosen=Some(url.href());priority=current;}
                }
                if let Some(url)=chosen{
                    if self.entries.len()>=MAX_ENDPOINTS{return Err(Error::Capacity)}
                    self.entries.push(Endpoint{name:name.into(),url,expires_at:Some(now_ms.saturating_add(max_age.saturating_mul(1000)))});
                }
            }
        }}
        Ok(())
    }
    pub fn get(&self,name:&str,now_ms:u64)->Option<&Endpoint>{self.entries.iter().find(|entry|entry.name==name&&entry.expires_at.is_none_or(|expiry|now_ms<expiry))}
    pub fn remove(&mut self,name:&str,url:&str){self.entries.retain(|entry|entry.name!=name||entry.url!=url);}
}

#[derive(Clone)]
pub struct CspBody {
    pub document_url:String,pub referrer:String,pub blocked_url:String,
    pub effective_directive:String,pub original_policy:String,pub source_file:Option<String>,
    pub sample:String,pub report_only:bool,pub status_code:u16,
    pub line_number:Option<u32>,pub column_number:Option<u32>,
}
impl CspBody {
    pub fn new(violation:&Violation,context:&Context<'_>)->Result<Self,Error>{
        // Reuse the existing bounds and URL sanitization contract.
        csp_report::validate_context(violation,context)?;
        Ok(Self{document_url:csp_report::reporting_url(context.document_url),referrer:csp_report::reporting_url(context.referrer),
            blocked_url:csp_report::reporting_url(&violation.blocked_uri),effective_directive:violation.directive.clone(),original_policy:violation.original_policy.clone(),
            source_file:context.source_file.map(csp_report::reporting_url),sample:violation.sample.clone(),report_only:violation.report_only,status_code:context.status_code,
            line_number:context.source_file.map(|_|context.line_number),column_number:context.source_file.map(|_|context.column_number)})
    }
    pub fn json(&self)->serde_json::Value{serde_json::json!({"documentURL":self.document_url,"referrer":self.referrer,"blockedURL":self.blocked_url,
        "effectiveDirective":self.effective_directive,"originalPolicy":self.original_policy,"sourceFile":self.source_file,"sample":self.sample,
        "disposition":if self.report_only{"report"}else{"enforce"},"statusCode":self.status_code,"lineNumber":self.line_number,"columnNumber":self.column_number})}
}
pub fn csp_envelope(body:&CspBody,user_agent:&str,age_ms:u64)->Result<Vec<u8>,Error>{
    if user_agent.len()>8192{return Err(Error::Capacity)}
    let bytes=serde_json::to_vec(&serde_json::json!([{"age":age_ms,"type":"csp-violation","url":body.document_url,"user_agent":user_agent,"body":body.json()}])).map_err(|_|Error::Capacity)?;
    if bytes.len()>MAX_REPORT_BYTES{return Err(Error::Capacity)}Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reporting_endpoints_use_structured_fields_response_base_and_trust(){
        let mut endpoints=Endpoints::default();
        endpoints.configure("https://example.test/path/page",&[("Reporting-Endpoints".into(),r#"reports="../collect,part";ignored="parameter", invalid=?1, insecure="http://other.test/report""#.into())],0).unwrap();
        assert_eq!(endpoints.get("reports",0).unwrap().url,"https://example.test/collect,part");assert!(endpoints.get("invalid",0).is_none());assert!(endpoints.get("insecure",0).is_none());
        endpoints.configure("https://example.test/page",&[("Reporting-Endpoints".into(),"reports=\"unterminated".into())],0).unwrap();assert!(endpoints.get("reports",0).is_none());
    }
    #[test]
    fn report_to_expiry_and_nullable_csp_metadata_are_preserved(){
        let mut endpoints=Endpoints::default();endpoints.configure("https://example.test/page",&[("Report-To".into(),r#"{"group":"csp","max_age":1,"endpoints":[{"url":"/collect"}]}"#.into())],100).unwrap();
        assert!(endpoints.get("csp",1099).is_some());assert!(endpoints.get("csp",1100).is_none());
        let violation=Violation{directive:"img-src".into(),original_policy:"img-src 'none'; report-to csp".into(),blocked_uri:"https://example.test/pixel".into(),report_only:true,sample:String::new()};
        let body=CspBody::new(&violation,&Context{document_url:"https://example.test/page#secret",referrer:"",source_file:None,status_code:200,line_number:0,column_number:0}).unwrap();
        let envelope:serde_json::Value=serde_json::from_slice(&csp_envelope(&body,"test",3).unwrap()).unwrap();assert!(envelope[0]["body"]["sourceFile"].is_null());assert!(envelope[0]["body"]["lineNumber"].is_null());assert_eq!(envelope[0]["age"],3);assert_eq!(envelope[0]["url"],"https://example.test/page");
    }
}
