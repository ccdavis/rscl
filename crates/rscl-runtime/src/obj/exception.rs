//! Exception objects. `class` is a small built-in taxonomy standing in for JVM
//! exception classes; `catch` matches on it (see docs/SEMANTICS.md).
use super::{Kind, alloc, cast};
use crate::value::{ObjHeader, Value};

pub const EXCEPTION_INFO: &str = "ExceptionInfo";
pub const ARITHMETIC: &str = "ArithmeticException";
pub const ILLEGAL_ARGUMENT: &str = "IllegalArgumentException";
pub const ARITY: &str = "ArityException";
pub const CLASS_CAST: &str = "ClassCastException";
pub const NULL_POINTER: &str = "NullPointerException";
pub const INDEX_OUT_OF_BOUNDS: &str = "IndexOutOfBoundsException";
pub const UNSUPPORTED: &str = "UnsupportedOperationException";
pub const ILLEGAL_STATE: &str = "IllegalStateException";
pub const STACK_OVERFLOW: &str = "StackOverflowError";
pub const OUT_OF_MEMORY: &str = "OutOfMemoryError";
pub const INTERRUPTED: &str = "InterruptedException";
pub const CANCELLATION: &str = "CancellationException";
pub const RUNTIME: &str = "RuntimeException";
pub const COMPILER: &str = "CompilerException";

#[repr(C)]
pub struct ExcObj {
    pub hdr: ObjHeader,
    pub class: Box<str>,
    /// A string or `nil`.
    pub message: Value,
    /// `ex-data`: a map or `nil`.
    pub data: Value,
    /// Another exception or `nil`.
    pub cause: Value,
    /// rscl fn names active when the exception was first thrown (innermost first).
    pub trace: std::sync::Mutex<Vec<String>>,
}

pub fn new(class: &str, message: Option<&str>, data: Value, cause: Value) -> Value {
    alloc(ExcObj {
        hdr: ObjHeader::new(Kind::Exception),
        class: class.into(),
        message: message.map_or(Value::NIL, super::string::new),
        data,
        cause,
        trace: std::sync::Mutex::new(Vec::new()),
    })
}

/// Record the stack trace once (rethrows keep the original).
pub fn fill_trace(v: &Value) {
    if let Some(e) = as_exc(v) {
        let mut t = e.trace.lock().unwrap_or_else(|x| x.into_inner());
        if t.is_empty() {
            *t = crate::trace::capture();
        }
    }
}

pub fn trace_of(v: &Value) -> Vec<String> {
    as_exc(v)
        .map(|e| e.trace.lock().unwrap_or_else(|x| x.into_inner()).clone())
        .unwrap_or_default()
}

pub fn get(v: &Value) -> &ExcObj {
    debug_assert!(v.is_kind(Kind::Exception));
    unsafe { cast::<ExcObj>(v) }
}

pub fn as_exc(v: &Value) -> Option<&ExcObj> {
    if v.is_kind(Kind::Exception) {
        Some(get(v))
    } else {
        None
    }
}

impl ExcObj {
    pub fn message_str(&self) -> &str {
        super::string::as_str(&self.message).unwrap_or("")
    }
    /// Does `catch <class>` select this exception? `Throwable`, `Exception` and
    /// `Error` catch everything; otherwise the class name must match.
    pub fn is_instance(&self, class: &str) -> bool {
        matches!(class, "Throwable" | "Exception" | "Error" | "Object") || &*self.class == class
    }
}
