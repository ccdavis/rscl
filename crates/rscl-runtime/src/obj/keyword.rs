//! Keywords are interned and immortal, so equality is pointer equality. The table
//! grows without bound (documented limitation; Clojure uses weak references).
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use super::{Kind, cast};
use crate::value::{ObjHeader, Value};

#[repr(C)]
pub struct KwObj {
    pub hdr: ObjHeader,
    pub ns: Option<Box<str>>,
    pub name: Box<str>,
    pub hash: u64,
}

struct Table(HashMap<(Option<String>, String), usize>);
// SAFETY: the table stores addresses of leaked, immortal objects.
unsafe impl Send for Table {}
unsafe impl Sync for Table {}

fn table() -> &'static RwLock<Table> {
    static T: OnceLock<RwLock<Table>> = OnceLock::new();
    T.get_or_init(|| RwLock::new(Table(HashMap::new())))
}

/// Intern a keyword. The returned value is immortal; retain/release are harmless.
pub fn intern(ns: Option<&str>, name: &str) -> Value {
    let key = (ns.map(str::to_owned), name.to_owned());
    if let Some(&p) = table().read().unwrap().0.get(&key) {
        return unsafe { Value::from_raw(p as u64) };
    }
    let mut w = table().write().unwrap();
    if let Some(&p) = w.0.get(&key) {
        return unsafe { Value::from_raw(p as u64) };
    }
    let hash = crate::equiv::hash_named(ns, name) ^ 0x9e37_79b9_7f4a_7c15;
    let obj = Box::new(KwObj {
        hdr: ObjHeader::immortal(Kind::Keyword),
        ns: ns.map(Into::into),
        name: name.into(),
        hash,
    });
    let p = Box::into_raw(obj) as usize;
    w.0.insert(key, p);
    unsafe { Value::from_raw(p as u64) }
}

/// Intern from `ns/name` text (without the leading colon).
pub fn parse(s: &str) -> Value {
    let (ns, name) = super::symbol::split_ns(s);
    intern(ns, name)
}

pub fn get(v: &Value) -> &KwObj {
    debug_assert!(v.is_kind(Kind::Keyword));
    unsafe { cast::<KwObj>(v) }
}

pub fn as_kw(v: &Value) -> Option<&KwObj> {
    if v.is_kind(Kind::Keyword) {
        Some(get(v))
    } else {
        None
    }
}

impl KwObj {
    pub fn ns(&self) -> Option<&str> {
        self.ns.as_deref()
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn qualified(&self) -> String {
        match &self.ns {
            Some(ns) => format!("{ns}/{}", self.name),
            None => self.name.to_string(),
        }
    }
}
