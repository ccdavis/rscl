//! Heap accounting and the soft memory limit.
//!
//! The global allocator is wrapped so the bytes in use are known at all times (each
//! thread batches its delta and flushes it to a global counter every 64 KB). When
//! the total crosses the limit, the thread that crossed it is asked to throw an
//! `OutOfMemoryError` at its next safepoint (fn entry, loop back-edge or a blocking
//! operation's poll), after one cycle collection has had a chance to bring the heap
//! back under the limit. The limit then re-arms a sixteenth of the limit higher, so a
//! handler can allocate while it reports the error, but a program that keeps
//! allocating keeps being told.
//!
//! The limit comes from `RSCL_MAX_HEAP_MB` (`0`: none); the default is half of the
//! physical memory (or of the cgroup limit, when smaller). The accounting counts
//! bytes requested from the allocator, so the process's resident size is somewhat
//! larger. A hard allocation failure (the allocator itself returns null) still aborts
//! the process, as in any Rust program; a message points at the limit.
use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, AtomicUsize, Ordering};

/// Bytes in use, as flushed by every thread.
static LIVE: AtomicIsize = AtomicIsize::new(0);
/// The soft limit; `usize::MAX` for none.
static LIMIT: AtomicUsize = AtomicUsize::new(usize::MAX);
/// The next byte count at which an error is raised.
static NEXT: AtomicUsize = AtomicUsize::new(usize::MAX);
/// An error was raised and not yet delivered (`OOM_THREAD` says to whom).
static OOM_PENDING: AtomicBool = AtomicBool::new(false);
static OOM_THREAD: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static DELTA: Cell<isize> = const { Cell::new(0) };
}

const FLUSH: isize = 1 << 16;

/// Wraps an allocator with the accounting above.
pub struct Counting<A>(pub A);

// SAFETY: every method forwards to the wrapped allocator and only adds bookkeeping
// on atomics and a `const` thread-local without a destructor (so it is usable at any
// point of a thread's life and never allocates).
unsafe impl<A: GlobalAlloc> GlobalAlloc for Counting<A> {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { self.0.alloc(layout) };
        if p.is_null() {
            hard_failure(layout.size());
        } else {
            account(layout.size() as isize);
        }
        p
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { self.0.alloc_zeroed(layout) };
        if p.is_null() {
            hard_failure(layout.size());
        } else {
            account(layout.size() as isize);
        }
        p
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.0.dealloc(ptr, layout) };
        account(-(layout.size() as isize));
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { self.0.realloc(ptr, layout, new_size) };
        if p.is_null() {
            hard_failure(new_size);
        } else {
            account(new_size as isize - layout.size() as isize);
        }
        p
    }
}

#[inline]
fn account(delta: isize) {
    DELTA.with(|d| {
        let n = d.get() + delta;
        if n.abs() >= FLUSH {
            d.set(0);
            flush(n);
        } else {
            d.set(n);
        }
    });
}

#[cold]
fn flush(n: isize) {
    let live = LIVE.fetch_add(n, Ordering::Relaxed) + n;
    let live = live.max(0) as usize;
    let limit = LIMIT.load(Ordering::Relaxed);
    if live >= NEXT.load(Ordering::Relaxed) {
        NEXT.store(
            live.saturating_add(limit / 16).max(limit),
            Ordering::Relaxed,
        );
        OOM_THREAD.store(crate::native::thread_id(), Ordering::Relaxed);
        OOM_PENDING.store(true, Ordering::Release);
        crate::native::request_oom();
    } else if live < limit && NEXT.load(Ordering::Relaxed) != limit {
        NEXT.store(limit, Ordering::Relaxed);
    }
}

#[cold]
fn hard_failure(size: usize) {
    // No allocation here: the process is about to abort in `handle_alloc_error`.
    let mut buf = [0u8; 160];
    let msg = b"rscl: the allocator could not provide ";
    let tail = b" bytes (RSCL_MAX_HEAP_MB sets the limit at which OutOfMemoryError is thrown)\n";
    let mut n = 0;
    for &b in msg {
        buf[n] = b;
        n += 1;
    }
    let mut digits = [0u8; 20];
    let mut k = 0;
    let mut s = size;
    loop {
        digits[k] = b'0' + (s % 10) as u8;
        k += 1;
        s /= 10;
        if s == 0 {
            break;
        }
    }
    while k > 0 {
        k -= 1;
        buf[n] = digits[k];
        n += 1;
    }
    for &b in tail {
        if n == buf.len() {
            break;
        }
        buf[n] = b;
        n += 1;
    }
    unsafe extern "C" {
        fn write(fd: i32, buf: *const u8, n: usize) -> isize;
    }
    // SAFETY: plain write(2) of a stack buffer to stderr.
    unsafe { write(2, buf.as_ptr(), n) };
}

/// Is a raised error waiting for this thread? Consumes it. The error is retargeted
/// at whoever crosses the limit next, so an unregistered thread (one not running rscl
/// code) cannot keep it forever.
pub fn take_oom() -> bool {
    if !OOM_PENDING.load(Ordering::Acquire) {
        return false;
    }
    let target = OOM_THREAD.load(Ordering::Relaxed);
    if target != crate::native::thread_id() {
        return false;
    }
    OOM_PENDING.swap(false, Ordering::AcqRel)
}

pub fn oom_pending() -> bool {
    OOM_PENDING.load(Ordering::Acquire)
}

/// Bytes in use right now (accurate to a flush per thread).
pub fn used() -> usize {
    LIVE.load(Ordering::Relaxed).max(0) as usize
}

/// The soft limit in bytes, if any.
pub fn limit() -> Option<usize> {
    match LIMIT.load(Ordering::Relaxed) {
        usize::MAX => None,
        l => Some(l),
    }
}

/// `(used, limit)` in bytes when the heap is over its limit.
pub fn over_limit() -> Option<(usize, usize)> {
    let limit = limit()?;
    let used = used();
    (used >= limit).then_some((used, limit))
}

/// Set the limit in bytes (`None`: unlimited).
pub fn set_limit(bytes: Option<usize>) {
    let l = bytes.unwrap_or(usize::MAX);
    LIMIT.store(l, Ordering::Relaxed);
    NEXT.store(l, Ordering::Relaxed);
}

fn physical_memory() -> Option<usize> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: usize = info
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    Some(kb * 1024)
}

fn cgroup_limit() -> Option<usize> {
    for path in [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ] {
        if let Ok(s) = std::fs::read_to_string(path)
            && let Ok(n) = s.trim().parse::<usize>()
            // A "no limit" value is written as a huge number in cgroup v1.
            && n < 1 << 60
        {
            return Some(n);
        }
    }
    None
}

/// Read `RSCL_MAX_HEAP_MB`, or default to half of the memory available to the
/// process. Called once at runtime start.
pub fn configure() {
    match std::env::var("RSCL_MAX_HEAP_MB") {
        Ok(s) => match s.trim().parse::<usize>() {
            Ok(0) => set_limit(None),
            Ok(mb) => set_limit(Some(mb.saturating_mul(1 << 20))),
            Err(_) => {
                eprintln!("rscl: ignoring RSCL_MAX_HEAP_MB={s:?} (expected a number of megabytes)");
                set_limit(None);
            }
        },
        Err(_) => {
            let phys = physical_memory();
            let cg = cgroup_limit();
            let avail = match (phys, cg) {
                (Some(p), Some(c)) => Some(p.min(c)),
                (p, c) => p.or(c),
            };
            set_limit(avail.map(|a| a / 2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounting_tracks_allocations() {
        let before = used();
        let v: Vec<u8> = vec![0; 4 << 20];
        // The thread flushes every 64 KB, so a 4 MB allocation is visible at once.
        assert!(
            used() >= before + (4 << 20) - FLUSH as usize,
            "{} {before}",
            used()
        );
        drop(v);
        assert!(used() < before + (4 << 20));
    }
}
