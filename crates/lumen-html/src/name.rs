//! Interned element and attribute names.
use alloc::{rc::Rc, string::String};
use core::{
    borrow::Borrow,
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    ops::Deref,
};

/// Sorted by byte order; `Name::new` binary-searches it.
static ATOMS: &[&str] = &[
    "a",
    "abbr",
    "accept",
    "accept-charset",
    "accesskey",
    "acronym",
    "action",
    "address",
    "align",
    "alink",
    "allow",
    "allowfullscreen",
    "alt",
    "area",
    "aria-checked",
    "aria-controls",
    "aria-current",
    "aria-describedby",
    "aria-disabled",
    "aria-expanded",
    "aria-hidden",
    "aria-label",
    "aria-labelledby",
    "aria-live",
    "aria-selected",
    "article",
    "aside",
    "async",
    "audio",
    "autocapitalize",
    "autocomplete",
    "autofocus",
    "autoplay",
    "axis",
    "b",
    "background",
    "base",
    "bdi",
    "bdo",
    "bgcolor",
    "blockquote",
    "body",
    "border",
    "bordercolor",
    "br",
    "button",
    "canvas",
    "caption",
    "cellpadding",
    "cellspacing",
    "char",
    "charoff",
    "charset",
    "checked",
    "circle",
    "cite",
    "class",
    "clear",
    "clippath",
    "code",
    "codebase",
    "col",
    "colgroup",
    "color",
    "cols",
    "colspan",
    "compact",
    "content",
    "contenteditable",
    "controls",
    "coords",
    "crossorigin",
    "cx",
    "cy",
    "d",
    "data",
    "data-id",
    "data-index",
    "data-key",
    "data-state",
    "data-testid",
    "data-value",
    "datalist",
    "datetime",
    "dd",
    "decoding",
    "default",
    "defer",
    "defs",
    "del",
    "details",
    "dfn",
    "dialog",
    "dir",
    "dirname",
    "disabled",
    "div",
    "dl",
    "download",
    "draggable",
    "dt",
    "ellipse",
    "em",
    "embed",
    "enctype",
    "enterkeyhint",
    "face",
    "fieldset",
    "figcaption",
    "figure",
    "fill",
    "footer",
    "for",
    "foreignobject",
    "form",
    "formaction",
    "formenctype",
    "formmethod",
    "formnovalidate",
    "formtarget",
    "frameborder",
    "g",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "headers",
    "height",
    "hgroup",
    "hidden",
    "high",
    "hr",
    "href",
    "hreflang",
    "hspace",
    "html",
    "http-equiv",
    "i",
    "id",
    "iframe",
    "image",
    "imagesizes",
    "imagesrcset",
    "img",
    "inert",
    "input",
    "inputmode",
    "ins",
    "integrity",
    "is",
    "ismap",
    "itemid",
    "itemprop",
    "itemref",
    "itemscope",
    "itemtype",
    "kbd",
    "kind",
    "label",
    "lang",
    "language",
    "legend",
    "li",
    "line",
    "lineargradient",
    "link",
    "list",
    "loading",
    "loop",
    "low",
    "main",
    "map",
    "marginheight",
    "marginwidth",
    "mark",
    "mask",
    "math",
    "max",
    "maxlength",
    "media",
    "menu",
    "meta",
    "meter",
    "method",
    "mi",
    "min",
    "minlength",
    "mn",
    "mo",
    "mrow",
    "ms",
    "mtext",
    "multiple",
    "muted",
    "name",
    "nav",
    "nomodule",
    "nonce",
    "noscript",
    "noshade",
    "novalidate",
    "nowrap",
    "object",
    "offset",
    "ol",
    "onblur",
    "onchange",
    "onclick",
    "onerror",
    "onfocus",
    "oninput",
    "onkeydown",
    "onkeyup",
    "onload",
    "onmouseout",
    "onmouseover",
    "onsubmit",
    "opacity",
    "open",
    "optgroup",
    "optimum",
    "option",
    "output",
    "p",
    "param",
    "part",
    "path",
    "pattern",
    "picture",
    "ping",
    "placeholder",
    "playsinline",
    "points",
    "polygon",
    "polyline",
    "popover",
    "poster",
    "pre",
    "preload",
    "preserveaspectratio",
    "progress",
    "q",
    "r",
    "radialgradient",
    "readonly",
    "rect",
    "referrerpolicy",
    "rel",
    "required",
    "rev",
    "reversed",
    "role",
    "rows",
    "rowspan",
    "rp",
    "rt",
    "ruby",
    "rules",
    "rx",
    "ry",
    "s",
    "samp",
    "sandbox",
    "scheme",
    "scope",
    "script",
    "scrolling",
    "search",
    "section",
    "select",
    "selected",
    "shape",
    "size",
    "sizes",
    "slot",
    "small",
    "source",
    "span",
    "spellcheck",
    "src",
    "srcdoc",
    "srclang",
    "srcset",
    "start",
    "step",
    "stop",
    "stroke",
    "stroke-width",
    "strong",
    "style",
    "sub",
    "summary",
    "sup",
    "svg",
    "symbol",
    "tabindex",
    "table",
    "target",
    "tbody",
    "td",
    "template",
    "text",
    "textarea",
    "tfoot",
    "th",
    "thead",
    "time",
    "title",
    "tr",
    "track",
    "transform",
    "translate",
    "tspan",
    "type",
    "u",
    "ul",
    "use",
    "usemap",
    "valign",
    "value",
    "valuetype",
    "var",
    "version",
    "video",
    "viewbox",
    "vlink",
    "vspace",
    "wbr",
    "width",
    "wrap",
    "x",
    "x1",
    "x2",
    "xlink:href",
    "xml:lang",
    "xmlns",
    "xmlns:xlink",
    "y",
    "y1",
    "y2",
];

/// An element or attribute name: an index into a static table for well-known
/// names, a shared string for the rest. Cloning never copies the text and
/// equal known names compare as integers.
#[derive(Clone)]
pub enum Name {
    Atom(u16),
    Heap(Rc<str>),
}

impl Name {
    pub fn new(name: &str) -> Self {
        match ATOMS.binary_search_by(|probe| probe.as_bytes().cmp(name.as_bytes())) {
            Ok(index) => Self::Atom(index as u16),
            Err(_) => Self::Heap(Rc::from(name)),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Atom(index) => ATOMS[*index as usize],
            Self::Heap(name) => name,
        }
    }
}

impl Deref for Name {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for Name {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq for Name {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Atom(a), Self::Atom(b)) => a == b,
            (Self::Heap(a), Self::Heap(b)) => Rc::ptr_eq(a, b) || a == b,
            _ => false,
        }
    }
}

impl Eq for Name {}

impl PartialEq<str> for Name {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Name {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for Name {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other.as_str()
    }
}

impl PartialEq<Name> for str {
    fn eq(&self, other: &Name) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<Name> for &str {
    fn eq(&self, other: &Name) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<Name> for String {
    fn eq(&self, other: &Name) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Hash for Name {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialOrd for Name {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Name {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl From<&str> for Name {
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

impl From<&String> for Name {
    fn from(name: &String) -> Self {
        Self::new(name)
    }
}

impl From<String> for Name {
    fn from(name: String) -> Self {
        match ATOMS.binary_search_by(|probe| probe.as_bytes().cmp(name.as_bytes())) {
            Ok(index) => Self::Atom(index as u16),
            Err(_) => Self::Heap(Rc::from(name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atoms_are_sorted_and_unique() {
        assert!(ATOMS.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(ATOMS.len() <= u16::MAX as usize);
    }

    #[test]
    fn known_names_share_identity_and_unknown_names_compare_by_text() {
        assert!(matches!(Name::new("div"), Name::Atom(_)));
        assert_eq!(Name::new("div"), Name::from(String::from("div")));
        let custom = Name::new("my-widget");
        assert!(matches!(custom, Name::Heap(_)));
        assert_eq!(custom, Name::new("my-widget"));
        assert_ne!(custom, Name::new("div"));
        assert_eq!(custom, "my-widget");
        assert_eq!(custom.as_str(), "my-widget");
    }
}
