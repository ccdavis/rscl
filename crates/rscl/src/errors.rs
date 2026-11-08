//! Compile-time errors with source positions.
use std::fmt;

use rscl_runtime::pos;

#[derive(Debug, Clone)]
pub struct CompileError {
    pub msg: String,
    /// Position id (see `rscl_runtime::pos`), 0 when unknown.
    pub pos: u32,
}

impl CompileError {
    pub fn new(msg: impl Into<String>, pos: u32) -> Self {
        CompileError {
            msg: msg.into(),
            pos,
        }
    }
    pub fn at(form: &rscl_runtime::Value, msg: impl Into<String>) -> Self {
        CompileError {
            msg: msg.into(),
            pos: pos::pos_id(form),
        }
    }
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.pos != 0 {
            write!(f, "{}: {}", pos::describe(self.pos), self.msg)
        } else {
            f.write_str(&self.msg)
        }
    }
}

impl std::error::Error for CompileError {}

pub type CResult<T> = Result<T, CompileError>;

/// Shorthand for `Err(CompileError::at(form, msg))`.
pub fn fail<T>(form: &rscl_runtime::Value, msg: impl Into<String>) -> CResult<T> {
    Err(CompileError::at(form, msg))
}
