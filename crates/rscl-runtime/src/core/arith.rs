//! Arithmetic and comparison builtins.
use std::cmp::Ordering;

use crate::builtins;
use crate::exc::RtResult;
use crate::num::{self, Num, expect_num, to_value};
use crate::value::Value;

fn fold(args: &[Value], init: Num, op: fn(Num, Num) -> RtResult<Num>) -> RtResult<Value> {
    let mut acc = init;
    for a in args {
        acc = op(acc, expect_num(a)?)?;
    }
    Ok(to_value(acc))
}

fn chain(args: &[Value], ok: fn(Option<Ordering>) -> bool) -> RtResult<Value> {
    let mut prev = expect_num(&args[0])?;
    for a in &args[1..] {
        let cur = expect_num(a)?;
        if !ok(num::cmp(&prev, &cur)) {
            return Ok(Value::FALSE);
        }
        prev = cur;
    }
    Ok(Value::TRUE)
}

fn parity(args: &[Value], want_even: bool) -> RtResult<Value> {
    match num::is_even(&expect_num(&args[0])?) {
        Some(even) => Ok(Value::bool(even == want_even)),
        None => crate::exc::err(
            crate::obj::exception::ILLEGAL_ARGUMENT,
            "Argument must be an integer",
        ),
    }
}

builtins! {
    "+" => fn cf_add(args) [; 0] { fold(args, Num::I(0), num::add) }
    "*" => fn cf_mul(args) [; 0] { fold(args, Num::I(1), num::mul) }
    "-" => fn cf_sub(args) [; 1] {
        if args.len() == 1 { return Ok(to_value(num::neg(expect_num(&args[0])?)?)); }
        fold(&args[1..], expect_num(&args[0])?, num::sub)
    }
    "/" => fn cf_div(args) [; 1] {
        if args.len() == 1 { return Ok(to_value(num::div(Num::I(1), expect_num(&args[0])?)?)); }
        fold(&args[1..], expect_num(&args[0])?, num::div)
    }
    "quot" => fn cf_quot(args) [2] { Ok(to_value(num::quot(expect_num(&args[0])?, expect_num(&args[1])?)?)) }
    "rem" => fn cf_rem(args) [2] { Ok(to_value(num::rem(expect_num(&args[0])?, expect_num(&args[1])?)?)) }
    "mod" => fn cf_mod(args) [2] { Ok(to_value(num::modulo(expect_num(&args[0])?, expect_num(&args[1])?)?)) }
    "inc" => fn cf_inc(args) [1] { Ok(to_value(num::inc(expect_num(&args[0])?)?)) }
    "dec" => fn cf_dec(args) [1] { Ok(to_value(num::dec(expect_num(&args[0])?)?)) }
    "<" => fn cf_lt(args) [; 1] { chain(args, |o| o == Some(Ordering::Less)) }
    "<=" => fn cf_le(args) [; 1] { chain(args, |o| matches!(o, Some(Ordering::Less | Ordering::Equal))) }
    ">" => fn cf_gt(args) [; 1] { chain(args, |o| o == Some(Ordering::Greater)) }
    ">=" => fn cf_ge(args) [; 1] { chain(args, |o| matches!(o, Some(Ordering::Greater | Ordering::Equal))) }
    "==" => fn cf_num_eq(args) [; 1] { chain(args, |o| o == Some(Ordering::Equal)) }
    "max" => fn cf_max(args) [; 1] {
        let mut best = expect_num(&args[0])?;
        for a in &args[1..] { let n = expect_num(a)?; if num::cmp(&n, &best) == Some(Ordering::Greater) { best = n; } }
        Ok(to_value(best))
    }
    "min" => fn cf_min(args) [; 1] {
        let mut best = expect_num(&args[0])?;
        for a in &args[1..] { let n = expect_num(a)?; if num::cmp(&n, &best) == Some(Ordering::Less) { best = n; } }
        Ok(to_value(best))
    }
    "zero?" => fn cf_zero_p(args) [1] { Ok(Value::bool(num::is_zero(&expect_num(&args[0])?))) }
    "pos?" => fn cf_pos_p(args) [1] { Ok(Value::bool(num::sign(&expect_num(&args[0])?) == Ordering::Greater)) }
    "neg?" => fn cf_neg_p(args) [1] { Ok(Value::bool(num::sign(&expect_num(&args[0])?) == Ordering::Less)) }
    "even?" => fn cf_even_p(args) [1] { parity(args, true) }
    "odd?" => fn cf_odd_p(args) [1] { parity(args, false) }
    // Auto-promoting arithmetic: a 64-bit overflow gives a BigInt instead of throwing.
    "+'" => fn cf_add_p(args) [; 0] { fold(args, Num::I(0), num::add_p) }
    "*'" => fn cf_mul_p(args) [; 0] { fold(args, Num::I(1), num::mul_p) }
    "-'" => fn cf_sub_p(args) [; 1] {
        if args.len() == 1 {
            return Ok(to_value(num::neg_p(expect_num(&args[0])?)));
        }
        fold(&args[1..], expect_num(&args[0])?, num::sub_p)
    }
    "inc'" => fn cf_inc_p(args) [1] { Ok(to_value(num::inc_p(expect_num(&args[0])?)?)) }
    "dec'" => fn cf_dec_p(args) [1] { Ok(to_value(num::dec_p(expect_num(&args[0])?)?)) }
    "bigint" => fn cf_bigint(args) [1] {
        if let Some(s) = crate::obj::string::as_str(&args[0]) {
            return match s.trim().parse::<num_bigint::BigInt>() {
                Ok(b) => Ok(crate::obj::bigint::new(b)),
                Err(_) => crate::exc::err(crate::obj::exception::ILLEGAL_ARGUMENT, format!("Invalid BigInt: {s:?}")),
            };
        }
        Ok(crate::obj::bigint::new(num::to_big(&expect_num(&args[0])?)?))
    }
    "biginteger" => fn cf_biginteger(args) [1] { Ok(crate::obj::bigint::new(num::to_big(&expect_num(&args[0])?)?)) }
    "bigint?" => fn cf_bigint_p(args) [1] { Ok(Value::bool(args[0].is_kind(crate::obj::Kind::BigInt))) }
}
