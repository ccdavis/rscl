//! Collection and sequence operations shared by the builtins and the runtime.
//!
//! The seq of a vector is a position in it (`obj/vecseq.rs`), so `seq`, `rest` and
//! `next` of a vector cost one small object whatever its size. The seq of a map,
//! set or string is such a seq over a vector of its elements, made once. Maps
//! iterate as `[k v]` vectors in the deterministic hasher order.
use std::cmp::Ordering;

use crate::exc::{RtResult, err};
use crate::obj::exception::{CLASS_CAST, ILLEGAL_ARGUMENT, INDEX_OUT_OF_BOUNDS, UNSUPPORTED};
use crate::obj::{
    self, Kind, cons, lazyseq, list, map, set, sorted, string, types, vecseq, vector,
};
use crate::value::Value;

fn type_name(v: &Value) -> &'static str {
    crate::core::type_name(v)
}

fn entry(k: &Value, v: &Value) -> Value {
    vector::from_slice(&[*k, *v])
}

/// Materialise any seqable value as a `Vec` of its elements (realising lazy seqs).
pub fn to_vec(v: &Value) -> RtResult<Vec<Value>> {
    Ok(match v.kind() {
        None if v.is_nil() => Vec::new(),
        Some(Kind::List) => list::get(v).to_vec(),
        Some(Kind::Vector) => vector::get(v).iter().cloned().collect(),
        Some(Kind::Map) => map::get(v).m.iter().map(|(k, x)| entry(k, x)).collect(),
        Some(Kind::Set) => set::get(v).s.iter().cloned().collect(),
        Some(Kind::String) => string::get(v).chars().map(Value::char).collect(),
        Some(Kind::Cons | Kind::LazySeq) => seq_iter(v).collect::<RtResult<Vec<Value>>>()?,
        Some(Kind::VecSeq) => {
            let s = vecseq::get(v);
            let mut items: Vec<Value> = s.items().iter().skip(s.i).copied().collect();
            if !s.more.is_nil() {
                // (The elements so far are reachable through `v`.)
                items.extend(to_vec(&s.more)?);
            }
            items
        }
        Some(Kind::SortedMap) => sorted::map_get(v)
            .m
            .iter()
            .map(|(k, x)| entry(&k.0, x))
            .collect(),
        Some(Kind::SortedSet) => sorted::set_get(v).s.iter().map(|k| k.0).collect(),
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
                    | Kind::VecSeq
                    | Kind::SortedMap
                    | Kind::SortedSet
            )
        )
        || types::is_record(v)
}

/// A seq object (as opposed to a seqable collection).
pub fn is_seq(v: &Value) -> bool {
    matches!(
        v.kind(),
        Some(Kind::List | Kind::Cons | Kind::LazySeq | Kind::VecSeq)
    )
}

/// Where a sequence walk keeps its position. A native that walks one of its
/// arguments uses the argument slot itself as the cursor (see `core::ArgSlots`):
/// the head is never copied into a native frame, where a stale copy would keep
/// every cell behind the cursor alive for the conservative stack scan.
pub enum Cursor {
    /// An argument slot of the native doing the walk.
    Slot(*mut Value),
    Own(Value),
}

impl Cursor {
    #[inline]
    pub fn get(&self) -> Value {
        match self {
            Cursor::Slot(p) => unsafe { std::ptr::read_volatile(*p) },
            Cursor::Own(v) => *v,
        }
    }
    /// Is the value a cons or lazy seq? Looks through the slot without copying the
    /// value into this frame (a copy of a sequence's head left in a long-lived
    /// frame would keep the whole sequence alive).
    #[inline]
    pub fn is_lazy(&self) -> bool {
        match self {
            Cursor::Slot(p) => is_lazy_kind(unsafe { &**p }),
            Cursor::Own(v) => is_lazy_kind(v),
        }
    }
    #[inline]
    fn set(&mut self, v: Value) {
        match self {
            Cursor::Slot(p) => unsafe { std::ptr::write_volatile(*p, v) },
            Cursor::Own(slot) => *slot = v,
        }
    }
}

/// Walk any seqable value element by element, realising lazy seqs as it goes.
pub struct SeqIter {
    cur: Cursor,
    done: bool,
    /// A vector or a chunked seq is walked a vector at a time, with the cursor
    /// already at what follows the vector: the vector being handed out (or
    /// `nil`), the index after the part of it in `part`, and the elements of that
    /// part still to come (one chunk of the vector).
    chunk: Value,
    at: usize,
    part: std::slice::Iter<'static, Value>,
}

pub fn seq_iter(v: &Value) -> SeqIter {
    seq_iter_at(Cursor::Own(*v))
}

/// Walk a sequence from a cursor: as the iterator advances, nothing else
/// refers to the cells behind it, so a lazy sequence is collected as it is consumed.
pub fn seq_iter_at(cur: Cursor) -> SeqIter {
    SeqIter {
        cur,
        done: false,
        chunk: Value::NIL,
        at: 0,
        part: [].iter(),
    }
}

/// A cons, a lazy seq or a chunk with more after it: walked cell by cell;
/// everything else is a materialised collection with its own fast paths.
pub fn is_lazy_kind(v: &Value) -> bool {
    match v.kind() {
        Some(Kind::Cons | Kind::LazySeq) => true,
        Some(Kind::VecSeq) => !vecseq::get(v).more.is_nil(),
        _ => false,
    }
}

/// Elements of the value at a cursor: a lazy sequence streams (its cells are
/// freed as it is walked), any other collection is materialised once.
pub enum CursorIter {
    Seq(SeqIter),
    /// The materialised elements stay rooted while the iterator exists: some are
    /// fresh objects (map entries) that nothing else refers to, and the consumer
    /// may call rscl code between elements.
    Vec(std::vec::IntoIter<Value>, crate::gc::SliceRoot<'static>),
}

impl Iterator for CursorIter {
    type Item = RtResult<Value>;
    fn next(&mut self) -> Option<RtResult<Value>> {
        match self {
            CursorIter::Seq(it) => it.next(),
            CursorIter::Vec(it, _) => it.next().map(Ok),
        }
    }
}

pub fn iter_at(c: Cursor) -> RtResult<CursorIter> {
    // Lists and vectors are walked where they are, like lazy sequences; the other
    // collections have to produce their elements first (a map its entries).
    let in_place = c.is_lazy()
        || matches!(
            c.get().kind(),
            Some(Kind::List | Kind::Vector | Kind::VecSeq)
        );
    Ok(if in_place {
        CursorIter::Seq(seq_iter_at(c))
    } else {
        let items = to_vec(&c.get())?;
        // SAFETY: the buffer belongs to the iterator stored next to the guard.
        let root = unsafe { crate::gc::root_words(items.as_ptr() as *const u64, items.len()) };
        CursorIter::Vec(items.into_iter(), root)
    })
}

/// `count` of the value at a cursor (streams lazy sequences).
pub fn count_at(v: Cursor) -> RtResult<i64> {
    if !v.is_lazy() {
        return count(&v.get());
    }
    let mut n = 0;
    for x in seq_iter_at(v) {
        x?;
        n += 1;
    }
    Ok(n)
}

/// `last` of the value at a cursor (streams lazy sequences).
pub fn last_at(v: Cursor) -> RtResult<Value> {
    match v.get().kind() {
        Some(Kind::Vector) => {
            return Ok(vector::get(&v.get()).back().cloned().unwrap_or(Value::NIL));
        }
        // Never empty.
        Some(Kind::VecSeq) if !v.is_lazy() => {
            return Ok(vecseq::get(&v.get())
                .items()
                .back()
                .cloned()
                .unwrap_or(Value::NIL));
        }
        _ => {}
    }
    let mut last = Value::NIL;
    for x in iter_at(v)? {
        last = x?;
    }
    Ok(last)
}

/// `nth` of the value at a cursor (streams lazy sequences).
pub fn nth_at(coll: Cursor, i: i64, not_found: Option<&Value>) -> RtResult<Value> {
    if !coll.is_lazy() || i < 0 {
        return nth(&coll.get(), i, not_found);
    }
    let mut n = 0i64;
    for x in seq_iter_at(coll) {
        let x = x?;
        if n == i {
            return Ok(x);
        }
        n += 1;
    }
    match not_found {
        Some(v) => Ok(*v),
        None => err(
            INDEX_OUT_OF_BOUNDS,
            format!("index {i} out of bounds for length {n}"),
        ),
    }
}

impl SeqIter {
    /// Move on to the part of `chunk` that starts at `at` (which is in range).
    fn take_part(&mut self) {
        let part = vector::get(&self.chunk).chunk_from(self.at);
        self.at += part.len();
        // SAFETY: the slice is inside the vector `self.chunk` refers to (or one of
        // its nodes), which lives as long as this iterator holds that reference.
        self.part = unsafe { std::mem::transmute::<&[Value], &'static [Value]>(part) }.iter();
    }
}

impl Iterator for SeqIter {
    type Item = RtResult<Value>;
    fn next(&mut self) -> Option<RtResult<Value>> {
        if self.done {
            return None;
        }
        if let Some(x) = self.part.next() {
            return Some(Ok(*x));
        }
        if !self.chunk.is_nil() {
            if self.at < vector::get(&self.chunk).len() {
                self.take_part();
                return self.part.next().map(|x| Ok(*x));
            }
            self.chunk = Value::NIL;
        }
        let step = (|| {
            let cur = self.cur.get();
            if cur.is_kind(Kind::Vector) {
                // (No seq object is needed to walk a vector.)
                if vector::get(&cur).is_empty() {
                    self.cur.set(Value::NIL);
                    return Ok(None);
                }
                self.chunk = cur;
                self.at = 0;
                self.cur.set(Value::NIL);
                self.take_part();
                return Ok(self.part.next().copied());
            }
            let s = seq(&cur)?;
            if s.is_nil() {
                self.cur.set(Value::NIL);
                return Ok(None);
            }
            if s.is_kind(Kind::VecSeq) {
                let o = vecseq::get(&s);
                self.chunk = o.vec;
                self.at = o.i;
                self.cur.set(o.more);
                self.take_part();
                return Ok(self.part.next().copied());
            }
            let f = first(&s)?;
            self.cur.set(rest(&s)?);
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
        Some(Kind::Vector) => vector::get(v).len() as i64,
        Some(Kind::Map) => map::get(v).m.len() as i64,
        Some(Kind::Set) => set::get(v).s.len() as i64,
        Some(Kind::String) => string::get(v).chars().count() as i64,
        Some(Kind::SortedMap) => sorted::map_get(v).m.len() as i64,
        Some(Kind::SortedSet) => sorted::set_get(v).s.len() as i64,
        Some(Kind::Instance) if types::is_record(v) => types::inst(v).count() as i64,
        Some(Kind::VecSeq) if !is_lazy_kind(v) => vecseq::get(v).here() as i64,
        Some(Kind::Cons | Kind::LazySeq | Kind::VecSeq) => count_at(Cursor::Own(*v))?,
        _ => {
            return err(
                UNSUPPORTED,
                format!("count not supported on this type: {}", type_name(v)),
            );
        }
    })
}

/// The seq of a vector object from index `i`: `nil` past the end.
fn vec_seq(vec: &Value, i: usize) -> Value {
    if i < vector::get(vec).len() {
        vecseq::new(*vec, i, Value::NIL)
    } else {
        Value::NIL
    }
}

/// `seq`: `nil` for empty, otherwise a seq object (a list, a cons, a realised lazy
/// seq, or a position in a vector). Maps, sets and strings are copied into a
/// vector first.
pub fn seq(v: &Value) -> RtResult<Value> {
    match v.kind() {
        None if v.is_nil() => Ok(Value::NIL),
        Some(Kind::List) => Ok(if list::get(v).is_empty() {
            Value::NIL
        } else {
            *v
        }),
        Some(Kind::Cons | Kind::VecSeq) => Ok(*v),
        Some(Kind::LazySeq) => lazyseq::realize(v),
        Some(Kind::Vector) => Ok(vec_seq(v, 0)),
        _ => {
            let items = to_vec(v)?;
            Ok(if items.is_empty() {
                Value::NIL
            } else {
                vecseq::new(vector::from_iter(items), 0, Value::NIL)
            })
        }
    }
}

pub fn first(v: &Value) -> RtResult<Value> {
    Ok(match v.kind() {
        None if v.is_nil() => Value::NIL,
        Some(Kind::List) => list::get(v).first,
        Some(Kind::Cons) => cons::get(v).first,
        Some(Kind::LazySeq) => first(&lazyseq::realize(v)?)?,
        Some(Kind::VecSeq) => vecseq::get(v).first(),
        Some(Kind::Vector) => vector::get(v).front().cloned().unwrap_or(Value::NIL),
        Some(Kind::String) => string::get(v)
            .chars()
            .next()
            .map_or(Value::NIL, Value::char),
        Some(Kind::Map) => map::get(v)
            .m
            .iter()
            .next()
            .map_or(Value::NIL, |(k, x)| entry(k, x)),
        Some(Kind::Set) => set::get(v).s.iter().next().copied().unwrap_or(Value::NIL),
        _ => to_vec(v)?.into_iter().next().unwrap_or(Value::NIL),
    })
}

/// A seq as the result of `rest`: the empty list where `seq` gives `nil`.
fn or_empty(s: Value) -> Value {
    if s.is_nil() { list::empty() } else { s }
}

/// What follows an element, as `rest` returns it: a seq, an unrealised lazy seq
/// as it is, or the empty list.
pub fn tail(more: &Value) -> RtResult<Value> {
    Ok(match more.kind() {
        None if more.is_nil() => list::empty(),
        Some(Kind::List) => {
            if list::get(more).is_empty() {
                list::empty()
            } else {
                *more
            }
        }
        Some(Kind::Cons | Kind::LazySeq | Kind::VecSeq) => *more,
        _ => or_empty(seq(more)?),
    })
}

/// `rest`: a seq (possibly an unrealised lazy seq), or the empty list.
pub fn rest(v: &Value) -> RtResult<Value> {
    Ok(match v.kind() {
        None if v.is_nil() => list::empty(),
        Some(Kind::List) => list::get(v).rest(),
        Some(Kind::Cons) => tail(&cons::get(v).more)?,
        Some(Kind::LazySeq) => rest(&lazyseq::realize(v)?)?,
        Some(Kind::VecSeq) => tail(&vecseq::get(v).rest())?,
        Some(Kind::Vector) => or_empty(vec_seq(v, 1)),
        _ => {
            let s = seq(v)?;
            if s.is_nil() { list::empty() } else { rest(&s)? }
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
    last_at(Cursor::Own(*v))
}

/// `cons`: onto `nil` or a list gives a list; onto anything else a cons cell whose
/// tail is the (possibly lazy) collection itself.
pub fn cons(x: Value, coll: &Value) -> RtResult<Value> {
    if coll.is_nil() {
        return Ok(list::from_iter(vec![x]));
    }
    if coll.is_kind(Kind::List) {
        return Ok(list::cons(x, *coll));
    }
    if !is_seqable(coll) {
        return err(
            ILLEGAL_ARGUMENT,
            format!("Don't know how to create ISeq from: {}", type_name(coll)),
        );
    }
    Ok(cons::new(x, *coll))
}

pub fn conj(coll: &Value, x: Value) -> RtResult<Value> {
    match coll.kind() {
        None if coll.is_nil() => Ok(list::from_iter(vec![x])),
        Some(Kind::List) => Ok(list::cons(x, *coll)),
        Some(Kind::Cons | Kind::LazySeq | Kind::VecSeq) => Ok(cons::new(x, *coll)),
        Some(Kind::Vector) => Ok(vector::get(coll).conj(x)),
        Some(Kind::Set) => {
            let mut s = set::get(coll).s;
            // An equal element that is already there (`1` for `1N`) stays.
            if !s.insert(x) {
                return Ok(*coll);
            }
            Ok(with_meta_of(coll, set::new(s)))
        }
        Some(Kind::Map) => {
            let mut m = map::get(coll).m;
            match x.kind() {
                Some(Kind::Vector) if vector::get(&x).len() == 2 => {
                    let e = vector::get(&x);
                    map::put(&mut m, e[0], e[1]);
                }
                Some(Kind::Map) => {
                    for (k, v) in map::get(&x).m.iter() {
                        map::put(&mut m, *k, *v);
                    }
                }
                None if x.is_nil() => {}
                // Any other map (sorted, record): its entries, one by one.
                Some(Kind::SortedMap | Kind::Instance) if is_map_like(&x) => {
                    for e in to_vec(&x)? {
                        let e = vector::get(&e);
                        map::put(&mut m, e[0], e[1]);
                    }
                }
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
            let mut c = *coll;
            match x.kind() {
                Some(Kind::Vector) if vector::get(&x).len() == 2 => {
                    let e = vector::get(&x);
                    c = assoc(&c, e[0], e[1])?;
                }
                Some(Kind::Map | Kind::SortedMap | Kind::Instance) => {
                    for e in to_vec(&x)? {
                        let e = vector::get(&e);
                        c = assoc(&c, e[0], e[1])?;
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
pub fn with_meta_of(src: &Value, dst: Value) -> Value {
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
            // The element as the set holds it (`1` for a lookup with `1N`).
            set::stored(&set::get(coll).s, k).unwrap_or_else(nf)
        }
        Some(Kind::Vector) => match k.int_value() {
            Some(i) if i >= 0 => vector::get(coll)
                .get(i as usize)
                .cloned()
                .unwrap_or_else(nf),
            _ => nf(),
        },
        Some(Kind::String) => match k.int_value() {
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
                *k
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
            Some(v) => Ok(*v),
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
            let v = vector::get(coll);
            match v.get(i as usize) {
                Some(x) => Ok(*x),
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
                Some(x) => Ok(*x),
                None => oob(l.count as usize),
            }
        }
        Some(Kind::VecSeq) if !is_lazy_kind(coll) => {
            let s = vecseq::get(coll);
            match s.items().get(s.i + i as usize) {
                Some(x) => Ok(*x),
                None => oob(s.here()),
            }
        }
        Some(Kind::Cons | Kind::LazySeq | Kind::VecSeq) => nth_at(Cursor::Own(*coll), i, not_found),
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
            map::put(&mut m, k, v);
            Ok(map::new(m))
        }
        Some(Kind::Map) => {
            let mut m = map::get(coll).m;
            // An equal key keeps its stored form (`1` stays `1` when `1N` is
            // assoc'ed): only the value is replaced.
            map::put(&mut m, k, v);
            Ok(with_meta_of(coll, map::new(m)))
        }
        Some(Kind::Vector) => {
            let vec = vector::get(coll);
            let Some(i) = k.int_value() else {
                return err(ILLEGAL_ARGUMENT, "Key must be integer");
            };
            if i < 0 || i as usize > vec.len() {
                return err(
                    INDEX_OUT_OF_BOUNDS,
                    format!("index {i} out of bounds for length {}", vec.len()),
                );
            }
            Ok(if i as usize == vec.len() {
                vec.conj(v)
            } else {
                vec.assoc(i as usize, v)
            })
        }
        Some(Kind::SortedMap) => {
            let m = sorted::map_assoc(&sorted::map_get(coll).m, k, v)?;
            Ok(with_meta_of(coll, sorted::map_new(m)))
        }
        Some(Kind::Instance) if types::is_record(coll) => {
            let i = types::inst(coll);
            if let Some(ix) = i.ty().field_kws.iter().position(|fk| fk.identical(&k)) {
                return Ok(types::with_field(coll, ix, v));
            }
            let mut ext = map::as_map(&i.ext).map_or_else(map::empty_map, |m| m.m);
            map::put(&mut ext, k, v);
            Ok(types::with(coll, i.fields(), map::new(ext)))
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
            let mut m = map::get(coll).m;
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
            let mut ext = map::as_map(&i.ext).map_or_else(map::empty_map, |m| m.m);
            ext.remove(k);
            let ext = if ext.is_empty() {
                Value::NIL
            } else {
                map::new(ext)
            };
            Ok(types::with(coll, i.fields(), ext))
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
            let mut s = set::get(coll).s;
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
            .int_value()
            .is_some_and(|i| i >= 0 && (i as usize) < vector::get(coll).len()),
        Some(Kind::String) => k
            .int_value()
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
        // The entry as the map holds it (key `1` for a lookup with `1N`).
        Some(Kind::Map) => map::get(coll)
            .m
            .get_key_value(k)
            .map_or(Value::NIL, |(sk, v)| entry(sk, v)),
        Some(Kind::Vector) => match k.int_value() {
            Some(i) if i >= 0 => vector::get(coll)
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
            let e = vector::get(e);
            (e[0], e[1])
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
            let ks = to_vec(coll)?.iter().map(|e| vector::get(e)[0]).collect();
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
            let vs = to_vec(coll)?.iter().map(|e| vector::get(e)[1]).collect();
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
        Some(Kind::Vector) => Ok(vector::get(coll).back().cloned().unwrap_or(Value::NIL)),
        Some(Kind::List) => Ok(list::get(coll).first),
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
            let v = vector::get(coll);
            if v.is_empty() {
                return err(ILLEGAL_ARGUMENT, "Can't pop empty vector");
            }
            Ok(v.pop())
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
        Some(Kind::List | Kind::Cons | Kind::LazySeq | Kind::VecSeq) => list::empty(),
        Some(Kind::Vector) => vector::empty(),
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
        return Ok(crate::num::cmp(&x, &y).unwrap_or(Ordering::Equal));
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
            let (x, y) = (vector::get(a), vector::get(b));
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
