//! The object heap: a reserved address range carved into 64 KB blocks, each holding
//! objects of one size class, with a bitmap of the slots in use.
//!
//! Heap objects do not come from the allocator the rest of the runtime uses, for
//! two reasons. The collector scans stacks conservatively, so it must decide for an
//! arbitrary word whether it is the address of an object: here that is a range
//! check, a table lookup and a bit test (pointers into the middle of an object
//! resolve to its start the same way). And a stale pointer to a freed `String` or
//! `Vec` left in some frame can never turn out to be the address of a newer object,
//! which with a shared allocator kept whole lazy sequences alive.
//!
//! Allocation is the cheap part by design, since everything a Lisp program does
//! allocates. A thread takes blocks for itself and, inside a block, claims a run
//! of free slots that it then hands out by moving a pointer: the common case of
//! `alloc` is a comparison, an addition and setting the slot's bit, with the size
//! class folded to a constant at each call site. A collection hands every block
//! back, frees the unmarked slots and releases blocks that became empty to the
//! operating system.
//!
//! Reclaiming is cheap for most garbage too. Objects that own Rust data (long
//! strings, sorted collections, big integers, regexes, exceptions) live in blocks
//! of their own; the others (cons cells, closures, lazy-seq cells, boxed numbers,
//! strings, records, vectors, maps, sets and their nodes) need no finalising, so
//! the sweep frees the dead ones in such a block by clearing bits a word at a
//! time, without reading them.
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::value::ObjHeader;

/// Empty blocks kept mapped after a collection (64 MB): what the next cycle is
/// likely to allocate into.
const KEEP_FREE: usize = 1024;
const BLOCK_SHIFT: usize = 16;
pub const BLOCK: usize = 1 << BLOCK_SHIFT;
/// Smallest slot; every size class is a multiple of it.
const GRAIN: usize = 16;
/// Bitmap words per block (one bit per `GRAIN` bytes).
const WORDS: usize = BLOCK / GRAIN / 64;
/// Classes 1..=32 are multiples of 16 bytes up to 512 (a full node of a persistent
/// vector is 280 bytes); then powers of two to a block.
const SMALL_CLASSES: usize = 32;
/// log2 of the largest small class.
const SMALL_SHIFT: usize = 9;
pub const NCLASSES: usize = SMALL_CLASSES + (BLOCK_SHIFT - SMALL_SHIFT) + 1;
/// Blocks come in two sorts per size class: *plain* blocks hold only objects
/// that own nothing outside the object heap (cons cells, closures, boxed
/// numbers), *drop* blocks hold objects with Rust data to drop when they die
/// (strings, collections). A plain block is swept without looking at the dead
/// objects in it. A pool is a (class, sort) pair.
const NPOOLS: usize = 2 * NCLASSES;
const _: () = assert!(NCLASSES <= CLASS_MASK as usize + 1);
/// A block's class byte: the size class (0: the block is free), and two flags.
const CLASS_MASK: u8 = 0x3f;
/// The block holds a permanent object: the sweep looks at each unreached object
/// before releasing it.
const HAS_PERMANENT: u8 = 0x40;
/// A drop block.
const DROPS: u8 = 0x80;
/// Longest run of free slots a thread claims at a time, in bytes (what it has
/// claimed counts as allocated).
const MAX_RUN: usize = 8 << 10;

#[inline]
fn class_of(size: usize) -> usize {
    if size <= GRAIN * SMALL_CLASSES {
        size.div_ceil(GRAIN).max(1)
    } else {
        let p = size.next_power_of_two();
        assert!(
            p <= BLOCK,
            "object of {size} bytes is too large for the heap"
        );
        SMALL_CLASSES + (p.trailing_zeros() as usize - SMALL_SHIFT)
    }
}

#[inline]
fn class_size(class: usize) -> usize {
    if class <= SMALL_CLASSES {
        class * GRAIN
    } else {
        1 << (class - SMALL_CLASSES + SMALL_SHIFT)
    }
}

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}
const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_PRIVATE: i32 = 2;
const MAP_ANONYMOUS: i32 = 0x20;
const MAP_NORESERVE: i32 = 0x4000;
const MADV_DONTNEED: i32 = 4;

fn reserve(len: usize) -> Option<usize> {
    let p = unsafe {
        mmap(
            std::ptr::null_mut(),
            len,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE,
            -1,
            0,
        )
    };
    (p as isize != -1 && !p.is_null()).then_some(p as usize)
}

/// The reserved ranges: object blocks, one class byte per block, and four bitmaps
/// with one bit per 16 bytes of heap; the bit of an object is
/// `(address - base) / 16` in each.
///
/// * `bitmaps`: the slot is in use.
/// * `marks`: reached in the collection that is running (scratch between
///   collections).
/// * `olds`: the object is old: it has survived two collections, and minor
///   collections no longer look at it.
/// * `ageds`: the object has survived one collection. It is still young, so it is
///   traced and swept like a new object, but its next survival makes it old.
struct Layout {
    base: usize,
    blocks: usize,
    classes: usize,
    bitmaps: usize,
    marks: usize,
    olds: usize,
    ageds: usize,
}

fn layout() -> &'static Layout {
    static L: OnceLock<Layout> = OnceLock::new();
    L.get_or_init(|| {
        // Address space only; pages are committed when touched. Smaller
        // reservations are tried when the system refuses a large one.
        for gb in [256usize, 64, 16, 4, 1] {
            let blocks = (gb << 30) >> BLOCK_SHIFT;
            // The heap itself, the class bytes, and the four bitmaps.
            let sizes = [
                blocks << BLOCK_SHIFT,
                blocks,
                blocks * WORDS * 8,
                blocks * WORDS * 8,
                blocks * WORDS * 8,
                blocks * WORDS * 8,
            ];
            let mut got = [0usize; 6];
            let mut ok = true;
            for (i, &size) in sizes.iter().enumerate() {
                match reserve(size) {
                    Some(p) => got[i] = p,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                // Give back what this attempt did get before trying a smaller one.
                for (i, &p) in got.iter().enumerate() {
                    if p != 0 {
                        unsafe { munmap(p as *mut u8, sizes[i]) };
                    }
                }
                continue;
            }
            return Layout {
                base: got[0],
                blocks,
                classes: got[1],
                bitmaps: got[2],
                marks: got[3],
                olds: got[4],
                ageds: got[5],
            };
        }
        panic!("cannot reserve address space for the object heap");
    })
}

/// Lowest and one-past-highest address objects have been allocated at (the range
/// check of the stack scan).
static USED_END: AtomicUsize = AtomicUsize::new(0);
/// Bumped by every collection: threads then drop the blocks they held.
static EPOCH: AtomicU64 = AtomicU64::new(1);

struct Pool {
    /// Blocks handed out so far (indices below this may be in use).
    next: usize,
    /// Empty blocks whose memory is still mapped (the most recently emptied last).
    free: Vec<u32>,
    /// Empty blocks whose memory was given back to the operating system.
    released: Vec<u32>,
    /// Per pool: blocks with free slots that no thread holds.
    partial: [Vec<u32>; NPOOLS],
    /// Blocks threads have taken since the last collection: every object
    /// allocated since then is in one of them.
    young: Vec<u32>,
}

fn pool() -> MutexGuard<'static, Pool> {
    static P: OnceLock<Mutex<Pool>> = OnceLock::new();
    P.get_or_init(|| {
        Mutex::new(Pool {
            next: 0,
            free: Vec::new(),
            released: Vec::new(),
            partial: [const { Vec::new() }; NPOOLS],
            young: Vec::new(),
        })
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

#[inline]
fn block_addr(l: &Layout, block: usize) -> usize {
    l.base + (block << BLOCK_SHIFT)
}

#[inline]
unsafe fn class_ptr(l: &Layout, block: usize) -> *mut u8 {
    (l.classes + block) as *mut u8
}

/// The size class of a block (0: free).
#[inline]
unsafe fn class_at(l: &Layout, block: usize) -> usize {
    (unsafe { *class_ptr(l, block) } & CLASS_MASK) as usize
}

/// The pool a block with this class byte belongs to.
#[inline]
fn pool_of(byte: u8) -> usize {
    (byte & CLASS_MASK) as usize + if byte & DROPS != 0 { NCLASSES } else { 0 }
}

/// Record that the object at `addr` became permanent: its block is from now on
/// swept object by object, so that the object is kept even when a collection
/// does not reach it.
pub fn note_permanent(addr: usize) {
    let l = layout();
    debug_assert!(addr >= l.base);
    let block = (addr - l.base) >> BLOCK_SHIFT;
    // Other threads may be flagging objects in the same block.
    let byte = unsafe { &*(class_ptr(l, block) as *const std::sync::atomic::AtomicU8) };
    byte.fetch_or(HAS_PERMANENT, Ordering::Relaxed);
}

#[inline]
unsafe fn bitmap(l: &Layout, block: usize) -> *mut u64 {
    (l.bitmaps + block * WORDS * 8) as *mut u64
}

/// Mark the object at `addr`; true if it was not reached before. In a `minor`
/// collection an old object counts as reached already and is left alone. The
/// marks are kept apart from the objects: a collection that meets many references
/// to objects it has already marked, or to old ones, decides each from these dense
/// bitmaps without touching the object. Only the collector calls this, with the
/// world stopped.
#[inline]
pub fn mark(addr: usize, minor: bool) -> bool {
    let l = layout();
    let i = (addr - l.base) >> 4;
    let at = (i >> 6) * 8;
    let bit = 1u64 << (i & 63);
    unsafe {
        if minor && *((l.olds + at) as *const u64) & bit != 0 {
            return false;
        }
        let w = (l.marks + at) as *mut u64;
        if *w & bit != 0 {
            false
        } else {
            *w |= bit;
            true
        }
    }
}

/// Was the object at `addr` reached in the collection that is running? (In a
/// `minor` one, old objects count as reached.)
#[inline]
pub fn reached(addr: usize, minor: bool) -> bool {
    let l = layout();
    let i = (addr - l.base) >> 4;
    let at = (i >> 6) * 8;
    let bit = 1u64 << (i & 63);
    unsafe {
        (minor && *((l.olds + at) as *const u64) & bit != 0)
            || *((l.marks + at) as *const u64) & bit != 0
    }
}

/// Is the object at `addr` old (see `Layout`)?
#[inline]
pub fn is_old(addr: usize) -> bool {
    let l = layout();
    let i = (addr - l.base) >> 4;
    let w = (l.olds + (i >> 6) * 8) as *const u64;
    unsafe { std::ptr::read_volatile(w) & (1u64 << (i & 63)) != 0 }
}

/// Will the object at `addr` still be young after the collection that is running,
/// if it is reached? (It is neither old nor aged: this is its first survival.)
#[inline]
pub fn stays_young(addr: usize) -> bool {
    let l = layout();
    let i = (addr - l.base) >> 4;
    let bit = 1u64 << (i & 63);
    unsafe {
        let old = std::ptr::read_volatile((l.olds + (i >> 6) * 8) as *const u64);
        let aged = std::ptr::read_volatile((l.ageds + (i >> 6) * 8) as *const u64);
        (old | aged) & bit == 0
    }
}

/// Clear the marks a collection is going to use: all of them for a full one; for
/// a minor one only those of the young blocks, since only young objects get
/// marked (an old object counts as reached without a mark, see `mark`). So a
/// minor collection costs nothing in proportion to the heap.
pub fn begin_marks(full: bool) {
    let l = layout();
    let p = pool();
    unsafe {
        if full {
            std::ptr::write_bytes(l.marks as *mut u8, 0, p.next * WORDS * 8);
        } else {
            for &block in &p.young {
                let at = block as usize * WORDS * 8;
                std::ptr::write_bytes((l.marks + at) as *mut u8, 0, WORDS * 8);
            }
        }
    }
}

/// A thread's allocation state. Per pool it holds a block and, inside it, a run
/// of free slots `[cur, end)` that it hands out one after the other; when the
/// run is used up it looks for the next one in the block (from slot `cursor`),
/// then takes another block.
pub struct Cache {
    epoch: Cell<u64>,
    /// Copies of the layout's addresses, for the fast path.
    base: Cell<usize>,
    bitmaps: Cell<usize>,
    cur: [Cell<usize>; NPOOLS],
    end: [Cell<usize>; NPOOLS],
    /// The block the run is in (index + 1; 0: none).
    block: [Cell<u32>; NPOOLS],
    cursor: [Cell<u32>; NPOOLS],
}

impl Cache {
    pub const fn new() -> Cache {
        Cache {
            epoch: Cell::new(0),
            base: Cell::new(0),
            bitmaps: Cell::new(0),
            cur: [const { Cell::new(0) }; NPOOLS],
            end: [const { Cell::new(0) }; NPOOLS],
            block: [const { Cell::new(0) }; NPOOLS],
            cursor: [const { Cell::new(0) }; NPOOLS],
        }
    }

    /// Give up every run: what was claimed and not used is no longer allocated.
    fn release(&self) {
        let mut unused = 0usize;
        for pc in 0..NPOOLS {
            unused += self.end[pc].get() - self.cur[pc].get();
            self.cur[pc].set(0);
            self.end[pc].set(0);
            self.block[pc].set(0);
        }
        if unused != 0 {
            crate::heap::account_object(-(unused as isize));
        }
    }
}

impl Default for Cache {
    fn default() -> Self {
        Cache::new()
    }
}

thread_local! {
    static CACHE: Cache = const { Cache::new() };
}

/// A thread that allocated is ending: return what it claimed and did not use.
pub fn thread_exit() {
    let _ = CACHE.try_with(Cache::release);
}

#[inline]
fn slot_used(bits: *const u64, bit: usize) -> bool {
    unsafe { *bits.add(bit / 64) >> (bit % 64) & 1 == 1 }
}

/// The first run of free slots at or after slot `from` in a block of `slots`
/// slots of `step` grains: (first slot, length), at most `max` slots long.
fn find_run(
    bits: *const u64,
    from: usize,
    slots: usize,
    step: usize,
    max: usize,
) -> Option<(usize, usize)> {
    let mut i = from;
    while i < slots && slot_used(bits, i * step) {
        i += 1;
    }
    if i >= slots {
        return None;
    }
    let mut n = 1;
    while n < max && i + n < slots && !slot_used(bits, (i + n) * step) {
        n += 1;
    }
    Some((i, n))
}

/// Allocate `size` bytes for an object; `drops` says whether the object owns Rust
/// data that must be dropped when it dies (see `NPOOLS`). The memory is not
/// initialised; the caller writes the whole object before anything can observe it
/// (the slot's bit is already set, so the collector treats it as an object: a
/// collection cannot start between the allocation and the write, since allocation
/// is not a safepoint).
#[inline]
pub fn alloc(size: usize, drops: bool) -> *mut u8 {
    // With a constant size (nearly every caller) these fold to constants.
    let class = class_of(size);
    let csize = class_size(class);
    let pc = class + if drops { NCLASSES } else { 0 };
    let fast = CACHE.try_with(|c| {
        let cur = c.cur[pc].get();
        let addr = if cur < c.end[pc].get() && c.epoch.get() == EPOCH.load(Ordering::Relaxed) {
            c.cur[pc].set(cur + csize);
            cur
        } else {
            refill(c, pc, csize)
        };
        // The slot is in use from here on.
        let i = (addr - c.base.get()) >> 4;
        unsafe { *((c.bitmaps.get() + (i >> 6) * 8) as *mut u64) |= 1u64 << (i & 63) };
        addr
    });
    match fast {
        Ok(addr) => addr as *mut u8,
        Err(_) => alloc_uncached(pc, csize),
    }
}

/// The slow path of `alloc`: claim the next run of free slots for pool `pc` and
/// return its first slot.
#[cold]
#[inline(never)]
fn refill(c: &Cache, pc: usize, csize: usize) -> usize {
    let l = layout();
    let epoch = EPOCH.load(Ordering::Relaxed);
    if c.epoch.get() != epoch {
        // A collection ran: the blocks went back to the pool.
        c.release();
        c.epoch.set(epoch);
        c.base.set(l.base);
        c.bitmaps.set(l.bitmaps);
    }
    let step = csize / GRAIN;
    let slots = BLOCK / csize;
    let max = (MAX_RUN / csize).max(1);
    loop {
        let held = c.block[pc].get();
        if held != 0 {
            let block = held as usize - 1;
            let from = c.cursor[pc].get() as usize;
            if let Some((i, n)) = find_run(unsafe { bitmap(l, block) }, from, slots, step, max) {
                let start = block_addr(l, block) + i * csize;
                c.cursor[pc].set((i + n) as u32);
                c.cur[pc].set(start + csize);
                c.end[pc].set(start + n * csize);
                crate::heap::account_object((n * csize) as isize);
                return start;
            }
        }
        let block = acquire(l, pc);
        c.block[pc].set(block as u32 + 1);
        c.cursor[pc].set(0);
        c.cur[pc].set(0);
        c.end[pc].set(0);
    }
}

/// Allocation on a thread whose locals are gone (it is exiting): one slot from a
/// block taken for the purpose.
#[cold]
fn alloc_uncached(pc: usize, csize: usize) -> *mut u8 {
    let l = layout();
    let block = acquire(l, pc);
    let (i, _) = find_run(
        unsafe { bitmap(l, block) },
        0,
        BLOCK / csize,
        csize / GRAIN,
        1,
    )
    .expect("a block from the pool has a free slot");
    let bit = i * (csize / GRAIN);
    unsafe { *bitmap(l, block).add(bit / 64) |= 1u64 << (bit % 64) };
    crate::heap::account_object(csize as isize);
    (block_addr(l, block) + i * csize) as *mut u8
}

/// A block of pool `pc` with free slots, taken out of the shared pool.
#[cold]
fn acquire(l: &Layout, pc: usize) -> usize {
    let mut p = pool();
    if let Some(b) = p.partial[pc].pop() {
        p.young.push(b);
        return b as usize;
    }
    let block = match p.free.pop().or_else(|| p.released.pop()) {
        Some(b) => b as usize,
        None => {
            if p.next >= l.blocks {
                // The heap limit is capped well below the reservation (see
                // `heap::configure`), so this takes pathological fragmentation.
                // There is no way to report it to the program from here.
                eprintln!(
                    "rscl: the object heap's address space ({} GB) is exhausted",
                    reserved_bytes() >> 30
                );
                std::process::abort();
            }
            p.next += 1;
            USED_END.fetch_max(block_addr(l, p.next), Ordering::Relaxed);
            p.next - 1
        }
    };
    let byte = if pc >= NCLASSES {
        (pc - NCLASSES) as u8 | DROPS
    } else {
        pc as u8
    };
    unsafe { *class_ptr(l, block) = byte };
    p.young.push(block as u32);
    block
}

/// The object that `addr` is the address of, or points into; `None` if `addr` is
/// not inside a live object. Exact matches and interior pointers are told apart by
/// comparing the result with `addr`.
#[inline]
pub fn find(addr: usize) -> Option<usize> {
    let l = layout();
    if addr < l.base || addr >= USED_END.load(Ordering::Relaxed) {
        return None;
    }
    let block = (addr - l.base) >> BLOCK_SHIFT;
    let class = unsafe { class_at(l, block) };
    if class == 0 {
        return None;
    }
    let csize = class_size(class);
    let slot = ((addr - l.base) & (BLOCK - 1)) / csize;
    let bit = slot * (csize / GRAIN);
    let used = unsafe { *bitmap(l, block).add(bit / 64) } >> (bit % 64) & 1 == 1;
    used.then(|| block_addr(l, block) + slot * csize)
}

/// Bytes of address space reserved for objects.
pub fn reserved_bytes() -> usize {
    layout().blocks << BLOCK_SHIFT
}

/// Is `addr` inside the part of the object heap that has been handed out?
pub fn in_heap(addr: usize) -> bool {
    let l = layout();
    addr >= l.base && addr < USED_END.load(Ordering::Relaxed)
}

/// Number of objects in the heap (counted from the bitmaps: a diagnostic).
pub fn object_count() -> usize {
    let l = layout();
    let p = pool();
    let mut n = 0usize;
    for block in 0..p.next {
        if unsafe { class_at(l, block) } == 0 {
            continue;
        }
        let bits = unsafe { bitmap(l, block) };
        for w in 0..WORDS {
            n += unsafe { *bits.add(w) }.count_ones() as usize;
        }
    }
    n
}

/// Visit every object in the heap. The world must be stopped.
pub fn for_each_object(mut f: impl FnMut(*mut ObjHeader)) {
    let l = layout();
    let p = pool();
    for block in 0..p.next {
        let class = unsafe { class_at(l, block) };
        if class == 0 {
            continue;
        }
        let csize = class_size(class);
        let step = csize / GRAIN;
        let bits = unsafe { bitmap(l, block) };
        for w in 0..WORDS {
            let mut word = unsafe { *bits.add(w) };
            while word != 0 {
                let b = word.trailing_zeros() as usize;
                word &= word - 1;
                let bit = w * 64 + b;
                f((block_addr(l, block) + bit / step * csize) as *mut ObjHeader);
            }
        }
    }
}

/// Sweep after marking. The world must be stopped.
///
/// * An object in use that was not reached is offered to `finalize`, which either
///   keeps it (returns true: a permanent object) or drops its fields and returns
///   false, whereupon its slot is released.
/// * A young object that was reached for the first time is *aged*: it stays young.
/// * A young object that was reached again is *promoted*: it is old from now on,
///   and is reported to `promoted` (it may refer to young objects, which the
///   collector must know about, since minor collections will not look inside it).
///
/// The dead objects of a plain block without permanent objects are released
/// without calling `finalize` (with `check_plain`, a verification mode, they are
/// offered to it all the same, and it must not keep any).
///
/// A `full` sweep visits every block; otherwise only the young blocks: those
/// allocated into since the last collection and those that still hold aged
/// objects. Blocks left empty are released, and every block visited goes back to
/// the pool, so threads start the next cycle with blocks of their own.
pub fn sweep(
    full: bool,
    check_plain: bool,
    mut finalize: impl FnMut(*mut ObjHeader) -> bool,
    mut promoted: impl FnMut(*mut ObjHeader),
) -> usize {
    let l = layout();
    let mut p = pool();
    EPOCH.fetch_add(1, Ordering::Relaxed);
    let mut young = std::mem::take(&mut p.young);
    let blocks: Vec<u32> = if full {
        // Every list is rebuilt from scratch.
        for list in p.partial.iter_mut() {
            list.clear();
        }
        (0..p.next as u32).collect()
    } else {
        // A block is listed once for each time a thread took it, and once more if
        // it holds aged objects: visit it once. (A young block left the partial
        // lists when a thread took it; one that only holds aged objects may still
        // be in them.)
        young.sort_unstable();
        young.dedup();
        for list in p.partial.iter_mut() {
            list.retain(|b| young.binary_search(b).is_err());
        }
        young
    };
    let mut freed = 0usize;
    let mut freed_bytes = 0usize;
    for block in blocks {
        let block = block as usize;
        let byte = unsafe { *class_ptr(l, block) };
        let class = (byte & CLASS_MASK) as usize;
        if class == 0 {
            continue;
        }
        // Nothing in a plain block needs finalising, and without a permanent
        // object in it nothing unreached is kept: its dead objects are released
        // without being looked at.
        let wholesale = byte & (DROPS | HAS_PERMANENT) == 0;
        let csize = class_size(class);
        let slots = BLOCK / csize;
        let at = block * WORDS * 8;
        let bits = (l.bitmaps + at) as *mut u64;
        let marks = (l.marks + at) as *const u64;
        let olds = (l.olds + at) as *mut u64;
        let ageds = (l.ageds + at) as *mut u64;
        let mut live = 0usize;
        let mut has_young = false;
        for w in 0..WORDS {
            let used = unsafe { *bits.add(w) };
            if used == 0 {
                continue;
            }
            let (mut mark, mut old, aged) = unsafe { (*marks.add(w), *olds.add(w), *ageds.add(w)) };
            if !full {
                // Old objects were not marked: they count as reached.
                mark |= old;
            }
            let young_live = used & mark & !old;
            let first = young_live & !aged;
            let mut second = young_live & aged;
            old |= second;
            let mut kept = used;
            let mut dead = used & !mark;
            if wholesale {
                if check_plain {
                    let mut d = dead;
                    while d != 0 {
                        let b = d.trailing_zeros() as usize;
                        d &= d - 1;
                        let addr = block_addr(l, block) + (w * 64 + b) * GRAIN;
                        assert!(
                            !finalize(addr as *mut ObjHeader),
                            "arena: an unreached object at {addr:#x} in a plain block must be kept"
                        );
                    }
                }
                kept &= !dead;
                // Counted bit by bit, not with `count_ones`: the marks derive from
                // words the stack scan read out of dead stack slots, which memory
                // checkers call uninitialised, and a count computed from them
                // would carry that into the heap accounting and into `(gc)`'s
                // result.
                while dead != 0 {
                    dead &= dead - 1;
                    freed += 1;
                    freed_bytes += csize;
                }
            }
            while dead != 0 {
                let b = dead.trailing_zeros() as usize;
                dead &= dead - 1;
                let addr = block_addr(l, block) + (w * 64 + b) * GRAIN;
                if finalize(addr as *mut ObjHeader) {
                    // Kept though unreached: it never needs looking at again.
                    old |= 1 << b;
                } else {
                    kept &= !(1 << b);
                    freed += 1;
                    freed_bytes += csize;
                }
            }
            while second != 0 {
                let b = second.trailing_zeros() as usize;
                second &= second - 1;
                promoted((block_addr(l, block) + (w * 64 + b) * GRAIN) as *mut ObjHeader);
            }
            let aged = (aged | first) & kept;
            let old = old & kept;
            unsafe {
                *bits.add(w) = kept;
                *olds.add(w) = old;
                *ageds.add(w) = aged;
            }
            live += kept.count_ones() as usize;
            has_young |= aged & !old != 0;
        }
        if live == 0 {
            unsafe { *class_ptr(l, block) = 0 };
            p.free.push(block as u32);
        } else {
            if live < slots {
                p.partial[pool_of(byte)].push(block as u32);
            }
            if has_young {
                // Its aged objects are examined again by the next collection.
                p.young.push(block as u32);
            }
        }
    }
    // A reserve of empty blocks stays mapped (the next cycle will want them); the
    // rest, the ones emptied longest ago, goes back to the operating system, once.
    if p.free.len() > KEEP_FREE {
        let excess = p.free.len() - KEEP_FREE;
        let Pool { free, released, .. } = &mut *p;
        for block in free.drain(..excess) {
            unsafe {
                madvise(
                    block_addr(l, block as usize) as *mut u8,
                    BLOCK,
                    MADV_DONTNEED,
                )
            };
            released.push(block);
        }
    }
    crate::heap::account_object(-(freed_bytes as isize));
    freed
}
