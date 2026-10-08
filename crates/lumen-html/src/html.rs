//! HTML document ingestion into the shared DOM arena.
use crate::{Document, Error as DomError, Name, Namespace, NodeId, NodeKind};
use alloc::{
    borrow::Cow,
    string::{String, ToString},
    vec::Vec,
};

pub const MAX_HTML_BYTES: usize = 4 * 1024 * 1024;
const MAX_INSERTION_DEPTH: usize = 512;

fn normalized_input(input: &str) -> Cow<'_, str> {
    let bytes = input.as_bytes();
    if !bytes.contains(&b'\r') {
        return Cow::Borrowed(input);
    }
    let mut out = String::with_capacity(input.len());
    let mut run = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' => {
                out.push_str(&input[run..index]);
                out.push('\n');
                if bytes.get(index + 1) == Some(&b'\n') {
                    index += 1;
                }
                run = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    out.push_str(&input[run..]);
    Cow::Owned(out)
}

fn replace_nulls(input: &str) -> Cow<'_, str> {
    if !input.as_bytes().contains(&0) {
        return Cow::Borrowed(input);
    }
    Cow::Owned(replace_nulls_into(
        input,
        String::with_capacity(input.len()),
    ))
}

fn replace_nulls_into(input: &str, mut output: String) -> String {
    let mut start = 0;
    for (index, byte) in input.bytes().enumerate() {
        if byte == 0 {
            output.push_str(&input[start..index]);
            output.push('\u{fffd}');
            start = index + 1;
        }
    }
    output.push_str(&input[start..]);
    output
}

/// Apply HTML plaintext input preprocessing without parsing markup/entities.
/// Ordinary text keeps its owned buffer; null replacement is bounded before
/// allocating its exact output capacity.
pub fn normalize_plaintext(input: String) -> Result<String, ParseError> {
    if input.len() > MAX_HTML_BYTES {
        return Err(error(0, "HTML input too large"));
    }
    let input = if input.as_bytes().contains(&b'\r') {
        normalized_input(&input).into_owned()
    } else {
        input
    };
    let nulls = input.bytes().filter(|byte| *byte == 0).count();
    if nulls == 0 {
        return Ok(input);
    }
    let additional = lumen_common::limits::size::repeat(nulls, 2, MAX_HTML_BYTES)
        .map_err(|_| error(0, "HTML plaintext output too large"))?;
    let length = lumen_common::limits::size::sum(input.len(), additional, MAX_HTML_BYTES)
        .map_err(|_| error(0, "HTML plaintext output too large"))?;
    let output = lumen_common::limits::size::string_with_capacity(length, MAX_HTML_BYTES)
        .map_err(|_| error(0, "HTML plaintext output too large"))?;
    Ok(replace_nulls_into(&input, output))
}

fn remove_nulls(input: &str) -> Cow<'_, str> {
    if !input.as_bytes().contains(&0) {
        return Cow::Borrowed(input);
    }
    let mut output = String::with_capacity(input.len());
    let mut start = 0;
    for (index, byte) in input.bytes().enumerate() {
        if byte == 0 {
            output.push_str(&input[start..index]);
            start = index + 1;
        }
    }
    output.push_str(&input[start..]);
    Cow::Owned(output)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseError {
    pub offset: usize,
    pub message: &'static str,
}

/// Classification used by DOM bindings to choose the platform interface for
/// an element in the HTML namespace. Custom names remain HTMLElement-derived;
/// names absent from the HTML element set use HTMLUnknownElement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HtmlElementNameKind {
    BuiltIn,
    Custom,
    Unknown,
}

pub fn classify_html_element_name(name: &str) -> HtmlElementNameKind {
    // These legacy names are still handled by the parser, but their HTML
    // interface is HTMLUnknownElement rather than HTMLElement or a specialized
    // HTML element interface.
    if matches!(
        name,
        "applet" | "bgsound" | "blink" | "isindex" | "keygen" | "multicol" | "nextid" | "spacer"
    ) {
        HtmlElementNameKind::Unknown
    } else if is_valid_custom_element_name(name) {
        HtmlElementNameKind::Custom
    } else if is_known_html_element_name(name) {
        HtmlElementNameKind::BuiltIn
    } else {
        HtmlElementNameKind::Unknown
    }
}

/// Whether `name` is a valid custom element name under the HTML syntax rules.
pub fn is_valid_custom_element_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() || !name.contains('-') {
        return false;
    }
    if chars.any(|ch| ch.is_ascii_uppercase()) || !crate::xml::is_valid_element_local_name(name) {
        return false;
    }
    ![
        "annotation-xml",
        "color-profile",
        "font-face",
        "font-face-src",
        "font-face-uri",
        "font-face-format",
        "font-face-name",
        "missing-glyph",
    ]
    .contains(&name)
}

/// Names of standard and legacy HTML elements. Parser aliases such as `image`
/// are intentionally excluded because they are not element interface names.
pub fn is_known_html_element_name(name: &str) -> bool {
    matches!(
        name,
        "a" | "abbr"
            | "acronym"
            | "address"
            | "applet"
            | "area"
            | "article"
            | "aside"
            | "audio"
            | "b"
            | "base"
            | "basefont"
            | "bdi"
            | "bdo"
            | "bgsound"
            | "big"
            | "blockquote"
            | "body"
            | "br"
            | "button"
            | "canvas"
            | "caption"
            | "center"
            | "cite"
            | "code"
            | "col"
            | "colgroup"
            | "data"
            | "datalist"
            | "dd"
            | "del"
            | "details"
            | "dfn"
            | "dialog"
            | "dir"
            | "div"
            | "dl"
            | "dt"
            | "em"
            | "embed"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "font"
            | "footer"
            | "form"
            | "frame"
            | "frameset"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "head"
            | "header"
            | "hgroup"
            | "hr"
            | "html"
            | "i"
            | "iframe"
            | "img"
            | "input"
            | "ins"
            | "isindex"
            | "kbd"
            | "keygen"
            | "label"
            | "legend"
            | "li"
            | "link"
            | "listing"
            | "main"
            | "map"
            | "mark"
            | "marquee"
            | "menu"
            | "menuitem"
            | "meta"
            | "meter"
            | "nav"
            | "nobr"
            | "noembed"
            | "noframes"
            | "noscript"
            | "object"
            | "ol"
            | "optgroup"
            | "option"
            | "output"
            | "p"
            | "param"
            | "picture"
            | "plaintext"
            | "pre"
            | "progress"
            | "q"
            | "rb"
            | "rp"
            | "rt"
            | "rtc"
            | "ruby"
            | "s"
            | "samp"
            | "script"
            | "search"
            | "section"
            | "select"
            | "selectedcontent"
            | "slot"
            | "small"
            | "source"
            | "spacer"
            | "span"
            | "strike"
            | "strong"
            | "style"
            | "sub"
            | "summary"
            | "sup"
            | "table"
            | "tbody"
            | "td"
            | "template"
            | "textarea"
            | "tfoot"
            | "th"
            | "thead"
            | "time"
            | "title"
            | "tr"
            | "track"
            | "tt"
            | "u"
            | "ul"
            | "var"
            | "video"
            | "wbr"
            | "xmp"
    )
}

/// Token emitted by the bounded conformance tokenizer used by the corpus gate.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConformanceToken {
    Character(String),
    Comment(String),
    ProcessingInstruction {
        target: String,
        data: String,
    },
    Doctype {
        name: Option<String>,
        public_id: Option<String>,
        system_id: Option<String>,
        correct: bool,
    },
    StartTag {
        name: String,
        attributes: Vec<(String, String)>,
        self_closing: bool,
    },
    EndTag(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InitialTokenizerState {
    Plaintext,
    Rcdata,
    Rawtext,
    ScriptData,
    Cdata,
}

fn error(offset: usize, message: &'static str) -> ParseError {
    ParseError { offset, message }
}

fn dom_error(offset: usize, value: DomError) -> ParseError {
    error(
        offset,
        match value {
            DomError::LimitExceeded => "node limit exceeded",
            DomError::UnsupportedDoctype => "unsupported doctype",
            _ => "invalid document tree",
        },
    )
}

/// HTML tree construction's adjusted insertion guards are deliberately
/// distinct from DOM insertion, which normally moves an already-parented node.
pub fn insert_at_adjusted_location<D: crate::parser_documents::ParserDocument + ?Sized>(document: &mut D, parent: NodeId, element: NodeId, before: Option<NodeId>) -> Result<bool, DomError> {
    if document.parent(element)?.is_some()
        || document.is_host_including_inclusive_ancestor(element, parent)? {
        return Ok(false);
    }
    if matches!(document.kind(parent)?, NodeKind::Document) {
        let mut child = document.first_child(parent)?;
        while let Some(node) = child {
            if matches!(document.kind(node)?, NodeKind::Element { .. }) { return Ok(false); }
            child = document.next_sibling(node)?;
        }
    }
    document.insert_before(parent, element, before)?;
    Ok(true)
}

struct ParsedDoctype {
    name: Option<String>,
    public_id: Option<String>,
    system_id: Option<String>,
    force_quirks: bool,
}

impl ParsedDoctype {
    fn conformance_token(&self) -> ConformanceToken {
        ConformanceToken::Doctype {
            name: self.name.clone(),
            public_id: self.public_id.clone(),
            system_id: self.system_id.clone(),
            correct: !self.force_quirks,
        }
    }
}

fn parse_doctype(raw: &str, terminated: bool) -> ParsedDoctype {
    // Feed the actual terminating `>` through the state machine as well. The
    // tokenizer exits quoted public/system identifiers on `>` with an abrupt
    // doctype parse error, rather than treating it as identifier data.
    let terminated_raw = terminated.then(|| alloc::format!("{raw}>"));
    let raw = terminated_raw.as_deref().unwrap_or(raw);
    let bytes = raw.as_bytes();
    let mut position = 0;
    let mut name = String::new();
    let mut public_id = None;
    let mut system_id = None;
    let mut force_quirks = false;
    let mut state = DoctypeState::BeforeName;
    while position < bytes.len() {
        let byte = bytes[position];
        let mut character_bytes = 1;
        let reconsume = match state {
            DoctypeState::BeforeName => match byte {
                b'\t' | b'\n' | 0x0c | b' ' => false,
                b'\0' => {
                    name.push('\u{fffd}');
                    state = DoctypeState::Name;
                    false
                }
                b'>' => {
                    force_quirks = true;
                    break;
                }
                _ => {
                    let character = raw[position..].chars().next().unwrap();
                    character_bytes = character.len_utf8();
                    name.push(if character.is_ascii() {
                        character.to_ascii_lowercase()
                    } else {
                        character
                    });
                    state = DoctypeState::Name;
                    false
                }
            },
            DoctypeState::Name => match byte {
                b'\t' | b'\n' | 0x0c | b' ' => {
                    state = DoctypeState::AfterName;
                    false
                }
                b'>' => break,
                b'\0' => {
                    name.push('\u{fffd}');
                    false
                }
                _ => {
                    let character = raw[position..].chars().next().unwrap();
                    character_bytes = character.len_utf8();
                    name.push(if character.is_ascii() {
                        character.to_ascii_lowercase()
                    } else {
                        character
                    });
                    false
                }
            },
            DoctypeState::AfterName => match byte {
                b'\t' | b'\n' | 0x0c | b' ' => false,
                b'>' => break,
                _ if starts_ascii_case_insensitive(&bytes[position..], b"PUBLIC") => {
                    position += 6;
                    state = DoctypeState::BeforePublicId;
                    true
                }
                _ if starts_ascii_case_insensitive(&bytes[position..], b"SYSTEM") => {
                    position += 6;
                    state = DoctypeState::BeforeSystemId;
                    true
                }
                _ => {
                    force_quirks = true;
                    state = DoctypeState::Bogus;
                    true
                }
            },
            DoctypeState::BeforePublicId => match byte {
                b'\t' | b'\n' | 0x0c | b' ' => false,
                b'"' | b'\'' => {
                    public_id = Some(String::new());
                    state = DoctypeState::PublicId(byte);
                    false
                }
                b'>' => {
                    force_quirks = true;
                    break;
                }
                _ => {
                    force_quirks = true;
                    state = DoctypeState::Bogus;
                    true
                }
            },
            DoctypeState::PublicId(quote) => {
                if byte == quote {
                    state = DoctypeState::AfterPublicId;
                } else if byte == b'>' {
                    force_quirks = true;
                    break;
                } else {
                    let character = raw[position..].chars().next().unwrap();
                    character_bytes = character.len_utf8();
                    public_id.as_mut().unwrap().push(if character == '\0' {
                        '\u{fffd}'
                    } else {
                        character
                    });
                }
                false
            }
            DoctypeState::AfterPublicId => match byte {
                b'\t' | b'\n' | 0x0c | b' ' => {
                    state = DoctypeState::BetweenPublicAndSystemId;
                    false
                }
                b'>' => break,
                b'"' | b'\'' => {
                    system_id = Some(String::new());
                    state = DoctypeState::SystemId(byte);
                    true
                }
                _ => {
                    force_quirks = true;
                    state = DoctypeState::Bogus;
                    true
                }
            },
            DoctypeState::BetweenPublicAndSystemId => match byte {
                b'\t' | b'\n' | 0x0c | b' ' => false,
                b'>' => break,
                b'"' | b'\'' => {
                    system_id = Some(String::new());
                    state = DoctypeState::SystemId(byte);
                    false
                }
                _ => {
                    force_quirks = true;
                    state = DoctypeState::Bogus;
                    true
                }
            },
            DoctypeState::BeforeSystemId => match byte {
                b'\t' | b'\n' | 0x0c | b' ' => false,
                b'"' | b'\'' => {
                    system_id = Some(String::new());
                    state = DoctypeState::SystemId(byte);
                    false
                }
                b'>' => {
                    force_quirks = true;
                    break;
                }
                _ => {
                    force_quirks = true;
                    state = DoctypeState::Bogus;
                    true
                }
            },
            DoctypeState::SystemId(quote) => {
                if byte == quote {
                    state = DoctypeState::AfterSystemId;
                } else if byte == b'>' {
                    force_quirks = true;
                    break;
                } else {
                    let character = raw[position..].chars().next().unwrap();
                    character_bytes = character.len_utf8();
                    system_id.as_mut().unwrap().push(if character == '\0' {
                        '\u{fffd}'
                    } else {
                        character
                    });
                }
                false
            }
            DoctypeState::AfterSystemId => match byte {
                b'\t' | b'\n' | 0x0c | b' ' => false,
                b'>' => break,
                _ => {
                    state = DoctypeState::Bogus;
                    true
                }
            },
            DoctypeState::Bogus => {
                if byte == b'>' {
                    break;
                }
                false
            }
        };
        if reconsume {
            continue;
        }
        position += character_bytes;
    }
    if name.is_empty()
        || matches!(state, DoctypeState::BeforeName)
        || (!terminated && !matches!(state, DoctypeState::Bogus))
    {
        force_quirks = true;
    }
    ParsedDoctype {
        name: (!name.is_empty()).then_some(name),
        public_id,
        system_id,
        force_quirks,
    }
}

fn starts_ascii_case_insensitive_str(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}

fn document_mode_for_doctype(doctype: &ParsedDoctype) -> crate::DocumentMode {
    use crate::DocumentMode;

    if doctype.force_quirks || doctype.name.as_deref() != Some("html") {
        return DocumentMode::Quirks;
    }

    let public_id = doctype.public_id.as_deref().unwrap_or("");
    let system_id = doctype.system_id.as_deref().unwrap_or("");
    let public_id_exact_quirks = [
        "-//W3O//DTD W3 HTML Strict 3.0//EN//",
        "-/W3C/DTD HTML 4.0 Transitional/EN",
        "HTML",
    ];
    let public_id_quirks_prefixes = [
        "+//Silmaril//dtd html Pro v0r11 19970101//",
        "-//AS//DTD HTML 3.0 asWedit + extensions//",
        "-//AdvaSoft Ltd//DTD HTML 3.0 asWedit + extensions//",
        "-//IETF//DTD HTML 2.0 Level 1//",
        "-//IETF//DTD HTML 2.0 Level 2//",
        "-//IETF//DTD HTML 2.0 Strict Level 1//",
        "-//IETF//DTD HTML 2.0 Strict Level 2//",
        "-//IETF//DTD HTML 2.0 Strict//",
        "-//IETF//DTD HTML 2.0//",
        "-//IETF//DTD HTML 2.1E//",
        "-//IETF//DTD HTML 3.0//",
        "-//IETF//DTD HTML 3.2 Final//",
        "-//IETF//DTD HTML 3.2//",
        "-//IETF//DTD HTML 3//",
        "-//IETF//DTD HTML Level 0//",
        "-//IETF//DTD HTML Level 1//",
        "-//IETF//DTD HTML Level 2//",
        "-//IETF//DTD HTML Level 3//",
        "-//IETF//DTD HTML Strict Level 0//",
        "-//IETF//DTD HTML Strict Level 1//",
        "-//IETF//DTD HTML Strict Level 2//",
        "-//IETF//DTD HTML Strict Level 3//",
        "-//IETF//DTD HTML Strict//",
        "-//IETF//DTD HTML//",
        "-//Metrius//DTD Metrius Presentational//",
        "-//Microsoft//DTD Internet Explorer 2.0 HTML Strict//",
        "-//Microsoft//DTD Internet Explorer 2.0 HTML//",
        "-//Microsoft//DTD Internet Explorer 2.0 Tables//",
        "-//Microsoft//DTD Internet Explorer 3.0 HTML Strict//",
        "-//Microsoft//DTD Internet Explorer 3.0 HTML//",
        "-//Microsoft//DTD Internet Explorer 3.0 Tables//",
        "-//Netscape Comm. Corp.//DTD HTML//",
        "-//Netscape Comm. Corp.//DTD Strict HTML//",
        "-//O'Reilly and Associates//DTD HTML 2.0//",
        "-//O'Reilly and Associates//DTD HTML Extended 1.0//",
        "-//O'Reilly and Associates//DTD HTML Extended Relaxed 1.0//",
        "-//SQ//DTD HTML 2.0 HoTMetaL + extensions//",
        "-//SoftQuad Software//DTD HoTMetaL PRO 6.0::19990601::extensions to HTML 4.0//",
        "-//SoftQuad//DTD HoTMetaL PRO 4.0::19971010::extensions to HTML 4.0//",
        "-//Spyglass//DTD HTML 2.0 Extended//",
        "-//Sun Microsystems Corp.//DTD HotJava HTML//",
        "-//Sun Microsystems Corp.//DTD HotJava Strict HTML//",
        "-//W3C//DTD HTML 3 1995-03-24//",
        "-//W3C//DTD HTML 3.2 Draft//",
        "-//W3C//DTD HTML 3.2 Final//",
        "-//W3C//DTD HTML 3.2//",
        "-//W3C//DTD HTML 3.2S Draft//",
        "-//W3C//DTD HTML 4.0 Frameset//",
        "-//W3C//DTD HTML 4.0 Transitional//",
        "-//W3C//DTD HTML Experimental 19960712//",
        "-//W3C//DTD HTML Experimental 970421//",
        "-//W3C//DTD W3 HTML//",
        "-//W3O//DTD W3 HTML 3.0//",
        "-//WebTechs//DTD Mozilla HTML 2.0//",
        "-//WebTechs//DTD Mozilla HTML//",
    ];
    let quirks_public_id = public_id_exact_quirks
        .iter()
        .any(|known| public_id.eq_ignore_ascii_case(known))
        || public_id_quirks_prefixes
            .iter()
            .any(|prefix| starts_ascii_case_insensitive_str(public_id, prefix))
        || [
            "-//W3C//DTD HTML 4.01 Frameset//",
            "-//W3C//DTD HTML 4.01 Transitional//",
        ]
        .iter()
        .any(|prefix| {
            (doctype.system_id.is_none() || system_id.is_empty())
                && starts_ascii_case_insensitive_str(public_id, prefix)
        });
    if quirks_public_id
        || system_id
            .eq_ignore_ascii_case("http://www.ibm.com/data/dtd/v11/ibmxhtml1-transitional.dtd")
    {
        return DocumentMode::Quirks;
    }

    let limited_quirks_public_id = [
        "-//W3C//DTD XHTML 1.0 Frameset//",
        "-//W3C//DTD XHTML 1.0 Transitional//",
    ]
    .iter()
    .any(|prefix| starts_ascii_case_insensitive_str(public_id, prefix))
        || (doctype.system_id.is_some()
            && !system_id.is_empty()
            && [
                "-//W3C//DTD HTML 4.01 Frameset//",
                "-//W3C//DTD HTML 4.01 Transitional//",
            ]
            .iter()
            .any(|prefix| starts_ascii_case_insensitive_str(public_id, prefix)));
    if limited_quirks_public_id {
        DocumentMode::LimitedQuirks
    } else {
        DocumentMode::NoQuirks
    }
}

#[derive(Clone, Copy)]
enum DoctypeState {
    BeforeName,
    Name,
    AfterName,
    BeforePublicId,
    PublicId(u8),
    AfterPublicId,
    BetweenPublicAndSystemId,
    BeforeSystemId,
    SystemId(u8),
    AfterSystemId,
    Bogus,
}

fn starts_ascii_case_insensitive(input: &[u8], prefix: &[u8]) -> bool {
    input
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}

fn create_html_element<D: crate::parser_documents::ParserDocument + ?Sized>(document: &mut D, name: impl Into<Name>, attributes: Vec<(Name, String)>) -> Result<NodeId, DomError> {
    document.create_unprefixed_element(Namespace::Html, name.into(), attributes)
}

#[cfg(test)]
fn element(name: impl Into<Name>, attributes: Vec<(Name, String)>) -> NodeKind {
    NodeKind::Element {
        namespace: Namespace::Html,
        name: name.into(),
        attributes,
    }
}

fn svg_element_name(name: &str) -> &str {
    match name {
        "altglyph" => "altGlyph",
        "altglyphdef" => "altGlyphDef",
        "altglyphitem" => "altGlyphItem",
        "animatecolor" => "animateColor",
        "animatemotion" => "animateMotion",
        "animatetransform" => "animateTransform",
        "clippath" => "clipPath",
        "feblend" => "feBlend",
        "fecolormatrix" => "feColorMatrix",
        "fecomponenttransfer" => "feComponentTransfer",
        "fecomposite" => "feComposite",
        "feconvolvematrix" => "feConvolveMatrix",
        "fediffuselighting" => "feDiffuseLighting",
        "fedisplacementmap" => "feDisplacementMap",
        "fedistantlight" => "feDistantLight",
        "fedropshadow" => "feDropShadow",
        "feflood" => "feFlood",
        "fefunca" => "feFuncA",
        "fefuncb" => "feFuncB",
        "fefuncg" => "feFuncG",
        "fefuncr" => "feFuncR",
        "fegaussianblur" => "feGaussianBlur",
        "feimage" => "feImage",
        "femerge" => "feMerge",
        "femergenode" => "feMergeNode",
        "femorphology" => "feMorphology",
        "feoffset" => "feOffset",
        "fepointlight" => "fePointLight",
        "fespecularlighting" => "feSpecularLighting",
        "fespotlight" => "feSpotLight",
        "fetile" => "feTile",
        "feturbulence" => "feTurbulence",
        "foreignobject" => "foreignObject",
        "glyphref" => "glyphRef",
        "lineargradient" => "linearGradient",
        "radialgradient" => "radialGradient",
        "textpath" => "textPath",
        _ => name,
    }
}

fn svg_attribute_name(name: &str) -> &str {
    match name {
        "attributename" => "attributeName",
        "attributetype" => "attributeType",
        "basefrequency" => "baseFrequency",
        "baseprofile" => "baseProfile",
        "calcmode" => "calcMode",
        "clippathunits" => "clipPathUnits",
        "diffuseconstant" => "diffuseConstant",
        "edgemode" => "edgeMode",
        "filterunits" => "filterUnits",
        "glyphref" => "glyphRef",
        "gradienttransform" => "gradientTransform",
        "gradientunits" => "gradientUnits",
        "kernelmatrix" => "kernelMatrix",
        "kernelunitlength" => "kernelUnitLength",
        "keypoints" => "keyPoints",
        "keysplines" => "keySplines",
        "keytimes" => "keyTimes",
        "lengthadjust" => "lengthAdjust",
        "limitingconeangle" => "limitingConeAngle",
        "markerheight" => "markerHeight",
        "markerunits" => "markerUnits",
        "markerwidth" => "markerWidth",
        "maskcontentunits" => "maskContentUnits",
        "maskunits" => "maskUnits",
        "numoctaves" => "numOctaves",
        "pathlength" => "pathLength",
        "patterncontentunits" => "patternContentUnits",
        "patterntransform" => "patternTransform",
        "patternunits" => "patternUnits",
        "pointsatx" => "pointsAtX",
        "pointsaty" => "pointsAtY",
        "pointsatz" => "pointsAtZ",
        "preservealpha" => "preserveAlpha",
        "preserveaspectratio" => "preserveAspectRatio",
        "primitiveunits" => "primitiveUnits",
        "refx" => "refX",
        "refy" => "refY",
        "repeatcount" => "repeatCount",
        "repeatdur" => "repeatDur",
        "requiredextensions" => "requiredExtensions",
        "requiredfeatures" => "requiredFeatures",
        "specularconstant" => "specularConstant",
        "specularexponent" => "specularExponent",
        "spreadmethod" => "spreadMethod",
        "startoffset" => "startOffset",
        "stddeviation" => "stdDeviation",
        "stitchtiles" => "stitchTiles",
        "surfacescale" => "surfaceScale",
        "systemlanguage" => "systemLanguage",
        "tablevalues" => "tableValues",
        "targetx" => "targetX",
        "targety" => "targetY",
        "textlength" => "textLength",
        "viewbox" => "viewBox",
        "viewtarget" => "viewTarget",
        "xchannelselector" => "xChannelSelector",
        "ychannelselector" => "yChannelSelector",
        "zoomandpan" => "zoomAndPan",
        "definitionurl" => "definitionURL",
        _ => name,
    }
}

fn foreign_attribute_namespace(name: &str) -> Option<&'static str> {
    match name {
        "xlink:actuate" | "xlink:arcrole" | "xlink:href" | "xlink:role" | "xlink:show"
        | "xlink:title" | "xlink:type" => Some("http://www.w3.org/1999/xlink"),
        "xml:base" | "xml:lang" | "xml:space" => Some("http://www.w3.org/XML/1998/namespace"),
        "xmlns" | "xmlns:xlink" => Some("http://www.w3.org/2000/xmlns/"),
        _ => None,
    }
}

/// Element names the tree builder distinguishes; everything else is `Other`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum Tag {
    Other,
    A,
    Applet,
    Address,
    Area,
    Article,
    Aside,
    B,
    Base,
    Basefont,
    Bgsound,
    Big,
    Blockquote,
    Body,
    Br,
    Button,
    Caption,
    Center,
    Col,
    Colgroup,
    Code,
    Dd,
    Details,
    Dialog,
    Dir,
    Div,
    Dl,
    Dt,
    Em,
    Embed,
    Fieldset,
    Font,
    Figcaption,
    Figure,
    Footer,
    Form,
    Frame,
    Frameset,
    H1,
    H2,
    H3,
    H4,
    H5,
    H6,
    Head,
    Header,
    Hgroup,
    Hr,
    Html,
    I,
    Iframe,
    Img,
    Isindex,
    Input,
    Keygen,
    Listing,
    Li,
    Link,
    Main,
    Menu,
    Marquee,
    Menuitem,
    Meta,
    Nav,
    Nobr,
    Noscript,
    Noembed,
    Noframes,
    Ol,
    Object,
    Optgroup,
    Option,
    P,
    Param,
    Pre,
    Plaintext,
    Rb,
    Rp,
    Rt,
    Rtc,
    Ruby,
    S,
    Script,
    Search,
    SelectedContent,
    Section,
    Select,
    Small,
    Source,
    Strong,
    Strike,
    Style,
    Summary,
    Table,
    Tbody,
    Td,
    Template,
    Textarea,
    Tfoot,
    Th,
    Thead,
    Title,
    Tt,
    Tr,
    Track,
    U,
    Ul,
    Wbr,
    Xmp,
}

impl Tag {
    fn classify(name: &str) -> Tag {
        match name {
            "a" => Tag::A,
            "applet" => Tag::Applet,
            "address" => Tag::Address,
            "area" => Tag::Area,
            "article" => Tag::Article,
            "aside" => Tag::Aside,
            "b" => Tag::B,
            "base" => Tag::Base,
            "basefont" => Tag::Basefont,
            "bgsound" => Tag::Bgsound,
            "big" => Tag::Big,
            "blockquote" => Tag::Blockquote,
            "body" => Tag::Body,
            "br" => Tag::Br,
            "button" => Tag::Button,
            "caption" => Tag::Caption,
            "center" => Tag::Center,
            "col" => Tag::Col,
            "colgroup" => Tag::Colgroup,
            "code" => Tag::Code,
            "dd" => Tag::Dd,
            "details" => Tag::Details,
            "dialog" => Tag::Dialog,
            "dir" => Tag::Dir,
            "div" => Tag::Div,
            "dl" => Tag::Dl,
            "dt" => Tag::Dt,
            "em" => Tag::Em,
            "embed" => Tag::Embed,
            "fieldset" => Tag::Fieldset,
            "font" => Tag::Font,
            "figcaption" => Tag::Figcaption,
            "figure" => Tag::Figure,
            "footer" => Tag::Footer,
            "form" => Tag::Form,
            "frame" => Tag::Frame,
            "frameset" => Tag::Frameset,
            "h1" => Tag::H1,
            "h2" => Tag::H2,
            "h3" => Tag::H3,
            "h4" => Tag::H4,
            "h5" => Tag::H5,
            "h6" => Tag::H6,
            "head" => Tag::Head,
            "header" => Tag::Header,
            "hgroup" => Tag::Hgroup,
            "hr" => Tag::Hr,
            "html" => Tag::Html,
            "i" => Tag::I,
            "iframe" => Tag::Iframe,
            "image" | "img" => Tag::Img,
            "isindex" => Tag::Isindex,
            "input" => Tag::Input,
            "keygen" => Tag::Keygen,
            "listing" => Tag::Listing,
            "li" => Tag::Li,
            "link" => Tag::Link,
            "main" => Tag::Main,
            "menu" => Tag::Menu,
            "marquee" => Tag::Marquee,
            "menuitem" => Tag::Menuitem,
            "meta" => Tag::Meta,
            "nav" => Tag::Nav,
            "nobr" => Tag::Nobr,
            "noscript" => Tag::Noscript,
            "noembed" => Tag::Noembed,
            "noframes" => Tag::Noframes,
            "ol" => Tag::Ol,
            "object" => Tag::Object,
            "optgroup" => Tag::Optgroup,
            "option" => Tag::Option,
            "p" => Tag::P,
            "param" => Tag::Param,
            "pre" => Tag::Pre,
            "plaintext" => Tag::Plaintext,
            "rb" => Tag::Rb,
            "rp" => Tag::Rp,
            "rt" => Tag::Rt,
            "rtc" => Tag::Rtc,
            "ruby" => Tag::Ruby,
            "s" => Tag::S,
            "script" => Tag::Script,
            "search" => Tag::Search,
            "selectedcontent" => Tag::SelectedContent,
            "section" => Tag::Section,
            "select" => Tag::Select,
            "small" => Tag::Small,
            "source" => Tag::Source,
            "strong" => Tag::Strong,
            "strike" => Tag::Strike,
            "style" => Tag::Style,
            "summary" => Tag::Summary,
            "table" => Tag::Table,
            "tbody" => Tag::Tbody,
            "td" => Tag::Td,
            "template" => Tag::Template,
            "textarea" => Tag::Textarea,
            "tfoot" => Tag::Tfoot,
            "th" => Tag::Th,
            "thead" => Tag::Thead,
            "title" => Tag::Title,
            "tt" => Tag::Tt,
            "tr" => Tag::Tr,
            "track" => Tag::Track,
            "u" => Tag::U,
            "ul" => Tag::Ul,
            "wbr" => Tag::Wbr,
            "xmp" => Tag::Xmp,
            _ => Tag::Other,
        }
    }

    fn is_void(self) -> bool {
        matches!(
            self,
            Tag::Area
                | Tag::Base
                | Tag::Basefont
                | Tag::Bgsound
                | Tag::Br
                | Tag::Col
                | Tag::Embed
                | Tag::Hr
                | Tag::Img
                | Tag::Input
                | Tag::Frame
                | Tag::Keygen
                | Tag::Menuitem
                | Tag::Link
                | Tag::Meta
                | Tag::Param
                | Tag::Source
                | Tag::Track
                | Tag::Wbr
        )
    }

    fn is_formatting(self) -> bool {
        matches!(
            self,
            Tag::A
                | Tag::B
                | Tag::Big
                | Tag::Code
                | Tag::Em
                | Tag::Font
                | Tag::I
                | Tag::Nobr
                | Tag::S
                | Tag::Small
                | Tag::Strong
                | Tag::Strike
                | Tag::Tt
                | Tag::U
        )
    }

    fn is_special(self) -> bool {
        matches!(
            self,
            Tag::Applet
                | Tag::Address
                | Tag::Article
                | Tag::Aside
                | Tag::Blockquote
                | Tag::Body
                | Tag::Br
                | Tag::Button
                | Tag::Center
                | Tag::Caption
                | Tag::Col
                | Tag::Colgroup
                | Tag::Dd
                | Tag::Details
                | Tag::Dialog
                | Tag::Dir
                | Tag::Div
                | Tag::Dl
                | Tag::Dt
                | Tag::Fieldset
                | Tag::Frame
                | Tag::Frameset
                | Tag::Figcaption
                | Tag::Figure
                | Tag::Footer
                | Tag::Form
                | Tag::H1
                | Tag::H2
                | Tag::H3
                | Tag::H4
                | Tag::H5
                | Tag::H6
                | Tag::Head
                | Tag::Header
                | Tag::Hgroup
                | Tag::Hr
                | Tag::Html
                | Tag::Iframe
                | Tag::Img
                | Tag::Input
                | Tag::Isindex
                | Tag::Keygen
                | Tag::Listing
                | Tag::Li
                | Tag::Link
                | Tag::Main
                | Tag::Marquee
                | Tag::Menu
                | Tag::Meta
                | Tag::Nav
                | Tag::Noscript
                | Tag::Noembed
                | Tag::Noframes
                | Tag::Ol
                | Tag::Object
                | Tag::P
                | Tag::Pre
                | Tag::Plaintext
                | Tag::Script
                | Tag::Search
                | Tag::Section
                | Tag::Select
                | Tag::Source
                | Tag::Style
                | Tag::Summary
                | Tag::Table
                | Tag::Tbody
                | Tag::Td
                | Tag::Template
                | Tag::Textarea
                | Tag::Tfoot
                | Tag::Th
                | Tag::Thead
                | Tag::Title
                | Tag::Tr
                | Tag::Ul
                | Tag::Wbr
                | Tag::Xmp
        )
    }

    /// Cell and caption boundaries that scope the active formatting list.
    fn is_marker(self) -> bool {
        matches!(
            self,
            Tag::Applet
                | Tag::Marquee
                | Tag::Td
                | Tag::Th
                | Tag::Template
                | Tag::Caption
                | Tag::Object
        )
    }

    fn is_table_context(self) -> bool {
        matches!(
            self,
            Tag::Table | Tag::Tbody | Tag::Thead | Tag::Tfoot | Tag::Tr
        )
    }

    fn is_table_part(self) -> bool {
        matches!(
            self,
            Tag::Caption
                | Tag::Colgroup
                | Tag::Col
                | Tag::Tbody
                | Tag::Thead
                | Tag::Tfoot
                | Tag::Tr
                | Tag::Td
                | Tag::Th
        )
    }

    fn is_raw_text(self) -> bool {
        matches!(
            self,
            Tag::Style
                | Tag::Script
                | Tag::Title
                | Tag::Textarea
                | Tag::Iframe
                | Tag::Noembed
                | Tag::Noframes
                | Tag::Xmp
        )
    }

    fn is_rcdata(self) -> bool {
        matches!(self, Tag::Title | Tag::Textarea)
    }

    fn closes_paragraph(self) -> bool {
        matches!(
            self,
            Tag::Address
                | Tag::Article
                | Tag::Aside
                | Tag::Blockquote
                | Tag::Details
                | Tag::Dialog
                | Tag::Center
                | Tag::Dir
                | Tag::Div
                | Tag::Dd
                | Tag::Dl
                | Tag::Fieldset
                | Tag::Figcaption
                | Tag::Figure
                | Tag::Footer
                | Tag::Form
                | Tag::H1
                | Tag::H2
                | Tag::H3
                | Tag::H4
                | Tag::H5
                | Tag::H6
                | Tag::Header
                | Tag::Hgroup
                | Tag::Hr
                | Tag::Listing
                | Tag::Dt
                | Tag::Li
                | Tag::Main
                | Tag::Menu
                | Tag::Nav
                | Tag::Ol
                | Tag::P
                | Tag::Pre
                | Tag::Section
                | Tag::Search
                | Tag::Summary
                | Tag::Table
                | Tag::Ul
        )
    }

    fn raw_text_name(self) -> &'static str {
        match self {
            Tag::Style => "style",
            Tag::Script => "script",
            Tag::Noscript => "noscript",
            Tag::Iframe => "iframe",
            Tag::Noembed => "noembed",
            Tag::Noframes => "noframes",
            Tag::Xmp => "xmp",
            Tag::Title => "title",
            _ => "textarea",
        }
    }
}

fn numeric_entity(input: &str) -> Option<(char, usize)> {
    let reference = lumen_common::entities::char_ref(input)?;
    Some((lumen_common::entities::html_char(reference.value), reference.len))
}

fn decode_entities(input: &str, attribute: bool) -> Cow<'_, str> {
    let Some(first) = input.find('&') else {
        return Cow::Borrowed(input);
    };
    let mut output = String::with_capacity(input.len());
    output.push_str(&input[..first]);
    let mut remaining = &input[first..];
    while let Some(start) = remaining.find('&') {
        output.push_str(&remaining[..start]);
        let after_amp = &remaining[start + 1..];
        if let Some((value, consumed)) = numeric_entity(after_amp) {
            output.push(value);
            remaining = &after_amp[consumed..];
            continue;
        }
        if let Some((end, value)) = lumen_common::entities::longest_named(after_amp.as_bytes()) {
            let bytes = after_amp.as_bytes();
            if !(attribute
                && bytes[end - 1] != b';'
                && bytes
                    .get(end)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'='))
            {
                output.push_str(value);
                remaining = &after_amp[end..];
                continue;
            }
        }
        output.push('&');
        remaining = after_amp;
    }
    output.push_str(remaining);
    Cow::Owned(output)
}

fn leading_lf_character_token(input: &str) -> Option<usize> {
    if input.starts_with('\n') {
        return Some(1);
    }
    let reference = input.strip_prefix('&')?;
    if let Some((value, consumed)) = numeric_entity(reference) {
        return (value == '\n').then_some(consumed + 1);
    }
    let (consumed, value) = lumen_common::entities::longest_named(reference.as_bytes())?;
    value.starts_with('\n').then_some(consumed + 1)
}

fn escape(output: &mut String, input: &str, attribute: bool) {
    let mut run = 0;
    for (index, character) in input.char_indices() {
        let replacement = match character {
            '&' => "&amp;",
            '\u{00a0}' => "&nbsp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '"' if attribute => "&quot;",
            _ => continue,
        };
        output.push_str(&input[run..index]);
        output.push_str(replacement);
        run = index + character.len_utf8();
    }
    output.push_str(&input[run..]);
}

fn append_doctype_identifier(output: &mut String, identifier: &str) {
    let quote = if identifier.contains('"') && !identifier.contains('\'') {
        '\''
    } else {
        '"'
    };
    output.push(quote);
    output.push_str(identifier);
    output.push(quote);
}

pub(crate) fn serializes_void(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "basefont"
            | "bgsound"
            | "br"
            | "col"
            | "embed"
            | "frame"
            | "hr"
            | "img"
            | "input"
            | "keygen"
            | "link"
            | "meta"
            | "menuitem"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn serializes_text_literally(name: &str) -> bool {
    matches!(
        name,
        "style" | "script" | "xmp" | "iframe" | "noembed" | "noframes" | "plaintext"
    )
}

fn serialization_name<'a>(document: &'a Document, id: NodeId, namespace: &Namespace, name: &'a str) -> Result<&'a str, DomError> {
    if matches!(namespace, Namespace::Html | Namespace::Svg | Namespace::MathMl) && name.contains(':') {
        Ok(document.element_name_parts(id)?.1)
    } else {
        Ok(name)
    }
}

fn serialization_raw_text(document: &Document, name: &str) -> bool {
    serializes_text_literally(name) || (name == "noscript" && document.scripting_enabled())
}

fn serialization_uses_template_contents(
    document: &Document,
    id: NodeId,
    name: &Name,
) -> Result<bool, DomError> {
    let qualified_name = name.as_str();
    if qualified_name != "template" && !qualified_name.ends_with(":template") {
        return Ok(false);
    }
    document.is_html_template(id)
}

enum SerializationStep {
    Node(NodeId, bool, bool),
    Children(NodeId, bool),
    ShadowStart(NodeId, crate::ShadowOptions),
    ShadowEnd,
}

enum SelectedShadowRoots<'a> {
    None,
    Borrowed(&'a [NodeId]),
    SortedKeys(&'a [u128]),
}

impl SelectedShadowRoots<'_> {
    fn is_empty(&self) -> bool {
        match self {
            Self::None => true,
            Self::Borrowed(roots) => roots.is_empty(),
            Self::SortedKeys(keys) => keys.is_empty(),
        }
    }

    fn contains(&self, root: NodeId) -> bool {
        match self {
            Self::None => false,
            Self::Borrowed(roots) => roots.contains(&root),
            Self::SortedKeys(keys) => keys.binary_search(&root.key()).is_ok(),
        }
    }
}

impl Default for SelectedShadowRoots<'_> {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Default)]
struct ShadowSerialization<'a> {
    serializable: bool,
    selected: SelectedShadowRoots<'a>,
}

fn push_serialization_children<G: crate::graph::DocumentGraph>(
    graph: &G,
    id: NodeId,
    template_contents: bool,
    raw: bool,
    shadows: &ShadowSerialization<'_>,
    pending: &mut Vec<SerializationStep>,
) -> Result<(), DomError> {
    let owner = graph.read(id)?;
    let document = &*owner;
    let children_root = if template_contents {
        document.template_content(id)?.unwrap_or(id)
    } else {
        id
    };
    if let Some(child) = graph.read(children_root)?.first_child(children_root)? {
        pending.push(SerializationStep::Children(child, raw));
    }
    // The stack visits the synthetic template before the host's light children.
    if shadows.serializable || !shadows.selected.is_empty() {
        if let Some((root, options)) = document.shadow_root_with_options_for_valid_node(id) {
            if (shadows.serializable && options.serializable) || shadows.selected.contains(root)
            {
                pending.push(SerializationStep::ShadowStart(root, options));
            }
        }
    }
    Ok(())
}

fn serialize_into<G: crate::graph::DocumentGraph>(
    graph: &G,
    root: NodeId,
    children_only: bool,
    output: &mut String,
    shadows: &ShadowSerialization<'_>,
) -> Result<(), DomError> {
    // Most Element/ShadowRoot.getHTML calls serialize only a few levels, and
    // empty roots serialize nothing. Let the iterative depth stack grow only
    // when needed instead of allocating 32 events for every call.
    let mut pending = Vec::new();
    let root_document = graph.read(root)?;
    let document = &*root_document;
    if children_only {
        let root_kind = document.kind(root)?;
        let template_contents = match root_kind {
            NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            } => {
                if serializes_void(serialization_name(document, root, &Namespace::Html, name)?) {
                    return Ok(());
                }
                serialization_uses_template_contents(document, root, name)?
            }
            _ => false,
        };
        let context = if matches!(root_kind, NodeKind::DocumentFragment) {
            document.shadow_host(root)?.unwrap_or(root)
        } else {
            root
        };
        let raw = match document.kind(context)? {
            NodeKind::Element { namespace: Namespace::Html, name, .. } =>
                serialization_raw_text(document, serialization_name(document, context, &Namespace::Html, name)?),
            _ => false,
        };
        push_serialization_children(
            graph,
            root,
            template_contents,
            raw,
            shadows,
            &mut pending,
        )?;
    } else {
        pending.push(SerializationStep::Node(root, false, false));
    }
    if !pending.is_empty() {
        output.reserve(256);
    }
    while let Some(step) = pending.pop() {
        let step_node = match &step {
            SerializationStep::Node(id, ..) | SerializationStep::Children(id, ..) | SerializationStep::ShadowStart(id, ..) => Some(*id),
            SerializationStep::ShadowEnd => None,
        };
        let step_document = step_node.map(|node| graph.read(node)).transpose()?;
        let document = step_document.as_deref().unwrap_or(&*root_document);
        let (id, closing, raw_text) = match step {
            SerializationStep::Node(id, closing, raw) => (id, closing, raw),
            SerializationStep::Children(id, raw) => {
                if let Some(next) = document.next_sibling(id)? {
                    pending.push(SerializationStep::Children(next, raw));
                }
                (id, false, raw)
            }
            SerializationStep::ShadowEnd => {
                output.push_str("</template>");
                continue;
            }
            SerializationStep::ShadowStart(root, options) => {
                output.push_str("<template shadowrootmode=\"");
                output.push_str(match options.mode { crate::ShadowMode::Open => "open", crate::ShadowMode::Closed => "closed" });
                output.push('"');
                for (enabled, attribute) in [
                    (options.delegates_focus, " shadowrootdelegatesfocus=\"\""),
                    (options.serializable, " shadowrootserializable=\"\""),
                    (
                        options.slot_assignment == crate::shadow::SlotAssignmentMode::Manual,
                        " shadowrootslotassignment=\"manual\"",
                    ),
                    (options.clonable, " shadowrootclonable=\"\""),
                ] {
                    if enabled { output.push_str(attribute); }
                }
                output.push('>');
                pending.push(SerializationStep::ShadowEnd);
                push_serialization_children(graph, root, false, false, shadows, &mut pending)?;
                continue;
            }
        };
        let kind = document.kind(id)?;
        if closing {
            if let NodeKind::Element { namespace, name, .. } = kind {
                output.push_str("</");
                output.push_str(serialization_name(document, id, namespace, name)?);
                output.push('>');
            }
            continue;
        }
        let mut descend = true;
        let mut child_raw = raw_text;
        let mut template_contents = false;
        match kind {
            NodeKind::Document | NodeKind::DocumentFragment => {}
            // Attributes are serialized with their owning element, never as
            // independent child nodes in an HTML fragment.
            NodeKind::Attribute { .. } => descend = false,
            NodeKind::DocumentType(name) => {
                output.push_str("<!DOCTYPE ");
                output.push_str(name);
                let public_id = document.doctype_public_id(id)?;
                let system_id = document.doctype_system_id(id)?;
                if !public_id.is_empty() {
                    output.push_str(" PUBLIC ");
                    append_doctype_identifier(output, public_id);
                    if !system_id.is_empty() {
                        output.push(' ');
                        append_doctype_identifier(output, system_id);
                    }
                } else if !system_id.is_empty() {
                    output.push_str(" SYSTEM ");
                    append_doctype_identifier(output, system_id);
                }
                output.push('>');
                descend = false;
            }
            NodeKind::Element {
                namespace, name, attributes, ..
            } => {
                template_contents = namespace == &Namespace::Html
                    && serialization_uses_template_contents(document, id, name)?;
                let name = serialization_name(document, id, namespace, name)?;
                output.push('<');
                output.push_str(name);
                let namespaces = document.attribute_namespace_metadata(id);
                if !attributes.iter().enumerate().any(|(index, (key, _))| key == "is" &&
                    !namespaces.iter().any(|(known, _)| *known == index)) {
                    if let Some(value) = document.custom_element_is_value(id)? {
                        output.push_str(" is=\"");
                        escape(output, value, true);
                        output.push('"');
                    }
                }
                for (index, (key, value)) in attributes.iter().enumerate() {
                    output.push(' ');
                    let uri = namespaces.iter().find(|(known, _)| *known == index).map(|(_, uri)| uri.as_ref());
                    let local = key.split_once(':').map_or(key.as_str(), |(_, local)| local);
                    match uri {
                        Some("http://www.w3.org/XML/1998/namespace") => {
                            output.push_str("xml:");
                            output.push_str(local);
                        }
                        Some("http://www.w3.org/2000/xmlns/") => {
                            if local != "xmlns" { output.push_str("xmlns:"); }
                            output.push_str(local);
                        }
                        Some("http://www.w3.org/1999/xlink") => {
                            output.push_str("xlink:");
                            output.push_str(local);
                        }
                        _ => output.push_str(key),
                    }
                    output.push_str("=\"");
                    escape(output, value, true);
                    output.push('"');
                }
                output.push('>');
                descend = namespace != &Namespace::Html || !serializes_void(name);
                child_raw = namespace == &Namespace::Html && serialization_raw_text(document, name);
                if descend {
                    pending.push(SerializationStep::Node(id, true, raw_text));
                }
            }
            NodeKind::Text(value) | NodeKind::CData(value) => {
                if raw_text {
                    output.push_str(value);
                } else {
                    escape(output, value, false);
                }
                descend = false;
            }
            NodeKind::Comment(value) => {
                output.push_str("<!--");
                output.push_str(value);
                output.push_str("-->");
                descend = false;
            }
            NodeKind::ProcessingInstruction { target, data } => {
                output.push_str("<?");
                output.push_str(target);
                output.push(' ');
                output.push_str(data);
                output.push_str("?>");
                descend = false;
            }
        }
        if descend {
            push_serialization_children(
                graph,
                id,
                template_contents,
                child_raw,
                shadows,
                &mut pending,
            )?;
        }
    }
    Ok(())
}

pub fn outer_html(document: &Document, id: NodeId) -> Result<String, DomError> { outer_html_with_graph(document, id) }

pub fn outer_html_with_graph<G: crate::graph::DocumentGraph>(graph: &G, id: NodeId) -> Result<String, DomError> {
    let mut output = String::new();
    serialize_into(graph, id, false, &mut output, &ShadowSerialization::default())?;
    Ok(output)
}

pub fn inner_html(document: &Document, id: NodeId) -> Result<String, DomError> { inner_html_with_graph(document, id) }

pub fn inner_html_with_graph<G: crate::graph::DocumentGraph>(graph: &G, id: NodeId) -> Result<String, DomError> {
    let mut output = String::new();
    serialize_into(graph, id, true, &mut output, &ShadowSerialization::default())?;
    Ok(output)
}

/// HTML fragment serialization with explicitly selected or serializable shadow trees.
pub fn get_html(document: &Document, id: NodeId, serializable: bool, selected: &[NodeId]) -> Result<String, DomError> { get_html_with_graph(document, id, serializable, selected) }

pub fn get_html_with_graph<G: crate::graph::DocumentGraph>(graph: &G, id: NodeId, serializable: bool, selected: &[NodeId]) -> Result<String, DomError> {
    // The common API use is one or two explicitly selected roots. Check those
    // directly; sorting a temporary key vector on every getHTML call is
    // needless allocation. Keep logarithmic membership for unusually large
    // lists so serialization stays bounded under hostile options.
    let selected_keys = if selected.len() > 8 {
        let mut keys = selected.iter().map(|root| root.key()).collect::<Vec<_>>();
        keys.sort_unstable();
        keys.dedup();
        Some(keys)
    } else {
        None
    };
    let selected_roots = match selected_keys.as_deref() {
        Some(keys) => SelectedShadowRoots::SortedKeys(keys),
        None if selected.is_empty() => SelectedShadowRoots::None,
        None => SelectedShadowRoots::Borrowed(selected),
    };
    let shadows = ShadowSerialization { serializable, selected: selected_roots };
    let mut output = String::new();
    serialize_into(graph, id, true, &mut output, &shadows)?;
    Ok(output)
}

/// Parse an application document. Scripts are retained as inert text.
pub fn parse(input: &str, max_nodes: usize) -> Result<Document, ParseError> {
    parse_with_options(input, max_nodes, ParseOptions::default())
}

/// Parse a document with the embedding owner's declarative-shadow permission.
/// Ordinary fragment/DOMParser entry points keep this permission disabled.
pub fn parse_with_declarative_shadow_roots(
    input: &str,
    max_nodes: usize,
    allow_declarative_shadow_roots: bool,
) -> Result<Document, ParseError> {
    parse_with_options(
        input,
        max_nodes,
        ParseOptions {
            allow_declarative_shadow_roots,
            ..ParseOptions::default()
        },
    )
}

/// Options for parsing an active HTML document.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ParseOptions {
    /// Whether the embedding host permits declarative shadow roots.
    pub allow_declarative_shadow_roots: bool,
    /// Whether `noscript` uses scripting-enabled parsing and rendering behavior.
    pub scripting_enabled: bool,
}

/// Parse an HTML document with explicit embedding options.
///
/// The default [`parse`] entry point leaves scripting disabled because it
/// constructs a document without an active script host.
pub fn parse_with_options(
    input: &str,
    max_nodes: usize,
    options: ParseOptions,
) -> Result<Document, ParseError> {
    parse_with_options_initialized(input, max_nodes, options, |_| {})
}

/// Parse with document services installed before the parser creates any nodes.
pub fn parse_with_options_initialized(
    input: &str,
    max_nodes: usize,
    options: ParseOptions,
    initialize: impl FnOnce(&mut Document),
) -> Result<Document, ParseError> {
    let mut document = Document::new(max_nodes);
    initialize(&mut document);
    let mut parser = HtmlDocumentParser::open(&mut document, options)?.0;
    parser.write(&mut document, input)?;
    parser.close(&mut document)?;
    Ok(document)
}

/// Incremental HTML document parser used by `Document.open/write/close` and
/// the one-shot document parser. It owns tree-builder state and the bounded
/// source buffer while borrowing the live document only for each feed.
pub struct HtmlDocumentParser {
    input: String,
    source_base: usize,
    total_input_bytes: usize,
    state: Option<ParserState>,
    pending_cr: bool,
    pending_insertion_cr: bool,
    pending_insertion_cr_pos: Option<usize>,
    paused_script: Option<NodeId>,
    insertion_cursor: Option<usize>,
    insertion_frames: Vec<InsertionFrame>,
    finish_requested: bool,
    closed: bool,
}

#[derive(Clone, Copy)]
struct InsertionFrame {
    writer: NodeId,
    end: usize,
}

impl HtmlDocumentParser {
    /// Start a new document parse in the existing arena, detaching old
    /// document children but preserving their identities for retained Nodes.
    pub fn open(
        document: &mut Document,
        options: ParseOptions,
    ) -> Result<(Self, Vec<NodeId>), ParseError> {
        let root = document.root();
        let mut removed = Vec::new();
        while let Some(child) = document
            .first_child(root)
            .map_err(|error| dom_error(0, error))?
        {
            removed
                .try_reserve(1)
                .map_err(|_| error(0, "HTML parser allocation limit exceeded"))?;
            document
                .remove(child)
                .map_err(|error| dom_error(0, error))?;
            removed.push(child);
        }
        document.set_html_document(true);
        document.set_scripting_enabled(options.scripting_enabled);
        document.set_allow_declarative_shadow_roots(options.allow_declarative_shadow_roots);
        // A new HTML input stream starts in quirks mode until its doctype is
        // processed by the ordinary document tree builder.
        document.set_document_mode(crate::DocumentMode::Quirks);

        let html = create_html_element(document, "html", Vec::new())
            .map_err(|error| dom_error(0, error))?;
        let head = create_html_element(document, "head", Vec::new())
            .map_err(|error| dom_error(0, error))?;
        let body = create_html_element(document, "body", Vec::new())
            .map_err(|error| dom_error(0, error))?;
        document.set_node_document(html, root).map_err(|failure| dom_error(0, failure))?;
        document.set_node_document(head, root).map_err(|failure| dom_error(0, failure))?;
        document.set_node_document(body, root).map_err(|failure| dom_error(0, failure))?;
        document.record_parser_element_birth(html, root, true).map_err(|failure| dom_error(0, failure))?;
        document.record_parser_element_birth(head, html, true).map_err(|failure| dom_error(0, failure))?;
        document.record_parser_element_birth(body, html, true).map_err(|failure| dom_error(0, failure))?;

        Ok((
            Self {
                input: String::new(),
                source_base: 0,
                total_input_bytes: 0,
                state: Some(ParserState::document(html, head, body, options)),
                pending_cr: false,
                pending_insertion_cr: false,
                pending_insertion_cr_pos: None,
                paused_script: None,
                insertion_cursor: None,
                insertion_frames: Vec::new(),
                finish_requested: false,
                closed: false,
            },
            removed,
        ))
    }

    /// Append a source chunk and synchronously construct every complete token
    /// while retaining unfinished tokenizer input for the next call.
    pub fn write<D: crate::parser_documents::ParserDocument + ?Sized>(&mut self, document: &mut D, chunk: &str) -> Result<(), ParseError> {
        self.append_chunk(chunk, false)?;
        self.run_available(document, false, false).map(|_| ())
    }

    /// Append input and stop after the next parser-inserted HTML script end tag.
    /// The tree-builder state and unread input remain live for `resume_until_script`.
    pub fn write_until_script<D: crate::parser_documents::ParserDocument + ?Sized>(
        &mut self,
        document: &mut D,
        chunk: &str,
    ) -> Result<Option<NodeId>, ParseError> {
        if self.paused_script.is_some() {
            return Err(error(
                self.total_input_bytes,
                "HTML parser must resume before appending source",
            ));
        }
        self.append_chunk(chunk, false)?;
        let script = self.parse_until_script(document)?;
        Ok(script)
    }

    pub fn write_final_until_script<D: crate::parser_documents::ParserDocument + ?Sized>(&mut self, document: &mut D, chunk: &str) -> Result<Option<NodeId>, ParseError> {
        self.append_chunk(chunk, false)?;
        self.request_close()?;
        self.parse_until_script(document)
    }

    /// Append all Web IDL `document.write()` arguments before parsing them as
    /// one source sequence, yielding at each parser-script boundary.
    pub fn write_parts_until_script<D: crate::parser_documents::ParserDocument + ?Sized>(
        &mut self,
        document: &mut D,
        chunks: &[String],
        append_newline: bool,
    ) -> Result<Option<NodeId>, ParseError> {
        if self.paused_script.is_some() {
            return Err(error(
                self.total_input_bytes,
                "HTML parser must resume before appending source",
            ));
        }
        for chunk in chunks {
            self.append_chunk(chunk, false)?;
        }
        if append_newline {
            self.append_chunk("\n", false)?;
        }
        self.parse_until_script(document)
    }

    fn parse_until_script<D: crate::parser_documents::ParserDocument + ?Sized>(
        &mut self,
        document: &mut D,
    ) -> Result<Option<NodeId>, ParseError> {
        let script = self.run_available(document, self.finish_requested, true)?;
        self.pause_at_script(script);
        if self.finish_requested && script.is_none() && self.pending_element().is_none() {
            self.closed = true;
        }
        Ok(script)
    }

    /// Resume after the embedder has prepared the yielded parser script.
    pub fn resume_until_script<D: crate::parser_documents::ParserDocument + ?Sized>(
        &mut self,
        document: &mut D,
    ) -> Result<Option<NodeId>, ParseError> {
        let Some(finished_script) = self.paused_script.take() else {
            return Err(error(
                self.total_input_bytes,
                "HTML parser is not paused at a script",
            ));
        };
        self.pop_insertion_frame(finished_script)?;
        if !self.insertion_frames.is_empty() {
            return Err(error(
                self.total_input_bytes,
                "HTML parser insertion nesting is unbalanced",
            ));
        }
        self.flush_pending_insertion_cr()?;
        self.insertion_cursor = None;
        let final_input = self.finish_requested;
        let script = self.run_available(document, final_input, true)?;
        self.pause_at_script(script);
        if final_input && script.is_none() && self.pending_element().is_none() {
            self.closed = true;
        }
        Ok(script)
    }

    /// Insert text at the current parser insertion point while a yielded script
    /// runs. Parsing resumes only after that script returns.
    pub fn insert_at_script_position(&mut self, chunk: &str) -> Result<(), ParseError> {
        if self.paused_script.is_none() {
            return Err(error(
                self.total_input_bytes,
                "HTML parser has no active script insertion point",
            ));
        }
        self.append_chunk(chunk, true)
    }

    /// Insert a write at the active parser script's insertion point and parse
    /// only the inserted source before returning. Any parser scripts in that
    /// source are yielded to the embedder; the currently executing script is
    /// restored as the paused parser owner when the inserted region is done.
    pub fn write_at_script_position_until_script<D: crate::parser_documents::ParserDocument + ?Sized>(
        &mut self,
        document: &mut D,
        writer_script: NodeId,
        chunks: &[String],
        append_newline: bool,
    ) -> Result<Option<NodeId>, ParseError> {
        let paused_script = self.paused_script.ok_or_else(|| {
            error(
                self.total_input_bytes,
                "HTML parser has no active script insertion point",
            )
        })?;
        if paused_script != writer_script {
            return Err(error(
                self.total_input_bytes,
                "HTML parser insertion writer does not own the current checkpoint",
            ));
        }
        if chunks.is_empty() && !append_newline {
            return Ok(None);
        }
        self.ensure_insertion_frame(writer_script)?;
        for chunk in chunks {
            self.append_chunk(chunk, true)?;
        }
        if append_newline {
            self.append_chunk("\n", true)?;
        }
        let end = self.insertion_frame_end(writer_script).ok_or_else(|| {
            error(
                self.total_input_bytes,
                "HTML parser insertion region is unavailable",
            )
        })?;
        self.paused_script = None;
        let script = self.run_available_limited(document, false, true, Some(end))?;
        if script.is_some() {
            self.flush_pending_insertion_cr()?;
        }
        self.pause_at_script(Some(script.unwrap_or(writer_script)));
        if script.is_none() {
            self.insertion_cursor = self.insertion_frame_end(writer_script);
        }
        Ok(script)
    }

    /// Continue parsing an inserted write region after the yielded nested
    /// parser script finishes. When the region is exhausted, restore its
    /// caller's still-running parser script without consuming the outer tail.
    pub fn resume_insertion_until_script<D: crate::parser_documents::ParserDocument + ?Sized>(
        &mut self,
        document: &mut D,
        finished_script: NodeId,
        parent_script: NodeId,
    ) -> Result<Option<NodeId>, ParseError> {
        if self.paused_script != Some(finished_script) {
            return Err(error(
                self.total_input_bytes,
                "HTML parser is not paused at an inserted script",
            ));
        }
        self.paused_script = None;
        self.pop_insertion_frame(finished_script)?;
        self.flush_pending_insertion_cr()?;
        let end = self.insertion_frame_end(parent_script).ok_or_else(|| {
            error(
                self.total_input_bytes,
                "HTML parser parent insertion region is unavailable",
            )
        })?;
        let script = self.run_available_limited(document, false, true, Some(end))?;
        if script.is_some() {
            self.flush_pending_insertion_cr()?;
        }
        self.pause_at_script(Some(script.unwrap_or(parent_script)));
        if script.is_none() {
            self.insertion_cursor = self.insertion_frame_end(parent_script);
        }
        Ok(script)
    }

    fn ensure_insertion_frame(&mut self, writer: NodeId) -> Result<(), ParseError> {
        match self.insertion_frames.last() {
            Some(frame) if frame.writer == writer => return Ok(()),
            Some(_)
                if self
                    .insertion_frames
                    .iter()
                    .any(|frame| frame.writer == writer) =>
            {
                return Err(error(
                    self.total_input_bytes,
                    "HTML parser insertion writer is not the innermost script",
                ));
            }
            _ => {}
        }
        if self.insertion_frames.len() >= MAX_INSERTION_DEPTH {
            return Err(error(
                self.total_input_bytes,
                "HTML parser insertion nesting limit exceeded",
            ));
        }
        self.insertion_frames.try_reserve(1).map_err(|_| {
            error(
                self.total_input_bytes,
                "HTML parser allocation limit exceeded",
            )
        })?;
        let end = self.insertion_cursor.ok_or_else(|| {
            error(
                self.total_input_bytes,
                "HTML parser insertion point is unavailable",
            )
        })?;
        self.insertion_frames.push(InsertionFrame { writer, end });
        Ok(())
    }

    fn insertion_frame_end(&self, writer: NodeId) -> Option<usize> {
        self.insertion_frames
            .iter()
            .rev()
            .find(|frame| frame.writer == writer)
            .map(|frame| frame.end)
    }

    fn pop_insertion_frame(&mut self, writer: NodeId) -> Result<(), ParseError> {
        match self.insertion_frames.last() {
            Some(frame) if frame.writer == writer => {
                self.insertion_frames.pop();
                Ok(())
            }
            Some(_)
                if self
                    .insertion_frames
                    .iter()
                    .any(|frame| frame.writer == writer) =>
            {
                Err(error(
                    self.total_input_bytes,
                    "HTML parser insertion frames returned out of order",
                ))
            }
            _ => Ok(()),
        }
    }

    /// Mark EOF while a parser script is active without re-entering the parser.
    /// The next resume consumes the remaining source with EOF rules.
    pub fn request_close(&mut self) -> Result<(), ParseError> {
        if self.closed {
            return Ok(());
        }
        self.finish_requested = true;
        self.flush_pending_insertion_cr()?;
        self.flush_pending_cr()
    }

    pub fn is_paused_for_script(&self) -> bool {
        self.paused_script.is_some()
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn is_finishing(&self) -> bool {
        self.finish_requested
    }

    pub fn pending_element(&self) -> Option<NodeId> {
        self.state.as_ref()?.pending_element.as_ref().map(|pending| pending.node)
    }

    pub fn pending_element_context(&self) -> Option<NodeId> {
        let pending = self.state.as_ref()?.pending_element.as_ref()?;
        pending.insertion.map(|(parent, _)| parent).or_else(|| pending.declarative.map(|(host, _)| host))
    }

    pub fn replace_pending_element(&mut self, replacement: NodeId) {
        let state = self.state.as_mut().expect("parser state between feeds");
        let old = state.pending_element.as_mut().expect("pending element creation").node;
        state.pending_element.as_mut().unwrap().node = replacement;
        for open in state.stack.iter_mut().chain(state.formatting.iter_mut().flatten()) {
            if open.id == old { open.id = replacement; }
        }
        if state.html == Some(old) { state.html = Some(replacement); }
        if state.head == Some(old) { state.head = Some(replacement); }
        if state.body == old { state.body = replacement; }
        if state.form_element == Some(old) { state.form_element = Some(replacement); }
        for select in &mut state.custom_selects { if *select == old { *select = replacement; } }
        for (template, root) in &mut state.declarative_roots {
            if *template == old { *template = replacement; }
            if *root == old { *root = replacement; }
        }
    }

    pub fn append_pending_element_attributes<D: crate::parser_documents::ParserDocument + ?Sized>(&mut self, document: &mut D) -> Result<(), ParseError> {
        let state = self.state.as_mut().expect("parser state between feeds");
        let pending = state.pending_element.as_mut().expect("pending element creation");
        for (name, value) in core::mem::take(&mut pending.attributes) {
            document.set_attribute(pending.node, name.as_str(), &value).map_err(|failure| dom_error(state.pos, failure))?;
        }
        Ok(())
    }

    pub fn insert_pending_element<D: crate::parser_documents::ParserDocument + ?Sized>(&mut self, document: &mut D) -> Result<(), ParseError> {
        let result=(|| {
        let state = self.state.as_mut().expect("parser state between feeds");
        let pending = state.pending_element.take().expect("pending element creation");
        let mut shadow = None;
        if let Some((host, options)) = pending.declarative {
            shadow = match document.attach_shadow_with_options(host, options) {
                Ok(root) => Some(root),
                Err(DomError::WrongKind | DomError::Hierarchy) => None,
                Err(failure) => return Err(dom_error(state.pos, failure)),
            };
        }
        if let Some(form) = pending.parser_form {
            // Token construction reactions run before the form-pointer step.
            // Recheck the actual intended parent's tree after those reactions;
            // the detached constructed element can have another node document.
            let same_tree=if let Some((parent,_))=pending.insertion {
                document.root_node(parent,false).map_err(|failure|dom_error(state.pos,failure))?
                    ==document.root_node(form,false).map_err(|failure|dom_error(state.pos,failure))?
            }else {false};
            if same_tree && document.get_attribute_ns_ref(pending.node, None, "form").map_err(|failure| dom_error(state.pos, failure))?.is_none() {
                document.associate_parser_form(pending.node, form).map_err(|failure| dom_error(state.pos, failure))?;
            }
        }
        if let Some(root) = shadow {
            state.declarative_roots.push((pending.node, root));
        } else if let Some((parent, before)) = pending.insertion {
            insert_at_adjusted_location(document, parent, pending.node, before).map_err(|failure| dom_error(state.pos, failure))?;
        }
        Ok(())
        })();
        // Construction reactions can move this detached element to another
        // arena. Commit every parser identity before the borrowed view's
        // operation-local adoption aliases are released, including failures.
        self.project_retained_nodes(|node|document.current_node(node));
        result
    }

    pub fn resume_after_pending_element<D: crate::parser_documents::ParserDocument + ?Sized>(&mut self, document: &mut D) -> Result<Option<NodeId>, ParseError> {
        if let Some(frame) = self.insertion_frames.last() {
            let writer = frame.writer;
            let end = frame.end;
            self.paused_script = None;
            let script = self.run_available_limited(document, false, true, Some(end))?;
            if script.is_some() { self.flush_pending_insertion_cr()?; }
            self.pause_at_script(Some(script.unwrap_or(writer)));
            if script.is_none() { self.insertion_cursor = self.insertion_frame_end(writer); }
            return Ok(script);
        }
        self.parse_until_script(document)
    }

    pub fn finish_pending_element<D: crate::parser_documents::ParserDocument + ?Sized>(&mut self, document: &mut D) -> Result<Option<NodeId>, ParseError> {
        self.insert_pending_element(document)?;
        self.resume_after_pending_element(document)
    }

    /// Native owners retain the live tree-builder identities across author
    /// script and garbage collection, including detached open elements.
    pub fn has_open_element(&self, node: NodeId) -> bool {
        self.state.as_ref().is_some_and(|state| state.stack.iter().any(|open| open.id == node))
    }

    pub fn visit_retained_nodes(&self, mut visit: impl FnMut(NodeId)) {
        if let Some(state) = &self.state {
            for node in state.html.into_iter().chain(state.head).chain(core::iter::once(state.body)) {
                visit(node);
            }
            for open in state.stack.iter().chain(state.formatting.iter().flatten()) {
                visit(open.id);
            }
            for &(template, root) in &state.declarative_roots {
                visit(template);
                visit(root);
            }
            for &select in &state.custom_selects { visit(select); }
            if let Some(pending) = &state.pending_element {
                visit(pending.node);
                if let Some(form)=pending.parser_form {visit(form);}
                if let Some((parent,before))=pending.insertion {visit(parent);if let Some(before)=before {visit(before);}}
                if let Some((host,_))=pending.declarative {visit(host);}
            }
            for node in state.form_element.into_iter().chain(state.fragment_context_node).chain(state.fragment_registry_target) {visit(node);}
        }
        if let Some(script) = self.paused_script { visit(script); }
        for frame in &self.insertion_frames { visit(frame.writer); }
    }

    /// Project retained tree-builder identities after a real DOM adoption.
    /// Tag/tokenizer state remains unchanged; the parser resumes in the actual
    /// current node document instead of inventing end tags or closing scopes.
    pub fn project_retained_nodes(&mut self, mut project: impl FnMut(NodeId)->NodeId) {
        if let Some(state)=&mut self.state {
            state.project_nodes(&mut project);
        }
        self.paused_script=self.paused_script.map(&mut project);
        for frame in &mut self.insertion_frames {frame.writer=project(frame.writer);}
    }

    fn pause_at_script(&mut self, script: Option<NodeId>) {
        self.paused_script = script;
        self.insertion_cursor = script.and_then(|_| self.state.as_ref().map(|state| state.pos));
    }

    fn append_chunk(&mut self, chunk: &str, at_insertion_point: bool) -> Result<(), ParseError> {
        if self.closed {
            return Err(error(self.total_input_bytes, "HTML parser is closed"));
        }
        let insertion_before = if at_insertion_point {
            Some(self.insertion_cursor.ok_or_else(|| {
                error(
                    self.total_input_bytes,
                    "HTML parser insertion point is unavailable",
                )
            })?)
        } else {
            None
        };
        let resolves_insertion_cr = at_insertion_point
            && self.pending_insertion_cr
            && self.pending_insertion_cr_pos == insertion_before;
        let pending_cr = if resolves_insertion_cr {
            true
        } else if at_insertion_point {
            false
        } else {
            self.pending_cr
        };
        let (normalized, next_pending_cr) =
            normalize_stream_chunk(chunk, pending_cr).map_err(|mut parse_error| {
                parse_error.offset = self.total_input_bytes.saturating_add(parse_error.offset);
                parse_error
            })?;
        let total_length = lumen_common::limits::size::sum(
            self.total_input_bytes,
            normalized.len(),
            MAX_HTML_BYTES,
        )
        .map_err(|_| error(self.total_input_bytes, "HTML input too large"))?;
        let length =
            lumen_common::limits::size::sum(self.input.len(), normalized.len(), MAX_HTML_BYTES)
                .map_err(|_| error(self.total_input_bytes, "HTML input too large"))?;
        self.input
            .try_reserve(length.saturating_sub(self.input.len()))
            .map_err(|_| {
                error(
                    self.total_input_bytes,
                    "HTML parser allocation limit exceeded",
                )
            })?;
        if at_insertion_point {
            let cursor = self.insertion_cursor.ok_or_else(|| {
                error(
                    self.total_input_bytes,
                    "HTML parser insertion point is unavailable",
                )
            })?;
            self.input.insert_str(cursor, &normalized);
            self.insertion_cursor = Some(cursor.saturating_add(normalized.len()));
            for frame in &mut self.insertion_frames {
                if frame.end >= cursor {
                    frame.end = frame.end.saturating_add(normalized.len());
                }
            }
            if self.pending_insertion_cr && !resolves_insertion_cr {
                if let Some(position) = self.pending_insertion_cr_pos.as_mut() {
                    if *position >= cursor {
                        *position = position.saturating_add(normalized.len());
                    }
                }
            }
        } else {
            self.input.push_str(&normalized);
        }
        self.total_input_bytes = total_length;
        if at_insertion_point {
            if next_pending_cr {
                self.pending_insertion_cr = true;
                self.pending_insertion_cr_pos = Some(
                    insertion_before
                        .unwrap_or_default()
                        .saturating_add(normalized.len()),
                );
            } else if resolves_insertion_cr {
                self.pending_insertion_cr = false;
                self.pending_insertion_cr_pos = None;
            }
        } else {
            self.pending_cr = next_pending_cr;
        }
        Ok(())
    }

    fn flush_pending_cr(&mut self) -> Result<(), ParseError> {
        if !self.pending_cr {
            return Ok(());
        }
        self.append_pending_newline(false)?;
        self.pending_cr = false;
        Ok(())
    }

    fn flush_pending_insertion_cr(&mut self) -> Result<(), ParseError> {
        if !self.pending_insertion_cr {
            return Ok(());
        }
        self.append_pending_newline(true)?;
        self.pending_insertion_cr = false;
        self.pending_insertion_cr_pos = None;
        Ok(())
    }

    fn append_pending_newline(&mut self, at_insertion_point: bool) -> Result<(), ParseError> {
        if self.total_input_bytes == MAX_HTML_BYTES {
            return Err(error(self.total_input_bytes, "HTML input too large"));
        }
        let length = lumen_common::limits::size::sum(self.input.len(), 1, MAX_HTML_BYTES)
            .map_err(|_| error(self.total_input_bytes, "HTML input too large"))?;
        self.input
            .try_reserve(length.saturating_sub(self.input.len()))
            .map_err(|_| {
                error(
                    self.total_input_bytes,
                    "HTML parser allocation limit exceeded",
                )
            })?;
        if at_insertion_point {
            let position = self.pending_insertion_cr_pos.ok_or_else(|| {
                error(
                    self.total_input_bytes,
                    "HTML parser pending newline position is unavailable",
                )
            })?;
            self.input.insert(position, '\n');
            if self
                .insertion_cursor
                .is_some_and(|cursor| cursor >= position)
            {
                self.insertion_cursor =
                    self.insertion_cursor.map(|cursor| cursor.saturating_add(1));
            }
            for frame in &mut self.insertion_frames {
                if frame.end >= position {
                    frame.end = frame.end.saturating_add(1);
                }
            }
        } else {
            self.input.push('\n');
        }
        self.total_input_bytes += 1;
        Ok(())
    }

    /// Finish the current stream. Incomplete tokenizer states are resolved by
    /// the same EOF rules as ordinary HTML document parsing.
    pub fn close<D: crate::parser_documents::ParserDocument + ?Sized>(&mut self, document: &mut D) -> Result<(), ParseError> {
        if self.closed {
            return Ok(());
        }
        self.flush_pending_insertion_cr()?;
        self.flush_pending_cr()?;
        self.finish_requested = true;
        self.run_available(document, true, false)?;
        self.closed = true;
        Ok(())
    }

    /// Request EOF and yield parser-inserted scripts one at a time. Reentrant
    /// `close()` calls set the EOF flag and return until the active script exits.
    pub fn finish_until_script<D: crate::parser_documents::ParserDocument + ?Sized>(
        &mut self,
        document: &mut D,
    ) -> Result<Option<NodeId>, ParseError> {
        if self.closed {
            return Ok(None);
        }
        self.finish_requested = true;
        self.flush_pending_insertion_cr()?;
        self.flush_pending_cr()?;
        if self.paused_script.is_some() {
            return Ok(None);
        }
        let script = self.run_available(document, true, true)?;
        self.pause_at_script(script);
        if script.is_none() {
            self.closed = true;
        }
        Ok(script)
    }

    fn run_available<D: crate::parser_documents::ParserDocument + ?Sized>(
        &mut self,
        document: &mut D,
        final_input: bool,
        stop_after_script: bool,
    ) -> Result<Option<NodeId>, ParseError> {
        self.run_available_limited(document, final_input, stop_after_script, None)
    }

    fn run_available_limited<D: crate::parser_documents::ParserDocument + ?Sized>(
        &mut self,
        document: &mut D,
        final_input: bool,
        stop_after_script: bool,
        max_end: Option<usize>,
    ) -> Result<Option<NodeId>, ParseError> {
        let state = self
            .state
            .take()
            .expect("HTML document parser state is available between feeds");
        let available_end = max_end.unwrap_or(self.input.len()).min(self.input.len());
        let safe_end = if final_input {
            available_end
        } else {
            incremental_safe_prefix(&self.input[..available_end], &state, document)
        };
        let result = {
            let input = &self.input[..safe_end];
            let mut parser = Parser {
                input,
                document,
                state,
                final_input,
                stop_after_script,
                yielded_script: None,
            };
            let result = parser.run();
            let yielded_script = parser.yielded_script;
            self.state = Some(parser.state);
            result.map(|()| yielded_script)
        };
        self.project_retained_nodes(|node|document.current_node(node));
        let result = result.map(|script|script.map(|node|document.current_node(node))).map_err(|mut parse_error| {
            parse_error.offset = self.source_base.saturating_add(parse_error.offset);
            parse_error
        })?;
        self.compact_consumed();
        Ok(result)
    }

    fn compact_consumed(&mut self) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let consumed = state.pos.min(self.input.len());
        let pending = self.input.len().saturating_sub(consumed);
        if consumed == 0 || (consumed < 64 * 1024 && consumed < pending) {
            return;
        }
        let tail = self.input[consumed..].to_string();
        self.input = tail;
        self.source_base = self.source_base.saturating_add(consumed);
        state.pos = 0;
        self.insertion_cursor = self
            .insertion_cursor
            .map(|cursor| cursor.saturating_sub(consumed));
        for frame in &mut self.insertion_frames {
            frame.end = frame.end.saturating_sub(consumed);
        }
        self.pending_insertion_cr_pos = self
            .pending_insertion_cr_pos
            .map(|position| position.saturating_sub(consumed));
    }
}

fn normalize_stream_chunk(chunk: &str, mut pending_cr: bool) -> Result<(String, bool), ParseError> {
    if chunk.len() > MAX_HTML_BYTES {
        return Err(error(0, "HTML input too large"));
    }
    let mut output = String::new();
    output
        .try_reserve(chunk.len())
        .map_err(|_| error(0, "HTML parser allocation limit exceeded"))?;
    for character in chunk.chars() {
        if pending_cr {
            output.push('\n');
            pending_cr = false;
            if character == '\n' {
                continue;
            }
        }
        if character == '\r' {
            pending_cr = true;
        } else {
            output.push(character);
        }
    }
    Ok((output, pending_cr))
}

/// Find the end of the largest prefix whose tokenizer input is complete.
/// The tree builder only sees this prefix, so a tag, character reference, or
/// raw-text end tag split across `document.write()` calls cannot be consumed
/// as EOF and then reparsed with different node identities.
fn incremental_safe_prefix<D: crate::parser_documents::ParserDocument + ?Sized>(input: &str, state: &ParserState, document: &D) -> usize {
    let bytes = input.as_bytes();
    let mut cursor = state.pos.min(bytes.len());

    if let Some(open) = state.stack.last() {
        if open.tag == Tag::Plaintext {
            return bytes.len();
        }
        let namespace = document.kind(open.id).ok().and_then(|kind| match kind {
            NodeKind::Element { namespace, .. } => Some(namespace),
            _ => None,
        });
        let raw_name = if open.tag.is_raw_text() {
            Some(open.tag.raw_text_name())
        } else if open.tag == Tag::Noscript
            && state.scripting_enabled
            && namespace == Some(&Namespace::Html)
        {
            Some("noscript")
        } else {
            None
        };
        if let Some(name) = raw_name {
            let relative = raw_text_end_in(input, cursor, name);
            if relative == bytes.len() - cursor {
                // Raw text itself can be emitted incrementally. Keep only a
                // suffix that could still become its appropriate end tag.
                let mut safe_end =
                    trailing_raw_end_tag_prefix(bytes, cursor, name).unwrap_or(bytes.len());
                if safe_end == bytes.len() && open.tag.is_rcdata() {
                    safe_end = trailing_character_reference(input, cursor).unwrap_or(safe_end);
                }
                return safe_end;
            }
            let close_start = cursor + relative;
            let Some(close_end) = find_tag_end(bytes, close_start) else {
                return close_start;
            };
            cursor = close_end + 1;
        }
    }

    while cursor < bytes.len() {
        if bytes[cursor] != b'<' {
            let text_end = bytes[cursor..]
                .iter()
                .position(|&byte| byte == b'<')
                .map_or(bytes.len(), |offset| cursor + offset);
            if text_end == bytes.len() {
                return trailing_character_reference(input, cursor).unwrap_or(text_end);
            }
            cursor = text_end;
            continue;
        }

        let start = cursor;
        let Some(&next) = bytes.get(cursor + 1) else {
            return start;
        };
        if next == b'!' {
            let remaining = &bytes[cursor..];
            if starts_ascii_case_insensitive(remaining, b"<!--") {
                if remaining.len() < 4 {
                    return start;
                }
                if remaining.get(4) == Some(&b'>') {
                    cursor += 5;
                    continue;
                }
                if remaining
                    .get(4..6)
                    .is_some_and(|delimiter| delimiter == b"->")
                {
                    cursor += 6;
                    continue;
                }
                if let Some(end) = comment_end(bytes, cursor + 4) {
                    cursor = end;
                    continue;
                }
                return start;
            }
            if starts_ascii_case_insensitive(remaining, b"<!doctype")
                || is_prefix_ascii_case_insensitive(remaining, b"<!doctype")
            {
                let Some(end) = bytes[cursor..].iter().position(|&byte| byte == b'>') else {
                    return start;
                };
                cursor += end + 1;
                continue;
            }
            if remaining.starts_with(b"<![CDATA[") {
                let foreign = state
                    .stack
                    .last()
                    .and_then(|open| document.kind(open.id).ok())
                    .is_some_and(|kind| {
                        matches!(kind, NodeKind::Element { namespace, .. } if namespace != &Namespace::Html)
                    });
                if foreign {
                    let Some(relative_end) = input[cursor + 9..].find("]]>") else {
                        return start;
                    };
                    cursor += 9 + relative_end + 3;
                    continue;
                }
            }
            let Some(end) = bytes[cursor..].iter().position(|&byte| byte == b'>') else {
                return start;
            };
            cursor += end + 1;
            continue;
        }
        if next == b'?' {
            let Some(end) = bytes[cursor..].iter().position(|&byte| byte == b'>') else {
                return start;
            };
            cursor += end + 1;
            continue;
        }
        if next == b'/' {
            let Some(end) = find_tag_end(bytes, cursor) else {
                return start;
            };
            cursor = end + 1;
            continue;
        }
        if next.is_ascii_alphabetic() {
            let Some(end) = find_tag_end(bytes, cursor) else {
                return start;
            };
            cursor = end + 1;
            continue;
        }

        // A less-than sign that cannot begin markup is character data. Keep
        // scanning so a later complete token in the same write can be built.
        cursor += 1;
    }
    cursor
}

fn trailing_character_reference(input: &str, start: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let ampersand = bytes[start..].iter().rposition(|&byte| byte == b'&')? + start;
    let tail = &bytes[ampersand + 1..];
    if tail
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'#' | b'x' | b'X'))
    {
        Some(ampersand)
    } else {
        None
    }
}

fn find_tag_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, &byte) in bytes.get(start..)?.iter().enumerate() {
        match (quote, byte) {
            (Some(delimiter), current) if delimiter == current => quote = None,
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'>') => return Some(start + offset),
            _ => {}
        }
    }
    None
}

fn comment_end(bytes: &[u8], body_start: usize) -> Option<usize> {
    let rest = bytes.get(body_start..)?;
    let mut from = 0;
    while let Some(found) = rest.get(from..)?.windows(2).position(|pair| pair == b"--") {
        let delimiter = body_start + from + found;
        let tail = bytes.get(delimiter + 2..)?;
        if tail.first() == Some(&b'>') {
            return Some(delimiter + 3);
        }
        if tail.starts_with(b"!>") {
            return Some(delimiter + 4);
        }
        from += found + 1;
    }
    None
}

fn raw_text_end_in(input: &str, start: usize, name: &str) -> usize {
    let bytes = &input.as_bytes()[start..];
    let appropriate = |index: usize, prefix: &[u8]| {
        bytes
            .get(index..index + prefix.len())
            .is_some_and(|value| value.eq_ignore_ascii_case(prefix))
            && bytes
                .get(index + prefix.len())
                .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
    };
    let mut script_state = 0;
    for index in 0..bytes.len() {
        if !matches!(bytes[index], b'<' | b'-') {
            continue;
        }
        if name == "script" {
            if script_state == 0 && bytes[index..].starts_with(b"<!--") {
                script_state = 1;
            } else if script_state != 0 && bytes[index..].starts_with(b"-->") {
                script_state = 0;
            } else if script_state == 1 && appropriate(index, b"<script") {
                script_state = 2;
            } else if script_state == 2 && appropriate(index, b"</script") {
                script_state = 1;
                continue;
            }
        }
        if script_state != 2
            && bytes[index..].starts_with(b"</")
            && bytes
                .get(index + 2..index + 2 + name.len())
                .is_some_and(|value| value.eq_ignore_ascii_case(name.as_bytes()))
            && bytes
                .get(index + 2 + name.len())
                .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
        {
            return index;
        }
    }
    bytes.len()
}

fn trailing_raw_end_tag_prefix(bytes: &[u8], start: usize, name: &str) -> Option<usize> {
    let candidate = bytes.get(start..)?.iter().rposition(|&byte| byte == b'<')? + start;
    let suffix = bytes.get(candidate..)?;
    let expected = name.as_bytes();
    if suffix.first() != Some(&b'<') {
        return None;
    }
    if suffix.len() == 1 {
        return Some(candidate);
    }
    if suffix.get(1) != Some(&b'/') {
        return None;
    }
    let name_start = 2;
    let compared = suffix.len().saturating_sub(name_start).min(expected.len());
    if !suffix[name_start..name_start + compared].eq_ignore_ascii_case(&expected[..compared]) {
        return None;
    }
    if compared < expected.len() {
        return Some(candidate);
    }
    let boundary = name_start + expected.len();
    if suffix
        .get(boundary)
        .is_some_and(|byte| !byte.is_ascii_whitespace() && !matches!(*byte, b'/' | b'>'))
    {
        return None;
    }
    if find_tag_end(bytes, candidate).is_none() {
        return Some(candidate);
    }
    None
}

fn is_prefix_ascii_case_insensitive(bytes: &[u8], word: &[u8]) -> bool {
    bytes.len() < word.len() && word[..bytes.len()].eq_ignore_ascii_case(bytes)
}

/// Run the same bounded scanner used by the HTML parser and return its emitted
/// token stream for conformance testing. `initial_state` uses the html5lib
/// tokenizer corpus labels; unsupported labels are rejected explicitly.
#[doc(hidden)]
pub fn tokenize_for_conformance(
    input: &str,
    initial_state: &str,
    last_start_tag: Option<&str>,
) -> Result<Vec<ConformanceToken>, ParseError> {
    if input.len() > MAX_HTML_BYTES {
        return Err(error(0, "HTML input too large"));
    }
    let initial_tokenizer_state = match initial_state {
        "Data state" => None,
        "PLAINTEXT state" => Some(InitialTokenizerState::Plaintext),
        "RCDATA state" => Some(InitialTokenizerState::Rcdata),
        "RAWTEXT state" => Some(InitialTokenizerState::Rawtext),
        "Script data state" => Some(InitialTokenizerState::ScriptData),
        "CDATA section state" => Some(InitialTokenizerState::Cdata),
        _ => return Err(error(0, "unsupported tokenizer state")),
    };
    let input = normalized_input(input);
    let mut document = Document::new(input.len().saturating_add(4));
    let root = document.root();
    let html = create_html_element(&mut document, "html", Vec::new())
        .map_err(|e| dom_error(0, e))?;
    let head = create_html_element(&mut document, "head", Vec::new())
        .map_err(|e| dom_error(0, e))?;
    let body = create_html_element(&mut document, "body", Vec::new())
        .map_err(|e| dom_error(0, e))?;
    document.attach_detached(root, html);
    document.attach_detached(html, head);
    document.attach_detached(html, body);
    let mut parser = Parser {
        input: &input,
        document: &mut document,
        state: ParserState {
            pos: 0,
            html: Some(html),
            head: Some(head),
            body,
            scaffold_attached: 7,
            pending_element: None,
            stack: Vec::with_capacity(32),
            formatting: Vec::new(),
            scratch: Vec::new(),
            template_modes: Vec::new(),
            declarative_roots: Vec::new(),
            declarative_cleanup_pending: false,
            allow_declarative_shadow_roots: false,
            scripting_enabled: false,
            custom_selects: Vec::new(),
            fragment_context: None,
            fragment_context_kind: None,
            fragment_context_node: None,
            fragment_registry_target: None,
            fragment_text_mode: None,
            form_element: None,
            conformance_tokens: Some(Vec::new()),
            initial_tokenizer_state,
            initial_last_start_tag: last_start_tag.map(str::to_ascii_lowercase),
            in_head: false,
            body_started: false,
            html_started: false,
            after_head: false,
            document_tail: 0,
            doctype_allowed: true,
        },
        final_input: true,
        stop_after_script: false,
        yielded_script: None,
    };
    parser.run()?;
    Ok(parser.conformance_tokens.take().unwrap_or_default())
}

/// Parse a detached fragment in an existing arena for template reuse.
pub fn parse_fragment(document: &mut Document, input: &str) -> Result<NodeId, ParseError> {
    parse_fragment_context(document, input, None, None, None, None, false)
}

/// Parse markup using the tokenizer and table context of an HTML element.
pub fn parse_fragment_in(
    document: &mut Document,
    context: NodeId,
    input: &str,
) -> Result<NodeId, ParseError> {
    parse_fragment_in_with_declarative_shadow_roots(document, context, input, false)
}

/// Use the same fragment parser with explicit declarative-shadow-root admission.
/// Ordinary markup setters keep the default opt-out above.
pub fn parse_fragment_in_with_declarative_shadow_roots(
    document: &mut Document,
    context: NodeId,
    input: &str,
    allow_declarative_shadow_roots: bool,
) -> Result<NodeId, ParseError> {
    parse_fragment_for_target(document,context,context,input,allow_declarative_shadow_roots)
}

/// Preserve the registry target when a shadow host supplies tokenizer context.
pub fn parse_fragment_for_target(
    document: &mut Document,
    context: NodeId,
    registry_target: NodeId,
    input: &str,
    allow_declarative_shadow_roots: bool,
) -> Result<NodeId, ParseError> {
    let mut kind = document.kind(context).map_err(|e| dom_error(0, e))?.clone();
    if !matches!(&kind, NodeKind::Element { .. }) {
        return Err(error(0, "fragment context must be an element"));
    }
    // Fragment parser state needs the DOM local name, not the lexical qualified name.
    // The context itself is never inserted into the result. Reuse an existing Name when
    // it is already local; only qualified contexts need an adjusted interned name.
    if let NodeKind::Element { name, .. } = &mut kind {
        let (prefix, local) = document.element_name_parts(context).map_err(|e| dom_error(0, e))?;
        if prefix.is_some() { *name = Name::new(local); }
    }
    let mut ancestor = Some(context);
    let mut form_element = None;
    while let Some(id) = ancestor {
        if matches!(
            document.kind(id),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                ..
            }) if document.element_name_parts(id).is_ok_and(|(_, local)| local == "form")
        ) {
            form_element = Some(id);
            break;
        }
        ancestor = document.parent(id).map_err(|error| dom_error(0, error))?;
    }
    parse_fragment_context(document, input, Some(kind), Some(context), Some(registry_target), form_element, allow_declarative_shadow_roots)
}

fn parse_fragment_context(
    document: &mut Document,
    input: &str,
    context: Option<NodeKind>,
    context_node: Option<NodeId>,
    registry_target: Option<NodeId>,
    form_element: Option<NodeId>,
    allow_declarative_shadow_roots: bool,
) -> Result<NodeId, ParseError> {
    if input.len() > MAX_HTML_BYTES {
        return Err(error(0, "HTML input too large"));
    }
    let input = normalized_input(input);
    let fragment = document
        .create(NodeKind::DocumentFragment)
        .map_err(|e| dom_error(0, e))?;
    let context_tag = match &context {
        Some(NodeKind::Element {
            namespace: Namespace::Html,
            name,
            ..
        }) => Tag::classify(name),
        _ => Tag::Other,
    };
    let fragment_context = context.as_ref().map(|_| context_tag);
    let fragment_text_mode = match &context {
        Some(NodeKind::Element {
            namespace: Namespace::Html,
            name,
            ..
        }) => match name.as_str() {
            "title" | "textarea" => Some(true),
            "style" | "script" | "iframe" | "xmp" | "noembed" | "noframes" | "plaintext" => {
                Some(false)
            }
            "noscript" if document.scripting_enabled() => Some(false),
            _ => None,
        },
        _ => None,
    };
    let mut stack = Vec::with_capacity(32);
    let scripting_enabled = document.scripting_enabled();
    stack.push(Open {
        id: fragment,
        tag: Tag::Html,
    });
    let mut head = None;
    let mut body = fragment;
    // The html fragment context exposes the same implied head/body scaffold
    // as html5lib's fragment tree. Other contexts contribute only parser state
    // and are never inserted into the returned fragment.
    if fragment_context == Some(Tag::Html) {
        let head_id = match create_html_element(document, "head", Vec::new()) {
            Ok(id) => id,
            Err(error) => {
                document
                    .destroy_subtree(fragment)
                    .map_err(|e| dom_error(0, e))?;
                return Err(dom_error(0, error));
            }
        };
        document.record_parser_element_birth(head_id, registry_target.unwrap_or(fragment), false)
            .map_err(|failure| dom_error(0, failure))?;
        document.attach_detached(fragment, head_id);
        let body_id = match create_html_element(document, "body", Vec::new()) {
            Ok(id) => id,
            Err(error) => {
                document
                    .destroy_subtree(fragment)
                    .map_err(|e| dom_error(0, e))?;
                return Err(dom_error(0, error));
            }
        };
        document.record_parser_element_birth(body_id, registry_target.unwrap_or(fragment), false)
            .map_err(|failure| dom_error(0, failure))?;
        document.attach_detached(fragment, body_id);
        head = Some(head_id);
        body = body_id;
    }
    let result = Parser {
        input: &input,
        document,
        state: ParserState {
            pos: 0,
            html: None,
            head,
            body,
            scaffold_attached: 7,
            pending_element: None,
            stack,
            formatting: Vec::new(),
            scratch: Vec::new(),
            template_modes: if context_tag == Tag::Template {
                alloc::vec![TemplateMode::InTemplate]
            } else {
                Vec::new()
            },
            declarative_roots: Vec::new(),
            declarative_cleanup_pending: false,
            allow_declarative_shadow_roots,
            scripting_enabled,
            custom_selects: Vec::new(),
            // A detached fragment has no context element. Treating `Other` as a
            // context tag made the first unknown element in a fragment impossible
            // to close (for example, sibling `<slot>` elements).
            fragment_context,
            fragment_context_kind: context.clone(),
            fragment_context_node: context_node,
            fragment_registry_target: registry_target,
            fragment_text_mode,
            form_element,
            conformance_tokens: None,
            initial_tokenizer_state: None,
            initial_last_start_tag: None,
            in_head: false,
            body_started: false,
            html_started: true,
            after_head: false,
            document_tail: 0,
            doctype_allowed: false,
        },
        final_input: true,
        stop_after_script: false,
        yielded_script: None,
    }
    .run();
    match result {
        Ok(()) => Ok(fragment),
        Err(error) => {
            document
                .destroy_subtree(fragment)
                .map_err(|e| dom_error(0, e))?;
            Err(error)
        }
    }
}

/// An open element together with its classified name.
#[derive(Clone, Copy)]
struct Open {
    id: NodeId,
    tag: Tag,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TemplateMode {
    InTemplate,
    InTable,
    InColumnGroup,
    InTableBody,
    InRow,
    InBody,
}

struct Parser<'a, 'd, D: crate::parser_documents::ParserDocument + ?Sized> {
    input: &'a str,
    document: &'d mut D,
    state: ParserState,
    final_input: bool,
    stop_after_script: bool,
    yielded_script: Option<NodeId>,
}

struct PendingElement {
    parser_form: Option<NodeId>,
    node: NodeId,
    attributes: Vec<(Name, String)>,
    insertion: Option<(NodeId, Option<NodeId>)>,
    declarative: Option<(NodeId, crate::shadow::ShadowOptions)>,
}

struct ParserState {
    pos: usize,
    html: Option<NodeId>,
    head: Option<NodeId>,
    body: NodeId,
    scaffold_attached: u8,
    pending_element: Option<PendingElement>,
    stack: Vec<Open>,
    formatting: Vec<Option<Open>>,
    scratch: Vec<(Name, String)>,
    template_modes: Vec<TemplateMode>,
    // Only declarative templates need this indirection. Their temporary
    // stack element remains detached while children go straight to the root.
    declarative_roots: Vec<(NodeId, NodeId)>,
    declarative_cleanup_pending: bool,
    allow_declarative_shadow_roots: bool,
    scripting_enabled: bool,
    custom_selects: Vec<NodeId>,
    fragment_context: Option<Tag>,
    // The HTML fragment algorithm keeps the context element outside the
    // returned fragment while still using its namespace and integration-point
    // state as the adjusted current node.
    fragment_context_kind: Option<NodeKind>,
    fragment_context_node: Option<NodeId>,
    fragment_registry_target: Option<NodeId>,
    // Some(true) is RCDATA; Some(false) is raw text/script/plaintext.
    // Fragment parsing has no appropriate end-tag token, so this mode lasts
    // through EOF even if the input contains a matching context end tag.
    fragment_text_mode: Option<bool>,
    form_element: Option<NodeId>,
    conformance_tokens: Option<Vec<ConformanceToken>>,
    initial_tokenizer_state: Option<InitialTokenizerState>,
    initial_last_start_tag: Option<String>,
    in_head: bool,
    body_started: bool,
    html_started: bool,
    after_head: bool,
    // Comment/PI placement after head (1), body (2), and html (3).
    document_tail: u8,
    doctype_allowed: bool,
}

impl ParserState {
    fn project_nodes(&mut self,mut project:impl FnMut(NodeId)->NodeId) {
            self.html=self.html.map(&mut project);self.head=self.head.map(&mut project);self.body=project(self.body);
            for open in self.stack.iter_mut().chain(self.formatting.iter_mut().flatten()) {open.id=project(open.id);}
            for (template,root) in &mut self.declarative_roots {*template=project(*template);*root=project(*root);}
            for select in &mut self.custom_selects {*select=project(*select);}
            self.form_element=self.form_element.map(&mut project);
            self.fragment_context_node=self.fragment_context_node.map(&mut project);
            self.fragment_registry_target=self.fragment_registry_target.map(&mut project);
            if let Some(pending)=&mut self.pending_element {
                pending.node=project(pending.node);pending.parser_form=pending.parser_form.map(&mut project);
                pending.insertion=pending.insertion.map(|(parent,before)|(project(parent),before.map(&mut project)));
                pending.declarative=pending.declarative.map(|(host,options)|(project(host),options));
            }
    }
    fn document(html: NodeId, head: NodeId, body: NodeId, options: ParseOptions) -> Self {
        Self {
            pos: 0,
            html: Some(html),
            head: Some(head),
            body,
            scaffold_attached: 0,
            pending_element: None,
            stack: Vec::with_capacity(32),
            formatting: Vec::new(),
            scratch: Vec::new(),
            template_modes: Vec::new(),
            declarative_roots: Vec::new(),
            declarative_cleanup_pending: false,
            allow_declarative_shadow_roots: options.allow_declarative_shadow_roots,
            scripting_enabled: options.scripting_enabled,
            custom_selects: Vec::new(),
            fragment_context: None,
            fragment_context_kind: None,
            fragment_context_node: None,
            fragment_registry_target: None,
            fragment_text_mode: None,
            form_element: None,
            conformance_tokens: None,
            initial_tokenizer_state: None,
            initial_last_start_tag: None,
            in_head: false,
            body_started: false,
            html_started: false,
            after_head: false,
            document_tail: 0,
            doctype_allowed: true,
        }
    }
}

impl<D: crate::parser_documents::ParserDocument + ?Sized> core::ops::Deref for Parser<'_, '_, D> {
    type Target = ParserState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl<D: crate::parser_documents::ParserDocument + ?Sized> core::ops::DerefMut for Parser<'_, '_, D> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

impl<'a, D: crate::parser_documents::ParserDocument + ?Sized> Parser<'a, '_, D> {
    fn pop_open_element(&mut self) -> Option<Open> {
        let open = self.stack.pop()?;
        self.document.record_parser_element_completion(open.id);
        Some(open)
    }

    fn remove_open_element(&mut self, index: usize) -> Open {
        let open = self.stack.remove(index);
        self.document.record_parser_element_completion(open.id);
        open
    }

    fn truncate_open_elements(&mut self, length: usize) {
        while self.stack.len() > length { self.pop_open_element(); }
    }

    fn registry_context(&self,parent:NodeId)->NodeId {
        if self.fragment_context.is_some() && self.stack.first().is_some_and(|open|open.id==parent) {
            self.fragment_registry_target.unwrap_or(parent)
        } else {parent}
    }
    fn insert_token_element(&mut self, parent: NodeId, node: NodeId, before: Option<NodeId>) -> Result<(), ParseError> {
        self.ensure_scaffold(parent)?;
        if self.pending_element.as_ref().is_some_and(|pending| pending.node == node) {
            let owner = self.document.node_document(parent).map_err(|failure| dom_error(self.pos, failure))?;
            self.document.set_node_document(node, owner).map_err(|failure| dom_error(self.pos, failure))?;
            self.pending_element.as_mut().unwrap().insertion = Some((parent, before));
            return Ok(());
        }
        insert_at_adjusted_location(self.document, parent, node, before).map(|_| ()).map_err(|failure| dom_error(self.pos, failure))
    }

    fn ensure_scaffold(&mut self, parent: NodeId) -> Result<(), ParseError> {
        let Some(html) = self.html else { return Ok(()); };
        if parent != html && Some(parent) != self.head && parent != self.body { return Ok(()); }
        if self.scaffold_attached & 1 == 0 {
            let root = self.document.root();
            if self.pending_element.as_ref().is_some_and(|pending| pending.node == html) {
                self.pending_element.as_mut().unwrap().insertion = Some((root, None));
            } else { insert_at_adjusted_location(self.document, root, html, None).map_err(|e| dom_error(self.pos, e))?; }
            self.scaffold_attached |= 1;
        }
        if Some(parent) == self.head || parent == self.body {
            if self.scaffold_attached & 2 == 0 {
                if let Some(head) = self.head {
                    if self.pending_element.as_ref().is_some_and(|pending| pending.node == head) {
                        self.pending_element.as_mut().unwrap().insertion = Some((html, None));
                    } else { insert_at_adjusted_location(self.document, html, head, None).map_err(|e| dom_error(self.pos, e))?; }
                }
                self.scaffold_attached |= 2;
            }
        }
        if parent == self.body && self.scaffold_attached & 4 == 0 {
            if self.pending_element.as_ref().is_some_and(|pending| pending.node == parent) {
                self.pending_element.as_mut().unwrap().insertion = Some((html, None));
            } else { insert_at_adjusted_location(self.document, html, parent, None).map_err(|e| dom_error(self.pos, e))?; }
            self.scaffold_attached |= 4;
        }
        Ok(())
    }
    fn emit_conformance_token(&mut self, token: ConformanceToken) {
        let Some(tokens) = &mut self.conformance_tokens else {
            return;
        };
        if let ConformanceToken::Character(text) = token {
            if text.is_empty() {
                return;
            }
            if let Some(ConformanceToken::Character(previous)) = tokens.last_mut() {
                previous.push_str(&text);
            } else {
                tokens.push(ConformanceToken::Character(text));
            }
        } else {
            tokens.push(token);
        }
    }

    fn active_html_parser_script(&self) -> Option<NodeId> {
        let open = *self.stack.last()?;
        if open.tag != Tag::Script {
            return None;
        }
        matches!(
            self.document.kind(open.id),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name.as_str() == "script"
        )
        .then_some(open.id)
    }

    fn initial_text_token(&mut self) -> Result<bool, ParseError> {
        let Some(state) = self.initial_tokenizer_state else {
            return Ok(false);
        };
        match state {
            InitialTokenizerState::Plaintext => {
                let text = replace_nulls(self.remaining()).into_owned();
                self.emit_conformance_token(ConformanceToken::Character(text));
                self.pos = self.input.len();
                self.initial_tokenizer_state = None;
                Ok(true)
            }
            InitialTokenizerState::Cdata => {
                let remaining = self.remaining();
                let (end, closes) = remaining
                    .find("]]>")
                    .map_or((remaining.len(), false), |end| (end, true));
                // CDATA sections only occur in foreign content, where the
                // tokenizer emits NULL as a character and tree construction
                // decides how to handle it.
                let text = remaining[..end].to_string();
                self.emit_conformance_token(ConformanceToken::Character(text));
                self.pos += end + if closes { 3 } else { 0 };
                if closes || self.pos == self.input.len() {
                    self.initial_tokenizer_state = None;
                }
                Ok(true)
            }
            InitialTokenizerState::Rcdata
            | InitialTokenizerState::Rawtext
            | InitialTokenizerState::ScriptData => {
                let Some(last_start_tag) = self.initial_last_start_tag.as_deref() else {
                    let raw = replace_nulls(self.remaining());
                    let text = if state == InitialTokenizerState::Rcdata {
                        decode_entities(raw.as_ref(), false).into_owned()
                    } else {
                        raw.into_owned()
                    };
                    self.emit_conformance_token(ConformanceToken::Character(text));
                    self.pos = self.input.len();
                    self.initial_tokenizer_state = None;
                    return Ok(true);
                };
                let end = self.raw_text_end(last_start_tag);
                let closes = end < self.remaining().len();
                if end != 0 {
                    let raw = replace_nulls(&self.input[self.pos..self.pos + end]);
                    let text = if state == InitialTokenizerState::Rcdata {
                        decode_entities(raw.as_ref(), false).into_owned()
                    } else {
                        raw.into_owned()
                    };
                    self.emit_conformance_token(ConformanceToken::Character(text));
                    self.pos += end;
                }
                if closes || self.pos == self.input.len() {
                    self.initial_tokenizer_state = None;
                }
                Ok(true)
            }
        }
    }

    fn raw_text_end(&self, name: &str) -> usize {
        raw_text_end_in(self.input, self.pos, name)
    }

    fn reconstruct_formatting(&mut self) -> Result<(), ParseError> {
        if self.formatting.is_empty() {
            return Ok(());
        }
        let mut index = self.formatting.len();
        while index > 0 {
            match self.formatting[index - 1] {
                Some(entry) if !self.stack.iter().any(|open| open.id == entry.id) => index -= 1,
                _ => break,
            }
        }
        while index < self.formatting.len() {
            let old = self.formatting[index].unwrap();
            let (parent, before) = self.insertion_location(self.parent(), self.parent_tag());
            let id = self.document.clone_shallow_at(parent, old.id).map_err(|e|dom_error(self.pos,e))?;
            self.document.record_parser_element_birth(id, self.registry_context(parent), self.html.is_some()).map_err(|e| dom_error(self.pos, e))?;
            self.ensure_scaffold(parent)?; self.document.insert_before(parent, id, before).map_err(|e| dom_error(self.pos, e))?;
            let open = Open { id, tag: old.tag };
            self.stack.push(open);
            self.formatting[index] = Some(open);
            index += 1;
        }
        Ok(())
    }

    fn end_formatting(&mut self, tag: Tag) -> Result<(), ParseError> {
        for _ in 0..8 {
            let Some(index) = self
                .formatting
                .iter()
                .rposition(|entry| entry.is_some_and(|open| open.tag == tag))
            else {
                self.close_open(tag);
                return Ok(());
            };
            if self.formatting[index + 1..].iter().any(Option::is_none) {
                return Ok(());
            }
            let id = self.formatting[index].unwrap().id;
            let Some(open) = self.stack.iter().position(|node| node.id == id) else {
                self.formatting.remove(index);
                return Ok(());
            };
            if self.stack[open + 1..]
                .iter()
                .any(|node| matches!(node.tag, Tag::Table | Tag::Td | Tag::Th | Tag::Template))
            {
                self.formatting.remove(index);
                return Ok(());
            }
            let block = self.stack[open + 1..]
                .iter()
                .position(|node| node.tag.is_special())
                .map(|position| open + 1 + position);
            let Some(block) = block else {
                self.truncate_open_elements(open);
                self.formatting.remove(index);
                return Ok(());
            };
            let (ancestor, ancestor_tag) = if open == 0 {
                (self.body, Tag::Other)
            } else {
                (self.stack[open - 1].id, self.stack[open - 1].tag)
            };
            let furthest = self.stack[block].id;
            let mut last = furthest;
            let mut bookmark = index;
            for (counter, position) in (open + 1..block).rev().enumerate() {
                let node = self.stack[position];
                let active = self
                    .formatting
                    .iter()
                    .position(|entry| entry.is_some_and(|open| open.id == node.id));
                let active = if counter >= 3 {
                    if let Some(active) = active {
                        self.formatting.remove(active);
                        if active < bookmark {
                            bookmark -= 1;
                        }
                    }
                    None
                } else {
                    active
                };
                let Some(active) = active else {
                    self.remove_open_element(position);
                    continue;
                };
                let copy = self
                    .document
                    .clone_shallow_at(ancestor, node.id)
                    .map_err(|e| dom_error(self.pos, e))?;
                self.document.record_parser_element_birth(copy, self.registry_context(ancestor), self.html.is_some()).map_err(|e| dom_error(self.pos, e))?;
                self.append(ancestor, copy)?;
                let copied = Open {
                    id: copy,
                    tag: node.tag,
                };
                self.formatting[active] = Some(copied);
                self.stack[position] = copied;
                if last == furthest {
                    bookmark = active + 1;
                }
                self.document
                    .detach(last)
                    .map_err(|e| dom_error(self.pos, e))?;
                self.append(copy, last)?;
                last = copy;
            }
            self.document
                .detach(last)
                .map_err(|e| dom_error(self.pos, e))?;
            let (parent, before) = self.insertion_location(ancestor, ancestor_tag);
            self.ensure_scaffold(parent)?; self.document.insert_before(parent, last, before).map_err(|e| dom_error(self.pos, e))?;
            let copy = self
                .document
                .clone_shallow_at(furthest, id)
                .map_err(|e| dom_error(self.pos, e))?;
            self.document.record_parser_element_birth(copy, self.registry_context(furthest), self.html.is_some()).map_err(|e| dom_error(self.pos, e))?;
            while let Some(child) = self
                .document
                .first_child(furthest)
                .map_err(|e| dom_error(self.pos, e))?
            {
                self.document
                    .detach(child)
                    .map_err(|e| dom_error(self.pos, e))?;
                self.append(copy, child)?;
            }
            self.append(furthest, copy)?;
            let active = self
                .formatting
                .iter()
                .position(|entry| entry.is_some_and(|open| open.id == id))
                .unwrap();
            self.formatting.remove(active);
            if active < bookmark {
                bookmark -= 1;
            }
            let copied = Open { id: copy, tag };
            self.formatting.insert(bookmark, Some(copied));
            self.remove_open_element(open);
            let block = self
                .stack
                .iter()
                .position(|node| node.id == furthest)
                .unwrap();
            self.stack.insert(block + 1, copied);
        }
        Ok(())
    }

    fn start_anchor(&mut self) -> Result<(), ParseError> {
        let marker = self
            .formatting
            .iter()
            .rposition(Option::is_none)
            .map_or(0, |index| index + 1);
        let Some(open) = self.formatting[marker..]
            .iter()
            .rev()
            .find_map(|entry| entry.filter(|open| open.tag == Tag::A))
        else {
            return Ok(());
        };

        self.end_formatting(Tag::A)?;
        if let Some(index) = self
            .formatting
            .iter()
            .position(|entry| entry.is_some_and(|entry| entry.id == open.id))
        {
            self.formatting.remove(index);
        }
        if let Some(index) = self.stack.iter().position(|entry| entry.id == open.id) {
            self.remove_open_element(index);
        }
        Ok(())
    }
    fn remaining(&self) -> &str {
        &self.input[self.pos..]
    }

    fn end_tag_close(&self) -> Option<usize> {
        let bytes = self.input.as_bytes();
        let mut quote = None;
        for index in self.pos..bytes.len() {
            match (quote, bytes[index]) {
                (Some(delimiter), byte) if delimiter == byte => quote = None,
                (None, b'\'' | b'"') => quote = Some(bytes[index]),
                (None, b'>') => return Some(index),
                _ => {}
            }
        }
        None
    }
    fn parent(&self) -> NodeId {
        if self.html.is_none() && self.fragment_context == Some(Tag::Html) && self.stack.len() == 1
        {
            return self.body;
        }
        self.stack.last().map_or(
            if self.in_head {
                self.head.unwrap_or(self.body)
            } else {
                self.body
            },
            |open| open.id,
        )
    }

    /// Scaffold and fragment roots are never table or template elements.
    fn parent_tag(&self) -> Tag {
        if self.html.is_none() && self.stack.len() == 1 {
            if let Some(context) = self.fragment_context {
                return if context == Tag::Html {
                    Tag::Body
                } else {
                    context
                };
            }
        }
        self.stack.last().map_or(Tag::Other, |open| open.tag)
    }

    fn current_namespace(&self) -> Namespace {
        self.stack
            .last()
            .map_or(Namespace::Html, |open| match self.document.kind(open.id) {
                Ok(NodeKind::Element { namespace, .. }) => namespace.clone(),
                _ => Namespace::Html,
            })
    }

    fn adjusted_current_kind(&self) -> Option<&NodeKind> {
        if self.at_fragment_context() {
            self.fragment_context_kind.as_ref()
        } else {
            self.stack
                .last()
                .and_then(|open| self.document.kind(open.id).ok())
        }
    }

    fn adjusted_current_namespace(&self) -> Namespace {
        match self.adjusted_current_kind() {
            Some(NodeKind::Element { namespace, .. }) => namespace.clone(),
            _ => self.current_namespace(),
        }
    }

    fn current_mathml_text_integration_point(&self) -> bool {
        matches!(
            self.adjusted_current_kind(),
            Some(NodeKind::Element {
                namespace: Namespace::MathMl,
                name,
                ..
            }) if matches!(name.as_str(), "mi" | "mo" | "mn" | "ms" | "mtext")
        )
    }

    fn current_foreign_integration_point(&self) -> bool {
        match self.adjusted_current_kind() {
            Some(NodeKind::Element {
                namespace: Namespace::Svg,
                name,
                ..
            }) => matches!(name.as_str(), "foreignObject" | "desc" | "title"),
            Some(NodeKind::Element {
                namespace: Namespace::MathMl,
                name,
                attributes,
            }) => {
                matches!(name.as_str(), "mi" | "mo" | "mn" | "ms" | "mtext")
                    || (name == "annotation-xml"
                        && attributes.iter().any(|(key, value)| {
                            key == "encoding"
                                && (value.eq_ignore_ascii_case("text/html")
                                    || value.eq_ignore_ascii_case("application/xhtml+xml"))
                        }))
            }
            _ => false,
        }
    }

    fn foreign_start_tag(
        &mut self,
        name: &str,
        attributes: Vec<(Name, String)>,
        self_closing: bool,
        namespace: Namespace,
        offset: usize,
    ) -> Result<(), ParseError> {
        let element_name = if namespace == Namespace::Svg {
            svg_element_name(name)
        } else {
            name
        };
        let mut foreign_namespaces = Vec::new();
        let attributes = attributes
            .into_iter()
            .map(|(key, value)| {
                let adjusted = if namespace == Namespace::Svg {
                    svg_attribute_name(key.as_str())
                } else if namespace == Namespace::MathMl && key == "definitionurl" {
                    "definitionURL"
                } else {
                    key.as_str()
                };
                if let Some(uri) = foreign_attribute_namespace(key.as_str()) {
                    foreign_namespaces.push((adjusted.to_string(), uri));
                }
                (Name::new(adjusted), value)
            })
            .collect();
        let (parent, before) = self.insertion_location(self.parent(), self.parent_tag());
        let id = self
            .document
            .create_unprefixed_at(parent, namespace, Name::new(element_name), attributes)
            .map_err(|e| dom_error(offset, e))?;
        for (name, uri) in foreign_namespaces {
            self.document
                .set_attribute_namespace_metadata(id, &name, Some(uri))
                .map_err(|e| dom_error(offset, e))?;
        }
        self.document.record_parser_element_birth(id, self.registry_context(parent), self.html.is_some()).map_err(|e| dom_error(offset, e))?;
        self.ensure_scaffold(parent)?; self.document.insert_before(parent, id, before).map_err(|e| dom_error(self.pos, e))?;
        if !self_closing {
            // Foreign elements do not participate in HTML insertion modes or the
            // active formatting list, even when their local name resembles one.
            self.stack.push(Open {
                id,
                tag: Tag::Other,
            });
        }
        Ok(())
    }

    fn foreign_breakout_tag(name: &str, attributes: &[(Name, String)]) -> bool {
        if matches!(
            name,
            "b" | "big"
                | "blockquote"
                | "body"
                | "br"
                | "center"
                | "code"
                | "dd"
                | "div"
                | "dl"
                | "dt"
                | "em"
                | "embed"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "head"
                | "hr"
                | "i"
                | "img"
                | "li"
                | "listing"
                | "menu"
                | "meta"
                | "nobr"
                | "ol"
                | "p"
                | "pre"
                | "ruby"
                | "s"
                | "small"
                | "span"
                | "strong"
                | "strike"
                | "sub"
                | "sup"
                | "table"
                | "tt"
                | "u"
                | "ul"
                | "var"
        ) {
            return true;
        }
        name == "font"
            && attributes
                .iter()
                .any(|(key, _)| matches!(key.as_str(), "color" | "face" | "size"))
    }

    fn pop_foreign_for_html(&mut self) {
        while !self.stack.is_empty() {
            if self.current_namespace() == Namespace::Html
                || self.current_foreign_integration_point()
            {
                break;
            }
            self.pop_open_element();
        }
    }

    fn foreign_end_tag(&mut self, name: &str) {
        for index in (0..self.stack.len()).rev() {
            let open = self.stack[index];
            match self.document.kind(open.id) {
                Ok(NodeKind::Element {
                    namespace,
                    name: open_name,
                    ..
                }) if *namespace != Namespace::Html => {
                    if open_name.eq_ignore_ascii_case(name) {
                        self.close_at(index);
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    fn at_fragment_context(&self) -> bool {
        self.html.is_none()
            && self.stack.len() == 1
            && self.stack[0].tag == Tag::Html
            && self.fragment_context.is_some()
    }

    fn in_fragment_context(&self, tags: &[Tag]) -> bool {
        self.html.is_none()
            && self
                .fragment_context
                .is_some_and(|context| tags.contains(&context))
    }

    fn fragment_context_mode_active(&self, contexts: &[Tag]) -> bool {
        self.in_fragment_context(contexts)
            && !self.stack.iter().skip(1).any(|open| {
                matches!(
                    open.tag,
                    Tag::Table
                        | Tag::Caption
                        | Tag::Colgroup
                        | Tag::Tbody
                        | Tag::Tfoot
                        | Tag::Thead
                        | Tag::Tr
                        | Tag::Td
                        | Tag::Th
                        | Tag::Template
                )
            })
    }

    fn table_insertion_mode(&self) -> Option<Tag> {
        let template = self
            .stack
            .iter()
            .rposition(|open| open.tag == Tag::Template);
        for (index, open) in self.stack.iter().enumerate().rev() {
            if template.is_some_and(|template| index < template) {
                break;
            }
            if matches!(
                open.tag,
                Tag::Table
                    | Tag::Caption
                    | Tag::Colgroup
                    | Tag::Tbody
                    | Tag::Tfoot
                    | Tag::Thead
                    | Tag::Tr
                    | Tag::Td
                    | Tag::Th
            ) {
                return Some(open.tag);
            }
        }
        if let Some(template) = template {
            let mode_index = self.stack[..=template]
                .iter()
                .filter(|open| open.tag == Tag::Template)
                .count()
                .saturating_sub(1);
            return match self.template_modes.get(mode_index) {
                Some(TemplateMode::InTable) => Some(Tag::Table),
                Some(TemplateMode::InColumnGroup) => Some(Tag::Colgroup),
                Some(TemplateMode::InTableBody) => Some(Tag::Tbody),
                Some(TemplateMode::InRow) => Some(Tag::Tr),
                _ => None,
            };
        }
        if self.fragment_context == Some(Tag::Template) {
            return match self.template_modes.last() {
                Some(TemplateMode::InTable) => Some(Tag::Table),
                Some(TemplateMode::InColumnGroup) => Some(Tag::Colgroup),
                Some(TemplateMode::InTableBody) => Some(Tag::Tbody),
                Some(TemplateMode::InRow) => Some(Tag::Tr),
                _ => None,
            };
        }
        self.fragment_context.filter(|tag| {
            matches!(
                tag,
                Tag::Table
                    | Tag::Caption
                    | Tag::Colgroup
                    | Tag::Tbody
                    | Tag::Tfoot
                    | Tag::Thead
                    | Tag::Tr
                    | Tag::Td
                    | Tag::Th
            )
        })
    }

    fn ignore_table_end_tag(&mut self, tag: Tag) -> bool {
        let Some(mode) = self.table_insertion_mode() else {
            return false;
        };
        let ignored = match mode {
            Tag::Table => matches!(
                tag,
                Tag::Body
                    | Tag::Caption
                    | Tag::Col
                    | Tag::Colgroup
                    | Tag::Html
                    | Tag::Tbody
                    | Tag::Td
                    | Tag::Tfoot
                    | Tag::Th
                    | Tag::Thead
                    | Tag::Tr
            ),
            Tag::Caption => matches!(
                tag,
                Tag::Body
                    | Tag::Col
                    | Tag::Colgroup
                    | Tag::Html
                    | Tag::Tbody
                    | Tag::Td
                    | Tag::Tfoot
                    | Tag::Th
                    | Tag::Thead
                    | Tag::Tr
            ),
            Tag::Colgroup => {
                if tag == Tag::Col {
                    true
                } else if tag == Tag::Colgroup {
                    self.close_open(Tag::Colgroup);
                    true
                } else {
                    if self.close_open(Tag::Colgroup) {
                        self.ignore_table_end_tag(tag)
                    } else {
                        true
                    }
                }
            }
            Tag::Tbody | Tag::Tfoot | Tag::Thead => matches!(
                tag,
                Tag::Body
                    | Tag::Caption
                    | Tag::Col
                    | Tag::Colgroup
                    | Tag::Html
                    | Tag::Td
                    | Tag::Th
                    | Tag::Tr
            ),
            Tag::Tr => matches!(
                tag,
                Tag::Body | Tag::Caption | Tag::Col | Tag::Colgroup | Tag::Html | Tag::Td | Tag::Th
            ),
            Tag::Td | Tag::Th => matches!(
                tag,
                Tag::Body | Tag::Caption | Tag::Col | Tag::Colgroup | Tag::Html
            ),
            _ => false,
        };
        ignored
    }

    fn template_content(&self, template: NodeId) -> Option<NodeId> {
        self.declarative_roots
            .iter()
            .rev()
            .find(|(node, _)| *node == template)
            .map(|(_, root)| *root)
            .or_else(|| self.document.template_content(template).ok().flatten())
    }

    fn content_of(&self, parent: NodeId, tag: Tag) -> NodeId {
        if tag == Tag::Template {
            self.template_content(parent).unwrap_or(parent)
        } else {
            parent
        }
    }

    fn append(&mut self, parent: NodeId, child: NodeId) -> Result<(), ParseError> {
        let parent = self.template_content(parent).unwrap_or(parent);
        self.ensure_scaffold(parent)?;
        self.document.append(parent, child).map_err(|e| dom_error(self.pos, e))
    }

    fn comment_location(&mut self)->Result<(NodeId,Option<NodeId>),ParseError> {
        let location=if let Some(html)=self.html.filter(|_|self.document_tail!=0) {
            let parent=if self.document_tail==3 {self.document.root()}else {html};
            let before=(self.document_tail==1 && self.scaffold_attached&4!=0).then_some(self.body);
            (parent,before)
        } else if let Some(html)=self.html.filter(|_|self.after_head && self.stack.is_empty()) {
            (html,(self.scaffold_attached&4!=0).then_some(self.body))
        } else if self.html.is_none() && self.fragment_context==Some(Tag::Html) && matches!(self.document_tail,2|3) {
            (self.stack[0].id,None)
        } else if let Some(html)=self.html.filter(|_|!self.body_started && !self.in_head && self.stack.is_empty()) {
            if self.html_started {(html,self.head.filter(|_|self.scaffold_attached&2!=0))}
            else {(self.document.root(),(self.scaffold_attached&1!=0).then_some(html))}
        } else {let parent=self.parent();(self.template_content(parent).unwrap_or(parent),None)};
        self.ensure_scaffold(location.0)?;Ok(location)
    }
    fn append_comment_kind(&mut self,kind:NodeKind,offset:usize)->Result<(),ParseError> {
        let (parent,before)=self.comment_location()?;
        let id=self.document.create_at(parent,kind,None).map_err(|e|dom_error(offset,e))?;
        self.document.insert_before(parent,id,before).map_err(|e|dom_error(offset,e))
    }

    fn append_text(&mut self, value: Cow<'_, str>, offset: usize) -> Result<(), ParseError> {
        if value.is_empty() {
            return Ok(());
        }
        let blank = value.bytes().all(|byte| byte.is_ascii_whitespace());
        let parent = self.parent();
        let tag = self.parent_tag();
        if tag == Tag::Colgroup && !blank {
            let split = value
                .bytes()
                .position(|byte| !byte.is_ascii_whitespace())
                .unwrap_or(value.len());
            if split != 0 {
                let text_parent = self.content_of(parent, tag);
                self.append_text_at(Cow::Borrowed(&value[..split]), text_parent, None, offset)?;
            }
            let rest = Cow::Owned(value[split..].to_string());
            if self.close_open(Tag::Colgroup) {
                return self.append_text(rest, offset);
            }
            let before = self.stack.first().map(|open| open.id);
            return self.append_text_at(rest, self.body, before, offset);
        }
        let (parent, before) = if blank {
            (self.content_of(parent, tag), None)
        } else {
            self.insertion_location(parent, tag)
        };
        self.append_text_at(value, parent, before, offset)
    }

    fn append_text_at(
        &mut self,
        value: Cow<'_, str>,
        parent: NodeId,
        before: Option<NodeId>,
        offset: usize,
    ) -> Result<(), ParseError> {
        if value.is_empty() {
            return Ok(());
        }
        let previous = if let Some(before) = before {
            self.document.previous_sibling(before)
        } else {
            self.document.last_child(parent)
        }
        .map_err(|e| dom_error(offset, e))?;
        if let Some(last) = previous {
            if matches!(self.document.kind(last), Ok(NodeKind::Text(_))) {
                self.document.append_data(last, &value).map_err(|e| dom_error(offset, e))?;
                return Ok(());
            }
        }
        let id = self
            .document
            .create_at(parent, NodeKind::Text(value.into_owned()), None)
            .map_err(|e| dom_error(offset, e))?;
        self.ensure_scaffold(parent)?; self.document.insert_before(parent, id, before).map_err(|e| dom_error(self.pos, e))?;
        Ok(())
    }

    fn insertion_location(&self, parent: NodeId, tag: Tag) -> (NodeId, Option<NodeId>) {
        if tag == Tag::Template {
            if let Some(content) = self.template_content(parent) {
                return (content, None);
            }
        }
        let template = self
            .stack
            .iter()
            .rposition(|open| open.tag == Tag::Template);
        if template.is_none()
            && self.in_fragment_context(&[Tag::Table, Tag::Tbody, Tag::Tfoot, Tag::Thead, Tag::Tr])
            && !self.stack.iter().any(|open| {
                matches!(
                    open.tag,
                    Tag::Table
                        | Tag::Caption
                        | Tag::Colgroup
                        | Tag::Tbody
                        | Tag::Tfoot
                        | Tag::Thead
                        | Tag::Tr
                        | Tag::Td
                        | Tag::Th
                )
            })
            && !tag.is_table_part()
        {
            return (self.body, None);
        }
        if let (Some(index), Some(mode)) = (template, self.template_modes.last()) {
            let foster_mode = matches!(
                mode,
                TemplateMode::InTable
                    | TemplateMode::InTableBody
                    | TemplateMode::InRow
            );
            if foster_mode && !tag.is_table_part() && tag != Tag::Template {
                if let Some(content) = self.template_content(self.stack[index].id) {
                    return (content, None);
                }
            }
        }
        if tag.is_table_context() {
            let table = self
                .stack
                .iter()
                .enumerate()
                .rev()
                .find(|(index, open)| {
                    open.tag == Tag::Table && template.map_or(true, |boundary| *index >= boundary)
                })
                .map(|(_, open)| open);
            if let Some(table) = table {
                if let Ok(Some(parent)) = self.document.parent(table.id) {
                    return (parent, Some(table.id));
                }
            }
            if let Some(index) = template {
                if let Some(content) = self.template_content(self.stack[index].id) {
                    return (content, None);
                }
            }
        }
        (parent, None)
    }

    fn close_open(&mut self, tag: Tag) -> bool {
        let boundary = if tag == Tag::Template {
            0
        } else {
            self.stack
                .iter()
                .rposition(|open| open.tag == Tag::Template)
                .unwrap_or(0)
        };
        if let Some(index) = (boundary..self.stack.len())
            .rev()
            .find(|&index| self.stack[index].tag == tag)
        {
            if index == 0 && self.html.is_none() && self.stack[index].tag == Tag::Html {
                return false;
            }
            self.close_at(index);
            true
        } else {
            false
        }
    }

    fn close_other(&mut self, name: &str) -> bool {
        for index in (0..self.stack.len()).rev() {
            let open = self.stack[index];
            if open.tag == Tag::Other
                && matches!(self.document.kind(open.id), Ok(NodeKind::Element { name: open, .. }) if open.eq_ignore_ascii_case(name))
            {
                self.close_at(index);
                return true;
            }
            if open.tag.is_special() {
                return false;
            }
        }
        false
    }

    fn close_at(&mut self, index: usize) {
        if index == 0 && self.html.is_none() && self.stack[index].tag == Tag::Html {
            return;
        }
        let markers = self.stack[index..]
            .iter()
            .filter(|open| open.tag.is_marker())
            .count();
        for _ in 0..markers {
            self.clear_formatting();
        }
        let removed_templates = self.stack[index..]
            .iter()
            .filter(|open| open.tag == Tag::Template)
            .count();
        let removed_selects: Vec<_> = self.stack[index..]
            .iter()
            .filter(|open| open.tag == Tag::Select)
            .map(|open| open.id)
            .collect();
        if removed_templates != 0 {
            self.declarative_cleanup_pending = !self.declarative_roots.is_empty();
            let remaining_modes = self.template_modes.len().saturating_sub(removed_templates);
            self.template_modes.truncate(remaining_modes);
        }
        self.custom_selects
            .retain(|select| !removed_selects.contains(select));
        self.truncate_open_elements(index);
    }

    fn clear_formatting(&mut self) {
        while let Some(entry) = self.formatting.pop() {
            if entry.is_none() {
                break;
            }
        }
    }

    fn clear_to_table_context(&mut self, tags: &[Tag]) {
        while self
            .stack
            .last()
            .is_some_and(|open| open.tag != Tag::Template && !tags.contains(&open.tag))
            && !(self.html.is_none() && self.stack.len() == 1 && self.stack[0].tag == Tag::Html)
        {
            self.pop_open_element();
        }
    }

    fn close_in_scope(&mut self, tags: &[Tag], boundary: &[Tag]) {
        for index in (0..self.stack.len()).rev() {
            let tag = self.stack[index].tag;
            if tags.contains(&tag) {
                self.close_at(index);
                return;
            }
            if boundary.contains(&tag) {
                return;
            }
        }
    }

    fn active_select(&self) -> Option<NodeId> {
        self.stack
            .iter()
            .rev()
            .find(|open| open.tag == Tag::Select)
            .map(|open| open.id)
    }

    fn select_is_custom(&self, id: NodeId) -> bool {
        self.custom_selects.contains(&id)
    }

    fn custom_select_content_tag(tag: Tag) -> bool {
        matches!(
            tag,
            Tag::Hr
                | Tag::Div
                | Tag::Button
                | Tag::SelectedContent
                | Tag::B
                | Tag::Code
                | Tag::Em
                | Tag::I
                | Tag::S
                | Tag::Small
                | Tag::Strong
                | Tag::U
                | Tag::Other
        )
    }

    fn in_scope(&self, wanted: Tag) -> bool {
        for open in self.stack.iter().rev() {
            if open.tag == wanted {
                return true;
            }
            if matches!(
                open.tag,
                Tag::Caption
                    | Tag::Html
                    | Tag::Object
                    | Tag::Table
                    | Tag::Td
                    | Tag::Th
                    | Tag::Template
            ) {
                return false;
            }
        }
        false
    }

    fn in_scope_before_boundary(&self, wanted: Tag, boundary: &[Tag]) -> bool {
        for open in self.stack.iter().rev() {
            if open.tag == wanted {
                return true;
            }
            if boundary.contains(&open.tag) {
                return false;
            }
        }
        false
    }

    fn implied_end_tags(&mut self, except: Option<Tag>) {
        while self.stack.last().is_some_and(|open| {
            Some(open.tag) != except
                && matches!(
                    open.tag,
                    Tag::Dd
                        | Tag::Dt
                        | Tag::Li
                        | Tag::Optgroup
                        | Tag::Option
                        | Tag::P
                        | Tag::Rb
                        | Tag::Rp
                        | Tag::Rt
                        | Tag::Rtc
                )
        }) {
            self.pop_open_element();
        }
    }

    fn merge_scaffold_attributes(&mut self, id: NodeId, attributes: Vec<(Name, String)>) -> Result<(), ParseError> {
        let bit = if self.html == Some(id) { 1 } else if self.head == Some(id) { 2 } else if self.body == id { 4 } else { 0 };
        let is_value = attributes.iter().find(|(name, _)| name == "is").map(|(_, value)| value.as_str());
        let defined = match self.document.kind(id).map_err(|failure| dom_error(self.pos, failure))? {
            NodeKind::Element { name, .. } => self.document.parser_custom_element_defined(self.document.root(), name.as_str(), is_value),
            _ => false,
        };
        if self.stop_after_script && bit != 0 && self.scaffold_attached & bit == 0 && defined {
            self.document.initialize_parser_is_value(id, is_value).map_err(|failure| dom_error(self.pos, failure))?;
            self.pending_element = Some(PendingElement { node: id, attributes, insertion: None, declarative: None, parser_form: None });
            return Ok(());
        }
        for (key, value) in attributes {
            if self.document.get_attribute_ns_ref(id, None, key.as_str())
                .map_err(|error| dom_error(self.pos, error))?.is_none() {
                self.document.set_attribute(id, key.as_str(), &value)
                    .map_err(|error| dom_error(self.pos, error))?;
            }
        }
        Ok(())
    }

    fn create_plain(&mut self, name: &'static str, offset: usize) -> Result<NodeId, ParseError> {
        let (parent,_)=self.insertion_location(self.parent(), self.parent_tag());
        let id=self.document.create_unprefixed_at(parent, Namespace::Html, Name::new(name), Vec::new()).map_err(|e| dom_error(offset, e))?;
        self.document.record_parser_element_birth(id,self.registry_context(self.parent()), self.html.is_some()).map_err(|e| dom_error(offset,e))?;
        Ok(id)
    }

    fn run(&mut self) -> Result<(), ParseError> {
        let input = self.input;
        let mut identity_revision=self.document.identity_revision();
        while self.pos < input.len() {
            let revision=self.document.identity_revision();
            if revision!=identity_revision {
                let document=&*self.document;
                self.state.project_nodes(|node|document.current_node(node));
                identity_revision=revision;
            }
            if self.initial_text_token()? {
                continue;
            }
            if self.doctype_allowed
                && self.document.document_mode() == crate::DocumentMode::NoQuirks
                && self.initial_token_forces_quirks()
            {
                // A script-created parser starts with no-quirks mode, but
                // still runs the ordinary initial insertion mode. If no
                // doctype appears before the first non-whitespace,
                // non-comment token, that mode switches the document to
                // quirks. One-shot parsing starts in quirks already, so this
                // only changes the explicit document.open starting state.
                self.document.set_document_mode(crate::DocumentMode::Quirks);
            }
            if self.at_fragment_context() {
                if let Some(rcdata) = self.fragment_text_mode {
                    let raw = replace_nulls(&input[self.pos..]);
                    let text = if rcdata {
                        decode_entities(raw.as_ref(), false)
                    } else {
                        raw
                    };
                    if self.conformance_tokens.is_some() {
 self.emit_conformance_token(ConformanceToken::Character(text.to_string()));
 }
                    self.append_text(text, self.pos)?;
                    self.pos = input.len();
                    continue;
                }
            }
            if let Some(&open) = self.stack.last() {
                if open.tag == Tag::Plaintext {
                    let text = replace_nulls(&input[self.pos..]);
                    if self.conformance_tokens.is_some() {
 self.emit_conformance_token(ConformanceToken::Character(text.to_string()));
 }
                    self.append_text(text, self.pos)?;
                    self.pos = input.len();
                    continue;
                }
                if open.tag.is_raw_text()
                    || (open.tag == Tag::Noscript
                        && self.scripting_enabled
                        && self.adjusted_current_namespace() == Namespace::Html)
                {
                    let end = self.raw_text_end(open.tag.raw_text_name());
                    if end > 0 {
                        let raw = replace_nulls(&input[self.pos..self.pos + end]);
                        let text = if open.tag.is_rcdata() {
                            decode_entities(&raw, false)
                        } else {
                            raw
                        };
                        if self.conformance_tokens.is_some() {
 self.emit_conformance_token(ConformanceToken::Character(text.to_string()));
 }
                        self.append_text(text, self.pos)?;
                        self.pos += end;
                        continue;
                    }
                }
            }
            let rest = &input[self.pos..];
            let bytes = rest.as_bytes();
            if bytes[0] != b'<' {
                self.text()?;
            } else if rest.starts_with("<!--") {
                self.comment()?;
            } else if bytes.len() >= 9 && bytes[..9].eq_ignore_ascii_case(b"<!doctype") {
                self.doctype()?;
            } else if rest.starts_with("</") {
                let parser_script = self
                    .stop_after_script
                    .then(|| self.active_html_parser_script())
                    .flatten();
                let parser_style = self.stack.last().filter(|open| open.tag == Tag::Style).map(|open| open.id);
                self.end_tag()?;
                if let Some(style) = parser_style.filter(|style| !self.stack.iter().any(|open| open.id == *style)) {
                    self.document.record_parser_style_block_update(style).map_err(|error| dom_error(self.pos, error))?;
                }
                if parser_script
                    .is_some_and(|script| !self.stack.iter().any(|open| open.id == script))
                {
                    self.yielded_script = parser_script;
                }
            } else if rest.starts_with("<?") {
                self.processing_instruction()?;
            } else if rest.starts_with("<![CDATA[")
                && self.adjusted_current_namespace() != Namespace::Html
            {
                self.cdata_section()?;
            } else if rest.starts_with("<!") {
                self.bogus_comment(self.pos + 2)?;
            } else if bytes.get(1).is_some_and(u8::is_ascii_alphabetic) {
                self.start_tag()?;
            } else {
                self.text()?;
            }
            if self.declarative_cleanup_pending {
                self.reclaim_declarative_templates(false)?;
            }
            if self.yielded_script.is_some() || self.pending_element.is_some() {
                break;
            }
        }
        if self.final_input && self.yielded_script.is_none() && self.pending_element.is_none() {
            if let Some(style) = self.stack.last().filter(|open| open.tag == Tag::Style).map(|open| open.id) {
                self.pop_open_element();
                self.document.record_parser_style_block_update(style).map_err(|error| dom_error(self.pos, error))?;
            }
            if self.html.is_some() {
                self.ensure_scaffold(self.body)?;
            }
            self.refresh_selected_content()?;
            // EOF completes remaining source-owned elements even when HTML's
            // stack remains open. The sink only records completion; native
            // loading is queued after this borrowed parser turn returns.
            for open in self.stack.iter().rev() {
                self.document.record_parser_element_completion(open.id);
            }

            // The spec's parser-only template stack elements must not retain an
            // extra template/content pair in the document arena after parsing.
            self.reclaim_declarative_templates(true)?;
        }
        Ok(())
    }

    fn initial_token_forces_quirks(&self) -> bool {
        let rest = &self.input[self.pos..];
        if rest.starts_with("<!--")
            || rest.starts_with("<?")
            || rest.starts_with("<!")
            || starts_ascii_case_insensitive(rest.as_bytes(), b"<!doctype")
        {
            return false;
        }
        let first_text_end = rest.find('<').unwrap_or(rest.len());
        rest[..first_text_end]
            .bytes()
            .any(|byte| !byte.is_ascii_whitespace())
            || rest
                .as_bytes()
                .first()
                .is_some_and(|byte| !byte.is_ascii_whitespace() && *byte == b'<')
    }

    fn reclaim_declarative_templates(&mut self, all: bool) -> Result<(), ParseError> {
        let mut index = 0;
        while index < self.declarative_roots.len() {
            let template = self.declarative_roots[index].0;
            if all || !self.stack.iter().any(|open| open.id == template) {
                self.document
                    .destroy_subtree(template)
                    .map_err(|e| dom_error(self.pos, e))?;
                self.declarative_roots.swap_remove(index);
            } else {
                index += 1;
            }
        }
        self.declarative_cleanup_pending = false;
        Ok(())
    }

    fn refresh_selected_content(&mut self) -> Result<(), ParseError> {
        let mut pending = alloc::vec![self.body];
        let mut selects = Vec::new();
        while let Some(node) = pending.pop() {
            if let Some(root) = self
                .document
                .shadow_root(node)
                .map_err(|e| dom_error(self.pos, e))?
            {
                pending.push(root);
            }
            if matches!(
                self.document.kind(node),
                Ok(NodeKind::Element { namespace: Namespace::Html, name, .. }) if name == "select"
            ) {
                selects.push(node);
            }
            let content = self
                .document
                .template_content(node)
                .map_err(|error| dom_error(self.pos, error))?;
            // A parser script can adopt associated template contents into a
            // different arena. Their control state then belongs to that
            // document's ordinary adoption/mutation hooks, not this parser's
            // owner-local completion refresh.
            if content.is_some_and(|content| content.document_id() != self.document.root().document_id()) {
                continue;
            }
            let mut child = self
                .document
                .last_child(content.unwrap_or(node))
                .map_err(|error| dom_error(self.pos, error))?;
            while let Some(id) = child {
                pending.push(id);
                child = self
                    .document
                    .previous_sibling(id)
                    .map_err(|error| dom_error(self.pos, error))?;
            }
        }
        for select in selects {
            let mut pending = alloc::vec![select];
            let mut options = Vec::new();
            let mut selected_content = Vec::new();
            while let Some(node) = pending.pop() {
                if node != select
                    && matches!(
                        self.document.kind(node),
                        Ok(NodeKind::Element { namespace: Namespace::Html, name, .. }) if name == "select"
                    )
                {
                    continue;
                }
                match self.document.kind(node) {
                    Ok(NodeKind::Element {
                        namespace: Namespace::Html,
                        name,
                        attributes,
                    }) if name == "option" => {
                        let is_selected = attributes.iter().any(|(key, _)| key == "selected");
                        options.push((node, is_selected));
                    }
                    Ok(NodeKind::Element {
                        namespace: Namespace::Html,
                        name,
                        ..
                    }) if name == "selectedcontent" => selected_content.push(node),
                    _ => {}
                }
                let mut child = self
                    .document
                    .last_child(node)
                    .map_err(|error| dom_error(self.pos, error))?;
                while let Some(id) = child {
                    pending.push(id);
                    child = self
                        .document
                        .previous_sibling(id)
                        .map_err(|error| dom_error(self.pos, error))?;
                }
            }
            let source = options
                .iter()
                .find(|(_, selected)| *selected)
                .or_else(|| options.first())
                .map(|(id, _)| *id);
            let mut source_children = Vec::new();
            if let Some(source) = source {
                let mut child = self
                    .document
                    .first_child(source)
                    .map_err(|error| dom_error(self.pos, error))?;
                while let Some(id) = child {
                    source_children.push(id);
                    child = self
                        .document
                        .next_sibling(id)
                        .map_err(|error| dom_error(self.pos, error))?;
                }
            }
            for target in selected_content {
                while let Some(child) = self
                    .document
                    .first_child(target)
                    .map_err(|error| dom_error(self.pos, error))?
                {
                    self.document
                        .detach(child)
                        .map_err(|error| dom_error(self.pos, error))?;
                    self.document
                        .destroy_subtree(child)
                        .map_err(|error| dom_error(self.pos, error))?;
                }
                for child in &source_children {
                    let clone = self
                        .document
                        .clone_subtree(*child)
                        .map_err(|error| dom_error(self.pos, error))?;
                    self.append(target, clone)?;
                }
            }
        }
        Ok(())
    }

    fn bogus_comment(&mut self, start: usize) -> Result<(), ParseError> {
        let end = self.input[start..]
            .find('>')
            .map_or(self.input.len(), |end| start + end);
        let data = replace_nulls(&self.input[start..end]).into_owned();
        if self.conformance_tokens.is_some() {
            self.emit_conformance_token(ConformanceToken::Comment(data.clone()));
        }
        self.append_comment_kind(NodeKind::Comment(data), self.pos)?;
        self.pos = (end + 1).min(self.input.len());
        Ok(())
    }

    fn cdata_section(&mut self) -> Result<(), ParseError> {
        let start = self.pos;
        let content_start = start + "<![CDATA[".len();
        let (end, closed) = self.input[content_start..]
            .find("]]>")
            .map_or((self.input.len(), false), |relative| {
                (content_start + relative, true)
            });
        let text = replace_nulls(&self.input[content_start..end]);
        if self.conformance_tokens.is_some() {
 self.emit_conformance_token(ConformanceToken::Character(text.to_string()));
 }
        self.append_text(text, start)?;
        self.pos = end + if closed { 3 } else { 0 };
        Ok(())
    }

    // HTML's processing-instruction states, including reserved XML targets and
    // the bogus-comment fallback. Incomplete valid instructions vanish at EOF.
    fn processing_instruction(&mut self) -> Result<(), ParseError> {
        let start = self.pos;
        let bytes = self.input.as_bytes();
        let target_start = start + 2;
        let Some(&first) = bytes.get(target_start) else {
            self.pos = bytes.len();
            return Ok(());
        };
        if !first.is_ascii_alphabetic() && first != b'_' {
            return self.bogus_comment(start + 1);
        }
        let mut end = target_start + 1;
        while bytes
            .get(end)
            .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            end += 1;
        }
        let Some(&delimiter) = bytes.get(end) else {
            self.pos = bytes.len();
            return Ok(());
        };
        let target = &self.input[target_start..end];
        if !matches!(delimiter, b'\t' | b'\n' | b'\x0c' | b' ' | b'?' | b'>')
            || target.eq_ignore_ascii_case("xml")
            || target.eq_ignore_ascii_case("xml-stylesheet")
        {
            return self.bogus_comment(start + 1);
        }
        let mut data_start = end;
        while bytes
            .get(data_start)
            .is_some_and(|b| matches!(b, b'\t' | b'\n' | b'\x0c' | b' '))
        {
            data_start += 1;
        }
        let Some(relative_end) = self.input[data_start..].find('>') else {
            self.pos = bytes.len();
            return Ok(());
        };
        let close = data_start + relative_end;
        let data_end = if close > data_start && bytes[close - 1] == b'?' {
            close - 1
        } else {
            close
        };
        let data = self.input[data_start..data_end].to_string();
        self.emit_conformance_token(ConformanceToken::ProcessingInstruction {
            target: target.to_string(),
            data: data.clone(),
        });
        self.append_comment_kind(NodeKind::ProcessingInstruction {target:target.to_string(),data},start)?;
        self.pos = close + 1;
        Ok(())
    }

    fn comment(&mut self) -> Result<(), ParseError> {
        let start = self.pos;
        let rest = &self.input[start + 4..];
        let mut ending = None;
        let mut from = 0;
        while let Some(found) = rest[from..].find("--") {
            let index = from + found;
            let tail = &rest[index + 2..];
            if tail.starts_with('>') {
                ending = Some((index, 3));
                break;
            }
            if tail.starts_with("!>") {
                ending = Some((index, 4));
                break;
            }
            from = index + 1;
        }
        let pending_hyphens = rest
            .as_bytes()
            .iter()
            .rev()
            .take(2)
            .take_while(|&&b| b == b'-')
            .count();
        let (end, suffix) = if rest.starts_with('>') {
            (0, 1)
        } else if rest.starts_with("->") {
            (0, 2)
        } else if rest == "-" {
            (0, 1)
        } else if ending.is_none() && rest.ends_with("--!") {
            // EOF in comment-end-bang consumes the pending `--!` delimiter.
            (rest.len() - 3, 3)
        } else if ending.is_none() {
            // EOF in comment-end[-dash] emits the token without the pending
            // delimiter hyphens (HTML §13.2.5.44 and §13.2.5.48–50).
            (rest.len() - pending_hyphens, pending_hyphens)
        } else {
            ending.unwrap()
        };
        let data = replace_nulls(&rest[..end]).into_owned();
        self.emit_conformance_token(ConformanceToken::Comment(data.clone()));
        self.append_comment_kind(NodeKind::Comment(data), start)?;
        self.pos = start + 4 + end + suffix;
        Ok(())
    }

    fn doctype(&mut self) -> Result<(), ParseError> {
        let start = self.pos;
        let bytes = self.input.as_bytes();
        let end = (self.pos + 9..bytes.len()).find(|&index| bytes[index] == b'>');
        let terminated = end.is_some();
        let end = end.unwrap_or(bytes.len());
        let raw = &self.input[start + 9..end];
        let doctype = parse_doctype(raw, terminated);
        if self.conformance_tokens.is_some() {
            self.emit_conformance_token(doctype.conformance_token());
        }
        if !self.doctype_allowed {
            self.pos = (end + if terminated { 1 } else { 0 }).min(self.input.len());
            return Ok(());
        }
        self.doctype_allowed = false;
        self.document
            .set_document_mode(document_mode_for_doctype(&doctype));
        let id = self
            .document
            .create(NodeKind::DocumentType(
                doctype.name.as_deref().unwrap_or("").to_string(),
            ))
            .map_err(|e| dom_error(start, e))?;
        self.document
            .set_doctype_identifiers(
                id,
                doctype.public_id.as_deref().unwrap_or(""),
                doctype.system_id.as_deref().unwrap_or(""),
            )
            .map_err(|e| dom_error(start, e))?;
        self.document
            .insert_before(self.document.root(), id, self.html.filter(|_| self.scaffold_attached & 1 != 0))
            .map_err(|error| dom_error(start, error))?;
        self.pos = (end + if terminated { 1 } else { 0 }).min(self.input.len());
        Ok(())
    }

    fn end_tag(&mut self) -> Result<(), ParseError> {
        let start = self.pos;
        self.pos += 2;
        if self.remaining().starts_with('>') {
            self.pos += 1;
            return Ok(());
        }
        let Some(&first) = self.remaining().as_bytes().first() else {
            self.emit_conformance_token(ConformanceToken::Character("</".to_string()));
            self.append_text(Cow::Borrowed("</"), start)?;
            return Ok(());
        };
        if !first.is_ascii_alphabetic() {
            let end = self.remaining().find('>').unwrap_or(self.remaining().len());
            if end > 0 {
                let data = replace_nulls(&self.remaining()[..end]).into_owned();
                self.emit_conformance_token(ConformanceToken::Comment(data.clone()));
                self.append_comment_kind(NodeKind::Comment(data), start)?;
            }
            self.pos = (self.pos + end + 1).min(self.input.len());
            return Ok(());
        }
        let name = self.name()?;
        let Some(end) = self.end_tag_close() else {
            // EOF in an end-tag state emits EOF and discards the unfinished
            // token. In text modes, the appropriate end-tag prefix was
            // already consumed as markup by the tokenizer.
            self.pos = self.input.len();
            return Ok(());
        };
        self.pos = end + 1;
        if self.conformance_tokens.is_some() {
 self.emit_conformance_token(ConformanceToken::EndTag(name.to_string()));
 }
        let tag = Tag::classify(&name);
        let namespace = self.adjusted_current_namespace();
        if namespace != Namespace::Html && !self.current_foreign_integration_point() {
            if matches!(name.as_ref(), "br" | "p") {
                self.pop_foreign_for_html();
            } else {
                self.foreign_end_tag(&name);
                return Ok(());
            }
        }
        if self.ignore_table_end_tag(tag) {
            return Ok(());
        }
        if self.html.is_some()
            && self.stack.is_empty()
            && !self.body_started
            && !self.in_head
            && !matches!(tag, Tag::Head | Tag::Body | Tag::Html | Tag::Br)
        {
            return Ok(());
        }
        if self.html.is_none() && self.fragment_context == Some(Tag::Html) && self.template_modes.is_empty() {
            match tag {
                Tag::Body => {
                    self.document_tail = 2;
                    return Ok(());
                }
                Tag::Html => {
                    self.document_tail = 3;
                    return Ok(());
                }
                _ => {}
            }
        }
        if !self.template_modes.is_empty() && matches!(tag, Tag::Head | Tag::Body | Tag::Html) {
            return Ok(());
        }
        if self.in_head && self.parent_tag() == Tag::Noscript {
            match tag {
                Tag::Noscript => {
                    self.close_open(Tag::Noscript);
                    return Ok(());
                }
                Tag::Head => {
                    self.close_open(Tag::Noscript);
                }
                Tag::Br => {
                    self.close_open(Tag::Noscript);
                    self.in_head = false;
                    self.body_started = true;
                    self.after_head = false;
                }
                _ => return Ok(()),
            }
        }
        if self.html.is_some()
            && self.in_head
            && self.stack.is_empty()
            && !matches!(
                tag,
                Tag::Head | Tag::Body | Tag::Html | Tag::Template | Tag::Br
            )
        {
            return Ok(());
        }
        if self.html.is_some() && self.in_head && self.stack.is_empty() && tag == Tag::Br {
            self.in_head = false;
            self.body_started = true;
            self.after_head = false;
        }
        match tag {
            Tag::Br => {
                self.reconstruct_formatting()?;
                let id = self.create_plain("br", start)?;
                let (parent, before) = self.insertion_location(self.parent(), self.parent_tag());
                self.ensure_scaffold(parent)?; self.document.insert_before(parent, id, before).map_err(|e| dom_error(self.pos, e))?;
            }
            Tag::P => {
                let boundary = [
                    Tag::Button,
                    Tag::Caption,
                    Tag::Html,
                    Tag::Object,
                    Tag::Table,
                    Tag::Td,
                    Tag::Th,
                    Tag::Template,
                ];
                if !self.in_scope_before_boundary(Tag::P, &boundary) {
                    let id = self.create_plain("p", start)?;
                    let (parent, before) =
                        self.insertion_location(self.parent(), self.parent_tag());
                    self.ensure_scaffold(parent)?; self.document.insert_before(parent, id, before).map_err(|e| dom_error(self.pos, e))?;
                } else {
                    self.close_in_scope(&[Tag::P], &boundary);
                }
            }
            Tag::Head if self.html.is_some() => {
                self.in_head = false;
                self.body_started = false;
                self.after_head = true;
                self.truncate_open_elements(0);
                self.document_tail = 1;
            }
            Tag::Body if self.html.is_some() => {
                self.body_started = true;
                self.in_head = false;
                self.after_head = false;
                self.document_tail = 2;
            }
            Tag::Html if self.html.is_some() => {
                self.body_started = true;
                self.in_head = false;
                self.after_head = false;
                self.document_tail = 3;
            }
            Tag::H1 | Tag::H2 | Tag::H3 | Tag::H4 | Tag::H5 | Tag::H6 => {
                let headings = [Tag::H1, Tag::H2, Tag::H3, Tag::H4, Tag::H5, Tag::H6];
                self.close_in_scope(
                    &headings,
                    &[
                        Tag::Html,
                        Tag::Object,
                        Tag::Table,
                        Tag::Td,
                        Tag::Th,
                        Tag::Template,
                    ],
                );
            }
            Tag::Li => {
                let boundary = [
                    Tag::Html,
                    Tag::Object,
                    Tag::Table,
                    Tag::Td,
                    Tag::Th,
                    Tag::Template,
                    Tag::Ol,
                    Tag::Ul,
                ];
                if self.in_scope_before_boundary(Tag::Li, &boundary) {
                    self.implied_end_tags(Some(Tag::Li));
                    self.close_in_scope(&[Tag::Li], &boundary);
                }
            }
            Tag::Template => {
                if let Some(index) = self
                    .stack
                    .iter()
                    .rposition(|open| open.tag == Tag::Template)
                {
                    self.close_at(index);
                }
            }
            Tag::Form => {
                if let Some(form) = self.form_element.take() {
                    if let Some(index) = self.stack.iter().position(|open| open.id == form) {
                        self.remove_open_element(index);
                    }
                } else if self.template_modes.is_empty()
                    && self.fragment_context != Some(Tag::Template)
                {
                    return Ok(());
                } else {
                    self.close_open(Tag::Form);
                }
            }
            Tag::Other => {
                self.close_other(&name);
            }
            _ if tag.is_formatting() => self.end_formatting(tag)?,
            _ => {
                self.close_open(tag);
            }
        }
        Ok(())
    }

    fn start_tag(&mut self) -> Result<(), ParseError> {
        let start = self.pos;
        self.pos += 1;
        let name = self.name()?;
        let tag = Tag::classify(&name);
        let input = self.input;
        let bytes = input.as_bytes();
        self.scratch.clear();
        let mut self_closing = false;
        loop {
            self.skip_space();
            match bytes.get(self.pos) {
                None => {
                    // EOF in any start-tag state emits EOF and drops the
                    // unfinished token; it must not be emitted as a tag.
                    self.pos = input.len();
                    return Ok(());
                }
                Some(b'/') if bytes.get(self.pos + 1) == Some(&b'>') => {
                    self.pos += 2;
                    self_closing = true;
                    break;
                }
                Some(b'>') => {
                    self.pos += 1;
                    break;
                }
                Some(b'/') => {
                    self.pos += 1;
                    continue;
                }
                Some(_) => {}
            }
            let attr = self.attribute_name()?;
            self.skip_space();
            let value = if bytes.get(self.pos) == Some(&b'=') {
                self.pos += 1;
                self.skip_space();
                self.attribute_value()
            } else {
                Cow::Borrowed("")
            };
            if !self.scratch.iter().any(|(key, _)| key == &*attr) {
                self.scratch.push((Name::new(&attr), value.into_owned()));
            }
        }
        let attributes: Vec<(Name, String)> = self.scratch.drain(..).collect();
        if self.conformance_tokens.is_some() {
            self.emit_conformance_token(ConformanceToken::StartTag {
                name: name.to_string(),
                attributes: attributes
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.clone()))
                    .collect(),
                self_closing,
            });
        }
        if self.in_head && self.parent_tag() == Tag::Noscript {
            match tag {
                Tag::Base | Tag::Link | Tag::Meta | Tag::Style => {}
                Tag::Head | Tag::Noscript => return Ok(()),
                Tag::Html => {
                    self.merge_scaffold_attributes(self.html.unwrap_or(self.body), attributes)?;
                    return Ok(());
                }
                _ => {
                    self.close_open(Tag::Noscript);
                    self.in_head = false;
                }
            }
        }
        let scaffold = self.html.is_some();
        if tag != Tag::Template {
            if let Some(mode) = self.template_modes.last_mut() {
                if *mode == TemplateMode::InTemplate {
                    *mode = match tag {
                        Tag::Base
                        | Tag::Link
                        | Tag::Meta
                        | Tag::Script
                        | Tag::Style
                        | Tag::Title => TemplateMode::InTemplate,
                        Tag::Caption | Tag::Colgroup | Tag::Tbody | Tag::Tfoot | Tag::Thead => {
                            TemplateMode::InTable
                        }
                        Tag::Col => TemplateMode::InColumnGroup,
                        Tag::Tr => TemplateMode::InTableBody,
                        Tag::Td | Tag::Th => TemplateMode::InRow,
                        _ => TemplateMode::InBody,
                    };
                }
            }
        }
        if let Some(context) = self.fragment_context {
            if matches!(tag, Tag::Html | Tag::Head | Tag::Body) && context != Tag::Html {
                return Ok(());
            }
            if context == Tag::Table
                && tag == Tag::Table
                && self.fragment_context_mode_active(&[Tag::Table])
            {
                return Ok(());
            }
            if context == Tag::Html && matches!(tag, Tag::Html | Tag::Head | Tag::Body) {
                let target = match tag {
                    Tag::Head => self.head,
                    Tag::Body => Some(self.body),
                    _ => None,
                };
                if let Some(target) = target {
                    self.merge_scaffold_attributes(target, attributes)?;
                }
                return Ok(());
            }
            if self.fragment_context_mode_active(&[Tag::Caption])
                && matches!(
                    tag,
                    Tag::Caption
                        | Tag::Col
                        | Tag::Colgroup
                        | Tag::Tbody
                        | Tag::Tfoot
                        | Tag::Thead
                        | Tag::Tr
                        | Tag::Td
                        | Tag::Th
                )
            {
                return Ok(());
            }
            if self.fragment_context_mode_active(&[Tag::Colgroup])
                && !matches!(tag, Tag::Col | Tag::Template | Tag::Style | Tag::Script)
            {
                return Ok(());
            }
            if self.fragment_context_mode_active(&[Tag::Tbody, Tag::Tfoot, Tag::Thead])
                && matches!(
                    tag,
                    Tag::Caption | Tag::Col | Tag::Colgroup | Tag::Tbody | Tag::Tfoot | Tag::Thead
                )
            {
                return Ok(());
            }
            if self.fragment_context_mode_active(&[Tag::Tr])
                && matches!(
                    tag,
                    Tag::Caption
                        | Tag::Col
                        | Tag::Colgroup
                        | Tag::Tbody
                        | Tag::Tfoot
                        | Tag::Thead
                        | Tag::Tr
                )
            {
                return Ok(());
            }
        }
        if scaffold {
            self.html_started = true;
            self.doctype_allowed = false;
        }
        let document_root = scaffold && self.stack.is_empty() && self.template_modes.is_empty();
        if document_root {
            match tag {
                Tag::Html => {
                    self.merge_scaffold_attributes(self.html.unwrap(), attributes)?;
                    self.ensure_scaffold(self.html.unwrap())?;
                    return Ok(());
                }
                Tag::Head => {
                    if self.body_started || self.after_head {
                        return Ok(());
                    }
                    self.merge_scaffold_attributes(self.head.unwrap(), attributes)?;
                    self.ensure_scaffold(self.head.unwrap())?;
                    self.in_head = true;
                    self.after_head = false;
                    return Ok(());
                }
                Tag::Body => {
                    self.merge_scaffold_attributes(self.body, attributes)?;
                    self.ensure_scaffold(self.body)?;
                    self.in_head = false;
                    self.body_started = true;
                    self.after_head = false;
                    self.truncate_open_elements(0);
                    self.document_tail = 0;
                    return Ok(());
                }
                _ => {}
            }
        } else if scaffold && matches!(tag, Tag::Html | Tag::Head | Tag::Body) {
            return Ok(());
        }
        self.document_tail = 0;
        if scaffold && self.stack.is_empty() && !self.body_started && self.template_modes.is_empty()
        {
            if matches!(
                tag,
                Tag::Base
                    | Tag::Link
                    | Tag::Meta
                    | Tag::Style
                    | Tag::Script
                    | Tag::Title
                    | Tag::Noscript
                    | Tag::Template
            ) {
                self.in_head = true;
            } else {
                self.in_head = false;
                self.body_started = true;
                self.after_head = false;
            }
        }
        let current_namespace = self.adjusted_current_namespace();
        if current_namespace != Namespace::Html {
            let math_text_integration = self.current_mathml_text_integration_point();
            let integration_point = self.current_foreign_integration_point();
            let math_glyph = matches!(name.as_ref(), "mglyph" | "malignmark");
            if !integration_point || (math_text_integration && math_glyph) {
                if Self::foreign_breakout_tag(&name, &attributes) {
                    self.pop_foreign_for_html();
                } else {
                    return self.foreign_start_tag(
                        &name,
                        attributes,
                        self_closing,
                        current_namespace,
                        start,
                    );
                }
            }
        }
        if name == "svg" || name == "math" {
            self.reconstruct_formatting()?;
            let namespace = if name == "svg" {
                Namespace::Svg
            } else {
                Namespace::MathMl
            };
            return self.foreign_start_tag(&name, attributes, self_closing, namespace, start);
        }
        if self.template_modes.last() == Some(&TemplateMode::InColumnGroup)
            && !matches!(tag, Tag::Col | Tag::Template | Tag::Style | Tag::Script)
        {
            return Ok(());
        }
        if self.template_modes.last() == Some(&TemplateMode::InBody)
            && tag.is_table_part()
            && self.table_insertion_mode().is_none()
        {
            return Ok(());
        }
        let template_scope_start = self
            .stack
            .iter()
            .rposition(|open| open.tag == Tag::Template)
            .map_or(0, |index| index + 1);
        let has_template_table_section = self.stack[template_scope_start..]
            .iter()
            .any(|open| matches!(open.tag, Tag::Tbody | Tag::Tfoot | Tag::Thead));
        let has_template_row = self.stack[template_scope_start..]
            .iter()
            .any(|open| open.tag == Tag::Tr);
        if self.template_modes.last() == Some(&TemplateMode::InTableBody)
            && !has_template_table_section
            && matches!(
                tag,
                Tag::Caption | Tag::Col | Tag::Colgroup | Tag::Tbody | Tag::Tfoot | Tag::Thead
            )
        {
            return Ok(());
        }
        if self.template_modes.last() == Some(&TemplateMode::InRow)
            && !has_template_row
            && matches!(
                tag,
                Tag::Caption
                    | Tag::Col
                    | Tag::Colgroup
                    | Tag::Tbody
                    | Tag::Tfoot
                    | Tag::Thead
                    | Tag::Tr
            )
        {
            return Ok(());
        }
        if !matches!(tag, Tag::Col | Tag::Template) && self.parent_tag() == Tag::Colgroup {
            self.close_open(Tag::Colgroup);
        }
        if (matches!(tag, Tag::Caption | Tag::Colgroup)
            || (tag == Tag::Col && self.table_insertion_mode() != Some(Tag::Colgroup)))
            && (self.stack.iter().any(|open| {
                matches!(
                    open.tag,
                    Tag::Table
                        | Tag::Caption
                        | Tag::Colgroup
                        | Tag::Tbody
                        | Tag::Thead
                        | Tag::Tfoot
                        | Tag::Tr
                        | Tag::Td
                        | Tag::Th
                        | Tag::Template
                )
            }) || self.in_fragment_context(&[
                Tag::Table,
                Tag::Tbody,
                Tag::Tfoot,
                Tag::Thead,
                Tag::Tr,
            ]))
        {
            self.clear_to_table_context(&[Tag::Table]);
        }
        if tag.is_table_part() && self.parent_tag() == Tag::Caption {
            self.close_open(Tag::Caption);
        }
        if tag.is_table_part()
            && !self.stack.iter().any(|open| {
                matches!(
                    open.tag,
                    Tag::Table
                        | Tag::Caption
                        | Tag::Colgroup
                        | Tag::Tbody
                        | Tag::Thead
                        | Tag::Tfoot
                        | Tag::Tr
                        | Tag::Template
                )
            })
            && !self.in_fragment_context(&[
                Tag::Table,
                Tag::Tbody,
                Tag::Tfoot,
                Tag::Thead,
                Tag::Tr,
                Tag::Colgroup,
            ])
            && self.fragment_context != Some(Tag::Template)
        {
            return Ok(());
        }
        if let Some(select) = self.active_select() {
            let select_index = self
                .stack
                .iter()
                .rposition(|open| open.id == select)
                .unwrap_or(0);
            let template = self
                .stack
                .iter()
                .rposition(|open| open.tag == Tag::Template);
            let table_in_scope = self.stack.iter().enumerate().any(|(index, open)| {
                open.tag == Tag::Table
                    && index < select_index
                    && template.map_or(true, |boundary| index >= boundary)
            });
            if table_in_scope && (tag == Tag::Table || tag.is_table_part()) {
                self.close_open(Tag::Select);
            }
        }
        if self.fragment_context == Some(Tag::Select) && tag == Tag::Select {
            return Ok(());
        }
        if let Some(select) = self.active_select() {
            let starts_custom_content =
                !self.select_is_custom(select) && Self::custom_select_content_tag(tag);
            if starts_custom_content {
                self.custom_selects.push(select);
            }
            if starts_custom_content && tag == Tag::Hr {
                self.close_in_scope(&[Tag::Option], &[Tag::Select]);
                self.close_in_scope(&[Tag::Optgroup], &[Tag::Select]);
            }
            match tag {
                Tag::Option => {
                    self.close_in_scope(&[Tag::Option], &[Tag::Select, Tag::Optgroup]);
                }
                Tag::Optgroup => {
                    self.close_in_scope(&[Tag::Option], &[Tag::Select]);
                    self.close_in_scope(&[Tag::Optgroup], &[Tag::Select]);
                }
                Tag::Select => {
                    self.close_open(Tag::Select);
                    return Ok(());
                }
                Tag::Script | Tag::Template => {}
                _ => {}
            }
        }
        if self.active_select().is_none() {
            if tag == Tag::Option && self.parent_tag() == Tag::Option {
                self.close_open(Tag::Option);
            } else if tag == Tag::Optgroup && self.parent_tag() == Tag::Option {
                self.close_open(Tag::Option);
            }
        }
        if tag == Tag::Table && self.parent_tag().is_table_context() {
            self.close_open(Tag::Table);
        }
        if tag == Tag::Form {
            let in_template =
                !self.template_modes.is_empty() || self.fragment_context == Some(Tag::Template);
            if self.form_element.is_some() && !in_template {
                return Ok(());
            }
        }
        if matches!(
            tag,
            Tag::H1 | Tag::H2 | Tag::H3 | Tag::H4 | Tag::H5 | Tag::H6
        ) {
            if self.stack.last().is_some_and(|open| {
                matches!(
                    open.tag,
                    Tag::H1 | Tag::H2 | Tag::H3 | Tag::H4 | Tag::H5 | Tag::H6
                )
            }) {
                self.pop_open_element();
            }
        }
        if tag.closes_paragraph() {
            self.close_in_scope(
                &[Tag::P],
                &[
                    Tag::Button,
                    Tag::Caption,
                    Tag::Html,
                    Tag::Object,
                    Tag::Table,
                    Tag::Td,
                    Tag::Th,
                    Tag::Template,
                ],
            );
        }
        match tag {
            Tag::Button => self.close_in_scope(
                &[Tag::Button],
                &[
                    Tag::Caption,
                    Tag::Html,
                    Tag::Object,
                    Tag::Table,
                    Tag::Td,
                    Tag::Th,
                    Tag::Template,
                ],
            ),
            Tag::Rb | Tag::Rtc | Tag::Rp | Tag::Rt if self.in_scope(Tag::Ruby) => {
                self.implied_end_tags(if matches!(tag, Tag::Rp | Tag::Rt) {
                    Some(Tag::Rtc)
                } else {
                    None
                });
            }
            Tag::Li => self.close_in_scope(&[Tag::Li], &[Tag::Ol, Tag::Ul]),
            Tag::Dt | Tag::Dd => self.close_in_scope(&[Tag::Dt, Tag::Dd], &[Tag::Dl]),
            Tag::Tbody | Tag::Thead | Tag::Tfoot => {
                self.close_in_scope(&[Tag::Td, Tag::Th], &[Tag::Table]);
                self.close_in_scope(&[Tag::Tr], &[Tag::Table]);
                self.close_in_scope(&[Tag::Tbody, Tag::Thead, Tag::Tfoot], &[Tag::Table]);
                self.clear_to_table_context(&[Tag::Table]);
            }
            Tag::Tr => {
                self.close_in_scope(&[Tag::Td, Tag::Th], &[Tag::Tr, Tag::Table]);
                self.close_in_scope(&[Tag::Tr], &[Tag::Table]);
                self.clear_to_table_context(&[Tag::Table, Tag::Tbody, Tag::Thead, Tag::Tfoot]);
                self.imply_tbody(start)?;
            }
            Tag::Td | Tag::Th => {
                self.close_in_scope(&[Tag::Td, Tag::Th], &[Tag::Tr, Tag::Table]);
                self.clear_to_table_context(&[
                    Tag::Table,
                    Tag::Tbody,
                    Tag::Thead,
                    Tag::Tfoot,
                    Tag::Tr,
                ]);
                self.imply_tbody(start)?;
                let parent = self.parent();
                if matches!(self.parent_tag(), Tag::Tbody | Tag::Thead | Tag::Tfoot)
                    || (self.parent_tag() == Tag::Template
                        && self.template_modes.last() == Some(&TemplateMode::InTableBody))
                {
                    let row = self.create_plain("tr", start)?;
                    self.append(parent, row)?;
                    self.stack.push(Open {
                        id: row,
                        tag: Tag::Tr,
                    });
                }
            }
            Tag::Col => {
                let parent = self.parent();
                if self.parent_tag() == Tag::Table {
                    let group = self.create_plain("colgroup", start)?;
                    self.append(parent, group)?;
                    self.stack.push(Open {
                        id: group,
                        tag: Tag::Colgroup,
                    });
                }
            }
            Tag::A => self.start_anchor()?,
            _ => {}
        }
        let formatting = tag.is_formatting();
        if formatting || !tag.is_special() || matches!(tag, Tag::Img | Tag::Br | Tag::Input) {
            self.reconstruct_formatting()?;
        }
        let marker = tag.is_marker();
        let void = tag.is_void();
        let skip_newline = matches!(tag, Tag::Pre | Tag::Listing | Tag::Textarea);
        let allowed_in_table = matches!(
            tag,
            Tag::Caption
                | Tag::Colgroup
                | Tag::Col
                | Tag::Tbody
                | Tag::Thead
                | Tag::Tfoot
                | Tag::Tr
                | Tag::Td
                | Tag::Th
                | Tag::Style
                | Tag::Script
                | Tag::Template
        ) || (tag == Tag::Input
            && attributes
                .iter()
                .any(|(key, value)| key == "type" && value.eq_ignore_ascii_case("hidden")));
        let form_in_table = tag == Tag::Form && self.table_insertion_mode() == Some(Tag::Table);
        let (parent, before) = if allowed_in_table || form_in_table {
            (self.content_of(self.parent(), self.parent_tag()), None)
        } else {
            self.insertion_location(self.parent(), self.parent_tag())
        };
        let element_name = if tag == Tag::Img && name.as_ref() == "image" {
            "img"
        } else {
            name.as_ref()
        };
        let declarative = if tag == Tag::Template && self.allow_declarative_shadow_roots {
            attributes
                .iter()
                .find(|(name, _)| name == "shadowrootmode")
                .and_then(|(_, value)| {
                    let mode = if value.eq_ignore_ascii_case("open") {
                        crate::shadow::ShadowMode::Open
                    } else if value.eq_ignore_ascii_case("closed") {
                        crate::shadow::ShadowMode::Closed
                    } else {
                        return None;
                    };
                    let has = |expected: &str| attributes.iter().any(|(name, _)| name == expected);
                    let slot_assignment = attributes
                        .iter()
                        .find(|(name, _)| name == "shadowrootslotassignment")
                        .map_or(crate::shadow::SlotAssignmentMode::Named, |(_, value)| {
                            if value.eq_ignore_ascii_case("manual") {
                                crate::shadow::SlotAssignmentMode::Manual
                            } else {
                                crate::shadow::SlotAssignmentMode::Named
                            }
                        });
                    Some(crate::shadow::ShadowOptions {
                        mode,
                        slot_assignment,
                        delegates_focus: has("shadowrootdelegatesfocus"),
                        clonable: has("shadowrootclonable"),
                        serializable: has("shadowrootserializable"),
                        declarative: true,
                        keep_custom_element_registry_null: has("shadowrootcustomelementregistry"),
                        ..crate::shadow::ShadowOptions::new(mode)
                    })
                })
        } else {
            None
        };
        let is_value = attributes.iter().find(|(name, _)| name == "is").map(|(_, value)| value.as_str());
        let deferred = self.stop_after_script && self.document.parser_custom_element_defined(parent, element_name, is_value);
        let id = if deferred {
            let id = self.document.create_at(parent, NodeKind::Element {
                namespace: Namespace::Html, name: Name::new(element_name), attributes: Vec::new(),
            }, is_value).map_err(|failure| dom_error(start, failure))?;
            self.pending_element = Some(PendingElement { node: id, attributes, insertion: None, declarative: None, parser_form: None });
            id
        } else {
            self.document.create_unprefixed_at(parent, Namespace::Html, Name::new(element_name), attributes).map_err(|failure| dom_error(start, failure))?
        };
        if deferred {
            let owner=self.document.node_document(parent).map_err(|e|dom_error(start,e))?;
            self.document.set_node_document(id,owner).map_err(|e|dom_error(start,e))?;
        }
        let registry_context=self.registry_context(parent);
        self.document.record_parser_element_birth(id,registry_context,self.html.is_some()).map_err(|e| dom_error(start,e))?;
        let shadow = if let Some(options) = declarative {
            let host = if self.at_fragment_context() {
                self.fragment_context_node.unwrap_or_else(|| self.parent())
            } else { self.parent() };
            if self.html == Some(host) {
                None
            } else if deferred {
                self.pending_element.as_mut().unwrap().declarative = Some((host, options));
                None
            } else {
                match self.document.attach_shadow_with_options(host, options) {
                    Ok(root) => Some(root),
                    Err(DomError::WrongKind | DomError::Hierarchy) => None,
                    Err(error) => return Err(dom_error(start, error)),
                }
            }
        } else {
            None
        };
        if matches!(element_name, "button" | "fieldset" | "input" | "object" | "output" | "select" | "textarea")
            && self.template_modes.is_empty() && self.fragment_context.is_none()
            && self.document.get_attribute_ns_ref(id, None, "form").map_err(|e| dom_error(start, e))?.is_none()
        {
            if let Some(form) = self.form_element {
                if self.document.root_node(parent, false).map_err(|e| dom_error(start, e))?
                    == self.document.root_node(form, false).map_err(|e| dom_error(start, e))? {
                    let mut ancestor = Some(parent);
                    let mut normal_owner = false;
                    while let Some(node) = ancestor {
                        if node == form { normal_owner = true; break; }
                        ancestor = self.document.parent(node).map_err(|e| dom_error(start, e))?;
                    }
                    if !normal_owner {
                        if deferred { self.pending_element.as_mut().unwrap().parser_form = Some(form); }
                        else { self.document.associate_parser_form(id, form).map_err(|e| dom_error(start, e))?; }
                    }
                }
            }
        }
        if let Some(root) = shadow {
            self.declarative_roots
                .try_reserve(1)
                .map_err(|_| error(start, "parser allocation limit"))?;
            self.declarative_roots.push((id, root));
        } else {
            self.insert_token_element(parent, id, before)?;
        }
        if tag == Tag::Form
            && self.template_modes.is_empty()
            && self.fragment_context != Some(Tag::Template)
        {
            self.form_element = Some(id);
        }
        let open = Open { id, tag };
        if !void && !(form_in_table) {
            self.stack.push(open);
        }
        if formatting {
            let marker = self
                .formatting
                .iter()
                .rposition(Option::is_none)
                .map_or(0, |index| index + 1);
            let kind = self.document.kind(id).map_err(|e| dom_error(start, e))?;
            let mut duplicates =
                self.formatting[marker..]
                    .iter()
                    .enumerate()
                    .filter_map(|(index, &entry)| {
                        entry
                            .filter(|entry| {
                                entry.tag == tag && self.document.kind(entry.id).ok() == Some(kind)
                            })
                            .map(|_| marker + index)
                    });
            if let Some(first) = duplicates.next() {
                if duplicates.count() >= 2 {
                    self.formatting.remove(first);
                }
            }
            self.formatting.push(Some(open));
        }
        if marker {
            self.formatting.push(None);
        }
        if tag == Tag::Template {
            self.template_modes.push(TemplateMode::InTemplate);
        }
        if skip_newline && self.conformance_tokens.is_none() {
            if let Some(consumed) = leading_lf_character_token(self.remaining()) {
                self.pos += consumed;
            }
        }
        Ok(())
    }

    fn imply_tbody(&mut self, offset: usize) -> Result<(), ParseError> {
        if self.parent_tag() == Tag::Table
            || (self.parent_tag() == Tag::Template
                && self.template_modes.last() == Some(&TemplateMode::InTable))
        {
            let parent = self.parent();
            let tbody = self.create_plain("tbody", offset)?;
            self.append(parent, tbody)?;
            self.stack.push(Open {
                id: tbody,
                tag: Tag::Tbody,
            });
        }
        Ok(())
    }

    fn text(&mut self) -> Result<(), ParseError> {
        let input = self.input;
        let start = self.pos;
        if input.as_bytes()[start] == b'<' {
            self.pos += 1;
        }
        self.pos += input[self.pos..]
            .find('<')
            .unwrap_or(input.len() - self.pos);
        let source = &input[start..self.pos];
        if self.conformance_tokens.is_some() {
            self.emit_conformance_token(ConformanceToken::Character(
                decode_entities(source, false).into_owned(),
            ));
        }
        // In the data state, the tokenizer emits U+0000 as a character token.
        // HTML tree construction ignores that token in the in-body and in-table
        // text modes, while foreign-content parsing replaces it with U+FFFD.
        // RCDATA, RAWTEXT, and PLAINTEXT are handled by their separate paths
        // above, where the tokenizer itself performs replacement.
        let foreign_content = self.adjusted_current_namespace() != Namespace::Html
            && !self.current_foreign_integration_point()
            && !self.current_mathml_text_integration_point();
        let raw = if foreign_content {
            replace_nulls(source)
        } else {
            remove_nulls(source)
        };
        let raw = raw.as_ref();
        let whitespace = raw.bytes().all(|byte| byte.is_ascii_whitespace());
        if self.template_modes.last() == Some(&TemplateMode::InColumnGroup) && !whitespace {
            return Ok(());
        }
        if self.in_head && self.parent_tag() == Tag::Noscript && !whitespace {
            self.close_open(Tag::Noscript);
            self.in_head = false;
        }
        if !whitespace {
            self.document_tail = 0;
        }
        let mut content = raw;
        if self.html.is_some() && !self.body_started && self.stack.is_empty() {
            if self.in_head {
                if whitespace {
                    return self.append_text_at(
                        decode_entities(content, false),
                        self.head.unwrap_or(self.body),
                        None,
                        start,
                    );
                }
                let split = content.bytes().take_while(u8::is_ascii_whitespace).count();
                if split != 0 {
                    self.append_text_at(
                        decode_entities(&content[..split], false),
                        self.head.unwrap_or(self.body),
                        None,
                        start,
                    )?;
                    content = &content[split..];
                }
            } else if self.after_head {
                if whitespace {
                    return self.append_text_at(
                        decode_entities(content, false),
                        self.html.unwrap_or(self.body),
                        (self.scaffold_attached & 4 != 0).then_some(self.body),
                        start,
                    );
                }
                let split = content.bytes().take_while(u8::is_ascii_whitespace).count();
                if split != 0 {
                    self.append_text_at(
                        decode_entities(&content[..split], false),
                        self.html.unwrap_or(self.body),
                        (self.scaffold_attached & 4 != 0).then_some(self.body),
                        start,
                    )?;
                    content = &content[split..];
                }
            } else {
                if whitespace {
                    return Ok(());
                }
                let split = content.bytes().take_while(u8::is_ascii_whitespace).count();
                content = &content[split..];
            }
            self.in_head = false;
            self.body_started = true;
            self.after_head = false;
            self.html_started = true;
            self.doctype_allowed = false;
        }
        let content_whitespace = content.bytes().all(|byte| byte.is_ascii_whitespace());
        if !content_whitespace || !self.parent_tag().is_table_context() {
            self.reconstruct_formatting()?;
        }
        self.append_text(decode_entities(content, false), start)
    }

    fn name(&mut self) -> Result<Cow<'a, str>, ParseError> {
        let input = self.input;
        let start = self.pos;
        let len = input.as_bytes()[start..]
            .iter()
            .position(|&byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
            .unwrap_or(input.len() - start);
        if len == 0 {
            return Err(error(start, "expected name"));
        }
        self.pos = start + len;
        Ok(Cow::Owned(
            replace_nulls(&input[start..self.pos]).to_ascii_lowercase(),
        ))
    }

    fn skip_space(&mut self) {
        while self
            .input
            .as_bytes()
            .get(self.pos)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.pos += 1;
        }
    }

    fn attribute_name(&mut self) -> Result<Cow<'a, str>, ParseError> {
        let input = self.input;
        let bytes = input.as_bytes();
        let start = self.pos;
        let mut end = start + usize::from(bytes.get(start) == Some(&b'='));
        while let Some(&byte) = bytes.get(end) {
            if byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>' | b'=') {
                break;
            }
            end += 1;
        }
        if end == start {
            return Err(error(start, "expected attribute name"));
        }
        self.pos = end;
        Ok(Cow::Owned(
            replace_nulls(&input[start..end]).to_ascii_lowercase(),
        ))
    }

    fn attribute_value(&mut self) -> Cow<'a, str> {
        let input = self.input;
        let bytes = input.as_bytes();
        let start = self.pos;
        if let Some(&quote @ (b'\'' | b'"')) = bytes.get(start) {
            let begin = start + 1;
            let end = bytes[begin..]
                .iter()
                .position(|&byte| byte == quote)
                .map_or(bytes.len(), |offset| begin + offset);
            self.pos = (end + 1).min(bytes.len());
            let raw = replace_nulls(&input[begin..end]);
            Cow::Owned(decode_entities(raw.as_ref(), true).into_owned())
        } else {
            let end = bytes[start..]
                .iter()
                .position(|&byte| byte.is_ascii_whitespace() || byte == b'>')
                .map_or(bytes.len(), |offset| start + offset);
            self.pos = end;
            let raw = replace_nulls(&input[start..end]);
            Cow::Owned(decode_entities(raw.as_ref(), true).into_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::borrow::ToOwned;
    #[test]
    fn specification_custom_element_names_use_shared_local_name_validation() {
        for name in ["a-a×","a-a\u{3000}","a-a\u{f0000}","a-:","a-="] {
            assert!(super::is_valid_custom_element_name(name),"valid local custom name: {name}");
            assert!(crate::xml::is_valid_element_local_name(name));
        }
        for name in ["A-a","a-A","a-a\0","a-a\t","a-a/","a-a>","annotation-xml","plain"] {
            assert!(!super::is_valid_custom_element_name(name),"invalid custom name: {name}");
        }
    }
    #[test]
    fn specification_parser_element_birth_precedes_connection_and_uses_actual_context() {
        use alloc::rc::Rc;
        use core::cell::RefCell;
        let births=Rc::new(RefCell::new(Vec::new()));
        let capture=births.clone();
        let mut document=parse_with_options_initialized(
            "<div id=host><template shadowrootmode=open shadowrootcustomelementregistry><span></span><x-undefined></x-undefined></template></div>",
            64,ParseOptions {allow_declarative_shadow_roots:true,..ParseOptions::default()},
            move |document|document.set_parser_element_birth_sink(Some(Rc::new(move |document,node,context,_document_parser| {
                assert_eq!(document.parent(node),Ok(None),"birth must precede connection");
                capture.borrow_mut().push((node,context));Ok(())
            }))),
        ).unwrap();
        let host=crate::selector::query_selector(&document,document.root(),"#host").unwrap().unwrap();
        let shadow=document.shadow_root(host).unwrap().unwrap();
        let span=crate::selector::query_selector(&document,shadow,"span").unwrap().unwrap();
        let custom=crate::selector::query_selector(&document,shadow,"x-undefined").unwrap().unwrap();
        assert!(births.borrow().contains(&(span,shadow)));
        assert!(births.borrow().contains(&(custom,shadow)));
        let html=crate::selector::query_selector(&document,document.root(),"html").unwrap().unwrap();
        assert!(births.borrow().contains(&(html,document.root())));
        let fragment=parse_fragment_for_target(&mut document,host,shadow,"<i><x-fragment></x-fragment></i>",false).unwrap();
        let italic=document.first_child(fragment).unwrap().unwrap();
        let fragment_custom=document.first_child(italic).unwrap().unwrap();
        assert!(births.borrow().contains(&(italic,shadow)),"shadow target differs from tokenizer host");
        assert!(births.borrow().contains(&(fragment_custom,italic)),"descendants use their newly born parent");
    }
    #[test]
    fn specification_parser_adjusted_insertion_guards() {
        let mut document = parse("<main></main>", 64).unwrap();
        let main = crate::selector::query_selector(&document, document.root(), "main").unwrap().unwrap();
        let element = create_html_element(&mut document, "aside", Vec::new()).unwrap();
        let root = document.root();
        assert!(!insert_at_adjusted_location(&mut document, root, element, None).unwrap());
        assert_eq!(document.parent(element), Ok(None));
        document.append(main, element).unwrap();
        let template = create_html_element(&mut document, "template", Vec::new()).unwrap();
        let content = document.template_content(template).unwrap().unwrap();
        assert!(!insert_at_adjusted_location(&mut document, content, element, None).unwrap());
        assert_eq!(document.parent(element), Ok(Some(main)));
        assert!(!insert_at_adjusted_location(&mut document, content, template, None).unwrap());
        let host = create_html_element(&mut document, "div", Vec::new()).unwrap();
        let shadow = document.attach_shadow(host, crate::ShadowMode::Open).unwrap();
        let intended = create_html_element(&mut document, "section", Vec::new()).unwrap();
        document.append(shadow, intended).unwrap();
        assert!(!insert_at_adjusted_location(&mut document, intended, host, None).unwrap());
        assert!(insert_at_adjusted_location(&mut document, main, host, None).unwrap());
        assert_eq!(document.parent(host), Ok(Some(main)));
    }
    #[test]
    fn specification_live_html_parser_element_creation_phases() {
        let mut document = Document::new(64);
        document.set_parser_custom_element_predicate(Some(alloc::rc::Rc::new(|_, _, local, _| local == "x-phase")));
        let (mut parser, _) = HtmlDocumentParser::open(&mut document, ParseOptions::default()).unwrap();
        assert!(parser.write_final_until_script(&mut document, "<x-phase data-value=token><span>child</span></x-phase><script>after</script>").unwrap().is_none());
        let node = parser.pending_element().expect("constructor phase");
        assert_eq!(document.parent(node), Ok(None));
        assert_eq!(document.first_child(node), Ok(None));
        assert_eq!(document.get_attribute_ns_ref(node, None, "data-value"), Ok(None));
        parser.append_pending_element_attributes(&mut document).unwrap();
        assert_eq!(document.get_attribute_ns_ref(node, None, "data-value").unwrap(), Some("token"));
        assert_eq!(document.parent(node), Ok(None));
        assert!(parser.finish_pending_element(&mut document).unwrap().is_some());
        assert!(document.parent(node).unwrap().is_some());
        assert!(document.first_child(node).unwrap().is_some());
        assert!(parser.resume_until_script(&mut document).unwrap().is_none());
    }
    #[test]
    fn initialized_document_parser_observes_initial_details_transitions() {
        use alloc::rc::Rc;
        use core::cell::RefCell;
        let events = Rc::new(RefCell::new(Vec::new()));
        let capture = events.clone();
        let mut calls = 0;
        let document = parse_with_options_initialized(
            "<details name=g open></details><details name=g open></details>",
            32,
            ParseOptions::default(),
            |document| {
                calls += 1;
                assert!(document.first_child(document.root()).unwrap().is_none());
                document.set_details_transition_sink(Some(Rc::new(move |_, event| {
                    capture.borrow_mut().push(event);
                })));
            },
        ).unwrap();
        assert_eq!(calls, 1);
        let nodes = crate::selector::query_selector_all(&document, document.root(), "details").unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(*events.borrow(), vec![
            crate::details::DetailsTransition { node: nodes[0], old_open: false, new_open: true },
            crate::details::DetailsTransition { node: nodes[1], old_open: false, new_open: true },
            crate::details::DetailsTransition { node: nodes[1], old_open: true, new_open: false },
        ]);
        assert_eq!(document.details_open_state(nodes[1]), Some(false));
    }

    #[test]
    fn initialized_document_parser_preserves_limits_and_errors() {
        for (input, max_nodes) in [("<details open>", 1), ("<p>text", 2)] {
            let mut calls = 0;
            let actual = parse_with_options_initialized(input, max_nodes, ParseOptions::default(), |_| calls += 1).err().unwrap();
            assert_eq!(calls, 1);
            assert_eq!(actual, parse(input, max_nodes).err().unwrap());
        }
    }
    #[test]
    fn html_element_name_classification_distinguishes_builtins_custom_and_unknown() {
        assert_eq!(
            classify_html_element_name("button"),
            HtmlElementNameKind::BuiltIn
        );
        assert_eq!(
            classify_html_element_name("span"),
            HtmlElementNameKind::BuiltIn
        );
        assert_eq!(
            classify_html_element_name("x-widget"),
            HtmlElementNameKind::Custom
        );
        assert_eq!(
            classify_html_element_name("unknown"),
            HtmlElementNameKind::Unknown
        );
        assert_eq!(
            classify_html_element_name("annotation-xml"),
            HtmlElementNameKind::Unknown
        );
        for legacy_unknown in [
            "applet", "bgsound", "blink", "isindex", "keygen", "multicol", "nextid", "spacer",
        ] {
            assert_eq!(
                classify_html_element_name(legacy_unknown),
                HtmlElementNameKind::Unknown
            );
        }
    }

    #[test]
    fn template_body_mode_does_not_override_a_nested_table() {
        let document = parse(
            "<template><table>before<tr><td>cell</table>after</template>",
            32,
        )
        .unwrap();
        let template = crate::selector::query_selector(&document, document.root(), "template")
            .unwrap()
            .unwrap();
        assert_eq!(
            inner_html(&document, template).unwrap(),
            "before<table><tbody><tr><td>cell</td></tr></tbody></table>after"
        );
    }

    #[test]
    fn custom_element_birth_values_are_sparse_immutable_shared_and_serialized() {
        let mut document = Document::new(128);
        let ordinary = document.create(NodeKind::Element {
            namespace: Namespace::Html, name: "div".into(), attributes: Vec::new(),
        }).unwrap();
        assert_eq!(document.custom_element_is_values.capacity(), 0);
        document.set_attribute(ordinary, "is", "late-value").unwrap();
        assert_eq!(document.custom_element_is_value(ordinary).unwrap(), None);
        let custom = document.create_with_is_value(NodeKind::Element {
            namespace: Namespace::Html, name: "p".into(), attributes: vec![("class".into(), "first".into())],
        }, Some("birth-value")).unwrap();
        assert_eq!(document.get_attribute_ns_ref(custom, None, "is").unwrap(), None);
        assert_eq!(outer_html(&document, custom).unwrap(), "<p is=\"birth-value\" class=\"first\"></p>");
        document.set_attribute(custom, "is", "other\"&\n").unwrap();
        assert_eq!(outer_html(&document, custom).unwrap(), "<p class=\"first\" is=\"other&quot;&amp;\n\"></p>");
        assert_eq!(document.custom_element_is_value(custom).unwrap(), Some("birth-value"));
        let clone = document.clone_node(custom, true).unwrap();
        assert_eq!(document.custom_element_is_value(clone).unwrap(), Some("birth-value"));
        assert_eq!(document.custom_element_is_value(clone).unwrap().unwrap().as_ptr(),
            document.custom_element_is_value(custom).unwrap().unwrap().as_ptr());
        document.remove_attribute(custom, "is").unwrap();
        assert_eq!(outer_html(&document, custom).unwrap(), "<p is=\"birth-value\" class=\"first\"></p>");
        let mut target = Document::new(128);
        let imported = target.clone_subtree_from(&document, custom, true).unwrap();
        assert_eq!(target.custom_element_is_value(imported).unwrap(), Some("birth-value"));
        assert_eq!(target.custom_element_is_value(imported).unwrap().unwrap().as_ptr(),
            document.custom_element_is_value(custom).unwrap().unwrap().as_ptr());
        let mut full = Document::new(1);
        assert_eq!(full.adopt_subtree_from(&mut document, custom), Err(DomError::LimitExceeded));
        assert_eq!(document.custom_element_is_value(custom).unwrap(), Some("birth-value"));
        let (adopted, _) = target.adopt_subtree_from(&mut document, custom).unwrap();
        assert!(document.custom_element_is_value(custom).is_err());
        assert_eq!(target.custom_element_is_value(adopted).unwrap(), Some("birth-value"));
        target.destroy_subtree(adopted).unwrap();
        let fresh = target.create(NodeKind::Element { namespace: Namespace::Html, name: "p".into(), attributes: Vec::new() }).unwrap();
        assert_eq!(target.custom_element_is_value(fresh).unwrap(), None);
        let fragment = parse_fragment(&mut document, "<p is='parser-value'></p><template><p is='template-value'></p></template>").unwrap();
        let parsed = document.first_child(fragment).unwrap().unwrap();
        document.set_attribute(parsed, "is", "changed").unwrap();
        assert_eq!(document.custom_element_is_value(parsed).unwrap(), Some("parser-value"));
        let template = document.next_sibling(parsed).unwrap().unwrap();
        let template_clone = document.clone_node(template, true).unwrap();
        let child = document.first_child(document.template_content(template_clone).unwrap().unwrap()).unwrap().unwrap();
        assert_eq!(document.custom_element_is_value(child).unwrap(), Some("template-value"));
    }

    #[test]
    fn get_html_uses_html_namespace_names_and_text_rules_in_xml_documents() {
        let document = crate::xml::parse(
            "<h:div xmlns:h='http://www.w3.org/1999/xhtml'><h:style><![CDATA[<&>]]></h:style><h:p><![CDATA[<&>]]></h:p><h:br/></h:div>",
            32,
        ).unwrap();
        let root = document.first_child(document.root()).unwrap().unwrap();
        assert_eq!(get_html(&document, root, false, &[]).unwrap(), "<style><&></style><p>&lt;&amp;&gt;</p><br>");
        assert!(crate::xml::inner_html(&document, root).unwrap().contains("<![CDATA[<&>]]>"));
        let foreign = crate::xml::parse("<root><q:br xmlns:q='urn:custom' xmlns:l='http://www.w3.org/1999/xlink' l:href='a&amp;b' xml:lang='en'>text</q:br></root>", 16).unwrap();
        let root = foreign.first_child(foreign.root()).unwrap().unwrap();
        assert_eq!(get_html(&foreign, root, false, &[]).unwrap(), "<q:br xmlns:q=\"urn:custom\" xmlns:l=\"http://www.w3.org/1999/xlink\" xlink:href=\"a&amp;b\" xml:lang=\"en\">text</q:br>");
    }

    #[test]
    fn get_html_uses_actual_html_local_name_for_template_contents() {
        let mut document = Document::new(16);
        document.set_html_document(true);
        let literal = document
            .create_unprefixed_element(Namespace::Html, "x:template".into(), Vec::new())
            .unwrap();
        let literal_text = document
            .create(NodeKind::Text("literal child".into()))
            .unwrap();
        document.append(literal, literal_text).unwrap();

        let qualified = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: "x:template".into(),
                attributes: Vec::new(),
            })
            .unwrap();
        let content = document.template_content(qualified).unwrap().unwrap();
        let template_text = document
            .create(NodeKind::Text("template content".into()))
            .unwrap();
        document.append(content, template_text).unwrap();
        let light_text = document
            .create(NodeKind::Text("qualified light child".into()))
            .unwrap();
        document.append(qualified, light_text).unwrap();

        assert_eq!(document.element_name_parts(literal), Ok((None, "x:template")));
        assert_eq!(document.element_name_parts(qualified), Ok((Some("x"), "template")));
        assert_eq!(document.template_content(literal), Ok(None));
        assert_eq!(document.template_content(qualified), Ok(Some(content)));
        assert_eq!(get_html(&document, literal, false, &[]).unwrap(), "literal child");
        assert_eq!(
            get_html(&document, qualified, false, &[]).unwrap(),
            "template content"
        );
    }

    #[test]
    fn get_html_serializes_selected_shadow_roots_before_light_children_without_mutation() {
        let document = parse_with_declarative_shadow_roots(
            "<div id=host><b>light</b><template shadowrootmode=open shadowrootdelegatesfocus shadowrootserializable shadowrootclonable><section><template shadowrootmode=closed><i>inner&amp;</i></template><em>nested light</em></section></template></div>",
            64, true,
        ).unwrap();
        let host = crate::selector::query_selector(&document, document.root(), "#host").unwrap().unwrap();
        let root = document.shadow_root(host).unwrap().unwrap();
        let section = document.first_child(root).unwrap().unwrap();
        let nested = document.shadow_root(section).unwrap().unwrap();
        let count = document.node_count();
        let version = document.version();
        let outer_start = "<template shadowrootmode=\"open\" shadowrootdelegatesfocus=\"\" shadowrootserializable=\"\" shadowrootclonable=\"\">";
        let inner = "<template shadowrootmode=\"closed\"><i>inner&amp;</i></template>";
        assert_eq!(get_html(&document, host, false, &[]).unwrap(), "<b>light</b>");
        assert_eq!(get_html(&document, host, false, &[nested]).unwrap(), "<b>light</b>");
        assert_eq!(get_html(&document, host, true, &[]).unwrap(), alloc::format!("{outer_start}<section><em>nested light</em></section></template><b>light</b>"));
        assert_eq!(get_html(&document, host, true, &[nested]).unwrap(), alloc::format!("{outer_start}<section>{inner}<em>nested light</em></section></template><b>light</b>"));
        assert_eq!(get_html(&document, host, false, &[nested, root, root]).unwrap(), get_html(&document, host, true, &[nested]).unwrap());
        assert_eq!(get_html(&document, root, false, &[nested]).unwrap(), alloc::format!("<section>{inner}<em>nested light</em></section>"));
        assert_eq!(document.node_count(), count);
        assert_eq!(document.version(), version);
        assert_eq!(inner_html(&document, host).unwrap(), "<b>light</b>");
    }

    #[test]
    fn specification_live_parser_foreign_template_adoption_before_eof() {
        let mut document = Document::new(96);
        let (mut parser, _) = HtmlDocumentParser::open(&mut document, ParseOptions {
            scripting_enabled: true, allow_declarative_shadow_roots: false,
        }).unwrap();
        assert!(parser.write_final_until_script(&mut document,
            "<!doctype html><template id=host><select><option selected>stored</option></select></template><script>pause</script><select id=live><option selected>live</option></select>").unwrap().is_some());
        let host = crate::selector::query_selector(&document, document.root(), "#host").unwrap().unwrap();
        let content = document.template_content(host).unwrap().unwrap();
        let mut destination = Document::new(96);
        let (moved, _) = destination.adopt_subtree_from(&mut document, content).unwrap();
        assert_eq!(document.template_content(host), Ok(Some(moved)));
        assert_ne!(moved.document_id(), document.root().document_id());
        assert!(parser.resume_until_script(&mut document).unwrap().is_none());
        assert!(parser.is_closed());
        assert!(crate::selector::query_selector(&document, document.root(), "#live").unwrap().is_some());
        assert!(destination.first_child(moved).unwrap().is_some(), "completion preserves destination-owned controls");
    }

    #[test]
    fn declarative_manual_slot_assignment_round_trips_through_get_html() {
        let document = parse_with_declarative_shadow_roots(
            "<div id=host><template shadowrootmode=open shadowrootdelegatesfocus shadowrootserializable shadowrootslotassignment=manual shadowrootclonable></template></div>",
            16,
            true,
        )
        .unwrap();
        let host = crate::selector::query_selector(&document, document.root(), "#host")
            .unwrap()
            .unwrap();
        let root = document.shadow_root(host).unwrap().unwrap();
        assert_eq!(
            document.shadow_options(root).unwrap().unwrap().slot_assignment,
            crate::shadow::SlotAssignmentMode::Manual
        );
        assert_eq!(
            get_html(&document, host, true, &[]).unwrap(),
            "<template shadowrootmode=\"open\" shadowrootdelegatesfocus=\"\" shadowrootserializable=\"\" shadowrootslotassignment=\"manual\" shadowrootclonable=\"\"></template>"
        );
    }

    #[test]
    fn get_html_preserves_reverse_attached_and_adopted_shadow_roots() {
        let mut source = parse("<main><div id=a></div><div id=b></div></main>", 64).unwrap();
        let a = crate::selector::query_selector(&source, source.root(), "#a").unwrap().unwrap();
        let b = crate::selector::query_selector(&source, source.root(), "#b").unwrap().unwrap();
        let mut options = crate::ShadowOptions::new(crate::ShadowMode::Closed);
        options.serializable = true;
        // Host creation and root attachment deliberately have opposite orders.
        for (host, text) in [(b, "B"), (a, "A")] {
            let root = source.attach_shadow_with_options(host, options).unwrap();
            let child = source.create(NodeKind::Text(text.into())).unwrap();
            source.append(root, child).unwrap();
        }
        let expected_a = "<template shadowrootmode=\"closed\" shadowrootserializable=\"\">A</template>";
        let expected_b = "<template shadowrootmode=\"closed\" shadowrootserializable=\"\">B</template>";
        assert_eq!(get_html(&source, a, true, &[]).unwrap(), expected_a);
        assert_eq!(get_html(&source, b, true, &[]).unwrap(), expected_b);
        let mut target = parse("<span></span>", 64).unwrap();
        let existing = crate::selector::query_selector(&target, target.root(), "span").unwrap().unwrap();
        let existing_root = target.attach_shadow(existing, crate::ShadowMode::Open).unwrap();
        let (adopted, _) = target.adopt_subtree_from(&mut source, a).unwrap();
        assert_eq!(get_html(&target, adopted, true, &[]).unwrap(), expected_a);
        assert_eq!(get_html(&source, b, true, &[]).unwrap(), expected_b);
        assert_eq!(target.shadow_root(existing).unwrap(), Some(existing_root));
        target.destroy_subtree(adopted).unwrap();
        assert!(target.shadow_root(adopted).is_err());
        assert_eq!(target.shadow_root(existing).unwrap(), Some(existing_root));
    }

    #[test]
    fn unsafe_fragment_permission_is_explicit_and_preserves_template_context() {
        let mut document = parse("<div></div><template></template>", 128).unwrap();
        let context = crate::selector::query_selector(&document, document.root(), "div").unwrap().unwrap();
        let markup = "<section><template shadowrootmode=closed shadowrootserializable><i>shadow</i></template><b>light</b></section>";
        let ordinary = parse_fragment_in(&mut document, context, markup).unwrap();
        let ordinary_host = document.first_child(ordinary).unwrap().unwrap();
        assert!(document.shadow_root(ordinary_host).unwrap().is_none());
        let fragment = parse_fragment_in_with_declarative_shadow_roots(&mut document, context, markup, true).unwrap();
        let host = document.first_child(fragment).unwrap().unwrap();
        let root = document.shadow_root(host).unwrap().unwrap();
        assert_eq!(inner_html(&document, root).unwrap(), "<i>shadow</i>");
        assert_eq!(inner_html(&document, host).unwrap(), "<b>light</b>");
        assert!(document.shadow_options(root).unwrap().unwrap().serializable);
        assert!(!document.allow_declarative_shadow_roots(), "opt-in must not alter the document parser default");
        let template = crate::selector::query_selector(&document, document.root(), "template").unwrap().unwrap();
        let content = parse_fragment_in_with_declarative_shadow_roots(&mut document, template, "<tr><td>cell", true).unwrap();
        assert_eq!(inner_html(&document, content).unwrap(), "<tr><td>cell</td></tr>");
        let ordinary_content = parse_fragment_in(&mut document, template, "<tr><td>cell").unwrap();
        assert_eq!(inner_html(&document, ordinary_content).unwrap(), "<tr><td>cell</td></tr>");
        let columns = parse_fragment_in_with_declarative_shadow_roots(&mut document, template, "<col><col>", true).unwrap();
        assert_eq!(inner_html(&document, columns).unwrap(), "<col><col>");
        let in_body = parse_fragment_in_with_declarative_shadow_roots(&mut document, template, "<div><tr><td>text</div>", true).unwrap();
        assert_eq!(inner_html(&document, in_body).unwrap(), "<div>text</div>");
        let outside_template = parse_fragment_in_with_declarative_shadow_roots(&mut document, context, "<tr><td>text", true).unwrap();
        assert_eq!(inner_html(&document, outside_template).unwrap(), "text");
    }

    #[test]
    fn declarative_shadow_roots_attach_during_document_parsing() {
        let document = parse_with_declarative_shadow_roots(
            "<div id=host><template shadowrootmode=OPEN shadowrootdelegatesfocus shadowrootclonable shadowrootserializable><p>shadow</p></template><b>light</b><template shadowrootmode=closed>ignored</template></div>",
            64,
            true,
        ).unwrap();
        let host = crate::selector::query_selector(&document, document.root(), "#host")
            .unwrap()
            .unwrap();
        let root = document.shadow_root(host).unwrap().unwrap();
        let options = document.shadow_options(root).unwrap().unwrap();
        assert_eq!(options.mode, crate::shadow::ShadowMode::Open);
        assert!(
            options.delegates_focus
                && options.clonable
                && options.serializable
                && options.declarative
        );
        assert_eq!(inner_html(&document, root).unwrap(), "<p>shadow</p>");
        assert_eq!(
            inner_html(&document, host).unwrap(),
            "<b>light</b><template shadowrootmode=\"closed\">ignored</template>"
        );
        assert!(
            crate::selector::query_selector(&document, document.root(), "p")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            document.node_count(),
            13,
            "parser-only template and its unused content must be reclaimed"
        );
    }

    #[test]
    fn declarative_shadow_permission_does_not_leak_to_fragments() {
        let source =
            "<div id=host><template shadowrootmode=closed><span>hidden</span></template></div>";
        let inert = parse(source, 32).unwrap();
        let host = crate::selector::query_selector(&inert, inert.root(), "#host")
            .unwrap()
            .unwrap();
        assert!(inert.shadow_root(host).unwrap().is_none());
        let document = parse_with_declarative_shadow_roots(source, 32, true).unwrap();
        let host = crate::selector::query_selector(&document, document.root(), "#host")
            .unwrap()
            .unwrap();
        let root = document.shadow_root(host).unwrap().unwrap();
        assert_eq!(
            document.shadow_mode(root).unwrap(),
            Some(crate::shadow::ShadowMode::Closed)
        );
        let mut fragment_document = parse("<div id=host></div>", 32).unwrap();
        let host =
            crate::selector::query_selector(&fragment_document, fragment_document.root(), "#host")
                .unwrap()
                .unwrap();
        let fragment = parse_fragment_in(
            &mut fragment_document,
            host,
            "<template shadowrootmode=open>x</template>",
        )
        .unwrap();
        assert!(fragment_document.shadow_root(host).unwrap().is_none());
        assert!(inner_html(&fragment_document, fragment)
            .unwrap()
            .contains("<template"));
    }

    #[test]
    fn specification_parser_form_pointer_association_and_mutation_reset() {
        let mut document = parse("<table><form id=owner><tr><td><input id=control><select id=selection></select></table>", 64).unwrap();
        let find = |document: &Document, id: &str| crate::selector::query_selector(document, document.root(), id).unwrap().unwrap();
        let form = find(&document, "#owner");
        let control = find(&document, "#control");
        let selection = find(&document, "#selection");
        assert_eq!(crate::forms::form_owner(&document, control), Some(form));
        assert_eq!(crate::forms::form_owner(&document, selection), Some(form));
        document.set_attribute(control, "form", "absent").unwrap();
        document.remove_attribute(control, "form").unwrap();
        assert_eq!(crate::forms::form_owner(&document, control), None);
        document.remove(selection).unwrap();
        assert_eq!(crate::forms::form_owner(&document, selection), None);
        let body = crate::selector::query_selector(&document, document.root(), "body").unwrap().unwrap();
        document.append(body, selection).unwrap();
        assert_eq!(crate::forms::form_owner(&document, selection), None);
    }

    #[test]
    fn specification_declarative_fragment_uses_actual_context_host_and_permission() {
        let mut document = parse("<main><div id=host></div><input id=invalid></main>", 64).unwrap();
        let host = crate::selector::query_selector(&document, document.root(), "#host").unwrap().unwrap();
        let fragment = parse_fragment_in_with_declarative_shadow_roots(&mut document, host,
            "<template shadowrootmode=closed shadowrootcustomelementregistry><p>shadow</p></template><b>light</b>", true).unwrap();
        let root = document.shadow_root(host).unwrap().expect("actual fragment context receives shadow root");
        assert_eq!(document.shadow_mode(root).unwrap(), Some(crate::shadow::ShadowMode::Closed));
        assert!(document.shadow_options(root).unwrap().unwrap().keep_custom_element_registry_null);
        assert_eq!(inner_html(&document, root).unwrap(), "<p>shadow</p>");
        assert_eq!(inner_html(&document, fragment).unwrap(), "<b>light</b>");
        let invalid = crate::selector::query_selector(&document, document.root(), "#invalid").unwrap().unwrap();
        let fragment = parse_fragment_in_with_declarative_shadow_roots(&mut document, invalid,
            "<template shadowrootmode=open>inert</template>", true).unwrap();
        assert!(document.shadow_root(invalid).unwrap().is_none());
        assert!(inner_html(&document, fragment).unwrap().contains("<template"));
        let fragment = parse_fragment_in(&mut document, invalid,
            "<template shadowrootmode=open>ordinary setter</template>").unwrap();
        assert!(document.shadow_root(invalid).unwrap().is_none());
        assert!(inner_html(&document, fragment).unwrap().contains("ordinary setter"));
    }

    #[test]
    fn declarative_shadow_nested_roots_and_invalid_hosts_follow_template_rules() {
        let document = parse_with_declarative_shadow_roots(
            "<div id=host><template shadowrootmode=open><section id=nested><template shadowrootmode=closed><table>before<tr><td>cell</table>after</template></section></template></div><a id=invalid><template shadowrootmode=open>x</template></a><template shadowrootmode=invalid>y</template>",
            64,
            true,
        ).unwrap();
        let host = crate::selector::query_selector(&document, document.root(), "#host")
            .unwrap()
            .unwrap();
        let outer = document.shadow_root(host).unwrap().unwrap();
        let nested = crate::selector::query_selector(&document, outer, "#nested")
            .unwrap()
            .unwrap();
        let inner = document.shadow_root(nested).unwrap().unwrap();
        assert_eq!(
            inner_html(&document, inner).unwrap(),
            "before<table><tbody><tr><td>cell</td></tr></tbody></table>after"
        );
        assert!(
            crate::selector::query_selector(&document, document.root(), "template")
                .unwrap()
                .is_some()
        );
        let invalid = crate::selector::query_selector(&document, document.root(), "#invalid")
            .unwrap()
            .unwrap();
        assert!(document.shadow_root(invalid).unwrap().is_none());
    }

    #[test]
    fn declarative_shadow_parser_reclaims_temporary_nodes_as_roots_close() {
        let source = "<div><template shadowrootmode=open>x</template></div>".repeat(100);
        let document = parse_with_declarative_shadow_roots(&source, 306, true).unwrap();
        assert_eq!(document.node_count(), 304);
        assert_eq!(document.shadow_roots().count(), 100);
    }

    #[test]
    fn template_contents_are_detached_cloned_and_reclaimed() {
        let mut doc = Document::new(40);
        let fragment = parse_fragment(
            &mut doc,
            "<template> <b>A</b><template><i>B</i></template></template>",
        )
        .unwrap();
        let template = doc.first_child(fragment).unwrap().unwrap();
        let content = doc.template_content(template).unwrap().unwrap();
        assert!(doc.first_child(template).unwrap().is_none());
        assert!(doc.parent(content).unwrap().is_none());
        assert_eq!(doc.append(content, template), Err(DomError::Hierarchy));
        assert_eq!(
            inner_html(&doc, template).unwrap(),
            " <b>A</b><template><i>B</i></template>"
        );
        let clone = doc.clone_subtree(template).unwrap();
        assert_ne!(doc.template_content(clone).unwrap(), Some(content));
        assert_eq!(
            outer_html(&doc, clone).unwrap(),
            outer_html(&doc, template).unwrap()
        );
        doc.destroy_subtree(clone).unwrap();
        doc.destroy_subtree(fragment).unwrap();
        assert_eq!(doc.node_count(), 1);
    }
    use super::*;
    use alloc::vec;

    #[test]
    fn formatting_recovery_reconstructs_and_adopts_blocks() {
        for (source, expected) in [
            (
                "<p>1<b>2<i>3</b>4</i>5</p>",
                "<p>1<b>2<i>3</i></b><i>4</i>5</p>",
            ),
            ("<b>1<p>2</b>3</p>", "<b>1</b><p><b>2</b>3</p>"),
            ("<p><b>A</p>B", "<p><b>A</b></p><b>B</b>"),
            ("<a>A<a>B</a>C", "<a>A</a><a>B</a>C"),
            (
                "<b><i><div>x</b>y</i>z",
                "<b><i></i></b><i></i><div><i><b>x</b>y</i>z</div>",
            ),
            (
                "<table><b><tr><td>aaa</td></tr>bbb</table>ccc",
                "<b></b><b>bbb</b><table><tbody><tr><td>aaa</td></tr></tbody></table><b>ccc</b>",
            ),
        ] {
            let mut doc = Document::new(64);
            let fragment = parse_fragment(&mut doc, source).unwrap();
            assert_eq!(inner_html(&doc, fragment).unwrap(), expected, "{source}");
            assert!(doc.mutations().is_empty());
        }
    }

    #[test]
    fn adoption_failure_reclaims_intermediate_clones() {
        let mut doc = Document::new(7);
        for _ in 0..4 {
            assert!(parse_fragment(&mut doc, "<b><i><div>x</b>").is_err());
            assert_eq!(doc.node_count(), 1);
        }
    }

    #[test]
    fn fragment_context_selects_table_and_text_tokenization() {
        for (name, source, expected) in [
            (
                "table",
                "<tr><td>A<td>B",
                "<tbody><tr><td>A</td><td>B</td></tr></tbody>",
            ),
            ("tbody", "<tr><td>A", "<tr><td>A</td></tr>"),
            ("tr", "<td>A<td>B", "<td>A</td><td>B</td>"),
            (
                "textarea",
                "<b>&amp;</textarea><p>x",
                "&lt;b&gt;&amp;&lt;/textarea&gt;&lt;p&gt;x",
            ),
            ("style", "<b>&amp;", "<b>&amp;"),
        ] {
            let mut doc = Document::new(32);
            let context = doc.create(element(name, Vec::new())).unwrap();
            let fragment = parse_fragment_in(&mut doc, context, source).unwrap();
            doc.append(context, fragment).unwrap();
            assert_eq!(inner_html(&doc, context).unwrap(), expected, "{name}");
        }
    }

    #[test]
    fn html_tokenizer_literal_colon_names_preserve_namespaces_and_reclaim_quota_failures() {
        let mut doc = parse("<main></main>", 32).unwrap();
        let before = doc.node_count();
        for _ in 0..100 {
            let fragment = parse_fragment(&mut doc,
                "<test:test></test:test><svg><test:test/></svg><math><test:test/></math>").unwrap();
            let html = doc.first_child(fragment).unwrap().unwrap();
            let svg = doc.next_sibling(html).unwrap().unwrap();
            let math = doc.next_sibling(svg).unwrap().unwrap();
            for (node, namespace) in [(html, Namespace::Html),
                (doc.first_child(svg).unwrap().unwrap(), Namespace::Svg),
                (doc.first_child(math).unwrap().unwrap(), Namespace::MathMl)] {
                assert_eq!(doc.element_name_parts(node).unwrap(), (None, "test:test"));
                assert!(matches!(doc.kind(node).unwrap(), NodeKind::Element { namespace: actual, .. } if *actual == namespace));
            }
            let clone = doc.clone_node(fragment, true).unwrap();
            assert!(crate::equality::is_equal_node(&doc, fragment, &doc, clone).unwrap());
            doc.destroy_subtree(clone).unwrap();
            doc.destroy_subtree(fragment).unwrap();
            assert_eq!(doc.node_count(), before);
            assert!(doc.literal_colon_names.is_empty());
            assert_eq!(doc.literal_colon_names.capacity(), 0);
        }
        let oversized = "<test:test></test:test>".repeat(64);
        for _ in 0..100 {
            assert!(parse_fragment(&mut doc, &oversized).is_err());
            assert_eq!(doc.node_count(), before);
            assert_eq!(doc.literal_colon_names.capacity(), 0);
        }
        let xml = crate::xml::parse("<p:root xmlns:p='urn:p'><p:child/></p:root>", 8).unwrap();
        let root = crate::selector::document_element(&xml).unwrap();
        assert_eq!(xml.element_name_parts(root).unwrap(), (Some("p"), "root"));
        assert_eq!(xml.element_name_parts(xml.first_child(root).unwrap().unwrap()).unwrap(), (Some("p"), "child"));
    }

    #[test]
    fn fragment_context_distinguishes_literal_colon_names_from_qualified_local_names() {
        let mut doc = Document::new(24);
        doc.set_html_document(true);
        let literal = doc.create_unprefixed_element(Namespace::Html, "h:textarea".into(), Vec::new()).unwrap();
        let qualified = doc.create(NodeKind::Element { namespace: Namespace::Html, name: "h:textarea".into(), attributes: Vec::new() }).unwrap();
        let literal_html = doc.create_unprefixed_element(Namespace::Html, "h:html".into(), Vec::new()).unwrap();
        for _ in 0..100 {
            let before = doc.node_count();
            let parsed = parse_fragment_in(&mut doc, literal, "<b>&amp;</b>").unwrap();
            let first = doc.first_child(parsed).unwrap().unwrap();
            assert!(matches!(doc.kind(first).unwrap(), NodeKind::Element { name, .. } if name == "b"));
            doc.destroy_subtree(parsed).unwrap();
            let rawtext = parse_fragment_in(&mut doc, qualified, "<b>&amp;</b>").unwrap();
            assert!(matches!(doc.kind(doc.first_child(rawtext).unwrap().unwrap()).unwrap(), NodeKind::Text(text) if text == "<b>&</b>"));
            doc.destroy_subtree(rawtext).unwrap();
            let ordinary = parse_fragment_in(&mut doc, literal_html, "<span>text</span>").unwrap();
            assert!(matches!(doc.kind(doc.first_child(ordinary).unwrap().unwrap()).unwrap(), NodeKind::Element { name, .. } if name == "span"));
            doc.destroy_subtree(ordinary).unwrap();
            assert_eq!(doc.node_count(), before);
        }
    }

    #[test]
    fn html_context_body_end_places_comments_after_body_and_reclaims_failed_fragments() {
        let mut doc = Document::new(16);
        let context = doc.create_unprefixed_element(Namespace::Html, "html".into(), Vec::new()).unwrap();
        for _ in 0..100 {
            for source in ["<head></head><body></body><!-- tail -->", "<body></body><!-- tail -->",
                           "<body></body></html><!-- tail -->"] {
                let fragment = parse_fragment_in(&mut doc, context, source).unwrap();
                assert_eq!(inner_html(&doc, fragment).unwrap(), "<head></head><body></body><!-- tail -->");
                doc.destroy_subtree(fragment).unwrap();
                assert_eq!(doc.node_count(), 2);
            }
            let source = "<body></body><!-- tail -->".to_owned() + &"<span></span>".repeat(32);
            assert!(parse_fragment_in(&mut doc, context, &source).is_err());
            assert_eq!(doc.node_count(), 2);
        }
        let full = parse("<!doctype html><html><head></head><body></body><!-- tail --></html><!-- document -->", 16).unwrap();
        assert_eq!(inner_html(&full, full.root()).unwrap(),
                   "<!DOCTYPE html><html><head></head><body></body><!-- tail --></html><!-- document -->");
    }

    #[test]
    fn prefixed_html_namespace_fragment_context_uses_local_name() {
        let mut doc = Document::new(48);
        doc.set_html_document(true);
        let container = doc.create(element("main", Vec::new())).unwrap();
        doc.append(doc.root(), container).unwrap();

        let table = doc
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: Name::new("p:table"),
                attributes: Vec::new(),
            })
            .unwrap();
        doc.append(container, table).unwrap();
        let fragment = parse_fragment_in(&mut doc, table, "<tr><td>A<td>B").unwrap();
        doc.append(table, fragment).unwrap();
        let tbody = doc.first_child(table).unwrap().unwrap();
        assert!(matches!(
            doc.kind(tbody),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "tbody"
        ));
        assert_eq!(
            inner_html(&doc, table).unwrap(),
            "<tbody><tr><td>A</td><td>B</td></tr></tbody>"
        );

        let textarea = doc
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: Name::new("p:textarea"),
                attributes: Vec::new(),
            })
            .unwrap();
        doc.append(container, textarea).unwrap();
        let fragment = parse_fragment_in(&mut doc, textarea, "<b>&amp;").unwrap();
        let text = doc.first_child(fragment).unwrap().unwrap();
        assert_eq!(doc.kind(text).unwrap(), &NodeKind::Text("<b>&".into()));

        let style = doc
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: Name::new("p:style"),
                attributes: Vec::new(),
            })
            .unwrap();
        doc.append(container, style).unwrap();
        let fragment = parse_fragment_in(&mut doc, style, "<b>&amp;").unwrap();
        let text = doc.first_child(fragment).unwrap().unwrap();
        assert_eq!(doc.kind(text).unwrap(), &NodeKind::Text("<b>&amp;".into()));
    }

    #[test]
    fn fragment_context_preserves_foreign_namespaces_and_integration_points() {
        let mut doc = Document::new(48);
        let svg_context = doc
            .create(NodeKind::Element {
                namespace: Namespace::Svg,
                name: Name::new("svg"),
                attributes: Vec::new(),
            })
            .unwrap();
        let svg_fragment = parse_fragment_in(
            &mut doc,
            svg_context,
            "<g viewbox='0 0 1 1'><foreignObject><div>A</div></foreignObject><![CDATA[x]]></g>",
        )
        .unwrap();
        let group = doc.first_child(svg_fragment).unwrap().unwrap();
        assert!(matches!(
            doc.kind(group),
            Ok(NodeKind::Element {
                namespace: Namespace::Svg,
                name,
                attributes,
            }) if name == "g" && attributes[0].0 == "viewBox"
        ));
        let foreign_object = doc.first_child(group).unwrap().unwrap();
        let div = doc.first_child(foreign_object).unwrap().unwrap();
        assert!(matches!(
            doc.kind(div),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "div"
        ));
        let text = doc.next_sibling(foreign_object).unwrap().unwrap();
        assert_eq!(doc.kind(text).unwrap(), &NodeKind::Text("x".to_string()));

        let math_context = doc
            .create(NodeKind::Element {
                namespace: Namespace::MathMl,
                name: Name::new("mi"),
                attributes: Vec::new(),
            })
            .unwrap();
        let math_fragment =
            parse_fragment_in(&mut doc, math_context, "<mglyph/><span>x</span>").unwrap();
        let mglyph = doc.first_child(math_fragment).unwrap().unwrap();
        assert!(matches!(
            doc.kind(mglyph),
            Ok(NodeKind::Element {
                namespace: Namespace::MathMl,
                name,
                ..
            }) if name == "mglyph"
        ));
        let span = doc.next_sibling(mglyph).unwrap().unwrap();
        assert!(matches!(
            doc.kind(span),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "span"
        ));

        let annotation_context = doc
            .create(NodeKind::Element {
                namespace: Namespace::MathMl,
                name: Name::new("annotation-xml"),
                attributes: alloc::vec![(Name::new("encoding"), "text/html".to_string())],
            })
            .unwrap();
        let annotation_fragment =
            parse_fragment_in(&mut doc, annotation_context, "<svg><circle/></svg><p>x</p>")
                .unwrap();
        let svg = doc.first_child(annotation_fragment).unwrap().unwrap();
        assert!(matches!(
            doc.kind(svg),
            Ok(NodeKind::Element {
                namespace: Namespace::Svg,
                name,
                ..
            }) if name == "svg"
        ));
        let paragraph = doc.next_sibling(svg).unwrap().unwrap();
        assert!(matches!(
            doc.kind(paragraph),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "p"
        ));
    }

    #[test]
    fn token_recovery_keeps_comments_and_discards_incomplete_tags() {
        for (source, expected) in [
            ("<div title=\"unfinished", ""),
            ("<p a=>A</p></p></br>", "<p a=\"\">A</p><p></p><br>"),
            (
                "<!bogus><!--unterminated",
                "<!--bogus--><!--unterminated-->",
            ),
            (
                "<select><option>A<option>B<optgroup label=x><option>C</select>",
                "<select><option>A</option><option>B</option><optgroup label=\"x\"><option>C</option></optgroup></select>",
            ),
            (
                "<template><tr><td>A</template>B",
                "<template><tr><td>A</td></tr></template>B",
            ),
            (
                "<table><tr><td><b>A<td>B</table>C",
                "<table><tbody><tr><td><b>A</b></td><td>B</td></tr></tbody></table>C",
            ),
        ] {
            let mut doc = Document::new(32);
            let fragment = parse_fragment(&mut doc, source).unwrap();
            assert_eq!(inner_html(&doc, fragment).unwrap(), expected, "{source}");
        }
    }

    #[test]
    fn comments_and_doctypes_follow_initial_and_before_html_states() {
        let document = parse(
            "<!--first--><!doctype html><?xml x><html><!--second--><!doctype html><title>T</title>",
            32,
        )
        .unwrap();
        assert_eq!(
            inner_html(&document, document.root()).unwrap(),
            "<!--first--><!DOCTYPE html><!--?xml x--><html><!--second--><head><title>T</title></head><body></body></html>"
        );
        for (source, expected) in [
            ("A<!-->B", "A<!---->B"),
            ("A<!--->B", "A<!---->B"),
            ("A<!--unfinished--", "A<!--unfinished-->"),
            ("A<!---", "A<!---->"),
        ] {
            let mut document = Document::new(32);
            let fragment = parse_fragment(&mut document, source).unwrap();
            assert_eq!(inner_html(&document, fragment).unwrap(), expected);
        }
    }

    #[test]
    fn block_start_tags_close_paragraphs_and_code_reconstructs_formatting() {
        for name in [
            "details",
            "dialog",
            "figcaption",
            "figure",
            "hgroup",
            "menu",
            "search",
            "summary",
        ] {
            let document = parse(&alloc::format!("<p>A<{name}>B<p>C"), 32).unwrap();
            assert_eq!(
                inner_html(&document, document.root()).unwrap(),
                alloc::format!(
                    "<html><head></head><body><p>A</p><{name}>B<p>C</p></{name}></body></html>"
                )
            );
        }
        let document = parse("<p><code>A</p>B", 32).unwrap();
        assert_eq!(
            inner_html(&document, document.root()).unwrap(),
            "<html><head></head><body><p><code>A</code></p><code>B</code></body></html>"
        );
    }

    #[test]
    fn processing_instructions_recover_and_follow_document_insertion_modes() {
        let document = parse("<?before?><html><head></head><?between?><body><?good   data??><p><button><div>X</div><button>Y</button></p></body><?after?></html><?last?>", 64).unwrap();
        assert_eq!(
            inner_html(&document, document.root()).unwrap(),
            "<?before ?><html><head></head><?between ?><body><?good data??><p><button><div>X</div></button><button>Y</button></p></body><?after ?></html><?last ?>"
        );
        let mut document = Document::new(32);
        let fragment =
            parse_fragment(&mut document, "<?xml?>< ?text><?a$><?valid?><?_unfinished").unwrap();
        assert_eq!(
            inner_html(&document, fragment).unwrap(),
            "<!--?xml?-->&lt; ?text&gt;<!--?a$--><?valid ?>"
        );
    }

    #[test]
    fn parses_document_tree() {
        let doc = parse("<!doctype html><html><head><style>p{color:red}</style></head><body><p id='x'>hi</p></body></html>", 16).unwrap();
        let root = doc.root();
        let doctype = doc.first_child(root).unwrap().unwrap();
        assert_eq!(
            doc.kind(doctype).unwrap(),
            &NodeKind::DocumentType("html".to_string())
        );
        let html = doc.next_sibling(doctype).unwrap().unwrap();
        let head = doc.first_child(html).unwrap().unwrap();
        let style = doc.first_child(head).unwrap().unwrap();
        assert!(
            matches!(doc.kind(style).unwrap(), NodeKind::Element { name, .. } if name == "style")
        );
    }

    #[test]
    fn style_and_script_keep_raw_text_inert() {
        let doc = parse(
            "<style>p::before{content:'<';}</style><script>if (a < b) run()</script>",
            12,
        )
        .unwrap();
        let html = doc.first_child(doc.root()).unwrap().unwrap();
        let head = doc.first_child(html).unwrap().unwrap();
        let style = doc.first_child(head).unwrap().unwrap();
        let text = doc.first_child(style).unwrap().unwrap();
        assert!(matches!(doc.kind(text).unwrap(), NodeKind::Text(value) if value.contains("'<'")));
        let script = doc.next_sibling(style).unwrap().unwrap();
        assert!(
            matches!(doc.kind(script).unwrap(), NodeKind::Element { name, .. } if name == "script")
        );
    }

    #[test]
    fn raw_and_rcdata_require_an_appropriate_end_tag() {
        let mut doc = Document::new(24);
        let fragment = parse_fragment(&mut doc,
            "<script>a</scriptx>&amp;<b></SCRIPT><textarea>\nA &amp; <b></textareax> B</textarea><title>&lt;x&gt;</title>").unwrap();
        assert_eq!(
            inner_html(&doc, fragment).unwrap(),
            "<script>a</scriptx>&amp;<b></script><textarea>A &amp; &lt;b&gt;&lt;/textareax&gt; B</textarea><title>&lt;x&gt;</title>"
        );
    }

    #[test]
    fn script_double_escaped_end_tag_remains_inert_text() {
        let source = "<script><!--<script>var a = 1;</script>after</script><p>x";
        let mut doc = Document::new(16);
        let fragment = parse_fragment(&mut doc, source).unwrap();
        assert_eq!(
            inner_html(&doc, fragment).unwrap(),
            "<script><!--<script>var a = 1;</script>after</script><p>x</p>"
        );
    }

    #[test]
    fn plaintext_normalizes_input_and_bounds_null_expansion_without_parsing_markup() {
        let text = normalize_plaintext("<tag>&amp;\r\nnext\r\0end".into()).unwrap();
        assert_eq!(text, "<tag>&amp;\nnext\n\u{fffd}end");
        assert!(normalize_plaintext("\0".repeat(MAX_HTML_BYTES / 3 + 1)).is_err());
        assert!(normalize_plaintext("x".repeat(MAX_HTML_BYTES + 1)).is_err());
    }

    #[test]
    fn tokenizer_normalizes_newlines_nulls_and_attribute_names() {
        let mut doc = Document::new(12);
        let fragment = parse_fragment(
            &mut doc,
            "<pre>\r\na\rb\r\nc\0</pre><p data.foo='x' @click='y' café='z'>",
        )
        .unwrap();
        assert_eq!(
            inner_html(&doc, fragment).unwrap(),
            "<pre>a\nb\nc</pre><p data.foo=\"x\" @click=\"y\" café=\"z\"></p>"
        );
        assert!(matches!(normalized_input("unchanged"), Cow::Borrowed(_)));
    }

    #[test]
    fn data_null_tokens_are_preserved_by_tokenizer_and_ignored_by_html_tree_builder() {
        let input = "<body><table>\0filler\0text\0";
        assert_eq!(
            tokenize_for_conformance(input, "Data state", None).unwrap(),
            vec![
                ConformanceToken::StartTag {
                    name: "body".into(),
                    attributes: Vec::new(),
                    self_closing: false,
                },
                ConformanceToken::StartTag {
                    name: "table".into(),
                    attributes: Vec::new(),
                    self_closing: false,
                },
                ConformanceToken::Character("\0filler\0text\0".into()),
            ]
        );

        let document = parse(input, 16).unwrap();
        assert_eq!(
            inner_html(&document, document.root()).unwrap(),
            "<html><head></head><body>fillertext<table></table></body></html>"
        );
        let foreign = parse("<svg>\0</svg>", 8).unwrap();
        assert_eq!(
            inner_html(&foreign, foreign.root()).unwrap(),
            "<html><head></head><body><svg>�</svg></body></html>"
        );
    }

    #[test]
    fn decodes_text_and_attribute_references() {
        let doc = parse("<p title='A &amp; B'>&lt;ok&#x21;&unknown;</p>", 6).unwrap();
        let html = doc.first_child(doc.root()).unwrap().unwrap();
        let body = doc
            .next_sibling(doc.first_child(html).unwrap().unwrap())
            .unwrap()
            .unwrap();
        let p = doc.first_child(body).unwrap().unwrap();
        assert!(
            matches!(doc.kind(p).unwrap(), NodeKind::Element { attributes, .. } if attributes[0].1 == "A & B")
        );
        let text = doc.first_child(p).unwrap().unwrap();
        assert_eq!(
            doc.kind(text).unwrap(),
            &NodeKind::Text("<ok!&unknown;".to_string())
        );
        assert_eq!(
            outer_html(&doc, p).unwrap(),
            "<p title=\"A &amp; B\">&lt;ok!&amp;unknown;</p>"
        );
    }

    #[test]
    fn whatwg_named_and_numeric_references() {
        assert_eq!(
            decode_entities("&copy and &acE; &#x80; &#0; &unknown;", false),
            "\u{a9} and \u{223e}\u{333} \u{20ac} \u{fffd} &unknown;"
        );
        assert_eq!(
            decode_entities("x&copy=1 &copy;!", true),
            "x&copy=1 \u{a9}!"
        );
    }

    #[test]
    fn fragment_parses_in_arena_and_rolls_back_errors() {
        let mut doc = Document::new(16);
        let fragment = parse_fragment(&mut doc, "<b>A</b><i>B</i>").unwrap();
        assert_eq!(inner_html(&doc, fragment).unwrap(), "<b>A</b><i>B</i>");
        assert!(doc.drain_mutations().is_empty());
        let copy = doc.clone_subtree(fragment).unwrap();
        assert_eq!(inner_html(&doc, copy).unwrap(), "<b>A</b><i>B</i>");
        let count = doc.node_count();
        let too_many_nodes = "<div><div><div><div><div><div><div><div><div><div>oops";
        assert!(parse_fragment(&mut doc, too_many_nodes).is_err());
        assert_eq!(doc.node_count(), count);
        let allocated = doc.nodes.len();
        for _ in 0..8 {
            assert!(parse_fragment(&mut doc, too_many_nodes).is_err());
            assert_eq!(doc.node_count(), count);
            assert_eq!(doc.nodes.len(), allocated);
        }
    }

    #[test]
    fn document_scaffold_keeps_explicit_attributes() {
        let doc = parse(
            "<html lang='en'><head id='h'></head><body style='background:red'></body></html>",
            4,
        )
        .unwrap();
        let html = doc.first_child(doc.root()).unwrap().unwrap();
        let head = doc.first_child(html).unwrap().unwrap();
        let body = doc.next_sibling(head).unwrap().unwrap();
        assert!(
            matches!(doc.kind(html).unwrap(), NodeKind::Element { attributes, .. } if attributes[0].1 == "en")
        );
        assert!(
            matches!(doc.kind(head).unwrap(), NodeKind::Element { attributes, .. } if attributes[0].1 == "h")
        );
        assert!(
            matches!(doc.kind(body).unwrap(), NodeKind::Element { attributes, .. } if attributes[0].1 == "background:red")
        );
    }

    #[test]
    fn implied_end_tags_and_adjacent_text_match_html_tree() {
        let doc = parse("<p>one<p>two<div>three</div><ul><li>A<li>B</ul>x < y", 24).unwrap();
        let html = doc.first_child(doc.root()).unwrap().unwrap();
        let head = doc.first_child(html).unwrap().unwrap();
        let body = doc.next_sibling(head).unwrap().unwrap();
        assert_eq!(
            inner_html(&doc, body).unwrap(),
            "<p>one</p><p>two</p><div>three</div><ul><li>A</li><li>B</li></ul>x &lt; y"
        );
    }

    #[test]
    fn nested_lists_keep_outer_item_open() {
        let doc = parse("<ul><li>outer<ul><li>inner</li></ul><li>next</ul>", 20).unwrap();
        let html = doc.first_child(doc.root()).unwrap().unwrap();
        let head = doc.first_child(html).unwrap().unwrap();
        let body = doc.next_sibling(head).unwrap().unwrap();
        assert_eq!(
            inner_html(&doc, body).unwrap(),
            "<ul><li>outer<ul><li>inner</li></ul></li><li>next</li></ul>"
        );
    }

    #[test]
    fn table_rows_insert_tbody_and_close_cells() {
        let doc = parse("<table><tr><td>A<td>B<tr><td>C</table>", 24).unwrap();
        let html = doc.first_child(doc.root()).unwrap().unwrap();
        let head = doc.first_child(html).unwrap().unwrap();
        let body = doc.next_sibling(head).unwrap().unwrap();
        assert_eq!(
            inner_html(&doc, body).unwrap(),
            "<table><tbody><tr><td>A</td><td>B</td></tr><tr><td>C</td></tr></tbody></table>"
        );
    }

    #[test]
    fn table_recovery_fosters_content_and_implies_missing_containers() {
        let mut doc = Document::new(32);
        let fragment =
            parse_fragment(&mut doc, "<table>before<div>x</div><td>A<td>B</table>after").unwrap();
        assert_eq!(
            inner_html(&doc, fragment).unwrap(),
            "before<div>x</div><table><tbody><tr><td>A</td><td>B</td></tr></tbody></table>after"
        );
        let columns = parse_fragment(&mut doc, "<table><col><tr><td>C</table>").unwrap();
        assert_eq!(
            inner_html(&doc, columns).unwrap(),
            "<table><colgroup><col></colgroup><tbody><tr><td>C</td></tr></tbody></table>"
        );
        assert!(doc.mutations().is_empty());
    }

    #[test]
    fn parses_obsolete_elements_and_foreign_content_with_namespaces() {
        let doc = parse(
            "<font>legacy</font><svg><foreignObject><div>html</div></foreignObject><use xlink:href=\"#shape\"/></svg><math><mrow><mi>x</mi></mrow></math>",
            32,
        )
        .unwrap();
        let html = doc.first_child(doc.root()).unwrap().unwrap();
        let head = doc.first_child(html).unwrap().unwrap();
        let body = doc.next_sibling(head).unwrap().unwrap();
        assert_eq!(
            inner_html(&doc, body).unwrap(),
            "<font>legacy</font><svg><foreignObject><div>html</div></foreignObject><use xlink:href=\"#shape\"></use></svg><math><mrow><mi>x</mi></mrow></math>"
        );

        let svg = doc.first_child(body).unwrap().unwrap();
        let svg = doc.next_sibling(svg).unwrap().unwrap();
        assert!(matches!(
            doc.kind(svg),
            Ok(NodeKind::Element {
                namespace: Namespace::Svg,
                name,
                ..
            }) if name == "svg"
        ));
        let foreign_object = doc.first_child(svg).unwrap().unwrap();
        assert!(matches!(
            doc.kind(foreign_object),
            Ok(NodeKind::Element {
                namespace: Namespace::Svg,
                name,
                ..
            }) if name == "foreignObject"
        ));
        let div = doc.first_child(foreign_object).unwrap().unwrap();
        assert!(matches!(
            doc.kind(div),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "div"
        ));
        let use_element = doc.next_sibling(foreign_object).unwrap().unwrap();
        assert_eq!(
            doc.attribute_namespace_uri(use_element, "xlink:href")
                .unwrap()
                .as_deref(),
            Some("http://www.w3.org/1999/xlink")
        );
        assert_eq!(
            doc.get_attribute_ns(use_element, Some("http://www.w3.org/1999/xlink"), "href")
                .unwrap()
                .as_deref(),
            Some("#shape")
        );

        let math = doc.next_sibling(svg).unwrap().unwrap();
        assert!(matches!(
            doc.kind(math),
            Ok(NodeKind::Element {
                namespace: Namespace::MathMl,
                name,
                ..
            }) if name == "math"
        ));
        let mrow = doc.first_child(math).unwrap().unwrap();
        assert!(matches!(
            doc.kind(mrow),
            Ok(NodeKind::Element {
                namespace: Namespace::MathMl,
                name,
                ..
            }) if name == "mrow"
        ));
    }

    #[test]
    fn doctypes_preserve_identifiers_and_select_html_document_mode() {
        let limited = parse(
            "<!doctype html PUBLIC \"-//W3C//DTD HTML 4.01 Transitional//EN\" \"http://www.w3.org/TR/html4/loose.dtd\"><p>legacy</p>",
            16,
        )
        .unwrap();
        let doctype = limited.doctype().unwrap().unwrap();
        assert_eq!(limited.document_mode(), crate::DocumentMode::LimitedQuirks);
        assert_eq!(limited.compat_mode(), "CSS1Compat");
        assert_eq!(
            limited.doctype_public_id(doctype).unwrap(),
            "-//W3C//DTD HTML 4.01 Transitional//EN"
        );
        assert_eq!(
            limited.doctype_system_id(doctype).unwrap(),
            "http://www.w3.org/TR/html4/loose.dtd"
        );
        assert_eq!(
            outer_html(&limited, doctype).unwrap(),
            "<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.01 Transitional//EN\" \"http://www.w3.org/TR/html4/loose.dtd\">"
        );

        let strict = parse(
            "<!doctype html PUBLIC \"-//W3C//DTD XHTML 1.0 Strict//EN\" \"http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd\"><p>strict</p>",
            16,
        )
        .unwrap();
        assert_eq!(strict.document_mode(), crate::DocumentMode::NoQuirks);

        let without_doctype = parse("<p>legacy</p>", 12).unwrap();
        assert_eq!(without_doctype.document_mode(), crate::DocumentMode::Quirks);
        assert_eq!(without_doctype.compat_mode(), "BackCompat");
        let non_html = parse("<!doctype svg><p>legacy</p>", 12).unwrap();
        assert_eq!(non_html.document_mode(), crate::DocumentMode::Quirks);
    }

    #[test]
    fn self_closing_tag_keeps_unquoted_attribute_value() {
        let doc = parse("<img src=tile.png/>", 8).unwrap();
        let html = doc.first_child(doc.root()).unwrap().unwrap();
        let head = doc.first_child(html).unwrap().unwrap();
        let body = doc.next_sibling(head).unwrap().unwrap();
        assert_eq!(inner_html(&doc, body).unwrap(), "<img src=\"tile.png/\">");
        let mut fragment_doc = Document::new(8);
        let fragment = parse_fragment(&mut fragment_doc, "<div/>text").unwrap();
        assert_eq!(
            inner_html(&fragment_doc, fragment).unwrap(),
            "<div>text</div>"
        );
    }

    #[test]
    fn noscript_parsing_and_user_agent_visibility_follow_scripting_mode() {
        let source = "<!doctype html><html><head></head><body><noscript id=n><span>fallback</span></noscript></body></html>";
        let disabled = parse(source, 24).unwrap();
        assert!(!disabled.scripting_enabled());
        let disabled_noscript =
            crate::selector::query_selector(&disabled, disabled.root(), "noscript")
                .unwrap()
                .unwrap();
        assert!(matches!(
            disabled.kind(disabled.first_child(disabled_noscript).unwrap().unwrap()),
            Ok(NodeKind::Element { name, .. }) if name == "span"
        ));

        let enabled = parse_with_options(
            source,
            24,
            ParseOptions {
                allow_declarative_shadow_roots: false,
                scripting_enabled: true,
            },
        )
        .unwrap();
        assert!(enabled.scripting_enabled());
        let enabled_noscript =
            crate::selector::query_selector(&enabled, enabled.root(), "noscript")
                .unwrap()
                .unwrap();
        assert!(matches!(
            enabled.kind(enabled.first_child(enabled_noscript).unwrap().unwrap()),
            Ok(NodeKind::Text(text)) if text == "<span>fallback</span>"
        ));

        let author_important = crate::css::StyleIndex::new(
            crate::css::parse("noscript { display: block !important }").unwrap(),
        );
        assert_eq!(
            crate::css::compute_node(&enabled, enabled_noscript, None, &author_important)
                .unwrap()
                .display,
            crate::css::Display::None
        );
        let disabled_style =
            crate::css::compute_node(&disabled, disabled_noscript, None, &author_important)
                .unwrap();
        assert_eq!(disabled_style.display, crate::css::Display::Block);

        let head_source = "<!doctype html><html><head><noscript><meta name=x></noscript></head><body></body></html>";
        let active_head = parse_with_options(
            head_source,
            24,
            ParseOptions {
                allow_declarative_shadow_roots: false,
                scripting_enabled: true,
            },
        )
        .unwrap();
        let head_noscript =
            crate::selector::query_selector(&active_head, active_head.root(), "noscript")
                .unwrap()
                .unwrap();
        assert!(matches!(
            active_head.kind(active_head.first_child(head_noscript).unwrap().unwrap()),
            Ok(NodeKind::Text(text)) if text == "<meta name=x>"
        ));
        assert_eq!(
            crate::css::compute_node(
                &active_head,
                head_noscript,
                None,
                &crate::css::StyleIndex::new(Vec::new()),
            )
            .unwrap()
            .display,
            crate::css::Display::None
        );

        let foreign = parse_with_options(
            "<!doctype html><html><head></head><body><svg><noscript><circle/></noscript></svg></body></html>",
            24,
            ParseOptions {
                allow_declarative_shadow_roots: false,
                scripting_enabled: true,
            },
        )
        .unwrap();
        let svg = crate::selector::query_selector(&foreign, foreign.root(), "svg")
            .unwrap()
            .unwrap();
        let foreign_noscript = foreign.first_child(svg).unwrap().unwrap();
        assert!(matches!(
            foreign.kind(foreign_noscript),
            Ok(NodeKind::Element {
                namespace: Namespace::Svg,
                name,
                ..
            }) if name == "noscript"
        ));
        assert!(matches!(
            foreign.kind(foreign.first_child(foreign_noscript).unwrap().unwrap()),
            Ok(NodeKind::Element {
                namespace: Namespace::Svg,
                name,
                ..
            }) if name == "circle"
        ));
        assert_ne!(
            crate::css::compute_node(
                &foreign,
                foreign_noscript,
                None,
                &crate::css::StyleIndex::new(Vec::new()),
            )
            .unwrap()
            .display,
            crate::css::Display::None
        );
    }

    #[test]
    fn incremental_document_parser_preserves_nodes_and_declarative_shadow_permission() {
        let source = parse_with_options(
            "<!doctype html><div>old</div>",
            64,
            ParseOptions {
                allow_declarative_shadow_roots: true,
                scripting_enabled: true,
            },
        )
        .unwrap();
        let mut document = source.clone_document(true).unwrap();
        assert!(document.allow_declarative_shadow_roots());
        let root = document.root();
        let options = ParseOptions {
            allow_declarative_shadow_roots: document.allow_declarative_shadow_roots(),
            scripting_enabled: document.scripting_enabled(),
        };
        let (mut parser, removed) = HtmlDocumentParser::open(&mut document, options).unwrap();
        assert_eq!(document.root(), root);
        assert_eq!(removed.len(), 2);

        let body = crate::selector::query_selector(&document, root, "body")
            .unwrap()
            .unwrap();
        parser.write(&mut document, "<div id=\"fragment").unwrap();
        assert!(document.first_child(body).unwrap().is_none());
        parser.write(&mut document, "\">alpha&amp").unwrap();
        let host = document.first_child(body).unwrap().unwrap();
        let text = document.first_child(host).unwrap().unwrap();
        assert!(matches!(document.kind(text), Ok(NodeKind::Text(value)) if value == "alpha"));

        parser
            .write(
                &mut document,
                ";omega</div><section id=host><template shadowrootmode=open>shadow",
            )
            .unwrap();
        assert_eq!(document.first_child(host).unwrap(), Some(text));
        parser
            .write(&mut document, "</template></section>")
            .unwrap();
        parser.close(&mut document).unwrap();
        assert!(parser.close(&mut document).is_ok());
        assert!(matches!(document.kind(text), Ok(NodeKind::Text(value)) if value == "alpha&omega"));

        let shadow_host = crate::selector::query_selector(&document, root, "#host")
            .unwrap()
            .unwrap();
        let shadow_root = document.shadow_root(shadow_host).unwrap().unwrap();
        let mut shadow_text = String::new();
        document
            .append_descendant_text(shadow_root, &mut shadow_text)
            .unwrap();
        assert_eq!(shadow_text, "shadow");
    }

    #[test]
    fn incremental_document_parser_compacts_consumed_source() {
        let mut document = Document::new(16);
        let (mut parser, _) =
            HtmlDocumentParser::open(&mut document, ParseOptions::default()).unwrap();
        let chunk = "x".repeat(128 * 1024);
        parser.write(&mut document, &chunk).unwrap();
        assert_eq!(parser.source_base, chunk.len());
        assert!(parser.input.is_empty());
        assert_eq!(parser.total_input_bytes, chunk.len());
        parser.close(&mut document).unwrap();
    }

    #[test]
    fn incremental_document_parser_yields_at_script_and_inserts_before_buffered_tail() {
        let mut document = Document::new(32);
        let options = ParseOptions {
            allow_declarative_shadow_roots: false,
            scripting_enabled: true,
        };
        let (mut parser, _) = HtmlDocumentParser::open(&mut document, options).unwrap();

        assert!(parser
            .write_until_script(&mut document, "<div><script id=writer>1</scr")
            .unwrap()
            .is_none());
        let script = parser
            .write_until_script(&mut document, "ipt><i id=tail>tail</i></div>\r")
            .unwrap()
            .expect("complete parser script should yield a checkpoint");
        assert!(matches!(
            document.kind(script),
            Ok(NodeKind::Element { name, .. }) if name == "script"
        ));
        assert!(
            crate::selector::query_selector(&document, document.root(), "#tail")
                .unwrap()
                .is_none()
        );

        let outer_script = script;
        assert!(parser
            .write_at_script_position_until_script(
                &mut document,
                outer_script,
                &["<b id=nested>nested</b>A\r".to_owned()],
                false,
            )
            .unwrap()
            .is_none());
        let nested_script = parser
            .write_at_script_position_until_script(
                &mut document,
                outer_script,
                &["\nB<script id=inner>nested script</script>C\r".to_owned()],
                false,
            )
            .unwrap()
            .expect("inserted parser script should yield synchronously");
        assert!(matches!(
            document.kind(nested_script),
            Ok(NodeKind::Element { name, .. }) if name == "script"
        ));
        let nested = crate::selector::query_selector(&document, document.root(), "#nested")
            .unwrap()
            .unwrap();
        assert!(parser
            .resume_insertion_until_script(&mut document, nested_script, outer_script)
            .unwrap()
            .is_none());
        assert!(parser.is_paused_for_script());
        assert!(parser.pending_cr);
        assert!(!parser.pending_insertion_cr);
        let unread = &parser.input[parser.state.as_ref().unwrap().pos..];
        assert!(unread.starts_with("<i id=tail>tail</i>"), "unread source: {unread:?}");
        let text_before_inner = document.next_sibling(nested).unwrap().unwrap();
        assert!(matches!(
            document.kind(text_before_inner),
            Ok(NodeKind::Text(value)) if value == "A\nB"
        ));
        let text_after_inner = document.next_sibling(nested_script).unwrap().unwrap();
        assert!(matches!(
            document.kind(text_after_inner),
            Ok(NodeKind::Text(value)) if value == "C\n"
        ));

        assert!(parser.finish_until_script(&mut document).unwrap().is_none());
        assert!(parser.resume_until_script(&mut document).unwrap().is_none());
        assert!(parser.is_closed());

        let script = crate::selector::query_selector(&document, document.root(), "#writer")
            .unwrap()
            .unwrap();
        let nested = crate::selector::query_selector(&document, document.root(), "#nested")
            .unwrap()
            .unwrap();
        let tail = crate::selector::query_selector(&document, document.root(), "#tail")
            .unwrap()
            .unwrap();
        assert_eq!(
            document.parent(nested).unwrap(),
            document.parent(script).unwrap()
        );
        assert_eq!(
            document.parent(tail).unwrap(),
            document.parent(script).unwrap()
        );
        assert_eq!(document.next_sibling(script).unwrap(), Some(nested));
        assert_eq!(document.next_sibling(nested).unwrap(), Some(text_before_inner));
        assert_eq!(document.next_sibling(text_before_inner).unwrap(), Some(nested_script));
        assert_eq!(document.next_sibling(nested_script).unwrap(), Some(text_after_inner));
        assert_eq!(document.next_sibling(text_after_inner).unwrap(), Some(tail));
    }

    #[test]
    fn nested_document_writes_are_bounded_to_the_innermost_insertion_region() {
        let mut document = Document::new(32);
        let (mut parser, _) =
            HtmlDocumentParser::open(&mut document, ParseOptions::default()).unwrap();
        let outer_script = parser
            .write_until_script(
                &mut document,
                "<script id=outer>outer</script><u id=source-tail>tail</u>",
            )
            .unwrap()
            .expect("outer parser script should yield");

        let inner_script = parser
            .write_at_script_position_until_script(
                &mut document,
                outer_script,
                &["<script id=inner>inner</script><i id=outer-write-tail>tail</i>".to_owned()],
                false,
            )
            .unwrap()
            .expect("outer write should synchronously yield its nested script");
        assert!(
            crate::selector::query_selector(&document, document.root(), "#outer-write-tail")
                .unwrap()
                .is_none()
        );

        assert!(parser
            .write_at_script_position_until_script(
                &mut document,
                inner_script,
                &["<b id=inner-write>inner write</b>".to_owned()],
                false,
            )
            .unwrap()
            .is_none());
        assert!(
            crate::selector::query_selector(&document, document.root(), "#inner-write")
                .unwrap()
                .is_some()
        );
        assert!(
            crate::selector::query_selector(&document, document.root(), "#outer-write-tail")
                .unwrap()
                .is_none()
        );

        assert!(parser
            .resume_insertion_until_script(&mut document, inner_script, outer_script)
            .unwrap()
            .is_none());
        assert!(
            crate::selector::query_selector(&document, document.root(), "#outer-write-tail")
                .unwrap()
                .is_some()
        );

        assert!(parser.resume_until_script(&mut document).unwrap().is_none());
        assert!(
            crate::selector::query_selector(&document, document.root(), "#source-tail")
                .unwrap()
                .is_some()
        );
    }
}
