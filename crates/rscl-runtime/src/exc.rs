//! Exceptions without unwinding.
//!
//! A throw stores the exception object in a thread-local slot and returns the `EXC`
//! immediate. Generated code checks for `EXC` after every call; Rust code propagates
//! `Err(Exc)` with `?`. A `catch` (or the REPL) calls `take()` to claim the object.
use std::cell::Cell;

use crate::obj::exception;
use crate::value::{EXC, Value};

thread_local! {
    static PENDING: Cell<u64> = const { Cell::new(0) };
}

/// Marker type: "an exception is pending in the thread-local slot".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exc;

pub type RtResult<T> = Result<T, Exc>;

/// Make `v` the pending exception (replacing and releasing any previous one).
pub fn throw(v: Value) -> Exc {
    exception::fill_trace(&v);
    PENDING.with(|p| {
        let old = p.replace(v.into_raw());
        if old != 0 {
            unsafe { Value::release_raw(old) }
        }
    });
    Exc
}

pub fn throw_new(class: &str, message: impl AsRef<str>) -> Exc {
    throw(exception::new(
        class,
        Some(message.as_ref()),
        Value::NIL,
        Value::NIL,
    ))
}

/// Convenience: `return err(CLASS, "message")`.
pub fn err<T>(class: &str, message: impl AsRef<str>) -> RtResult<T> {
    Err(throw_new(class, message))
}

/// Claim the pending exception, clearing the slot.
pub fn take() -> Option<Value> {
    PENDING.with(|p| {
        let bits = p.replace(0);
        if bits == 0 {
            None
        } else {
            Some(unsafe { Value::from_raw(bits) })
        }
    })
}

pub fn is_pending() -> bool {
    PENDING.with(|p| p.get() != 0)
}

/// Convert an FFI return value into a `RtResult`.
#[inline]
pub fn check(v: Value) -> RtResult<Value> {
    if v.is_exc() {
        std::mem::forget(v);
        Err(Exc)
    } else {
        Ok(v)
    }
}

/// `Class: message` for display.
pub fn format_exception(v: &Value) -> String {
    match exception::as_exc(v) {
        Some(e) => {
            let msg = e.message_str();
            if msg.is_empty() {
                e.class.to_string()
            } else {
                format!("{}: {}", e.class, msg)
            }
        }
        None => format!("thrown value: {}", crate::printer::pr_str(v)),
    }
}

/// `Class: message` followed by one `    at ns/fn` line per rscl frame.
pub fn format_exception_with_trace(v: &Value) -> String {
    let mut s = format_exception(v);
    for name in exception::trace_of(v) {
        s.push_str("\n    at ");
        s.push_str(&name);
    }
    s
}

// ---- FFI ----------------------------------------------------------------------------

/// Throw `v` (ownership transferred) and return `EXC`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_throw(v: Value) -> Value {
    throw(v);
    unsafe { Value::from_raw(EXC) }
}

/// Claim the pending exception (owned) or `nil` if none is pending.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_take_exception() -> Value {
    take().unwrap_or(Value::NIL)
}

/// Consume an `EXC` return value (already recorded as pending) into the marker.
#[allow(non_snake_case)]
pub fn Exc_from(v: Value) -> Exc {
    debug_assert!(v.is_exc());
    std::mem::forget(v);
    Exc
}
