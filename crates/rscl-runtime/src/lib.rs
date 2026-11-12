//! rscl runtime: values, objects, reader, printer, namespaces and core builtins.
//! No Cranelift dependency: this crate is also linked into AOT-built executables.
//!
//! Values are plain words kept alive by the tracing collector (see `gc`). At the FFI
//! boundary an `EXC` result means an exception is pending in the thread-local slot
//! (see `exc`).

// Every `pub unsafe extern "C" fn` is an FFI entry point called by generated code
// with arguments the compiler produced; per-function `# Safety` sections would
// only repeat that.
#![allow(clippy::missing_safety_doc)]

pub mod api;
pub mod arena;
pub mod bridge;
pub mod coll;
pub mod core;
pub mod entry;
pub mod equiv;
pub mod exc;
pub mod ffi;
pub mod gc;
pub mod heap;
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
pub use value::Value;

/// The system allocator, with the bytes in use counted for the heap limit (see
/// `heap.rs`). Heap objects do not come from it (see `arena.rs`); it serves the
/// runtime's own data, the compiler, and the contents of strings.
#[global_allocator]
static GLOBAL: heap::Counting = heap::Counting;
