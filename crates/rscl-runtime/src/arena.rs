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
//! A thread allocates from blocks it has taken for itself (no locking on the fast
//! path); a collection hands every block back, frees the unmarked slots and
//! releases blocks that became empty to the operating system.
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::value::ObjHeader;

/// Empty blocks kept mapped after a collection (64 MB).
const KEEP_FREE: usize = 1024;
const BLOCK_SHIFT: usize = 16;
pub const BLOCK: usize = 1 << BLOCK_SHIFT;
/// Smallest slot; every size class is a multiple of it.
const GRAIN: usize = 16;
/// Bitmap words per block (one bit per `GRAIN` bytes).
const WORDS: usize = BLOCK / GRAIN / 64;
/// Classes 1..=16 are multiples of 16 bytes up to 256; then powers of two to a block.
const SMALL_CLASSES: usize = 16;
pub const NCLASSES: usize = SMALL_CLASSES + 8 + 1;

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
        SMALL_CLASSES + (p.trailing_zeros() as usize - 8)
    }
}

#[inline]
fn class_size(class: usize) -> usize {
    if class <= SMALL_CLASSES {
        class * GRAIN
    } else {
        1 << (class - SMALL_CLASSES + 8)
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
    /// Blocks that were emptied and released.
    free: Vec<u32>,
    /// Per class: blocks with free slots that no thread holds.
    partial: [Vec<u32>; NCLASSES],
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
            partial: [const { Vec::new() }; NCLASSES],
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

/// A thread's allocation state: per class, the block it fills (index + 1; 0: none)
/// and the slot to try next.
pub struct Cache {
    epoch: Cell<u64>,
    block: [Cell<u32>; NCLASSES],
    cursor: [Cell<u32>; NCLASSES],
}

impl Cache {
    pub const fn new() -> Cache {
        Cache {
            epoch: Cell::new(0),
            block: [const { Cell::new(0) }; NCLASSES],
            cursor: [const { Cell::new(0) }; NCLASSES],
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

/// Find a free slot at or after `from` in a block of `slots` slots of `step` grains;
/// mark it used.
#[inline]
unsafe fn take_slot(bits: *mut u64, from: usize, slots: usize, step: usize) -> Option<usize> {
    let mut i = from;
    while i < slots {
        let bit = i * step;
        let w = unsafe { *bits.add(bit / 64) };
        if step == 1 {
            // Skip a run of used slots a word at a time.
            let rest = !w >> (bit % 64);
            if rest == 0 {
                i = (bit / 64 + 1) * 64;
                continue;
            }
            let i2 = i + rest.trailing_zeros() as usize;
            if i2 >= slots {
                return None;
            }
            let b2 = i2;
            unsafe { *bits.add(b2 / 64) |= 1 << (b2 % 64) };
            return Some(i2);
        }
        if w >> (bit % 64) & 1 == 0 {
            unsafe { *bits.add(bit / 64) = w | 1 << (bit % 64) };
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Allocate `size` bytes for an object. The memory is not initialised; the caller
/// writes the whole object before anything can observe it (the slot's bit is
/// already set, so the collector treats it as an object: a collection cannot start
/// between the allocation and the write, since allocation is not a safepoint).
pub fn alloc(size: usize) -> *mut u8 {
    let l = layout();
    let class = class_of(size);
    let csize = class_size(class);
    let step = csize / GRAIN;
    let slots = BLOCK / csize;
    let fast = CACHE.try_with(|c| {
        let epoch = EPOCH.load(Ordering::Relaxed);
        if c.epoch.get() != epoch {
            c.epoch.set(epoch);
            for b in &c.block {
                b.set(0);
            }
        }
        loop {
            let held = c.block[class].get();
            if held != 0 {
                let block = held as usize - 1;
                let from = c.cursor[class].get() as usize;
                if let Some(i) = unsafe { take_slot(bitmap(l, block), from, slots, step) } {
                    c.cursor[class].set(i as u32 + 1);
                    return block_addr(l, block) + i * csize;
                }
            }
            let block = acquire(l, class);
            c.block[class].set(block as u32 + 1);
            c.cursor[class].set(0);
        }
    });
    let addr = match fast {
        Ok(a) => a,
        // The thread's locals are gone (it is exiting): take a slot from a block
        // of its own.
        Err(_) => {
            let block = acquire(l, class);
            let i = unsafe { take_slot(bitmap(l, block), 0, slots, step) }.expect("empty block");
            block_addr(l, block) + i * csize
        }
    };
    crate::heap::account_object(csize as isize);
    addr as *mut u8
}

/// A block of `class` with free slots, taken out of the shared pool.
#[cold]
fn acquire(l: &Layout, class: usize) -> usize {
    let mut p = pool();
    if let Some(b) = p.partial[class].pop() {
        p.young.push(b);
        return b as usize;
    }
    let block = match p.free.pop() {
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
    unsafe { *class_ptr(l, block) = class as u8 };
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
    let class = unsafe { *class_ptr(l, block) } as usize;
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
        if unsafe { *class_ptr(l, block) } == 0 {
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
        let class = unsafe { *class_ptr(l, block) } as usize;
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
/// A `full` sweep visits every block; otherwise only the young blocks: those
/// allocated into since the last collection and those that still hold aged
/// objects. Blocks left empty are released, and every block visited goes back to
/// the pool, so threads start the next cycle with blocks of their own.
pub fn sweep(
    full: bool,
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
        let class = unsafe { *class_ptr(l, block) } as usize;
        if class == 0 {
            continue;
        }
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
            // A reserve of empty blocks stays mapped (the next cycle will want
            // them); the rest goes back to the operating system.
            if p.free.len() >= KEEP_FREE {
                unsafe { madvise(block_addr(l, block) as *mut u8, BLOCK, MADV_DONTNEED) };
            }
            p.free.push(block as u32);
        } else {
            if live < slots {
                p.partial[class].push(block as u32);
            }
            if has_young {
                // Its aged objects are examined again by the next collection.
                p.young.push(block as u32);
            }
        }
    }
    crate::heap::account_object(-(freed_bytes as isize));
    freed
}
