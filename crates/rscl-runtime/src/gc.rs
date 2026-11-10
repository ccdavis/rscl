//! Cycle collection for reference-counted objects (synchronous trial deletion, after
//! Bacon and Rajan's "Recycler").
//!
//! Every object except atoms and pending values (futures, promises, delays) is
//! immutable once it is shared, so it can only point at objects created before it:
//! a reference cycle must pass through an atom or a pending value. Those are the
//! candidates. One is buffered when it is written (the write may close a cycle) and
//! when it loses a reference without dying (the reference being dropped is handed
//! to the buffer instead, so the object stays valid without any decrement that could
//! race with a concurrent free). When enough new candidates have accumulated, or on
//! `(gc)`, the world is stopped and trial deletion runs from the buffered objects:
//!
//! 1. Mark: walk everything reachable from the candidates, counting for each object
//!    how many of its references come from inside the walked subgraph.
//! 2. Scan: an object with references from outside (its count stays positive) is
//!    live, and so is everything it reaches; the rest is garbage.
//! 3. Collect: garbage objects have their outgoing references cleared (which breaks
//!    the cycles) and are released.
//!
//! Persistent collections share tree nodes between versions, so a slot inside one
//! may be owned by collections the walk never sees. A collection reached by the walk
//! is therefore only counted through when it is the sole owner of its node tree,
//! which the collector guarantees by rebuilding it (same contents, fresh nodes) when
//! the collection object itself has a single reference; any other collection is a
//! leaf whose contents count as external references. Immortal objects (vars, types,
//! protocols, constants) are never entered.
//!
//! The collector needs every other rscl thread stopped at a safepoint (fn entry, loop
//! header, or a blocking runtime operation), since trial deletion reads and adjusts
//! counts non-atomically. Threads waiting for input or on a future declare
//! themselves idle, which counts as stopped.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::obj::{
    self, Kind, atom, cons, exception, func, list, map, pending, set, sorted, symbol, types, vector,
};
use crate::value::{ObjHeader, Value};

/// A buffered candidate: the buffer holds one reference to it.
#[derive(Clone, Copy)]
struct Candidate {
    addr: usize,
    /// The object's version (writes and decrements) when it was last examined.
    version: u32,
    /// Consecutive collections that found it live and unchanged.
    strikes: u8,
}

struct Buffer {
    objs: Vec<Candidate>,
    set: HashSet<usize>,
    /// Candidates added since the last collection.
    fresh: usize,
    /// Fresh candidates that request a collection (grows with the work of the last
    /// one, so the cost stays proportional to the candidates).
    trigger: usize,
}

fn buffer() -> &'static Mutex<Buffer> {
    static B: OnceLock<Mutex<Buffer>> = OnceLock::new();
    B.get_or_init(|| {
        Mutex::new(Buffer {
            objs: Vec::new(),
            set: HashSet::new(),
            fresh: 0,
            trigger: TRIGGER,
        })
    })
}

/// Fresh candidates before a collection is requested, at least.
const TRIGGER: usize = 1024;

/// A survivor found live and unchanged this many times in a row leaves the buffer
/// (its next write or lost reference brings it back).
const MAX_STRIKES: u8 = 3;

/// Allocations per thread between checks that a quiet program with buffered
/// candidates still collects now and then.
const ALLOC_CHECK: u32 = 1 << 20;

/// The object's version counter and buffered flag (atoms and pending values).
fn marks(addr: usize) -> (&'static std::sync::atomic::AtomicU32, &'static AtomicBool) {
    let p = addr as *mut ObjHeader;
    unsafe {
        match header(addr).kind {
            Kind::Atom => {
                let o = &*(p as *const atom::AtomObj);
                (&o.version, &o.buffered)
            }
            _ => {
                let o = &*(p as *const pending::PendingObj);
                (&o.version, &o.buffered)
            }
        }
    }
}

/// `None` if already buffered; otherwise whether a collection should be requested.
fn push_candidate(b: &mut Buffer, addr: usize, version: u32) -> Option<bool> {
    if !b.set.insert(addr) {
        return None;
    }
    b.objs.push(Candidate {
        addr,
        version,
        strikes: 0,
    });
    b.fresh += 1;
    Some(b.fresh >= b.trigger)
}

/// Called by `release_raw` for an atom or pending value: if it is not buffered yet,
/// the buffer takes over the reference being dropped and the drop is done; returns
/// false when the caller must decrement normally.
///
/// # Safety
/// `p` must point at a live atom or pending object the caller holds a reference to.
pub(crate) unsafe fn take_candidate(p: *const ObjHeader) -> bool {
    if collecting() {
        return false;
    }
    let addr = p as usize;
    let (version, buffered) = marks(addr);
    let v = version.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    if buffered.swap(true, Ordering::AcqRel) {
        return false;
    }
    let request = {
        let mut b = buffer().lock().unwrap_or_else(|e| e.into_inner());
        push_candidate(&mut b, addr, v.wrapping_sub(1))
    };
    match request {
        Some(true) => {
            request_gc();
            true
        }
        Some(false) => true,
        None => false,
    }
}

thread_local! {
    static ALLOCS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Called on every allocation: a quiet program still collects its candidates.
#[inline]
pub(crate) fn note_alloc() {
    let n = ALLOCS.with(|a| {
        let n = a.get().wrapping_add(1);
        a.set(n);
        n
    });
    if n.is_multiple_of(ALLOC_CHECK)
        && !buffer()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .objs
            .is_empty()
    {
        request_gc();
    }
}

static GC_WANTED: AtomicBool = AtomicBool::new(false);

/// Collections so far.
static COLLECTIONS: Mutex<u64> = Mutex::new(0);

/// An atom or pending value was written: buffer it as a candidate (the caller holds
/// a reference, so retaining it here cannot race with a free). The object's own flag
/// keeps the common case to one atomic swap.
pub fn note_write(v: &Value) {
    if collecting() {
        return;
    }
    let addr = v.bits() as usize;
    let (version, buffered) = marks(addr);
    let ver = version.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    if buffered.swap(true, Ordering::AcqRel) {
        return;
    }
    let request = {
        let mut b = buffer().lock().unwrap_or_else(|e| e.into_inner());
        match push_candidate(&mut b, addr, ver.wrapping_sub(1)) {
            Some(r) => r,
            None => return,
        }
    };
    std::mem::forget(v.clone());
    if request {
        request_gc();
    }
}

/// Mirror of `World::phase != Idle` for the slow path, so threads that hit the
/// interrupt flag for another reason do not serialise on the world lock.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// A collector is stopping or has stopped the world.
pub fn stopping() -> bool {
    STOPPING.load(Ordering::Acquire)
}

thread_local! {
    /// Set while this thread runs a collection: its own drops must not buffer
    /// candidates (they are the collector's bookkeeping, not program activity).
    static COLLECTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn collecting() -> bool {
    COLLECTING.with(|c| c.get())
}

/// Ask for a collection at the next safepoint. Always raises the flag: an earlier
/// request may have been consumed, deferred or finished since.
pub fn request_gc() {
    GC_WANTED.store(true, Ordering::Release);
    crate::native::request_safepoint();
}

pub fn gc_wanted() -> bool {
    GC_WANTED.load(Ordering::Acquire)
}

// ---- stopping the world ---------------------------------------------------------------

#[derive(PartialEq, Eq, Clone, Copy)]
enum Phase {
    Idle,
    /// A collector waits for the other threads to stop.
    Requested,
    Running,
}

struct World {
    phase: Phase,
    /// Registered rscl threads that are not idle or parked.
    running: usize,
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
        }),
        cv: Condvar::new(),
    })
}

thread_local! {
    /// Whether this thread is counted in `World::running` (test harnesses and
    /// embedders may run rscl code on threads that never registered).
    static REGISTERED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn registered() -> bool {
    REGISTERED.with(|r| r.get())
}

/// A thread that runs rscl code registers itself; it must call `unregister_thread`
/// before it exits.
pub fn register_thread() {
    let w = world();
    let mut g = w.world.lock().unwrap_or_else(|e| e.into_inner());
    // Join the world only between collections.
    while g.phase != Phase::Idle {
        g = w.cv.wait(g).unwrap_or_else(|e| e.into_inner());
    }
    g.running += 1;
    crate::heap::arm_exit_flush();
    REGISTERED.with(|r| r.set(true));
}

pub fn unregister_thread() {
    if !registered() {
        return;
    }
    let w = world();
    let mut g = w.world.lock().unwrap_or_else(|e| e.into_inner());
    g.running -= 1;
    REGISTERED.with(|r| r.set(false));
    w.cv.notify_all();
    drop(g);
    crate::heap::thread_exit();
}

/// The thread is about to block outside rscl code (reading input, waiting): it
/// counts as stopped until `leave_idle`.
pub fn enter_idle() {
    if !registered() {
        return;
    }
    let w = world();
    let mut g = w.world.lock().unwrap_or_else(|e| e.into_inner());
    g.running -= 1;
    w.cv.notify_all();
}

pub fn leave_idle() {
    if !registered() {
        return;
    }
    let w = world();
    let mut g = w.world.lock().unwrap_or_else(|e| e.into_inner());
    while g.phase != Phase::Idle {
        g = w.cv.wait(g).unwrap_or_else(|e| e.into_inner());
    }
    g.running += 1;
}

/// How long a collector waits for the other threads to reach a safepoint before
/// giving up (a thread inside a long native operation is not at one).
const STOP_TIMEOUT: Duration = Duration::from_millis(200);

/// A thread at a safepoint while a collection is wanted: either park until the
/// collection is over, or, if nobody is collecting yet, become the collector.
/// Returns the number of objects freed when this thread collected.
pub fn safepoint() -> Option<usize> {
    let w = world();
    let mut g = w.world.lock().unwrap_or_else(|e| e.into_inner());
    let me = usize::from(registered());
    match g.phase {
        Phase::Requested | Phase::Running => {
            if me == 1 {
                g.running -= 1;
                w.cv.notify_all();
            }
            while g.phase != Phase::Idle {
                g = w.cv.wait(g).unwrap_or_else(|e| e.into_inner());
            }
            g.running += me;
            None
        }
        Phase::Idle => {
            if !GC_WANTED.swap(false, Ordering::AcqRel) {
                // Nothing to do (a request already served): lower the flag.
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
            while g.running > me {
                let now = Instant::now();
                if now >= deadline {
                    // Someone is stuck in native code: try again at a later request.
                    g.phase = Phase::Idle;
                    STOPPING.store(false, Ordering::Release);
                    w.cv.notify_all();
                    drop(g);
                    crate::native::safepoint_done();
                    GC_WANTED.store(true, Ordering::Release);
                    return None;
                }
                let (guard, _) =
                    w.cv.wait_timeout(g, deadline - now)
                        .unwrap_or_else(|e| e.into_inner());
                g = guard;
            }
            g.phase = Phase::Running;
            drop(g);
            COLLECTING.with(|c| c.set(true));
            let freed = collect();
            COLLECTING.with(|c| c.set(false));
            let mut g = w.world.lock().unwrap_or_else(|e| e.into_inner());
            g.phase = Phase::Idle;
            STOPPING.store(false, Ordering::Release);
            w.cv.notify_all();
            drop(g);
            crate::native::safepoint_done();
            // Candidates that piled up during the collection want another one.
            if GC_WANTED.load(Ordering::Acquire) {
                crate::native::request_safepoint();
            }
            Some(freed)
        }
    }
}

/// `(gc)`: collect now, on the calling thread. `None` if the other threads could not
/// be stopped in time.
pub fn collect_now() -> Option<usize> {
    GC_WANTED.store(true, Ordering::Release);
    safepoint()
}

pub fn collections() -> u64 {
    *COLLECTIONS.lock().unwrap_or_else(|e| e.into_inner())
}

// ---- trial deletion -----------------------------------------------------------------

struct Info {
    /// Refcount minus the references found inside the walked subgraph.
    count: i64,
    black: bool,
}

struct Walk {
    info: HashMap<usize, Info>,
    slots: HashSet<usize>,
    /// Objects in visiting order, for the scan and collect phases.
    order: Vec<usize>,
}

fn header(addr: usize) -> &'static ObjHeader {
    unsafe { &*(addr as *const ObjHeader) }
}

/// A heap object that refcounting may free (not immortal).
fn mortal(v: &Value) -> Option<usize> {
    let h = v.header()?;
    if h.rc.load(Ordering::Relaxed) < 0 {
        return None;
    }
    Some(v.bits() as usize)
}

/// Addresses of the value slots an object holds. A collection is counted through
/// only when it is uniquely referenced (then its node tree is made unique first);
/// any other collection, and opaque objects, hold their contents as external
/// references.
unsafe fn child_slots(addr: usize) -> Vec<*const Value> {
    let h = header(addr);
    let rc = h.rc.load(Ordering::Relaxed);
    let p = addr as *mut ObjHeader;
    unsafe {
        match h.kind {
            Kind::List => {
                let o = &*(p as *const list::ListObj);
                vec![&o.first, &o.rest, &o.meta]
            }
            Kind::Cons => {
                let o = &*(p as *const cons::ConsObj);
                vec![&o.first, &o.more]
            }
            Kind::Symbol => vec![&(*(p as *const symbol::SymObj)).meta],
            Kind::Fn => {
                let o = &*(p as *const func::FnObj);
                (0..o.ncaptures as usize)
                    .map(|i| func::capture_ptr_raw(p as *const func::FnObj, i) as *const Value)
                    .collect()
            }
            Kind::Exception => {
                let o = &*(p as *const exception::ExcObj);
                vec![&o.message, &o.data, &o.cause]
            }
            Kind::Instance => {
                let o = &*(p as *const types::InstanceObj);
                let mut v: Vec<*const Value> = o.fields.iter().map(|f| f as *const Value).collect();
                v.push(&o.ext);
                v.push(&o.meta);
                v
            }
            Kind::Atom => atom::gc_slots(p as *mut atom::AtomObj),
            Kind::Pending => pending::gc_slots(p as *mut pending::PendingObj),
            Kind::LazySeq => obj::lazyseq::gc_slots(p as *mut obj::lazyseq::LazySeqObj),
            Kind::Vector if rc == 1 => {
                let o = &mut *(p as *mut vector::VecObj);
                o.v = o.v.iter().cloned().collect();
                let mut v: Vec<*const Value> = o.v.iter().map(|x| x as *const Value).collect();
                v.push(&o.meta);
                v
            }
            Kind::Map if rc == 1 => {
                let o = &mut *(p as *mut map::MapObj);
                o.m = o.m.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                let mut v: Vec<*const Value> = Vec::new();
                for (k, x) in o.m.iter() {
                    v.push(k as *const Value);
                    v.push(x as *const Value);
                }
                v.push(&o.meta);
                v
            }
            Kind::Set if rc == 1 => {
                let o = &mut *(p as *mut set::SetObj);
                o.s = o.s.iter().cloned().collect();
                let mut v: Vec<*const Value> = o.s.iter().map(|x| x as *const Value).collect();
                v.push(&o.meta);
                v
            }
            Kind::SortedMap if rc == 1 => {
                let o = &mut *(p as *mut sorted::SortedMapObj);
                o.m = o.m.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                let mut v: Vec<*const Value> = Vec::new();
                for (k, x) in o.m.iter() {
                    v.push(&k.0 as *const Value);
                    v.push(x as *const Value);
                }
                v.push(&o.meta);
                v
            }
            Kind::SortedSet if rc == 1 => {
                let o = &mut *(p as *mut sorted::SortedSetObj);
                o.s = o.s.iter().cloned().collect();
                let mut v: Vec<*const Value> = o.s.iter().map(|x| &x.0 as *const Value).collect();
                v.push(&o.meta);
                v
            }
            // Shared collections, strings, numbers, keywords, regexes, natives, and
            // the immortal kinds: nothing to walk through.
            _ => Vec::new(),
        }
    }
}

/// Clear every outgoing reference of a garbage object (releasing what it held).
unsafe fn unlink(addr: usize) {
    let p = addr as *mut ObjHeader;
    unsafe {
        match header(addr).kind {
            Kind::List => {
                let o = &mut *(p as *mut list::ListObj);
                o.first = Value::NIL;
                o.rest = Value::NIL;
                o.meta = Value::NIL;
            }
            Kind::Cons => {
                let o = &mut *(p as *mut cons::ConsObj);
                o.first = Value::NIL;
                o.more = Value::NIL;
            }
            Kind::Symbol => (*(p as *mut symbol::SymObj)).meta = Value::NIL,
            Kind::Fn => {
                let o = &*(p as *const func::FnObj);
                for i in 0..o.ncaptures as usize {
                    let slot = func::capture_ptr_raw(p as *const func::FnObj, i);
                    drop(std::ptr::replace(slot, Value::NIL));
                }
            }
            Kind::Exception => {
                let o = &mut *(p as *mut exception::ExcObj);
                o.message = Value::NIL;
                o.data = Value::NIL;
                o.cause = Value::NIL;
            }
            Kind::Instance => {
                let o = &mut *(p as *mut types::InstanceObj);
                for f in o.fields.iter_mut() {
                    *f = Value::NIL;
                }
                o.ext = Value::NIL;
                o.meta = Value::NIL;
            }
            Kind::Atom => atom::gc_unlink(p as *mut atom::AtomObj),
            Kind::Pending => pending::gc_unlink(p as *mut pending::PendingObj),
            Kind::LazySeq => obj::lazyseq::gc_unlink(p as *mut obj::lazyseq::LazySeqObj),
            Kind::Vector => {
                let o = &mut *(p as *mut vector::VecObj);
                o.v = vector::Vec_::new();
                o.meta = Value::NIL;
            }
            Kind::Map => {
                let o = &mut *(p as *mut map::MapObj);
                o.m = map::empty_map();
                o.meta = Value::NIL;
            }
            Kind::Set => {
                let o = &mut *(p as *mut set::SetObj);
                o.s = set::empty_set();
                o.meta = Value::NIL;
            }
            Kind::SortedMap => {
                let o = &mut *(p as *mut sorted::SortedMapObj);
                o.m = sorted::OMap::new();
                o.meta = Value::NIL;
            }
            Kind::SortedSet => {
                let o = &mut *(p as *mut sorted::SortedSetObj);
                o.s = sorted::OSet::new();
                o.meta = Value::NIL;
            }
            _ => {}
        }
    }
}

impl Walk {
    fn discover(&mut self, addr: usize, buffered: &HashSet<usize>) {
        let mut count = header(addr).rc.load(Ordering::Relaxed);
        if buffered.contains(&addr) {
            count -= 1; // the buffer's own reference
        }
        self.info.insert(
            addr,
            Info {
                count,
                black: false,
            },
        );
        self.order.push(addr);
    }

    /// First phase: count internal references over everything reachable from `root`.
    fn mark(&mut self, root: usize, buffered: &HashSet<usize>) {
        if self.info.contains_key(&root) {
            return;
        }
        self.discover(root, buffered);
        let mut stack = vec![root];
        let mut expanded: HashSet<usize> = HashSet::new();
        while let Some(addr) = stack.pop() {
            if !expanded.insert(addr) {
                continue;
            }
            for slot in unsafe { child_slots(addr) } {
                if !self.slots.insert(slot as usize) {
                    continue;
                }
                let child = unsafe { &*slot };
                let Some(c) = mortal(child) else { continue };
                if !self.info.contains_key(&c) {
                    self.discover(c, buffered);
                    stack.push(c);
                }
                self.info.get_mut(&c).expect("discovered").count -= 1;
            }
        }
    }

    /// Second phase: externally referenced objects, and all they reach, are live.
    fn scan(&mut self) {
        let roots: Vec<usize> = self
            .order
            .iter()
            .copied()
            .filter(|a| self.info[a].count > 0)
            .collect();
        let mut stack = roots;
        while let Some(addr) = stack.pop() {
            let i = self.info.get_mut(&addr).expect("walked");
            if i.black {
                continue;
            }
            i.black = true;
            for slot in unsafe { child_slots_no_rebuild(addr) } {
                let child = unsafe { &*slot };
                if let Some(c) = mortal(child)
                    && self.info.contains_key(&c)
                {
                    stack.push(c);
                }
            }
        }
    }
}

/// Children for the scan phase: the same slots as the mark phase, but collections
/// were already rebuilt, so read them as they are.
unsafe fn child_slots_no_rebuild(addr: usize) -> Vec<*const Value> {
    // After `mark`, a collection that was rebuilt still has rc 1 and `child_slots`
    // would rebuild it again (harmless but wasteful); read the slots directly.
    let h = header(addr);
    let p = addr as *mut ObjHeader;
    unsafe {
        match h.kind {
            Kind::Vector if h.rc.load(Ordering::Relaxed) == 1 => {
                let o = &*(p as *const vector::VecObj);
                let mut v: Vec<*const Value> = o.v.iter().map(|x| x as *const Value).collect();
                v.push(&o.meta);
                v
            }
            Kind::Map if h.rc.load(Ordering::Relaxed) == 1 => {
                let o = &*(p as *const map::MapObj);
                let mut v: Vec<*const Value> = Vec::new();
                for (k, x) in o.m.iter() {
                    v.push(k as *const Value);
                    v.push(x as *const Value);
                }
                v.push(&o.meta);
                v
            }
            Kind::Set if h.rc.load(Ordering::Relaxed) == 1 => {
                let o = &*(p as *const set::SetObj);
                let mut v: Vec<*const Value> = o.s.iter().map(|x| x as *const Value).collect();
                v.push(&o.meta);
                v
            }
            Kind::SortedMap if h.rc.load(Ordering::Relaxed) == 1 => {
                let o = &*(p as *const sorted::SortedMapObj);
                let mut v: Vec<*const Value> = Vec::new();
                for (k, x) in o.m.iter() {
                    v.push(&k.0 as *const Value);
                    v.push(x as *const Value);
                }
                v.push(&o.meta);
                v
            }
            Kind::SortedSet if h.rc.load(Ordering::Relaxed) == 1 => {
                let o = &*(p as *const sorted::SortedSetObj);
                let mut v: Vec<*const Value> = o.s.iter().map(|x| &x.0 as *const Value).collect();
                v.push(&o.meta);
                v
            }
            _ => child_slots(addr),
        }
    }
}

/// Run one collection (the world is stopped). Returns the number of objects freed.
fn collect() -> usize {
    let (cands, buffered): (Vec<Candidate>, HashSet<usize>) = {
        let mut b = buffer().lock().unwrap_or_else(|e| e.into_inner());
        let objs = std::mem::take(&mut b.objs);
        let set = std::mem::take(&mut b.set);
        b.fresh = 0;
        (objs, set)
    };
    let roots: Vec<usize> = cands.iter().map(|c| c.addr).collect();
    *COLLECTIONS.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    let mut walk = Walk {
        info: HashMap::new(),
        slots: HashSet::new(),
        order: Vec::new(),
    };
    for &r in &roots {
        walk.mark(r, &buffered);
    }
    walk.scan();
    if std::env::var_os("RSCL_GC_DEBUG").is_some() {
        eprintln!("gc: {} roots, {} walked", roots.len(), walk.order.len());
        for a in &walk.order {
            let i = &walk.info[a];
            eprintln!(
                "  {:?} rc={} count={} black={} root={}",
                header(*a).kind,
                header(*a).rc.load(Ordering::Relaxed),
                i.count,
                i.black,
                buffered.contains(a)
            );
        }
    }
    let whites: Vec<usize> = walk
        .order
        .iter()
        .copied()
        .filter(|a| !walk.info[a].black)
        .collect();
    // Keep the garbage alive while its references are cleared, then let it go.
    for &w in &whites {
        header(w).rc.fetch_add(1, Ordering::Relaxed);
    }
    for &w in &whites {
        unsafe { unlink(w) };
    }
    // Live candidates stay buffered (keeping their reference) for a few rounds, in
    // case a reference from outside the cycle goes away soon; ones that stay live
    // and unchanged leave the buffer until their next write or lost reference.
    let freed = whites.len();
    let white_set: HashSet<usize> = whites.iter().copied().collect();
    // Releasing may free objects whose drops reach the buffer: do it after the lock.
    let mut to_release: Vec<usize> = Vec::new();
    {
        let mut b = buffer().lock().unwrap_or_else(|e| e.into_inner());
        // Amortise re-walking live survivors: the more live objects a collection
        // walks, the more fresh candidates the next one waits for.
        b.trigger = TRIGGER.max(walk.order.len() - whites.len());
        for c in cands {
            if white_set.contains(&c.addr) {
                continue;
            }
            let (version, buffered) = marks(c.addr);
            let now = version.load(Ordering::Acquire);
            let strikes = if now != c.version { 0 } else { c.strikes + 1 };
            if strikes < MAX_STRIKES && b.set.insert(c.addr) {
                b.objs.push(Candidate {
                    addr: c.addr,
                    version: now,
                    strikes,
                });
            } else {
                buffered.store(false, Ordering::Release);
                to_release.push(c.addr);
            }
        }
    }
    for addr in to_release {
        unsafe { crate::value::release_raw(addr as u64) };
    }
    // The buffer's references to the garbage (released directly: a `Value` drop
    // would buffer them again).
    for &w in &whites {
        if buffered.contains(&w) {
            unsafe { crate::value::release_raw(w as u64) };
        }
    }
    for &w in &whites {
        unsafe { crate::value::release_raw(w as u64) };
    }
    freed
}
