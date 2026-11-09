//! Native (C ABI) symbol lookup for the typed FFI, the interrupt flag, and the stack
//! guard. Symbols are searched in the libraries opened by `load-native` (newest
//! first), then process-wide: the host binary (built with `-rdynamic`), libc/libm,
//! and plain C libraries, which `load-native` promotes to `RTLD_GLOBAL`.
use std::cell::Cell;
use std::ffi::{CString, c_char, c_int, c_void};
use std::sync::atomic::{AtomicU8, Ordering};

use crate::exc::{RtResult, err};
use crate::obj::exception::{INTERRUPTED, RUNTIME, STACK_OVERFLOW};
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

/// Set by the SIGINT handler; checked at loop headers and fn entries by generated code.
#[unsafe(no_mangle)]
pub static rscl_interrupt_flag: AtomicU8 = AtomicU8::new(0);

pub fn request_interrupt() {
    rscl_interrupt_flag.store(1, Ordering::Release);
}

pub fn interrupt_flag_addr() -> *const u8 {
    &rscl_interrupt_flag as *const AtomicU8 as *const u8
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

/// Loop-header slow path: the flag was set. Clears it and throws.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_interrupted() -> Value {
    rscl_interrupt_flag.store(0, Ordering::Release);
    crate::exc::throw_new(INTERRUPTED, "interrupted");
    Value::EXC
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
    match v.as_int() {
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
        Some(crate::num::Num::I(n)) => {
            unsafe { *out = n as f64 };
            Value::NIL
        }
        Some(crate::num::Num::D(d)) => {
            unsafe { *out = d };
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
