//! The header list behind `Headers`, and the `HeadersInit` conversion.
//!
//! [`HeadersData`] is shared by the `Headers` object and by the `Request` / `Response` that owns
//! it, so a request's headers are read without a script-visible object. The guard decides which
//! mutations are silently dropped (forbidden request headers in a browsing context) or refused
//! (an immutable response).

use super::is_token;
use crate::webidl::usv_string;
use lumen::embed::{ArgCx, Ctx, JsHost, OpError, OpResult, Slot, Value};
use lumen::embed::bind::FromArg;
use lumen_common::cors::{is_forbidden_request_header, is_no_cors_safelisted_header};
use std::{cell::RefCell, rc::Rc};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Guard {
    None,
    Request,
    RequestNoCors,
    Response,
    Immutable,
}

/// The reasons a header cannot be added.
pub(crate) enum HeaderError {
    Name(String),
    Value,
    Immutable,
}

impl HeaderError {
    pub(crate) fn into_op(self) -> OpError {
        match self {
            HeaderError::Name(name) => OpError::type_error(format!("invalid header name '{name}'")),
            HeaderError::Value => OpError::type_error("invalid header value"),
            HeaderError::Immutable => OpError::type_error("immutable Headers object"),
        }
    }
}

pub(crate) type SharedHeaders = Rc<RefCell<HeadersData>>;

/// An ordered header list; names are stored lowercase.
#[derive(Clone)]
pub(crate) struct HeadersData {
    list: Vec<(String, String)>,
    pub(crate) guard: Guard,
    version: u64,
}

fn is_http_space(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | b'\r' | b' ')
}

/// Strip leading and trailing HTTP whitespace.
pub(crate) fn normalize_value(value: &str) -> &str {
    let bytes = value.as_bytes();
    let start = bytes.iter().position(|byte| !is_http_space(*byte)).unwrap_or(bytes.len());
    let end = bytes.iter().rposition(|byte| !is_http_space(*byte)).map_or(start, |at| at + 1);
    &value[start..end]
}

fn is_valid_value(value: &str) -> bool {
    !value.bytes().any(|byte| matches!(byte, 0 | b'\r' | b'\n'))
}

fn is_forbidden_response_header(name: &str) -> bool {
    matches!(name, "set-cookie" | "set-cookie2")
}

fn is_privileged_no_cors(name: &str) -> bool {
    name == "range"
}

impl HeadersData {
    pub(crate) fn new(guard: Guard) -> HeadersData {
        HeadersData {
            list: Vec::new(),
            guard,
            version: 0,
        }
    }

    pub(crate) fn shared(guard: Guard) -> SharedHeaders {
        Rc::new(RefCell::new(HeadersData::new(guard)))
    }

    /// A list built from transport header pairs; pairs that are not valid headers are skipped.
    pub(crate) fn from_pairs(pairs: &[(String, String)], guard: Guard) -> HeadersData {
        let mut data = HeadersData::new(guard);
        for (name, value) in pairs {
            let value = normalize_value(value);
            if is_token(name) && is_valid_value(value) {
                data.list.push((name.to_ascii_lowercase(), value.to_owned()));
            }
        }
        data
    }

    pub(crate) fn version(&self) -> u64 {
        self.version
    }

    fn changed(&mut self) {
        self.version += 1;
    }

    /// The list with another guard and no filtering: a copy of a request's or response's headers.
    pub(crate) fn copy_with(&self, guard: Guard) -> HeadersData {
        HeadersData {
            list: self.list.clone(),
            guard,
            version: 0,
        }
    }

    fn combined(&self, lower: &str) -> Option<String> {
        let mut values = self
            .list
            .iter()
            .filter(|(name, _)| name == lower)
            .map(|(_, value)| value.as_str());
        let first = values.next()?;
        let mut combined = first.to_owned();
        for value in values {
            combined.push_str(", ");
            combined.push_str(value);
        }
        Some(combined)
    }

    /// The combined value of `name`, `None` when absent.
    pub(crate) fn get(&self, name: &str) -> Result<Option<String>, HeaderError> {
        if !is_token(name) {
            return Err(HeaderError::Name(name.to_owned()));
        }
        Ok(self.combined(&name.to_ascii_lowercase()))
    }

    /// The combined value of a name known to be valid and lowercase.
    pub(crate) fn value_of(&self, lower: &str) -> Option<String> {
        self.combined(lower)
    }

    pub(crate) fn has(&self, name: &str) -> Result<bool, HeaderError> {
        if !is_token(name) {
            return Err(HeaderError::Name(name.to_owned()));
        }
        let lower = name.to_ascii_lowercase();
        Ok(self.list.iter().any(|(known, _)| *known == lower))
    }

    /// Whether the guard drops a header named `lower` with `value`.
    fn dropped(&self, lower: &str, value: &str) -> bool {
        match self.guard {
            Guard::Request => is_forbidden_request_header(lower),
            Guard::RequestNoCors => {
                let combined = match self.combined(lower) {
                    Some(existing) => format!("{existing}, {value}"),
                    None => value.to_owned(),
                };
                !is_no_cors_safelisted_header(lower, &combined)
            }
            Guard::Response => is_forbidden_response_header(lower),
            Guard::None | Guard::Immutable => false,
        }
    }

    fn check(&self, name: &str, value: &str) -> Result<(), HeaderError> {
        if !is_token(name) {
            return Err(HeaderError::Name(name.to_owned()));
        }
        if !is_valid_value(value) {
            return Err(HeaderError::Value);
        }
        if self.guard == Guard::Immutable {
            return Err(HeaderError::Immutable);
        }
        Ok(())
    }

    pub(crate) fn append(&mut self, name: &str, value: &str) -> Result<(), HeaderError> {
        let value = normalize_value(value);
        self.check(name, value)?;
        let lower = name.to_ascii_lowercase();
        if self.dropped(&lower, value) {
            return Ok(());
        }
        self.list.push((lower, value.to_owned()));
        self.changed();
        Ok(())
    }

    pub(crate) fn set(&mut self, name: &str, value: &str) -> Result<(), HeaderError> {
        let value = normalize_value(value);
        self.check(name, value)?;
        let lower = name.to_ascii_lowercase();
        let existing = self.list.iter().any(|(known, _)| *known == lower);
        if existing && self.guard == Guard::RequestNoCors {
            if !is_no_cors_safelisted_header(&lower, value) {
                return Ok(());
            }
        } else if self.dropped(&lower, value) {
            return Ok(());
        }
        let mut replacement = Some(value.to_owned());
        self.list.retain_mut(|(known, current)| {
            if *known != lower {
                return true;
            }
            match replacement.take() {
                Some(value) => {
                    *current = value;
                    true
                }
                None => false,
            }
        });
        if let Some(value) = replacement {
            self.list.push((lower, value));
        }
        self.changed();
        Ok(())
    }

    pub(crate) fn delete(&mut self, name: &str) -> Result<(), HeaderError> {
        if !is_token(name) {
            return Err(HeaderError::Name(name.to_owned()));
        }
        if self.guard == Guard::Immutable {
            return Err(HeaderError::Immutable);
        }
        let lower = name.to_ascii_lowercase();
        let refused = match self.guard {
            Guard::Request => is_forbidden_request_header(&lower),
            Guard::RequestNoCors => {
                !is_no_cors_safelisted_header(&lower, "") && !is_privileged_no_cors(&lower)
            }
            Guard::Response => is_forbidden_response_header(&lower),
            Guard::None | Guard::Immutable => false,
        };
        if refused {
            return Ok(());
        }
        self.list.retain(|(known, _)| *known != lower);
        self.changed();
        Ok(())
    }

    /// The `Set-Cookie` values, each separate.
    pub(crate) fn set_cookies(&self) -> Vec<String> {
        self.list
            .iter()
            .filter(|(name, _)| name == "set-cookie")
            .map(|(_, value)| value.clone())
            .collect()
    }

    /// The list as `Headers` iteration and the transport see it: sorted by name, the values of a
    /// name combined, except `Set-Cookie`, whose values stay separate.
    pub(crate) fn sorted_combined(&self) -> Vec<(String, String)> {
        let mut names: Vec<&str> = self.list.iter().map(|(name, _)| name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        let mut result = Vec::with_capacity(names.len());
        for name in names {
            if name == "set-cookie" {
                for value in self.set_cookies() {
                    result.push((name.to_owned(), value));
                }
            } else if let Some(value) = self.combined(name) {
                result.push((name.to_owned(), value));
            }
        }
        result
    }
}

/// A `ByteString` argument: ToString, then every code unit must be a byte.
pub(crate) fn byte_string(ctx: &mut Ctx, value: &Value) -> OpResult<String> {
    let text = ctx.coerce_string(value).map_err(OpError::thrown)?;
    for (index, unit) in lumen_common::smuggle::utf16_units(&text).into_iter().enumerate() {
        if unit > 0xff {
            return Err(OpError::type_error(format!(
                "Cannot convert argument to a ByteString because the character at index {index} has a value of {unit} which is greater than 255."
            )));
        }
    }
    Ok(text.to_string())
}

/// A `ByteString` parameter.
pub(crate) struct ByteStr(pub String);

impl<'a> FromArg<'a, JsHost> for ByteStr {
    fn from_arg(cx: &'a ArgCx<'_>, value: &'a Value, _: Slot) -> Result<Self, Value> {
        cx.before_js();
        cx.with_ctx(|ctx| byte_string(ctx, value).map_err(|error| error.to_value(ctx)))
            .map(ByteStr)
    }
}

fn invalid_init() -> OpError {
    OpError::type_error(
        "The provided value is not of type '(sequence<sequence<ByteString>> or record<ByteString, ByteString>)'.",
    )
}

/// Fill `data` from a `HeadersInit`: another `Headers`, a sequence of pairs, or a record.
pub(crate) fn fill(
    ctx: &mut Ctx,
    data: &SharedHeaders,
    init: &Value,
    existing: impl Fn(&mut Ctx, &Value) -> Option<SharedHeaders>,
) -> OpResult<()> {
    match init {
        Value::Undefined => return Ok(()),
        Value::Obj(_) => {}
        _ => return Err(invalid_init()),
    }
    if let Some(other) = existing(ctx, init) {
        let pairs = other.borrow().sorted_combined();
        for (name, value) in pairs {
            data.borrow_mut().append(&name, &value).map_err(HeaderError::into_op)?;
        }
        return Ok(());
    }
    let iterator = ctx.well_known_symbol("iterator").expect("Symbol.iterator");
    let method = ctx.reflect_get(init, &iterator, init).map_err(OpError::thrown)?;
    if method.is_callable() {
        let items = ctx.iterable_to_list(init, usize::MAX)?;
        for item in items {
            if !matches!(item, Value::Obj(_)) {
                return Err(invalid_init());
            }
            let pair = ctx.iterable_to_list(&item, usize::MAX)?;
            if pair.len() != 2 {
                return Err(OpError::type_error(format!(
                    "Headers constructor: expected name/value pair to be length 2, found {}.",
                    pair.len()
                )));
            }
            let name = byte_string(ctx, &pair[0])?;
            let value = byte_string(ctx, &pair[1])?;
            data.borrow_mut().append(&name, &value).map_err(HeaderError::into_op)?;
        }
        return Ok(());
    }
    let global = ctx.global_object();
    let object = ctx.member_get(&global, "Object").map_err(OpError::thrown)?;
    let keys_fn = ctx.member_get(&object, "keys").map_err(OpError::thrown)?;
    let keys = ctx
        .invoke(keys_fn, object, std::slice::from_ref(init))
        .map_err(OpError::thrown)?;
    for key in ctx.iterable_to_list(&keys, usize::MAX)? {
        let name = byte_string(ctx, &key)?;
        let value = ctx
            .member_get(init, &name)
            .map_err(OpError::thrown)?;
        let value = byte_string(ctx, &value)?;
        data.borrow_mut().append(&name, &value).map_err(HeaderError::into_op)?;
    }
    Ok(())
}

/// The text of `value` as a USVString, for URLs and other non-header strings.
pub(crate) fn usv(ctx: &mut Ctx, value: &Value) -> OpResult<String> {
    usv_string(ctx, value).map_err(OpError::thrown)
}
