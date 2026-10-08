//! Argument types and Node-style errors shared by the native Web API classes
//! (`encoding`, `url`, ...).
use lumen::embed::bind::FromArg;
use lumen::embed::{ArgCx, Ctx, JsHost, OpError, Slot, Value};

/// Attach Node's `err.code` to an error.
pub fn coded(error: OpError, code: &'static str) -> OpError {
    error.with_code(code)
}

/// Node's `Value of "this" must be of type <type>` (`ERR_INVALID_THIS`).
pub fn invalid_this(type_name: &str) -> OpError {
    coded(
        OpError::type_error(format!("Value of \"this\" must be of type {type_name}")),
        "ERR_INVALID_THIS",
    )
}

/// Node's `The "<name>" argument must be <expected>. Received ...` (`ERR_INVALID_ARG_TYPE`); a
/// dotted name (`options.signal`) is a property.
pub fn invalid_arg_type(ctx: &mut Ctx, name: &str, expected: &str, value: &Value) -> OpError {
    let kind = if name.contains('.') { "property" } else { "argument" };
    coded(
        OpError::type_error(format!(
            "The \"{name}\" {kind} must be {expected}.{}",
            received_suffix(ctx, value)
        )),
        "ERR_INVALID_ARG_TYPE",
    )
}

/// Node's `The "a" and "b" arguments must be specified` (`ERR_MISSING_ARGS`).
pub fn missing_args(names: &[&str]) -> OpError {
    let quoted: Vec<String> = names.iter().map(|name| format!("\"{name}\"")).collect();
    let text = match quoted.as_slice() {
        [one] => format!("The {one} argument"),
        many => format!("The {} arguments", many.join(" and ")),
    };
    coded(
        OpError::type_error(format!("{text} must be specified")),
        "ERR_MISSING_ARGS",
    )
}

/// Node's `Received ...` tail for argument type errors.
pub fn received_suffix(ctx: &mut Ctx, value: &Value) -> String {
    match value {
        Value::Null => return " Received null".into(),
        Value::Undefined => return " Received undefined".into(),
        _ => {}
    }
    if value.is_callable() {
        let name = ctx
            .member_get(value, "name")
            .ok()
            .and_then(|name| ctx.coerce_string(&name).ok())
            .unwrap_or_default();
        return format!(" Received function {name}");
    }
    if matches!(value, Value::Obj(_)) {
        let name = ctx
            .member_get(value, "constructor")
            .ok()
            .filter(|constructor| matches!(constructor, Value::Obj(_)))
            .and_then(|constructor| ctx.member_get(&constructor, "name").ok())
            .and_then(|name| ctx.coerce_string(&name).ok())
            .filter(|name| !name.is_empty());
        return match name {
            Some(name) => format!(" Received an instance of {name}"),
            None => " Received [Object: null prototype] {}".into(),
        };
    }
    let kind = value.type_of();
    let text = if kind == "symbol" {
        let global = ctx.global_object();
        let string = ctx.member_get(&global, "String").unwrap_or(Value::Undefined);
        ctx.invoke(string, Value::Undefined, std::slice::from_ref(value))
            .ok()
            .and_then(|text| ctx.coerce_string(&text).ok())
            .unwrap_or_default()
    } else {
        ctx.coerce_string(value).unwrap_or_default()
    };
    let mut shown = if kind == "string" {
        let units = lumen_common::smuggle::utf16_units(&text);
        if units.len() > 28 {
            format!(
                "'{}...'",
                lumen_common::smuggle::utf16_from_units(&units[..25])
            )
        } else {
            format!("'{text}'")
        }
    } else {
        text.to_string()
    };
    if kind == "bigint" {
        shown.push('n');
    }
    format!(" Received type {kind} ({shown})")
}

/// WebIDL `USVString` conversion: ToString, then lone surrogates become U+FFFD.
pub fn usv_string(ctx: &mut Ctx, value: &Value) -> Result<String, Value> {
    if let Value::Str(text) = value {
        return Ok(lumen::well_formed_utf8(text.as_str()).into_owned());
    }
    let text = ctx.coerce_string(value)?;
    Ok(lumen::well_formed_utf8(&text).into_owned())
}

/// A `USVString` parameter.
pub struct Usv(pub String);

/// WebIDL `LegacyNullToEmptyString`: null becomes empty, while undefined and all
/// other values use ordinary string conversion. Converted storage belongs to the
/// binding argument context, so setters need not copy large input strings.
pub struct LegacyNullToEmptyString<'a>(pub &'a str);

impl<'a> FromArg<'a, JsHost> for LegacyNullToEmptyString<'a> {
    fn from_arg(cx: &'a ArgCx<'_>, value: &'a Value, at: Slot) -> Result<Self, Value> {
        if matches!(value, Value::Null) {
            Ok(Self(""))
        } else {
            <&'a str as FromArg<'a, JsHost>>::from_arg(cx, value, at).map(Self)
        }
    }
}

impl<'a> FromArg<'a, JsHost> for Usv {
    fn from_arg(cx: &'a ArgCx<'_>, value: &'a Value, _: Slot) -> Result<Self, Value> {
        cx.before_js();
        cx.with_ctx(|ctx| usv_string(ctx, value)).map(Usv)
    }
}

/// An optional `USVString` parameter: `undefined` (or an omitted argument) is `None`, every
/// other value, `null` included, converts like [`Usv`].
#[derive(Default)]
pub struct OptUsv(pub Option<String>);

impl<'a> FromArg<'a, JsHost> for OptUsv {
    fn from_arg(cx: &'a ArgCx<'_>, value: &'a Value, _: Slot) -> Result<Self, Value> {
        if matches!(value, Value::Undefined) {
            return Ok(OptUsv(None));
        }
        cx.before_js();
        cx.with_ctx(|ctx| usv_string(ctx, value))
            .map(|text| OptUsv(Some(text)))
    }
}
