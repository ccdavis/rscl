//! rscl runtime: values, objects, reader, printer, namespaces and core builtins.
//! No Cranelift dependency: this crate is also linked into AOT-built executables.
//!
//! Ownership conventions at the FFI boundary: arguments are borrowed (`VRef`) unless
//! documented as consumed (`Value`); results are owned (+1). An `EXC` result means an
//! exception is pending in the thread-local slot (see `exc`).

// Every `pub unsafe extern "C" fn` is an FFI entry point whose contract is the
// ownership convention above; per-function `# Safety` sections would only repeat it.
#![allow(clippy::missing_safety_doc)]

pub mod api;
pub mod coll;
pub mod core;
pub mod entry;
pub mod equiv;
pub mod exc;
pub mod ffi;
pub mod native;
pub mod ns;
pub mod num;
pub mod obj;
pub mod pos;
pub mod printer;
pub mod reader;
pub mod value;

pub use exc::{Exc, RtResult};
pub use obj::Kind;
pub use value::{VRef, Value, live_objects};
