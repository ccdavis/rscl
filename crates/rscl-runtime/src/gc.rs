//! The garbage collector: stop-the-world mark and sweep over a non-moving heap.
//!
//! Every heap object lives in the object heap (`arena.rs`) and stays at its
//! address until a collection finds it unreachable. A `Value` is a plain word that
//! is copied freely; nothing counts references.
//!
//! # Roots
//!
//! * **Machine stacks and registers**, scanned *conservatively*: any word that is
//!   the address of a registered object (or points inside one) keeps that object
//!   alive. This covers the frames of compiled code and of the Rust runtime alike,
//!   so neither needs stack maps or handles. Objects never move, so a word that
//!   merely looks like a pointer costs nothing but some retention.
//! * **Constant tables** of compiled units (registered ranges, scanned the same way).
//! * **Permanent objects**: vars (and through them every root value), namespaces,
//!   types, protocols, fn prototypes' metadata.
//! * **Thread-local runtime state**: the pending exception, `binding` frames, the
//!   future a worker computes, and argument slices handed to rscl functions by
//!   natives ([`root_slice`]).
//! * **Pins**: values that live only in Rust heap data for a while (the closure of a
//!   thread being started, values held by a native library): [`Pinned`].
//!
//! From the roots, objects are traced precisely: each kind knows the values it
//! holds (`trace`).
//!
//! # When a collection may run
//!
//! Only at a *safepoint*: fn entry and loop headers of compiled code, and explicit
//! blocking regions of the runtime ([`blocking`]). A native function is never
//! interrupted by a collection unless it calls back into rscl code, so values it
//! holds in Rust heap containers (a `Vec<Value>` being built, say) are safe as long
//! as it does not call rscl functions meanwhile; one that does keeps such values
//! reachable through a root ([`root_slice`], [`Pinned`]). The compiler holds forms
//! in its own data structures while it runs macros, so it defers collections for
//! the duration with [`inhibit`].
//!
//! All other rscl threads must be stopped (parked at a safepoint or inside a
//! blocking region) while a collection runs; their stacks are scanned from the
//! stack pointer they published when they stopped.
//!
//! # Design notes
//!
//! The approach borrows from `gc-arena` (collections happen only at points where
//! the mutator's state is known, and each object type has a trace method) and from
//! `shredder` (a non-moving collector that traces through ordinary Rust data), but
//! neither crate can see references held by JIT-compiled machine code, which is why
//! the stack scan is conservative.
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use crate::obj::{
    self, Kind, atom, cons, exception, func, lazyseq, list, map, pending, set, sorted, symbol,
    types, var, vector,
};
use crate::value::{MARK, ObjHeader, PERMANENT, Value};

// ---- the object registry -------------------------------------------------------------

/// The collector's root tables (the objects themselves live in `arena.rs`).
struct Heap {
    /// Objects that are roots and never freed.
    permanent: Vec<usize>,
    /// Fn prototypes (not objects; their metadata is a root).
    protos: Vec<usize>,
    /// Constant tables of compiled units: (address, words).
    ranges: Vec<(usize, usize)>,
    /// Pinned values and how many pins each has.
    pins: HashMap<u64, usize>,
}

fn heap() -> MutexGuard<'static, Heap> {
    static H: OnceLock<Mutex<Heap>> = OnceLock::new();
    H.get_or_init(|| {
        Mutex::new(Heap {
            permanent: Vec::new(),
            protos: Vec::new(),
            ranges: Vec::new(),
            pins: HashMap::new(),
        })
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

/// Per-thread collector state.
struct Local {
    registered: Cell<bool>,
    /// Highest address of this thread's stack.
    stack_base: Cell<usize>,
    /// Lowest usable address of this thread's stack (0: unknown).
    stack_floor: Cell<usize>,
    /// Inside a blocking region (already counted as stopped).
    stopped: Cell<bool>,
    /// This thread runs the collection.
    collecting: Cell<bool>,
    /// Depth of `inhibit` guards.
    inhibit: Cell<u32>,
    /// A collection was wanted while inhibited.
    deferred: Cell<bool>,
    /// Argument slices of calls into rscl code in progress: (address, values).
    slices: RefCell<Vec<(usize, usize)>>,
}

impl Drop for Local {
    fn drop(&mut self) {
        leave_world(self);
    }
}

thread_local! {
    static LOCAL: Local = const {
        Local {
            registered: Cell::new(false),
            stack_base: Cell::new(0),
            stack_floor: Cell::new(0),
            stopped: Cell::new(false),
            collecting: Cell::new(false),
            inhibit: Cell::new(0),
            deferred: Cell::new(false),
            slices: RefCell::new(Vec::new()),
        }
    };
}

/// Register an object that is never freed and is a root (its header is permanent).
pub(crate) fn register_permanent(p: *mut ObjHeader) {
    heap().permanent.push(p as usize);
}

/// Register a fn prototype: its metadata is a root.
pub(crate) fn register_proto(p: *const func::FnProto) {
    heap().protos.push(p as usize);
}

/// Make an existing object permanent: it becomes a root and is never freed.
pub fn make_permanent(v: &Value) {
    let Some(h) = v.header() else { return };
    if h.flags.fetch_or(PERMANENT, Ordering::Relaxed) & PERMANENT == 0 {
        heap().permanent.push(v.bits() as usize);
    }
}

/// Register a table of values as roots (a compiled unit's constant table). The
/// memory must stay valid for the rest of the process.
pub fn add_root_range(addr: *const u64, words: usize) {
    if words > 0 {
        heap().ranges.push((addr as usize, words));
    }
}

/// Keep `v` alive until a matching `unpin`.
pub fn pin(v: &Value) {
    if v.is_ptr() {
        *heap().pins.entry(v.bits()).or_insert(0) += 1;
    }
}

pub fn unpin(v: &Value) {
    if !v.is_ptr() {
        return;
    }
    let mut h = heap();
    if let Some(n) = h.pins.get_mut(&v.bits()) {
        *n -= 1;
        if *n == 0 {
            h.pins.remove(&v.bits());
        }
    }
}

/// A value kept alive while this handle exists: for values that live in Rust heap
/// data the collector cannot see (a closure handed to a new thread, a native
/// library's own storage).
pub struct Pinned(Value);

impl Pinned {
    pub fn new(v: Value) -> Pinned {
        pin(&v);
        Pinned(v)
    }
    pub fn get(&self) -> Value {
        self.0.clone()
    }
}

impl Drop for Pinned {
    fn drop(&mut self) {
        unpin(&self.0);
    }
}

/// Keeps a slice of values reachable while it exists (see [`root_slice`]).
pub struct SliceRoot<'a> {
    ptr: usize,
    len: usize,
    life: std::marker::PhantomData<&'a [Value]>,
}

/// Root the values of `s` for the lifetime of the guard: for natives that call rscl
/// code while they hold values in a `Vec` or another heap container.
pub fn root_slice(s: &[Value]) -> SliceRoot<'_> {
    unsafe { root_words(s.as_ptr() as *const u64, s.len()) }
}

/// # Safety
/// `ptr` must point at `len` values that stay valid while the guard exists.
pub(crate) unsafe fn root_words<'a>(ptr: *const u64, len: usize) -> SliceRoot<'a> {
    let _ = LOCAL.try_with(|l| l.slices.borrow_mut().push((ptr as usize, len)));
    SliceRoot {
        ptr: ptr as usize,
        len,
        life: std::marker::PhantomData,
    }
}

impl Drop for SliceRoot<'_> {
    fn drop(&mut self) {
        let _ = LOCAL.try_with(|l| {
            // Nearly always the newest entry; guards held in struct fields may go
            // in another order.
            let mut slices = l.slices.borrow_mut();
            if let Some(i) = slices.iter().rposition(|&e| e == (self.ptr, self.len)) {
                slices.remove(i);
            }
        });
    }
}

/// Number of objects the collector knows about.
pub fn object_count() -> usize {
    crate::arena::object_count()
}

// ---- deferring collections -------------------------------------------------------------

/// While this guard exists, the calling thread neither collects nor stops for a
/// collection. For code that holds values where the collector cannot see them
/// across calls into rscl code (the compiler, while it expands macros).
pub struct Inhibit(());

pub fn inhibit() -> Inhibit {
    let _ = LOCAL.try_with(|l| l.inhibit.set(l.inhibit.get() + 1));
    Inhibit(())
}

impl Drop for Inhibit {
    fn drop(&mut self) {
        let _ = LOCAL.try_with(|l| {
            l.inhibit.set(l.inhibit.get() - 1);
            if l.inhibit.get() == 0 && l.deferred.replace(false) {
                request_gc();
            }
        });
    }
}

thread_local! {
    /// Number of active native frames that have not declared themselves safe to
    /// collect under (see `hold`).
    static HOLD: Cell<u32> = const { Cell::new(0) };
}

/// Marks a native frame that may hold values where the collector cannot see them
/// (in a `Vec`, a map under construction, an iterator): while one exists, the
/// thread does not collect, and a wanted collection waits for the next safepoint
/// after it is gone. Every builtin takes one on entry unless it is declared `gc`
/// (see `native_fns!`), so a native that calls back into rscl code is safe by
/// default and only the audited ones let a collection run under them.
pub struct Hold(());

#[inline]
pub fn hold() -> Hold {
    HOLD.with(|h| h.set(h.get() + 1));
    Hold(())
}

impl Drop for Hold {
    #[inline]
    fn drop(&mut self) {
        HOLD.with(|h| h.set(h.get() - 1));
    }
}

/// Collections are deferred on this thread right now (a `Hold` or an `Inhibit`).
pub fn deferred_here() -> bool {
    HOLD.with(|h| h.get()) > 0 || LOCAL.try_with(|l| l.inhibit.get() > 0).unwrap_or(true)
}

// ---- requests ----------------------------------------------------------------------------

static GC_WANTED: AtomicBool = AtomicBool::new(false);
static COLLECTIONS: AtomicU64 = AtomicU64::new(0);

/// Mirror of `World::phase != Idle` for the slow path, so threads that hit the
/// interrupt flag for another reason do not serialise on the world lock.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// A collector is stopping or has stopped the world.
pub fn stopping() -> bool {
    STOPPING.load(Ordering::Acquire)
}

/// This thread is running a collection (its allocations are the collector's own).
pub(crate) fn collecting() -> bool {
    LOCAL.try_with(|l| l.collecting.get()).unwrap_or(false)
}

/// Ask for a collection at the next safepoint. Only touches atomics: the allocator
/// calls this.
pub fn request_gc() {
    GC_WANTED.store(true, Ordering::Release);
    crate::native::request_safepoint();
}

pub fn gc_wanted() -> bool {
    GC_WANTED.load(Ordering::Acquire)
}

pub fn collections() -> u64 {
    COLLECTIONS.load(Ordering::Relaxed)
}

struct Config {
    /// Collect at every n-th safepoint (0: off). For testing.
    stress: u64,
    debug: bool,
    /// Check every traced reference against the registry.
    verify: bool,
    /// Report the stack slots that refer to sequence cells.
    explain: bool,
}

fn config() -> &'static Config {
    static C: OnceLock<Config> = OnceLock::new();
    C.get_or_init(|| {
        let num = |k: &str| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        Config {
            stress: num("RSCL_GC_STRESS").unwrap_or(0),
            debug: std::env::var_os("RSCL_GC_DEBUG").is_some(),
            verify: std::env::var_os("RSCL_GC_VERIFY").is_some(),
            explain: std::env::var_os("RSCL_GC_EXPLAIN").is_some(),
        }
    })
}

/// Read the collector's environment settings (call once at startup).
pub fn configure() {
    if config().stress > 0 {
        crate::native::request_safepoint();
    }
}

/// Stress mode: every n-th safepoint poll asks for a collection. True if stress
/// mode is on (the interrupt flag then stays raised).
pub(crate) fn stress_tick() -> bool {
    let n = config().stress;
    if n == 0 {
        return false;
    }
    static TICKS: AtomicU64 = AtomicU64::new(0);
    if TICKS.fetch_add(1, Ordering::Relaxed).is_multiple_of(n) {
        GC_WANTED.store(true, Ordering::Release);
    }
    true
}

// ---- stopping the world ------------------------------------------------------------------

/// Callee-saved registers captured when a thread stops.
#[cfg(target_arch = "x86_64")]
const NREGS: usize = 6;
#[cfg(target_arch = "aarch64")]
const NREGS: usize = 12;

/// Where a stopped thread's live state is: its stack from `sp` up, and the
/// callee-saved registers it had when it stopped.
#[derive(Clone, Copy)]
struct Frame {
    sp: usize,
    regs: [usize; NREGS],
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn capture() -> Frame {
    let mut regs = [0usize; NREGS];
    let sp: usize;
    unsafe {
        std::arch::asm!(
            "mov [{b}], rbx",
            "mov [{b} + 8], rbp",
            "mov [{b} + 16], r12",
            "mov [{b} + 24], r13",
            "mov [{b} + 32], r14",
            "mov [{b} + 40], r15",
            "mov {sp}, rsp",
            b = in(reg) regs.as_mut_ptr(),
            sp = out(reg) sp,
            options(nostack, preserves_flags),
        );
    }
    Frame { sp, regs }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn capture() -> Frame {
    let mut regs = [0usize; NREGS];
    let sp: usize;
    unsafe {
        std::arch::asm!(
            "stp x19, x20, [{b}]",
            "stp x21, x22, [{b}, #16]",
            "stp x23, x24, [{b}, #32]",
            "stp x25, x26, [{b}, #48]",
            "stp x27, x28, [{b}, #64]",
            "str x29, [{b}, #80]",
            "mov {sp}, sp",
            b = in(reg) regs.as_mut_ptr(),
            sp = out(reg) sp,
            options(nostack, preserves_flags),
        );
    }
    Frame { sp, regs }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("the collector's stack scan supports x86-64 and aarch64 only");

/// Run `f` with the calling thread's stack pointer and callee-saved registers
/// recorded. The frame of this function stays on the stack while `f` runs, so every
/// value its callers hold is either in a frame above `sp` or in `regs`.
#[inline(never)]
fn with_frame<R>(f: impl FnOnce(&Frame) -> R) -> R {
    let frame = capture();
    let r = f(&frame);
    std::hint::black_box(&frame);
    r
}

/// Zero the dead part of the calling thread's stack (below the current frame).
///
/// Frames do not initialise all of their memory, so a new frame can expose words
/// left behind by calls that returned long ago; to the conservative scan those are
/// references, and one of them pointing at (an address later reused for) the head
/// of a lazy sequence keeps the whole sequence alive. Clearing the dead area before
/// long-lived frames are pushed, and after every collection, keeps such leftovers
/// rare. The sweep goes down page by page and stops at the first run of pages that
/// are already zero (stack never used, or cleared before).
#[inline(never)]
pub fn clear_dead_stack() {
    const PAGE: usize = 4096;
    /// Stop after this many pages in a row that needed no clearing.
    const CLEAN_RUN: usize = 4;
    let floor = LOCAL.try_with(|l| l.stack_floor.get()).unwrap_or(0);
    if floor == 0 {
        return;
    }
    // Leave room for the guard pages and for whatever is below the limit anyway.
    let floor = floor + 16 * PAGE;
    // Calling something makes this a function with a real frame: its own locals are
    // above the stack pointer (never in the red zone below it), so everything below
    // the stack pointer is dead and may be zeroed, the frame of that call included.
    not_a_leaf();
    let sp = capture().sp;
    let mut hi = sp.saturating_sub(16) & !7;
    let mut clean = 0;
    while hi > floor && clean < CLEAN_RUN {
        let lo = ((hi - 1) & !(PAGE - 1)).max(floor);
        let words = (hi - lo) / 8;
        let mut dirty = false;
        for i in 0..words {
            let p = (lo + 8 * i) as *mut usize;
            unsafe {
                if std::ptr::read_volatile(p) != 0 {
                    std::ptr::write_volatile(p, 0);
                    dirty = true;
                }
            }
        }
        clean = if dirty { 0 } else { clean + 1 };
        hi = lo;
    }
}

#[inline(never)]
fn not_a_leaf() {
    std::hint::black_box(());
}

/// What the collector needs from a stopped thread.
#[derive(Clone)]
struct Stopped {
    frame: Frame,
    /// Thread-local roots (see `thread_roots`).
    roots: Vec<u64>,
}

struct ThreadRec {
    id: u64,
    stack_base: usize,
    stopped: Option<Stopped>,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Phase {
    Idle,
    /// A collector waits for the other threads to stop.
    Requested,
    Running,
}

struct World {
    phase: Phase,
    /// Registered threads that are not stopped.
    running: usize,
    threads: Vec<ThreadRec>,
}

struct Sync {
    world: Mutex<World>,
    cv: Condvar,
}

fn world() -> &'static Sync {
    static W: OnceLock<Sync> = OnceLock::new();
    W.get_or_init(|| Sync {
        world: Mutex::new(World {
            phase: Phase::Idle,
            running: 0,
            threads: Vec::new(),
        }),
        cv: Condvar::new(),
    })
}

fn lock_world() -> MutexGuard<'static, World> {
    world().world.lock().unwrap_or_else(|e| e.into_inner())
}

fn wait_idle(mut g: MutexGuard<'static, World>) -> MutexGuard<'static, World> {
    while g.phase != Phase::Idle {
        g = world().cv.wait(g).unwrap_or_else(|e| e.into_inner());
    }
    g
}

/// Add the calling thread to the set the collector stops and scans.
fn join_world(l: &Local) {
    let (floor, top) = crate::native::thread_stack_bounds();
    l.stack_floor.set(floor);
    // Unknown bounds (not a pthread stack): scan from here up to the page end at
    // least; better than nothing, and unknown on no supported platform.
    let marker = 0usize;
    let base = if top != 0 {
        // Thread-local storage lives at the top of the stack mapping. It is not
        // frames, and the allocator's own thread-local state there points at heap
        // blocks, which the scan would take for references.
        tls_start(floor, top).unwrap_or(top)
    } else {
        (&marker as *const usize as usize | 0xfff) + 1
    };
    l.stack_base.set(base);
    let mut g = wait_idle(lock_world());
    g.running += 1;
    g.threads.push(ThreadRec {
        id: crate::native::thread_id(),
        stack_base: base,
        stopped: None,
    });
    drop(g);
    l.registered.set(true);
    crate::heap::arm_exit_flush();
}

#[repr(C)]
struct DlPhdrInfo {
    addr: usize,
    name: *const std::ffi::c_char,
    phdr: *const std::ffi::c_void,
    phnum: u16,
    adds: u64,
    subs: u64,
    tls_modid: usize,
    tls_data: *mut std::ffi::c_void,
}

unsafe extern "C" {
    fn dl_iterate_phdr(
        cb: unsafe extern "C" fn(*mut DlPhdrInfo, usize, *mut std::ffi::c_void) -> i32,
        data: *mut std::ffi::c_void,
    ) -> i32;
}

/// The lowest address of the calling thread's thread-local blocks that lies inside
/// `[floor, top)`, its stack mapping: frames end below it.
fn tls_start(floor: usize, top: usize) -> Option<usize> {
    struct Find {
        floor: usize,
        top: usize,
        lowest: usize,
    }
    unsafe extern "C" fn each(
        info: *mut DlPhdrInfo,
        size: usize,
        data: *mut std::ffi::c_void,
    ) -> i32 {
        // Older loaders pass a shorter struct without the TLS fields.
        if size < std::mem::size_of::<DlPhdrInfo>() {
            return 0;
        }
        let f = unsafe { &mut *(data as *mut Find) };
        let p = unsafe { (*info).tls_data } as usize;
        if p > f.floor && p < f.top && p < f.lowest {
            f.lowest = p;
        }
        0
    }
    let mut f = Find {
        floor,
        top,
        lowest: usize::MAX,
    };
    unsafe { dl_iterate_phdr(each, &mut f as *mut Find as *mut std::ffi::c_void) };
    // A thread-local of this crate, in case the loader reported nothing useful.
    let own = HOLD.with(|h| h as *const Cell<u32> as usize);
    if own > floor && own < top && own < f.lowest {
        f.lowest = own;
    }
    (f.lowest != usize::MAX).then_some(f.lowest & !7)
}

/// Remove the calling thread.
fn leave_world(l: &Local) {
    if !l.registered.get() {
        return;
    }
    let id = crate::native::thread_id();
    let mut g = lock_world();
    if let Some(i) = g.threads.iter().position(|t| t.id == id) {
        if g.threads[i].stopped.is_none() {
            g.running -= 1;
        }
        g.threads.swap_remove(i);
    }
    world().cv.notify_all();
    drop(g);
    l.registered.set(false);
}

/// A thread that runs rscl code registers itself before it allocates or holds
/// values (stopping at a safepoint or in a blocking region registers too).
pub fn register_thread() {
    let _ = LOCAL.try_with(|l| {
        if !l.registered.get() {
            join_world(l);
        }
    });
}

/// Leave the set of rscl threads (also done when the thread exits).
pub fn unregister_thread() {
    let _ = LOCAL.try_with(leave_world);
    crate::heap::thread_exit();
}

/// Values held in this thread's runtime state rather than on its stack.
fn thread_roots(l: &Local) -> Vec<u64> {
    let mut out = Vec::new();
    crate::exc::pending_root(&mut out);
    var::binding_roots(&mut out);
    crate::native::future_root(&mut out);
    if let Ok(slices) = l.slices.try_borrow() {
        for &(p, n) in slices.iter() {
            for i in 0..n {
                out.push(unsafe { *(p as *const u64).add(i) });
            }
        }
    }
    out
}

/// Publish this thread as stopped.
fn stop_here(l: &Local, frame: &Frame) -> MutexGuard<'static, World> {
    let roots = thread_roots(l);
    let id = crate::native::thread_id();
    let mut g = lock_world();
    if let Some(t) = g.threads.iter_mut().find(|t| t.id == id) {
        t.stopped = Some(Stopped {
            frame: *frame,
            roots,
        });
    }
    g.running -= 1;
    world().cv.notify_all();
    g
}

/// Back from being stopped (waits for a collection in progress first).
fn resume(g: MutexGuard<'static, World>) {
    let id = crate::native::thread_id();
    let mut g = wait_idle(g);
    if let Some(t) = g.threads.iter_mut().find(|t| t.id == id) {
        t.stopped = None;
    }
    g.running += 1;
}

/// Run a blocking operation (sleeping, waiting for a lock or for input). The thread
/// counts as stopped for the collector meanwhile, so `f` must not touch rscl values
/// or allocate objects; whatever it returns is used only after a collection in
/// progress has finished.
pub fn blocking<R>(f: impl FnOnce() -> R) -> R {
    let direct = LOCAL
        .try_with(|l| l.stopped.get() || l.collecting.get() || l.inhibit.get() > 0)
        .unwrap_or(true);
    if direct || HOLD.with(|h| h.get()) > 0 {
        return f();
    }
    register_thread();
    with_frame(|frame| {
        struct Back;
        impl Drop for Back {
            fn drop(&mut self) {
                resume(lock_world());
                let _ = LOCAL.try_with(|l| l.stopped.set(false));
            }
        }
        LOCAL.with(|l| {
            drop(stop_here(l, frame));
            l.stopped.set(true);
        });
        let _back = Back;
        f()
    })
}

/// How long a collector waits for the other threads to reach a safepoint before
/// giving up (a thread inside a long native operation is not at one).
const STOP_TIMEOUT: Duration = Duration::from_millis(200);

/// A thread at a safepoint while a collection is wanted: either park until the
/// collection is over, or, if nobody is collecting yet, become the collector.
/// Returns the number of objects freed when this thread collected.
pub fn safepoint() -> Option<usize> {
    if HOLD.with(|h| h.get()) > 0 {
        // A native frame below holds values the scan would miss. The request stays
        // up, so the first safepoint after that frame is gone collects.
        return None;
    }
    let skip = LOCAL
        .try_with(|l| {
            if l.inhibit.get() > 0 {
                // Not now: ask again when the last guard goes, and stop sending this
                // thread to the slow path meanwhile.
                if GC_WANTED.swap(false, Ordering::AcqRel) || stopping() {
                    l.deferred.set(true);
                }
                if !stopping() {
                    crate::native::safepoint_done();
                }
                return true;
            }
            l.stopped.get() || l.collecting.get()
        })
        .unwrap_or(true);
    if skip {
        return None;
    }
    register_thread();
    with_frame(safepoint_at)
}

fn safepoint_at(frame: &Frame) -> Option<usize> {
    let w = world();
    let g = lock_world();
    match g.phase {
        Phase::Requested | Phase::Running => {
            drop(g);
            let g = LOCAL.with(|l| stop_here(l, frame));
            resume(g);
            clear_dead_stack();
            None
        }
        Phase::Idle => {
            let mut g = g;
            if !GC_WANTED.swap(false, Ordering::AcqRel) {
                // Nothing to do (a request already served): lower the flag.
                drop(g);
                crate::native::safepoint_done();
                return None;
            }
            // The flag stays raised until the collection is over, so every running
            // thread reaches `take_interrupt` and parks.
            crate::native::request_safepoint();
            g.phase = Phase::Requested;
            STOPPING.store(true, Ordering::Release);
            w.cv.notify_all();
            let deadline = Instant::now() + STOP_TIMEOUT;
            while g.running > 1 {
                let now = Instant::now();
                if now >= deadline {
                    // Someone is busy in native code or compiling: try again after
                    // some more allocation.
                    g.phase = Phase::Idle;
                    STOPPING.store(false, Ordering::Release);
                    w.cv.notify_all();
                    drop(g);
                    crate::native::safepoint_done();
                    crate::heap::gc_postponed();
                    return None;
                }
                let (guard, _) =
                    w.cv.wait_timeout(g, deadline - now)
                        .unwrap_or_else(|e| e.into_inner());
                g = guard;
            }
            g.phase = Phase::Running;
            let me = crate::native::thread_id();
            let others: Vec<(usize, Stopped)> = g
                .threads
                .iter()
                .filter(|t| t.id != me)
                .filter_map(|t| t.stopped.clone().map(|s| (t.stack_base, s)))
                .collect();
            drop(g);
            let freed = LOCAL.with(|l| {
                l.collecting.set(true);
                let freed = collect(l, frame, &others);
                l.collecting.set(false);
                freed
            });
            let mut g = lock_world();
            g.phase = Phase::Idle;
            STOPPING.store(false, Ordering::Release);
            w.cv.notify_all();
            drop(g);
            crate::native::safepoint_done();
            clear_dead_stack();
            Some(freed)
        }
    }
}

/// `(gc)`: collect now, on the calling thread. `None` if the other threads could not
/// be stopped in time (or collections are deferred on this thread).
pub fn collect_now() -> Option<usize> {
    clear_dead_stack();
    collect_now_inner()
}

#[inline(never)]
fn collect_now_inner() -> Option<usize> {
    GC_WANTED.store(true, Ordering::Release);
    safepoint()
}

// ---- marking -----------------------------------------------------------------------------

/// Largest distance, in words, between an object's address and a pointer into it
/// that the conservative scan still recognises.
const INTERIOR_WORDS: usize = 16;

pub(crate) struct Marker {
    stack: Vec<usize>,
    verify: bool,
    /// `RSCL_GC_EXPLAIN`: who first reached each object (0: a root).
    parents: Option<HashMap<usize, usize>>,
    current: usize,
}

#[inline]
fn header(addr: usize) -> &'static ObjHeader {
    unsafe { &*(addr as *const ObjHeader) }
}

impl Marker {
    /// Mark the object at `addr` (known to be one) and queue it for tracing.
    #[inline]
    fn object(&mut self, addr: usize) {
        let h = header(addr);
        let f = h.flags.load(Ordering::Relaxed);
        if f & MARK == 0 {
            h.flags.store(f | MARK, Ordering::Relaxed);
            self.stack.push(addr);
            if let Some(p) = &mut self.parents {
                p.insert(addr, self.current);
            }
        }
    }

    /// A reference held by a traced object (or another precise root).
    #[inline]
    pub(crate) fn value(&mut self, v: &Value) {
        if v.is_ptr() {
            let addr = v.bits() as usize;
            if self.verify && crate::arena::find(addr) != Some(addr) {
                panic!("gc: traced reference {addr:#x} is not a live object");
            }
            self.object(addr);
        }
    }

    /// A word that is a reference if it is the address of an object.
    #[inline]
    fn word(&mut self, w: usize) {
        if w & 7 == 0 && crate::arena::find(w) == Some(w) {
            self.object(w);
        }
    }

    /// A word from a stack or register: a reference if it is the address of an
    /// object or points inside one.
    #[inline]
    fn stack_word(&mut self, w: usize) {
        let Some(obj) = crate::arena::find(w) else {
            return;
        };
        if obj != w {
            // A pointer into an object counts only for the kinds whose contents
            // Rust code borrows (collections, strings, records). For sequence
            // cells, closures and lock-guarded objects the only inner pointers are
            // lock guards and field reads by a holder that has the object itself,
            // and a stale one would keep a whole lazy sequence alive (a thunk's
            // captures include the sequence it walks).
            if matches!(
                header(obj).kind,
                Kind::LazySeq
                    | Kind::Cons
                    | Kind::List
                    | Kind::Fn
                    | Kind::Atom
                    | Kind::Pending
                    | Kind::Var
            ) || w - obj > 8 * INTERIOR_WORDS
            {
                return;
            }
        }
        self.object(obj);
    }

    fn drain(&mut self) {
        while let Some(addr) = self.stack.pop() {
            self.current = addr;
            unsafe { trace(addr, self) };
        }
    }
}

/// Scan `[from, to)` word by word. The memory may hold anything (dead stack slots
/// included), which is why this function has a name tools can suppress on.
#[inline(never)]
fn scan_conservative(m: &mut Marker, from: usize, to: usize) {
    let mut p = (from + 7) & !7;
    let explain = config().explain;
    while p + 8 <= to {
        let w = unsafe { std::ptr::read_volatile(p as *const usize) };
        if explain {
            explain_root(m, p, w, to);
        }
        m.stack_word(w);
        p += 8;
    }
}

/// Diagnostic landmarks: label an address on the stack (`RSCL_GC_EXPLAIN`).
pub fn landmark(name: &str) {
    if config().explain {
        let m = 0usize;
        eprintln!("[gc] landmark {name} at {:#x}", &m as *const usize as usize);
    }
}

/// `RSCL_GC_EXPLAIN`: report each stack slot that refers to a sequence cell, with
/// the compiled fns whose return addresses lie above it (the frames it may be in).
fn explain_root(m: &Marker, slot: usize, w: usize, to: usize) {
    let _ = m;
    if w & 7 != 0 || crate::arena::find(w) != Some(w) {
        return;
    }
    let kind = header(w).kind;
    if !matches!(kind, Kind::LazySeq | Kind::Cons | Kind::List | Kind::Fn) {
        return;
    }
    let mut frames = Vec::new();
    let mut q = slot + 8;
    while q + 8 <= to && frames.len() < 3 && q < slot + 4096 {
        let ra = unsafe { std::ptr::read_volatile(q as *const usize) };
        if let Some(name) = crate::trace::fn_at(ra) {
            frames.push(format!("{name}@+{}", q - slot));
        }
        q += 8;
    }
    let first = if kind == Kind::Cons {
        unsafe { (*(w as *const cons::ConsObj)).first.bits() as i64 >> 1 }
    } else {
        -1
    };
    eprintln!(
        "[gc] root {kind:?} {w:#x} (first {first}) in slot {slot:#x} ({} below base); above: {}",
        to - slot,
        frames.join(", ")
    );
}

/// Visit every value the object at `addr` holds.
unsafe fn trace(addr: usize, m: &mut Marker) {
    let p = addr as *mut ObjHeader;
    unsafe {
        match (*p).kind {
            Kind::List => {
                let o = &*(p as *const list::ListObj);
                m.value(&o.first);
                m.value(&o.rest);
                m.value(&o.meta);
            }
            Kind::Cons => {
                let o = &*(p as *const cons::ConsObj);
                m.value(&o.first);
                m.value(&o.more);
            }
            Kind::Symbol => m.value(&(*(p as *const symbol::SymObj)).meta),
            Kind::Fn => {
                let o = &*(p as *const func::FnObj);
                for i in 0..o.ncaptures as usize {
                    m.value(&*func::capture_ptr_raw(p as *const func::FnObj, i));
                }
            }
            Kind::Exception => {
                let o = &*(p as *const exception::ExcObj);
                m.value(&o.message);
                m.value(&o.data);
                m.value(&o.cause);
            }
            Kind::Instance => {
                let o = &*(p as *const types::InstanceObj);
                for f in o.fields.iter() {
                    m.value(f);
                }
                m.value(&o.ext);
                m.value(&o.meta);
            }
            Kind::Atom => atom::gc_trace(p as *mut atom::AtomObj, &mut |v| m.value(v)),
            Kind::Pending => pending::gc_trace(p as *mut pending::PendingObj, &mut |v| m.value(v)),
            Kind::LazySeq => lazyseq::gc_trace(p as *mut lazyseq::LazySeqObj, &mut |v| m.value(v)),
            Kind::Vector => {
                let o = &*(p as *const vector::VecObj);
                for x in o.v.iter() {
                    m.value(x);
                }
                m.value(&o.meta);
            }
            Kind::Map => {
                let o = &*(p as *const map::MapObj);
                for (k, x) in o.m.iter() {
                    m.value(k);
                    m.value(x);
                }
                if let Some(order) = &o.order {
                    for x in order.iter() {
                        m.value(x);
                    }
                }
                m.value(&o.meta);
            }
            Kind::Set => {
                let o = &*(p as *const set::SetObj);
                for x in o.s.iter() {
                    m.value(x);
                }
                if let Some(order) = &o.order {
                    for x in order.iter() {
                        m.value(x);
                    }
                }
                m.value(&o.meta);
            }
            Kind::SortedMap => {
                let o = &*(p as *const sorted::SortedMapObj);
                for (k, x) in o.m.iter() {
                    m.value(&k.0);
                    m.value(x);
                }
                m.value(&o.meta);
            }
            Kind::SortedSet => {
                let o = &*(p as *const sorted::SortedSetObj);
                for x in o.s.iter() {
                    m.value(&x.0);
                }
                m.value(&o.meta);
            }
            Kind::Var => var::gc_trace(p as *mut var::VarObj, &mut |v| m.value(v)),
            Kind::Type => {
                let o = &*(p as *const types::TypeObj);
                for k in o.field_kws.iter() {
                    m.value(k);
                }
            }
            Kind::Protocol => {
                types::gc_trace_protocol(p as *mut types::ProtocolObj, &mut |v| m.value(v))
            }
            // Leaves: numbers, strings, keywords, regexes, native handles; a
            // namespace maps names to vars, which are permanent themselves.
            Kind::Long
            | Kind::Double
            | Kind::String
            | Kind::Keyword
            | Kind::Regex
            | Kind::Native
            | Kind::BigInt
            | Kind::Namespace => {}
        }
    }
}

// ---- a collection ------------------------------------------------------------------------

/// Mark from the roots and free what was not reached. The world is stopped;
/// `others` are the other threads' published states.
fn collect(l: &Local, frame: &Frame, others: &[(usize, Stopped)]) -> usize {
    let started = Instant::now();
    let own_roots = thread_roots(l);
    let mut guard = heap();
    let h = &mut *guard;
    let before = crate::arena::object_count();
    let mut m = Marker {
        stack: Vec::new(),
        verify: config().verify,
        parents: config().explain.then(HashMap::new),
        current: 0,
    };
    m.current = 1;
    for &p in &h.permanent {
        m.object(p);
    }
    m.current = 2;
    for &p in &h.protos {
        m.value(unsafe { &(*(p as *const func::FnProto)).meta });
    }
    m.current = 3;
    for &bits in h.pins.keys() {
        m.word(bits as usize);
    }
    m.current = 4;
    for &(addr, words) in &h.ranges {
        for i in 0..words {
            m.word(unsafe { *(addr as *const usize).add(i) });
        }
    }
    // This thread, then the stopped ones.
    m.current = 5;
    scan_conservative(&mut m, frame.sp, l.stack_base.get());
    m.current = 6;
    for &r in &frame.regs {
        if config().explain && crate::arena::find(r) == Some(r) {
            eprintln!("[gc] root {:?} {r:#x} in a register", header(r).kind);
        }
        m.stack_word(r);
    }
    m.current = 7;
    for &r in &own_roots {
        if config().explain && crate::arena::find(r as usize) == Some(r as usize) {
            eprintln!(
                "[gc] root {:?} {r:#x} in thread state",
                header(r as usize).kind
            );
        }
        m.word(r as usize);
    }
    m.current = 8;
    for (base, s) in others {
        scan_conservative(&mut m, s.frame.sp, *base);
        for &r in &s.frame.regs {
            m.stack_word(r);
        }
        for &r in &s.roots {
            m.word(r as usize);
        }
    }
    m.drain();
    if let Some(parents) = &m.parents {
        // The longest chain of objects, printed from its root end.
        let mut depth: HashMap<usize, u32> = HashMap::new();
        let mut best = (0u32, 0usize);
        for &start in parents.keys() {
            let mut path = Vec::new();
            let mut a = start;
            let base = loop {
                if let Some(&d) = depth.get(&a) {
                    break d;
                }
                path.push(a);
                match parents.get(&a) {
                    Some(&p) if p > 16 => a = p,
                    _ => break 0,
                }
            };
            for (i, &x) in path.iter().rev().enumerate() {
                depth.insert(x, base + i as u32 + 1);
            }
            let d = base + path.len() as u32;
            if d > best.0 {
                best = (d, start);
            }
        }
        let mut chain = Vec::new();
        let mut a = best.1;
        let tag = loop {
            chain.push(a);
            match parents.get(&a) {
                Some(&p) if p > 16 => a = p,
                Some(&p) => break p,
                None => break 99,
            }
        };
        let tail: Vec<String> = chain
            .iter()
            .rev()
            .take(8)
            .map(|&a| format!("{:?}@{a:#x}", header(a).kind))
            .collect();
        eprintln!(
            "[gc] longest chain: {} objects, root kind {tag} (1 permanent, 2 proto, 3 pin, 4 table, 5 stack, 6 register, 7 thread state, 8 other thread): {}",
            best.0,
            tail.join(" -> ")
        );
    }

    // Sweep: an unmarked object is finalised (a keyword leaves the intern table
    // first) and its slot released.
    let freed = crate::arena::sweep(|p| {
        let hd = unsafe { &*p };
        let f = hd.flags.load(Ordering::Relaxed);
        if f & MARK != 0 {
            hd.flags.store(f & !MARK, Ordering::Relaxed);
            true
        } else if f & PERMANENT != 0 {
            true
        } else {
            if hd.kind == Kind::Keyword {
                obj::keyword::forget(&[p as usize]);
            }
            unsafe { obj::free(p) };
            false
        }
    });
    let after = crate::arena::object_count();
    drop(guard);
    COLLECTIONS.fetch_add(1, Ordering::Relaxed);
    crate::heap::gc_finished();
    if config().debug {
        eprintln!(
            "[gc] {} objects, {} freed, {} live, {} threads, {:.2} ms, heap {} KB",
            before,
            freed,
            after,
            others.len() + 1,
            started.elapsed().as_secs_f64() * 1e3,
            crate::heap::used() >> 10,
        );
    }
    freed
}
