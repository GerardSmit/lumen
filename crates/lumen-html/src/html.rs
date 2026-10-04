//! HTML document ingestion into the shared DOM arena.
use crate::{Document, Error as DomError, Name, Namespace, NodeId, NodeKind};
use alloc::{
    borrow::Cow,
    string::{String, ToString},
    vec::Vec,
};

const MAX_HTML_BYTES: usize = 4 * 1024 * 1024;

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
    let mut output = String::with_capacity(input.len());
    let mut start = 0;
    for (index, byte) in input.bytes().enumerate() {
        if byte == 0 {
            output.push_str(&input[start..index]);
            output.push('\u{fffd}');
            start = index + 1;
        }
    }
    output.push_str(&input[start..]);
    Cow::Owned(output)
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
                | Tag::Dd
                | Tag::Dl
                | Tag::Dt
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
                | Tag::Li
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
            Tag::Iframe => "iframe",
            Tag::Noembed => "noembed",
            Tag::Noframes => "noframes",
            Tag::Xmp => "xmp",
            Tag::Title => "title",
            _ => "textarea",
        }
    }
}

fn ascii_lowercase(value: &str) -> Cow<'_, str> {
    if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(value.to_ascii_lowercase())
    } else {
        Cow::Borrowed(value)
    }
}

/// Longest named reference at the start of `input` (bytes after `&`), as
/// (consumed length, replacement). Walks the sorted table one byte at a time,
/// narrowing the candidate range instead of searching per prefix.
fn longest_entity(input: &[u8]) -> Option<(usize, &'static str)> {
    use crate::entities::{ENTRIES, NAMES, VALUES};
    let names = NAMES.as_bytes();
    let mut range = ENTRIES;
    let mut best = None;
    for (depth, &byte) in input.iter().take(32).enumerate() {
        if !byte.is_ascii_alphanumeric() && byte != b';' {
            break;
        }
        let at = |entry: &(u16, u8, u16, u8)| names[entry.0 as usize + depth];
        // Candidates share input[..depth]; the one equal to it (if any) sorts first.
        let first = range.partition_point(|entry| entry.1 as usize <= depth || at(entry) < byte);
        let rest = &range[first..];
        let count = rest.partition_point(|entry| at(entry) == byte);
        if count == 0 {
            break;
        }
        range = &rest[..count];
        let head = range[0];
        if head.1 as usize == depth + 1 {
            best = Some((
                depth + 1,
                &VALUES[head.2 as usize..head.2 as usize + head.3 as usize],
            ));
        }
        if byte == b';' {
            break;
        }
    }
    best
}

fn numeric_entity(input: &str) -> Option<(char, usize)> {
    let rest = input.strip_prefix('#')?;
    let (radix, rest, prefix) =
        if let Some(hex) = rest.strip_prefix('x').or_else(|| rest.strip_prefix('X')) {
            (16, hex, 2)
        } else {
            (10, rest, 1)
        };
    let digits = rest
        .bytes()
        .take_while(|byte| {
            if radix == 16 {
                byte.is_ascii_hexdigit()
            } else {
                byte.is_ascii_digit()
            }
        })
        .count();
    if digits == 0 {
        return None;
    }
    let mut value = u32::from_str_radix(&rest[..digits], radix).unwrap_or(0xfffd);
    const C1: [u32; 32] = [
        0x20ac, 0x81, 0x201a, 0x192, 0x201e, 0x2026, 0x2020, 0x2021, 0x2c6, 0x2030, 0x160, 0x2039,
        0x152, 0x8d, 0x17d, 0x8f, 0x90, 0x2018, 0x2019, 0x201c, 0x201d, 0x2022, 0x2013, 0x2014,
        0x2dc, 0x2122, 0x161, 0x203a, 0x153, 0x9d, 0x17e, 0x178,
    ];
    if (0x80..=0x9f).contains(&value) {
        value = C1[(value - 0x80) as usize];
    }
    let value = char::from_u32(value)
        .filter(|&value| value != '\0')
        .unwrap_or('\u{fffd}');
    let consumed = prefix + digits + usize::from(rest.as_bytes().get(digits) == Some(&b';'));
    Some((value, consumed))
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
        if let Some((end, value)) = longest_entity(after_amp.as_bytes()) {
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
    let (consumed, value) = longest_entity(reference.as_bytes())?;
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

fn serializes_void(name: &str) -> bool {
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

fn serialize_into(
    document: &Document,
    root: NodeId,
    children_only: bool,
    output: &mut String,
) -> Result<(), DomError> {
    let mut pending = Vec::new();
    if children_only {
        if matches!(document.kind(root)?, NodeKind::Element { name, .. } if serializes_void(name.as_str()))
        {
            return Ok(());
        }
        let raw = matches!(document.kind(root)?, NodeKind::Element { name, .. } if serializes_text_literally(name.as_str()));
        let mut child = document.last_child(document.template_content(root)?.unwrap_or(root))?;
        while let Some(id) = child {
            pending.push((id, false, raw));
            child = document.previous_sibling(id)?;
        }
    } else {
        pending.push((root, false, false));
    }
    while let Some((id, closing, raw_text)) = pending.pop() {
        let kind = document.kind(id)?;
        if closing {
            if let NodeKind::Element { name, .. } = kind {
                output.push_str("</");
                output.push_str(name);
                output.push('>');
            }
            continue;
        }
        let mut descend = true;
        let mut child_raw = raw_text;
        match kind {
            NodeKind::Document | NodeKind::DocumentFragment => {}
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
                name, attributes, ..
            } => {
                output.push('<');
                output.push_str(name);
                for (key, value) in attributes {
                    output.push(' ');
                    output.push_str(key);
                    output.push_str("=\"");
                    escape(output, value, true);
                    output.push('"');
                }
                output.push('>');
                descend = !serializes_void(name.as_str());
                child_raw = serializes_text_literally(name.as_str());
                if descend {
                    pending.push((id, true, raw_text));
                }
            }
            NodeKind::Text(value) => {
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
            let mut child = document.last_child(document.template_content(id)?.unwrap_or(id))?;
            while let Some(next) = child {
                pending.push((next, false, child_raw));
                child = document.previous_sibling(next)?;
            }
        }
    }
    Ok(())
}

pub fn outer_html(document: &Document, id: NodeId) -> Result<String, DomError> {
    let mut output = String::new();
    serialize_into(document, id, false, &mut output)?;
    Ok(output)
}

pub fn inner_html(document: &Document, id: NodeId) -> Result<String, DomError> {
    let mut output = String::new();
    serialize_into(document, id, true, &mut output)?;
    Ok(output)
}

/// Parse an application document. Scripts are retained as inert text.
pub fn parse(input: &str, max_nodes: usize) -> Result<Document, ParseError> {
    parse_with_declarative_shadow_roots(input, max_nodes, false)
}

/// Parse a document with the embedding owner's declarative-shadow permission.
/// Ordinary fragment/DOMParser entry points keep this permission disabled.
pub fn parse_with_declarative_shadow_roots(
    input: &str,
    max_nodes: usize,
    allow_declarative_shadow_roots: bool,
) -> Result<Document, ParseError> {
    if input.len() > MAX_HTML_BYTES {
        return Err(error(0, "HTML input too large"));
    }
    let input = normalized_input(input);
    let mut document = Document::new(max_nodes);
    document.set_html_document(true);
    // HTML documents without a doctype begin in quirks mode; an initial
    // doctype token below replaces this with the mode selected by the standard.
    document.set_document_mode(crate::DocumentMode::Quirks);
    let root = document.root();
    let html = document
        .create(element("html", Vec::new()))
        .map_err(|e| dom_error(0, e))?;
    let head = document
        .create(element("head", Vec::new()))
        .map_err(|e| dom_error(0, e))?;
    let body = document
        .create(element("body", Vec::new()))
        .map_err(|e| dom_error(0, e))?;
    document.attach_detached(root, html);
    document.attach_detached(html, head);
    document.attach_detached(html, body);
    Parser {
        input: &input,
        pos: 0,
        document: &mut document,
        html: Some(html),
        head: Some(head),
        body,
        stack: Vec::with_capacity(32),
        formatting: Vec::new(),
        scratch: Vec::new(),
        template_modes: Vec::new(),
        declarative_roots: Vec::new(),
        declarative_cleanup_pending: false,
        allow_declarative_shadow_roots,
        custom_selects: Vec::new(),
        fragment_context: None,
        fragment_context_kind: None,
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
    .run()?;
    Ok(document)
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
    let html = document
        .create(element("html", Vec::new()))
        .map_err(|e| dom_error(0, e))?;
    let head = document
        .create(element("head", Vec::new()))
        .map_err(|e| dom_error(0, e))?;
    let body = document
        .create(element("body", Vec::new()))
        .map_err(|e| dom_error(0, e))?;
    document.attach_detached(root, html);
    document.attach_detached(html, head);
    document.attach_detached(html, body);
    let mut parser = Parser {
        input: &input,
        pos: 0,
        document: &mut document,
        html: Some(html),
        head: Some(head),
        body,
        stack: Vec::with_capacity(32),
        formatting: Vec::new(),
        scratch: Vec::new(),
        template_modes: Vec::new(),
        declarative_roots: Vec::new(),
        declarative_cleanup_pending: false,
        allow_declarative_shadow_roots: false,
        custom_selects: Vec::new(),
        fragment_context: None,
        fragment_context_kind: None,
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
    };
    parser.run()?;
    Ok(parser.conformance_tokens.take().unwrap_or_default())
}

/// Parse a detached fragment in an existing arena for template reuse.
pub fn parse_fragment(document: &mut Document, input: &str) -> Result<NodeId, ParseError> {
    parse_fragment_context(document, input, None, None)
}

/// Parse markup using the tokenizer and table context of an HTML element.
pub fn parse_fragment_in(
    document: &mut Document,
    context: NodeId,
    input: &str,
) -> Result<NodeId, ParseError> {
    let kind = document.kind(context).map_err(|e| dom_error(0, e))?.clone();
    if !matches!(&kind, NodeKind::Element { .. }) {
        return Err(error(0, "fragment context must be an element"));
    }
    let mut ancestor = Some(context);
    let mut form_element = None;
    while let Some(id) = ancestor {
        if matches!(
            document.kind(id),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "form"
        ) {
            form_element = Some(id);
            break;
        }
        ancestor = document.parent(id).map_err(|error| dom_error(0, error))?;
    }
    parse_fragment_context(document, input, Some(kind), form_element)
}

fn parse_fragment_context(
    document: &mut Document,
    input: &str,
    context: Option<NodeKind>,
    form_element: Option<NodeId>,
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
            _ => None,
        },
        _ => None,
    };
    let mut stack = Vec::with_capacity(32);
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
        let head_id = match document.create(element("head", Vec::new())) {
            Ok(id) => id,
            Err(error) => {
                document
                    .destroy_subtree(fragment)
                    .map_err(|e| dom_error(0, e))?;
                return Err(dom_error(0, error));
            }
        };
        document.attach_detached(fragment, head_id);
        let body_id = match document.create(element("body", Vec::new())) {
            Ok(id) => id,
            Err(error) => {
                document
                    .destroy_subtree(fragment)
                    .map_err(|e| dom_error(0, e))?;
                return Err(dom_error(0, error));
            }
        };
        document.attach_detached(fragment, body_id);
        head = Some(head_id);
        body = body_id;
    }
    let result = Parser {
        input: &input,
        pos: 0,
        document,
        html: None,
        head,
        body,
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
        allow_declarative_shadow_roots: false,
        custom_selects: Vec::new(),
        // A detached fragment has no context element. Treating `Other` as a
        // context tag made the first unknown element in a fragment impossible
        // to close (for example, sibling `<slot>` elements).
        fragment_context,
        fragment_context_kind: context.clone(),
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
    InCell,
    InBody,
}

struct Parser<'a, 'd> {
    input: &'a str,
    pos: usize,
    document: &'d mut Document,
    html: Option<NodeId>,
    head: Option<NodeId>,
    body: NodeId,
    stack: Vec<Open>,
    formatting: Vec<Option<Open>>,
    scratch: Vec<(Name, String)>,
    template_modes: Vec<TemplateMode>,
    // Only declarative templates need this indirection. Their temporary
    // stack element remains detached while children go straight to the root.
    declarative_roots: Vec<(NodeId, NodeId)>,
    declarative_cleanup_pending: bool,
    allow_declarative_shadow_roots: bool,
    custom_selects: Vec<NodeId>,
    fragment_context: Option<Tag>,
    // The HTML fragment algorithm keeps the context element outside the
    // returned fragment while still using its namespace and integration-point
    // state as the adjusted current node.
    fragment_context_kind: Option<NodeKind>,
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

impl<'a> Parser<'a, '_> {
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
        let bytes = self.remaining().as_bytes();
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
            let id = self
                .document
                .clone_shallow(old.id)
                .map_err(|e| dom_error(self.pos, e))?;
            let (parent, before) = self.insertion_location(self.parent(), self.parent_tag());
            self.document.insert_detached_before(parent, id, before);
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
                self.stack.truncate(open);
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
                    self.stack.remove(position);
                    continue;
                };
                let copy = self
                    .document
                    .clone_shallow(node.id)
                    .map_err(|e| dom_error(self.pos, e))?;
                self.append(ancestor, copy);
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
                self.append(copy, last);
                last = copy;
            }
            self.document
                .detach(last)
                .map_err(|e| dom_error(self.pos, e))?;
            let (parent, before) = self.insertion_location(ancestor, ancestor_tag);
            self.document.insert_detached_before(parent, last, before);
            let copy = self
                .document
                .clone_shallow(id)
                .map_err(|e| dom_error(self.pos, e))?;
            while let Some(child) = self
                .document
                .first_child(furthest)
                .map_err(|e| dom_error(self.pos, e))?
            {
                self.document
                    .detach(child)
                    .map_err(|e| dom_error(self.pos, e))?;
                self.append(copy, child);
            }
            self.append(furthest, copy);
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
            self.stack.remove(open);
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
            self.stack.remove(index);
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
        let id = self
            .document
            .create(NodeKind::Element {
                namespace,
                name: Name::new(element_name),
                attributes,
            })
            .map_err(|e| dom_error(offset, e))?;
        for (name, uri) in foreign_namespaces {
            self.document
                .set_attribute_namespace_metadata(id, &name, Some(uri))
                .map_err(|e| dom_error(offset, e))?;
        }
        let (parent, before) = self.insertion_location(self.parent(), self.parent_tag());
        self.document.insert_detached_before(parent, id, before);
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
            self.stack.pop();
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
                Some(TemplateMode::InCell) => Some(Tag::Td),
                _ => None,
            };
        }
        if self.fragment_context == Some(Tag::Template) {
            return match self.template_modes.last() {
                Some(TemplateMode::InTable) => Some(Tag::Table),
                Some(TemplateMode::InColumnGroup) => Some(Tag::Colgroup),
                Some(TemplateMode::InTableBody) => Some(Tag::Tbody),
                Some(TemplateMode::InRow) => Some(Tag::Tr),
                Some(TemplateMode::InCell) => Some(Tag::Td),
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

    fn append(&mut self, parent: NodeId, child: NodeId) {
        let parent = self.template_content(parent).unwrap_or(parent);
        self.document.attach_detached(parent, child);
    }

    fn append_comment(&mut self, child: NodeId, offset: usize) -> Result<(), ParseError> {
        if let Some(html) = self.html.filter(|_| self.document_tail != 0) {
            let parent = if self.document_tail == 3 {
                self.document.root()
            } else {
                html
            };
            let before = if self.document_tail == 1 {
                Some(self.body)
            } else {
                None
            };
            self.document
                .insert_before(parent, child, before)
                .map_err(|error| dom_error(offset, error))?;
        } else if let Some(html) = self
            .html
            .filter(|_| self.after_head && self.stack.is_empty())
        {
            self.document
                .insert_before(html, child, Some(self.body))
                .map_err(|error| dom_error(offset, error))?;
        } else if self.html.is_none()
            && self.fragment_context == Some(Tag::Html)
            && self.document_tail == 3
            && self.stack.len() == 1
        {
            self.document
                .insert_before(self.stack[0].id, child, None)
                .map_err(|error| dom_error(offset, error))?;
        } else if let Some(html) = self
            .html
            .filter(|_| !self.body_started && !self.in_head && self.stack.is_empty())
        {
            let (parent, before) = if self.html_started {
                (html, self.head)
            } else {
                (self.document.root(), Some(html))
            };
            self.document
                .insert_before(parent, child, before)
                .map_err(|error| dom_error(offset, error))?;
        } else {
            self.append(self.parent(), child);
        }
        Ok(())
    }

    fn append_text(&mut self, value: Cow<'_, str>, offset: usize) -> Result<(), ParseError> {
        if value.is_empty() {
            return Ok(());
        }
        let blank = value.bytes().all(|byte| byte.is_ascii_whitespace());
        let mut parent = self.parent();
        let mut tag = self.parent_tag();
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
            if let NodeKind::Text(existing) = &mut self.document.node_mut(last).kind {
                existing.push_str(&value);
                return Ok(());
            }
        }
        let id = self
            .document
            .create(NodeKind::Text(value.into_owned()))
            .map_err(|e| dom_error(offset, e))?;
        self.document.insert_detached_before(parent, id, before);
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
                    | TemplateMode::InCell
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
            self.template_modes
                .truncate(self.template_modes.len().saturating_sub(removed_templates));
        }
        self.custom_selects
            .retain(|select| !removed_selects.contains(select));
        self.stack.truncate(index);
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
            self.stack.pop();
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
            self.stack.pop();
        }
    }

    fn merge_scaffold_attributes(&mut self, id: NodeId, attributes: Vec<(Name, String)>) {
        if let NodeKind::Element {
            attributes: existing,
            ..
        } = &mut self.document.node_mut(id).kind
        {
            for (key, value) in attributes {
                if !existing.iter().any(|(name, _)| name == &key) {
                    existing.push((key, value));
                }
            }
        }
    }

    fn create_plain(&mut self, name: &'static str, offset: usize) -> Result<NodeId, ParseError> {
        self.document
            .create(element(name, Vec::new()))
            .map_err(|e| dom_error(offset, e))
    }

    fn run(&mut self) -> Result<(), ParseError> {
        let input = self.input;
        while self.pos < input.len() {
            if self.initial_text_token()? {
                continue;
            }
            if self.at_fragment_context() {
                if let Some(rcdata) = self.fragment_text_mode {
                    let raw = replace_nulls(&input[self.pos..]);
                    let text = if rcdata {
                        decode_entities(raw.as_ref(), false)
                    } else {
                        raw
                    };
                    self.emit_conformance_token(ConformanceToken::Character(text.to_string()));
                    self.append_text(text, self.pos)?;
                    self.pos = input.len();
                    continue;
                }
            }
            if let Some(&open) = self.stack.last() {
                if open.tag == Tag::Plaintext {
                    let text = replace_nulls(&input[self.pos..]);
                    self.emit_conformance_token(ConformanceToken::Character(text.to_string()));
                    self.append_text(text, self.pos)?;
                    self.pos = input.len();
                    continue;
                }
                if open.tag.is_raw_text() {
                    let end = self.raw_text_end(open.tag.raw_text_name());
                    if end > 0 {
                        let raw = replace_nulls(&input[self.pos..self.pos + end]);
                        let text = if open.tag.is_rcdata() {
                            decode_entities(&raw, false)
                        } else {
                            raw
                        };
                        self.emit_conformance_token(ConformanceToken::Character(text.to_string()));
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
                self.end_tag()?;
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
        }
        self.refresh_selected_content()?;
        // The spec's parser-only template stack elements must not retain an
        // extra template/content pair in the document arena after parsing.
        self.reclaim_declarative_templates(true)?;
        Ok(())
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
                    self.document.attach_detached(target, clone);
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
        self.emit_conformance_token(ConformanceToken::Comment(data.clone()));
        let id = self
            .document
            .create(NodeKind::Comment(data))
            .map_err(|e| dom_error(self.pos, e))?;
        self.append_comment(id, self.pos)?;
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
        self.emit_conformance_token(ConformanceToken::Character(text.to_string()));
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
        let id = self
            .document
            .create(NodeKind::ProcessingInstruction {
                target: target.to_string(),
                data,
            })
            .map_err(|e| dom_error(start, e))?;
        self.append_comment(id, start)?;
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
        let id = self
            .document
            .create(NodeKind::Comment(data))
            .map_err(|e| dom_error(start, e))?;
        self.append_comment(id, start)?;
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
            .insert_before(self.document.root(), id, self.html)
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
                let id = self
                    .document
                    .create(NodeKind::Comment(data))
                    .map_err(|e| dom_error(start, e))?;
                self.append_comment(id, start)?;
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
        self.emit_conformance_token(ConformanceToken::EndTag(name.to_string()));
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
        if self.html.is_none() && self.fragment_context == Some(Tag::Html) && tag == Tag::Html {
            self.document_tail = 3;
            return Ok(());
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
                self.document.insert_detached_before(parent, id, before);
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
                    self.document.insert_detached_before(parent, id, before);
                } else {
                    self.close_in_scope(&[Tag::P], &boundary);
                }
            }
            Tag::Head if self.html.is_some() => {
                self.in_head = false;
                self.body_started = false;
                self.after_head = true;
                self.stack.clear();
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
                        self.stack.remove(index);
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
                    self.merge_scaffold_attributes(self.html.unwrap_or(self.body), attributes);
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
                    self.merge_scaffold_attributes(target, attributes);
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
                    self.merge_scaffold_attributes(self.html.unwrap(), attributes);
                    return Ok(());
                }
                Tag::Head => {
                    if self.body_started || self.after_head {
                        return Ok(());
                    }
                    self.merge_scaffold_attributes(self.head.unwrap(), attributes);
                    self.in_head = true;
                    self.after_head = false;
                    return Ok(());
                }
                Tag::Body => {
                    self.merge_scaffold_attributes(self.body, attributes);
                    self.in_head = false;
                    self.body_started = true;
                    self.after_head = false;
                    self.stack.clear();
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
                self.stack.pop();
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
                    self.append(parent, row);
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
                    self.append(parent, group);
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
                    Some(crate::shadow::ShadowOptions {
                        mode,
                        delegates_focus: has("shadowrootdelegatesfocus"),
                        clonable: has("shadowrootclonable"),
                        serializable: has("shadowrootserializable"),
                        declarative: true,
                    })
                })
        } else {
            None
        };
        let id = self
            .document
            .create(element(element_name, attributes))
            .map_err(|e| dom_error(start, e))?;
        let shadow = if let Some(options) = declarative {
            let host = self.parent();
            if self.html == Some(host) {
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
        if let Some(root) = shadow {
            self.declarative_roots
                .try_reserve(1)
                .map_err(|_| error(start, "parser allocation limit"))?;
            self.declarative_roots.push((id, root));
        } else {
            self.document.insert_detached_before(parent, id, before);
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
            self.append(parent, tbody);
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
        self.emit_conformance_token(ConformanceToken::Character(
            decode_entities(source, false).into_owned(),
        ));
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
                        Some(self.body),
                        start,
                    );
                }
                let split = content.bytes().take_while(u8::is_ascii_whitespace).count();
                if split != 0 {
                    self.append_text_at(
                        decode_entities(&content[..split], false),
                        self.html.unwrap_or(self.body),
                        Some(self.body),
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
        assert_eq!(inner_html(&document, document.root()).unwrap(),
            "<!--first--><!DOCTYPE html><!--?xml x--><html><!--second--><head><title>T</title></head><body></body></html>");
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
        assert_eq!(inner_html(&document, document.root()).unwrap(),
            "<?before ?><html><head></head><?between ?><body><?good data??><p><button><div>X</div></button><button>Y</button></p></body><?after ?></html><?last ?>");
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
}
