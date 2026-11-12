//! Lazy seqs, atoms, gensym and the macro/var helpers that `core.clj` builds on.
use std::sync::atomic::{AtomicU64, Ordering};

use crate::builtins;
use crate::coll;
use crate::core::args;
use crate::exc::{RtResult, err};
use crate::obj::exception::{CLASS_CAST, ILLEGAL_ARGUMENT};
use crate::obj::var::FLAG_MACRO;
use crate::obj::{self, Kind, atom, lazyseq, list, symbol, var, vector};
use crate::value::Value;

static GENSYM: AtomicU64 = AtomicU64::new(1);

/// Next value of the process-wide gensym counter (shared with the reader's `x#`).
pub fn next_gensym() -> u64 {
    GENSYM.fetch_add(1, Ordering::Relaxed)
}

fn expect_atom(v: &Value) -> RtResult<&atom::AtomObj> {
    atom::as_atom(v).map_or_else(
        || {
            err(
                CLASS_CAST,
                format!("{} cannot be cast to Atom", crate::core::type_name(v)),
            )
        },
        Ok,
    )
}

fn expect_var(v: &Value) -> RtResult<&var::VarObj> {
    var::as_var(v).map_or_else(
        || {
            err(
                CLASS_CAST,
                format!("{} cannot be cast to Var", crate::core::type_name(v)),
            )
        },
        Ok,
    )
}

/// `swap!`-style update: call `f` with the current value and extra args until the
/// compare-and-set succeeds. Returns (old, new).
fn swap(a: &atom::AtomObj, f: &Value, extra: &[Value]) -> RtResult<(Value, Value)> {
    loop {
        let old = a.deref();
        let mut fargs = vec![old];
        fargs.extend(extra.iter().copied());
        let new = f.call(&fargs)?;
        if a.compare_and_set(&old, new) {
            return Ok((old, new));
        }
    }
}

/// Expand `form` once if its head names a macro var visible from the current
/// namespace; otherwise return it unchanged.
pub fn macroexpand_1(form: &Value) -> RtResult<Value> {
    let Some(l) = list::as_list(form) else {
        return Ok(*form);
    };
    let items = l.to_vec();
    let Some(head) = items.first().and_then(symbol::as_sym) else {
        return Ok(*form);
    };
    let cur = crate::ns::current();
    let Some(v) = crate::ns::resolve(cur, head.ns(), head.name()) else {
        return Ok(*form);
    };
    if !v.is_macro() {
        return Ok(*form);
    }
    let f = v.root();
    if f.is_unbound() {
        return Ok(*form);
    }
    let mut margs = vec![*form, Value::NIL];
    margs.extend(items[1..].iter().cloned());
    f.call(&margs)
}

/// The chunked seq an argument must be.
fn chunked(v: &Value) -> RtResult<&crate::obj::vecseq::VecSeqObj> {
    if v.is_kind(Kind::VecSeq) {
        Ok(crate::obj::vecseq::get(v))
    } else {
        err(
            ILLEGAL_ARGUMENT,
            format!("{} is not a chunked seq", crate::core::type_name(v)),
        )
    }
}

builtins! {
    "lazy-seq*" => fn cf_lazy_seq_star(args) [1] { Ok(lazyseq::new(args[0])) }
    "realized?" => fn cf_realized_p(args) [1] {
        match args[0].kind() {
            Some(Kind::LazySeq) => Ok(Value::bool(lazyseq::is_realized(&args[0]))),
            Some(Kind::Pending) => Ok(Value::bool(crate::obj::pending::get(&args[0]).is_realized())),
            _ => Ok(Value::TRUE),
        }
    }
    // Chunked seqs (see `obj/vecseq.rs`): a chunk is a vector of up to 32 elements.
    "chunked-seq?" => fn cf_chunked_seq_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::VecSeq))) }
    "chunk-first" => fn cf_chunk_first(args) [1] {
        let s = chunked(&args[0])?;
        let chunk = s.chunk();
        // (The vector itself when it is one whole chunk.)
        Ok(if chunk.len() == s.items().len() { s.vec } else { vector::from_slice(chunk) })
    }
    "chunk-rest" => fn cf_chunk_rest(args) [1] { coll::tail(&chunked(&args[0])?.after_chunk()) }
    "chunk-cons" => fn cf_chunk_cons(args) [2] {
        let Some(chunk) = vector::as_vec(&args[0]) else { return err(ILLEGAL_ARGUMENT, "chunk-cons requires a vector") };
        if !coll::is_seqable(&args[1]) {
            return err(ILLEGAL_ARGUMENT, format!("Don't know how to create ISeq from: {}", crate::core::type_name(&args[1])));
        }
        Ok(if chunk.is_empty() { args[1] } else { crate::obj::vecseq::new(args[0], 0, args[1]) })
    }
    // `f` applied to every element of a chunk. (The results are rooted while the
    // later calls run.)
    "chunk-map" => @gc fn cf_chunk_map(args) [2] {
        let Some(chunk) = vector::as_vec(&args[1]) else { return err(ILLEGAL_ARGUMENT, "chunk-map requires a vector") };
        let root = crate::gc::ArgRoot::empty();
        let _rooted = root.enter();
        let mut out = Vec::with_capacity(chunk.len());
        for x in chunk.iter() {
            out.push(args[0].call(std::slice::from_ref(x))?);
            root.set(&out);
        }
        Ok(vector::from_slice(&out))
    }
    // The elements of a chunk for which `pred` holds (they are in the chunk, which
    // the arguments keep).
    "chunk-filter" => @gc fn cf_chunk_filter(args) [2] {
        let Some(chunk) = vector::as_vec(&args[1]) else { return err(ILLEGAL_ARGUMENT, "chunk-filter requires a vector") };
        let mut out = Vec::with_capacity(chunk.len());
        for x in chunk.iter() {
            if args[0].call(std::slice::from_ref(x))?.truthy() { out.push(*x); }
        }
        Ok(vector::from_slice(&out))
    }
    // Up to 32 numbers of the arithmetic progression from `start` towards `end`
    // (none when it is exhausted, or when `step` is zero).
    "range-chunk" => fn cf_range_chunk(args) [3] {
        use std::cmp::Ordering;
        let mut out = Vec::with_capacity(32);
        if let (Some(mut i), Some(end), Some(step)) = (args[0].as_fixnum(), args[1].as_fixnum(), args[2].as_fixnum()) {
            while out.len() < 32 && ((step > 0 && i < end) || (step < 0 && i > end)) {
                out.push(Value::int(i));
                i += step;
            }
            return Ok(vector::from_slice(&out));
        }
        let (mut cur, end, step) = (crate::num::expect_num(&args[0])?, crate::num::expect_num(&args[1])?, crate::num::expect_num(&args[2])?);
        let toward = match crate::num::sign(&step) { Ordering::Greater => Ordering::Less, Ordering::Less => Ordering::Greater, Ordering::Equal => return Ok(vector::empty()) };
        while out.len() < 32 && crate::num::cmp(&cur, &end) == Some(toward) {
            out.push(crate::num::to_value(cur.clone()));
            cur = crate::num::add(cur, step.clone())?;
        }
        Ok(vector::from_slice(&out))
    }
    "doall" => @gc fn cf_doall(args) [1, 2] {
        let s = &args[args.len() - 1];
        for x in coll::seq_iter(s) { x?; }
        Ok(*s)
    }
    "dorun" => @gc fn cf_dorun(args; slots = slots) [1, 2] {
        for x in coll::iter_at(slots.cursor(args.len() - 1))? { x?; }
        Ok(Value::NIL)
    }
    "atom" => fn cf_atom(args) [; 1] { Ok(atom::new(args[0])) }
    "atom?" => fn cf_atom_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::Atom))) }
    "deref" => @gc fn cf_deref(args) [1, 3] {
        if args.len() == 3
            && !crate::obj::pending::as_pending(&args[0])
                .is_some_and(|p| p.flavor != crate::obj::pending::Flavor::Delay)
        {
            return err(ILLEGAL_ARGUMENT, "deref with a timeout requires a future or promise");
        }
        match args[0].kind() {
            Some(Kind::Atom) => Ok(atom::get(&args[0]).deref()),
            Some(Kind::Pending) => {
                let p = crate::obj::pending::get(&args[0]);
                if args.len() == 3 {
                    let ms = args::int(&args[1], "timeout-ms")?.max(0) as u64;
                    let timeout = std::time::Duration::from_millis(ms);
                    Ok(p.deref(Some(timeout))?.unwrap_or_else(|| args[2]))
                } else {
                    Ok(p.deref(None)?.unwrap_or(Value::NIL))
                }
            }
            Some(Kind::Var) => {
                let r = var::get(&args[0]).root();
                if r.is_unbound() { return err(crate::obj::exception::ILLEGAL_STATE, format!("Var {} is unbound", var::get(&args[0]).qualified())); }
                Ok(r)
            }
            _ => err(CLASS_CAST, format!("{} cannot be cast to IDeref", crate::core::type_name(&args[0]))),
        }
    }
    "reset!" => fn cf_reset(args) [2] { Ok(expect_atom(&args[0])?.reset(args[1])) }
    "reset-vals!" => fn cf_reset_vals(args) [2] {
        let a = expect_atom(&args[0])?;
        let old = a.deref();
        let new = a.reset(args[1]);
        Ok(vector::from_iter(vec![old, new]))
    }
    "swap!" => @gc fn cf_swap(args) [; 2] { Ok(swap(expect_atom(&args[0])?, &args[1], &args[2..])?.1) }
    "swap-vals!" => @gc fn cf_swap_vals(args) [; 2] {
        let (old, new) = swap(expect_atom(&args[0])?, &args[1], &args[2..])?;
        Ok(vector::from_iter(vec![old, new]))
    }
    "compare-and-set!" => fn cf_compare_and_set(args) [3] {
        Ok(Value::bool(expect_atom(&args[0])?.compare_and_set(&args[1], args[2])))
    }
    "gensym" => fn cf_gensym(args) [0, 1] {
        let prefix = if args.is_empty() { "G__".to_string() } else { crate::printer::str_of(&args[0]) };
        Ok(symbol::new(None, &format!("{prefix}{}", next_gensym())))
    }
    "set-macro!" => fn cf_set_macro(args) [1] {
        expect_var(&args[0])?.set_flag(FLAG_MACRO, true);
        Ok(args[0])
    }
    "macro?" => fn cf_macro_p(args) [1] { Ok(Value::bool(var::as_var(&args[0]).is_some_and(|v| v.is_macro()))) }
    "macroexpand-1" => fn cf_macroexpand_1(args) [1] { macroexpand_1(&args[0]) }
    "macroexpand" => fn cf_macroexpand(args) [1] {
        let mut form = args[0];
        loop {
            let next = macroexpand_1(&form)?;
            if next.identical(&form) { return Ok(form); }
            form = next;
        }
    }
    "special-symbol?" => fn cf_special_symbol_p(args) [1] {
        Ok(Value::bool(symbol::as_sym(&args[0]).is_some_and(|s| s.ns().is_none() && crate::reader::is_special(s.name()))))
    }
    "resolve" => fn cf_resolve(args) [1] {
        let Some(s) = symbol::as_sym(&args[0]) else { return err(ILLEGAL_ARGUMENT, "resolve requires a symbol") };
        Ok(crate::ns::resolve(crate::ns::current(), s.ns(), s.name()).map_or(Value::NIL, |v| v.as_value()))
    }
    "var-get" => fn cf_var_get(args) [1] {
        let r = expect_var(&args[0])?.root();
        if r.is_unbound() { return Ok(Value::NIL); }
        Ok(r)
    }
    "bound?" => fn cf_bound_p(args) [; 1] {
        for a in args { if !expect_var(a)?.is_bound() { return Ok(Value::FALSE); } }
        Ok(Value::TRUE)
    }
    "meta" => fn cf_meta(args) [1] { Ok(obj::meta(&args[0]).unwrap_or(Value::NIL)) }
    "with-meta" => fn cf_with_meta(args) [2] {
        let v = &args[0];
        if !args[1].is_nil() && !args[1].is_kind(Kind::Map) {
            return err(ILLEGAL_ARGUMENT, "metadata must be a map or nil");
        }
        let copy = match v.kind() {
            Some(Kind::Symbol) => symbol::with_meta(v, args[1]),
            Some(Kind::List | Kind::Cons | Kind::LazySeq | Kind::VecSeq) => { let c = list::from_iter(coll::to_vec(v)?); obj::set_meta_unshared(&c, args[1]); c }
            Some(Kind::Vector) => vector::get(v).with_meta(args[1]),
            Some(Kind::Map) => { let c = obj::map::copy_of(obj::map::get(v)); obj::set_meta_unshared(&c, args[1]); c }
            Some(Kind::Set) => { let c = obj::set::copy_of(obj::set::get(v)); obj::set_meta_unshared(&c, args[1]); c }
            Some(Kind::SortedMap) => { let c = obj::sorted::map_new(obj::sorted::map_get(v).m.clone()); obj::set_meta_unshared(&c, args[1]); c }
            Some(Kind::SortedSet) => { let c = obj::sorted::set_new(obj::sorted::set_get(v).s.clone()); obj::set_meta_unshared(&c, args[1]); c }
            Some(Kind::Instance) => {
                let i = obj::types::inst(v);
                let c = obj::types::with(v, i.fields(), i.ext);
                obj::set_meta_unshared(&c, args[1]);
                c
            }
            _ => return err(CLASS_CAST, format!("{} cannot be cast to IObj", crate::core::type_name(v))),
        };
        Ok(copy)
    }
    "rand" => fn cf_rand(args) [0, 1] {
        let r = rand_f64();
        Ok(Value::double(if args.is_empty() { r } else { r * crate::num::f(&crate::num::expect_num(&args[0])?) }))
    }
    "rand-int" => fn cf_rand_int(args) [1] {
        let n = args::int(&args[0], "n")?;
        Ok(Value::int(if n <= 0 { 0 } else { (rand_f64() * n as f64) as i64 }))
    }
}

/// A small xorshift generator seeded from the clock; good enough for `rand`.
fn rand_f64() -> f64 {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        x = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9)
            | 1;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, Ordering::Relaxed);
    (x >> 11) as f64 / (1u64 << 53) as f64
}
