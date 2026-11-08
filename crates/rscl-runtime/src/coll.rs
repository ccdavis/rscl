//! Collection and sequence operations shared by the builtins and the runtime.
//!
//! Interim seq representation: `seq` on a vector, map, set or string materialises a
//! list (Phase 4 adds proper seq objects and lazy seqs). Maps iterate as `[k v]`
//! vectors in the deterministic hasher order.
use std::cmp::Ordering;

use crate::exc::{RtResult, err};
use crate::obj::exception::{CLASS_CAST, ILLEGAL_ARGUMENT, INDEX_OUT_OF_BOUNDS, UNSUPPORTED};
use crate::obj::{self, Kind, list, map, set, string, vector};
use crate::value::Value;

fn type_name(v: &Value) -> &'static str {
    crate::core::type_name(v)
}

fn entry(k: &Value, v: &Value) -> Value {
    vector::from_slice(&[k.clone(), v.clone()])
}

/// Materialise any seqable value as a `Vec` of owned elements.
pub fn to_vec(v: &Value) -> RtResult<Vec<Value>> {
    Ok(match v.kind() {
        None if v.is_nil() => Vec::new(),
        Some(Kind::List) => list::get(v).to_vec(),
        Some(Kind::Vector) => vector::get(v).v.iter().cloned().collect(),
        Some(Kind::Map) => map::get(v).m.iter().map(|(k, x)| entry(k, x)).collect(),
        Some(Kind::Set) => set::get(v).s.iter().cloned().collect(),
        Some(Kind::String) => string::get(v).chars().map(Value::char).collect(),
        _ => {
            return err(
                ILLEGAL_ARGUMENT,
                format!("Don't know how to create ISeq from: {}", type_name(v)),
            );
        }
    })
}

pub fn is_seqable(v: &Value) -> bool {
    v.is_nil()
        || matches!(
            v.kind(),
            Some(Kind::List | Kind::Vector | Kind::Map | Kind::Set | Kind::String)
        )
}

pub fn count(v: &Value) -> RtResult<i64> {
    Ok(match v.kind() {
        None if v.is_nil() => 0,
        Some(Kind::List) => list::get(v).count,
        Some(Kind::Vector) => vector::get(v).v.len() as i64,
        Some(Kind::Map) => map::get(v).m.len() as i64,
        Some(Kind::Set) => set::get(v).s.len() as i64,
        Some(Kind::String) => string::get(v).chars().count() as i64,
        _ => {
            return err(
                UNSUPPORTED,
                format!("count not supported on this type: {}", type_name(v)),
            );
        }
    })
}

/// `seq`: `nil` for empty, otherwise a non-empty list.
pub fn seq(v: &Value) -> RtResult<Value> {
    if let Some(l) = list::as_list(v) {
        return Ok(if l.is_empty() { Value::NIL } else { v.clone() });
    }
    let items = to_vec(v)?;
    Ok(if items.is_empty() {
        Value::NIL
    } else {
        list::from_iter(items)
    })
}

pub fn first(v: &Value) -> RtResult<Value> {
    Ok(match v.kind() {
        None if v.is_nil() => Value::NIL,
        Some(Kind::List) => list::get(v).first.clone(),
        Some(Kind::Vector) => vector::get(v).v.front().cloned().unwrap_or(Value::NIL),
        Some(Kind::String) => string::get(v)
            .chars()
            .next()
            .map_or(Value::NIL, Value::char),
        _ => to_vec(v)?.into_iter().next().unwrap_or(Value::NIL),
    })
}

/// `rest`: always a list (the empty list for empty or one-element input).
pub fn rest(v: &Value) -> RtResult<Value> {
    Ok(match v.kind() {
        None if v.is_nil() => list::empty(),
        Some(Kind::List) => list::get(v).rest(),
        Some(Kind::Vector) => {
            let vv = &vector::get(v).v;
            if vv.len() <= 1 {
                list::empty()
            } else {
                list::from_iter(vv.iter().skip(1).cloned().collect::<Vec<_>>())
            }
        }
        _ => {
            let items = to_vec(v)?;
            if items.len() <= 1 {
                list::empty()
            } else {
                list::from_iter(items.into_iter().skip(1).collect::<Vec<_>>())
            }
        }
    })
}

pub fn next(v: &Value) -> RtResult<Value> {
    let r = rest(v)?;
    seq(&r)
}

pub fn second(v: &Value) -> RtResult<Value> {
    let r = rest(v)?;
    first(&r)
}

pub fn last(v: &Value) -> RtResult<Value> {
    Ok(match v.kind() {
        Some(Kind::Vector) => vector::get(v).v.back().cloned().unwrap_or(Value::NIL),
        _ => to_vec(v)?.pop().unwrap_or(Value::NIL),
    })
}

pub fn cons(x: Value, coll: &Value) -> RtResult<Value> {
    if coll.is_nil() {
        return Ok(list::from_iter(vec![x]));
    }
    if coll.is_kind(Kind::List) {
        return Ok(list::cons(x, coll.clone()));
    }
    let mut items = to_vec(coll)?;
    items.insert(0, x);
    Ok(list::from_iter(items))
}

pub fn conj(coll: &Value, x: Value) -> RtResult<Value> {
    match coll.kind() {
        None if coll.is_nil() => Ok(list::from_iter(vec![x])),
        Some(Kind::List) => Ok(list::cons(x, coll.clone())),
        Some(Kind::Vector) => {
            let mut v = vector::get(coll).v.clone();
            v.push_back(x);
            Ok(with_meta_of(coll, vector::new(v)))
        }
        Some(Kind::Set) => {
            let mut s = set::get(coll).s.clone();
            s.insert(x);
            Ok(with_meta_of(coll, set::new(s)))
        }
        Some(Kind::Map) => {
            let mut m = map::get(coll).m.clone();
            match x.kind() {
                Some(Kind::Vector) if vector::get(&x).v.len() == 2 => {
                    let e = &vector::get(&x).v;
                    m.insert(e[0].clone(), e[1].clone());
                }
                Some(Kind::Map) => {
                    for (k, v) in map::get(&x).m.iter() {
                        m.insert(k.clone(), v.clone());
                    }
                }
                None if x.is_nil() => {}
                _ => return err(ILLEGAL_ARGUMENT, "Vector arg to map conj must be a pair"),
            }
            Ok(with_meta_of(coll, map::new(m)))
        }
        _ => err(
            CLASS_CAST,
            format!(
                "{} cannot be cast to IPersistentCollection",
                type_name(coll)
            ),
        ),
    }
}

/// Collections created from another keep its metadata.
fn with_meta_of(src: &Value, dst: Value) -> Value {
    if let Some(m) = obj::meta(src)
        && !m.is_nil()
    {
        obj::set_meta_unshared(&dst, m);
    }
    dst
}

pub fn get(coll: &Value, k: &Value, not_found: Option<&Value>) -> RtResult<Value> {
    let nf = || not_found.cloned().unwrap_or(Value::NIL);
    Ok(match coll.kind() {
        Some(Kind::Map) => map::get(coll).m.get(k).cloned().unwrap_or_else(nf),
        Some(Kind::Set) => {
            if set::get(coll).s.contains(k) {
                k.clone()
            } else {
                nf()
            }
        }
        Some(Kind::Vector) => match k.as_int() {
            Some(i) if i >= 0 => vector::get(coll)
                .v
                .get(i as usize)
                .cloned()
                .unwrap_or_else(nf),
            _ => nf(),
        },
        Some(Kind::String) => match k.as_int() {
            Some(i) if i >= 0 => string::get(coll)
                .chars()
                .nth(i as usize)
                .map_or_else(nf, Value::char),
            _ => nf(),
        },
        _ => nf(),
    })
}

pub fn nth(coll: &Value, i: i64, not_found: Option<&Value>) -> RtResult<Value> {
    let oob = |len: usize| -> RtResult<Value> {
        match not_found {
            Some(v) => Ok(v.clone()),
            None => err(
                INDEX_OUT_OF_BOUNDS,
                format!("index {i} out of bounds for length {len}"),
            ),
        }
    };
    if i < 0 {
        return oob(0);
    }
    match coll.kind() {
        None if coll.is_nil() => Ok(Value::NIL),
        Some(Kind::Vector) => {
            let v = &vector::get(coll).v;
            match v.get(i as usize) {
                Some(x) => Ok(x.clone()),
                None => oob(v.len()),
            }
        }
        Some(Kind::String) => {
            let s = string::get(coll);
            match s.chars().nth(i as usize) {
                Some(c) => Ok(Value::char(c)),
                None => oob(s.chars().count()),
            }
        }
        Some(Kind::List) => {
            let l = list::get(coll);
            match l.iter().nth(i as usize) {
                Some(x) => Ok(x.clone()),
                None => oob(l.count as usize),
            }
        }
        _ => err(
            UNSUPPORTED,
            format!("nth not supported on this type: {}", type_name(coll)),
        ),
    }
}

pub fn assoc(coll: &Value, k: Value, v: Value) -> RtResult<Value> {
    match coll.kind() {
        None if coll.is_nil() => {
            let mut m = map::empty_map();
            m.insert(k, v);
            Ok(map::new(m))
        }
        Some(Kind::Map) => {
            let mut m = map::get(coll).m.clone();
            m.insert(k, v);
            Ok(with_meta_of(coll, map::new(m)))
        }
        Some(Kind::Vector) => {
            let vec = &vector::get(coll).v;
            let Some(i) = k.as_int() else {
                return err(ILLEGAL_ARGUMENT, "Key must be integer");
            };
            if i < 0 || i as usize > vec.len() {
                return err(
                    INDEX_OUT_OF_BOUNDS,
                    format!("index {i} out of bounds for length {}", vec.len()),
                );
            }
            let mut nv = vec.clone();
            if i as usize == vec.len() {
                nv.push_back(v)
            } else {
                nv.set(i as usize, v);
            }
            Ok(with_meta_of(coll, vector::new(nv)))
        }
        _ => err(
            CLASS_CAST,
            format!("{} cannot be cast to Associative", type_name(coll)),
        ),
    }
}

pub fn dissoc(coll: &Value, k: &Value) -> RtResult<Value> {
    match coll.kind() {
        None if coll.is_nil() => Ok(Value::NIL),
        Some(Kind::Map) => {
            let mut m = map::get(coll).m.clone();
            m.remove(k);
            Ok(with_meta_of(coll, map::new(m)))
        }
        _ => err(
            CLASS_CAST,
            format!("{} cannot be cast to IPersistentMap", type_name(coll)),
        ),
    }
}

pub fn disj(coll: &Value, k: &Value) -> RtResult<Value> {
    match coll.kind() {
        None if coll.is_nil() => Ok(Value::NIL),
        Some(Kind::Set) => {
            let mut s = set::get(coll).s.clone();
            s.remove(k);
            Ok(with_meta_of(coll, set::new(s)))
        }
        _ => err(
            CLASS_CAST,
            format!("{} cannot be cast to IPersistentSet", type_name(coll)),
        ),
    }
}

pub fn contains(coll: &Value, k: &Value) -> RtResult<bool> {
    Ok(match coll.kind() {
        None if coll.is_nil() => false,
        Some(Kind::Map) => map::get(coll).m.contains_key(k),
        Some(Kind::Set) => set::get(coll).s.contains(k),
        Some(Kind::Vector) => k
            .as_int()
            .is_some_and(|i| i >= 0 && (i as usize) < vector::get(coll).v.len()),
        Some(Kind::String) => k
            .as_int()
            .is_some_and(|i| i >= 0 && (i as usize) < string::get(coll).chars().count()),
        _ => {
            return err(
                ILLEGAL_ARGUMENT,
                format!("contains? not supported on type: {}", type_name(coll)),
            );
        }
    })
}

pub fn find(coll: &Value, k: &Value) -> RtResult<Value> {
    Ok(match coll.kind() {
        Some(Kind::Map) => map::get(coll).m.get(k).map_or(Value::NIL, |v| entry(k, v)),
        Some(Kind::Vector) => match k.as_int() {
            Some(i) if i >= 0 => vector::get(coll)
                .v
                .get(i as usize)
                .map_or(Value::NIL, |v| entry(k, v)),
            _ => Value::NIL,
        },
        _ => Value::NIL,
    })
}

pub fn keys(coll: &Value) -> RtResult<Value> {
    match coll.kind() {
        None if coll.is_nil() => Ok(Value::NIL),
        Some(Kind::Map) => {
            let m = &map::get(coll).m;
            Ok(if m.is_empty() {
                Value::NIL
            } else {
                list::from_iter(m.keys().cloned().collect::<Vec<_>>())
            })
        }
        _ => err(
            CLASS_CAST,
            format!("{} cannot be cast to a map", type_name(coll)),
        ),
    }
}

pub fn vals(coll: &Value) -> RtResult<Value> {
    match coll.kind() {
        None if coll.is_nil() => Ok(Value::NIL),
        Some(Kind::Map) => {
            let m = &map::get(coll).m;
            Ok(if m.is_empty() {
                Value::NIL
            } else {
                list::from_iter(m.values().cloned().collect::<Vec<_>>())
            })
        }
        _ => err(
            CLASS_CAST,
            format!("{} cannot be cast to a map", type_name(coll)),
        ),
    }
}

/// `peek`: last of a vector, first of a list.
pub fn peek(coll: &Value) -> RtResult<Value> {
    match coll.kind() {
        None if coll.is_nil() => Ok(Value::NIL),
        Some(Kind::Vector) => Ok(vector::get(coll).v.back().cloned().unwrap_or(Value::NIL)),
        Some(Kind::List) => Ok(list::get(coll).first.clone()),
        _ => err(
            CLASS_CAST,
            format!("{} cannot be cast to IPersistentStack", type_name(coll)),
        ),
    }
}

pub fn pop(coll: &Value) -> RtResult<Value> {
    match coll.kind() {
        None if coll.is_nil() => Ok(Value::NIL),
        Some(Kind::Vector) => {
            let v = &vector::get(coll).v;
            if v.is_empty() {
                return err(ILLEGAL_ARGUMENT, "Can't pop empty vector");
            }
            let mut nv = v.clone();
            nv.pop_back();
            Ok(vector::new(nv))
        }
        Some(Kind::List) => {
            let l = list::get(coll);
            if l.is_empty() {
                return err(ILLEGAL_ARGUMENT, "Can't pop empty list");
            }
            Ok(l.rest())
        }
        _ => err(
            CLASS_CAST,
            format!("{} cannot be cast to IPersistentStack", type_name(coll)),
        ),
    }
}

/// An empty collection of the same kind.
pub fn empty(coll: &Value) -> Value {
    match coll.kind() {
        Some(Kind::List) => list::empty(),
        Some(Kind::Vector) => vector::new(vector::Vec_::new()),
        Some(Kind::Map) => map::new(map::empty_map()),
        Some(Kind::Set) => set::new(set::empty_set()),
        _ => Value::NIL,
    }
}

/// Clojure `compare`: numbers numerically, strings/keywords/symbols lexically,
/// booleans, `nil` first, vectors by length then elements.
pub fn compare(a: &Value, b: &Value) -> RtResult<Ordering> {
    if a.is_nil() {
        return Ok(if b.is_nil() {
            Ordering::Equal
        } else {
            Ordering::Less
        });
    }
    if b.is_nil() {
        return Ok(Ordering::Greater);
    }
    if let (Some(x), Some(y)) = (crate::num::num(a), crate::num::num(b)) {
        return Ok(crate::num::cmp(x, y).unwrap_or(Ordering::Equal));
    }
    if let (Some(x), Some(y)) = (a.as_bool(), b.as_bool()) {
        return Ok(x.cmp(&y));
    }
    if let (Some(x), Some(y)) = (a.as_char(), b.as_char()) {
        return Ok(x.cmp(&y));
    }
    match (a.kind(), b.kind()) {
        (Some(Kind::String), Some(Kind::String)) => Ok(string::get(a).cmp(string::get(b))),
        (Some(Kind::Keyword), Some(Kind::Keyword)) => {
            let (x, y) = (obj::keyword::get(a), obj::keyword::get(b));
            Ok(x.ns().cmp(&y.ns()).then_with(|| x.name().cmp(y.name())))
        }
        (Some(Kind::Symbol), Some(Kind::Symbol)) => {
            let (x, y) = (obj::symbol::get(a), obj::symbol::get(b));
            Ok(x.ns().cmp(&y.ns()).then_with(|| x.name().cmp(y.name())))
        }
        (Some(Kind::Vector), Some(Kind::Vector)) => {
            let (x, y) = (&vector::get(a).v, &vector::get(b).v);
            if x.len() != y.len() {
                return Ok(x.len().cmp(&y.len()));
            }
            for (p, q) in x.iter().zip(y.iter()) {
                let c = compare(p, q)?;
                if c != Ordering::Equal {
                    return Ok(c);
                }
            }
            Ok(Ordering::Equal)
        }
        _ => err(
            CLASS_CAST,
            format!("{} cannot be compared to {}", type_name(a), type_name(b)),
        ),
    }
}
