//! CPython's integer-literal digit limit, shared by the lexer and the interpreter's limits.

use std::cell::Cell;

/// CPython's default for `sys.get_int_max_str_digits()`.
pub const DEFAULT_INT_MAX_STR_DIGITS: usize = 4300;

thread_local! {
    static LITERAL_DIGITS: Cell<usize> = const { Cell::new(DEFAULT_INT_MAX_STR_DIGITS) };
}

/// The digit limit the lexer applies to decimal integer literals while [`Interp::compile_source`]
/// is running.
pub fn literal_digit_limit() -> usize {
    LITERAL_DIGITS.with(|c| c.get())
}

pub fn with_literal_digit_limit<T>(limit: usize, f: impl FnOnce() -> T) -> T {
    let prev = LITERAL_DIGITS.with(|c| c.replace(limit));
    let out = f();
    LITERAL_DIGITS.with(|c| c.set(prev));
    out
}

pub fn digit_limit_message(limit: usize, found: Option<usize>) -> String {
    match found {
        Some(n) => format!(
            "Exceeds the limit ({limit} digits) for integer string conversion: value has {n} digits; use sys.set_int_max_str_digits() to increase the limit"
        ),
        None => format!(
            "Exceeds the limit ({limit} digits) for integer string conversion; use sys.set_int_max_str_digits() to increase the limit"
        ),
    }
}

