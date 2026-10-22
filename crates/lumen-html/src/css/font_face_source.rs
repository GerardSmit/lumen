//! `@font-face` `src` descriptor grammar: per-component recovery, `format()` and `tech()`.
use super::*;

const FORMAT_KEYWORDS: [&str; 7] = [
    "collection", "opentype", "truetype", "woff", "woff2", "embedded-opentype", "svg",
];
const TECH_KEYWORDS: [&str; 11] = [
    "features-opentype", "features-aat", "features-graphite", "color-colrv0", "color-colrv1",
    "color-svg", "color-sbix", "color-cbdt", "variations", "palettes", "incremental",
];

pub(super) struct ParsedSources {
    pub sources: Vec<FontFaceSource>,
    pub suffixes: Vec<Arc<str>>,
}

fn split_components(raw: &str) -> Option<Vec<Option<&str>>> {
    let parts = syntax::split_top_level_recovering(raw, b',', 64, MAX_CSS_BYTES)?;
    Some(parts.into_iter().map(|part| part.map(str::trim)).collect())
}

fn function_argument<'a>(token: &'a str, name: &str) -> Option<&'a str> {
    let (function, inner) = token.split_once('(')?;
    if !function.eq_ignore_ascii_case(name) {
        return None;
    }
    inner.strip_suffix(')')
}

fn parse_format(token: &str) -> Option<String> {
    let inner = function_argument(token, "format")?.trim();
    if inner.starts_with(['\'', '"']) {
        let value = css_string(inner)?;
        return Some(alloc::format!("format({})", serialize_css_string(&value)));
    }
    let lowered = ascii_lower(inner);
    FORMAT_KEYWORDS
        .contains(&&*lowered)
        .then(|| alloc::format!("format({lowered})"))
}

fn parse_tech(token: &str) -> Option<String> {
    let inner = function_argument(token, "tech")?;
    let mut keywords = Vec::new();
    for keyword in inner.split(',') {
        let keyword = keyword.trim();
        if keyword.contains(|c: char| c.is_ascii_whitespace()) {
            return None;
        }
        let lowered = ascii_lower(keyword);
        if !TECH_KEYWORDS.contains(&&*lowered) {
            return None;
        }
        keywords.push(lowered.into_owned());
    }
    Some(alloc::format!("tech({})", keywords.join(", ")))
}

fn parse_component(part: &str) -> Option<(FontFaceSource, Arc<str>)> {
    let tokens = components_bounded(part, MAX_CSS_BYTES)?;
    let first = *tokens.first()?;
    if let Some(url) = background_url(first) {
        let mut suffix = String::new();
        let mut rest = tokens[1..].iter().copied().peekable();
        if let Some(format) = rest.peek().and_then(|token| parse_format(token)) {
            suffix.push(' ');
            suffix.push_str(&format);
            rest.next();
        }
        if let Some(tech) = rest.peek().and_then(|token| parse_tech(token)) {
            suffix.push(' ');
            suffix.push_str(&tech);
            rest.next();
        }
        if rest.next().is_some() {
            return None;
        }
        return Some((FontFaceSource::Url(url), Arc::from(suffix)));
    }
    if tokens.len() != 1 {
        return None;
    }
    let contents = function_argument(first, "local")?;
    let names = font_families(contents)?;
    let [FontFamily::Named(name)] = names.as_ref() else {
        return None;
    };
    Some((FontFaceSource::Local(name.clone()), Arc::from("")))
}

pub(super) fn parse(raw: &str) -> Option<ParsedSources> {
    let mut sources = Vec::new();
    let mut suffixes = Vec::new();
    for part in split_components(raw)?.into_iter().flatten() {
        if let Some((source, suffix)) = parse_component(part) {
            sources.push(source);
            suffixes.push(suffix);
        }
    }
    (!sources.is_empty()).then_some(ParsedSources { sources, suffixes })
}

pub(super) fn serialize(sources: &[FontFaceSource], suffixes: &[Arc<str>]) -> String {
    sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let suffix = suffixes.get(index).map_or("", |suffix| suffix);
            match source {
                FontFaceSource::Url(url) => alloc::format!("{}{suffix}", serialize_url(url)),
                FontFaceSource::Local(name) => {
                    alloc::format!("local({})", serialize_css_string(name))
                }
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_per_component_and_keeps_tech() {
        let parsed = parse("url(a b/c.ttf), url(x.ttf) format(woff2) tech(Variations, palettes), junk").unwrap();
        assert_eq!(parsed.sources.len(), 1);
        assert_eq!(&*parsed.suffixes[0], " format(woff2) tech(variations, palettes)");
        assert!(parse("url(x.ttf) tech(features-opentype) format(opentype)").is_none());
        assert!(parse("url(x.ttf) format(a, b)").is_none());
    }
}
