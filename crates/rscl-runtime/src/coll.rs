//! Collection and sequence operations shared by the builtins and the runtime.
//!
//! Interim seq representation: `seq` on a vector, map, set or string materialises a
//! list (Phase 4 adds proper seq objects and lazy seqs). Maps iterate as `[k v]`
//! vectors in the deterministic hasher order.
use std::cmp::Ordering;

use crate::exc::{RtResult, err};
use crate::obj::exception::{CLASS_CAST, ILLEGAL_ARGUMENT, INDEX_OUT_OF_BOUNDS, UNSUPPORTED};
use crate::obj::{self, Kind, cons, lazyseq, list, map, set, sorted, string, types, vector};
use crate::value::Value;

fn type_name(v: &Value) -> &'static str {
    crate::core::type_name(v)
}

fn entry(k: &Value, v: &Value) -> Value {
    vector::from_slice(&[k.clone(), v.clone()])
}

/// Materialise any seqable value as a `Vec` of owned elements (realising lazy seqs).
pub fn to_vec(v: &Value) -> RtResult<Vec<Value>> {
    Ok(match v.kind() {
        None if v.is_nil() => Vec::new(),
        Some(Kind::List) => list::get(v).to_vec(),
        Some(Kind::Vector) => vector::get(v).v.iter().cloned().collect(),
        Some(Kind::Map) => map::get(v).m.iter().map(|(k, x)| entry(k, x)).collect(),
        Some(Kind::Set) => set::get(v).s.iter().cloned().collect(),
        Some(Kind::String) => string::get(v).chars().map(Value::char).collect(),
        Some(Kind::Cons | Kind::LazySeq) => seq_iter(v).collect::<RtResult<Vec<Value>>>()?,
        Some(Kind::SortedMap) => sorted::map_get(v)
            .m
            .iter()
            .map(|(k, x)| entry(&k.0, x))
            .collect(),
        Some(Kind::SortedSet) => sorted::set_get(v).s.iter().map(|k| k.0.clone()).collect(),
        Some(Kind::Instance) if types::is_record(v) => types::inst(v)
            .entries()
            .into_iter()
            .map(|(k, x)| entry(&k, &x))
            .collect(),
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
            Some(
                Kind::List
                    | Kind::Vector
                    | Kind::Map
                    | Kind::Set
                    | Kind::String
                    | Kind::Cons
                    | Kind::LazySeq
                    | Kind::SortedMap
                    | Kind::SortedSet
            )
        )
        || types::is_record(v)
}

/// A seq object (as opposed to a seqable collection).
pub fn is_seq(v: &Value) -> bool {
    matches!(v.kind(), Some(Kind::List | Kind::Cons | Kind::LazySeq))
}

/// Walk any seqable value element by element, realising lazy seqs as it goes.
pub struct SeqIter {
    cur: Value,
    done: bool,
}

pub fn seq_iter(v: &Value) -> SeqIter {
    SeqIter {
        cur: v.clone(),
        done: false,
    }
}

impl Iterator for SeqIter {
    type Item = RtResult<Value>;
    fn next(&mut self) -> Option<RtResult<Value>> {
        if self.done {
            return None;
        }
        let step = (|| {
            let s = seq(&self.cur)?;
            if s.is_nil() {
                return Ok(None);
            }
            let f = first(&s)?;
            self.cur = rest(&s)?;
            Ok(Some(f))
        })();
        match step {
            Ok(Some(x)) => Some(Ok(x)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

pub fn count(v: &Value) -> RtResult<i64> {
    Ok(match v.kind() {
        None if v.is_nil() => 0,
        Some(Kind::List) => list::get(v).count,
        Some(Kind::Vector) => vector::get(v).v.len() as i64,
        Some(Kind::Map) => map::get(v).m.len() as i64,
        Some(Kind::Set) => set::get(v).s.len() as i64,
        Some(Kind::String) => string::get(v).chars().count() as i64,
        Some(Kind::SortedMap) => sorted::map_get(v).m.len() as i64,
        Some(Kind::SortedSet) => sorted::set_get(v).s.len() as i64,
        Some(Kind::Instance) if types::is_record(v) => types::inst(v).count() as i64,
        Some(Kind::Cons | Kind::LazySeq) => {
            let mut n = 0;
            for x in seq_iter(v) {
                x?;
                n += 1;
            }
            n
        }
        _ => {
            return err(
                UNSUPPORTED,
                format!("count not supported on this type: {}", type_name(v)),
            );
        }
    })
}

/// `seq`: `nil` for empty, otherwise a seq object (list, cons or realised lazy seq).
/// Vectors, maps, sets and strings are materialised into a list.
pub fn seq(v: &Value) -> RtResult<Value> {
    match v.kind() {
        None if v.is_nil() => Ok(Value::NIL),
        Some(Kind::List) => Ok(if list::get(v).is_empty() {
            Value::NIL
        } else {
            v.clone()
        }),
        Some(Kind::Cons) => Ok(v.clone()),
        Some(Kind::LazySeq) => lazyseq::realize(v),
        _ => {
            let items = to_vec(v)?;
            Ok(if items.is_empty() {
                Value::NIL
            } else {
                list::from_iter(items)
            })
        }
    }
}

pub fn first(v: &Value) -> RtResult<Value> {
    Ok(match v.kind() {
        None if v.is_nil() => Value::NIL,
        Some(Kind::List) => list::get(v).first.clone(),
        Some(Kind::Cons) => cons::get(v).first.clone(),
        Some(Kind::LazySeq) => first(&lazyseq::realize(v)?)?,
        Some(Kind::Vector) => vector::get(v).v.front().cloned().unwrap_or(Value::NIL),
        Some(Kind::String) => string::get(v)
            .chars()
            .next()
            .map_or(Value::NIL, Value::char),
        _ => to_vec(v)?.into_iter().next().unwrap_or(Value::NIL),
    })
}

/// `rest`: a seq (possibly an unrealised lazy seq), or the empty list.
pub fn rest(v: &Value) -> RtResult<Value> {
    Ok(match v.kind() {
        None if v.is_nil() => list::empty(),
        Some(Kind::List) => list::get(v).rest(),
        Some(Kind::Cons) => {
            let more = &cons::get(v).more;
            match more.kind() {
                None if more.is_nil() => list::empty(),
                Some(Kind::List) => {
                    if list::get(more).is_empty() {
                        list::empty()
                    } else {
                        more.clone()
                    }
                }
                Some(Kind::Cons | Kind::LazySeq) => more.clone(),
                _ => {
                    let s = seq(more)?;
                    if s.is_nil() { list::empty() } else { s }
                }
            }
        }
        Some(Kind::LazySeq) => rest(&lazyseq::realize(v)?)?,
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

/// `cons`: onto `nil` or a list gives a list; onto anything else a cons cell whose
/// tail is the (possibly lazy) collection itself.
pub fn cons(x: Value, coll: &Value) -> RtResult<Value> {
    if coll.is_nil() {
        return Ok(list::from_iter(vec![x]));
    }
    if coll.is_kind(Kind::List) {
        return Ok(list::cons(x, coll.clone()));
    }
    if !is_seqable(coll) {
        return err(
            ILLEGAL_ARGUMENT,
            format!("Don't know how to create ISeq from: {}", type_name(coll)),
        );
    }
    Ok(cons::new(x, coll.clone()))
}

pub fn conj(coll: &Value, x: Value) -> RtResult<Value> {
    match coll.kind() {
        None if coll.is_nil() => Ok(list::from_iter(vec![x])),
        Some(Kind::List) => Ok(list::cons(x, coll.clone())),
        Some(Kind::Cons | Kind::LazySeq) => Ok(cons::new(x, coll.clone())),
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
        Some(Kind::SortedSet) => {
            let s = sorted::set_insert(&sorted::set_get(coll).s, x)?;
            Ok(with_meta_of(coll, sorted::set_new(s)))
        }
        Some(Kind::SortedMap) | Some(Kind::Instance) => {
            if coll.is_kind(Kind::Instance) && !types::is_record(coll) {
                return err(
                    CLASS_CAST,
                    format!(
                        "{} cannot be cast to IPersistentCollection",
                        type_name(coll)
                    ),
                );
            }
            let mut c = coll.clone();
            match x.kind() {
                Some(Kind::Vector) if vector::get(&x).v.len() == 2 => {
                    let e = &vector::get(&x).v;
                    c = assoc(&c, e[0].clone(), e[1].clone())?;
                }
                Some(Kind::Map | Kind::SortedMap | Kind::Instance) => {
                    for e in to_vec(&x)? {
                        let e = &vector::get(&e).v;
                        c = assoc(&c, e[0].clone(), e[1].clone())?;
                    }
                }
                None if x.is_nil() => {}
                _ => return err(ILLEGAL_ARGUMENT, "Vector arg to map conj must be a pair"),
            }
            Ok(c)
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
        Some(Kind::SortedMap) => sorted::map_lookup(&sorted::map_get(coll).m, k)?
            .cloned()
            .unwrap_or_else(nf),
        Some(Kind::SortedSet) => {
            if sorted::set_contains(&sorted::set_get(coll).s, k)? {
                k.clone()
            } else {
                nf()
            }
        }
        Some(Kind::Instance) if types::is_record(coll) => {
            types::inst(coll).get_kw(k).unwrap_or_else(nf)
        }
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
        Some(Kind::Cons | Kind::LazySeq) => {
            let mut n = 0usize;
            for x in seq_iter(coll) {
                let x = x?;
                if n == i as usize {
                    return Ok(x);
                }
                n += 1;
            }
            oob(n)
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
        Some(Kind::SortedMap) => {
            let m = sorted::map_assoc(&sorted::map_get(coll).m, k, v)?;
            Ok(with_meta_of(coll, sorted::map_new(m)))
        }
        Some(Kind::Instance) if types::is_record(coll) => {
            let i = types::inst(coll);
            let mut fields = i.fields.clone();
            if let Some(ix) = i.ty().field_kws.iter().position(|fk| fk.identical(&k)) {
                fields[ix] = v;
                return Ok(types::with(coll, fields, i.ext.clone()));
            }
            let mut ext = map::as_map(&i.ext).map_or_else(map::empty_map, |m| m.m.clone());
            ext.insert(k, v);
            Ok(types::with(coll, fields, map::new(ext)))
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
        Some(Kind::SortedMap) => {
            let m = sorted::map_remove(&sorted::map_get(coll).m, k)?;
            Ok(with_meta_of(coll, sorted::map_new(m)))
        }
        Some(Kind::Instance) if types::is_record(coll) => {
            let i = types::inst(coll);
            if i.ty().field_kws.iter().any(|fk| fk.identical(k)) {
                // Removing a declared field turns the record into a plain map.
                let mut m = map::empty_map();
                for (ek, ev) in i.entries() {
                    if !ek.identical(k) {
                        m.insert(ek, ev);
                    }
                }
                return Ok(with_meta_of(coll, map::new(m)));
            }
            let mut ext = map::as_map(&i.ext).map_or_else(map::empty_map, |m| m.m.clone());
            ext.remove(k);
            let ext = if ext.is_empty() {
                Value::NIL
            } else {
                map::new(ext)
            };
            Ok(types::with(coll, i.fields.clone(), ext))
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
        Some(Kind::SortedSet) => {
            let s = sorted::set_remove(&sorted::set_get(coll).s, k)?;
            Ok(with_meta_of(coll, sorted::set_new(s)))
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
        Some(Kind::SortedMap) => sorted::map_lookup(&sorted::map_get(coll).m, k)?.is_some(),
        Some(Kind::SortedSet) => sorted::set_contains(&sorted::set_get(coll).s, k)?,
        Some(Kind::Instance) if types::is_record(coll) => types::inst(coll).get_kw(k).is_some(),
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
        Some(Kind::SortedMap) => {
            sorted::map_lookup(&sorted::map_get(coll).m, k)?.map_or(Value::NIL, |v| entry(k, v))
        }
        Some(Kind::Instance) if types::is_record(coll) => types::inst(coll)
            .get_kw(k)
            .map_or(Value::NIL, |v| entry(k, &v)),
        _ => Value::NIL,
    })
}

/// A map, sorted map or record.
pub fn is_map_like(coll: &Value) -> bool {
    matches!(coll.kind(), Some(Kind::Map | Kind::SortedMap)) || types::is_record(coll)
}

/// The `(k, v)` pairs of a map-like value (`nil` is empty); anything else is an error,
/// so callers never cast an arbitrary element as an entry vector.
pub fn map_entries(v: &Value, what: &str) -> RtResult<Vec<(Value, Value)>> {
    if v.is_nil() {
        return Ok(Vec::new());
    }
    if !is_map_like(v) {
        return err(
            ILLEGAL_ARGUMENT,
            format!("{what} must be a map, got {}", type_name(v)),
        );
    }
    Ok(to_vec(v)?
        .iter()
        .map(|e| {
            let e = &vector::get(e).v;
            (e[0].clone(), e[1].clone())
        })
        .collect())
}

fn list_or_nil(items: Vec<Value>) -> Value {
    if items.is_empty() {
        Value::NIL
    } else {
        list::from_iter(items)
    }
}

pub fn keys(coll: &Value) -> RtResult<Value> {
    match coll.kind() {
        None if coll.is_nil() => Ok(Value::NIL),
        Some(Kind::Map) => Ok(list_or_nil(map::get(coll).m.keys().cloned().collect())),
        _ if is_map_like(coll) => {
            let ks = to_vec(coll)?
                .iter()
                .map(|e| vector::get(e).v[0].clone())
                .collect();
            Ok(list_or_nil(ks))
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
        Some(Kind::Map) => Ok(list_or_nil(map::get(coll).m.values().cloned().collect())),
        _ if is_map_like(coll) => {
            let vs = to_vec(coll)?
                .iter()
                .map(|e| vector::get(e).v[1].clone())
                .collect();
            Ok(list_or_nil(vs))
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
        Some(Kind::List | Kind::Cons | Kind::LazySeq) => list::empty(),
        Some(Kind::Vector) => vector::new(vector::Vec_::new()),
        Some(Kind::Map) => map::new(map::empty_map()),
        Some(Kind::Set) => set::new(set::empty_set()),
        Some(Kind::SortedMap) => sorted::map_new(sorted::OMap::new()),
        Some(Kind::SortedSet) => sorted::set_new(sorted::OSet::new()),
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
