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
    "count" => fn cf_count(args; owned = taker) [1] { Ok(Value::int(coll::count_owned(taker.take(0))?)) }
    "seq" => fn cf_seq(args) [1] { coll::seq(&args[0]) }
    "first" => fn cf_first(args) [1] { coll::first(&args[0]) }
    "second" => fn cf_second(args) [1] { coll::second(&args[0]) }
    "rest" => fn cf_rest(args) [1] { coll::rest(&args[0]) }
    "next" => fn cf_next(args) [1] { coll::next(&args[0]) }
    "last" => fn cf_last(args; owned = taker) [1] { coll::last_owned(taker.take(0)) }
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
    "nth" => fn cf_nth(args; owned = taker) [2, 3] {
        let i = args::int(&args[1], "index")?;
        coll::nth_owned(taker.take(0), i, args.get(2).map(|v| &**v))
    }
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
    "into" => fn cf_into(args; owned = taker) [1, 2] {
        if args.len() == 1 { return Ok(args[0].owned()); }
        let mut c = args[0].owned();
        for x in coll::iter_owned(taker.take(1))? { c = coll::conj(&c, x?)?; }
        Ok(c)
    }
    "concat" => fn cf_concat(args) [; 0] {
        let mut out = Vec::new();
        for a in args { out.extend(coll::to_vec(a)?); }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "reduce" => fn cf_reduce(args; owned = taker) [2, 3] {
        // A lazy sequence is taken over and walked cell by cell; a collection is
        // materialised once.
        let mut it = coll::iter_owned(taker.take(args.len() - 1))?;
        let mut acc = if args.len() == 3 { args[1].owned() } else {
            match it.next() { Some(x) => x?, None => return call(&args[0], &[]) }
        };
        for x in it {
            acc = call(&args[0], &[acc, x?])?;
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
    "frequencies" => fn cf_frequencies(args; owned = taker) [1] {
        let mut m = map::empty_map();
        for x in coll::iter_owned(taker.take(0))? {
            let x = x?;
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
    "map?" => fn cf_map_p(args) [1] { Ok(Value::bool(coll::is_map_like(&args[0]))) }
    "set?" => fn cf_set_p(args) [1] { Ok(Value::bool(matches!(args[0].kind(), Some(Kind::Set | Kind::SortedSet)))) }
    "coll?" => fn cf_coll_p(args) [1] { Ok(Value::bool(matches!(args[0].kind(), Some(Kind::List | Kind::Vector | Kind::Map | Kind::Set | Kind::Cons | Kind::LazySeq | Kind::SortedMap | Kind::SortedSet)) || crate::obj::types::is_record(&args[0]))) }
    "seq?" => fn cf_seq_p(args) [1] { Ok(Value::bool(coll::is_seq(&args[0]))) }
    "sequential?" => fn cf_sequential_p(args) [1] { Ok(Value::bool(matches!(args[0].kind(), Some(Kind::List | Kind::Vector | Kind::Cons | Kind::LazySeq)))) }
    "associative?" => fn cf_associative_p(args) [1] { Ok(Value::bool(matches!(args[0].kind(), Some(Kind::Map | Kind::Vector | Kind::SortedMap)) || crate::obj::types::is_record(&args[0]))) }
    "seqable?" => fn cf_seqable_p(args) [1] { Ok(Value::bool(coll::is_seqable(&args[0]))) }
    "ifn?" => fn cf_ifn_p(args) [1] { Ok(Value::bool(matches!(args[0].kind(), Some(Kind::Fn | Kind::Keyword | Kind::Map | Kind::Set | Kind::Vector | Kind::Var | Kind::SortedMap | Kind::SortedSet)))) }
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
