//! Static render profile for untrusted markup such as HTML email.
//!
//! The document is parsed with scripting disabled and then sanitized in place on the parsed tree
//! (never serialized and re-parsed). What the output guarantees:
//!
//! - Elements and attributes are filtered by allowlist; `on*`, `srcset`, forms and every element
//!   that can execute or fetch are gone. `<style>` elements go through the same attribute filter.
//! - URLs are classified with the WHATWG parser from `lumen_common::url`. Anything that does not
//!   parse is removed. Links keep only `link_schemes` (plus fragments). Images keep `cid:`,
//!   `data:image/{png,jpeg,gif,webp}` whose base64 payload starts with the matching magic bytes,
//!   and `http(s)` only when the profile allows remote content. Kept URLs are the parser's
//!   serialization, never the author's text.
//! - CSS (`<style>` and `style=""`) is parsed with Lumen's own CSS source parser and declaration
//!   block. Only allowlisted properties survive, written back as longhand `name:value` pairs.
//!   Values containing `var()`, `env()`, `attr()`, escapes, comments or any function outside a
//!   small allowlist are dropped, as are all custom properties, `@import`, `@font-face`, and every
//!   at-rule except `@media`. `url()` is accepted only in `background`/`background-image` and only
//!   for URLs the profile allows. Resource properties (`content`, `cursor`, `mask`, `filter`,
//!   `border-image`, `shape-outside`, `list-style-image` other than `none`) are not allowlisted.
//! - Layout escape hatches are removed: `position` other than `static`/`relative`, `top`/`left`/
//!   `right`/`bottom`/`inset*`, `z-index`, transforms, `clip-path`, `filter`, blend modes, negative
//!   margins, negative `text-indent`/`vertical-align`, and numbers above 20000. Style rules whose
//!   subject is `html`, `body`, `:root`, `:host`, `:scope`, a bare `*` or a bare pseudo-class are
//!   dropped, as are selectors using `:is()`/`:where()`/`:not()`/`:has()` or nesting `&`.
//!
//! Remaining style can still draw over its own box's descendants and `overflow` stays available, so
//! the host must still clip the message area (for example with its own scroll container and
//! `overflow:hidden`) and must not give the sanitized `html`/`body` access to its own chrome.

use alloc::{
    borrow::ToOwned,
    format,
    string::String,
    vec,
    vec::Vec,
};

use lumen_common::url::{parse_url, Url};

use crate::{
    css::{
        declaration_block::DeclarationBlock,
        nesting::{self, SourceRule, SourceRuleKind},
    },
    html, Document, Namespace, NodeId, NodeKind,
};

/// Whether `http(s)` images and CSS `url()` references may stay in the document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteContent {
    /// Remove every remote reference and list it in [`SanitizeReport::blocked_remote`].
    Blocked,
    /// Keep `http(s)` image references (the caller still decides whether anything is fetched).
    Allowed,
}

#[derive(Clone, Debug)]
pub struct UntrustedProfile {
    pub remote_content: RemoteContent,
    pub max_html_bytes: usize,
    pub max_nodes: usize,
    pub max_depth: usize,
    /// Total bytes of sanitized CSS kept over all `<style>` elements and `style` attributes.
    pub max_css_bytes: usize,
    pub allow_style_elements: bool,
    pub allow_inline_style: bool,
    pub allow_svg: bool,
    /// Lowercase schemes that survive in `a[href]` and `area[href]`. Fragments always survive.
    pub link_schemes: &'static [&'static str],
    /// Upper bound for each list in the report.
    pub max_report_entries: usize,
}

impl Default for UntrustedProfile {
    fn default() -> Self {
        Self {
            remote_content: RemoteContent::Blocked,
            max_html_bytes: 1024 * 1024,
            max_nodes: 16_384,
            max_depth: 64,
            max_css_bytes: 128 * 1024,
            allow_style_elements: true,
            allow_inline_style: true,
            allow_svg: false,
            link_schemes: &["http", "https", "mailto"],
            max_report_entries: 256,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkInfo {
    /// The normalized link target (control and bidi characters removed).
    pub href: String,
    /// Visible text of the link, whitespace collapsed and truncated.
    pub text: String,
    /// The text looks like a web address whose host differs from the real target.
    pub text_mismatch: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SanitizeReport {
    /// Distinct remote URLs that were removed, bounded by `max_report_entries`.
    pub blocked_remote: Vec<String>,
    /// Number of removed remote references including repeats and entries past the bound.
    pub blocked_remote_total: usize,
    pub links: Vec<LinkInfo>,
    pub links_total: usize,
    pub removed_elements: usize,
    pub unwrapped_elements: usize,
    pub removed_attributes: usize,
    pub removed_css: usize,
    pub html_truncated: bool,
    pub node_limit_hit: bool,
    pub depth_limit_hit: bool,
    pub css_limit_hit: bool,
}

/// Parse `html` with scripting disabled and sanitize the resulting tree.
pub fn parse_untrusted(html_source: &str, profile: &UntrustedProfile) -> (Document, SanitizeReport) {
    let mut report = SanitizeReport::default();
    let input = truncate_to_boundary(html_source, profile.max_html_bytes);
    report.html_truncated = input.len() < html_source.len();
    let mut document = match html::parse_with_options(input, profile.max_nodes, html::ParseOptions::default()) {
        Ok(document) => document,
        Err(_) => {
            report.node_limit_hit = true;
            html::parse("", profile.max_nodes).unwrap_or_else(|_| Document::new(profile.max_nodes))
        }
    };
    let mut sanitizer = Sanitizer { profile, report, css_used: 0 };
    sanitizer.run(&mut document);
    (document, sanitizer.report)
}

fn truncate_to_boundary(input: &str, max: usize) -> &str {
    if input.len() <= max {
        return input;
    }
    let mut end = max;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Action {
    Keep,
    Remove,
    Unwrap,
}

const KEPT_HTML: &[&str] = &[
    "html", "head", "body", "title", "style", "a", "abbr", "acronym", "address", "area", "article",
    "aside", "b", "bdi", "bdo", "big", "blockquote", "br", "caption", "center", "cite", "code",
    "col", "colgroup", "dd", "del", "details", "dfn", "div", "dl", "dt", "em", "figcaption",
    "figure", "font", "footer", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hgroup", "hr", "i",
    "img", "ins", "kbd", "li", "main", "map", "mark", "nav", "ol", "p", "pre", "q", "rp", "rt",
    "ruby", "s", "samp", "section", "small", "span", "strike", "strong", "sub", "summary", "sup",
    "table", "tbody", "td", "tfoot", "th", "thead", "tr", "tt", "u", "ul", "var", "wbr",
];

const REMOVED_HTML: &[&str] = &[
    "script", "iframe", "frame", "frameset", "object", "embed", "applet", "base", "meta", "link",
    "template", "noembed", "noframes", "audio", "video", "source", "track", "canvas", "param",
    "datalist", "dialog", "slot", "portal", "select", "textarea", "option", "optgroup", "input",
    "keygen", "isindex", "xmp", "plaintext",
];

const KEPT_SVG: &[&str] = &[
    "svg", "g", "defs", "path", "rect", "circle", "ellipse", "line", "polyline", "polygon", "text",
    "tspan", "lineargradient", "radialgradient", "stop", "title", "desc", "clippath", "mask",
    "symbol",
];

const ALLOWED_ATTRIBUTES: &[&str] = &[
    "id", "class", "title", "lang", "dir", "align", "valign", "bgcolor", "width", "height",
    "border", "cellpadding", "cellspacing", "color", "face", "size", "hspace", "vspace", "nowrap",
    "colspan", "rowspan", "span", "abbr", "scope", "headers", "clear", "type", "start", "reversed",
    "value", "compact", "alt", "media", "text", "link", "vlink", "alink", "role", "summary",
    "frame", "rules", "char", "charoff", "open", "shape", "coords", "name", "usemap",
    "bordercolor",
];

enum ImageUrl {
    Local(String),
    /// `shown` goes into the report, `kept` replaces the attribute value when remote content is allowed.
    Remote { shown: String, kept: String },
    Reject,
}

struct Sanitizer<'a> {
    profile: &'a UntrustedProfile,
    report: SanitizeReport,
    css_used: usize,
}

impl Sanitizer<'_> {
    fn html_action(&self, name: &str) -> Action {
        if KEPT_HTML.contains(&name) {
            Action::Keep
        } else if REMOVED_HTML.contains(&name) {
            Action::Remove
        } else {
            Action::Unwrap
        }
    }

    fn element_action(&self, namespace: &Namespace, name: &str) -> Action {
        match namespace {
            Namespace::Html => self.html_action(name),
            Namespace::Svg if self.profile.allow_svg => {
                if KEPT_SVG.contains(&name) {
                    Action::Keep
                } else {
                    Action::Remove
                }
            }
            _ => Action::Remove,
        }
    }

    fn run(&mut self, document: &mut Document) {
        let mut links: Vec<(NodeId, String)> = Vec::new();
        let mut stack = vec![(document.root(), 0usize)];
        while let Some((parent, depth)) = stack.pop() {
            let mut cursor = document.first_child(parent).ok().flatten();
            while let Some(node) = cursor {
                let next = document.next_sibling(node).ok().flatten();
                let element = match document.kind(node) {
                    Ok(NodeKind::Element { namespace, name, .. }) => {
                        let local = match namespace {
                            Namespace::Html => name.as_str(),
                            _ => crate::svg::local_name(name),
                        };
                        Some((namespace.clone(), local.to_ascii_lowercase()))
                    }
                    Ok(NodeKind::Text(_)) | Ok(NodeKind::DocumentType(_)) => {
                        cursor = next;
                        continue;
                    }
                    _ => None,
                };
                let Some((namespace, name)) = element else {
                    drop_node(document, node);
                    cursor = next;
                    continue;
                };
                if depth >= self.profile.max_depth {
                    self.report.depth_limit_hit = true;
                    self.report.removed_elements += 1;
                    drop_node(document, node);
                    cursor = next;
                    continue;
                }
                match self.element_action(&namespace, &name) {
                    Action::Remove => {
                        self.report.removed_elements += 1;
                        drop_node(document, node);
                        cursor = next;
                    }
                    Action::Unwrap => {
                        self.report.unwrapped_elements += 1;
                        cursor = unwrap_node(document, parent, node).or(next);
                    }
                    Action::Keep => {
                        cursor = next;
                        if namespace == Namespace::Html && name == "style" {
                            if !self.profile.allow_style_elements {
                                self.report.removed_elements += 1;
                                drop_node(document, node);
                            } else {
                                self.sanitize_attributes(document, node, &namespace, &name);
                                self.sanitize_style_element(document, node);
                            }
                            continue;
                        }
                        if let Some(href) = self.sanitize_attributes(document, node, &namespace, &name) {
                            links.push((node, href));
                        }
                        stack.push((node, depth + 1));
                    }
                }
            }
        }
        if document.node_count() >= self.profile.max_nodes {
            self.report.node_limit_hit = true;
        }
        for (node, href) in links {
            self.record_link(document, node, href);
        }
    }

    fn record_link(&mut self, document: &Document, node: NodeId, href: String) {
        self.report.links_total += 1;
        if self.report.links.len() >= self.profile.max_report_entries {
            return;
        }
        let mut raw = String::new();
        let _ = document.append_descendant_text(node, &mut raw);
        let text = collapse_text(&raw, 200);
        let text_mismatch = text_mismatches(&text, &href);
        self.report.links.push(LinkInfo { href, text, text_mismatch });
    }

    fn note_blocked(&mut self, url: &str) {
        self.report.blocked_remote_total += 1;
        if self.report.blocked_remote.len() < self.profile.max_report_entries
            && !self.report.blocked_remote.iter().any(|known| known == url)
        {
            self.report.blocked_remote.push(url.to_owned());
        }
    }

    fn classify_image(&self, raw: &str) -> ImageUrl {
        match resolve_reference(raw) {
            None => ImageUrl::Reject,
            Some(Reference::NetworkPath { shown, url }) => ImageUrl::Remote { shown, kept: url.href() },
            Some(Reference::Absolute(url)) => match url.scheme.as_str() {
                "cid" => ImageUrl::Local(url.href()),
                "data" => {
                    let href = url.href();
                    if data_image_is_sound(&href) { ImageUrl::Local(href) } else { ImageUrl::Reject }
                }
                "http" | "https" => {
                    let href = url.href();
                    ImageUrl::Remote { shown: href.clone(), kept: href }
                }
                _ => ImageUrl::Reject,
            },
        }
    }

    fn classify_link(&self, raw: &str) -> Option<String> {
        let cleaned = clean_url(raw);
        if cleaned.starts_with('#') {
            return Some(cleaned);
        }
        let url = parse_url(&cleaned, None)?;
        if self.profile.link_schemes.contains(&url.scheme.as_str()) {
            Some(url.href())
        } else {
            None
        }
    }

    /// Returns the kept value for an image-bearing attribute, or `None` to remove it.
    fn image_attribute(&mut self, value: &str) -> Option<String> {
        match self.classify_image(value) {
            ImageUrl::Local(url) => Some(url),
            ImageUrl::Remote { shown, kept } => match self.profile.remote_content {
                RemoteContent::Allowed => Some(kept),
                RemoteContent::Blocked => {
                    self.note_blocked(&shown);
                    None
                }
            },
            ImageUrl::Reject => None,
        }
    }

    fn sanitize_attributes(
        &mut self,
        document: &mut Document,
        node: NodeId,
        namespace: &Namespace,
        name: &str,
    ) -> Option<String> {
        let attributes: Vec<(String, String)> = match document.kind(node) {
            Ok(NodeKind::Element { attributes, .. }) => attributes
                .iter()
                .map(|(key, value)| (key.as_str().to_owned(), value.clone()))
                .collect(),
            _ => return None,
        };
        let is_html = *namespace == Namespace::Html;
        let mut link = None;
        for (key, value) in attributes {
            let lower = key.to_ascii_lowercase();
            let outcome: Option<String> = if lower.starts_with("on") {
                None
            } else if lower == "style" {
                if self.profile.allow_inline_style {
                    let kept = self.sanitize_declarations_budgeted(&value);
                    if kept.is_empty() { None } else { Some(kept) }
                } else {
                    None
                }
            } else if is_html {
                self.html_attribute(&lower, name, &value, &mut link)
            } else {
                svg_attribute(&lower, &value)
            };
            match outcome {
                Some(kept) if kept == value => {}
                Some(kept) => {
                    if document.set_attribute(node, &key, &kept).is_err() {
                        let _ = document.remove_attribute(node, &key);
                        self.report.removed_attributes += 1;
                    }
                }
                None => {
                    let _ = document.remove_attribute(node, &key);
                    self.report.removed_attributes += 1;
                }
            }
        }
        link
    }

    fn html_attribute(
        &mut self,
        attribute: &str,
        element: &str,
        value: &str,
        link: &mut Option<String>,
    ) -> Option<String> {
        match attribute {
            "href" if element == "a" || element == "area" => {
                let kept = self.classify_link(value)?;
                *link = Some(kept.clone());
                Some(kept)
            }
            "src" if element == "img" => self.image_attribute(value),
            "background" => self.image_attribute(value),
            _ if ALLOWED_ATTRIBUTES.contains(&attribute) => numeric_attribute_in_range(attribute, value)
                .then(|| value.to_owned()),
            _ => None,
        }
    }

    fn sanitize_style_element(&mut self, document: &mut Document, style: NodeId) {
        let mut texts = Vec::new();
        let mut combined = String::new();
        let mut child = document.first_child(style).ok().flatten();
        while let Some(node) = child {
            if let Ok(NodeKind::Text(text)) = document.kind(node) {
                combined.push_str(text);
                texts.push(node);
            }
            child = document.next_sibling(node).ok().flatten();
        }
        let kept = self.sanitize_stylesheet_budgeted(&combined);
        if kept.trim().is_empty() || texts.is_empty() {
            drop_node(document, style);
            return;
        }
        for extra in texts.iter().skip(1) {
            drop_node(document, *extra);
        }
        if document.replace_data(texts[0], &kept).is_err() {
            drop_node(document, style);
        }
    }

    fn take_css_budget(&mut self, len: usize) -> bool {
        if self.css_used.saturating_add(len) > self.profile.max_css_bytes {
            self.report.css_limit_hit = true;
            return false;
        }
        self.css_used += len;
        true
    }

    fn sanitize_stylesheet_budgeted(&mut self, css: &str) -> String {
        if !self.take_css_budget(css.len()) {
            return String::new();
        }
        let Ok(rules) = nesting::parse_source_rules(css, &[], false) else {
            self.report.removed_css += 1;
            return String::new();
        };
        let mut out = String::new();
        self.emit_rules(css, &rules, 0, &mut out);
        out
    }

    fn sanitize_declarations_budgeted(&mut self, css: &str) -> String {
        if !self.take_css_budget(css.len()) {
            return String::new();
        }
        self.sanitize_declarations(css)
    }

    fn emit_rules(&mut self, css: &str, rules: &[SourceRule], depth: usize, out: &mut String) {
        for rule in rules {
            let prelude = css.get(rule.prelude.clone()).unwrap_or("").trim();
            match rule.kind {
                SourceRuleKind::Style => {
                    let mut body = rule.declaration_text(css);
                    for child in &rule.children {
                        if child.kind == SourceRuleKind::NestedDeclarations {
                            body.push(';');
                            body.push_str(&child.declaration_text(css));
                        } else {
                            self.report.removed_css += 1;
                        }
                    }
                    if !selector_is_safe(prelude) {
                        self.report.removed_css += 1;
                        continue;
                    }
                    let declarations = self.sanitize_declarations(&body);
                    if !declarations.is_empty() {
                        out.push_str(prelude);
                        out.push('{');
                        out.push_str(&declarations);
                        out.push('}');
                    }
                }
                SourceRuleKind::Group if depth < 4 && is_media_prelude(prelude) => {
                    let mut inner = String::new();
                    self.emit_rules(css, &rule.children, depth + 1, &mut inner);
                    if !inner.is_empty() {
                        out.push_str(prelude);
                        out.push('{');
                        out.push_str(&inner);
                        out.push('}');
                    }
                }
                SourceRuleKind::Statement => {
                    self.report.removed_css += 1;
                    if prelude.to_ascii_lowercase().starts_with("@import") {
                        self.note_import(prelude);
                    }
                }
                _ => self.report.removed_css += 1,
            }
        }
    }

    fn note_import(&mut self, prelude: &str) {
        let rest = prelude[7..].trim();
        let target = if rest.to_ascii_lowercase().starts_with("url(") {
            css_url_arguments(rest).into_iter().next()
        } else {
            let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'');
            quote.and_then(|q| rest[1..].split(q).next().map(str::to_owned))
        };
        if let Some(target) = target {
            if let ImageUrl::Remote { shown, .. } = self.classify_image(&target) {
                self.note_blocked(&shown);
            }
        }
    }

    fn sanitize_declarations(&mut self, body: &str) -> String {
        let Ok(block) = DeclarationBlock::parse(body) else {
            self.report.removed_css += 1;
            return String::new();
        };
        let mut out = String::new();
        for name in block.names() {
            let Some((value, important)) = block.value(name) else { continue };
            let name = name.to_ascii_lowercase();
            match self.sanitize_declaration(&name, &value) {
                Some(kept) => {
                    out.push_str(&name);
                    out.push(':');
                    out.push_str(&kept);
                    if important {
                        out.push_str(" !important");
                    }
                    out.push(';');
                }
                None => self.report.removed_css += 1,
            }
        }
        out
    }

    fn sanitize_declaration(&mut self, name: &str, value: &str) -> Option<String> {
        if !ALLOWED_PROPERTIES.contains(&name) {
            return None;
        }
        let value = value.trim();
        let url_allowed = matches!(name, "background" | "background-image");
        let scan = scan_value(value, url_allowed)?;
        let lower = value.to_ascii_lowercase();
        if (scan.negative || scan.math) && NON_NEGATIVE_PROPERTIES.iter().any(|prefix| name.starts_with(prefix)) {
            return None;
        }
        match name {
            "position" if !matches!(lower.as_str(), "static" | "relative") => return None,
            "display" if !ALLOWED_DISPLAY.contains(&lower.as_str()) => return None,
            "list-style-image" if lower != "none" => return None,
            _ => {}
        }
        if scan.urls > 0 {
            let arguments = css_url_arguments(value);
            if arguments.len() != scan.urls {
                return None;
            }
            let mut blocked = Vec::new();
            for argument in &arguments {
                match self.classify_image(argument) {
                    ImageUrl::Local(_) => {}
                    ImageUrl::Remote { shown, .. } => match self.profile.remote_content {
                        RemoteContent::Allowed => {}
                        RemoteContent::Blocked => blocked.push(shown),
                    },
                    ImageUrl::Reject => return None,
                }
            }
            if !blocked.is_empty() {
                for url in &blocked {
                    self.note_blocked(url);
                }
                return None;
            }
        }
        Some(value.to_owned())
    }
}

const ALLOWED_PROPERTIES: &[&str] = &[
    "color", "opacity", "direction", "unicode-bidi", "visibility",
    "font", "font-family", "font-size", "font-style", "font-weight", "font-variant",
    "font-variant-caps", "font-variant-ligatures", "font-variant-numeric",
    "font-variant-alternates", "font-variant-east-asian", "font-variant-position",
    "font-feature-settings", "font-kerning", "font-width", "font-stretch", "font-size-adjust",
    "line-height", "letter-spacing", "word-spacing", "text-align", "text-align-last",
    "text-indent", "text-transform", "text-decoration", "text-decoration-line",
    "text-decoration-style", "text-decoration-color", "text-decoration-thickness",
    "text-underline-offset", "text-overflow", "text-shadow", "text-size-adjust",
    "-webkit-text-size-adjust", "-ms-text-size-adjust", "white-space", "word-break",
    "overflow-wrap", "word-wrap", "hyphens", "line-break", "tab-size", "vertical-align",
    "background", "background-color", "background-image", "background-repeat",
    "background-position", "background-position-x", "background-position-y", "background-size",
    "background-clip", "background-origin", "background-attachment",
    "border", "border-top", "border-right", "border-bottom", "border-left",
    "border-width", "border-style", "border-color",
    "border-top-width", "border-right-width", "border-bottom-width", "border-left-width",
    "border-top-style", "border-right-style", "border-bottom-style", "border-left-style",
    "border-top-color", "border-right-color", "border-bottom-color", "border-left-color",
    "border-radius", "border-top-left-radius", "border-top-right-radius",
    "border-bottom-left-radius", "border-bottom-right-radius",
    "border-collapse", "border-spacing", "box-shadow", "box-sizing",
    "margin", "margin-top", "margin-right", "margin-bottom", "margin-left",
    "margin-block", "margin-block-start", "margin-block-end",
    "margin-inline", "margin-inline-start", "margin-inline-end",
    "padding", "padding-top", "padding-right", "padding-bottom", "padding-left",
    "padding-block", "padding-block-start", "padding-block-end",
    "padding-inline", "padding-inline-start", "padding-inline-end",
    "width", "height", "min-width", "min-height", "max-width", "max-height", "aspect-ratio",
    "display", "position", "float", "clear", "overflow", "overflow-x", "overflow-y",
    "list-style", "list-style-type", "list-style-position", "list-style-image",
    "table-layout", "empty-cells", "caption-side", "object-fit", "object-position",
];

const NON_NEGATIVE_PROPERTIES: &[&str] = &["margin", "text-indent", "vertical-align"];

const ALLOWED_DISPLAY: &[&str] = &[
    "none", "block", "inline", "inline-block", "list-item", "table", "inline-table", "table-row",
    "table-cell", "table-row-group", "table-header-group", "table-footer-group", "table-column",
    "table-column-group", "table-caption", "flow-root",
];

const ALLOWED_FUNCTIONS: &[&str] = &[
    "rgb", "rgba", "hsl", "hsla", "hwb", "calc", "min", "max", "clamp", "linear-gradient",
    "radial-gradient", "conic-gradient", "repeating-linear-gradient", "repeating-radial-gradient",
    "repeating-conic-gradient",
];

const MAX_CSS_NUMBER: f64 = 20_000.0;
const MAX_SPAN_ATTRIBUTE: f64 = 1_000.0;

const NUMERIC_ATTRIBUTES: &[&str] = &[
    "width", "height", "border", "cellpadding", "cellspacing", "size", "span", "start", "hspace",
    "vspace", "colspan", "rowspan",
];

/// Whether the leading number of a presentational attribute stays within the layout caps.
fn numeric_attribute_in_range(attribute: &str, value: &str) -> bool {
    if !NUMERIC_ATTRIBUTES.contains(&attribute) {
        return true;
    }
    let trimmed = value.trim_start();
    let end = trimmed.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(trimmed.len());
    let Ok(number) = trimmed[..end].parse::<f64>() else { return true };
    let limit = if matches!(attribute, "colspan" | "rowspan") { MAX_SPAN_ATTRIBUTE } else { MAX_CSS_NUMBER };
    number <= limit
}

struct ValueScan {
    urls: usize,
    negative: bool,
    math: bool,
}

const MATH_FUNCTIONS: &[&str] = &["calc", "min", "max", "clamp"];

fn is_unsafe_text_byte(byte: u8) -> bool {
    matches!(byte, b'\\' | b'{' | b'}' | b';' | b'<' | b'>' | b'@')
}

/// Conservative single-pass check of one canonical declaration value. Returns `None` for escapes,
/// comments, unbalanced brackets or strings, disallowed functions (including `var()`, `env()`,
/// `attr()`), scientific notation and numbers above [`MAX_CSS_NUMBER`].
fn scan_value(value: &str, url_allowed: bool) -> Option<ValueScan> {
    let bytes = value.as_bytes();
    let mut scan = ValueScan { urls: 0, negative: false, math: false };
    let mut groups: Vec<bool> = Vec::new();
    let mut pending_math = false;
    let mut index = 0;
    if bytes.is_empty() {
        return None;
    }
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'"' | b'\'' => {
                index += 1;
                loop {
                    match bytes.get(index) {
                        None | Some(b'\n' | b'\r' | 0x0c) => return None,
                        Some(&found) if found == byte => {
                            index += 1;
                            break;
                        }
                        Some(&found) if is_unsafe_text_byte(found) => return None,
                        Some(_) => index += 1,
                    }
                }
            }
            b'(' => {
                groups.push(core::mem::take(&mut pending_math));
                index += 1;
            }
            b')' => {
                groups.pop()?;
                index += 1;
            }
            b'*' | b'/' if groups.contains(&true) => return None,
            b'#' => {
                index += 1;
                while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'-' || bytes[index] == b'_') {
                    index += 1;
                }
            }
            b'\\' | b'{' | b'}' | b';' | b'<' | b'>' | b'@' | b'!' | b'[' | b']' | b'`' => return None,
            b'/' if bytes.get(index + 1) == Some(&b'*') => return None,
            b'\t' | b'\n' | b'\r' | b' ' => index += 1,
            0..=31 | 127 => return None,
            b'0'..=b'9' | b'.' => {
                let start = index;
                while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'.') {
                    index += 1;
                }
                if matches!(bytes.get(index), Some(b'e' | b'E'))
                    && matches!(bytes.get(index + 1), Some(b'0'..=b'9' | b'+' | b'-'))
                {
                    return None;
                }
                if let Ok(number) = value[start..index].parse::<f64>() {
                    if number > MAX_CSS_NUMBER {
                        return None;
                    }
                }
            }
            _ if byte == b'-' && matches!(bytes.get(index + 1), Some(b'0'..=b'9' | b'.')) => {
                scan.negative = true;
                index += 1;
            }
            _ if byte == b'-' || byte == b'_' || byte.is_ascii_alphabetic() || byte >= 0x80 => {
                let start = index;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric()
                        || bytes[index] == b'-'
                        || bytes[index] == b'_'
                        || bytes[index] >= 0x80)
                {
                    index += 1;
                }
                if bytes.get(index) != Some(&b'(') {
                    continue;
                }
                let function = value[start..index].to_ascii_lowercase();
                if function == "url" && url_allowed {
                    scan.urls += 1;
                    let mut probe = index + 1;
                    while matches!(bytes.get(probe), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                        probe += 1;
                    }
                    if !matches!(bytes.get(probe), Some(b'"' | b'\'')) {
                        let close = value[probe..].find(')')? + probe;
                        let content = &value[probe..close];
                        if content.contains(['(', '"', '\'']) || content.bytes().any(is_unsafe_text_byte) {
                            return None;
                        }
                        index = close + 1;
                    }
                } else if !ALLOWED_FUNCTIONS.contains(&function.as_str()) {
                    return None;
                } else if MATH_FUNCTIONS.contains(&function.as_str()) {
                    scan.math = true;
                    pending_math = true;
                }
            }
            _ => index += 1,
        }
    }
    groups.is_empty().then_some(scan)
}

fn is_media_prelude(prelude: &str) -> bool {
    let Some(rest) = prelude.strip_prefix("@media").or_else(|| prelude.strip_prefix("@MEDIA")) else {
        return false;
    };
    !rest.is_empty()
        && rest.starts_with(|c: char| c.is_ascii_whitespace() || c == '(')
        && rest.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || c.is_ascii_whitespace()
                || matches!(c, '(' | ')' | ':' | ',' | '.' | '-' | '%' | '/' | '=' | '_' | '>')
        })
}

fn selector_is_safe(prelude: &str) -> bool {
    let allowed = |c: char| {
        c.is_ascii_alphanumeric()
            || c.is_ascii_whitespace()
            || matches!(c, '.' | '#' | '*' | '>' | '+' | '~' | ',' | ':' | '(' | ')' | '[' | ']' | '=' | '"' | '\'' | '-' | '_' | '^' | '$' | '%')
    };
    if prelude.is_empty() || !prelude.chars().all(allowed) {
        return false;
    }
    let lower = prelude.to_ascii_lowercase();
    const BANNED: &[&str] = &[
        ":root", ":host", ":scope", ":is(", ":where(", ":not(", ":has(", ":matches(", "-any(",
        ":global", ":local", "::slotted", "::part",
    ];
    if BANNED.iter().any(|needle| lower.contains(needle)) {
        return false;
    }
    split_top_level(&lower, |c| c == ',').into_iter().all(|selector| {
        let compounds = split_top_level(selector, |c| c.is_ascii_whitespace() || matches!(c, '>' | '+' | '~'));
        compounds
            .into_iter()
            .rev()
            .find(|compound| !compound.is_empty())
            .is_some_and(compound_is_anchored)
    })
}

/// A compound selector must pick elements by something other than `html`, `body`, `*` or a bare
/// pseudo-class, which would match the message root.
fn compound_is_anchored(compound: &str) -> bool {
    let (head, rest) = if let Some(rest) = compound.strip_prefix('*') {
        ("*", rest)
    } else {
        let end = compound
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(compound.len());
        compound.split_at(end)
    };
    match head {
        "html" | "body" => false,
        "" | "*" => rest.contains(['.', '#', '[']),
        _ => true,
    }
}

/// Splits at separators outside parentheses, brackets and strings.
fn split_top_level(text: &str, is_separator: impl Fn(char) -> bool) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut quote = None;
    let mut start = 0;
    for (index, c) in text.char_indices() {
        match quote {
            Some(open) => {
                if c == open {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '(' | '[' => depth += 1,
                ')' | ']' => depth = depth.saturating_sub(1),
                _ if depth == 0 && is_separator(c) => {
                    parts.push(&text[start..index]);
                    start = index + c.len_utf8();
                }
                _ => {}
            },
        }
    }
    parts.push(&text[start..]);
    parts
}

fn svg_attribute(attribute: &str, value: &str) -> Option<String> {
    if attribute.contains("href") || attribute == "src" || attribute == "style" {
        return None;
    }
    let lower = value.to_ascii_lowercase();
    if lower.contains("javascript:") || lower.contains("vbscript:") || lower.contains("data:") {
        return None;
    }
    for (position, _) in lower.match_indices("url(") {
        let after = lower[position + 4..].trim_start().trim_start_matches(['"', '\'']);
        if !after.starts_with('#') {
            return None;
        }
    }
    Some(value.to_owned())
}

fn drop_node(document: &mut Document, node: NodeId) {
    if document.remove(node).is_ok() {
        let _ = document.destroy_subtree(node);
    }
}

/// Moves the children of `node` in front of it, deletes `node` and returns the first moved child.
fn unwrap_node(document: &mut Document, parent: NodeId, node: NodeId) -> Option<NodeId> {
    let mut first = None;
    let mut child = document.first_child(node).ok().flatten();
    while let Some(current) = child {
        let following = document.next_sibling(current).ok().flatten();
        if document.insert_before(parent, current, Some(node)).is_err() {
            break;
        }
        if first.is_none() {
            first = Some(current);
        }
        child = following;
    }
    drop_node(document, node);
    first
}

fn is_ignorable_url_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}' | '\u{FEFF}' | '\u{00AD}')
}

/// Trims, then drops control, bidi and zero-width characters the way a browser's URL parser
/// would ignore tabs and newlines, but more strictly.
fn clean_url(raw: &str) -> String {
    raw.trim().chars().filter(|c| !is_ignorable_url_char(*c)).collect()
}

enum Reference {
    Absolute(Url),
    /// `//host/path` or `\\host\path`: resolved against a placeholder `https` base.
    NetworkPath { shown: String, url: Url },
}

const PLACEHOLDER_HOST: &str = "relative.invalid";

/// Absolute URLs and network-path references only; relative references and parse failures are `None`.
fn resolve_reference(raw: &str) -> Option<Reference> {
    let cleaned = clean_url(raw);
    if let Some(url) = parse_url(&cleaned, None) {
        return Some(Reference::Absolute(url));
    }
    let base = parse_url(&alloc::format!("https://{PLACEHOLDER_HOST}/"), None)?;
    let url = parse_url(&cleaned, Some(&base))?;
    if url.host.as_deref() == Some(PLACEHOLDER_HOST) {
        return None;
    }
    Some(Reference::NetworkPath { shown: cleaned, url })
}

/// Accepts `data:image/{png,jpeg,gif,webp};base64,...` whose payload begins with the magic bytes
/// of the declared type.
fn data_image_is_sound(href: &str) -> bool {
    let Some(rest) = href.strip_prefix("data:") else { return false };
    let Some((meta, payload)) = rest.split_once(',') else { return false };
    let meta = meta.to_ascii_lowercase();
    let Some(kind) = meta.strip_suffix(";base64") else { return false };
    let payload = payload.as_bytes();
    let head = &payload[..payload.len().min(16)];
    let Some(bytes) = lumen_common::codec::base64_decode_forgiving(head) else { return false };
    match kind {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        _ => false,
    }
}

/// The argument of every `url(...)` in `value`, without quotes. A malformed call ends the scan.
fn css_url_arguments(value: &str) -> Vec<String> {
    let lower = value.to_ascii_lowercase();
    let mut arguments = Vec::new();
    let mut search = 0;
    while let Some(found) = lower[search..].find("url(") {
        let start = search + found + 4;
        let rest = value[start..].trim_start();
        let offset = value.len() - rest.len();
        let (argument, consumed) = match rest.chars().next() {
            Some(quote @ ('"' | '\'')) => match rest[1..].find(quote) {
                Some(close) => {
                    let after = &rest[close + 2..];
                    let trimmed = after.trim_start();
                    if !trimmed.starts_with(')') {
                        break;
                    }
                    (rest[1..close + 1].to_owned(), close + 2 + (after.len() - trimmed.len()) + 1)
                }
                None => break,
            },
            Some(_) => match rest.find(')') {
                Some(close) => (rest[..close].trim().to_owned(), close + 1),
                None => break,
            },
            None => break,
        };
        arguments.push(argument);
        search = offset + consumed;
    }
    arguments
}

fn collapse_text(raw: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    for word in raw.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
            count += 1;
        }
        for c in word.chars().filter(|c| !is_ignorable_url_char(*c)) {
            if count >= max_chars {
                return out;
            }
            out.push(c);
            count += 1;
        }
    }
    out
}

fn host_of(address: &str) -> Option<String> {
    let lower = address.trim().to_ascii_lowercase();
    let candidate = if lower.starts_with("http://") || lower.starts_with("https://") {
        lower
    } else if lower.starts_with("www.") {
        format!("https://{lower}")
    } else {
        return None;
    };
    let url = parse_url(&candidate, None)?;
    let host = url.host?;
    host.contains('.').then_some(host)
}

fn text_mismatches(text: &str, href: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    if lower.contains(char::is_whitespace) {
        return false;
    }
    let looks_like_address =
        lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("www.");
    if !looks_like_address {
        return false;
    }
    let (Some(shown), Some(real)) = (host_of(&lower), host_of(href)) else {
        return false;
    };
    let strip = |host: &str| host.strip_prefix("www.").unwrap_or(host).to_owned();
    strip(&shown) != strip(&real)
}

impl SanitizeReport {
    pub fn is_clean(&self) -> bool {
        self.removed_elements == 0
            && self.removed_attributes == 0
            && self.removed_css == 0
            && self.blocked_remote_total == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selector;
    use alloc::string::ToString;

    fn run(source: &str) -> (Document, SanitizeReport) {
        parse_untrusted(source, &UntrustedProfile::default())
    }

    fn run_with(source: &str, profile: UntrustedProfile) -> (Document, SanitizeReport) {
        parse_untrusted(source, &profile)
    }

    fn find(document: &Document, query: &str) -> Option<NodeId> {
        selector::query_selector(document, document.root(), query).ok().flatten()
    }

    fn attribute(document: &Document, query: &str, name: &str) -> Option<String> {
        let node = find(document, query)?;
        document
            .get_attribute_ns_ref(node, None, name)
            .ok()
            .flatten()
            .map(str::to_owned)
    }

    fn dump(document: &Document) -> String {
        let root = document.document_element_at(document.root()).ok().flatten().unwrap();
        html::outer_html(document, root).unwrap()
    }

    fn style_text(document: &Document) -> String {
        let node = find(document, "style");
        let mut out = String::new();
        if let Some(node) = node {
            let _ = document.append_descendant_text(node, &mut out);
        }
        out
    }

    #[test]
    fn active_elements_are_removed_and_counted() {
        let (document, report) = run(
            "<p>keep</p><script>alert(1)</script><iframe src=\"x\"></iframe><object data=x></object>\
             <embed src=x><applet></applet><base href=\"http://evil/\"><meta http-equiv=refresh content=\"0;url=x\">\
             <link rel=stylesheet href=\"http://evil/a.css\">",
        );
        for query in ["script", "iframe", "object", "embed", "applet", "base", "meta", "link"] {
            assert!(find(&document, query).is_none(), "{query} survived");
        }
        assert!(dump(&document).contains("keep"));
        assert!(report.removed_elements >= 8);
    }

    #[test]
    fn mixed_case_and_nested_active_elements_are_removed() {
        let (document, _) = run("<ScRiPt>x</sCrIpT><DIV><IFRAME SRC=x></IFRAME></DIV><scr<script>ipt>y</script>");
        assert!(find(&document, "script").is_none());
        assert!(find(&document, "iframe").is_none());
    }

    #[test]
    fn form_controls_are_inert_and_text_stays() {
        let (document, _) = run(
            "<form action=\"http://evil/\">Name <label for=a>Label</label> <input name=a value=secret>\
             <button formaction=\"http://evil/\">Go</button><select><option>o</option></select>\
             <textarea>t</textarea></form>",
        );
        for query in ["form", "input", "button", "select", "textarea", "label", "option"] {
            assert!(find(&document, query).is_none(), "{query} survived");
        }
        let text = dump(&document);
        assert!(text.contains("Name"));
        assert!(text.contains("Label"));
        assert!(text.contains("Go"));
        assert!(!text.contains("secret"));
        assert!(!text.contains("formaction"));
    }

    #[test]
    fn event_handler_and_unknown_attributes_are_removed() {
        let (document, report) = run(
            "<p id=a class=c onclick=\"x()\" onmouseover=y ONLOAD=z formaction=q ping=p data-x=1>t</p>",
        );
        assert_eq!(attribute(&document, "p", "class").as_deref(), Some("c"));
        for name in ["onclick", "onmouseover", "onload", "formaction", "ping", "data-x"] {
            assert!(attribute(&document, "p", name).is_none(), "{name} survived");
        }
        assert!(report.removed_attributes >= 6);
    }

    #[test]
    fn dangerous_link_schemes_are_removed_including_obfuscation() {
        let cases = [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "  javascript:alert(1)",
            "java\tscript:alert(1)",
            "java\nscript:alert(1)",
            "\u{1}javascript:alert(1)",
            "java\u{0}script:alert(1)",
            "java\u{200B}script:alert(1)",
            "&#106;avascript:alert(1)",
            "&#x6A;avascript&colon;alert(1)",
            "jav&Tab;ascript:alert(1)",
            "vbscript:msgbox",
            "VBScript:msgbox",
            "data:text/html,<script>alert(1)</script>",
            "file:///etc/passwd",
            "ftp://x/y",
            "relative/path",
        ];
        for case in cases {
            let source = alloc::format!("<a href=\"{case}\">x</a>");
            let (document, report) = run(&source);
            assert!(attribute(&document, "a", "href").is_none(), "href kept for {case:?}");
            assert!(report.links.is_empty(), "link recorded for {case:?}");
        }
    }

    #[test]
    fn single_quoted_and_unquoted_scheme_attacks_are_removed() {
        for source in [
            "<a href='javascript:alert(1)'>x</a>",
            "<a href=javascript:alert(1)>x</a>",
            "<a href=\"&#x20;javascript:alert(1)\">x</a>",
        ] {
            let (document, _) = run(source);
            assert!(attribute(&document, "a", "href").is_none(), "{source}");
        }
    }

    #[test]
    fn safe_links_are_kept_and_reported() {
        let (document, report) = run(
            "<a href=\"https://example.org/a?b=1\">Example <b>site</b></a>\
             <a href=\"mailto:a@example.org\">mail</a><a href=\"#top\">top</a>",
        );
        assert_eq!(
            attribute(&document, "a", "href").as_deref(),
            Some("https://example.org/a?b=1")
        );
        assert_eq!(report.links.len(), 3);
        assert_eq!(report.links[0].href, "https://example.org/a?b=1");
        assert_eq!(report.links[0].text, "Example site");
        assert!(!report.links[0].text_mismatch);
        assert_eq!(report.links[1].href, "mailto:a@example.org");
    }

    #[test]
    fn link_text_that_names_another_host_is_flagged() {
        let (_, report) = run(
            "<a href=\"http://evil.example/login\">https://bank.example/login</a>\
             <a href=\"https://bank.example/x\">https://www.bank.example/y</a>\
             <a href=\"http://evil.example/\">click here</a>\
             <a href=\"http://user@evil.example/\">www.bank.example</a>",
        );
        assert!(report.links[0].text_mismatch);
        assert!(!report.links[1].text_mismatch);
        assert!(!report.links[2].text_mismatch);
        assert!(report.links[3].text_mismatch);
    }

    #[test]
    fn link_hrefs_lose_bidi_and_control_characters() {
        let (_, report) = run("<a href=\"https://exa\u{202E}mple.org/\u{200B}x\">t</a>");
        assert_eq!(report.links[0].href, "https://example.org/x");
    }

    #[test]
    fn remote_images_are_blocked_and_listed() {
        let (document, report) = run(
            "<img src=\"https://tracker.example/p.gif\" alt=\"pixel\" srcset=\"https://t.example/2x.gif 2x\">\
             <img src=\"//cdn.example/a.png\">",
        );
        assert!(attribute(&document, "img", "src").is_none());
        assert!(attribute(&document, "img", "srcset").is_none());
        assert_eq!(attribute(&document, "img", "alt").as_deref(), Some("pixel"));
        assert_eq!(
            report.blocked_remote,
            vec!["https://tracker.example/p.gif".to_string(), "//cdn.example/a.png".to_string()]
        );
        assert_eq!(report.blocked_remote_total, 2);
    }

    fn data_uri(mime: &str, bytes: &[u8]) -> String {
        alloc::format!("data:{mime};base64,{}", lumen_common::codec::base64_encode(bytes, false, true))
    }

    const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    #[test]
    fn cid_and_data_images_are_kept() {
        let png = data_uri("image/png", PNG_MAGIC);
        let source = alloc::format!(
            "<img id=a src=\"cid:logo@mail\"><img id=b src=\"{png}\">\
             <img id=c src=\"data:text/html,x\"><img id=d src=\"data:image/svg+xml,<svg/>\">\
             <img id=e src=\"javascript:alert(1)\">"
        );
        let (document, report) = run(&source);
        assert_eq!(attribute(&document, "#a", "src").as_deref(), Some("cid:logo@mail"));
        assert_eq!(attribute(&document, "#b", "src").as_deref(), Some(png.as_str()));
        for id in ["#c", "#d", "#e"] {
            assert!(attribute(&document, id, "src").is_none(), "{id}");
        }
        assert!(report.blocked_remote.is_empty());
    }

    #[test]
    fn data_images_must_match_their_declared_magic_bytes() {
        let gif = data_uri("image/gif", b"GIF89a\x01\0\x01\0\0\0\0\0");
        let jpeg = data_uri("image/jpeg", &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F']);
        let webp = data_uri("image/webp", b"RIFF\x10\0\0\0WEBPVP8 ");
        let png = data_uri("image/png", PNG_MAGIC);
        let good = [&gif, &jpeg, &webp, &png];
        let bad = [
            data_uri("image/png", b"GIF89a\x01\0\x01\0\0\0\0\0"),
            data_uri("image/png", b"<svg xmlns=\"http://www.w3.org/2000/svg\" onload=x>"),
            data_uri("image/jpeg", PNG_MAGIC),
            data_uri("image/bmp", b"BM\0\0\0\0\0\0\0\0\0\0\0\0"),
            data_uri("image/svg+xml", b"<svg onload=alert(1)>"),
            data_uri("image/svg+xml", PNG_MAGIC),
            alloc::format!("data:image/png,{}", "%89PNG"),
            "data:image/png;base64,AAAA".to_string(),
        ];
        for uri in good {
            let (document, _) = run(&alloc::format!("<img src=\"{uri}\">"));
            assert_eq!(attribute(&document, "img", "src").as_deref(), Some(uri.as_str()), "{uri}");
        }
        for uri in &bad {
            let (document, _) = run(&alloc::format!("<img src=\"{uri}\">"));
            assert!(attribute(&document, "img", "src").is_none(), "{uri}");
        }
    }

    #[test]
    fn allowed_remote_content_keeps_http_images() {
        let profile = UntrustedProfile { remote_content: RemoteContent::Allowed, ..UntrustedProfile::default() };
        let (document, report) = run_with(
            "<img src=\"https://a.example/p.png\"><div style=\"background:url(https://a.example/b.png)\">x</div>",
            profile,
        );
        assert_eq!(attribute(&document, "img", "src").as_deref(), Some("https://a.example/p.png"));
        assert!(attribute(&document, "div", "style").unwrap().contains("https://a.example/b.png"));
        assert!(report.blocked_remote.is_empty());
    }

    #[test]
    fn allowed_remote_content_still_rejects_script_urls() {
        let profile = UntrustedProfile { remote_content: RemoteContent::Allowed, ..UntrustedProfile::default() };
        let (document, _) = run_with(
            "<img src=\"javascript:x\"><div style=\"background:url(javascript:alert(1))\">x</div>",
            profile,
        );
        assert!(attribute(&document, "img", "src").is_none());
        assert!(attribute(&document, "div", "style").is_none());
    }

    #[test]
    fn background_attribute_follows_remote_policy() {
        let (document, report) = run(
            "<table id=a background=\"http://t.example/bg.png\"><tr><td id=b background=\"cid:bg\">x</td></tr></table>",
        );
        assert!(attribute(&document, "#a", "background").is_none());
        assert_eq!(attribute(&document, "#b", "background").as_deref(), Some("cid:bg"));
        assert_eq!(report.blocked_remote, vec!["http://t.example/bg.png".to_string()]);
    }

    #[test]
    fn style_element_loses_remote_references_and_keeps_the_rest() {
        let (document, report) = run(
            "<style>@import url(http://evil.example/a.css);\
             a{background:url(http://evil.example/i.png);color:red}\
             b{background:url(cid:abc)}\
             @font-face{font-family:x;src:url(http://evil.example/f.woff)}\
             p{margin:0}</style><a>x</a>",
        );
        let css = style_text(&document);
        assert!(!css.contains("evil.example"), "{css}");
        assert!(!css.contains("@import"));
        assert!(!css.contains("@font-face"));
        assert!(css.contains("color:red"));
        assert!(css.contains("cid:abc"), "{css}");
        assert!(css.contains("margin-top:0"), "{css}");
        assert!(report.blocked_remote.contains(&"http://evil.example/a.css".to_string()));
        assert!(report.blocked_remote.contains(&"http://evil.example/i.png".to_string()));
    }

    #[test]
    fn media_rules_are_sanitized_recursively() {
        let (document, _) = run(
            "<style>@media (max-width:600px){a{color:red;background:url(http://x.example/i.png)}}\
             @keyframes k{from{left:0}}</style>",
        );
        let css = style_text(&document);
        assert!(css.contains("@media (max-width:600px)"));
        assert!(css.contains("color:red"));
        assert!(!css.contains("x.example"));
        assert!(!css.contains("@keyframes"));
    }

    #[test]
    fn style_attribute_is_sanitized_declaration_by_declaration() {
        let (document, _) = run(
            "<p style=\"background:url('http://x.example/i.png');color:red;margin:0\">t</p>",
        );
        let style = attribute(&document, "p", "style").unwrap();
        assert!(style.contains("color:red"));
        assert!(style.contains("margin-top:0"), "{style}");
        assert!(!style.contains("url"));
    }

    #[test]
    fn css_obfuscation_is_rejected() {
        let styles = [
            "background:u\\72l(http://x.example/i.png)",
            "background:ur/**/l(http://x.example/i.png)",
            "width:expression(alert(1))",
            "behavior:url(x.htc)",
            "-moz-binding:url(x.xml#b)",
            "background:image-set(\"http://x.example/a.png\" 1x)",
            "background:-webkit-image-set(url(http://x.example/a.png) 1x)",
            "cursor:url(http://x.example/c.cur),auto",
            "content:url(http://x.example/c.png)",
            "--x:url(http://x.example/c.png)",
            "background:URL( \"HTTP://x.example/i.png\" )",
            "background:url(javascript:alert(1))",
            "list-style:url(\"data:text/html,x\")",
        ];
        for style in styles {
            let source = alloc::format!("<p style='{style}'>t</p>");
            let (document, _) = run(&source);
            let dumped = dump(&document);
            assert!(!dumped.contains("x.example"), "{style}: {dumped}");
            assert!(!dumped.contains("expression"), "{style}: {dumped}");
            assert!(!dumped.contains("javascript"), "{style}: {dumped}");
            assert!(!dumped.contains("behavior"), "{style}: {dumped}");
            assert!(!dumped.contains("-moz-binding"), "{style}: {dumped}");
        }
    }

    #[test]
    fn fixed_positioning_and_z_index_are_dropped() {
        let (document, _) = run("<div style=\"position:fixed;z-index:9999;top:0;color:red\">x</div>");
        let style = attribute(&document, "div", "style").unwrap();
        assert!(!style.contains("position"));
        assert!(!style.contains("z-index"));
        assert!(style.contains("color:red"));
    }

    #[test]
    fn style_options_can_disable_style_content() {
        let profile = UntrustedProfile {
            allow_style_elements: false,
            allow_inline_style: false,
            ..UntrustedProfile::default()
        };
        let (document, _) = run_with("<style>a{color:red}</style><p style=\"color:red\">t</p>", profile);
        assert!(find(&document, "style").is_none());
        assert!(attribute(&document, "p", "style").is_none());
    }

    #[test]
    fn css_budget_is_enforced() {
        let profile = UntrustedProfile { max_css_bytes: 16, ..UntrustedProfile::default() };
        let (document, report) = run_with(
            "<style>a{color:red;margin:0;padding:0;border:1px solid red}</style>",
            profile,
        );
        assert!(find(&document, "style").is_none());
        assert!(report.css_limit_hit);
    }

    #[test]
    fn svg_and_mathml_are_removed_by_default() {
        let (document, _) = run(
            "<p>a</p><svg onload=\"x()\"><script>alert(1)</script><rect/></svg><math><mi>x</mi></math>",
        );
        assert!(find(&document, "svg").is_none());
        assert!(find(&document, "math").is_none());
        assert!(dump(&document).contains("a"));
    }

    #[test]
    fn allowed_svg_drops_script_foreign_object_animation_and_hrefs() {
        let profile = UntrustedProfile { allow_svg: true, ..UntrustedProfile::default() };
        let (document, _) = run_with(
            "<svg onload=\"x()\" width=\"10\"><script>alert(1)</script>\
             <foreignObject><div>x</div></foreignObject><a xlink:href=\"javascript:x\"><rect id=r width=\"5\" fill=\"url(http://x.example/p)\" style=\"fill:red\"/></a>\
             <animate attributeName=href values=\"javascript:x\"/><set attributeName=href to=\"javascript:x\"/>\
             <use href=\"http://x.example/s.svg#a\"/><circle id=c fill=\"url(#grad)\"/></svg>",
            profile,
        );
        let dumped = dump(&document);
        assert!(find(&document, "svg").is_some());
        assert!(!dumped.contains("script"), "{dumped}");
        assert!(!dumped.contains("foreignObject"), "{dumped}");
        assert!(!dumped.contains("animate"), "{dumped}");
        assert!(!dumped.contains("javascript"), "{dumped}");
        assert!(!dumped.contains("x.example"), "{dumped}");
        assert!(!dumped.contains("onload"), "{dumped}");
        assert!(dumped.contains("url(#grad)"));
        assert!(dumped.contains("width=\"10\""));
    }

    #[test]
    fn comments_and_noscript_wrappers_do_not_carry_active_content() {
        let (document, _) = run(
            "<!--[if mso]><script>alert(1)</script><![endif]--><noscript><p title=\"</noscript><img src=x onerror=alert(1)>\">n</p></noscript>",
        );
        assert!(find(&document, "img").is_none());
        assert!(find(&document, "noscript").is_none());
        assert!(!dump(&document).contains("--"));
        assert!(attribute(&document, "p", "onerror").is_none());
    }

    #[test]
    fn unknown_elements_are_unwrapped_and_children_sanitized() {
        let (document, report) = run("<custom-box><b onclick=x>in</b><script>s</script></custom-box>");
        assert!(find(&document, "custom-box").is_none());
        assert!(find(&document, "script").is_none());
        assert!(attribute(&document, "b", "onclick").is_none());
        assert!(dump(&document).contains("in"));
        assert!(report.unwrapped_elements >= 1);
    }

    #[test]
    fn size_node_and_depth_limits_are_reported() {
        let profile = UntrustedProfile { max_html_bytes: 8, ..UntrustedProfile::default() };
        let (_, report) = run_with("<p>héllo wörld this is long</p>", profile);
        assert!(report.html_truncated);

        let profile = UntrustedProfile { max_nodes: 8, ..UntrustedProfile::default() };
        let (_, report) = run_with("<p>a</p><p>b</p><p>c</p><p>d</p><p>e</p><p>f</p><p>g</p><p>h</p>", profile);
        assert!(report.node_limit_hit);

        let profile = UntrustedProfile { max_depth: 4, ..UntrustedProfile::default() };
        let deep = "<div>".repeat(20) + "deep";
        let (document, report) = run_with(&deep, profile);
        assert!(report.depth_limit_hit);
        assert!(!dump(&document).contains("deep"));
    }

    #[test]
    fn report_bounds_hold() {
        let profile = UntrustedProfile { max_report_entries: 2, ..UntrustedProfile::default() };
        let mut source = String::new();
        for index in 0..10 {
            source.push_str(&alloc::format!(
                "<a href=\"https://e.example/{index}\">l</a><img src=\"https://t.example/{index}.gif\">"
            ));
        }
        let (_, report) = run_with(&source, profile);
        assert_eq!(report.links.len(), 2);
        assert_eq!(report.links_total, 10);
        assert_eq!(report.blocked_remote.len(), 2);
        assert_eq!(report.blocked_remote_total, 10);
    }

    #[test]
    fn clean_document_reports_clean() {
        let (_, report) = run("<p class=\"a\">hello <b>world</b></p>");
        assert!(report.is_clean());
    }

    #[test]
    fn css_url_arguments_handle_quotes_and_spacing() {
        assert_eq!(css_url_arguments("url( a.png )"), vec!["a.png".to_string()]);
        assert_eq!(css_url_arguments("url(\"a b.png\") url('c')"), vec!["a b.png".to_string(), "c".to_string()]);
        assert!(css_url_arguments("url(\"unterminated").is_empty());
    }

    fn declarations_of(style: &str) -> String {
        let source = alloc::format!("<p style='{style}'>t</p>");
        let (document, _) = run(&source);
        attribute(&document, "p", "style").unwrap_or_default()
    }

    fn assert_no_resource(style: &str) {
        let source = alloc::format!("<p style='{style}'>t</p><style>p{{{style}}}</style>");
        let (document, _) = run(&source);
        let dumped = dump(&document).to_ascii_lowercase();
        assert!(!dumped.contains("url("), "{style}: {dumped}");
        assert!(!dumped.contains("t.example"), "{style}: {dumped}");
    }

    #[test]
    fn var_assembled_url_is_dropped() {
        assert_no_resource("--a:ur;--b:l(http://t.example/p.gif);background-image:var(--a)var(--b)");
        assert_no_resource("--a:ur;--b:l(http://t.example/p.gif);background:var(--a)var(--b)");
        assert_no_resource("background-image:VAR(--a)");
        assert_no_resource("--u:url(http://t.example/p.gif);background:var(--u)");
        let kept = declarations_of("--x:1;color:red;width:var(--x);height:env(safe-area-inset-top);margin:attr(x)");
        assert!(kept.contains("color:red"));
        for forbidden in ["--x", "var(", "env(", "attr(", "width", "height", "margin"] {
            assert!(!kept.contains(forbidden), "{forbidden} in {kept}");
        }
    }

    #[test]
    fn css_escapes_do_not_bypass_filters() {
        for style in [
            "background:u\\rl(http://t.example/p.gif)",
            "background:\\75 rl(http://t.example/p.gif)",
            "background:\\75\\72\\6c(http://t.example/p.gif)",
            "background:ur\\6c(http://t.example/p.gif)",
            "backgr\\6fund:url(http://t.example/p.gif)",
            "\\62ackground-image:url(http://t.example/p.gif)",
            "background:\\76 ar(--x)",
            "background:u/**/rl(http://t.example/p.gif)",
            "background:image-set(\"http://t.example/a.png\" 1x)",
            "background:-webkit-image-set(url(http://t.example/a.png) 1x)",
            "background:cross-fade(url(http://t.example/a.png),url(http://t.example/b.png))",
            "background:element(#x)",
            "background:paint(x)",
            "background:src(\"http://t.example/a.png\")",
        ] {
            assert_no_resource(style);
            let kept = declarations_of(style);
            assert!(!kept.contains('\\'), "{style}: {kept}");
        }
    }

    #[test]
    fn only_allowlisted_properties_survive() {
        let kept = declarations_of(
            "color:red;content:\"x\";cursor:pointer;mask:none;filter:blur(2px);shape-outside:circle(50%);\
             clip-path:circle(5px);transform:scale(9);translate:10px;rotate:5deg;mix-blend-mode:multiply;\
             backdrop-filter:blur(2px);inset:0;top:0;left:0;z-index:5;border-image:linear-gradient(red,blue) 1;\
             animation:x 1s;pointer-events:none;font-family:Arial",
        );
        assert!(kept.contains("color:red"));
        assert!(kept.contains("font-family:Arial"), "{kept}");
        for forbidden in [
            "content", "cursor", "mask", "filter", "shape-outside", "clip-path", "transform", "translate",
            "rotate", "blend", "inset", "top", "left", "z-index", "border-image", "animation", "pointer",
        ] {
            assert!(!kept.contains(forbidden), "{forbidden} in {kept}");
        }
    }

    #[test]
    fn values_with_unlisted_functions_or_large_numbers_are_dropped() {
        let kept = declarations_of(
            "color:rgb(1,2,3);width:calc(100% - 4px);background-image:linear-gradient(red,blue);\
             height:99999px;font-size:1e9px;color:foo(1);list-style-image:linear-gradient(red,blue)",
        );
        assert!(kept.contains("rgb(1"), "{kept}");
        assert!(kept.contains("calc("), "{kept}");
        assert!(kept.contains("linear-gradient("), "{kept}");
        assert!(!kept.contains("99999"), "{kept}");
        assert!(!kept.contains("1e9"), "{kept}");
        assert!(!kept.contains("foo("), "{kept}");
        assert!(!kept.contains("list-style-image"), "{kept}");
    }

    #[test]
    fn positioning_and_negative_offsets_are_dropped() {
        let kept = declarations_of(
            "position:absolute;margin:-100px 0 0 0;margin-left:-5px;text-indent:-9999px;vertical-align:-3px;\
             padding:2px;width:50%;overflow:hidden;opacity:0.5",
        );
        for forbidden in ["position", "-100px", "-5px", "text-indent", "vertical-align", "-3px"] {
            assert!(!kept.contains(forbidden), "{forbidden} in {kept}");
        }
        for wanted in ["padding-top:2px", "width:50%", "overflow", "opacity:0.5"] {
            assert!(kept.contains(wanted), "{wanted} missing in {kept}");
        }
        assert!(declarations_of("position:relative").contains("position:relative"));
        assert!(declarations_of("position:sticky").is_empty());
        assert!(declarations_of("margin:0 auto").contains("margin-left:auto"));
        assert!(declarations_of("margin:calc(0px - 4px)").is_empty());
    }

    #[test]
    fn hex_colors_survive() {
        for color in ["#333333", "#999999", "#1e90ff", "#12e456", "#11223344", "#abc"] {
            let kept = declarations_of(&alloc::format!("color:{color};background-color:{color}"));
            assert!(kept.contains("color:"), "{color}: {kept}");
            assert!(kept.contains("background-color:"), "{color}: {kept}");
        }
        let kept = declarations_of("color:#999999");
        assert!(kept.contains("999") || kept.contains("rgb"), "{kept}");
    }

    #[test]
    fn math_functions_cannot_make_negative_offsets() {
        for style in [
            "margin-top:calc(1em - 5em)",
            "margin-left:calc(0% - 4px)",
            "margin:min(-1px, 0px)",
            "text-indent:max(-9px, -8px)",
            "vertical-align:clamp(-1px, 0px, 1px)",
        ] {
            let kept = declarations_of(style);
            assert!(!kept.contains("url(") && !kept.contains("font-family") && !kept.contains('<'), "{style}: {kept}");
        }
        assert!(declarations_of("width:calc(100% - 4px)").contains("calc("));
    }

    #[test]
    fn math_with_multiplication_or_division_is_dropped() {
        assert!(declarations_of("width:calc(20000px*20000)").is_empty());
        assert!(declarations_of("font:12px/1.5 Arial").contains("font"));
    }

    #[test]
    fn markup_characters_in_strings_and_urls_are_dropped() {
        for style in [
            "font-family:\"</style><script>\"",
            "font-family:'a{b'",
            "background:url(cid:</style>)",
            "background:url(cid:a;b)",
            "background:url(\"cid:</style>\")",
        ] {
            let source = alloc::format!("<p style='{style}'>t</p>");
            let (document, _) = run(&source);
            let dumped = dump(&document);
            assert!(!dumped.contains("</style><"), "{style}: {dumped}");
            assert!(!dumped.contains("<script"), "{style}: {dumped}");
            let kept = declarations_of(style);
            assert!(!kept.contains("url(") && !kept.contains("font-family") && !kept.contains('<'), "{style}: {kept}");
        }
        assert!(declarations_of("font-family:\"Helvetica Neue\",Arial").contains("Helvetica Neue"));
        assert!(declarations_of("background:url(cid:logo)").contains("cid:logo"));
    }

    #[test]
    fn oversized_numeric_attributes_are_dropped() {
        let (document, _) = run(
            "<table width=99999 height=20000 cellpadding=1e9 border=30000 cellspacing=2><tr>\
             <td colspan=5000 rowspan=3>a</td><td colspan=1000 width=50%>b</td></tr></table>\
             <ol start=999999999999999999999999><li>x</li></ol>",
        );
        for name in ["width", "border"] {
            assert!(attribute(&document, "table", name).is_none(), "{name}");
        }
        assert_eq!(attribute(&document, "table", "height").as_deref(), Some("20000"));
        assert_eq!(attribute(&document, "table", "cellspacing").as_deref(), Some("2"));
        assert!(attribute(&document, "td", "colspan").is_none());
        assert_eq!(attribute(&document, "td", "rowspan").as_deref(), Some("3"));
        assert_eq!(attribute(&document, "td:nth-child(2)", "colspan").as_deref(), Some("1000"));
        assert_eq!(attribute(&document, "td:nth-child(2)", "width").as_deref(), Some("50%"));
        assert!(attribute(&document, "ol", "start").is_none());
    }

    #[test]
    fn display_is_limited_to_flow_and_table_keywords() {
        assert!(declarations_of("display:block").contains("display:block"));
        assert!(declarations_of("display:table-cell").contains("display:table-cell"));
        assert!(declarations_of("display:flex").is_empty());
        assert!(declarations_of("display:grid").is_empty());
    }

    #[test]
    fn rules_targeting_the_message_root_are_dropped() {
        let (document, _) = run(
            "<style>html{color:red}body{color:red}:root{color:red}*{color:red}:host{color:red}\
             body.x,p{color:red}.a body{color:red}:hover{color:red}*:first-child{color:red}\
             :is(html,p){color:red}:not(.q){color:red}&{color:red}html>body{color:red}\
             .keep{color:blue}p.keep2{color:green}a:hover{color:navy}*.star{color:teal}\
             @media (max-width:5px){body{color:red}.m{color:olive}}</style>",
        );
        let css = style_text(&document);
        for forbidden in ["html", "body", "root", "host", "red", ":is", ":not", "&"] {
            assert!(!css.contains(forbidden), "{forbidden} in {css}");
        }
        for wanted in [".keep{", "p.keep2{", "a:hover{", "*.star{", ".m{"] {
            assert!(css.contains(wanted), "{wanted} missing in {css}");
        }
    }

    #[test]
    fn selector_preludes_with_escapes_or_odd_characters_are_dropped() {
        let (document, _) = run(
            "<style>.\\62ody{color:red}p\\{x{color:red}.a{color:blue}[x=\"</style>y\"]{color:red}</style>",
        );
        let css = style_text(&document);
        assert!(css.contains(".a{"), "{css}");
        assert!(!css.contains("red"), "{css}");
        assert!(!css.contains('\\') && !css.contains('<'), "{css}");
    }

    #[test]
    fn at_rules_other_than_media_are_dropped() {
        let (document, report) = run(
            "<style>@import url(http://t.example/a.css);@import 'http://t.example/b.css';\
             @font-face{font-family:x;src:url(data:font/woff;base64,AAAA)}@layer a{p{color:red}}\
             @supports (display:grid){p{color:red}}@scope (.a){p{color:red}}\
             @keyframes k{from{color:red}}@property --x{syntax:'*';inherits:false}@namespace svg url(http://t.example/);\
             .ok{color:blue}</style>",
        );
        let css = style_text(&document);
        assert_eq!(css, ".ok{color:blue;}");
        assert!(report.blocked_remote.contains(&"http://t.example/a.css".to_string()));
        assert!(report.blocked_remote.contains(&"http://t.example/b.css".to_string()));
    }

    #[test]
    fn nested_style_rules_do_not_leak_declarations() {
        let (document, _) = run("<style>.a{color:blue;&.b{background:url(http://t.example/p.gif)}margin-top:1px}</style>");
        let css = style_text(&document);
        assert!(css.contains("color:blue"), "{css}");
        assert!(css.contains("margin-top:1px"), "{css}");
        assert!(!css.contains("t.example") && !css.contains("&"), "{css}");
    }

    #[test]
    fn style_element_attributes_are_filtered() {
        let (document, report) = run(
            "<style onload=\"alert(1)\" nonce=abc src=\"http://t.example/x\" media=screen>.a{color:red}</style>",
        );
        let node = find(&document, "style").unwrap();
        for name in ["onload", "nonce", "src"] {
            assert!(attribute(&document, "style", name).is_none(), "{name} survived on {node:?}");
        }
        assert_eq!(attribute(&document, "style", "media").as_deref(), Some("screen"));
        assert!(report.removed_attributes >= 3);
    }

    #[test]
    fn url_scheme_tricks_are_classified_by_the_whatwg_parser() {
        let hrefs = [
            "java\tscript:alert(1)",
            " JAVASCRIPT:alert(1)",
            "jav&#x09;ascript:alert(1)",
            "jav&#x0A;ascript:alert(1)",
            "javascript&colon;alert(1)",
            "%6Aavascript:alert(1)",
            "&#37;6Aavascript:alert(1)",
            "javascript%3Aalert(1)",
            "\u{1}javascript:alert(1)",
            "data:text/html;base64,PHNjcmlwdD4=",
            "//evil.example/x",
            "\\\\evil.example\\x",
            "/relative",
        ];
        for href in hrefs {
            let (document, report) = run(&alloc::format!("<a href=\"{href}\">x</a>"));
            assert!(attribute(&document, "a", "href").is_none(), "{href:?}");
            assert!(report.links.is_empty(), "{href:?}");
        }
    }

    #[test]
    fn backslash_hosts_are_normalized_and_blocked() {
        let (document, report) = run(
            "<img src=\"http:\\\\evil.example\\p.gif\"><img src=\"http:/\\evil2.example/p.gif\">\
             <a href=\"http:\\\\good.example\\x\">t</a>",
        );
        assert!(attribute(&document, "img", "src").is_none());
        assert!(report.blocked_remote.contains(&"http://evil.example/p.gif".to_string()), "{:?}", report.blocked_remote);
        assert!(report.blocked_remote.contains(&"http://evil2.example/p.gif".to_string()), "{:?}", report.blocked_remote);
        assert_eq!(attribute(&document, "a", "href").as_deref(), Some("http://good.example/x"));
    }

    #[test]
    fn relative_and_unparsable_image_urls_are_removed() {
        for src in ["logo.png", "/logo.png", "?x", "http://", "http://a b/", "ht tp://x/y.png", "file:///etc/passwd"] {
            let (document, _) = run(&alloc::format!("<img src=\"{src}\">"));
            assert!(attribute(&document, "img", "src").is_none(), "{src}");
        }
    }

    #[test]
    fn host_comparison_uses_the_url_parser() {
        let (_, report) = run(
            "<a href=\"http://evil.example:8080/x\">https://bank.example/</a>\
             <a href=\"https://bank.example/\">https://BANK.example:443/a</a>\
             <a href=\"mailto:a@bank.example\">www.bank.example</a>",
        );
        assert!(report.links[0].text_mismatch);
        assert!(!report.links[1].text_mismatch);
        assert!(!report.links[2].text_mismatch);
    }

    #[test]
    fn untrusted_css_in_remote_mode_still_restricts_url_schemes() {
        let profile = UntrustedProfile { remote_content: RemoteContent::Allowed, ..UntrustedProfile::default() };
        let (document, _) = run_with(
            "<p style=\"background-image:url(javascript:alert(1))\">a</p>\
             <p id=b style=\"background-image:url(file:///etc/passwd)\">b</p>\
             <p id=c style=\"background-image:image-set('https://a.example/x.png' 1x)\">c</p>\
             <p id=d style=\"background-image:url('https://a.example/x.png')\">d</p>",
            profile,
        );
        let dumped = dump(&document);
        assert!(!dumped.contains("javascript"), "{dumped}");
        assert!(!dumped.contains("file:"), "{dumped}");
        assert!(!dumped.contains("image-set"), "{dumped}");
        assert!(attribute(&document, "#d", "style").unwrap().contains("https://a.example/x.png"));
    }
}
