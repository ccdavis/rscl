//! Concurrency: futures, promises, delays, `locking`, sleeping and interruption.
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use crate::builtins;
use crate::core::args;
use crate::exc::{self, RtResult, err};
use crate::obj::exception::{ILLEGAL_ARGUMENT, ILLEGAL_STATE};
use crate::obj::pending::{self, Flavor};
use crate::obj::{Kind, func, var};
use crate::value::Value;

/// Stack of a future's thread (reserved, committed on use).
const FUTURE_STACK: usize = 64 << 20;

/// Threads started for futures, joined at exit so a program's futures finish like
/// Clojure's non-daemon threads do.
static THREADS: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());

/// Wait for every future thread to finish (new ones may be started meanwhile).
pub fn join_all() {
    loop {
        let handles: Vec<_> = THREADS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        if handles.is_empty() {
            return;
        }
        for h in handles {
            // Ctrl-C while waiting gives up on them: the process exits anyway.
            while !h.is_finished() {
                if crate::native::ctrl_c_pending() {
                    eprintln!("interrupted: not waiting for running futures");
                    return;
                }
                // The main thread may have crossed the heap limit in its last form.
                if crate::native::check_interrupt().is_err() {
                    let e = crate::exc::take().unwrap_or(Value::NIL);
                    eprintln!("Uncaught exception: {}", crate::exc::format_exception(&e));
                    std::process::exit(1);
                }
                crate::native::blocking(|| std::thread::sleep(Duration::from_millis(20)));
            }
            let _ = crate::native::blocking(|| h.join());
        }
    }
}

fn expect_pending<'a>(v: &'a Value, what: &str) -> RtResult<&'a pending::PendingObj> {
    pending::as_pending(v).ok_or_else(|| {
        exc::throw_new(
            ILLEGAL_ARGUMENT,
            format!(
                "{what} requires a future, promise or delay, got {}",
                crate::core::type_name(v)
            ),
        )
    })
}

/// Start `f` on a new thread; the future resolves to its result or its exception.
/// The caller's dynamic bindings are conveyed to the thread, as in Clojure.
pub fn future_call(f: Value) -> RtResult<Value> {
    let fut = pending::new(Flavor::Future, None);
    // The closure lives on the Rust heap until the thread runs: what it refers to
    // is pinned until then (and, harmlessly, for the thread's whole life).
    let pins: Vec<crate::gc::Pinned> = [f.clone(), fut.clone()]
        .into_iter()
        .chain(var::snapshot_bindings().into_iter().map(|(_, v)| v))
        .map(crate::gc::Pinned::new)
        .collect();
    let fut2 = fut.clone();
    let bindings = var::snapshot_bindings();
    let spawned = std::thread::Builder::new()
        .name("rscl-future".into())
        .stack_size(FUTURE_STACK)
        .spawn(move || {
            crate::native::set_stack_limit_here(FUTURE_STACK);
            crate::gc::register_thread();
            let _pins = pins;
            // Leaves the world in order even if something panics: bindings popped
            // and values dropped first, the thread unregistered last.
            let _guard = WorkerGuard {
                bindings: var::install_bindings(bindings),
            };
            crate::native::set_current_future(Some(fut2.clone()));
            let p = pending::get(&fut2);
            if !p.cancel.load(std::sync::atomic::Ordering::Acquire) {
                match f.call(&[]) {
                    Ok(v) => {
                        p.deliver(v);
                    }
                    Err(_) => {
                        let e = exc::take().unwrap_or(Value::NIL);
                        p.fail(e);
                    }
                }
            }
            // A cancellation that arrived too late to interrupt anything is consumed
            // here, so the interrupt flag can go back down.
            if p.cancel.swap(false, std::sync::atomic::Ordering::AcqRel)
                && p.cancel_counted
                    .swap(false, std::sync::atomic::Ordering::AcqRel)
            {
                crate::native::cancel_consumed();
            }
            crate::native::set_current_future(None);
        });
    match spawned {
        Ok(handle) => {
            let mut threads = THREADS.lock().unwrap_or_else(|e| e.into_inner());
            threads.retain(|h| !h.is_finished());
            threads.push(handle);
            Ok(fut)
        }
        Err(e) => err(ILLEGAL_STATE, format!("cannot start a thread: {e}")),
    }
}

// ---- locking --------------------------------------------------------------------------

/// Tears a worker thread down in order (also on a panic): its binding frame and the
/// future it was computing go first, then it leaves the stop-the-world protocol.
struct WorkerGuard {
    bindings: bool,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        crate::native::set_current_future(None);
        if self.bindings {
            var::pop_bindings();
        }
        crate::gc::unregister_thread();
    }
}

/// A reentrant lock keyed by object identity, for `locking`. An entry lives only
/// while the lock is held or waited for; the table is locked before the lock's own
/// state, in both `lock` and `unlock`.
struct Lock {
    /// (owner thread, hold count, waiters)
    state: Mutex<(u64, u32, u32)>,
    cv: Condvar,
}

type LockTable = Mutex<HashMap<u64, Arc<Lock>>>;

fn locks() -> &'static LockTable {
    static L: OnceLock<LockTable> = OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock(v: &Value) -> RtResult<()> {
    if !v.is_ptr() {
        return err(ILLEGAL_ARGUMENT, "Cannot lock an immediate value");
    }
    let me = crate::native::thread_id();
    let key = v.bits();
    // Register as a waiter under the table lock, so `unlock` cannot drop the entry
    // between the lookup and the wait.
    let l = {
        let mut table = locks().lock().unwrap_or_else(|e| e.into_inner());
        let l = table
            .entry(key)
            .or_insert_with(|| {
                Arc::new(Lock {
                    state: Mutex::new((0, 0, 0)),
                    cv: Condvar::new(),
                })
            })
            .clone();
        l.state.lock().unwrap_or_else(|e| e.into_inner()).2 += 1;
        l
    };
    let mut st = l.state.lock().unwrap_or_else(|e| e.into_inner());
    let r = loop {
        if st.0 == 0 || st.0 == me {
            st.0 = me;
            st.1 += 1;
            break Ok(());
        }
        drop(st);
        // Waiting is a blocking region (stopped for the collector, interruptible).
        crate::native::blocking(|| {
            let st = l.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.0 != 0 && st.0 != me {
                let _ =
                    l.cv.wait_timeout(st, Duration::from_millis(20))
                        .unwrap_or_else(|e| e.into_inner());
            }
        });
        let check = crate::native::check_interrupt();
        st = l.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = check {
            break Err(e);
        }
    };
    st.2 -= 1;
    if r.is_err() && st.0 == 0 && st.1 == 0 && st.2 == 0 {
        // Interrupted, and the holder let go meanwhile: nobody will unlock it, so
        // the entry goes now (table before state, as everywhere).
        drop(st);
        let mut table = locks().lock().unwrap_or_else(|e| e.into_inner());
        let st = l.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.0 == 0 && st.1 == 0 && st.2 == 0 {
            table.remove(&key);
        }
    }
    r
}

fn unlock(v: &Value) -> RtResult<()> {
    let me = crate::native::thread_id();
    let mut table = locks().lock().unwrap_or_else(|e| e.into_inner());
    let Some(l) = table.get(&v.bits()).cloned() else {
        return err(ILLEGAL_STATE, "unlock of an object that is not locked");
    };
    let mut st = l.state.lock().unwrap_or_else(|e| e.into_inner());
    if st.0 != me {
        return err(
            ILLEGAL_STATE,
            "unlock by a thread that does not hold the lock",
        );
    }
    st.1 -= 1;
    if st.1 == 0 {
        st.0 = 0;
        if st.2 == 0 {
            // Nobody holds or waits for it: forget the entry.
            table.remove(&v.bits());
        } else {
            l.cv.notify_one();
        }
    }
    Ok(())
}

/// Sleep in short slices so an interrupt or cancellation is noticed promptly.
pub fn sleep_ms(ms: i64) -> RtResult<()> {
    let deadline = std::time::Instant::now() + Duration::from_millis(ms.max(0) as u64);
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            return Ok(());
        }
        crate::native::blocking(|| {
            std::thread::sleep((deadline - now).min(Duration::from_millis(10)))
        });
        crate::native::check_interrupt()?;
    }
}

builtins! {
    "future-call" => @gc fn cc_future_call(args) [1] {
        if func::as_fn(&args[0]).is_none() {
            return err(ILLEGAL_ARGUMENT, "future-call requires a fn");
        }
        future_call(args[0].owned())
    }
    "future?" => fn cc_future_p(args) [1] {
        Ok(Value::bool(pending::as_pending(&args[0]).is_some_and(|p| p.flavor == Flavor::Future)))
    }
    "future-done?" => fn cc_future_done_p(args) [1] { Ok(Value::bool(expect_pending(&args[0], "future-done?")?.is_realized())) }
    "future-cancelled?" => fn cc_future_cancelled_p(args) [1] { Ok(Value::bool(expect_pending(&args[0], "future-cancelled?")?.is_cancelled())) }
    "future-cancel" => fn cc_future_cancel(args) [1] { Ok(Value::bool(expect_pending(&args[0], "future-cancel")?.cancel())) }
    "promise" => fn cc_promise(args) [0] { let _ = args; Ok(pending::new(Flavor::Promise, None)) }
    "deliver" => fn cc_deliver(args) [2] {
        let p = expect_pending(&args[0], "deliver")?;
        if p.flavor != Flavor::Promise {
            return err(ILLEGAL_ARGUMENT, "deliver requires a promise");
        }
        Ok(if p.deliver(args[1].owned()) { args[0].owned() } else { Value::NIL })
    }
    "delay*" => fn cc_delay_star(args) [1] {
        if func::as_fn(&args[0]).is_none() {
            return err(ILLEGAL_ARGUMENT, "delay requires a fn");
        }
        Ok(pending::new(Flavor::Delay, Some(args[0].owned())))
    }
    "delay?" => fn cc_delay_p(args) [1] {
        Ok(Value::bool(pending::as_pending(&args[0]).is_some_and(|p| p.flavor == Flavor::Delay)))
    }
    "force" => @gc fn cc_force(args) [1] {
        match pending::as_pending(&args[0]) {
            Some(p) if p.flavor == Flavor::Delay => Ok(p.deref(None)?.unwrap_or(Value::NIL)),
            _ => Ok(args[0].owned()),
        }
    }
    "lock*" => @gc fn cc_lock(args) [1] {
        lock(&args[0])?;
        Ok(Value::NIL)
    }
    // (gc): a full collection. (gc :minor): only what was allocated since the last
    // collection is examined. Returns the number of objects freed.
    "gc" => @gc fn cc_gc(args) [0, 1] {
        let minor = match args.first() {
            None => false,
            Some(k) if crate::obj::keyword::as_kw(k).is_some_and(|k| k.ns().is_none() && k.name() == "minor") => true,
            Some(k) => return err(ILLEGAL_ARGUMENT, format!("gc: unknown option {}", crate::printer::pr_str(k))),
        };
        match crate::gc::collect_now(minor) {
            Some(n) => Ok(Value::int(n as i64)),
            None => err(ILLEGAL_STATE, "gc: cannot collect now (another thread did not reach a safepoint in time, or collections are deferred here)"),
        }
    }
    "gc-count" => fn cc_gc_count(args) [0] { let _ = args; Ok(Value::int(crate::gc::collections() as i64)) }
    "unlock*" => fn cc_unlock(args) [1] { unlock(&args[0])?; Ok(Value::NIL) }
    "sleep" => @gc fn cc_sleep(args) [1] { sleep_ms(args::int(&args[0], "ms")?)?; Ok(Value::NIL) }
    "available-processors" => fn cc_available_processors(args) [0] {
        let _ = args;
        Ok(Value::int(std::thread::available_parallelism().map_or(1, |n| n.get() as i64)))
    }
    "thread-id" => fn cc_thread_id(args) [0] { let _ = args; Ok(Value::int(crate::native::thread_id() as i64)) }
    "pending-kind" => fn cc_pending_kind(args) [1] {
        Ok(match args[0].kind() {
            Some(Kind::Pending) => crate::obj::keyword::intern(None, &pending::get(&args[0]).flavor.name().to_lowercase()),
            _ => Value::NIL,
        })
    }
}
