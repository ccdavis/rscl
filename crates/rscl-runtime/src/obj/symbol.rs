//! Symbols: optional namespace, name, metadata. Not interned (they carry metadata);
//! equality is by namespace and name.
use super::{Kind, alloc, cast};
use crate::value::{ObjHeader, Value};

#[repr(C)]
pub struct SymObj {
    pub hdr: ObjHeader,
    pub ns: Option<Box<str>>,
    pub name: Box<str>,
    pub meta: Value,
}

pub fn new(ns: Option<&str>, name: &str) -> Value {
    alloc(SymObj {
        hdr: ObjHeader::new(Kind::Symbol),
        ns: ns.map(Into::into),
        name: name.into(),
        meta: Value::NIL,
    })
}

/// Parse `ns/name` (a lone `/` is the symbol named "/").
pub fn parse(s: &str) -> Value {
    match split_ns(s) {
        (Some(ns), name) => new(Some(ns), name),
        (None, name) => new(None, name),
    }
}

/// Split a symbol or keyword body at the namespace separator.
pub fn split_ns(s: &str) -> (Option<&str>, &str) {
    if s == "/" {
        return (None, s);
    }
    match s.find('/') {
        Some(i) if i > 0 && i + 1 < s.len() => (Some(&s[..i]), &s[i + 1..]),
        _ => (None, s),
    }
}

pub fn get(v: &Value) -> &SymObj {
    debug_assert!(v.is_kind(Kind::Symbol));
    unsafe { cast::<SymObj>(v) }
}

pub fn as_sym(v: &Value) -> Option<&SymObj> {
    if v.is_kind(Kind::Symbol) {
        Some(get(v))
    } else {
        None
    }
}

impl SymObj {
    pub fn ns(&self) -> Option<&str> {
        self.ns.as_deref()
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn is(&self, ns: Option<&str>, name: &str) -> bool {
        self.ns() == ns && &*self.name == name
    }
    pub fn qualified(&self) -> String {
        match &self.ns {
            Some(ns) => format!("{ns}/{}", self.name),
            None => self.name.to_string(),
        }
    }
}

/// A copy of the symbol with different metadata (symbols are immutable).
pub fn with_meta(v: &Value, meta: Value) -> Value {
    let s = get(v);
    alloc(SymObj {
        hdr: ObjHeader::new(Kind::Symbol),
        ns: s.ns.clone(),
        name: s.name.clone(),
        meta,
    })
}
