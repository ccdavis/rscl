//! Fn-name stack traces for exceptions, from frame pointers. The compiler registers
//! the code range of every compiled fn; at `throw` time the chain of return
//! addresses is mapped back to names. Rust frames in between are skipped (they are
//! not in the table); generated code and this crate keep frame pointers.
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

/// start → (end, name)
type Table = BTreeMap<usize, (usize, String)>;

fn table() -> &'static RwLock<Table> {
    static T: OnceLock<RwLock<Table>> = OnceLock::new();
    T.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Register the code range `[start, start+size)` of a compiled fn.
pub fn register_code(start: usize, size: usize, name: &str) {
    if size == 0 {
        return;
    }
    table()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(start, (start + size, name.to_string()));
}

fn lookup(addr: usize, t: &Table) -> Option<&str> {
    let (_, (end, name)) = t.range(..=addr).next_back()?;
    if addr < *end { Some(name) } else { None }
}

fn debug_enabled() -> bool {
    static D: OnceLock<bool> = OnceLock::new();
    *D.get_or_init(|| std::env::var_os("RSCL_DEBUG_TRACE").is_some())
}

thread_local! {
    static STACK_BOUNDS: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}

/// The thread's stack bounds, queried from pthreads once per thread.
fn stack_bounds() -> (usize, usize) {
    STACK_BOUNDS.with(|b| match b.get() {
        Some(v) => v,
        None => {
            let v = crate::native::thread_stack_bounds();
            b.set(Some(v));
            v
        }
    })
}

const MAX_FRAMES: usize = 200;

/// Walk the frame-pointer chain from here and collect the rscl fn names found.
#[inline(never)]
pub fn capture() -> Vec<String> {
    let (floor, top) = stack_bounds();
    let mut fp: usize;
    // SAFETY: reading rbp is always fine; it is only used as a bounded pointer below.
    unsafe {
        std::arch::asm!("mov {}, rbp", out(reg) fp, options(nomem, nostack, preserves_flags))
    };
    let t = table().read().unwrap_or_else(|e| e.into_inner());
    let debug = debug_enabled();
    if debug {
        eprintln!(
            "[trace] {} ranges, stack {floor:#x}..{top:#x}, rbp {fp:#x}",
            t.len()
        );
    }
    let mut out = Vec::new();
    let mut prev = 0usize;
    for _ in 0..MAX_FRAMES {
        if fp == 0
            || fp <= prev
            || !fp.is_multiple_of(8)
            || (floor != 0 && (fp < floor || fp + 16 > top))
        {
            if debug {
                eprintln!("[trace] stop at fp {fp:#x}");
            }
            break;
        }
        // SAFETY: fp is inside this thread's stack (checked above) and 8-aligned.
        let (ret, next) = unsafe { (*((fp + 8) as *const usize), *(fp as *const usize)) };
        if ret == 0 {
            break;
        }
        let hit = lookup(ret - 1, &t);
        if debug {
            eprintln!("[trace] fp {fp:#x} ret {ret:#x} -> {:?}", hit);
        }
        if let Some(name) = hit {
            out.push(name.to_string());
        }
        prev = fp;
        fp = next;
    }
    out
}

/// Registration entry point for compiled programs and the JIT.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_register_code(start: *const u8, size: i64, name: *const u8, len: i64) {
    let name = unsafe { crate::ffi::str_arg(name, len) };
    register_code(start as usize, size as usize, name);
}
