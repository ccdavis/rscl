//! Sorted map and set (`sorted-map`, `sorted-set`) backed by `imbl`'s ordered
//! collections, ordered by Clojure `compare`.
use std::cmp::Ordering;

use super::{Kind, alloc, cast};
use crate::exc::{RtResult, err};
use crate::obj::exception::CLASS_CAST;
use crate::value::{ObjHeader, Value};

/// A key ordered by `compare`. Incomparable keys are rejected on insertion, so the
/// fallback order (by kind, then hash) only keeps `Ord` total for `imbl`'s sake.
#[derive(Clone)]
pub struct Key(pub Value);

impl Key {
    fn order(a: &Value, b: &Value) -> Ordering {
        match crate::coll::compare(a, b) {
            Ok(o) => o,
            Err(_) => {
                crate::exc::take();
                let ra = a.kind().map_or(0, |k| k as u32 + 1);
                let rb = b.kind().map_or(0, |k| k as u32 + 1);
                ra.cmp(&rb)
                    .then_with(|| crate::equiv::hasheq(a).cmp(&crate::equiv::hasheq(b)))
            }
        }
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Key) -> bool {
        Key::order(&self.0, &other.0) == Ordering::Equal
    }
}
impl Eq for Key {}
impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Key) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Key {
    fn cmp(&self, other: &Key) -> Ordering {
        Key::order(&self.0, &other.0)
    }
}

pub type OMap = imbl::OrdMap<Key, Value>;
pub type OSet = imbl::OrdSet<Key>;

#[repr(C)]
pub struct SortedMapObj {
    pub hdr: ObjHeader,
    pub m: OMap,
    pub meta: Value,
}

#[repr(C)]
pub struct SortedSetObj {
    pub hdr: ObjHeader,
    pub s: OSet,
    pub meta: Value,
}

pub fn map_new(m: OMap) -> Value {
    alloc(SortedMapObj {
        hdr: ObjHeader::new(Kind::SortedMap),
        m,
        meta: Value::NIL,
    })
}

pub fn set_new(s: OSet) -> Value {
    alloc(SortedSetObj {
        hdr: ObjHeader::new(Kind::SortedSet),
        s,
        meta: Value::NIL,
    })
}

pub fn map_get(v: &Value) -> &SortedMapObj {
    debug_assert!(v.is_kind(Kind::SortedMap));
    unsafe { cast::<SortedMapObj>(v) }
}

pub fn set_get(v: &Value) -> &SortedSetObj {
    debug_assert!(v.is_kind(Kind::SortedSet));
    unsafe { cast::<SortedSetObj>(v) }
}

/// `k` must be comparable with the keys already present (checked against one of them).
pub fn check_key(existing: Option<&Value>, k: &Value) -> RtResult<()> {
    if let Some(e) = existing {
        crate::coll::compare(e, k).map(drop).map_err(|_| {
            crate::exc::take();
            crate::exc::throw_new(
                CLASS_CAST,
                format!(
                    "{} cannot be compared with {}",
                    crate::core::type_name(k),
                    crate::core::type_name(e)
                ),
            )
        })?;
    } else if crate::coll::compare(k, k).is_err() {
        crate::exc::take();
        return err(
            CLASS_CAST,
            format!(
                "{} cannot be used as a sorted key",
                crate::core::type_name(k)
            ),
        );
    }
    Ok(())
}

/// A representative existing key: `nil` compares with everything, so prefer another.
fn witness<'a>(keys: impl Iterator<Item = &'a Key>) -> Option<&'a Value> {
    let mut first = None;
    for k in keys {
        if !k.0.is_nil() {
            return Some(&k.0);
        }
        first.get_or_insert(&k.0);
    }
    first
}

/// Lookups must not silently miss on a key of the wrong type: `(get (sorted-map 1 :a)
/// "x")` throws like Clojure. An empty collection accepts any key.
fn lookup_check(existing: Option<&Value>, k: &Value) -> RtResult<()> {
    match existing {
        Some(_) => check_key(existing, k),
        None => Ok(()),
    }
}

pub fn map_lookup<'a>(m: &'a OMap, k: &Value) -> RtResult<Option<&'a Value>> {
    lookup_check(witness(m.keys()), k)?;
    Ok(m.get(&Key(k.clone())))
}

pub fn set_contains(s: &OSet, k: &Value) -> RtResult<bool> {
    lookup_check(witness(s.iter()), k)?;
    Ok(s.contains(&Key(k.clone())))
}

pub fn map_remove(m: &OMap, k: &Value) -> RtResult<OMap> {
    lookup_check(witness(m.keys()), k)?;
    let mut n = m.clone();
    n.remove(&Key(k.clone()));
    Ok(n)
}

pub fn set_remove(s: &OSet, k: &Value) -> RtResult<OSet> {
    lookup_check(witness(s.iter()), k)?;
    let mut n = s.clone();
    n.remove(&Key(k.clone()));
    Ok(n)
}

pub fn map_assoc(m: &OMap, k: Value, v: Value) -> RtResult<OMap> {
    check_key(witness(m.keys()), &k)?;
    let mut n = m.clone();
    n.insert(Key(k), v);
    Ok(n)
}

pub fn set_insert(s: &OSet, k: Value) -> RtResult<OSet> {
    check_key(witness(s.iter()), &k)?;
    let mut n = s.clone();
    n.insert(Key(k));
    Ok(n)
}
