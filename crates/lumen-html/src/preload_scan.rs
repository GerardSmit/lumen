//! Speculative start-tag scan over unparsed HTML source. It never builds a
//! tree: it only walks tags, skipping comments, raw-text elements, `<template>`
//! content and `<noscript>` content (scripting is enabled for network parsers).
use crate::html::{comment_end, decode_entities, raw_text_end_in};
use alloc::{string::String, vec::Vec};

pub struct ScannedTag {
    pub name: &'static str,
    pub attributes: Vec<(String, String)>,
}
impl ScannedTag {
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }
}

pub struct PreloadScan {
    pub has_doctype: bool,
    pub tags: Vec<ScannedTag>,
}

const RAW_TEXT: [&str; 8] = ["script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes"];

fn interesting(name: &str) -> Option<&'static str> {
    ["base", "link", "script", "img"].into_iter().find(|candidate| *candidate == name)
}

/// Returns `base`, `link`, `script` and `img` start tags in source order,
/// outside inert content, up to `max_tags`.
pub fn scan(input: &str, max_tags: usize) -> PreloadScan {
    let bytes = input.as_bytes();
    let mut result = PreloadScan { has_doctype: false, tags: Vec::new() };
    let mut pos = 0;
    let mut template_depth = 0usize;
    let mut seen_start_tag = false;
    while result.tags.len() < max_tags {
        let Some(offset) = input[pos..].find('<') else { break };
        let at = pos + offset;
        pos = at + 1;
        match bytes.get(pos) {
            Some(b'!') => {
                if bytes[at..].starts_with(b"<!--") {
                    pos = if bytes[at + 4..].starts_with(b">") {
                        at + 5
                    } else if bytes[at + 4..].starts_with(b"->") {
                        at + 6
                    } else {
                        match comment_end(bytes, at + 4) {
                            Some(end) => end,
                            None => break,
                        }
                    };
                    continue;
                }
                let end = input[pos..].find('>').map_or(input.len(), |end| pos + end + 1);
                if !seen_start_tag && bytes[at..end].len() >= 9 && bytes[at + 2..at + 9].eq_ignore_ascii_case(b"doctype") {
                    result.has_doctype = true;
                }
                pos = end;
            }
            Some(b'/') => {
                let name_start = pos + 1;
                if !bytes.get(name_start).is_some_and(u8::is_ascii_alphabetic) {
                    pos = input[pos..].find('>').map_or(input.len(), |end| pos + end + 1);
                    continue;
                }
                let name_end = tag_name_end(bytes, name_start);
                if input[name_start..name_end].eq_ignore_ascii_case("template") {
                    template_depth = template_depth.saturating_sub(1);
                }
                pos = input[name_end..].find('>').map_or(input.len(), |end| name_end + end + 1);
            }
            Some(byte) if byte.is_ascii_alphabetic() => {
                seen_start_tag = true;
                let name_end = tag_name_end(bytes, pos);
                let name = input[pos..name_end].to_ascii_lowercase();
                pos = name_end;
                let Some(attributes) = attributes(input, &mut pos) else { break };
                if name == "plaintext" {
                    break;
                }
                if name == "template" {
                    template_depth += 1;
                    continue;
                }
                if template_depth == 0 {
                    if let Some(name) = interesting(&name) {
                        result.tags.push(ScannedTag { name, attributes });
                    }
                }
                if RAW_TEXT.contains(&name.as_str()) || name == "noscript" {
                    pos += raw_text_end_in(input, pos, &name);
                }
            }
            _ => {}
        }
    }
    result
}

fn tag_name_end(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|&byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
        .map_or(bytes.len(), |len| start + len)
}

fn attributes(input: &str, pos: &mut usize) -> Option<Vec<(String, String)>> {
    let bytes = input.as_bytes();
    let mut attributes: Vec<(String, String)> = Vec::new();
    loop {
        while bytes.get(*pos).is_some_and(u8::is_ascii_whitespace) {
            *pos += 1;
        }
        match bytes.get(*pos) {
            None => return None,
            Some(b'>') => {
                *pos += 1;
                return Some(attributes);
            }
            Some(b'/') => {
                *pos += 1;
                continue;
            }
            Some(_) => {}
        }
        let start = *pos;
        let mut end = start + usize::from(bytes[start] == b'=');
        while let Some(&byte) = bytes.get(end) {
            if byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>' | b'=') {
                break;
            }
            end += 1;
        }
        let key = input[start..end].to_ascii_lowercase();
        *pos = end;
        while bytes.get(*pos).is_some_and(u8::is_ascii_whitespace) {
            *pos += 1;
        }
        let mut value = String::new();
        if bytes.get(*pos) == Some(&b'=') {
            *pos += 1;
            while bytes.get(*pos).is_some_and(u8::is_ascii_whitespace) {
                *pos += 1;
            }
            let (raw_start, raw_end) = match bytes.get(*pos) {
                Some(&quote @ (b'\'' | b'"')) => {
                    let begin = *pos + 1;
                    let end = bytes[begin..].iter().position(|&byte| byte == quote).map_or(bytes.len(), |len| begin + len);
                    *pos = (end + 1).min(bytes.len());
                    (begin, end)
                }
                _ => {
                    let begin = *pos;
                    let end = bytes[begin..]
                        .iter()
                        .position(|&byte| byte.is_ascii_whitespace() || byte == b'>')
                        .map_or(bytes.len(), |len| begin + len);
                    *pos = end;
                    (begin, end)
                }
            };
            value = decode_entities(&input[raw_start..raw_end], true).into_owned();
        }
        if !attributes.iter().any(|(existing, _)| *existing == key) {
            attributes.push((key, value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn names(scan: &PreloadScan) -> Vec<(&'static str, Option<&str>)> {
        scan.tags.iter().map(|tag| (tag.name, tag.attribute("href").or(tag.attribute("src")))).collect()
    }
    #[test]
    fn scan_skips_inert_content_and_reports_tags_in_order() {
        let scan = scan(
            "<!DOCTYPE html><!-- <link href=c> --><base href=/b/><link rel=stylesheet href='/a.css?x=1&amp;y=2'>\
             <template><link href=t></template><noscript><link href=n></noscript><title><link href=ti></title>\
             <script src=/s.js>var x='<link href=in>';</script><textarea><img src=ta></textarea><img src=/i.png>",
            64,
        );
        assert!(scan.has_doctype);
        assert_eq!(
            names(&scan),
            [("base", Some("/b/")), ("link", Some("/a.css?x=1&y=2")), ("script", Some("/s.js")), ("img", Some("/i.png"))]
        );
    }
    #[test]
    fn scan_caps_tags_and_detects_missing_doctype() {
        let scan = scan("<link href=1><link href=2><link href=3>", 2);
        assert!(!scan.has_doctype);
        assert_eq!(scan.tags.len(), 2);
    }
}
