//! Values that arrive later: futures (computed on another thread), promises
//! (delivered by anyone) and delays (computed on first deref by the dereferencing
//! thread). One object kind with a flavor; `deref` blocks until the value is there.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use super::exception::{CANCELLATION, ILLEGAL_STATE};
use super::{Kind, alloc, cast};
use crate::exc::{self, Exc, RtResult, err};
use crate::value::{ObjHeader, Value};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flavor {
    Future,
    Promise,
    Delay,
}

impl Flavor {
    pub fn name(self) -> &'static str {
        match self {
            Flavor::Future => "Future",
            Flavor::Promise => "Promise",
            Flavor::Delay => "Delay",
        }
    }
}

enum State {
    Pending,
    Done(Value),
    /// The computation threw; every deref rethrows it.
    Failed(Value),
    Cancelled,
}

#[repr(C)]
pub struct PendingObj {
    pub hdr: ObjHeader,
    pub flavor: Flavor,
    state: Mutex<State>,
    cv: Condvar,
    /// A delay's thunk until it is forced (the lock also serialises forcing).
    thunk: Mutex<Option<Value>>,
    /// Thread forcing the delay, to report a recursive deref instead of deadlocking.
    forcing: AtomicU64,
    /// A future's cancellation request, consumed by its worker thread at a safepoint.
    pub cancel: AtomicBool,
    /// The request raised the global flag (the worker was running compiled code).
    pub cancel_counted: AtomicBool,
    /// The worker is inside a blocking runtime operation (set by the worker).
    pub worker_blocked: AtomicBool,
    /// Cycle-collector bookkeeping (see `gc.rs`): buffered as a candidate, and a
    /// counter of writes and lost references.
    pub buffered: AtomicBool,
    pub version: std::sync::atomic::AtomicU32,
}

pub fn new(flavor: Flavor, thunk: Option<Value>) -> Value {
    alloc(PendingObj {
        hdr: ObjHeader::new(Kind::Pending),
        flavor,
        state: Mutex::new(State::Pending),
        cv: Condvar::new(),
        thunk: Mutex::new(thunk),
        forcing: AtomicU64::new(0),
        cancel: AtomicBool::new(false),
        cancel_counted: AtomicBool::new(false),
        worker_blocked: AtomicBool::new(false),
        buffered: AtomicBool::new(false),
        version: std::sync::atomic::AtomicU32::new(0),
    })
}

/// Slots the collector walks: the outcome (or the delay's thunk).
///
/// # Safety
/// The world is stopped and `p` is a live pending object.
pub(crate) unsafe fn gc_slots(p: *mut PendingObj) -> Vec<*const Value> {
    let o = unsafe { &mut *p };
    let mut v = Vec::new();
    match o.state.get_mut().unwrap_or_else(|e| e.into_inner()) {
        State::Done(x) | State::Failed(x) => v.push(x as *const Value),
        _ => {}
    }
    if let Some(t) = o.thunk.get_mut().unwrap_or_else(|e| e.into_inner()) {
        v.push(t as *const Value);
    }
    v
}

/// Drop every reference a garbage pending object holds.
///
/// # Safety
/// The world is stopped and `p` is an unreachable pending object.
pub(crate) unsafe fn gc_unlink(p: *mut PendingObj) {
    let o = unsafe { &mut *p };
    *o.state.get_mut().unwrap_or_else(|e| e.into_inner()) = State::Cancelled;
    *o.thunk.get_mut().unwrap_or_else(|e| e.into_inner()) = None;
}

pub fn get(v: &Value) -> &PendingObj {
    debug_assert!(v.is_kind(Kind::Pending));
    unsafe { cast::<PendingObj>(v) }
}

pub fn as_pending(v: &Value) -> Option<&PendingObj> {
    if v.is_kind(Kind::Pending) {
        Some(get(v))
    } else {
        None
    }
}

/// How often a blocked deref checks for an interrupt or cancellation.
const POLL: Duration = Duration::from_millis(20);

impl PendingObj {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A write may close a reference cycle through this object: make it a candidate.
    fn noted(&self) {
        let v = std::mem::ManuallyDrop::new(unsafe {
            Value::from_raw(self as *const PendingObj as u64)
        });
        crate::gc::note_write(&v);
    }

    /// Set the value; false if one is already there (or the future was cancelled).
    pub fn deliver(&self, v: Value) -> bool {
        let mut st = self.lock();
        if !matches!(*st, State::Pending) {
            return false;
        }
        *st = State::Done(v);
        self.cv.notify_all();
        drop(st);
        self.noted();
        true
    }

    /// Record a thrown exception as the outcome.
    pub fn fail(&self, e: Value) -> bool {
        let mut st = self.lock();
        if !matches!(*st, State::Pending) {
            return false;
        }
        *st = State::Failed(e);
        self.cv.notify_all();
        drop(st);
        self.noted();
        true
    }

    /// `future-cancel`: true if the future was still pending. The worker is told to
    /// stop at its next safepoint; if it is inside a blocking runtime operation it
    /// notices on the way out, so the global flag is raised only for a worker that is
    /// running compiled code.
    pub fn cancel(&self) -> bool {
        let mut st = self.lock();
        if self.flavor != Flavor::Future || !matches!(*st, State::Pending) {
            return false;
        }
        *st = State::Cancelled;
        // Dekker-style with `leave_blocking` (store cancel, then read blocked;
        // the worker stores unblocked, then reads cancel): one side raises the flag.
        self.cancel.store(true, Ordering::SeqCst);
        if !self.worker_blocked.load(Ordering::SeqCst) {
            self.raise_cancel();
        }
        self.cv.notify_all();
        true
    }

    /// Raise the global flag for this cancellation (once).
    pub fn raise_cancel(&self) {
        if !self.cancel_counted.swap(true, Ordering::SeqCst) {
            crate::native::request_cancel();
        }
    }

    pub fn is_realized(&self) -> bool {
        !matches!(*self.lock(), State::Pending)
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(*self.lock(), State::Cancelled)
    }

    /// `deref`: the value, or `None` when `timeout` elapses first. A future that
    /// threw rethrows; a cancelled one throws `CancellationException`. Waiting is
    /// interruptible (Ctrl-C on the main thread, `future-cancel` on a worker).
    pub fn deref(&self, timeout: Option<Duration>) -> RtResult<Option<Value>> {
        if self.flavor == Flavor::Delay {
            return self.force().map(Some);
        }
        let deadline = timeout.map(|d| std::time::Instant::now() + d);
        let st = self.lock();
        if !matches!(*st, State::Pending) {
            return self.finished(st);
        }
        drop(st);
        crate::native::enter_blocking();
        let r = self.wait(deadline);
        crate::native::leave_blocking();
        r
    }

    fn finished(&self, st: std::sync::MutexGuard<'_, State>) -> RtResult<Option<Value>> {
        match &*st {
            State::Done(v) => Ok(Some(v.clone())),
            State::Failed(e) => {
                let e = e.clone();
                drop(st);
                Err(exc::throw(e))
            }
            State::Cancelled => {
                drop(st);
                err(CANCELLATION, "future was cancelled")
            }
            State::Pending => Ok(None),
        }
    }

    fn wait(&self, deadline: Option<std::time::Instant>) -> RtResult<Option<Value>> {
        let mut st = self.lock();
        loop {
            match &*st {
                State::Done(v) => return Ok(Some(v.clone())),
                State::Failed(e) => {
                    let e = e.clone();
                    drop(st);
                    return Err(exc::throw(e));
                }
                State::Cancelled => {
                    drop(st);
                    return err(CANCELLATION, "future was cancelled");
                }
                State::Pending => {}
            }
            let slice = match deadline {
                Some(d) => {
                    let now = std::time::Instant::now();
                    if now >= d {
                        return Ok(None);
                    }
                    (d - now).min(POLL)
                }
                None => POLL,
            };
            let (guard, _) = self
                .cv
                .wait_timeout(st, slice)
                .unwrap_or_else(|e| e.into_inner());
            st = guard;
            if matches!(*st, State::Pending) {
                drop(st);
                crate::native::check_interrupt()?;
                st = self.lock();
            }
        }
    }

    /// Force a delay on the current thread (once; later derefs reuse the outcome).
    fn force(&self) -> RtResult<Value> {
        let me = crate::native::thread_id();
        if self.forcing.load(Ordering::Acquire) == me {
            return err(ILLEGAL_STATE, "Recursive delay dereference");
        }
        // Another thread may be forcing it: waiting for the lock is a blocking
        // operation (idle for the collector).
        crate::native::enter_blocking();
        let mut slot = self.thunk.lock().unwrap_or_else(|e| e.into_inner());
        crate::native::leave_blocking();
        match &*self.lock() {
            State::Done(v) => return Ok(v.clone()),
            State::Failed(e) => return Err(exc::throw(e.clone())),
            _ => {}
        }
        let Some(thunk) = slot.take() else {
            return err(ILLEGAL_STATE, "delay has no thunk");
        };
        self.forcing.store(me, Ordering::Release);
        let r = crate::ffi::invoke_root(&thunk, &[]);
        self.forcing.store(0, Ordering::Release);
        let out = match r {
            Ok(v) => {
                *self.lock() = State::Done(v.clone());
                Ok(v)
            }
            Err(Exc) => {
                let e = exc::take().unwrap_or(Value::NIL);
                *self.lock() = State::Failed(e.clone());
                Err(exc::throw(e))
            }
        };
        self.noted();
        out
    }
}
