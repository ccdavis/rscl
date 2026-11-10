//! Native (C ABI) symbol lookup for the typed FFI, the interrupt flag, and the stack
//! guard. Symbols are searched in the libraries opened by `load-native` (newest
//! first), then process-wide: the host binary (built with `-rdynamic`), libc/libm,
//! and plain C libraries, which `load-native` promotes to `RTLD_GLOBAL`.
use std::cell::{Cell, RefCell};
use std::ffi::{CString, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use crate::exc::{RtResult, err};
use crate::obj::exception::{INTERRUPTED, OUT_OF_MEMORY, RUNTIME, STACK_OVERFLOW};
use crate::value::Value;

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
    fn pthread_self() -> usize;
    fn pthread_getattr_np(thread: usize, attr: *mut PthreadAttr) -> c_int;
    fn pthread_attr_getstack(
        attr: *const PthreadAttr,
        addr: *mut *mut c_void,
        size: *mut usize,
    ) -> c_int;
    fn pthread_attr_destroy(attr: *mut PthreadAttr) -> c_int;
}

/// Opaque `pthread_attr_t` (56 bytes on x86-64 glibc; oversized for safety).
#[repr(C)]
struct PthreadAttr {
    _data: [u64; 16],
}

/// `(floor, top)` of the current thread's stack from pthreads; `(0, 0)` if unknown.
pub fn thread_stack_bounds() -> (usize, usize) {
    unsafe {
        let mut attr = PthreadAttr { _data: [0; 16] };
        if pthread_getattr_np(pthread_self(), &mut attr) != 0 {
            return (0, 0);
        }
        let mut addr: *mut c_void = std::ptr::null_mut();
        let mut size: usize = 0;
        let rc = pthread_attr_getstack(&attr, &mut addr, &mut size);
        pthread_attr_destroy(&mut attr);
        if rc != 0 || addr.is_null() {
            (0, 0)
        } else {
            (addr as usize, addr as usize + size)
        }
    }
}

/// The lowest usable address of the current thread's stack; 0 if unknown.
fn thread_stack_floor() -> usize {
    thread_stack_bounds().0
}

pub const RTLD_NOW: c_int = 2;
pub const RTLD_LOCAL: c_int = 0;
pub const RTLD_GLOBAL: c_int = 0x100;

/// `dlopen` (always `RTLD_NOW`) with the error turned into a `RuntimeException` that
/// names `display` (the path as the user wrote it).
pub fn dlopen_checked(path: &str, display: &str, flags: c_int) -> RtResult<*mut c_void> {
    let c = CString::new(path)
        .map_err(|_| crate::exc::throw_new(RUNTIME, "library path contains NUL"))?;
    let h = unsafe { dlopen(c.as_ptr(), RTLD_NOW | flags) };
    if h.is_null() {
        let msg = unsafe {
            let e = dlerror();
            if e.is_null() {
                "unknown error".to_string()
            } else {
                std::ffi::CStr::from_ptr(e).to_string_lossy().into_owned()
            }
        };
        return err(
            RUNTIME,
            format!("cannot load native library {display}: {msg}"),
        );
    }
    Ok(h)
}

/// `dlopen` a shared library globally so its symbols become visible to `lookup`.
pub fn load_library(path: &str) -> RtResult<()> {
    dlopen_checked(path, path, RTLD_GLOBAL).map(drop)
}

/// Handles of libraries opened by `load-native`, newest first, so a reloaded library's
/// symbols win over the previous copy's.
static HANDLES: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

pub fn remember_handle(h: *mut c_void) {
    let mut v = HANDLES.lock().unwrap_or_else(|e| e.into_inner());
    v.retain(|&x| x != h as usize);
    v.insert(0, h as usize);
}

/// Address of a C symbol: libraries opened by `load-native` (newest first), then the
/// whole process. On a miss the standard libraries are opened once and the lookup
/// retried, so `cos` works even in a host that was linked without libm.
pub fn lookup(name: &str) -> *const u8 {
    let Ok(c) = CString::new(name) else {
        return std::ptr::null();
    };
    let handles: Vec<usize> = HANDLES.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for h in handles {
        let p = unsafe { dlsym(h as *mut c_void, c.as_ptr()) as *const u8 };
        if !p.is_null() {
            return p;
        }
    }
    let p = unsafe { dlsym(std::ptr::null_mut(), c.as_ptr()) as *const u8 };
    if !p.is_null() {
        return p;
    }
    static STD_LIBS: std::sync::Once = std::sync::Once::new();
    STD_LIBS.call_once(|| {
        for lib in ["libm.so.6", "libc.so.6", "libdl.so.2"] {
            let _ = load_library(lib);
            let _ = crate::exc::take();
        }
    });
    unsafe { dlsym(std::ptr::null_mut(), c.as_ptr()) as *const u8 }
}

// ---- interrupt flag and stack guard ---------------------------------------------------

/// Checked at loop headers and fn entries by generated code: non-zero when some
/// thread has something to do at its next safepoint (Ctrl-C for the main thread,
/// `future-cancel` for a worker, a collection for everyone). Threads that hit the
/// slow path and find nothing for them continue.
#[unsafe(no_mangle)]
pub static rscl_interrupt_flag: AtomicU8 = AtomicU8::new(0);

static CTRL_C: AtomicBool = AtomicBool::new(false);
/// Cancellations raised for threads running compiled code and not yet consumed.
static PENDING_CANCELS: AtomicUsize = AtomicUsize::new(0);
static GC_PENDING: AtomicBool = AtomicBool::new(false);

fn anything_pending() -> bool {
    CTRL_C.load(Ordering::Acquire)
        || PENDING_CANCELS.load(Ordering::Acquire) > 0
        || GC_PENDING.load(Ordering::Acquire)
        || crate::heap::oom_pending()
}

/// Recompute the fast-path flag. Lock-free: every request sets its own atomic before
/// raising the flag, so re-checking after the store cannot lose a request.
fn refresh_flag() {
    let pending = anything_pending();
    rscl_interrupt_flag.store(u8::from(pending), Ordering::Release);
    if !pending && anything_pending() {
        rscl_interrupt_flag.store(1, Ordering::Release);
    }
}

/// Ctrl-C: interrupt the main thread's evaluation. Called from the signal handler,
/// so only atomics.
pub fn request_interrupt() {
    CTRL_C.store(true, Ordering::Release);
    rscl_interrupt_flag.store(1, Ordering::Release);
}

/// Forget a Ctrl-C nobody consumed (the REPL, before it evaluates the next input).
pub fn clear_ctrl_c() {
    CTRL_C.store(false, Ordering::Release);
    refresh_flag();
}

pub fn ctrl_c_pending() -> bool {
    CTRL_C.load(Ordering::Acquire)
}

/// `future-cancel` of a worker running compiled code: make every thread check.
pub fn request_cancel() {
    PENDING_CANCELS.fetch_add(1, Ordering::AcqRel);
    rscl_interrupt_flag.store(1, Ordering::Release);
}

/// A counted cancellation request was consumed.
pub fn cancel_consumed() {
    PENDING_CANCELS.fetch_sub(1, Ordering::AcqRel);
    refresh_flag();
}

/// The heap crossed its limit: the thread that did so must notice at its next
/// safepoint. Called from inside the allocator, so only atomics.
pub fn request_oom() {
    rscl_interrupt_flag.store(1, Ordering::Release);
}

/// A pending heap-limit error was dropped without being delivered.
pub fn oom_cleared() {
    refresh_flag();
}

/// The collector wants every thread at a safepoint.
pub fn request_safepoint() {
    GC_PENDING.store(true, Ordering::Release);
    rscl_interrupt_flag.store(1, Ordering::Release);
}

pub fn safepoint_done() {
    GC_PENDING.store(false, Ordering::Release);
    refresh_flag();
}

pub fn interrupt_flag_addr() -> *const u8 {
    &rscl_interrupt_flag as *const AtomicU8 as *const u8
}

static NEXT_THREAD: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static THREAD_ID: Cell<u64> = const { Cell::new(0) };
    static IS_MAIN: Cell<bool> = const { Cell::new(false) };
    /// The future this worker thread is computing (for cancellation).
    static CURRENT_FUTURE: RefCell<Option<Value>> = const { RefCell::new(None) };
    /// Inside a blocking runtime operation: stopped for the collector, and cancellation
    /// is noticed on the way out instead of through the global flag.
    static BLOCKED: Cell<bool> = const { Cell::new(false) };
}

/// A small per-thread id (lazy seq ownership, locks, delays).
pub fn thread_id() -> u64 {
    THREAD_ID.with(|t| {
        if t.get() == 0 {
            t.set(NEXT_THREAD.fetch_add(1, Ordering::Relaxed));
        }
        t.get()
    })
}

/// Mark the calling thread as the one Ctrl-C interrupts.
pub fn set_main_thread() {
    IS_MAIN.with(|m| m.set(true));
}

pub fn set_current_future(f: Option<Value>) {
    CURRENT_FUTURE.with(|c| *c.borrow_mut() = f);
}

fn with_current_future<R>(f: impl FnOnce(&crate::obj::pending::PendingObj) -> R) -> Option<R> {
    CURRENT_FUTURE.with(|c| c.borrow().as_ref().map(|v| f(crate::obj::pending::get(v))))
}

/// The future this worker computes, as a root for the collector.
pub(crate) fn future_root(out: &mut Vec<u64>) {
    let _ = CURRENT_FUTURE.try_with(|c| {
        if let Ok(c) = c.try_borrow()
            && let Some(v) = c.as_ref()
        {
            out.push(v.bits());
        }
    });
}

/// Run a blocking runtime operation (`sleep`, a blocking `deref`, `locking`,
/// `read-line`): the thread counts as stopped for the collector, and a cancellation
/// targets it directly. `f` must not touch rscl values or allocate objects (see
/// `gc::blocking`); a cancellation that arrived meanwhile is raised on the way out,
/// so the next safepoint or `check_interrupt` delivers it.
pub fn blocking<R>(f: impl FnOnce() -> R) -> R {
    if is_blocked() {
        return f();
    }
    struct Leave;
    impl Drop for Leave {
        fn drop(&mut self) {
            with_current_future(|p| {
                p.worker_blocked.store(false, Ordering::SeqCst);
                if p.cancel.load(Ordering::SeqCst) {
                    p.raise_cancel();
                }
            });
            BLOCKED.with(|b| b.set(false));
        }
    }
    BLOCKED.with(|b| b.set(true));
    with_current_future(|p| p.worker_blocked.store(true, Ordering::Release));
    let _leave = Leave;
    crate::gc::blocking(f)
}

pub fn is_blocked() -> bool {
    BLOCKED.with(|b| b.get())
}

/// Deliver a pending interrupt to this thread, if there is one for it: Ctrl-C on the
/// main thread, the cancellation of the future it is computing, or the heap limit
/// it crossed. A pending collection is joined first (unless the thread is inside a
/// blocking operation, where it already counts as stopped). The result is the
/// exception class and message to throw.
fn take_interrupt() -> Option<(&'static str, String)> {
    let stress = crate::gc::stress_tick();
    if !is_blocked() && (GC_PENDING.load(Ordering::Acquire) || crate::gc::stopping()) {
        crate::gc::safepoint();
    }
    if stress {
        // Stress mode keeps every thread on the slow path.
        rscl_interrupt_flag.store(1, Ordering::Release);
    }
    if crate::heap::take_oom(!is_blocked()) {
        refresh_flag();
        // Garbage may hold the memory: collect once before giving up.
        if !is_blocked() {
            crate::gc::collect_now();
        }
        if let Some((used, limit)) = crate::heap::over_limit() {
            return Some((
                OUT_OF_MEMORY,
                format!(
                    "heap limit exceeded: {} MB in use, limit {} MB (RSCL_MAX_HEAP_MB)",
                    used >> 20,
                    limit >> 20
                ),
            ));
        }
    }
    if IS_MAIN.with(|m| m.get()) && CTRL_C.swap(false, Ordering::AcqRel) {
        refresh_flag();
        return Some((INTERRUPTED, "interrupted".to_string()));
    }
    let cancelled = with_current_future(|p| {
        if p.cancel.swap(false, Ordering::AcqRel) {
            if p.cancel_counted.swap(false, Ordering::AcqRel) {
                cancel_consumed();
            }
            true
        } else {
            false
        }
    })
    .unwrap_or(false);
    if cancelled {
        return Some((INTERRUPTED, "future cancelled".to_string()));
    }
    None
}

/// For blocking runtime operations: throw if an interrupt is pending for this
/// thread (checked directly; the global flag may not be raised for a blocked thread).
pub fn check_interrupt() -> crate::exc::RtResult<()> {
    if let Some((class, msg)) = take_interrupt() {
        return crate::exc::err(class, msg);
    }
    Ok(())
}

thread_local! {
    static STACK_LIMIT: Cell<usize> = const { Cell::new(0) };
}

/// Safety margin kept free below the limit so the runtime can still build and
/// report the `StackOverflowError`.
const STACK_MARGIN: usize = 1 << 20;

/// Call near the start of a thread whose stack is `size` bytes.
pub fn set_stack_limit_here(size: usize) {
    let here = &size as *const usize as usize;
    let limit = here.saturating_sub(size.saturating_sub(STACK_MARGIN));
    STACK_LIMIT.with(|l| l.set(limit));
}

/// The guard limit for this thread, discovering the stack bounds on first use.
fn stack_limit() -> usize {
    let limit = STACK_LIMIT.with(|l| l.get());
    if limit != 0 {
        return limit;
    }
    let floor = thread_stack_floor();
    // usize::MAX marks "unknown": never trips.
    let limit = if floor == 0 {
        usize::MAX
    } else {
        floor + STACK_MARGIN
    };
    STACK_LIMIT.with(|l| l.set(limit));
    limit
}

/// Is this thread's stack nearly used up? For recursive natives (`=`, `hash`, the
/// reader), which the fn-entry guard of compiled code does not cover.
pub fn stack_low() -> bool {
    let marker = 0u8;
    let limit = stack_limit();
    limit != usize::MAX && (&marker as *const u8 as usize) < limit
}

/// Fn-entry check: stack depth and pending interrupt. Returns `nil` or `EXC`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_stack_check(sp: usize) -> Value {
    let limit = stack_limit();
    if limit != usize::MAX && sp < limit {
        crate::exc::throw_new(STACK_OVERFLOW, "stack overflow");
        return Value::EXC;
    }
    if rscl_interrupt_flag.load(Ordering::Relaxed) != 0 {
        return unsafe { rt_interrupted() };
    }
    Value::NIL
}

/// Loop-header slow path: the flag was set. Throws `InterruptedException` if the
/// interrupt is for this thread; otherwise returns `nil` and the thread goes on
/// (after taking part in a collection, if one is pending).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_interrupted() -> Value {
    match take_interrupt() {
        Some((class, msg)) => {
            crate::exc::throw_new(class, &msg);
            Value::EXC
        }
        None => Value::NIL,
    }
}

/// Resolve a native symbol for the typed FFI at unit initialisation; null if missing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_native_symbol(name: *const u8, len: i64) -> *const u8 {
    let name = unsafe { crate::ffi::str_arg(name, len) };
    lookup(name)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_throw_unresolved_native(name: *const u8, len: i64) -> Value {
    let name = unsafe { crate::ffi::str_arg(name, len) };
    crate::exc::throw_new(
        RUNTIME,
        format!("unresolved native symbol: {name} (is its library loaded?)"),
    );
    Value::EXC
}

// ---- argument conversion for native calls ------------------------------------------

/// Write the integer value of `v` to `out`; returns `nil` or `EXC`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_to_long(v: crate::value::VRef, out: *mut i64) -> Value {
    match v.int_value() {
        Some(n) => {
            unsafe { *out = n };
            Value::NIL
        }
        None => match v.as_double() {
            Some(d) => {
                unsafe { *out = d as i64 };
                Value::NIL
            }
            None => {
                crate::exc::throw_new(
                    crate::obj::exception::CLASS_CAST,
                    format!(
                        "{} cannot be passed as a native integer",
                        crate::core::type_name(&v)
                    ),
                );
                Value::EXC
            }
        },
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_to_double(v: crate::value::VRef, out: *mut f64) -> Value {
    match crate::num::num(&v) {
        Some(n) => {
            unsafe { *out = crate::num::f(&n) };
            Value::NIL
        }
        None => {
            crate::exc::throw_new(
                crate::obj::exception::CLASS_CAST,
                format!(
                    "{} cannot be passed as a native double",
                    crate::core::type_name(&v)
                ),
            );
            Value::EXC
        }
    }
}

/// Write a NUL-terminated pointer (valid while the string value lives); `nil` → null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_to_cstr(v: crate::value::VRef, out: *mut *const c_char) -> Value {
    if v.is_nil() {
        unsafe { *out = std::ptr::null() };
        return Value::NIL;
    }
    if v.is_kind(crate::obj::Kind::String) {
        unsafe { *out = crate::obj::string::cstr(&v) };
        return Value::NIL;
    }
    crate::exc::throw_new(
        crate::obj::exception::CLASS_CAST,
        format!(
            "{} cannot be passed as a native string",
            crate::core::type_name(&v)
        ),
    );
    Value::EXC
}

/// Copy a C string returned by a native call (null → `nil`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_string_from_cstr(p: *const c_char) -> Value {
    if p.is_null() {
        return Value::NIL;
    }
    crate::obj::string::from_string(
        unsafe { std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// `catch` matching: does `e` belong to `class`?
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_exception_instance(
    e: crate::value::VRef,
    class: *const u8,
    len: i64,
) -> i64 {
    let class = unsafe { crate::ffi::str_arg(class, len) };
    let hit = match crate::obj::exception::as_exc(&e) {
        Some(x) => x.is_instance(class),
        None => matches!(class, "Throwable" | "Object"),
    };
    hit as i64
}
