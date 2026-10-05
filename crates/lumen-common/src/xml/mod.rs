//! A streaming, namespace-aware, non-validating XML 1.0 parser with expat's behaviour: the same
//! event stream, error codes, error positions and DTD handling, but no I/O and no allocation
//! callbacks. It is engine-neutral: a language binding implements [`Handler`] and drives a
//! [`Parser`] (`lumen-py`'s `pyexpat` does; a JS `DOMParser` can too).
//!
//! Input is bytes in any supported encoding (UTF-8, UTF-16, ISO-8859-1, US-ASCII, or a single-byte
//! table the handler supplies for other names); text reaches the handler as UTF-8 `&str`.

mod chars;
mod dtd;
mod parser;
mod scan;

pub use chars::{is_name, is_name_char, is_name_start, is_xml_char};
pub use parser::{Parser, ParserKind};

use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// expat's `XML_Error` codes.
pub mod err {
    pub const NONE: u32 = 0;
    pub const NO_MEMORY: u32 = 1;
    pub const SYNTAX: u32 = 2;
    pub const NO_ELEMENTS: u32 = 3;
    pub const INVALID_TOKEN: u32 = 4;
    pub const UNCLOSED_TOKEN: u32 = 5;
    pub const PARTIAL_CHAR: u32 = 6;
    pub const TAG_MISMATCH: u32 = 7;
    pub const DUPLICATE_ATTRIBUTE: u32 = 8;
    pub const JUNK_AFTER_DOC_ELEMENT: u32 = 9;
    pub const PARAM_ENTITY_REF: u32 = 10;
    pub const UNDEFINED_ENTITY: u32 = 11;
    pub const RECURSIVE_ENTITY_REF: u32 = 12;
    pub const ASYNC_ENTITY: u32 = 13;
    pub const BAD_CHAR_REF: u32 = 14;
    pub const BINARY_ENTITY_REF: u32 = 15;
    pub const ATTRIBUTE_EXTERNAL_ENTITY_REF: u32 = 16;
    pub const MISPLACED_XML_PI: u32 = 17;
    pub const UNKNOWN_ENCODING: u32 = 18;
    pub const INCORRECT_ENCODING: u32 = 19;
    pub const UNCLOSED_CDATA_SECTION: u32 = 20;
    pub const EXTERNAL_ENTITY_HANDLING: u32 = 21;
    pub const NOT_STANDALONE: u32 = 22;
    pub const UNEXPECTED_STATE: u32 = 23;
    pub const ENTITY_DECLARED_IN_PE: u32 = 24;
    pub const FEATURE_REQUIRES_XML_DTD: u32 = 25;
    pub const CANT_CHANGE_FEATURE_ONCE_PARSING: u32 = 26;
    pub const UNBOUND_PREFIX: u32 = 27;
    pub const UNDECLARING_PREFIX: u32 = 28;
    pub const INCOMPLETE_PE: u32 = 29;
    pub const XML_DECL: u32 = 30;
    pub const TEXT_DECL: u32 = 31;
    pub const PUBLICID: u32 = 32;
    pub const SUSPENDED: u32 = 33;
    pub const NOT_SUSPENDED: u32 = 34;
    pub const ABORTED: u32 = 35;
    pub const FINISHED: u32 = 36;
    pub const SUSPEND_PE: u32 = 37;
    pub const RESERVED_PREFIX_XML: u32 = 38;
    pub const RESERVED_PREFIX_XMLNS: u32 = 39;
    pub const RESERVED_NAMESPACE_URI: u32 = 40;
    pub const INVALID_ARGUMENT: u32 = 41;
    pub const NO_BUFFER: u32 = 42;
    pub const AMPLIFICATION_LIMIT_BREACH: u32 = 43;
    pub const NOT_STARTED: u32 = 44;
}

/// `(constant name, message)` indexed by error code.
pub const ERRORS: [(&str, &str); 45] = [
    ("XML_ERROR_NONE", ""),
    ("XML_ERROR_NO_MEMORY", "out of memory"),
    ("XML_ERROR_SYNTAX", "syntax error"),
    ("XML_ERROR_NO_ELEMENTS", "no element found"),
    ("XML_ERROR_INVALID_TOKEN", "not well-formed (invalid token)"),
    ("XML_ERROR_UNCLOSED_TOKEN", "unclosed token"),
    ("XML_ERROR_PARTIAL_CHAR", "partial character"),
    ("XML_ERROR_TAG_MISMATCH", "mismatched tag"),
    ("XML_ERROR_DUPLICATE_ATTRIBUTE", "duplicate attribute"),
    ("XML_ERROR_JUNK_AFTER_DOC_ELEMENT", "junk after document element"),
    ("XML_ERROR_PARAM_ENTITY_REF", "illegal parameter entity reference"),
    ("XML_ERROR_UNDEFINED_ENTITY", "undefined entity"),
    ("XML_ERROR_RECURSIVE_ENTITY_REF", "recursive entity reference"),
    ("XML_ERROR_ASYNC_ENTITY", "asynchronous entity"),
    ("XML_ERROR_BAD_CHAR_REF", "reference to invalid character number"),
    ("XML_ERROR_BINARY_ENTITY_REF", "reference to binary entity"),
    ("XML_ERROR_ATTRIBUTE_EXTERNAL_ENTITY_REF", "reference to external entity in attribute"),
    ("XML_ERROR_MISPLACED_XML_PI", "XML or text declaration not at start of entity"),
    ("XML_ERROR_UNKNOWN_ENCODING", "unknown encoding"),
    ("XML_ERROR_INCORRECT_ENCODING", "encoding specified in XML declaration is incorrect"),
    ("XML_ERROR_UNCLOSED_CDATA_SECTION", "unclosed CDATA section"),
    ("XML_ERROR_EXTERNAL_ENTITY_HANDLING", "error in processing external entity reference"),
    ("XML_ERROR_NOT_STANDALONE", "document is not standalone"),
    ("XML_ERROR_UNEXPECTED_STATE", "unexpected parser state - please send a bug report"),
    ("XML_ERROR_ENTITY_DECLARED_IN_PE", "entity declared in parameter entity"),
    ("XML_ERROR_FEATURE_REQUIRES_XML_DTD", "requested feature requires XML_DTD support in Expat"),
    ("XML_ERROR_CANT_CHANGE_FEATURE_ONCE_PARSING", "cannot change setting once parsing has begun"),
    ("XML_ERROR_UNBOUND_PREFIX", "unbound prefix"),
    ("XML_ERROR_UNDECLARING_PREFIX", "must not undeclare prefix"),
    ("XML_ERROR_INCOMPLETE_PE", "incomplete markup in parameter entity"),
    ("XML_ERROR_XML_DECL", "XML declaration not well-formed"),
    ("XML_ERROR_TEXT_DECL", "text declaration not well-formed"),
    ("XML_ERROR_PUBLICID", "illegal character(s) in public id"),
    ("XML_ERROR_SUSPENDED", "parser suspended"),
    ("XML_ERROR_NOT_SUSPENDED", "parser not suspended"),
    ("XML_ERROR_ABORTED", "parsing aborted"),
    ("XML_ERROR_FINISHED", "parsing finished"),
    ("XML_ERROR_SUSPEND_PE", "cannot suspend in external parameter entity"),
    (
        "XML_ERROR_RESERVED_PREFIX_XML",
        "reserved prefix (xml) must not be undeclared or bound to another namespace name",
    ),
    ("XML_ERROR_RESERVED_PREFIX_XMLNS", "reserved prefix (xmlns) must not be declared or undeclared"),
    ("XML_ERROR_RESERVED_NAMESPACE_URI", "prefix must not be bound to one of the reserved namespace names"),
    ("XML_ERROR_INVALID_ARGUMENT", "invalid argument"),
    ("XML_ERROR_NO_BUFFER", "a successful prior call to function XML_GetBuffer is required"),
    (
        "XML_ERROR_AMPLIFICATION_LIMIT_BREACH",
        "limit on input amplification factor (from DTD and entities) breached",
    ),
    ("XML_ERROR_NOT_STARTED", "parser not started"),
];

/// `XML_ErrorString`.
pub fn error_string(code: u32) -> Option<&'static str> {
    match code {
        0 => None,
        c => ERRORS.get(c as usize).map(|e| e.1),
    }
}

/// What a handler callback tells the parser.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flow {
    /// No handler is installed for this event (the default handler may see it instead).
    Unset,
    /// Handled; keep parsing. For the boolean-returning callbacks: the handler answered "ok".
    Continue,
    /// Boolean-returning callbacks only: the handler answered "not ok".
    Fail,
    /// Stop parsing now (the handler raised an error); `parse` fails with `ABORTED`.
    Abort,
}

/// Which default handler is installed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DefaultMode {
    None,
    /// `XML_SetDefaultHandler`: internal entity references are not expanded.
    Raw,
    /// `XML_SetDefaultHandlerExpand`.
    Expand,
}

/// Content model node types and quantifiers (`XML_CTYPE_*`, `XML_CQUANT_*`).
pub mod model {
    pub const EMPTY: u8 = 1;
    pub const ANY: u8 = 2;
    pub const MIXED: u8 = 3;
    pub const NAME: u8 = 4;
    pub const CHOICE: u8 = 5;
    pub const SEQ: u8 = 6;
    pub const QUANT_NONE: u8 = 0;
    pub const QUANT_OPT: u8 = 1;
    pub const QUANT_REP: u8 = 2;
    pub const QUANT_PLUS: u8 = 3;
}

/// One node of an `<!ELEMENT>` content model. `nodes[0]` is the root; a node's children are
/// `nodes[first..first + count]` (expat's flat, breadth-first layout, so a tree of any depth needs no
/// recursion to build or to walk bottom-up).
#[derive(Clone, Debug)]
pub struct ModelNode {
    pub kind: u8,
    pub quant: u8,
    pub name: Option<String>,
    pub first: usize,
    pub count: usize,
}

#[derive(Clone, Debug)]
pub struct Model {
    pub nodes: Vec<ModelNode>,
}

/// An error from [`Parser::parse`]; the position is where expat would report it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XmlError {
    pub code: u32,
}

impl XmlError {
    pub fn message(&self) -> &'static str {
        error_string(self.code).unwrap_or("")
    }
}

/// Where the parser is: 1-based line, 0-based column (in characters), and byte offset into the
/// input (in the document's own encoding).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    pub line: u64,
    pub column: u64,
    pub byte: u64,
}

/// Parser settings fixed at creation.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// `Some(sep)` turns namespace processing on; qualified names are `uri<sep>local`.
    pub namespace_separator: Option<char>,
    /// With namespaces, report `uri<sep>local<sep>prefix`.
    pub namespace_prefixes: bool,
    /// Overrides the encoding of the input and any declaration in it.
    pub encoding: Option<String>,
}

/// State a binding reads and writes while a parse is running (callbacks may query the parser
/// they are called from).
pub struct Shared {
    pub(crate) pos: Cell<Position>,
    pub(crate) base: RefCell<Option<String>>,
    pub(crate) dtd: Rc<RefCell<dtd::Dtd>>,
    pub(crate) param_entity_parsing: Cell<u8>,
    pub(crate) use_foreign_dtd: Cell<bool>,
    pub(crate) reparse_deferral: Cell<bool>,
    pub(crate) started: Cell<bool>,
    pub(crate) account: Rc<Account>,
    pub(crate) error_pos: Cell<Position>,
    pub(crate) error_code: Cell<u32>,
    pub(crate) opts: Options,
    pub(crate) triplet: Cell<bool>,
    pub(crate) external: bool,
}

/// Amplification accounting shared by a root parser and its external entity parsers.
pub struct Account {
    pub(crate) direct: Cell<u64>,
    pub(crate) indirect: Cell<u64>,
    pub(crate) max_amplification: Cell<f32>,
    pub(crate) activation_threshold: Cell<u64>,
    pub(crate) alloc_max_amplification: Cell<f32>,
    pub(crate) alloc_activation_threshold: Cell<u64>,
}

impl Shared {
    pub(crate) fn new(dtd: Rc<RefCell<dtd::Dtd>>, account: Rc<Account>, opts: Options, external: bool) -> Shared {
        Shared {
            pos: Cell::new(Position { line: 1, column: 0, byte: 0 }),
            base: RefCell::new(None),
            dtd,
            param_entity_parsing: Cell::new(0),
            use_foreign_dtd: Cell::new(false),
            reparse_deferral: Cell::new(true),
            started: Cell::new(false),
            account,
            error_pos: Cell::new(Position { line: 1, column: 0, byte: 0 }),
            error_code: Cell::new(0),
            triplet: Cell::new(opts.namespace_prefixes),
            external,
            opts,
        }
    }

    /// Position of the event being reported (`XML_GetCurrentLineNumber` and friends).
    pub fn position(&self) -> Position {
        self.pos.get()
    }

    /// Where the last error occurred.
    pub fn error_position(&self) -> Position {
        self.error_pos.get()
    }

    pub fn error_code(&self) -> u32 {
        self.error_code.get()
    }

    pub fn base(&self) -> Option<String> {
        self.base.borrow().clone()
    }

    pub fn set_base(&self, base: Option<String>) {
        *self.base.borrow_mut() = base;
    }

    /// `XML_SetParamEntityParsing`; refused once parsing began.
    pub fn set_param_entity_parsing(&self, mode: u8) -> bool {
        if self.started.get() {
            return false;
        }
        self.param_entity_parsing.set(mode);
        true
    }

    /// `XML_UseForeignDTD`; `false` once parsing began.
    pub fn use_foreign_dtd(&self, use_dtd: bool) -> bool {
        if self.started.get() {
            return false;
        }
        self.use_foreign_dtd.set(use_dtd);
        true
    }

    /// `XML_SetReturnNSTriplet`; ignored once parsing began.
    pub fn set_namespace_prefixes(&self, on: bool) {
        if !self.started.get() {
            self.triplet.set(on);
        }
    }

    pub fn is_root(&self) -> bool {
        !self.external
    }

    pub fn reparse_deferral_enabled(&self) -> bool {
        self.reparse_deferral.get()
    }

    pub fn set_reparse_deferral_enabled(&self, on: bool) {
        self.reparse_deferral.set(on);
    }

    pub fn set_billion_laughs_threshold(&self, bytes: u64) {
        self.account.activation_threshold.set(bytes);
    }

    pub fn set_billion_laughs_max_amplification(&self, factor: f32) -> bool {
        if factor.is_nan() || factor < 1.0 {
            return false;
        }
        self.account.max_amplification.set(factor);
        true
    }

    pub fn set_alloc_tracker_threshold(&self, bytes: u64) {
        self.account.alloc_activation_threshold.set(bytes);
    }

    pub fn set_alloc_tracker_max_amplification(&self, factor: f32) -> bool {
        if factor.is_nan() || factor < 1.0 {
            return false;
        }
        self.account.alloc_max_amplification.set(factor);
        true
    }
}

impl Account {
    pub(crate) fn new() -> Account {
        Account {
            direct: Cell::new(0),
            indirect: Cell::new(0),
            max_amplification: Cell::new(100.0),
            activation_threshold: Cell::new(8 * 1024 * 1024),
            alloc_max_amplification: Cell::new(100.0),
            alloc_activation_threshold: Cell::new(64 * 1024 * 1024),
        }
    }
}

/// An attribute handed to [`Handler::start_element`].
#[derive(Clone, Debug)]
pub struct Attribute {
    pub name: String,
    pub value: String,
}

/// The events a parse produces. Every method has a default that reports "no handler"; the parser
/// then offers the event to [`Handler::default_text`] when one is installed, as expat does.
#[allow(unused_variables)]
pub trait Handler {
    fn xml_decl(&mut self, version: Option<&str>, encoding: Option<&str>, standalone: i32) -> Flow {
        Flow::Unset
    }
    fn start_doctype(&mut self, name: &str, sysid: Option<&str>, pubid: Option<&str>, has_internal_subset: bool) -> Flow {
        Flow::Unset
    }
    fn end_doctype(&mut self) -> Flow {
        Flow::Unset
    }
    fn element_decl(&mut self, name: &str, model: &Model) -> Flow {
        Flow::Unset
    }
    fn attlist_decl(&mut self, element: &str, attribute: &str, atype: &str, default: Option<&str>, required: bool) -> Flow {
        Flow::Unset
    }
    #[allow(clippy::too_many_arguments)]
    fn entity_decl(
        &mut self,
        name: &str,
        is_param: bool,
        value: Option<&str>,
        base: Option<&str>,
        sysid: Option<&str>,
        pubid: Option<&str>,
        notation: Option<&str>,
    ) -> Flow {
        Flow::Unset
    }
    /// The pre-`entity_decl` callback for unparsed (`NDATA`) entities; consulted first.
    fn unparsed_entity_decl(&mut self, name: &str, base: Option<&str>, sysid: Option<&str>, pubid: Option<&str>, notation: &str) -> Flow {
        Flow::Unset
    }
    fn notation_decl(&mut self, name: &str, base: Option<&str>, sysid: Option<&str>, pubid: Option<&str>) -> Flow {
        Flow::Unset
    }
    /// `attrs[..specified]` were written in the document; the rest come from `<!ATTLIST>` defaults.
    fn start_element(&mut self, name: &str, attrs: &[Attribute], specified: usize) -> Flow {
        Flow::Unset
    }
    fn end_element(&mut self, name: &str) -> Flow {
        Flow::Unset
    }
    fn chardata(&mut self, data: &str) -> Flow {
        Flow::Unset
    }
    fn comment(&mut self, data: &str) -> Flow {
        Flow::Unset
    }
    fn processing_instruction(&mut self, target: &str, data: &str) -> Flow {
        Flow::Unset
    }
    fn start_cdata(&mut self) -> Flow {
        Flow::Unset
    }
    fn end_cdata(&mut self) -> Flow {
        Flow::Unset
    }
    fn start_namespace(&mut self, prefix: Option<&str>, uri: Option<&str>) -> Flow {
        Flow::Unset
    }
    fn end_namespace(&mut self, prefix: Option<&str>) -> Flow {
        Flow::Unset
    }
    fn skipped_entity(&mut self, name: &str, is_param: bool) -> Flow {
        Flow::Unset
    }
    /// `Continue` = handled, `Fail` = report `EXTERNAL_ENTITY_HANDLING`.
    fn external_entity_ref(&mut self, context: Option<&str>, base: Option<&str>, sysid: Option<&str>, pubid: Option<&str>) -> Flow {
        Flow::Unset
    }
    /// `Fail` = report `NOT_STANDALONE`.
    fn not_standalone(&mut self) -> Flow {
        Flow::Unset
    }
    /// A 256-entry table for a single-byte encoding the parser does not know: a value `>= 0` is the
    /// code point of that byte, `-1` marks an invalid byte. `None` = unknown encoding.
    fn unknown_encoding(&mut self, name: &str) -> Option<Vec<i32>> {
        None
    }
    fn default_mode(&self) -> DefaultMode {
        DefaultMode::None
    }
    fn default_text(&mut self, text: &str) -> Flow {
        Flow::Unset
    }
}
