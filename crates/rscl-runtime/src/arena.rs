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

/// The reserved ranges: object blocks, one class byte per block, and the bitmaps.
struct Layout {
    base: usize,
    blocks: usize,
    classes: usize,
    bitmaps: usize,
}

fn layout() -> &'static Layout {
    static L: OnceLock<Layout> = OnceLock::new();
    L.get_or_init(|| {
        // Address space only; pages are committed when touched. Smaller
        // reservations are tried when the system refuses a large one.
        for gb in [256usize, 64, 16, 4, 1] {
            let blocks = (gb << 30) >> BLOCK_SHIFT;
            let Some(base) = reserve(blocks << BLOCK_SHIFT) else {
                continue;
            };
            let (Some(classes), Some(bitmaps)) = (reserve(blocks), reserve(blocks * WORDS * 8))
            else {
                continue;
            };
            return Layout {
                base,
                blocks,
                classes,
                bitmaps,
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
static OBJECTS: AtomicUsize = AtomicUsize::new(0);

struct Pool {
    /// Blocks handed out so far (indices below this may be in use).
    next: usize,
    /// Blocks that were emptied and released.
    free: Vec<u32>,
    /// Per class: blocks with free slots that no thread holds.
    partial: [Vec<u32>; NCLASSES],
}

fn pool() -> MutexGuard<'static, Pool> {
    static P: OnceLock<Mutex<Pool>> = OnceLock::new();
    P.get_or_init(|| {
        Mutex::new(Pool {
            next: 0,
            free: Vec::new(),
            partial: [const { Vec::new() }; NCLASSES],
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
    OBJECTS.fetch_add(1, Ordering::Relaxed);
    crate::heap::account_object(csize as isize);
    addr as *mut u8
}

/// A block of `class` with free slots, taken out of the shared pool.
#[cold]
fn acquire(l: &Layout, class: usize) -> usize {
    let mut p = pool();
    if let Some(b) = p.partial[class].pop() {
        return b as usize;
    }
    let block = match p.free.pop() {
        Some(b) => b as usize,
        None => {
            assert!(
                p.next < l.blocks,
                "the object heap's address space is exhausted"
            );
            p.next += 1;
            USED_END.fetch_max(block_addr(l, p.next), Ordering::Relaxed);
            p.next - 1
        }
    };
    unsafe { *class_ptr(l, block) = class as u8 };
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

/// Bytes of the slot that holds the object at `addr`.
pub fn slot_size(addr: usize) -> usize {
    let l = layout();
    let block = (addr - l.base) >> BLOCK_SHIFT;
    class_size(unsafe { *class_ptr(l, block) } as usize)
}

pub fn object_count() -> usize {
    OBJECTS.load(Ordering::Relaxed)
}

/// Visit every object; `keep` decides whether it stays (and may clear its mark).
/// Objects that do not stay are dropped by `keep` itself before it returns false.
/// The world must be stopped. Blocks left empty are released; every other block
/// goes back to the pool, so threads start the next cycle with fresh blocks.
pub fn sweep(mut keep: impl FnMut(*mut ObjHeader) -> bool) -> usize {
    let l = layout();
    let mut p = pool();
    EPOCH.fetch_add(1, Ordering::Relaxed);
    for list in p.partial.iter_mut() {
        list.clear();
    }
    let mut freed = 0usize;
    let mut freed_bytes = 0usize;
    for block in 0..p.next {
        let class = unsafe { *class_ptr(l, block) } as usize;
        if class == 0 {
            continue;
        }
        let csize = class_size(class);
        let step = csize / GRAIN;
        let slots = BLOCK / csize;
        let bits = unsafe { bitmap(l, block) };
        let mut live = 0usize;
        for w in 0..WORDS {
            let mut word = unsafe { *bits.add(w) };
            while word != 0 {
                let b = word.trailing_zeros() as usize;
                word &= word - 1;
                let bit = w * 64 + b;
                let addr = block_addr(l, block) + bit / step * csize;
                if keep(addr as *mut ObjHeader) {
                    live += 1;
                } else {
                    unsafe { *bits.add(w) &= !(1 << b) };
                    freed += 1;
                    freed_bytes += csize;
                }
            }
        }
        if live == 0 {
            unsafe { *class_ptr(l, block) = 0 };
            // A reserve of empty blocks stays mapped (the next cycle will want
            // them); the rest goes back to the operating system.
            if p.free.len() >= KEEP_FREE {
                unsafe { madvise(block_addr(l, block) as *mut u8, BLOCK, MADV_DONTNEED) };
            }
            p.free.push(block as u32);
        } else if live < slots {
            p.partial[class].push(block as u32);
        }
    }
    OBJECTS.fetch_sub(freed, Ordering::Relaxed);
    crate::heap::account_object(-(freed_bytes as isize));
    freed
}
