//! The contract between the rscl runtime and native bridge libraries.
//!
//! A bridge library is a `cdylib` that exports `rscl_module_register(api) -> i32`.
//! The runtime calls it with a pointer to its `RtApi` table; the library keeps the
//! pointer and talks to the runtime only through it, so the process has exactly one
//! runtime (one interning table, one heap). Values cross the boundary as raw 64-bit
//! words.
//!
//! Lifetimes: the runtime has a tracing garbage collector. A value a library
//! function holds on its stack (or received as an argument) stays alive for the
//! call, and collections are deferred while the function calls back into rscl
//! code. A library must not keep values in storage of its own beyond a call: the
//! collector does not see them there. `retain` and `release` remain in the table
//! and do nothing.
#![no_std]

use core::ffi::c_void;

/// Bump when `RtApi` changes incompatibly.
pub const API_VERSION: u32 = 4;

/// The symbol every bridge library exports.
pub const REGISTER_SYMBOL: &str = "rscl_module_register";

/// The uniform calling convention of every rscl fn (compiled or native). `owned` is
/// non-zero when the argument slots belong to the callee for the duration of the
/// call (the caller will not read them again, so the callee may overwrite them),
/// and zero when they are memory the caller still uses.
pub type Entry =
    unsafe extern "C" fn(f: *const c_void, argc: i64, argv: *const u64, owned: u64) -> u64;

/// Destructor for a native object's payload.
pub type DropFn = unsafe extern "C" fn(data: *mut c_void);

/// Runtime services available to a bridge library. Strings are UTF-8 (ptr, len).
#[repr(C)]
pub struct RtApi {
    pub version: u32,
    /// The "exception pending" sentinel returned by throwing calls.
    pub exc: u64,
    pub retain: unsafe extern "C" fn(v: u64),
    pub release: unsafe extern "C" fn(v: u64),
    /// Static type name of a value, for error messages.
    pub type_name: unsafe extern "C" fn(v: u64, out: *mut *const u8, len: *mut i64),
    pub is_nil: unsafe extern "C" fn(v: u64) -> i32,
    pub truthy: unsafe extern "C" fn(v: u64) -> i32,
    pub nil: u64,
    pub bool_new: unsafe extern "C" fn(b: i32) -> u64,
    /// 1 and writes `out` if `v` is an integer.
    pub int_get: unsafe extern "C" fn(v: u64, out: *mut i64) -> i32,
    pub int_new: unsafe extern "C" fn(n: i64) -> u64,
    /// 1 and writes `out` if `v` is a number (integers are converted).
    pub double_get: unsafe extern "C" fn(v: u64, out: *mut f64) -> i32,
    pub double_new: unsafe extern "C" fn(d: f64) -> u64,
    /// 1 and writes (ptr, len) of the UTF-8 bytes if `v` is a string. Valid while `v` lives.
    pub string_get: unsafe extern "C" fn(v: u64, out: *mut *const u8, len: *mut i64) -> i32,
    pub string_new: unsafe extern "C" fn(ptr: *const u8, len: i64) -> u64,
    /// `ns` may be null.
    pub keyword_new:
        unsafe extern "C" fn(ns: *const u8, ns_len: i64, name: *const u8, name_len: i64) -> u64,
    /// 1 and writes the name (without namespace) if `v` is a keyword or symbol.
    pub named_name: unsafe extern "C" fn(v: u64, out: *mut *const u8, len: *mut i64) -> i32,
    /// Any seqable value as a vector (+1), or `exc`.
    pub to_vector: unsafe extern "C" fn(v: u64) -> u64,
    pub vector_len: unsafe extern "C" fn(v: u64) -> i64,
    /// Element `i` (+1); `nil` when out of range.
    pub vector_get: unsafe extern "C" fn(v: u64, i: i64) -> u64,
    pub vector_new: unsafe extern "C" fn(items: *const u64, n: i64) -> u64,
    /// Alternating keys and values.
    pub map_new: unsafe extern "C" fn(kvs: *const u64, n: i64) -> u64,
    /// Value for `k` (+1) or `not_found` (+1).
    pub map_get: unsafe extern "C" fn(m: u64, k: u64, not_found: u64) -> u64,
    /// Vector of `[k v]` vectors (+1), or `exc` if `m` is not a map.
    pub map_entries: unsafe extern "C" fn(m: u64) -> u64,
    /// Call an rscl fn; returns the result (+1) or `exc`.
    pub invoke: unsafe extern "C" fn(f: u64, argc: i64, argv: *const u64) -> u64,
    /// Throw a new exception of `class` with `message`; returns `exc`.
    pub throw:
        unsafe extern "C" fn(class: *const u8, class_len: i64, msg: *const u8, msg_len: i64) -> u64,
    /// Wrap a library-owned object. `drop` is called with `data` when the value dies.
    /// `token` identifies the library instance that owns the type, so a reloaded or
    /// different library never reinterprets the payload.
    pub native_new: unsafe extern "C" fn(
        type_name: *const u8,
        len: i64,
        token: u64,
        data: *mut c_void,
        drop: Option<DropFn>,
    ) -> u64,
    /// The payload of a native object of exactly `type_name` created by the library
    /// instance `token`, else null.
    pub native_get:
        unsafe extern "C" fn(v: u64, type_name: *const u8, len: i64, token: u64) -> *mut c_void,
    /// Namespace that subsequent `register_fn` calls define vars in.
    pub begin_module: unsafe extern "C" fn(ns: *const u8, len: i64),
    /// Define `name` in the current module namespace as a fn with the given arities.
    pub register_fn: unsafe extern "C" fn(
        name: *const u8,
        len: i64,
        entry: Entry,
        arity_mask: u64,
        variadic_min: i64,
    ),
    /// `pr-str` of a value (+1 string).
    pub pr_str: unsafe extern "C" fn(v: u64) -> u64,
    /// Drop the calling thread's pending exception, if any.
    pub exc_clear: unsafe extern "C" fn(),
}
