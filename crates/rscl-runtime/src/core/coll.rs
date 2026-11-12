//! Collection builtins over `crate::coll`.
use std::cmp::Ordering;

use crate::builtins;
use crate::coll;
use crate::core::args;
use crate::exc::{RtResult, err};
use crate::obj::exception::{CLASS_CAST, ILLEGAL_ARGUMENT};
use crate::obj::{Kind, list, map, set, vector};
use crate::value::Value;

builtins! {
    "list" => fn cf_list(args) [; 0] { Ok(list::from_slice(args)) }
    "vector" => fn cf_vector(args) [; 0] { Ok(vector::from_slice(args)) }
    "vec" => @gc fn cf_vec(args) [1] { Ok(vector::from_iter(coll::to_vec(&args[0])?)) }
    "set" => @gc fn cf_set(args) [1] {
        // Rooted: hashing an element may run rscl code (a lazy seq in it), and the
        // set under construction is Rust data the collector does not see.
        let items = coll::to_vec(&args[0])?;
        let _root = crate::gc::root_slice(&items);
        let mut s = set::empty_set();
        // (An element equal to one already there is not added.)
        for x in &items { s.insert(*x); }
        Ok(set::new(s))
    }
    "hash-map" => fn cf_hash_map(args) [; 0] {
        if !args.len().is_multiple_of(2) { return err(ILLEGAL_ARGUMENT, "hash-map requires an even number of arguments"); }
        let mut m = map::empty_map();
        for kv in args.chunks(2) { map::put(&mut m, kv[0], kv[1]); }
        Ok(map::new(m))
    }
    "hash-set" => fn cf_hash_set(args) [; 0] {
        let mut s = set::empty_set();
        for x in args { s.insert(*x); }
        Ok(set::new(s))
    }
    "count" => @gc fn cf_count(args; slots = slots) [1] { Ok(Value::int(coll::count_at(slots.cursor(0))?)) }
    "seq" => @gc fn cf_seq(args) [1] { coll::seq(&args[0]) }
    "first" => @gc fn cf_first(args) [1] { coll::first(&args[0]) }
    "second" => @gc fn cf_second(args) [1] { coll::second(&args[0]) }
    "rest" => @gc fn cf_rest(args) [1] { coll::rest(&args[0]) }
    "next" => @gc fn cf_next(args) [1] { coll::next(&args[0]) }
    "last" => @gc fn cf_last(args; slots = slots) [1] { coll::last_at(slots.cursor(0)) }
    "butlast" => @gc fn cf_butlast(args) [1] {
        let mut v = coll::to_vec(&args[0])?;
        v.pop();
        Ok(if v.is_empty() { Value::NIL } else { list::from_iter(v) })
    }
    "cons" => fn cf_cons(args) [2] { coll::cons(args[0], &args[1]) }
    "conj" => fn cf_conj(args) [; 1] {
        let mut c = args[0];
        for x in &args[1..] { c = coll::conj(&c, *x)?; }
        Ok(c)
    }
    "get" => fn cf_get(args) [2, 3] { coll::get(&args[0], &args[1], args.get(2)) }
    "get-in" => fn cf_get_in(args) [2, 3] {
        let mut cur = args[0];
        for k in coll::to_vec(&args[1])? {
            // UNBOUND is never a real value, so it marks "key absent".
            let next = coll::get(&cur, &k, Some(&Value::UNBOUND))?;
            if next.is_unbound() { return Ok(args.get(2).map_or(Value::NIL, |v| *v)); }
            cur = next;
        }
        Ok(cur)
    }
    "nth" => @gc fn cf_nth(args; slots = slots) [2, 3] {
        let i = args::int(&args[1], "index")?;
        coll::nth_at(slots.cursor(0), i, args.get(2))
    }
    "assoc" => fn cf_assoc(args) [; 3] {
        if !(args.len() - 1).is_multiple_of(2) { return err(ILLEGAL_ARGUMENT, "assoc expects even number of arguments after map/vector"); }
        let mut c = args[0];
        for kv in args[1..].chunks(2) { c = coll::assoc(&c, kv[0], kv[1])?; }
        Ok(c)
    }
    "assoc-in" => fn cf_assoc_in(args) [3] { assoc_in(&args[0], &coll::to_vec(&args[1])?, args[2]) }
    "update" => @gc fn cf_update(args) [; 3] {
        let old = coll::get(&args[0], &args[1], None)?;
        let mut fargs = vec![old];
        fargs.extend(args[3..].iter().copied());
        let new = args[2].call(&fargs)?;
        coll::assoc(&args[0], args[1], new)
    }
    "update-in" => @gc fn cf_update_in(args) [; 3] {
        let path = coll::to_vec(&args[1])?;
        let old = { let mut cur = args[0]; for k in &path { cur = coll::get(&cur, k, None)?; } cur };
        let mut fargs = vec![old];
        fargs.extend(args[3..].iter().copied());
        let new = args[2].call(&fargs)?;
        assoc_in(&args[0], &path, new)
    }
    "dissoc" => fn cf_dissoc(args) [; 1] {
        let mut c = args[0];
        for k in &args[1..] { c = coll::dissoc(&c, k)?; }
        Ok(c)
    }
    "disj" => fn cf_disj(args) [; 1] {
        let mut c = args[0];
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
    "empty?" => @gc fn cf_empty_p(args) [1] { Ok(Value::bool(coll::seq(&args[0])?.is_nil())) }
    "not-empty" => @gc fn cf_not_empty(args) [1] { Ok(if coll::seq(&args[0])?.is_nil() { Value::NIL } else { args[0] }) }
    "reverse" => @gc fn cf_reverse(args) [1] {
        let mut v = coll::to_vec(&args[0])?;
        v.reverse();
        Ok(list::from_iter(v))
    }
    "into" => @gc fn cf_into(args; slots = slots) [1, 2] {
        if args.len() == 1 { return Ok(args[0]); }
        if let Some(target) = vector::as_vec(&args[0]) {
            // One new vector, not one per element. The elements gathered so far
            // are rooted while the source is realised.
            let root = crate::gc::ArgRoot::empty();
            let _rooted = root.enter();
            let mut items = Vec::new();
            for x in coll::iter_at(slots.cursor(1))? {
                items.push(x?);
                root.set(&items);
            }
            return Ok(target.extend(&items));
        }
        let mut c = args[0];
        for x in coll::iter_at(slots.cursor(1))? { c = coll::conj(&c, x?)?; }
        Ok(c)
    }
    "concat" => @gc fn cf_concat(args) [; 0] {
        // What has been gathered is rooted while a later argument is realised (the
        // entries of a map argument exist nowhere else).
        let root = crate::gc::ArgRoot::empty();
        let _rooted = root.enter();
        let mut out = Vec::new();
        for a in args {
            let items = coll::to_vec(a)?;
            out.extend(items);
            root.set(&out);
        }
        Ok(if out.is_empty() { Value::NIL } else { list::from_iter(out) })
    }
    "reduce" => @gc fn cf_reduce(args; slots = slots) [2, 3] {
        // A lazy sequence is taken over and walked cell by cell; a collection is
        // materialised once.
        // One loop with one call site of `next`: a separate "first element" step
        // would leave its copy of the first cell in this frame for the whole walk,
        // and with it every cell of a lazy sequence (see `gc::clear_dead_stack`).
        let mut acc = if args.len() == 3 { Some(args[1]) } else { None };
        for x in coll::iter_at(slots.cursor(args.len() - 1))? {
            let x = x?;
            acc = Some(match acc {
                Some(a) => args[0].call(&[a, x])?,
                None => x,
            });
        }
        match acc {
            Some(a) => Ok(a),
            None => args[0].call(&[]),
        }
    }
    // (The entries are the map's own keys and values: reachable through the map.)
    "reduce-kv" => @gc fn cf_reduce_kv(args) [3] {
        let mut acc = args[1];
        let Some(m) = map::as_map(&args[2]) else { return err(CLASS_CAST, "reduce-kv requires a map") };
        let entries: Vec<(Value, Value)> = m.m.iter().map(|(k, v)| (*k, *v)).collect();
        for (k, v) in entries { acc = args[0].call(&[acc, k, v])?; }
        Ok(acc)
    }
    "frequencies" => @gc fn cf_frequencies(args; slots = slots) [1] {
        // The sequence is walked through its argument slot, so the elements behind
        // the cursor live only in the map being built, which is Rust data: the
        // distinct ones are kept in a rooted `Vec` beside it.
        let root = crate::gc::ArgRoot::empty();
        let _rooted = root.enter();
        let mut keys: Vec<Value> = Vec::new();
        let mut m = map::empty_map();
        for x in coll::iter_at(slots.cursor(0))? {
            let x = x?;
            let n = match m.get(&x).and_then(|v| v.as_int()) {
                Some(n) => n,
                None => {
                    keys.push(x);
                    root.set(&keys);
                    0
                }
            };
            map::put(&mut m, x, Value::int(n + 1));
        }
        Ok(map::new(m))
    }
    "group-by" => @gc fn cf_group_by(args) [2] {
        // First the keys, with the items and the keys so far rooted while `f` runs;
        // then the map, with collections held off (its groups are fresh vectors that
        // only the map under construction refers to, and hashing a key may run rscl
        // code).
        let items = coll::to_vec(&args[1])?;
        let _items = crate::gc::root_slice(&items);
        let root = crate::gc::ArgRoot::empty();
        let _rooted = root.enter();
        let mut keys: Vec<Value> = Vec::with_capacity(items.len());
        for x in &items {
            let k = args[0].call(std::slice::from_ref(x))?;
            keys.push(k);
            root.set(&keys);
        }
        let _hold = crate::gc::hold();
        let mut m = map::empty_map();
        for (k, x) in keys.iter().zip(&items) {
            let cur = m.get(k).cloned().unwrap_or_else(vector::empty);
            let v = coll::conj(&cur, *x)?;
            map::put(&mut m, *k, v);
        }
        Ok(map::new(m))
    }
    "merge" => fn cf_merge(args) [; 0] {
        let mut acc = Value::NIL;
        for a in args {
            if a.is_nil() { continue; }
            if acc.is_nil() { acc = *a; continue; }
            acc = coll::conj(&acc, *a)?;
        }
        Ok(acc)
    }
    "zipmap" => @gc fn cf_zipmap(args) [2] {
        let ks = coll::to_vec(&args[0])?;
        let _ks = crate::gc::root_slice(&ks);
        let vs = coll::to_vec(&args[1])?;
        let _vs = crate::gc::root_slice(&vs);
        let mut m = map::empty_map();
        for (k, v) in ks.iter().zip(&vs) { map::put(&mut m, *k, *v); }
        Ok(map::new(m))
    }
    "compare" => fn cf_compare(args) [2] {
        Ok(Value::int(match coll::compare(&args[0], &args[1])? { Ordering::Less => -1, Ordering::Equal => 0, Ordering::Greater => 1 }))
    }
    "sort" => @gc fn cf_sort(args) [1, 2] {
        let items = coll::to_vec(&args[args.len() - 1])?;
        // The sort moves the items through vectors of its own while the comparator
        // runs: a copy that stays put keeps them rooted.
        let keep = items.clone();
        let _root = crate::gc::root_slice(&keep);
        let items = if args.len() == 2 {
            merge_sort(items, &mut |a, b| compare_with(&args[0], a, b))?
        } else {
            merge_sort(items, &mut |a, b| coll::compare(a, b))?
        };
        Ok(if items.is_empty() { list::empty() } else { list::from_iter(items) })
    }
    "sort-by" => @gc fn cf_sort_by(args) [2, 3] {
        // The items and the keys computed so far are rooted while the key fn and the
        // comparator run (the sort itself works on vectors of its own).
        let items = coll::to_vec(&args[args.len() - 1])?;
        let _items = crate::gc::root_slice(&items);
        let root = crate::gc::ArgRoot::empty();
        let _rooted = root.enter();
        let mut keys: Vec<Value> = Vec::with_capacity(items.len());
        for x in &items {
            let k = args[0].call(std::slice::from_ref(x))?;
            keys.push(k);
            root.set(&keys);
        }
        let keyed: Vec<(Value, Value)> = keys.iter().cloned().zip(items.iter().cloned()).collect();
        let keyed = merge_sort(keyed, &mut |(ka, _), (kb, _)| {
            if args.len() == 3 { compare_with(&args[1], ka, kb) } else { coll::compare(ka, kb) }
        })?;
        Ok(if keyed.is_empty() { list::empty() } else { list::from_iter(keyed.into_iter().map(|(_, x)| x).collect::<Vec<_>>()) })
    }
    "apply" => @gc fn cf_apply(args) [; 2] {
        // The arguments are gathered on the stack when there are few of them (the
        // spread collection keeps its elements alive; `call` roots what it is
        // given).
        const INLINE: usize = 12;
        let fixed = &args[1..args.len() - 1];
        let last = &args[args.len() - 1];
        let mut inline = [Value::NIL; INLINE];
        let mut n = fixed.len().min(INLINE);
        inline[..n].copy_from_slice(&fixed[..n]);
        let mut spill: Vec<Value> = Vec::new();
        if n < fixed.len() { spill.extend_from_slice(fixed); }
        for x in coll::seq_iter(last) {
            let x = x?;
            if spill.is_empty() && n < INLINE {
                inline[n] = x;
                n += 1;
            } else {
                if spill.is_empty() { spill.extend_from_slice(&inline[..n]); }
                spill.push(x);
            }
        }
        args[0].call(if spill.is_empty() { &inline[..n] } else { &spill })
    }
    "list?" => fn cf_list_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::List))) }
    "vector?" => fn cf_vector_p(args) [1] { Ok(Value::bool(args[0].is_kind(Kind::Vector))) }
    "map?" => fn cf_map_p(args) [1] { Ok(Value::bool(coll::is_map_like(&args[0]))) }
    "set?" => fn cf_set_p(args) [1] { Ok(Value::bool(matches!(args[0].kind(), Some(Kind::Set | Kind::SortedSet)))) }
    "coll?" => fn cf_coll_p(args) [1] { Ok(Value::bool(matches!(args[0].kind(), Some(Kind::List | Kind::Vector | Kind::Map | Kind::Set | Kind::Cons | Kind::LazySeq | Kind::VecSeq | Kind::SortedMap | Kind::SortedSet)) || crate::obj::types::is_record(&args[0]))) }
    "seq?" => fn cf_seq_p(args) [1] { Ok(Value::bool(coll::is_seq(&args[0]))) }
    "sequential?" => fn cf_sequential_p(args) [1] { Ok(Value::bool(matches!(args[0].kind(), Some(Kind::List | Kind::Vector | Kind::Cons | Kind::LazySeq | Kind::VecSeq)))) }
    "associative?" => fn cf_associative_p(args) [1] { Ok(Value::bool(matches!(args[0].kind(), Some(Kind::Map | Kind::Vector | Kind::SortedMap)) || crate::obj::types::is_record(&args[0]))) }
    "seqable?" => fn cf_seqable_p(args) [1] { Ok(Value::bool(coll::is_seqable(&args[0]))) }
    "ifn?" => fn cf_ifn_p(args) [1] { Ok(Value::bool(matches!(args[0].kind(), Some(Kind::Fn | Kind::Keyword | Kind::Map | Kind::Set | Kind::Vector | Kind::Var | Kind::SortedMap | Kind::SortedSet)))) }
    "hash" => fn cf_hash(args) [1] { Ok(Value::int((crate::equiv::hasheq(&args[0]) as i64) & 0x7fff_ffff)) }
}

/// A stable merge sort with a fallible comparator. Unlike `slice::sort_by` it accepts
/// any comparator (a user fn need not be a total order; the result is then merely
/// some permutation) and stops at the first error.
fn merge_sort<T>(
    mut items: Vec<T>,
    cmp: &mut dyn FnMut(&T, &T) -> RtResult<Ordering>,
) -> RtResult<Vec<T>> {
    if items.len() < 2 {
        return Ok(items);
    }
    let right = items.split_off(items.len() / 2);
    let left = merge_sort(items, cmp)?;
    let right = merge_sort(right, cmp)?;
    let mut out = Vec::with_capacity(left.len() + right.len());
    let (mut l, mut r) = (left.into_iter().peekable(), right.into_iter().peekable());
    loop {
        match (l.peek(), r.peek()) {
            (Some(a), Some(b)) => {
                // Stable: the right element goes first only when strictly smaller.
                if cmp(b, a)? == Ordering::Less {
                    out.extend(r.next());
                } else {
                    out.extend(l.next());
                }
            }
            (Some(_), None) => out.extend(l.by_ref()),
            (None, Some(_)) => out.extend(r.by_ref()),
            (None, None) => return Ok(out),
        }
    }
}

/// Order two values with a user comparator: an integer result is a signum, a boolean
/// result is a predicate (`(pred a b)` → less, `(pred b a)` → greater, else equal).
fn compare_with(f: &Value, a: &Value, b: &Value) -> RtResult<Ordering> {
    let r = f.call(&[*a, *b])?;
    // Any number is a signum (Clojure takes its `intValue`).
    if let Some(n) = crate::num::num(&r) {
        return Ok(crate::num::sign(&n));
    }
    if r.truthy() {
        return Ok(Ordering::Less);
    }
    let back = f.call(&[*b, *a])?;
    Ok(if back.truthy() {
        Ordering::Greater
    } else {
        Ordering::Equal
    })
}

fn assoc_in(m: &Value, path: &[Value], v: Value) -> RtResult<Value> {
    match path {
        [] => Ok(v),
        [k] => coll::assoc(m, *k, v),
        [k, rest @ ..] => {
            let inner = coll::get(m, k, None)?;
            let new_inner = assoc_in(&inner, rest, v)?;
            coll::assoc(m, *k, new_inner)
        }
    }
}
