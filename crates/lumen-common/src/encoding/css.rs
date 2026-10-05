//! CSS byte encoding selection. BOM overrides transport and fallback hints.
use encoding_rs::Encoding;

/// Select a CSS byte decoder and borrow its BOM-stripped input. Charset
/// detection is exact ASCII syntax at the start, bounded to 1024 bytes.
pub fn stylesheet_encoding<'a>(
    bytes: &'a [u8],
    transport_label: Option<&str>,
    environment_encoding: Option<&str>,
) -> (&'a [u8], &'static str) {
    if let Some((encoding, length)) = Encoding::for_bom(bytes) {
        return (&bytes[length..], encoding.name());
    }
    let encoding = transport_label
        .and_then(|label| Encoding::for_label(label.as_bytes()))
        .or_else(|| {
            let prefix = &bytes[..bytes.len().min(1024)];
            let rest = prefix.strip_prefix(b"@charset \"")?;
            let end = rest.iter().position(|byte| *byte == b'"')?;
            if rest.get(end + 1) != Some(&b';') || !rest[..end].is_ascii() {
                return None;
            }
            let encoding = Encoding::for_label(&rest[..end])?;
            Some(match encoding.name() {
                "UTF-16LE" | "UTF-16BE" => encoding_rs::UTF_8,
                _ => encoding,
            })
        })
        .or_else(|| environment_encoding.and_then(|label| Encoding::for_label(label.as_bytes())))
        .unwrap_or(encoding_rs::UTF_8);
    (bytes, encoding.name())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stylesheet_encoding_obeys_bom_transport_declaration_and_environment_precedence() {
        let css = b"@charset \"Shift_JIS\";a{}";
        assert_eq!(
            stylesheet_encoding(css, None, Some("windows-1252")).1,
            "Shift_JIS"
        );
        assert_eq!(
            stylesheet_encoding(css, Some("UTF-8"), Some("windows-1252")).1,
            "UTF-8"
        );
        assert_eq!(
            stylesheet_encoding(css, Some("unknown"), Some("windows-1252")).1,
            "Shift_JIS"
        );
        assert_eq!(
            stylesheet_encoding(b"@charset \"UTF-16LE\";", None, None).1,
            "UTF-8"
        );
        let bom = b"\xff\xfeA\0";
        assert_eq!(
            stylesheet_encoding(bom, Some("UTF-8"), None),
            (b"A\0".as_slice(), "UTF-16LE")
        );
        assert_eq!(
            stylesheet_encoding(b"a{}", None, Some("windows-1251")).1,
            "windows-1251"
        );
    }

    #[test]
    fn stylesheet_charset_requires_exact_complete_prefix_inside_byte_budget() {
        for css in [
            b" @charset \"big5\";".as_slice(),
            b"@CHARSET \"big5\";",
            b"@charset 'big5';",
            b"@charset  \"big5\";",
            b"@charset \"big5\"",
            b"@charset \"big5\x80\";",
        ] {
            assert_eq!(
                stylesheet_encoding(css, None, Some("windows-1252")).1,
                "windows-1252"
            );
        }
        let mut css = alloc::vec::Vec::from(b"@charset \"".as_slice());
        css.extend(core::iter::repeat_n(b' ', 1010));
        css.extend_from_slice(b"UTF-8\";");
        assert_eq!(
            stylesheet_encoding(&css, None, Some("windows-1252")).1,
            "windows-1252"
        );
    }
}
