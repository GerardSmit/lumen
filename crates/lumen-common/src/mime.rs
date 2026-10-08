//! MIME policy shared by language and host adapters.

/// Borrow Fetch's MIME essence without allocating parameter collections. The
/// maintained parser handles normal names; Fetch's broader HTTP-token names
/// reuse the canonical HTTP lexer rather than RFC registration-name limits.
pub fn mime_essence(raw:&str)->Option<&str> {
    let raw=raw.trim_matches(|c|matches!(c,' '| '\t' | '\r' | '\n'));
    let essence=raw.split(';').next()?.trim_end_matches(|c|matches!(c,' '| '\t' | '\r' | '\n'));
    if mediatype::MediaType::parse(essence).is_ok() {return Some(essence)}
    let (ty,subty)=essence.split_once('/')?;
    (!ty.is_empty() && !subty.is_empty() && ty.bytes().all(crate::http_body::token)
        && subty.bytes().all(crate::http_body::token)).then_some(essence)
}

/// Image formats available to the shared static image decoder. A CSS image-set
/// type descriptor requires a valid complete MIME string, unlike a response
/// essence obtained by permissive Fetch parsing. Parameters do not change the
/// format, and the descriptor never overrides the resource's decoded bytes.
pub fn image_type_supported(raw: &str) -> bool {
    let Ok(parsed) = mediatype::MediaType::parse(raw) else { return false; };
    // The maintained parser accepts an empty final parameter and unquoted
    // empty values; neither is a valid media-type production for a descriptor.
    if raw.trim_end_matches([' ', '\t']).ends_with(';')
        || parsed.params.iter().any(|(_, value)| value.as_str().is_empty()) {
        return false;
    }
    mime_essence(raw).is_some_and(|essence| {
        ["image/png", "image/jpeg", "image/webp", "image/gif", "image/bmp", "image/x-ms-bmp", "image/svg+xml"]
            .iter().any(|supported| essence.eq_ignore_ascii_case(supported))
    })
}

/// Fetch header-list MIME extraction. Later valid values replace earlier ones;
/// repeated essences inherit charset, but a changed essence clears it. The
/// maintained media-type lexer preserves quoted commas and parameter values.
pub fn extract_mime_type<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<alloc::string::String> {
    use alloc::string::ToString;
    use mediatype::WriteParams;
    let mut previous_essence = None;
    let mut charset = None;
    let mut selected = None;
    for value in values {
        for parsed in mediatype::MediaTypeList::new(value) {
            let Ok(mut mime) = parsed else { continue };
            let essence = mime.essence().to_string().to_ascii_lowercase();
            if essence == "*/*" { continue; }
            if previous_essence.as_ref() != Some(&essence) { charset = None; }
            if let Some(value) = mime.params.iter().find(|(name, _)| *name == mediatype::names::CHARSET).map(|(_, value)| *value) {
                charset = Some(value);
            } else if let Some(value) = charset {
                mime.set_param(mediatype::names::CHARSET, value);
            }
            previous_essence = Some(essence);
            selected = Some(mime.to_string());
        }
    }
    selected
}

/// HTML linked stylesheets default an absent or malformed MIME type to CSS.
/// The quirks exception requires actual same-origin URLs, rather than a clean
/// CORS response. CSS modules retain their separate strict MIME contract.
pub fn stylesheet_mime_allowed(content_type:Option<&str>,quirks:bool,same_origin:bool)->bool {
    let Some(raw)=content_type else {return true};
    let Some(essence)=mime_essence(raw) else {return true};
    essence.eq_ignore_ascii_case("text/css") || (quirks && same_origin)
}

/// An element's optional MIME hint is distinct from response defaulting.
/// Parameters and surrounding HTTP whitespace do not change the essence.
pub fn stylesheet_hint_supported(raw:&str)->bool {
    let raw=raw.trim_matches(|c|matches!(c,' '| '\t' | '\r' | '\n'));
    if raw.is_empty() {return true}
    mime_essence(raw).is_some_and(|essence|essence.eq_ignore_ascii_case("text/css"))
}

/// HTML's style-block algorithm deliberately uses the complete attribute value,
/// unlike the MIME hint processing for an external link.
pub fn inline_stylesheet_type_supported(raw:&str)->bool {
    raw.is_empty() || raw.eq_ignore_ascii_case("text/css")
}

/// Fetch's script-like response MIME block and classic-script nosniff policy.
pub fn classic_script_mime_allowed(content_type: Option<&str>, nosniff: bool) -> bool {
    if nosniff && !is_javascript_module_mime(content_type) {
        return false;
    }
    let Some(value) = content_type else {
        return true;
    };
    let essence = value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    !["audio/", "image/", "video/"]
        .iter()
        .any(|prefix| essence.starts_with(prefix))
        && essence != "text/csv"
}

/// Fetch determines nosniff from the first decoded header-list token.
pub fn determine_nosniff(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        value
            .split(',')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("nosniff")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specification_image_types_share_decoder_capabilities_and_validate_parameters() {
        for raw in ["image/png", "IMAGE/JPEG", "image/webp", "image/gif", "image/bmp",
            "image/x-ms-bmp", "IMAGE/X-MS-BMP;version=3", "image/svg+xml", "image/svg+xml;charset=utf-8", "image/png;name=\"a;b\""] {
            assert!(image_type_supported(raw), "{raw}");
        }
        for raw in ["image/avif", "image/tiff", "image/x-icon", "text/plain", "not a mime",
            "image/x-ms-bmp;", "image/x-ms-bmp;charset=", "image/png;", "image/png;charset", "image/png;charset=", "image/png;charset=\"unterminated"] {
            assert!(!image_type_supported(raw), "{raw}");
        }
    }

    #[test]
    fn specification_response_mime_uses_last_valid_value_and_retains_charset() {
        for (values, expected) in [
            (alloc::vec!["application/json", "text/html; charset=utf-8"], "text/html; charset=utf-8"),
            (alloc::vec!["text/plain;charset=gbk, text/html"], "text/html"),
            (alloc::vec!["text/html;charset=gbk;a=b", "text/html;x=y"], "text/html; x=y; charset=gbk"),
            (alloc::vec!["text/html;charset=gbk", "x/x", "text/html;x=y"], "text/html; x=y"),
            (alloc::vec!["text/html", "cannot-parse", "*/*", ""], "text/html"),
            (alloc::vec!["text/html; message=\"hello, world\", text/plain"], "text/plain"),
        ] {
            assert_eq!(extract_mime_type(values).as_deref(), Some(expected));
        }
        assert_eq!(extract_mime_type(["invalid", "*/*"]), None);
    }

    #[test]
    fn specification_stylesheet_mime_hint_and_response_defaulting_are_distinct() {
        assert!(inline_stylesheet_type_supported(""));
        assert!(inline_stylesheet_type_supported("TEXT/CSS"));
        for raw in ["text/css;charset=utf-8"," text/css","text/css ","text/plain"] {
            assert!(!inline_stylesheet_type_supported(raw),"inline style type {raw}");
        }
        for hint in ["", " text/css ; charset=utf-8 ", "TEXT/CSS;charset=windows-1250", "text/css; ignored-parameter"] {
            assert!(stylesheet_hint_supported(hint),"{hint}");
        }
        for hint in ["text/plain", "text/css+json", "not a mime"] {
            assert!(!stylesheet_hint_supported(hint),"{hint}");
        }
        assert!(stylesheet_mime_allowed(None,false,false));
        assert!(stylesheet_mime_allowed(Some("not a mime"),false,false));
        assert!(!stylesheet_mime_allowed(Some("text/plain"),false,true));
        assert!(!stylesheet_mime_allowed(Some("!foo/bar"),false,false));
        assert!(!stylesheet_mime_allowed(Some(&alloc::format!("{}/css", "x".repeat(200))),false,false));
        assert!(!stylesheet_mime_allowed(Some("text/plain"),true,false),"CORS-clean is not same-origin");
        assert!(stylesheet_mime_allowed(Some("text/plain"),true,true));
        assert!(stylesheet_mime_allowed(Some("TEXT/CSS; charset=utf-8"),false,false));
    }

    #[test]
    fn classic_script_mime_blocks_media_and_obeys_first_nosniff_token() {
        assert!(classic_script_mime_allowed(None, false));
        assert!(classic_script_mime_allowed(
            Some("text/plain; charset=utf-8"),
            false
        ));
        for mime in ["IMAGE/svg+xml", "audio/ogg", "video/mp4", "text/csv"] {
            assert!(!classic_script_mime_allowed(Some(mime), false));
        }
        assert!(!classic_script_mime_allowed(None, true));
        assert!(!classic_script_mime_allowed(Some("text/plain"), true));
        assert!(classic_script_mime_allowed(
            Some("TEXT/JAVASCRIPT; charset=utf-8"),
            true
        ));
        assert!(determine_nosniff(Some(" NoSnIfF, ignored")));
        assert!(!determine_nosniff(Some("ignored, nosniff")));
        assert!(!determine_nosniff(None));
    }

    #[test]
    fn specification_text_document_category_keeps_json_and_script_navigation_inert() {
        for mime in ["Application/JSON; charset=utf-8", "text/json", "application/problem+JSON", "application/+json", "text/javascript1.2", "text/vtt", "text/css", "text/plain"] {
            assert!(is_text_document_mime(Some(mime)), "{mime}");
        }
        for mime in ["text/html", "application/xhtml+xml", "image/svg+xml", "application/octet-stream", "json"] {
            assert!(!is_text_document_mime(Some(mime)), "{mime}");
        }
        assert!(!is_text_document_mime(None));
        assert!(is_json_mime(Some("application/problem+json")));
        assert!(!is_javascript_module_mime(Some("application/json")));
        assert!(!is_javascript_module_mime(Some("text/vtt")));
    }
}

/// Whether a Content-Type value has a JavaScript MIME essence accepted for module scripts.
/// Parameters are ignored and essence matching is ASCII case-insensitive.
pub fn is_javascript_module_mime(content_type: Option<&str>) -> bool {
    const ESSENCES: &[&str] = &[
        "application/ecmascript",
        "application/javascript",
        "application/x-ecmascript",
        "application/x-javascript",
        "text/ecmascript",
        "text/javascript",
        "text/javascript1.0",
        "text/javascript1.1",
        "text/javascript1.2",
        "text/javascript1.3",
        "text/javascript1.4",
        "text/javascript1.5",
        "text/jscript",
        "text/livescript",
        "text/x-ecmascript",
        "text/x-javascript",
    ];
    let Some(value) = content_type else {
        return false;
    };
    let essence = value.split(';').next().unwrap_or("").trim();
    ESSENCES
        .iter()
        .any(|accepted| essence.eq_ignore_ascii_case(accepted))
}

/// CSS modules require the text/css MIME essence.
pub fn is_css_module_mime(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value|value.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("text/css"))
}

/// JSON MIME types include application/json, text/json and the +json suffix.
pub fn is_json_mime(content_type: Option<&str>) -> bool {
    let Some(value) = content_type else { return false; };
    let essence = value.split(';').next().unwrap_or("").trim();
    if essence.eq_ignore_ascii_case("application/json") || essence.eq_ignore_ascii_case("text/json") { return true; }
    let Some((kind, subtype)) = essence.split_once('/') else { return false; };
    !kind.is_empty() && subtype.len() >= 5 && subtype.as_bytes()[subtype.len()-5..].eq_ignore_ascii_case(b"+json")
}

/// HTML's text-document navigation category, absent an external JSON handler.
pub fn is_text_document_mime(content_type: Option<&str>) -> bool {
    let Some(value) = content_type else { return false; };
    let essence = value.split(';').next().unwrap_or("").trim();
    is_javascript_module_mime(Some(value)) || is_json_mime(Some(value))
        || ["text/plain", "text/css", "text/vtt"].iter().any(|mime| essence.eq_ignore_ascii_case(mime))
}

/// HTML's object type-selection algorithm. This UA obeys a valid response
/// Content-Type strictly, as explicitly permitted by HTML; an absent response
/// type uses the element hint, then the shared WHATWG sniffer. Decoders still
/// verify bytes independently, and XML MIME types always select a document.
#[cfg(feature = "object-mime")]
pub fn object_resource_mime(content_type: Option<&str>, hint: Option<&str>, body: &[u8]) -> alloc::string::String {
    use alloc::string::ToString;
    for declared in [content_type, hint].into_iter().flatten() {
        if let Some(essence) = mime_essence(declared) { return essence.to_ascii_lowercase(); }
    }
    static CLASSIFIER: std::sync::OnceLock<mime_classifier::MimeClassifier> = std::sync::OnceLock::new();
    CLASSIFIER.get_or_init(mime_classifier::MimeClassifier::new).classify(
        mime_classifier::LoadContext::Browsing, mime_classifier::NoSniffFlag::Off,
        mime_classifier::ApacheBugFlag::Off, &None, &body[..body.len().min(1445)],
    ).essence_str().to_string()
}

/// XML is checked before the image category: SVG creates an actual nested
/// document rather than an image-only substitute with fabricated getters.
pub fn is_xml_mime(content_type: Option<&str>) -> bool {
    content_type.and_then(mime_essence).is_some_and(|essence| {
        essence.eq_ignore_ascii_case("text/xml") || essence.eq_ignore_ascii_case("application/xml")
            || essence.get(essence.len().saturating_sub(4)..).is_some_and(|suffix| suffix.eq_ignore_ascii_case("+xml"))
    })
}

#[cfg(all(test,feature="object-mime"))]
mod object_tests {
    use super::*;
    #[test]
    fn specification_object_mime_respects_response_and_shared_browsing_classifier() {
        assert_eq!(object_resource_mime(Some("image/png; charset=UTF-8"),Some("text/html"),b"<!doctype html>"),"image/png");
        assert_eq!(object_resource_mime(None,Some("image/svg+xml"),b"<svg/>"),"image/svg+xml");
        assert_eq!(object_resource_mime(None,None,b"<!DOCTYPE HTML><html></html>"),"text/html");
        assert!(is_xml_mime(Some("image/svg+xml")));assert!(is_xml_mime(Some("application/example+xml")));
        assert!(!is_xml_mime(Some("image/png")));
    }
}
