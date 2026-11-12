//! Clojure equality (`=`) and hashing (`hash`) over values.
//!
//! Rules: numbers compare within category (`(= 1 1.0)` is false; `==` lives in
//! `num.rs`); strings, symbols and chars by content; keywords by identity (interned);
//! lists and vectors are mutually equal when their elements are; maps and sets
//! structurally; everything else by identity. Hashing is consistent with these rules
//! and deterministic across processes.
use crate::obj::{self, Kind};
use crate::value::Value;

pub fn equiv(a: &Value, b: &Value) -> bool {
    if a.bits() == b.bits() {
        return true;
    }
    // Too deep to compare: `StackOverflowError` is left pending (the `=` builtin
    // rethrows it) and the answer is "not equal".
    // (Only collections recurse.)
    if a.kind().is_some_and(nests) && crate::native::stack_low() {
        crate::exc::throw_new(crate::obj::exception::STACK_OVERFLOW, "stack overflow");
        return false;
    }
    // Immediates (nil, bools, chars, fixnums) are equal only when identical, except
    // that a fixnum may meet a boxed long.
    if a.kind().is_none() && b.kind().is_none() {
        return false;
    }
    if let (Some(x), Some(y)) = (a.int_value(), b.int_value()) {
        return x == y;
    }
    let (Some(ka), Some(kb)) = (a.kind(), b.kind()) else {
        return false;
    };
    match (ka, kb) {
        (Kind::Double, Kind::Double) => obj::double::get(a) == obj::double::get(b),
        (Kind::BigInt, Kind::BigInt) => obj::bigint::get(a) == obj::bigint::get(b),
        (Kind::String, Kind::String) => obj::string::get(a) == obj::string::get(b),
        (Kind::Symbol, Kind::Symbol) => {
            let (x, y) = (obj::symbol::get(a), obj::symbol::get(b));
            x.hash == y.hash && x.ns == y.ns && x.name == y.name
        }
        (
            Kind::List | Kind::Vector | Kind::Cons | Kind::LazySeq | Kind::VecSeq,
            Kind::List | Kind::Vector | Kind::Cons | Kind::LazySeq | Kind::VecSeq,
        ) => seq_equiv(a, b),
        // Clones (O(1)): comparing elements can run rscl code (lazy seqs), during
        // which a collection may replace the node trees of `a` and `b` themselves.
        (Kind::Map, Kind::Map) => obj::map::get(a).m == obj::map::get(b).m,
        (Kind::Map | Kind::SortedMap, Kind::Map | Kind::SortedMap) => map_equiv(a, b),
        (Kind::Set, Kind::Set) => obj::set::get(a).s == obj::set::get(b).s,
        (Kind::Set | Kind::SortedSet, Kind::Set | Kind::SortedSet) => set_equiv(a, b),
        (Kind::Regex, Kind::Regex) => obj::regex::get(a).source == obj::regex::get(b).source,
        (Kind::Instance, Kind::Instance) => {
            let (x, y) = (obj::types::inst(a), obj::types::inst(b));
            // Records of the same type compare by value; deftype instances by identity.
            std::ptr::eq(x.ty, y.ty)
                && x.ty().is_record
                && x.fields
                    .iter()
                    .zip(y.fields.iter())
                    .all(|(p, q)| equiv(p, q))
                && equiv(&x.ext, &y.ext)
        }
        _ => false,
    }
}

/// Kinds whose equality and hash recurse into elements.
fn nests(k: Kind) -> bool {
    matches!(
        k,
        Kind::List
            | Kind::Vector
            | Kind::Cons
            | Kind::LazySeq
            | Kind::VecSeq
            | Kind::Map
            | Kind::Set
            | Kind::SortedMap
            | Kind::SortedSet
            | Kind::Instance
    )
}

/// Entry-wise comparison of a hash map and/or a sorted map.
fn map_equiv(a: &Value, b: &Value) -> bool {
    let ea = map_entries(a);
    let eb = map_entries(b);
    if ea.len() != eb.len() {
        return false;
    }
    ea.iter().all(
        |(k, v)| match crate::coll::get(b, k, Some(&Value::UNBOUND)) {
            Ok(w) => !w.is_unbound() && equiv(v, &w),
            Err(_) => {
                crate::exc::take();
                false
            }
        },
    )
}

fn map_entries(v: &Value) -> Vec<(Value, Value)> {
    match v.kind() {
        Some(Kind::Map) => obj::map::get(v).m.iter().map(|(k, x)| (*k, *x)).collect(),
        Some(Kind::SortedMap) => obj::sorted::map_get(v)
            .m
            .iter()
            .map(|(k, x)| (k.0, *x))
            .collect(),
        _ => Vec::new(),
    }
}

fn set_equiv(a: &Value, b: &Value) -> bool {
    let ea = set_items(a);
    let eb = set_items(b);
    ea.len() == eb.len()
        && ea.iter().all(|k| match crate::coll::contains(b, k) {
            Ok(x) => x,
            Err(_) => {
                crate::exc::take();
                false
            }
        })
}

fn set_items(v: &Value) -> Vec<Value> {
    match v.kind() {
        Some(Kind::Set) => obj::set::get(v).s.iter().cloned().collect(),
        Some(Kind::SortedSet) => obj::sorted::set_get(v).s.iter().map(|k| k.0).collect(),
        _ => Vec::new(),
    }
}

/// Element-wise comparison of sequential values. A realisation error inside a lazy
/// seq cannot be reported from `==`; it is discarded and the values compare unequal.
fn seq_equiv(a: &Value, b: &Value) -> bool {
    let (ca, cb) = (fast_count(a), fast_count(b));
    if let (Some(x), Some(y)) = (ca, cb)
        && x != y
    {
        return false;
    }
    let mut ia = crate::coll::seq_iter(a);
    let mut ib = crate::coll::seq_iter(b);
    loop {
        match (ia.next(), ib.next()) {
            (None, None) => return true,
            (Some(Ok(x)), Some(Ok(y))) => {
                if !equiv(&x, &y) {
                    return false;
                }
            }
            (Some(Err(_)), _) | (_, Some(Err(_))) => {
                crate::exc::take();
                return false;
            }
            _ => return false,
        }
    }
}

fn fast_count(v: &Value) -> Option<usize> {
    match v.kind() {
        Some(Kind::List) => Some(obj::list::get(v).count as usize),
        Some(Kind::Vector) => Some(obj::vector::get(v).len()),
        _ => None,
    }
}

// ---- hashing -------------------------------------------------------------------

#[inline]
pub fn mix64(mut x: u64) -> u64 {
    // splitmix64 finaliser
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    x
}

pub fn hash_bytes(bytes: &[u8]) -> u64 {
    // FNV-1a
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub fn hash_named(ns: Option<&str>, name: &str) -> u64 {
    let mut h = hash_bytes(name.as_bytes());
    if let Some(ns) = ns {
        h = mix64(h ^ hash_bytes(ns.as_bytes()).rotate_left(17));
    }
    h
}

const SEQ_SEED: u64 = 0x5eed_5eed_0000_0001;
const MAP_SEED: u64 = 0x5eed_5eed_0000_0002;
const SET_SEED: u64 = 0x5eed_5eed_0000_0003;

fn hash_map_entries<'a>(entries: impl Iterator<Item = (&'a Value, &'a Value)>) -> u64 {
    let mut h = MAP_SEED;
    for (k, val) in entries {
        h = h.wrapping_add(mix64(hasheq(k).wrapping_mul(31).wrapping_add(hasheq(val))));
    }
    mix64(h)
}

fn hash_set_items<'a>(items: impl Iterator<Item = &'a Value>) -> u64 {
    let mut h = SET_SEED;
    for x in items {
        h = h.wrapping_add(mix64(hasheq(x)));
    }
    mix64(h)
}

pub fn hasheq(v: &Value) -> u64 {
    if let Some(n) = v.int_value() {
        return mix64(n as u64);
    }
    let Some(kind) = v.kind() else {
        // nil, bools, chars: the bit pattern itself is unique per value.
        return mix64(v.bits() ^ 0xa5a5);
    };
    // Too deep to walk: stop descending (equal structures still hash alike).
    if nests(kind) && crate::native::stack_low() {
        return mix64(0x5eed);
    }
    match kind {
        Kind::Long => unreachable!("handled by int_value"),
        // A BigInt that fits a long was handled by `int_value`, with the long's hash.
        Kind::BigInt => {
            mix64(hash_bytes(&obj::bigint::get(v).to_signed_bytes_le()) ^ 0xb16b_0000_0000_0001)
        }
        // `+ 0.0` turns -0.0 into 0.0: the two are `=`, so they hash alike.
        Kind::Double => mix64((obj::double::get(v) + 0.0).to_bits() ^ 0xd0d0),
        Kind::String => mix64(hash_bytes(obj::string::get(v).as_bytes())),
        Kind::Symbol => mix64(obj::symbol::get(v).hash),
        Kind::Keyword => obj::keyword::get(v).hash,
        Kind::List | Kind::Vector | Kind::Cons | Kind::LazySeq | Kind::VecSeq => {
            let mut h = SEQ_SEED;
            for x in crate::coll::seq_iter(v) {
                match x {
                    Ok(x) => h = h.wrapping_mul(31).wrapping_add(hasheq(&x)),
                    Err(_) => {
                        crate::exc::take();
                        break;
                    }
                }
            }
            mix64(h)
        }
        Kind::Regex => mix64(hash_bytes(obj::regex::get(v).source.as_bytes()) ^ 0x5e5e),
        // Sorted and hashed collections with the same contents must hash alike.
        Kind::Map => hash_map_entries(obj::map::get(v).m.iter()),
        Kind::SortedMap => hash_map_entries(
            obj::sorted::map_get(v)
                .m
                .clone()
                .iter()
                .map(|(k, val)| (&k.0, val)),
        ),
        Kind::Set => hash_set_items(obj::set::get(v).s.iter()),
        Kind::SortedSet => hash_set_items(obj::sorted::set_get(v).s.clone().iter().map(|k| &k.0)),
        Kind::Instance => {
            let i = obj::types::inst(v);
            if !i.ty().is_record {
                return mix64(v.bits());
            }
            let mut h = MAP_SEED ^ i.ty().id;
            for (k, val) in i.entries() {
                h = h.wrapping_add(mix64(
                    hasheq(&k).wrapping_mul(31).wrapping_add(hasheq(&val)),
                ));
            }
            mix64(h)
        }
        // Identity-hashed kinds.
        Kind::Node
        | Kind::Fn
        | Kind::Var
        | Kind::Exception
        | Kind::Atom
        | Kind::Namespace
        | Kind::Native
        | Kind::Type
        | Kind::Protocol
        | Kind::Pending => mix64(v.bits()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obj::{keyword, list, map, set, string, symbol, vector};

    #[test]
    fn numbers() {
        assert!(equiv(&Value::int(1), &Value::int(1)));
        assert!(!equiv(&Value::int(1), &Value::double(1.0)));
        assert!(equiv(&Value::double(1.5), &Value::double(1.5)));
        assert!(!equiv(&Value::double(f64::NAN), &Value::double(f64::NAN)));
        assert!(equiv(&Value::int(i64::MAX), &Value::int(i64::MAX)));
        assert_eq!(hasheq(&Value::int(5)), hasheq(&Value::int(5)));
    }

    #[test]
    fn strings_symbols_keywords() {
        assert!(equiv(&string::new("ab"), &string::new("ab")));
        assert!(!equiv(&string::new("ab"), &symbol::new(None, "ab")));
        assert!(equiv(&symbol::new(Some("a"), "b"), &symbol::parse("a/b")));
        assert!(equiv(
            &keyword::parse("a/b"),
            &keyword::intern(Some("a"), "b")
        ));
        assert!(!equiv(&keyword::parse("b"), &keyword::parse("a/b")));
        assert_eq!(hasheq(&string::new("x")), hasheq(&string::new("x")));
    }

    #[test]
    fn sequentials_and_maps() {
        let l = list::from_slice(&[Value::int(1), Value::int(2)]);
        let v = vector::from_slice(&[Value::int(1), Value::int(2)]);
        assert!(equiv(&l, &v));
        assert_eq!(hasheq(&l), hasheq(&v));
        assert!(!equiv(&l, &vector::from_slice(&[Value::int(1)])));
        let m1 = map::from_pairs(&[
            keyword::parse("a"),
            Value::int(1),
            keyword::parse("b"),
            Value::int(2),
        ]);
        let m2 = map::from_pairs(&[
            keyword::parse("b"),
            Value::int(2),
            keyword::parse("a"),
            Value::int(1),
        ]);
        assert!(equiv(&m1, &m2));
        assert_eq!(hasheq(&m1), hasheq(&m2));
        let s1 = set::from_slice(&[Value::int(1), Value::int(2)]);
        let s2 = set::from_slice(&[Value::int(2), Value::int(1)]);
        assert!(equiv(&s1, &s2));
        assert!(!equiv(&s1, &v));
    }
}
