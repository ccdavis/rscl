//! rscl compiler library: analyzer, Cranelift codegen, JIT driver and REPL. The
//! `rscl` binary is a thin CLI over this crate; tests use it in-process.
pub mod analyzer;
pub mod aot;
pub mod codegen;
pub mod errors;
pub mod jit;
pub mod repl;

pub use errors::CompileError;
pub use jit::{EvalError, Jit};
