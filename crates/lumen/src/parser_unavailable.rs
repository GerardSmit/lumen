//! Source entry points in a native-only runtime always reject compilation.
use crate::ast::{Expr, Function, LazyBody, Pattern, Stmt};
pub use crate::parser_support::*;

fn unavailable<T>() -> Result<T, ParseError> {
    Err(ParseError {
        message: "source compilation is unavailable in the Aot profile".into(),
        line: 0,
        at_eof: false,
    })
}

pub fn with_eager_bodies<R>(f: impl FnOnce() -> R) -> R {
    f()
}
pub fn last_error_span() -> (u32, u32) {
    (0, 0)
}
pub(crate) fn ts_error_span() -> Option<(u32, u32)> {
    None
}
pub fn error_code(_: &str) -> &'static str {
    "ERR_INVALID_TYPESCRIPT_SYNTAX"
}
pub(crate) fn jsx_path(key: &str) -> Option<bool> {
    let path = key.split(['?', '#']).next().unwrap_or(key);
    if path.ends_with(".tsx") {
        Some(true)
    } else if path.ends_with(".jsx") {
        Some(false)
    } else {
        None
    }
}
pub fn parse_script(_: &str, _: bool) -> Result<Vec<Stmt>, ParseError> {
    unavailable()
}
pub fn parse_script_lazy(_: &str) -> Result<Vec<Stmt>, ParseError> {
    unavailable()
}
pub fn parse_dynamic_function(_: &str) -> Result<Vec<Stmt>, ParseError> {
    unavailable()
}
pub fn parse_script_eval(
    _: &str,
    _: bool,
    _: bool,
    _: bool,
    _: &[String],
) -> Result<Vec<Stmt>, ParseError> {
    unavailable()
}
pub fn parse_module(_: &str) -> Result<Vec<Stmt>, ParseError> {
    unavailable()
}
pub fn parse_module_ts(_: &str) -> Result<Vec<Stmt>, ParseError> {
    unavailable()
}
pub fn parse_module_jsx(_: &str, _: bool, _: &JsxOptions) -> Result<Vec<Stmt>, ParseError> {
    unavailable()
}
pub fn parse_cjs_function(_: &str, _: &[&str], _: bool) -> Result<Function, ParseError> {
    unavailable()
}
pub(crate) fn parse_cjs_function_jsx(
    _: &str,
    _: &[&str],
    _: bool,
    _: Option<&JsxOptions>,
) -> Result<Function, ParseError> {
    unavailable()
}
pub fn parse_lazy_body(_: &LazyBody, _: &Function) -> Result<Vec<Stmt>, ParseError> {
    unavailable()
}
pub fn strip_types(_: &str) -> Result<String, (ParseError, Option<(u32, u32)>)> {
    unavailable().map_err(|error| (error, None))
}
pub(crate) fn destructuring_target(_: &Expr) -> Option<Pattern> {
    None
}
