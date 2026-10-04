//! Shared JavaScript URL bindings over the language-neutral WHATWG parser.
use lumen::embed::{Ctx, OpResult, Value};
use lumen_common::url;
fn record(ctx: &mut Ctx, url: &url::Url) -> Value {
    let mut items = vec![Value::from_string(url.href())];
    items.extend(url.components().iter().map(|&c| Value::Num(c as f64)));
    items.push(Value::from_string(url.origin()));
    ctx.make_array(items)
}

#[lumen_bind::module(name = "__lumenUrl")]
pub mod bindings {
    use super::*;
    #[op]
    pub fn parse(ctx: &mut Ctx, input: String, base: Option<String>) -> OpResult<Value> {
        let base = match base {
            Some(base) => match url::parse_url(&base, None) {
                Some(url) => Some(url),
                None => return Ok(Value::Null),
            },
            None => None,
        };
        Ok(url::parse_url(&input, base.as_ref()).map_or(Value::Null, |url| record(ctx, &url)))
    }
    #[op]
    pub fn update(ctx: &mut Ctx, href: String, action: i32, value: String) -> OpResult<Value> {
        let Some(mut url) = url::parse_url(&href, None) else {
            return Ok(Value::Null);
        };
        let accepted = match action {
            0 => url.set_protocol(&value),
            1 => url.set_host(&value),
            2 => url.set_hostname(&value),
            3 => url.set_port(&value),
            4 => url.set_username(&value),
            5 => url.set_password(&value),
            6 => url.set_pathname(&value),
            7 => {
                url.set_search(&value);
                true
            }
            8 => {
                url.set_hash(&value);
                true
            }
            9 => url.set_href(&value),
            _ => false,
        };
        Ok(if accepted {
            record(ctx, &url)
        } else {
            Value::Null
        })
    }
    #[op(rename(js = "canParse"))]
    pub fn can_parse(input: String, base: Option<String>) -> bool {
        match base {
            Some(base) => url::parse_url(&base, None)
                .is_some_and(|base| url::parse_url(&input, Some(&base)).is_some()),
            None => url::parse_url(&input, None).is_some(),
        }
    }
    #[op(rename(js = "domainToASCII"))]
    pub fn domain_to_ascii(input: String) -> String {
        url::domain_to_ascii(&input)
    }
    #[op(rename(js = "domainToUnicode"))]
    pub fn domain_to_unicode(input: String) -> String {
        url::domain_to_unicode(&input)
    }
    #[op(rename(js = "toASCII"))]
    pub fn to_ascii(input: String) -> String {
        url::idna_to_ascii(&input)
    }
    #[op(rename(js = "toUnicode"))]
    pub fn to_unicode(input: String) -> String {
        url::domain_to_unicode_raw(&input)
    }
    #[op]
    pub fn format(href: String, hash: bool, unicode: bool, search: bool, auth: bool) -> String {
        url::format(&href, hash, unicode, search, auth).unwrap_or(href)
    }
}
