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
pub mod bridge;
pub mod coll;
pub mod core;
pub mod entry;
pub mod equiv;
pub mod exc;
pub mod ffi;
pub mod gc;
pub mod native;
pub mod ns;
pub mod num;
pub mod obj;
pub mod pos;
pub mod printer;
pub mod reader;
pub mod trace;
pub mod value;

/// The bootstrap library, compiled into every session and every AOT binary.
pub const CORE_CLJ: &str = include_str!("../core/core.clj");

pub use exc::{Exc, RtResult};
pub use obj::Kind;
pub use value::{VRef, Value, live_objects};

/// Allocation-heavy workloads (persistent collections, lazy seqs) spend a quarter of
/// their time in malloc; mimalloc roughly halves that. The staticlib carries it into
/// AOT-built programs too.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
