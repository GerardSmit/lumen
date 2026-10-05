//! HTML's bounded byte prescan. It borrows input slices and allocates nothing.
//! Transport charset/BOM precedence and parser restarts belong to the caller.

use encoding_rs::Encoding;

const PRESCAN_BYTES: usize = 1024;

fn space(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

fn label(bytes: &[u8]) -> Option<&'static str> {
    Encoding::for_label(bytes).map(Encoding::name)
}

fn meta_label(encoding: &'static str) -> &'static str {
    match encoding {
        "UTF-16LE" | "UTF-16BE" => "UTF-8",
        "x-user-defined" => "windows-1252",
        _ => encoding,
    }
}

/// Return an HTML meta/XML declaration encoding from at most 1024 bytes.
/// A truncated token is ignored; unrelated tag attributes and comments are
/// skipped, and duplicate relevant attributes obey first-occurrence semantics.
pub fn prescan_html_encoding(input: &[u8]) -> Option<&'static str> {
    let bytes = &input[..input.len().min(PRESCAN_BYTES)];
    if bytes.starts_with(b"<\0?\0x\0") {
        return Some("UTF-16LE");
    }
    if bytes.starts_with(b"\0<\0?\0x") {
        return Some("UTF-16BE");
    }
    scan_meta(bytes).or_else(|| xml_declaration(bytes))
}

fn scan_meta(bytes: &[u8]) -> Option<&'static str> {
    let mut position = 0;
    while position < bytes.len() {
        let remaining = &bytes[position..];
        if remaining.starts_with(b"<!--") {
            // The two dashes in the opener can also close <!-->.
            let end = remaining[2..].windows(3).position(|part| part == b"-->")?;
            position += 2 + end + 3;
            continue;
        }
        let meta = remaining
            .get(..5)
            .is_some_and(|part| part.eq_ignore_ascii_case(b"<meta"))
            && remaining
                .get(5)
                .is_some_and(|byte| space(*byte) || *byte == b'/');
        if meta {
            position += 5;
            let mut seen = 0u8;
            let mut pragma = false;
            let mut need_pragma = None;
            // Outer Option distinguishes no label from an unrecognized label.
            let mut charset: Option<Option<&'static str>> = None;
            while let Some((name, value)) = attribute(bytes, &mut position)? {
                let bit = if name.eq_ignore_ascii_case(b"http-equiv") {
                    1
                } else if name.eq_ignore_ascii_case(b"content") {
                    2
                } else if name.eq_ignore_ascii_case(b"charset") {
                    4
                } else {
                    0
                };
                if bit == 0 || seen & bit != 0 {
                    continue;
                }
                seen |= bit;
                match bit {
                    1 => pragma = value.eq_ignore_ascii_case(b"content-type"),
                    2 => {
                        if let Some(encoding) = content_encoding(value) {
                            if charset.is_none() {
                                charset = Some(Some(encoding));
                                need_pragma = Some(true);
                            }
                        }
                    }
                    4 => {
                        charset = Some(label(value));
                        need_pragma = Some(false);
                    }
                    _ => {}
                }
            }
            if need_pragma.is_some_and(|needed| !needed || pragma) {
                if let Some(Some(encoding)) = charset {
                    return Some(meta_label(encoding));
                }
            }
            position += 1; // attribute parsing leaves the closing > untouched.
            continue;
        }
        let tag_name = if remaining.starts_with(b"</") {
            remaining.get(2)
        } else if remaining.starts_with(b"<") {
            remaining.get(1)
        } else {
            None
        };
        if tag_name.is_some_and(u8::is_ascii_alphabetic) {
            while bytes
                .get(position)
                .is_some_and(|byte| !space(*byte) && *byte != b'>')
            {
                position += 1;
            }
            while attribute(bytes, &mut position)?.is_some() {}
            position += 1;
        } else if remaining.starts_with(b"<!")
            || remaining.starts_with(b"</")
            || remaining.starts_with(b"<?")
        {
            position += remaining.iter().position(|byte| *byte == b'>')? + 1;
        } else {
            position += 1;
        }
    }
    None
}

/// None aborts a truncated prescan; Some(None) denotes a complete tag end.
fn attribute<'a>(bytes: &'a [u8], position: &mut usize) -> Option<Option<(&'a [u8], &'a [u8])>> {
    while bytes
        .get(*position)
        .is_some_and(|byte| space(*byte) || *byte == b'/')
    {
        *position += 1;
    }
    if *bytes.get(*position)? == b'>' {
        return Some(None);
    }
    let start = *position;
    loop {
        let byte = *bytes.get(*position)?;
        if space(byte) || byte == b'/' || byte == b'>' || (byte == b'=' && *position > start) {
            break;
        }
        *position += 1;
    }
    let name = &bytes[start..*position];
    while bytes.get(*position).is_some_and(|byte| space(*byte)) {
        *position += 1;
    }
    if *bytes.get(*position)? != b'=' {
        return Some(Some((name, &[])));
    }
    *position += 1;
    while bytes.get(*position).is_some_and(|byte| space(*byte)) {
        *position += 1;
    }
    let first = *bytes.get(*position)?;
    if first == b'\'' || first == b'"' {
        *position += 1;
        let start = *position;
        let count = bytes[start..].iter().position(|byte| *byte == first)?;
        *position += count + 1;
        Some(Some((name, &bytes[start..start + count])))
    } else {
        let start = *position;
        while bytes
            .get(*position)
            .is_some_and(|byte| !space(*byte) && *byte != b'>')
        {
            *position += 1;
        }
        bytes.get(*position)?;
        Some(Some((name, &bytes[start..*position])))
    }
}

fn content_encoding(value: &[u8]) -> Option<&'static str> {
    let mut position = 0;
    while position + 7 <= value.len() {
        let found = value[position..]
            .windows(7)
            .position(|part| part.eq_ignore_ascii_case(b"charset"))?;
        position += found + 7;
        while value.get(position).is_some_and(|byte| space(*byte)) {
            position += 1;
        }
        if value.get(position) != Some(&b'=') {
            continue;
        }
        position += 1;
        while value.get(position).is_some_and(|byte| space(*byte)) {
            position += 1;
        }
        let first = *value.get(position)?;
        if first == b'\'' || first == b'"' {
            position += 1;
            let end = position + value[position..].iter().position(|byte| *byte == first)?;
            return label(&value[position..end]);
        }
        let end = position
            + value[position..]
                .iter()
                .position(|byte| space(*byte) || *byte == b';')
                .unwrap_or(value.len() - position);
        return label(&value[position..end]);
    }
    None
}

fn xml_declaration(bytes: &[u8]) -> Option<&'static str> {
    if !bytes.starts_with(b"<?xml") {
        return None;
    }
    let end = bytes.iter().position(|byte| *byte == b'>')?;
    let bytes = &bytes[..end];
    let mut position = bytes.windows(8).position(|part| part == b"encoding")? + 8;
    while bytes.get(position).is_some_and(|byte| *byte <= 0x20) {
        position += 1;
    }
    if bytes.get(position) != Some(&b'=') {
        return None;
    }
    position += 1;
    while bytes.get(position).is_some_and(|byte| *byte <= 0x20) {
        position += 1;
    }
    let quote = *bytes.get(position)?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    position += 1;
    let end = position + bytes[position..].iter().position(|byte| *byte == quote)?;
    let candidate = &bytes[position..end];
    if candidate.iter().any(|byte| *byte <= 0x20) {
        return None;
    }
    let encoding = label(candidate)?;
    Some(match encoding {
        "UTF-16LE" | "UTF-16BE" => "UTF-8",
        _ => encoding,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_prescan_obeys_meta_precedence_comments_quotes_duplicates_and_bounds() {
        for (bytes, expected) in [
            (
                b"<!--<meta charset=big5>--><META CHARSET='Shift_JIS'>".as_slice(),
                Some("Shift_JIS"),
            ),
            (
                b"<div title=\"<meta charset=big5>\"><meta charset=utf-8>".as_slice(),
                Some("UTF-8"),
            ),
            (b"<meta content='text/html;charset=big5'>".as_slice(), None),
            (
                b"<meta content='text/html;charset=big5' HTTP-EQUIV=Content-Type>".as_slice(),
                Some("Big5"),
            ),
            (
                b"<meta content='text/html;charset=big5' charset=windows-1251>".as_slice(),
                Some("windows-1251"),
            ),
            (
                b"<meta charset=invalid charset=big5><meta charset=utf-16>".as_slice(),
                Some("UTF-8"),
            ),
            (
                b"<meta charset=x-user-defined>".as_slice(),
                Some("windows-1252"),
            ),
            (b"<meta charset='big5'".as_slice(), None),
            (b"<!--><meta charset=big5>".as_slice(), Some("Big5")),
        ] {
            assert_eq!(prescan_html_encoding(bytes), expected, "{bytes:?}");
        }
        let mut late = alloc::vec![b' '; 1024];
        late.extend_from_slice(b"<meta charset=big5>");
        assert_eq!(prescan_html_encoding(&late), None);
        for cut in 0..b"<meta charset='big5'>".len() {
            assert_eq!(
                prescan_html_encoding(&b"<meta charset='big5'>"[..cut]),
                None
            );
        }
    }

    #[test]
    fn html_prescan_xml_fallback_and_utf16_signatures_are_distinct() {
        assert_eq!(
            prescan_html_encoding(b"<?xml version='1.0' encoding='Shift_JIS'?>"),
            Some("Shift_JIS")
        );
        assert_eq!(
            prescan_html_encoding(b"<?xml encoding='UTF-16LE'?>"),
            Some("UTF-8")
        );
        assert_eq!(
            prescan_html_encoding(b"<?xml encoding=' shift_jis'?>"),
            None
        );
        assert_eq!(prescan_html_encoding(b"<\0?\0x\0"), Some("UTF-16LE"));
        assert_eq!(prescan_html_encoding(b"\0<\0?\0x"), Some("UTF-16BE"));
    }
}
