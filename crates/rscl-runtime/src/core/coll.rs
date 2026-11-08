//! Collection builtins over `crate::coll`.
use std::cmp::Ordering;

use crate::builtins;
use crate::coll;
use crate::core::args;
use crate::exc::{RtResult, err};
use crate::obj::exception::{CLASS_CAST, ILLEGAL_ARGUMENT};
use crate::obj::{Kind, list, map, set, vector};
use crate::value::{VRef, Value};

fn owned_vec(args: &[VRef]) -> Vec<Value> {
    args.iter().map(|a| a.owned()).collect()
}

/// Call `f` with `args` and propagate a thrown exception.
pub fn call(f: &Value, args: &[Value]) -> RtResult<Value> {
    f.call(args)
}

builtins! {
    "list" => fn cf_list(args) [; 0] { Ok(list::from_iter(owned_vec(args))) }
    "vector" => fn cf_vector(args) [; 0] { Ok(vector::from_iter(owned_vec(args))) }
    "vec" => fn cf_vec(args) [1] { Ok(vector::from_iter(coll::to_vec(&args[0])?)) }
    "set" => fn cf_set(args) [1] {
        let mut s = set::empty_set();
        for x in coll::to_vec(&args[0])? { s.insert(x); }
        Ok(set::new(s))
    }
    "hash-map" => fn cf_hash_map(args) [; 0] {
        if !args.len().is_multiple_of(2) { return err(ILLEGAL_ARGUMENT, "hash-map requires an even number of arguments"); }
        let mut m = map::empty_map();
        for kv in args.chunks(2) { m.insert(kv[0].owned(), kv[1].owned()); }
        Ok(map::new(m))
    }
    "hash-set" => fn cf_hash_set(args) [; 0] {
        let mut s = set::empty_set();
        for x in args { s.insert(x.owned()); }
        Ok(set::new(s))
    }
    "count" => fn cf_count(args) [1] { Ok(Value::int(coll::count(&args[0])?)) }
    "seq" => fn cf_seq(args) [1] { coll::seq(&args[0]) }
    "first" => fn cf_first(args) [1] { coll::first(&args[0]) }
    "second" => fn cf_second(args) [1] { coll::second(&args[0]) }
    "rest" => fn cf_rest(args) [1] { coll::rest(&args[0]) }
    "next" => fn cf_next(args) [1] { coll::next(&args[0]) }
    "last" => fn cf_last(args) [1] { coll::last(&args[0]) }
    "butlast" => fn cf_butlast(args) [1] {
        let mut v = coll::to_vec(&args[0])?;
        v.pop();
        Ok(if v.is_empty() { Value::NIL } else { list::from_iter(v) })
    }
    "cons" => fn cf_cons(args) [2] { coll::cons(args[0].owned(), &args[1]) }
    "conj" => fn cf_conj(args) [; 1] {
        let mut c = args[0].owned();
        for x in &args[1..] { c = coll::conj(&c, x.owned())?; }
        Ok(c)
    }
    "get" => fn cf_get(args) [2, 3] { coll::get(&args[0], &args[1], args.get(2).map(|v| &**v)) }
    "get-in" => fn cf_get_in(args) [2, 3] {
        let mut cur = args[0].owned();
        for k in coll::to_vec(&args[1])? {
            // UNBOUND is never a real value, so it marks "key absent".
            let next = coll::get(&cur, &k, Some(&Value::UNBOUND))?;
            if next.is_unbound() { return Ok(args.get(2).map_or(Value::NIL, |v| v.owned())); }
            cur = next;
        }
        Ok(cur)
    }
    "nth" => fn cf_nth(args) [2, 3] { coll::nth(&args[0], args::int(&args[1], "index")?, args.get(2).map(|v| &**v)) }
    "assoc" => fn cf_assoc(args) [; 3] {
        if !(args.len() - 1).is_multiple_of(2) { return err(ILLEGAL_ARGUMENT, "assoc expects even number of arguments after map/vector"); }
        let mut c = args[0].owned();
        for kv in args[1..].chunks(2) { c = coll::assoc(&c, kv[0].owned(), kv[1].owned())?; }
        Ok(c)
    }
    "assoc-in" => fn cf_assoc_in(args) [3] { assoc_in(&args[0], &coll::to_vec(&args[1])?, args[2].owned()) }
    "update" => fn cf_update(args) [; 3] {
        let old = coll::get(&args[0], &args[1], None)?;
        let mut fargs = vec![old];
        fargs.extend(args[3..].iter().map(|a| a.owned()));
        let new = call(&args[2], &fargs)?;
        coll::assoc(&args[0], args[1].owned(), new)
    }
    "update-in" => fn cf_update_in(args) [; 3] {
        let path = coll::to_vec(&args[1])?;
        let old = { let mut cur = args[0].owned(); for k in &path { cur = coll::get(&cur, k, None)?; } cur };
        let mut fargs = vec![old];
        fargs.extend(args[3..].iter().map(|a| a.owned()));
        let new = call(&args[2], &fargs)?;
        assoc_in(&args[0], &path, new)
    }
    "dissoc" => fn cf_dissoc(args) [; 1] {
        let mut c = args[0].owned();
        for k in &args[1..] { c = coll::dissoc(&c, k)?; }
        Ok(c)
    }
    "disj" => fn cf_disj(args) [; 1] {
        let mut c = args[0].owned();
        for k in &args[1..] { c = coll::disj(&c, k)?; }
        Ok(c)
    }
    "contains?" => fn cf_contains_p(args) [2] { Ok(Value::bool(coll::contains(&args[0], &args[1])?)) }
    "find" => fn cf_find(args) [2] { coll::find(&args[0], &args[1]) }
    "keys" => fn cf_keys(args) [1] { coll::keys(&args[0]) }
    "vals" => fn cf_vals(args) [1] { coll::vals(&args[0]) }
    "key" => fn cf_key(args) [1] { coll::nth(&args[0], 0, None) }
    "val" => fn cf_val(args) [1] { coll::nth(&args[0], 1, None) }
    "peek" => fn cf_peek(args) [1] { coll::peek(&args[0]) }
    "pop" => fn cf_pop(args) [1] { coll::pop(&args[0]) }
    "empty" => fn cf_empty(args) [1] { Ok(coll::empty(&args[0])) }
    "empty?" => fn cf_empty_p(args) [1] { Ok(Value::bool(coll::seq(&args[0])?.is_nil())) }
    "not-empty" => fn cf_not_empty(args) [1] { Ok(if coll::seq(&args[0])?.is_nil() { Value::NIL } else { args[0].owned() }) }
    "reverse" => fn cf_reverse(args) [1] {
        let mut v = coll::to_vec(&args[0])?;
        v.reverse();
        Ok(list::from_iter(v))
    }
    "into" => fn cf_into(args) [1, 2] {
        if args.len() == 1 { return Ok(args[0].owned()); }
        let mut c = args[0].owned();
        for x in coll::to_vec(&args[1])? { c = coll::conj(&c, x)?; }
        Ok(c)
    }
    "concat" => fn cf_concat(args) [; 0] {
        let mut out = Vec::new();
        for a in args { out.extend(coll::to_vec(a)?); }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "range" => fn cf_range(args) [1, 2, 3] {
        let (start, end, step) = match args.len() {
            1 => (0, args::int(&args[0], "end")?, 1),
            2 => (args::int(&args[0], "start")?, args::int(&args[1], "end")?, 1),
            _ => (args::int(&args[0], "start")?, args::int(&args[1], "end")?, args::int(&args[2], "step")?),
        };
        if step == 0 { return err(ILLEGAL_ARGUMENT, "range step must not be zero"); }
        let mut out = Vec::new();
        let mut i = start;
        while (step > 0 && i < end) || (step < 0 && i > end) {
            out.push(Value::int(i));
            i += step;
            if out.len() > 100_000_000 { return err(ILLEGAL_ARGUMENT, "range too large"); }
        }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "take" => fn cf_take(args) [2] {
        let n = args::int(&args[0], "n")?.max(0) as usize;
        let v: Vec<Value> = coll::to_vec(&args[1])?.into_iter().take(n).collect();
        Ok(if v.is_empty() { Value::NIL } else { list::from_iter(v) })
    }
    "drop" => fn cf_drop(args) [2] {
        let n = args::int(&args[0], "n")?.max(0) as usize;
        let v: Vec<Value> = coll::to_vec(&args[1])?.into_iter().skip(n).collect();
        Ok(if v.is_empty() { Value::NIL } else { list::from_iter(v) })
    }
    "take-while" => fn cf_take_while(args) [2] {
        let mut out = Vec::new();
        for x in coll::to_vec(&args[1])? { if call(&args[0], std::slice::from_ref(&x))?.truthy() { out.push(x) } else { break } }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "drop-while" => fn cf_drop_while(args) [2] {
        let mut out = Vec::new();
        let mut dropping = true;
        for x in coll::to_vec(&args[1])? {
            if dropping && call(&args[0], std::slice::from_ref(&x))?.truthy() { continue; }
            dropping = false;
            out.push(x);
        }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "map" => fn cf_map(args) [; 2] {
        let colls: Vec<Vec<Value>> = args[1..].iter().map(|c| coll::to_vec(c)).collect::<RtResult<_>>()?;
        let n = colls.iter().map(Vec::len).min().unwrap_or(0);
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let fargs: Vec<Value> = colls.iter().map(|c| c[i].clone()).collect();
            out.push(call(&args[0], &fargs)?);
        }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "mapcat" => fn cf_mapcat(args) [2] {
        let mut out = Vec::new();
        for x in coll::to_vec(&args[1])? { out.extend(coll::to_vec(&call(&args[0], std::slice::from_ref(&x))?)?); }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "map-indexed" => fn cf_map_indexed(args) [2] {
        let mut out = Vec::new();
        for (i, x) in coll::to_vec(&args[1])?.into_iter().enumerate() { out.push(call(&args[0], &[Value::int(i as i64), x])?); }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "filter" => fn cf_filter(args) [2] {
        let mut out = Vec::new();
        for x in coll::to_vec(&args[1])? { if call(&args[0], std::slice::from_ref(&x))?.truthy() { out.push(x) } }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "remove" => fn cf_remove(args) [2] {
        let mut out = Vec::new();
        for x in coll::to_vec(&args[1])? { if !call(&args[0], std::slice::from_ref(&x))?.truthy() { out.push(x) } }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "keep" => fn cf_keep(args) [2] {
        let mut out = Vec::new();
        for x in coll::to_vec(&args[1])? { let r = call(&args[0], std::slice::from_ref(&x))?; if !r.is_nil() { out.push(r) } }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "reduce" => fn cf_reduce(args) [2, 3] {
        let items = coll::to_vec(&args[args.len() - 1])?;
        let mut it = items.into_iter();
        let mut acc = if args.len() == 3 { args[1].owned() } else {
            match it.next() { Some(x) => x, None => return call(&args[0], &[]) }
        };
        for x in it {
            acc = call(&args[0], &[acc, x])?;
        }
        Ok(acc)
    }
    "reduce-kv" => fn cf_reduce_kv(args) [3] {
        let mut acc = args[1].owned();
        let Some(m) = map::as_map(&args[2]) else { return err(CLASS_CAST, "reduce-kv requires a map") };
        let entries: Vec<(Value, Value)> = m.m.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (k, v) in entries { acc = call(&args[0], &[acc, k, v])?; }
        Ok(acc)
    }
    "some" => fn cf_some(args) [2] {
        for x in coll::to_vec(&args[1])? { let r = call(&args[0], std::slice::from_ref(&x))?; if r.truthy() { return Ok(r) } }
        Ok(Value::NIL)
    }
    "every?" => fn cf_every_p(args) [2] {
        for x in coll::to_vec(&args[1])? { if !call(&args[0], std::slice::from_ref(&x))?.truthy() { return Ok(Value::FALSE) } }
        Ok(Value::TRUE)
    }
    "not-any?" => fn cf_not_any_p(args) [2] {
        for x in coll::to_vec(&args[1])? { if call(&args[0], std::slice::from_ref(&x))?.truthy() { return Ok(Value::FALSE) } }
        Ok(Value::TRUE)
    }
    "repeat" => fn cf_repeat(args) [2] {
        let n = args::int(&args[0], "n")?.max(0) as usize;
        Ok(if n == 0 { Value::NIL } else { list::from_iter(std::iter::repeat_n(args[1].owned(), n).collect::<Vec<_>>()) })
    }
    "repeatedly" => fn cf_repeatedly(args) [2] {
        let n = args::int(&args[0], "n")?.max(0) as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n { out.push(call(&args[1], &[])?); }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "iterate" => fn cf_iterate(args) [3] {
        // Eager, bounded variant: (iterate f x n) — the lazy version arrives with lazy seqs.
        let n = args::int(&args[2], "n")?.max(0) as usize;
        let mut out = Vec::with_capacity(n);
        let mut cur = args[1].owned();
        for _ in 0..n { out.push(cur.clone()); cur = call(&args[0], std::slice::from_ref(&cur))?; }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "interleave" => fn cf_interleave(args) [; 0] {
        let colls: Vec<Vec<Value>> = args.iter().map(|c| coll::to_vec(c)).collect::<RtResult<_>>()?;
        let n = colls.iter().map(Vec::len).min().unwrap_or(0);
        let mut out = Vec::new();
        for i in 0..n { for c in &colls { out.push(c[i].clone()); } }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "interpose" => fn cf_interpose(args) [2] {
        let mut out = Vec::new();
        for (i, x) in coll::to_vec(&args[1])?.into_iter().enumerate() { if i > 0 { out.push(args[0].owned()); } out.push(x); }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "partition" => fn cf_partition(args) [2, 3] {
        let n = args::int(&args[0], "n")?;
        if n <= 0 { return err(ILLEGAL_ARGUMENT, "partition size must be positive"); }
        let (step, coll_v) = if args.len() == 3 { (args::int(&args[1], "step")?, &args[2]) } else { (n, &args[1]) };
        if step <= 0 { return err(ILLEGAL_ARGUMENT, "partition step must be positive"); }
        let items = coll::to_vec(coll_v)?;
        let mut out = Vec::new();
        let mut i = 0usize;
        while i + (n as usize) <= items.len() {
            out.push(list::from_slice(&items[i..i + n as usize]));
            i += step as usize;
        }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "partition-all" => fn cf_partition_all(args) [2] {
        let n = args::int(&args[0], "n")?;
        if n <= 0 { return err(ILLEGAL_ARGUMENT, "partition size must be positive"); }
        let items = coll::to_vec(&args[1])?;
        let out: Vec<Value> = items.chunks(n as usize).map(list::from_slice).collect();
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "distinct" => fn cf_distinct(args) [1] {
        let mut seen = set::empty_set();
        let mut out = Vec::new();
        for x in coll::to_vec(&args[0])? { if !seen.contains(&x) { seen.insert(x.clone()); out.push(x); } }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "frequencies" => fn cf_frequencies(args) [1] {
        let mut m = map::empty_map();
        for x in coll::to_vec(&args[0])? {
            let n = m.get(&x).and_then(|v| v.as_int()).unwrap_or(0);
            m.insert(x, Value::int(n + 1));
        }
        Ok(map::new(m))
    }
    "group-by" => fn cf_group_by(args) [2] {
        let mut m = map::empty_map();
        for x in coll::to_vec(&args[1])? {
            let k = call(&args[0], std::slice::from_ref(&x))?;
            let cur = m.get(&k).cloned().unwrap_or_else(|| vector::new(vector::Vec_::new()));
            m.insert(k, coll::conj(&cur, x)?);
        }
        Ok(map::new(m))
    }
    "merge" => fn cf_merge(args) [; 0] {
        let mut acc = Value::NIL;
        for a in args {
            if a.is_nil() { continue; }
            if acc.is_nil() { acc = a.owned(); continue; }
            acc = coll::conj(&acc, a.owned())?;
        }
        Ok(acc)
    }
    "zipmap" => fn cf_zipmap(args) [2] {
        let ks = coll::to_vec(&args[0])?;
        let vs = coll::to_vec(&args[1])?;
        let mut m = map::empty_map();
        for (k, v) in ks.into_iter().zip(vs) { m.insert(k, v); }
        Ok(map::new(m))
    }
    "compare" => fn cf_compare(args) [2] {
        Ok(Value::int(match coll::compare(&args[0], &args[1])? { Ordering::Less => -1, Ordering::Equal => 0, Ordering::Greater => 1 }))
    }
    "sort" => fn cf_sort(args) [1, 2] {
        let mut items = coll::to_vec(&args[args.len() - 1])?;
        let mut failure: Option<crate::exc::Exc> = None;
        if args.len() == 2 {
            items.sort_by(|a, b| {
                if failure.is_some() { return Ordering::Equal; }
                compare_with(&args[0], a, b).unwrap_or_else(|e| { failure = Some(e); Ordering::Equal })
            });
        } else {
            items.sort_by(|a, b| {
                if failure.is_some() { return Ordering::Equal; }
                coll::compare(a, b).unwrap_or_else(|e| { failure = Some(e); Ordering::Equal })
            });
        }
        if let Some(e) = failure { return Err(e); }
        Ok(if items.is_empty() { Value::NIL } else { list::from_iter(items) })
    }
    "sort-by" => fn cf_sort_by(args) [2, 3] {
        let items = coll::to_vec(&args[args.len() - 1])?;
        let mut keyed: Vec<(Value, Value)> = Vec::with_capacity(items.len());
        for x in items { keyed.push((call(&args[0], std::slice::from_ref(&x))?, x)); }
        let mut failure: Option<crate::exc::Exc> = None;
        keyed.sort_by(|(ka, _), (kb, _)| {
            if failure.is_some() { return Ordering::Equal; }
            if args.len() == 3 {
                compare_with(&args[1], ka, kb).unwrap_or_else(|e| { failure = Some(e); Ordering::Equal })
            } else {
                coll::compare(ka, kb).unwrap_or_else(|e| { failure = Some(e); Ordering::Equal })
            }
        });
        if let Some(e) = failure { return Err(e); }
        Ok(if keyed.is_empty() { Value::NIL } else { list::from_iter(keyed.into_iter().map(|(_, x)| x).collect::<Vec<_>>()) })
    }
    "apply" => fn cf_apply(args) [; 2] {
        let mut fargs: Vec<Value> = args[1..args.len() - 1].iter().map(|a| a.owned()).collect();
        fargs.extend(coll::to_vec(&args[args.len() - 1])?);
        call(&args[0], &fargs)
    }
    "list?" => fn cf_list_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::List))) }
    "vector?" => fn cf_vector_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::Vector))) }
    "map?" => fn cf_map_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::Map))) }
    "set?" => fn cf_set_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::Set))) }
    "coll?" => fn cf_coll_p(args) [1] { Ok(Value::bool(matches!(args[0].kind(), Some(Kind::List | Kind::Vector | Kind::Map | Kind::Set)))) }
    "seq?" => fn cf_seq_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::List))) }
    "sequential?" => fn cf_sequential_p(args) [1] { Ok(Value::bool(matches!(args[0].kind(), Some(Kind::List | Kind::Vector)))) }
    "associative?" => fn cf_associative_p(args) [1] { Ok(Value::bool(matches!(args[0].kind(), Some(Kind::Map | Kind::Vector)))) }
    "seqable?" => fn cf_seqable_p(args) [1] { Ok(Value::bool(coll::is_seqable(&args[0]))) }
    "ifn?" => fn cf_ifn_p(args) [1] { Ok(Value::bool(matches!(args[0].kind(), Some(Kind::Fn | Kind::Keyword | Kind::Map | Kind::Set | Kind::Vector | Kind::Var)))) }
    "hash" => fn cf_hash(args) [1] { Ok(Value::int((crate::equiv::hasheq(&args[0]) as i64) & 0x7fff_ffff)) }
}

/// Order two values with a user comparator: an integer result is a signum, a boolean
/// result is a predicate (`(pred a b)` → less, `(pred b a)` → greater, else equal).
fn compare_with(f: &Value, a: &Value, b: &Value) -> RtResult<Ordering> {
    let r = call(f, &[a.clone(), b.clone()])?;
    if let Some(n) = r.as_int() {
        return Ok(n.cmp(&0));
    }
    if r.truthy() {
        return Ok(Ordering::Less);
    }
    let back = call(f, &[b.clone(), a.clone()])?;
    Ok(if back.truthy() {
        Ordering::Greater
    } else {
        Ordering::Equal
    })
}

fn assoc_in(m: &Value, path: &[Value], v: Value) -> RtResult<Value> {
    match path {
        [] => Ok(v),
        [k] => coll::assoc(m, k.clone(), v),
        [k, rest @ ..] => {
            let inner = coll::get(m, k, None)?;
            let new_inner = assoc_in(&inner, rest, v)?;
            coll::assoc(m, k.clone(), new_inner)
        }
    }
}
