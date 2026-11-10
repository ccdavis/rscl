//! Keywords are interned, so equality is pointer equality, but the table only holds
//! weak references: a keyword nothing refers to any more is removed when the
//! collector frees it, so `(keyword untrusted-string)` cannot grow memory without
//! bound (Clojure uses weak references for the same reason). Keywords that appear in
//! compiled code are kept by their unit's constant table.
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

/// Live keyword objects by name (weak: the collector removes the dead ones).
struct Table {
    names: HashMap<(Option<String>, String), usize>,
}

fn table() -> &'static RwLock<Table> {
    static T: OnceLock<RwLock<Table>> = OnceLock::new();
    T.get_or_init(|| {
        RwLock::new(Table {
            names: HashMap::new(),
        })
    })
}

/// Intern a keyword: the one live object for this name.
pub fn intern(ns: Option<&str>, name: &str) -> Value {
    let key = (ns.map(str::to_owned), name.to_owned());
    if let Some(&p) = table().read().unwrap().names.get(&key) {
        return unsafe { Value::from_raw(p as u64) };
    }
    let mut w = table().write().unwrap();
    if let Some(&p) = w.names.get(&key) {
        return unsafe { Value::from_raw(p as u64) };
    }
    let hash = crate::equiv::hash_named(ns, name) ^ 0x9e37_79b9_7f4a_7c15;
    let obj = Box::new(KwObj {
        hdr: ObjHeader::new(Kind::Keyword),
        ns: ns.map(Into::into),
        name: name.into(),
        hash,
    });
    let p = Box::into_raw(obj) as usize;
    crate::gc::register(p as *mut ObjHeader);
    w.names.insert(key, p);
    unsafe { Value::from_raw(p as u64) }
}

/// The collector is about to free these keywords: forget them (the world is
/// stopped, so nobody can be interning one of them right now).
pub(crate) fn forget(dead: &[usize]) {
    if dead.is_empty() {
        return;
    }
    let mut w = table().write().unwrap_or_else(|e| e.into_inner());
    for &p in dead {
        let kw = unsafe { &*(p as *const KwObj) };
        let key = (kw.ns.as_deref().map(str::to_owned), kw.name.to_string());
        if w.names.get(&key) == Some(&p) {
            w.names.remove(&key);
        }
    }
}

/// Number of interned keywords (for diagnostics).
pub fn interned_count() -> usize {
    table().read().unwrap().names.len()
}

/// Whether a keyword of this name is currently interned (for tests).
pub fn is_interned(ns: Option<&str>, name: &str) -> bool {
    let key = (ns.map(str::to_owned), name.to_owned());
    table().read().unwrap().names.contains_key(&key)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning() {
        let (ns, name) = (Some("weak-test"), "one");
        let a = intern(ns, name);
        let b = intern(ns, name);
        assert!(a.identical(&b));
        assert!(is_interned(ns, name));
        let d = parse("weak-test/one");
        assert!(a.identical(&d));
    }
}
