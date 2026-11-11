//! Function combinators. These return native closures: `FnObj`s whose entry is a Rust
//! function that reads its captures (`this`).
use std::sync::OnceLock;

use crate::coll;
use crate::exc::RtResult;
use crate::obj::func::{self, Entry, FnProto};
use crate::obj::{list, vector};
use crate::value::{VRef, Value};
use crate::{builtins, native_fns};

/// Lazily created immortal prototype for a native closure kind.
fn proto(
    cell: &'static OnceLock<usize>,
    entry: Entry,
    name: &str,
    mask: u64,
    vmin: i64,
) -> *const FnProto {
    *cell.get_or_init(|| func::new_proto(entry, name, mask, vmin) as usize) as *const FnProto
}

fn closure(p: *const FnProto, captures: Vec<Value>) -> Value {
    let f = func::new(p, captures.len());
    for (i, c) in captures.into_iter().enumerate() {
        unsafe { func::set_capture(&f, i, c) }
    }
    f
}

fn cap(this: VRef, i: usize) -> Value {
    func::capture(&this, i).clone()
}

fn all_args(args: &[VRef]) -> Vec<Value> {
    args.iter().map(|a| a.owned()).collect()
}

static PARTIAL: OnceLock<usize> = OnceLock::new();
static COMP: OnceLock<usize> = OnceLock::new();
static COMPLEMENT: OnceLock<usize> = OnceLock::new();
static CONSTANTLY: OnceLock<usize> = OnceLock::new();
static JUXT: OnceLock<usize> = OnceLock::new();
static FNIL: OnceLock<usize> = OnceLock::new();
static EVERY_PRED: OnceLock<usize> = OnceLock::new();
static SOME_FN: OnceLock<usize> = OnceLock::new();

native_fns! {
    "partial" => @gc fn nf_partial(args, this) [; 0] {
        let f = cap(this, 0);
        let mut v = coll::to_vec(&cap(this, 1))?;
        v.extend(all_args(args));
        f.call(&v)
    }
    "comp" => @gc fn nf_comp(args, this) [; 0] {
        let fs = coll::to_vec(&cap(this, 0))?;
        let mut it = fs.iter().rev();
        let mut acc = match it.next() { Some(f) => f.call(&all_args(args))?, None => return Ok(args.first().map_or(Value::NIL, |a| a.owned())) };
        for f in it { acc = f.call(std::slice::from_ref(&acc))?; }
        Ok(acc)
    }
    "complement" => @gc fn nf_complement(args, this) [; 0] {
        Ok(Value::bool(!cap(this, 0).call(&all_args(args))?.truthy()))
    }
    "constantly" => fn nf_constantly(args, this) [; 0] { let _ = args; Ok(cap(this, 0)) }
    "juxt" => @gc fn nf_juxt(args, this) [; 0] {
        let fs = coll::to_vec(&cap(this, 0))?;
        let a = all_args(args);
        // The results so far are rooted while the remaining fns run.
        let root = crate::gc::ArgRoot::empty();
        let _rooted = root.enter();
        let mut out = Vec::with_capacity(fs.len());
        for f in &fs {
            let r = f.call(&a)?;
            out.push(r);
            root.set(&out);
        }
        Ok(vector::from_iter(out))
    }
    "fnil" => @gc fn nf_fnil(args, this) [; 1] {
        let f = cap(this, 0);
        let defaults = coll::to_vec(&cap(this, 1))?;
        let mut a = all_args(args);
        for (i, d) in defaults.iter().enumerate() { if i < a.len() && a[i].is_nil() { a[i] = d.clone(); } }
        f.call(&a)
    }
    "every-pred" => @gc fn nf_every_pred(args, this) [; 1] {
        for p in coll::to_vec(&cap(this, 0))? { for a in args { if !p.call(&[a.owned()])?.truthy() { return Ok(Value::FALSE); } } }
        Ok(Value::TRUE)
    }
    "some-fn" => @gc fn nf_some_fn(args, this) [; 1] {
        for p in coll::to_vec(&cap(this, 0))? { for a in args { let r = p.call(&[a.owned()])?; if r.truthy() { return Ok(r); } } }
        Ok(Value::NIL)
    }
}

builtins! {
    "partial" => fn cf_partial(args) [; 1] {
        let p = proto(&PARTIAL, nf_partial, "clojure.core/partial", 0, 0);
        Ok(closure(p, vec![args[0].owned(), vector::from_iter(all_args(&args[1..]))]))
    }
    "comp" => fn cf_comp(args) [; 0] {
        let p = proto(&COMP, nf_comp, "clojure.core/comp", 0, 0);
        Ok(closure(p, vec![vector::from_iter(all_args(args))]))
    }
    "complement" => fn cf_complement(args) [1] {
        let p = proto(&COMPLEMENT, nf_complement, "clojure.core/complement", 0, 0);
        Ok(closure(p, vec![args[0].owned()]))
    }
    "constantly" => fn cf_constantly(args) [1] {
        let p = proto(&CONSTANTLY, nf_constantly, "clojure.core/constantly", 0, 0);
        Ok(closure(p, vec![args[0].owned()]))
    }
    "juxt" => fn cf_juxt(args) [; 1] {
        let p = proto(&JUXT, nf_juxt, "clojure.core/juxt", 0, 0);
        Ok(closure(p, vec![vector::from_iter(all_args(args))]))
    }
    "fnil" => fn cf_fnil(args) [; 2] {
        let p = proto(&FNIL, nf_fnil, "clojure.core/fnil", 0, 1);
        Ok(closure(p, vec![args[0].owned(), vector::from_iter(all_args(&args[1..]))]))
    }
    "every-pred" => fn cf_every_pred(args) [; 1] {
        let p = proto(&EVERY_PRED, nf_every_pred, "clojure.core/every-pred", 0, 1);
        Ok(closure(p, vec![vector::from_iter(all_args(args))]))
    }
    "some-fn" => fn cf_some_fn(args) [; 1] {
        let p = proto(&SOME_FN, nf_some_fn, "clojure.core/some-fn", 0, 1);
        Ok(closure(p, vec![vector::from_iter(all_args(args))]))
    }
    "fn-name" => fn cf_fn_name(args) [1] {
        Ok(func::as_fn(&args[0]).map_or(Value::NIL, |f| crate::obj::string::new(&f.proto().name)))
    }
    "list*" => fn cf_list_star(args) [; 1] {
        let mut v: Vec<Value> = all_args(&args[..args.len() - 1]);
        v.extend(coll::to_vec(&args[args.len() - 1])?);
        Ok(if v.is_empty() { Value::NIL } else { list::from_iter(v) })
    }
}

#[allow(dead_code)]
fn _result_type() -> RtResult<Value> {
    Ok(Value::NIL)
}
