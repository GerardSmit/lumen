//! MIME policy shared by language and host adapters.

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
