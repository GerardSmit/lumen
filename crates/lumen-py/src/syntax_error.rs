//! The error the lexer and parser report for source that is not valid Python.

use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub struct SyntaxError {
    pub msg: String,
    pub line: u32,
    pub col: u32,
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SyntaxError: {} (line {}, column {})",
            self.msg,
            self.line,
            self.col + 1
        )
    }
}

impl std::error::Error for SyntaxError {}
