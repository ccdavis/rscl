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
pub const INTERRUPTED: &str = "InterruptedException";
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
}

pub fn new(class: &str, message: Option<&str>, data: Value, cause: Value) -> Value {
    alloc(ExcObj {
        hdr: ObjHeader::new(Kind::Exception),
        class: class.into(),
        message: message.map_or(Value::NIL, super::string::new),
        data,
        cause,
    })
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
