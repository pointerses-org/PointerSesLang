//! Shared code-generation error type.

use std::fmt;

#[derive(Debug)]
pub struct CodegenError {
    pub msg: String,
}

impl CodegenError {
    pub fn new(msg: String) -> Self {
        CodegenError { msg }
    }
}

impl fmt::Display for CodegenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl std::error::Error for CodegenError {}
