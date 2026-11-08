//! Persistent singly linked list with a cached count. `()` is a distinct empty list
//! object (not `nil`), as in Clojure.
use super::{Kind, alloc, cast};
use crate::value::{ObjHeader, Value};

#[repr(C)]
pub struct ListObj {
    pub hdr: ObjHeader,
    pub first: Value,
    /// The tail: another list, or `nil` for the last cell and for the empty list.
    pub rest: Value,
    pub count: i64,
    pub meta: Value,
}

pub fn empty() -> Value {
    alloc(ListObj {
        hdr: ObjHeader::new(Kind::List),
        first: Value::NIL,
        rest: Value::NIL,
        count: 0,
        meta: Value::NIL,
    })
}

/// Prepend `x` to `tail` (a list or `nil`, treated as empty). Takes ownership of both.
pub fn cons(x: Value, tail: Value) -> Value {
    let count = if tail.is_kind(Kind::List) {
        get(&tail).count
    } else {
        0
    };
    let rest = if count == 0 { Value::NIL } else { tail };
    alloc(ListObj {
        hdr: ObjHeader::new(Kind::List),
        first: x,
        rest,
        count: count + 1,
        meta: Value::NIL,
    })
}

/// Build a list from borrowed elements.
pub fn from_slice(items: &[Value]) -> Value {
    let mut l = empty();
    for x in items.iter().rev() {
        l = cons(x.clone(), l);
    }
    l
}

pub fn from_iter<I: IntoIterator<Item = Value>>(iter: I) -> Value
where
    I::IntoIter: DoubleEndedIterator,
{
    let mut l = empty();
    for x in iter.into_iter().rev() {
        l = cons(x, l);
    }
    l
}

pub fn get(v: &Value) -> &ListObj {
    debug_assert!(v.is_kind(Kind::List));
    unsafe { cast::<ListObj>(v) }
}

pub fn as_list(v: &Value) -> Option<&ListObj> {
    if v.is_kind(Kind::List) {
        Some(get(v))
    } else {
        None
    }
}

impl ListObj {
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            cur: if self.count == 0 { None } else { Some(self) },
        }
    }
    /// The tail as an owned list value (the empty list for a one-element list).
    pub fn rest(&self) -> Value {
        if self.count <= 1 {
            empty()
        } else {
            self.rest.clone()
        }
    }
    pub fn to_vec(&self) -> Vec<Value> {
        self.iter().cloned().collect()
    }
}

pub struct Iter<'a> {
    cur: Option<&'a ListObj>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = &'a Value;
    fn next(&mut self) -> Option<&'a Value> {
        let cell = self.cur?;
        self.cur = if cell.rest.is_kind(Kind::List) {
            Some(get(&cell.rest))
        } else {
            None
        };
        Some(&cell.first)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.cur.map_or(0, |c| c.count as usize);
        (n, Some(n))
    }
}
impl ExactSizeIterator for Iter<'_> {}
