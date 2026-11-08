//! Lazy sequences: a thunk that is run at most once, on first `seq`, and replaced by
//! its result. Thread-safe; a thread that re-enters its own realisation gets an
//! `IllegalStateException` instead of a deadlock.
use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{Kind, alloc, cast};
use crate::exc::{RtResult, err};
use crate::obj::exception::ILLEGAL_STATE;
use crate::value::{ObjHeader, Value};

enum State {
    Unrealized(Value),
    Realizing(u64),
    /// The realised seq: `nil` or a seq object.
    Realized(Value),
}

#[repr(C)]
pub struct LazySeqObj {
    pub hdr: ObjHeader,
    state: Mutex<State>,
}

static NEXT_THREAD: AtomicU64 = AtomicU64::new(1);
thread_local! {
    static THREAD_ID: Cell<u64> = const { Cell::new(0) };
}

fn thread_id() -> u64 {
    THREAD_ID.with(|t| {
        if t.get() == 0 {
            t.set(NEXT_THREAD.fetch_add(1, Ordering::Relaxed));
        }
        t.get()
    })
}

/// Wrap a zero-argument fn.
pub fn new(thunk: Value) -> Value {
    alloc(LazySeqObj {
        hdr: ObjHeader::new(Kind::LazySeq),
        state: Mutex::new(State::Unrealized(thunk)),
    })
}

pub fn get(v: &Value) -> &LazySeqObj {
    debug_assert!(v.is_kind(Kind::LazySeq));
    unsafe { cast::<LazySeqObj>(v) }
}

pub fn is_realized(v: &Value) -> bool {
    matches!(
        &*get(v).state.lock().unwrap_or_else(|e| e.into_inner()),
        State::Realized(_)
    )
}

/// Force the seq; returns the realised seq (`nil` or a seq object), owned.
pub fn realize(v: &Value) -> RtResult<Value> {
    let obj = get(v);
    let me = thread_id();
    let thunk = loop {
        let mut g = obj.state.lock().unwrap_or_else(|e| e.into_inner());
        match &*g {
            State::Realized(s) => return Ok(s.clone()),
            State::Realizing(t) if *t == me => {
                drop(g);
                return err(
                    ILLEGAL_STATE,
                    "lazy seq realised recursively from its own body",
                );
            }
            State::Realizing(_) => {
                drop(g);
                std::thread::yield_now();
                continue;
            }
            State::Unrealized(_) => {
                let State::Unrealized(thunk) = std::mem::replace(&mut *g, State::Realizing(me))
                else {
                    unreachable!()
                };
                break thunk;
            }
        }
    };
    let result = thunk.call(&[]).and_then(|r| crate::coll::seq(&r));
    let mut g = obj.state.lock().unwrap_or_else(|e| e.into_inner());
    match result {
        Ok(s) => {
            *g = State::Realized(s.clone());
            Ok(s)
        }
        Err(e) => {
            *g = State::Unrealized(thunk);
            Err(e)
        }
    }
}

/// Take the realised value out for iterative freeing (see `obj::free_tail`).
pub(super) fn take_tail(obj: &mut LazySeqObj) -> Value {
    match std::mem::replace(
        obj.state.get_mut().unwrap_or_else(|e| e.into_inner()),
        State::Realized(Value::NIL),
    ) {
        State::Realized(s) => s,
        _ => Value::NIL,
    }
}
