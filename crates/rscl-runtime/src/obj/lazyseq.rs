//! Lazy sequences: a thunk that is run at most once, on first `seq`, and replaced by
//! its result. Thread-safe; a thread that re-enters its own realisation gets an
//! `IllegalStateException` instead of a deadlock.
use std::sync::Mutex;

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

fn thread_id() -> u64 {
    crate::native::thread_id()
}

/// Wrap a zero-argument fn.
pub fn new(thunk: Value) -> Value {
    alloc(LazySeqObj {
        hdr: ObjHeader::new(Kind::LazySeq),
        state: Mutex::new(State::Unrealized(thunk)),
    })
}

/// Visit the value the lazy seq refers to: its thunk or its realised seq (the world
/// is stopped, so the lock is bypassed). While it is being realised the thunk lives
/// on the realising thread's stack.
///
/// # Safety
/// `p` is a live lazy seq and no other thread runs rscl code.
pub(crate) unsafe fn gc_trace(p: *mut LazySeqObj, f: &mut dyn FnMut(&Value)) {
    let o = unsafe { &mut *p };
    match o.state.get_mut().unwrap_or_else(|e| e.into_inner()) {
        State::Unrealized(v) | State::Realized(v) => f(v),
        State::Realizing(_) => {}
    }
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

/// Force the seq; returns the realised seq (`nil` or a seq object).
pub fn realize(v: &Value) -> RtResult<Value> {
    let obj = get(v);
    let me = thread_id();
    let thunk = loop {
        let mut g = obj.state.lock().unwrap_or_else(|e| e.into_inner());
        match &*g {
            State::Realized(s) => return Ok(*s),
            State::Realizing(t) if *t == me => {
                drop(g);
                return err(
                    ILLEGAL_STATE,
                    "lazy seq realised recursively from its own body",
                );
            }
            State::Realizing(_) => {
                // Another thread is realising it: wait as a blocking operation (idle
                // for the collector, interruptible).
                drop(g);
                crate::native::blocking(|| {
                    std::thread::sleep(std::time::Duration::from_micros(200))
                });
                crate::native::check_interrupt()?;
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
            *g = State::Realized(s);
            crate::gc::write_barrier(obj);
            Ok(s)
        }
        Err(e) => {
            *g = State::Unrealized(thunk);
            crate::gc::write_barrier(obj);
            Err(e)
        }
    }
}
