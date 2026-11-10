//! The garbage collector: stop-the-world mark and sweep over a non-moving heap.
//!
//! Every heap object is registered here when it is allocated and stays at its
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
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use crate::obj::{
    self, Kind, atom, cons, exception, func, lazyseq, list, map, pending, set, sorted, symbol,
    types, var, vector,
};
use crate::value::{MARK, ObjHeader, PERMANENT, Value};

// ---- the object registry -------------------------------------------------------------

/// Hasher for object addresses (already unique; one multiplication spreads them).
#[derive(Default)]
struct AddrHasher(u64);

impl Hasher for AddrHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 << 8) | b as u64;
        }
    }
    fn write_usize(&mut self, n: usize) {
        self.0 = ((n as u64) >> 3).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

type AddrSet = HashSet<usize, BuildHasherDefault<AddrHasher>>;

struct Heap {
    /// Every object, permanent ones included.
    objects: AddrSet,
    /// Objects handed over by threads since the last collection.
    incoming: Vec<usize>,
    /// Objects that are roots and never freed.
    permanent: Vec<usize>,
    /// Fn prototypes (not objects; their metadata is a root).
    protos: Vec<usize>,
    /// Constant tables of compiled units: (address, words).
    ranges: Vec<(usize, usize)>,
    /// Pinned values and how many pins each has.
    pins: HashMap<u64, usize>,
    /// Lowest and highest object address (a cheap filter for the stack scan).
    lo: usize,
    hi: usize,
}

fn heap() -> MutexGuard<'static, Heap> {
    static H: OnceLock<Mutex<Heap>> = OnceLock::new();
    H.get_or_init(|| {
        Mutex::new(Heap {
            objects: AddrSet::default(),
            incoming: Vec::new(),
            permanent: Vec::new(),
            protos: Vec::new(),
            ranges: Vec::new(),
            pins: HashMap::new(),
            lo: usize::MAX,
            hi: 0,
        })
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

/// Per-thread collector state.
struct Local {
    /// Objects this thread allocated and has not handed to the registry yet.
    new: RefCell<Vec<usize>>,
    registered: Cell<bool>,
    /// Highest address of this thread's stack.
    stack_base: Cell<usize>,
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
            new: RefCell::new(Vec::new()),
            registered: Cell::new(false),
            stack_base: Cell::new(0),
            stopped: Cell::new(false),
            collecting: Cell::new(false),
            inhibit: Cell::new(0),
            deferred: Cell::new(false),
            slices: RefCell::new(Vec::new()),
        }
    };
}

/// Hand a freshly allocated object to the collector.
#[inline]
pub(crate) fn register(p: *mut ObjHeader) {
    let done = LOCAL.try_with(|l| {
        if !l.registered.get() {
            join_world(l);
        }
        l.new.borrow_mut().push(p as usize);
    });
    if done.is_err() {
        // The thread's locals are gone (it is exiting).
        heap().incoming.push(p as usize);
    }
}

/// Register an object that is never freed and is a root (its header is permanent).
pub(crate) fn register_permanent(p: *mut ObjHeader) {
    let mut h = heap();
    h.incoming.push(p as usize);
    h.permanent.push(p as usize);
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
pub struct SliceRoot<'a>(std::marker::PhantomData<&'a [Value]>);

/// Root the values of `s` for the lifetime of the guard: for natives that call rscl
/// code while they hold values in a `Vec` or another heap container. Guards must be
/// dropped in the reverse order of their creation (as scopes do).
pub fn root_slice(s: &[Value]) -> SliceRoot<'_> {
    unsafe { root_words(s.as_ptr() as *const u64, s.len()) }
}

/// # Safety
/// `ptr` must point at `len` values that stay valid while the guard exists.
pub(crate) unsafe fn root_words<'a>(ptr: *const u64, len: usize) -> SliceRoot<'a> {
    let _ = LOCAL.try_with(|l| l.slices.borrow_mut().push((ptr as usize, len)));
    SliceRoot(std::marker::PhantomData)
}

impl Drop for SliceRoot<'_> {
    fn drop(&mut self) {
        let _ = LOCAL.try_with(|l| l.slices.borrow_mut().pop());
    }
}

/// Number of objects the collector knows about.
pub fn object_count() -> usize {
    let h = heap();
    h.objects.len() + h.incoming.len()
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
    let (_, top) = crate::native::thread_stack_bounds();
    // Unknown bounds (not a pthread stack): scan from here up to the page end at
    // least; better than nothing, and unknown on no supported platform.
    let marker = 0usize;
    let base = if top != 0 {
        top
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

/// Remove the calling thread: its unregistered objects go to the registry first,
/// while no collection can be running.
fn leave_world(l: &Local) {
    if !l.registered.get() {
        return;
    }
    flush_new(l);
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

fn flush_new(l: &Local) {
    let mut new = l.new.borrow_mut();
    if !new.is_empty() {
        heap().incoming.append(&mut new);
    }
}

/// A thread that runs rscl code. Registration also happens by itself on the first
/// allocation; calling this early makes the whole stack visible from the start.
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

/// Publish this thread as stopped. Its new objects are handed over first.
fn stop_here(l: &Local, frame: &Frame) -> MutexGuard<'static, World> {
    flush_new(l);
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
    if direct {
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
            Some(freed)
        }
    }
}

/// `(gc)`: collect now, on the calling thread. `None` if the other threads could not
/// be stopped in time (or collections are deferred on this thread).
pub fn collect_now() -> Option<usize> {
    GC_WANTED.store(true, Ordering::Release);
    safepoint()
}

// ---- marking -----------------------------------------------------------------------------

/// Largest distance, in words, between an object's address and a pointer into it
/// that the conservative scan still recognises.
const INTERIOR_WORDS: usize = 16;

pub(crate) struct Marker<'a> {
    objects: &'a AddrSet,
    lo: usize,
    hi: usize,
    stack: Vec<usize>,
    verify: bool,
}

#[inline]
fn header(addr: usize) -> &'static ObjHeader {
    unsafe { &*(addr as *const ObjHeader) }
}

impl Marker<'_> {
    /// Mark the object at `addr` (known to be one) and queue it for tracing.
    #[inline]
    fn object(&mut self, addr: usize) {
        let h = header(addr);
        let f = h.flags.load(Ordering::Relaxed);
        if f & MARK == 0 {
            h.flags.store(f | MARK, Ordering::Relaxed);
            self.stack.push(addr);
        }
    }

    /// A reference held by a traced object (or another precise root).
    #[inline]
    pub(crate) fn value(&mut self, v: &Value) {
        if v.is_ptr() {
            let addr = v.bits() as usize;
            if self.verify && !self.objects.contains(&addr) {
                panic!("gc: traced reference {addr:#x} is not a live object");
            }
            self.object(addr);
        }
    }

    /// A word that is a reference if it is the address of an object.
    #[inline]
    fn word(&mut self, w: usize) {
        if w >= self.lo && w <= self.hi && w & 7 == 0 && self.objects.contains(&w) {
            self.object(w);
        }
    }

    /// A word from a stack or register: a reference if it is the address of an
    /// object or points inside one.
    #[inline]
    fn stack_word(&mut self, w: usize) {
        if w < self.lo || w > self.hi.saturating_add(8 * INTERIOR_WORDS) || w & 7 != 0 {
            return;
        }
        if self.objects.contains(&w) {
            self.object(w);
            return;
        }
        for back in 1..=INTERIOR_WORDS {
            let Some(a) = w.checked_sub(8 * back) else {
                return;
            };
            if a < self.lo {
                return;
            }
            if self.objects.contains(&a) {
                if 8 * back < unsafe { obj::size_of(a as *const ObjHeader) } {
                    self.object(a);
                }
                return;
            }
        }
    }

    fn drain(&mut self) {
        while let Some(addr) = self.stack.pop() {
            unsafe { trace(addr, self) };
        }
    }
}

/// Scan `[from, to)` word by word. The memory may hold anything (dead stack slots
/// included), which is why this function has a name tools can suppress on.
#[inline(never)]
fn scan_conservative(m: &mut Marker<'_>, from: usize, to: usize) {
    let mut p = (from + 7) & !7;
    while p + 8 <= to {
        let w = unsafe { std::ptr::read_volatile(p as *const usize) };
        m.stack_word(w);
        p += 8;
    }
}

/// Visit every value the object at `addr` holds.
unsafe fn trace(addr: usize, m: &mut Marker<'_>) {
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
    flush_new(l);
    let own_roots = thread_roots(l);
    let mut guard = heap();
    let h = &mut *guard;
    // New objects join the registry. A mark left on one (it was reached by an
    // earlier collection before its thread handed it over) is cleared.
    for p in h.incoming.drain(..) {
        let hd = header(p);
        hd.flags.fetch_and(!MARK, Ordering::Relaxed);
        h.lo = h.lo.min(p);
        h.hi = h.hi.max(p);
        h.objects.insert(p);
    }
    let before = h.objects.len();
    let mut m = Marker {
        objects: &h.objects,
        lo: h.lo,
        hi: h.hi,
        stack: Vec::new(),
        verify: config().verify,
    };
    for &p in &h.permanent {
        m.object(p);
    }
    for &p in &h.protos {
        m.value(unsafe { &(*(p as *const func::FnProto)).meta });
    }
    for &bits in h.pins.keys() {
        m.word(bits as usize);
    }
    for &(addr, words) in &h.ranges {
        for i in 0..words {
            m.word(unsafe { *(addr as *const usize).add(i) });
        }
    }
    // This thread, then the stopped ones.
    scan_conservative(&mut m, frame.sp, l.stack_base.get());
    for &r in &frame.regs {
        m.stack_word(r);
    }
    for &r in &own_roots {
        m.word(r as usize);
    }
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

    // Sweep.
    let mut dead: Vec<usize> = Vec::new();
    let mut dead_keywords: Vec<usize> = Vec::new();
    h.objects.retain(|&p| {
        let hd = header(p);
        let f = hd.flags.load(Ordering::Relaxed);
        if f & MARK != 0 {
            hd.flags.store(f & !MARK, Ordering::Relaxed);
            true
        } else if f & PERMANENT != 0 {
            true
        } else {
            if hd.kind == Kind::Keyword {
                dead_keywords.push(p);
            }
            dead.push(p);
            false
        }
    });
    obj::keyword::forget(&dead_keywords);
    for &p in &dead {
        unsafe { obj::free(p as *mut ObjHeader) };
    }
    let after = h.objects.len();
    drop(guard);
    COLLECTIONS.fetch_add(1, Ordering::Relaxed);
    crate::heap::gc_finished();
    if config().debug {
        eprintln!(
            "[gc] {} objects, {} freed, {} live, {} threads, {:.2} ms, heap {} KB",
            before,
            dead.len(),
            after,
            others.len() + 1,
            started.elapsed().as_secs_f64() * 1e3,
            crate::heap::used() >> 10,
        );
    }
    dead.len()
}
