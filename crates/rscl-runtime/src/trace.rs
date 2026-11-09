//! Stack traces for exceptions, from frame pointers. The compiler registers the
//! code range of every compiled fn together with the source positions of its calls;
//! at `throw` time the chain of return addresses is mapped back to fn names and
//! `file:line`. Rust frames in between are skipped (they are not in the table);
//! generated code and this crate keep frame pointers.
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

/// The code `[start, end)` within a fn (offsets) that belongs to a call at a source
/// position.
#[derive(Clone, Debug)]
pub struct Line {
    pub start: u32,
    pub end: u32,
    pub file: String,
    pub line: u32,
    pub col: u32,
}

struct Range {
    end: usize,
    name: String,
    /// Sorted by `start`.
    lines: Vec<Line>,
}

/// start → range
type Table = BTreeMap<usize, Range>;

fn table() -> &'static RwLock<Table> {
    static T: OnceLock<RwLock<Table>> = OnceLock::new();
    T.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Register the code range `[start, start+size)` of a compiled fn and the source
/// positions of the calls in it.
pub fn register_code(start: usize, size: usize, name: &str, mut lines: Vec<Line>) {
    if size == 0 {
        return;
    }
    lines.sort_by_key(|l| l.start);
    table().write().unwrap_or_else(|e| e.into_inner()).insert(
        start,
        Range {
            end: start + size,
            name: name.to_string(),
            lines,
        },
    );
}

/// The fn containing `addr`, and the source line of the call there if known.
fn lookup(addr: usize, t: &Table) -> Option<(&str, Option<&Line>)> {
    let (start, r) = t.range(..=addr).next_back()?;
    if addr >= r.end {
        return None;
    }
    let off = (addr - start) as u32;
    let i = r.lines.partition_point(|l| l.start <= off);
    let line = i
        .checked_sub(1)
        .map(|i| &r.lines[i])
        .filter(|l| off < l.end);
    Some((&r.name, line))
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
        if let Some((name, line)) = hit {
            out.push(match line {
                Some(l) => format!("{name} ({}:{})", l.file, l.line),
                None => name.to_string(),
            });
        }
        prev = fp;
        fp = next;
    }
    out
}

/// Registration entry point for compiled programs: `entries` points at `n` records
/// of six little-endian `u32`s (start, end, line, col, file offset, file length),
/// the file name being at `blob + offset`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_register_code(
    start: *const u8,
    size: i64,
    name: *const u8,
    len: i64,
    blob: *const u8,
    entries: *const u8,
    n: i64,
) {
    let name = unsafe { crate::ffi::str_arg(name, len) };
    let mut lines = Vec::with_capacity(n.max(0) as usize);
    for i in 0..n.max(0) as usize {
        let rec = unsafe { entries.add(i * 24) } as *const u32;
        let field = |k: usize| unsafe { rec.add(k).read_unaligned() };
        let file = unsafe { crate::ffi::str_arg(blob.add(field(4) as usize), field(5) as i64) };
        lines.push(Line {
            start: field(0),
            end: field(1),
            file: file.to_string(),
            line: field(2),
            col: field(3),
        });
    }
    register_code(start as usize, size as usize, name, lines);
}
