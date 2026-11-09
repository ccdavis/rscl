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
    // Immediates (nil, bools, chars, fixnums) are equal only when identical, except
    // that a fixnum may meet a boxed long.
    if a.kind().is_none() && b.kind().is_none() {
        return false;
    }
    if let (Some(x), Some(y)) = (a.as_int(), b.as_int()) {
        return x == y;
    }
    let (Some(ka), Some(kb)) = (a.kind(), b.kind()) else {
        return false;
    };
    match (ka, kb) {
        (Kind::Double, Kind::Double) => obj::double::get(a) == obj::double::get(b),
        (Kind::String, Kind::String) => obj::string::get(a) == obj::string::get(b),
        (Kind::Symbol, Kind::Symbol) => {
            let (x, y) = (obj::symbol::get(a), obj::symbol::get(b));
            x.ns == y.ns && x.name == y.name
        }
        (
            Kind::List | Kind::Vector | Kind::Cons | Kind::LazySeq,
            Kind::List | Kind::Vector | Kind::Cons | Kind::LazySeq,
        ) => seq_equiv(a, b),
        (Kind::Map, Kind::Map) => obj::map::get(a).m == obj::map::get(b).m,
        (Kind::Set, Kind::Set) => obj::set::get(a).s == obj::set::get(b).s,
        (Kind::Regex, Kind::Regex) => obj::regex::get(a).source == obj::regex::get(b).source,
        _ => false,
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
        Some(Kind::Vector) => Some(obj::vector::get(v).v.len()),
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

pub fn hasheq(v: &Value) -> u64 {
    if let Some(n) = v.as_int() {
        return mix64(n as u64);
    }
    let Some(kind) = v.kind() else {
        // nil, bools, chars: the bit pattern itself is unique per value.
        return mix64(v.bits() ^ 0xa5a5);
    };
    match kind {
        Kind::Long => unreachable!("handled by as_int"),
        Kind::Double => mix64(obj::double::get(v).to_bits() ^ 0xd0d0),
        Kind::String => mix64(hash_bytes(obj::string::get(v).as_bytes())),
        Kind::Symbol => {
            let s = obj::symbol::get(v);
            mix64(hash_named(s.ns(), s.name()))
        }
        Kind::Keyword => obj::keyword::get(v).hash,
        Kind::List | Kind::Vector | Kind::Cons | Kind::LazySeq => {
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
        Kind::Map => {
            let mut h = MAP_SEED;
            for (k, val) in obj::map::get(v).m.iter() {
                h = h.wrapping_add(mix64(hasheq(k).wrapping_mul(31).wrapping_add(hasheq(val))));
            }
            mix64(h)
        }
        Kind::Set => {
            let mut h = SET_SEED;
            for x in obj::set::get(v).s.iter() {
                h = h.wrapping_add(mix64(hasheq(x)));
            }
            mix64(h)
        }
        // Identity-hashed kinds.
        Kind::Fn | Kind::Var | Kind::Exception | Kind::Atom | Kind::Namespace | Kind::Native => {
            mix64(v.bits())
        }
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
