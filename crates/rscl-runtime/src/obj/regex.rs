//! Compiled regular expressions (`#"..."`, `re-pattern`).
use super::{Kind, alloc, cast};
use crate::exc::{RtResult, err};
use crate::obj::exception::ILLEGAL_ARGUMENT;
use crate::value::{ObjHeader, Value};

#[repr(C)]
pub struct RegexObj {
    pub hdr: ObjHeader,
    pub re: regex::Regex,
    pub source: Box<str>,
}

pub fn new(source: &str) -> RtResult<Value> {
    match regex::Regex::new(source) {
        Ok(re) => Ok(alloc(RegexObj {
            hdr: ObjHeader::new(Kind::Regex),
            re,
            source: source.into(),
        })),
        Err(e) => err(ILLEGAL_ARGUMENT, format!("invalid regex {source:?}: {e}")),
    }
}

pub fn get(v: &Value) -> &RegexObj {
    debug_assert!(v.is_kind(Kind::Regex));
    unsafe { cast::<RegexObj>(v) }
}

pub fn as_regex(v: &Value) -> Option<&RegexObj> {
    if v.is_kind(Kind::Regex) {
        Some(get(v))
    } else {
        None
    }
}
