#![no_main]
use libfuzzer_sys::fuzz_target;
use lumen_html::{
    untrusted::{parse_untrusted, RemoteContent, UntrustedProfile},
    Document, Namespace, NodeId, NodeKind,
};

/// Every `url(` outside a string must point at `cid:`, `data:image/` or a fragment.
fn assert_local_urls(css: &str) {
    let lower = css.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            quote @ (b'"' | b'\'') => {
                index += 1;
                while index < bytes.len() && bytes[index] != quote {
                    index += 1;
                }
                index += 1;
            }
            _ if lower[index..].starts_with("url(") => {
                let rest = lower[index + 4..].trim_start().trim_start_matches(['"', '\'']);
                assert!(
                    rest.starts_with("cid:") || rest.starts_with("data:image/") || rest.starts_with('#'),
                    "remote url survived: {css}"
                );
                index += 4;
            }
            _ => index += 1,
        }
    }
}

fn check(document: &Document) {
    let mut stack: Vec<NodeId> = vec![document.root()];
    while let Some(parent) = stack.pop() {
        let mut cursor = document.first_child(parent).ok().flatten();
        while let Some(node) = cursor {
            cursor = document.next_sibling(node).ok().flatten();
            if let Ok(NodeKind::Element { namespace, name, attributes, .. }) = document.kind(node) {
                let local = name.as_str().to_ascii_lowercase();
                assert!(!local.ends_with("script"), "script element survived");
                assert!(*namespace == Namespace::Html, "foreign element survived: {local}");
                for (key, value) in attributes.iter() {
                    let key = key.as_str().to_ascii_lowercase();
                    let value = value.to_ascii_lowercase();
                    assert!(!key.starts_with("on"), "event handler survived: {key}");
                    match key.as_str() {
                        "src" | "background" | "href" => {
                            assert!(!value.contains("javascript:"), "javascript: survived in {key}")
                        }
                        _ => {}
                    }
                    match key.as_str() {
                        "src" | "background" => assert!(
                            !(value.starts_with("http:") || value.starts_with("https:") || value.starts_with("//")),
                            "remote {key} survived: {value}"
                        ),
                        "style" => assert_local_urls(&value),
                        "href" if local != "a" && local != "area" => panic!("href on {local}"),
                        _ => {}
                    }
                }
                if local == "style" {
                    let mut text = String::new();
                    let _ = document.append_descendant_text(node, &mut text);
                    assert_local_urls(&text);
                }
                stack.push(node);
            }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() > 64 * 1024 {
        return;
    }
    let input = String::from_utf8_lossy(data);
    let profile = UntrustedProfile {
        remote_content: RemoteContent::Blocked,
        max_html_bytes: 64 * 1024,
        max_nodes: 4096,
        ..UntrustedProfile::default()
    };
    let (document, report) = parse_untrusted(&input, &profile);
    assert!(report.links.len() <= profile.max_report_entries);
    assert!(report.blocked_remote.len() <= profile.max_report_entries);
    check(&document);
});
