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
    pub file: std::sync::Arc<str>,
    pub line: u32,
    pub col: u32,
}

/// Where a compiled function's frames keep references, for the collector.
///
/// Generated code declares its references to Cranelift, which keeps the ones that
/// are live across a call in stack slots and records those slots per call site
/// (a stack map). With this the collector reads a compiled frame precisely: only
/// the listed slots are roots, and a value that is dead at the call is not one.
#[derive(Clone, Debug, Default)]
pub struct FrameInfo {
    /// Distance from the frame pointer down to the stack pointer while the
    /// function's frame is active (`u32::MAX`: unknown).
    pub fp_to_sp: u32,
    /// Every call site, by the offset of its return address in the function, with
    /// the offsets from the stack pointer of the slots that hold live references
    /// there. Sorted by return offset.
    pub sites: Vec<(u32, Vec<u32>)>,
}

impl FrameInfo {
    /// Little-endian `u32`s: the number of sites, then for each site its return
    /// offset, its slot count and its slot offsets (the form AOT programs carry).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut put = |x: u32| out.extend_from_slice(&x.to_le_bytes());
        put(self.sites.len() as u32);
        for (ret, slots) in &self.sites {
            put(*ret);
            put(slots.len() as u32);
            for s in slots {
                put(*s);
            }
        }
        out
    }

    /// # Safety
    /// `p` must point at a table produced by `encode`.
    unsafe fn decode(fp_to_sp: u32, p: *const u8) -> FrameInfo {
        let p = p as *const u32;
        let mut i = 0usize;
        let mut next = || {
            let x = unsafe { p.add(i).read_unaligned() };
            i += 1;
            u32::from_le(x)
        };
        let n = next();
        let mut sites = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let ret = next();
            let k = next();
            sites.push((ret, (0..k).map(|_| next()).collect()));
        }
        FrameInfo { fp_to_sp, sites }
    }

    /// The live reference slots at the call that returns to `offset`; `None` if
    /// that is not the return address of a call in this function.
    pub fn site(&self, offset: u32) -> Option<&[u32]> {
        self.sites
            .binary_search_by_key(&offset, |s| s.0)
            .ok()
            .map(|i| &self.sites[i].1[..])
    }
}

struct Range {
    end: usize,
    name: String,
    /// Sorted by `start`.
    lines: Vec<Line>,
    frames: Option<FrameInfo>,
    /// Not an rscl fn (a unit body, a loader): not shown in stack traces.
    hidden: bool,
}

/// start → range
type Table = BTreeMap<usize, Range>;

fn table() -> &'static RwLock<Table> {
    static T: OnceLock<RwLock<Table>> = OnceLock::new();
    T.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Register the code range `[start, start+size)` of a compiled function: its name
/// and the source positions of its calls (for stack traces; `hidden` keeps it out
/// of them), and its frame maps (for the collector).
pub fn register_code(
    start: usize,
    size: usize,
    name: &str,
    mut lines: Vec<Line>,
    frames: Option<FrameInfo>,
    hidden: bool,
) {
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
            frames,
            hidden,
        },
    );
}

/// Read access to the code table for a stack walk (one lock for the whole walk).
pub(crate) struct CodeTable(std::sync::RwLockReadGuard<'static, Table>);

pub(crate) fn code_table() -> CodeTable {
    CodeTable(table().read().unwrap_or_else(|e| e.into_inner()))
}

impl CodeTable {
    /// For a return address inside a compiled function with frame maps: the
    /// function's frame size and the live reference slots at that call.
    pub(crate) fn site(&self, ret: usize) -> Option<(u32, &[u32])> {
        let (start, r) = self.0.range(..=ret).next_back()?;
        if ret >= r.end {
            return None;
        }
        let f = r.frames.as_ref()?;
        if f.fp_to_sp == u32::MAX {
            return None;
        }
        Some((f.fp_to_sp, f.site((ret - start) as u32)?))
    }
}

/// The fn containing `addr`, and the source line of the call there if known.
fn lookup(addr: usize, t: &Table) -> Option<(&str, Option<&Line>)> {
    let (start, r) = t.range(..=addr).next_back()?;
    if addr >= r.end {
        return None;
    }
    if r.hidden {
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

/// Name of the compiled fn whose code contains `addr` (collector diagnostics).
pub(crate) fn fn_at(addr: usize) -> Option<String> {
    let t = table().read().unwrap_or_else(|e| e.into_inner());
    lookup(addr, &t).map(|(name, _)| name.to_string())
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
    // Nearly every entry of a fn names the same file: share the string.
    let mut last: Option<((u32, u32), std::sync::Arc<str>)> = None;
    for i in 0..n.max(0) as usize {
        let rec = unsafe { entries.add(i * 24) } as *const u32;
        let field = |k: usize| unsafe { rec.add(k).read_unaligned() };
        let key = (field(4), field(5));
        let file = match &last {
            Some((k, f)) if *k == key => f.clone(),
            _ => {
                let f: std::sync::Arc<str> =
                    unsafe { crate::ffi::str_arg(blob.add(key.0 as usize), key.1 as i64) }.into();
                last = Some((key, f.clone()));
                f
            }
        };
        lines.push(Line {
            start: field(0),
            end: field(1),
            file,
            line: field(2),
            col: field(3),
        });
    }
    register_code(start as usize, size as usize, name, lines, None, false);
}

/// Registration of a compiled function's frame maps (see `FrameInfo::encode`); for
/// a `hidden` function this is its only registration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rt_register_frames(
    start: *const u8,
    size: i64,
    hidden: i64,
    fp_to_sp: i64,
    table_: *const u8,
) {
    let frames = unsafe { FrameInfo::decode(fp_to_sp as u32, table_) };
    let mut t = table().write().unwrap_or_else(|e| e.into_inner());
    match t.get_mut(&(start as usize)) {
        Some(r) => r.frames = Some(frames),
        None => {
            t.insert(
                start as usize,
                Range {
                    end: start as usize + size as usize,
                    name: String::new(),
                    lines: Vec::new(),
                    frames: Some(frames),
                    hidden: hidden != 0,
                },
            );
        }
    }
}
