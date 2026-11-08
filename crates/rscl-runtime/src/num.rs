//! Numeric tower: 64-bit integers (fixnum or boxed) and doubles. Integer overflow
//! throws `ArithmeticException` like Clojure's unprimed operators. `/` on integers
//! that do not divide exactly yields a double (documented divergence until Ratio).
use std::cmp::Ordering;

use crate::exc::{RtResult, err};
use crate::obj::exception::{ARITHMETIC, CLASS_CAST};
use crate::value::Value;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Num {
    I(i64),
    D(f64),
}

pub fn num(v: &Value) -> Option<Num> {
    if let Some(n) = v.as_int() {
        Some(Num::I(n))
    } else {
        v.as_double().map(Num::D)
    }
}

pub fn to_value(n: Num) -> Value {
    match n {
        Num::I(i) => Value::int(i),
        Num::D(d) => Value::double(d),
    }
}

pub fn expect_num(v: &Value) -> RtResult<Num> {
    match num(v) {
        Some(n) => Ok(n),
        None => err(
            CLASS_CAST,
            format!("{} cannot be cast to Number", crate::core::type_name(v)),
        ),
    }
}

fn overflow<T>() -> RtResult<T> {
    err(ARITHMETIC, "integer overflow")
}

pub fn add(a: Num, b: Num) -> RtResult<Num> {
    Ok(match (a, b) {
        (Num::I(x), Num::I(y)) => Num::I(x.checked_add(y).map_or_else(overflow, Ok)?),
        (x, y) => Num::D(f(x) + f(y)),
    })
}

pub fn sub(a: Num, b: Num) -> RtResult<Num> {
    Ok(match (a, b) {
        (Num::I(x), Num::I(y)) => Num::I(x.checked_sub(y).map_or_else(overflow, Ok)?),
        (x, y) => Num::D(f(x) - f(y)),
    })
}

pub fn mul(a: Num, b: Num) -> RtResult<Num> {
    Ok(match (a, b) {
        (Num::I(x), Num::I(y)) => Num::I(x.checked_mul(y).map_or_else(overflow, Ok)?),
        (x, y) => Num::D(f(x) * f(y)),
    })
}

pub fn div(a: Num, b: Num) -> RtResult<Num> {
    Ok(match (a, b) {
        (Num::I(_), Num::I(0)) => return err(ARITHMETIC, "Divide by zero"),
        (Num::I(x), Num::I(y)) => match x.checked_rem(y) {
            Some(0) => Num::I(x.checked_div(y).map_or_else(overflow, Ok)?),
            Some(_) => Num::D(x as f64 / y as f64),
            None => return overflow(),
        },
        (x, y) => Num::D(f(x) / f(y)),
    })
}

pub fn quot(a: Num, b: Num) -> RtResult<Num> {
    Ok(match (a, b) {
        (Num::I(_), Num::I(0)) => return err(ARITHMETIC, "Divide by zero"),
        (Num::I(x), Num::I(y)) => Num::I(x.checked_div(y).map_or_else(overflow, Ok)?),
        (x, y) => {
            let q = f(x) / f(y);
            if q.is_finite() {
                Num::D(q.trunc())
            } else {
                return err(ARITHMETIC, "Divide by zero");
            }
        }
    })
}

pub fn rem(a: Num, b: Num) -> RtResult<Num> {
    Ok(match (a, b) {
        (Num::I(_), Num::I(0)) => return err(ARITHMETIC, "Divide by zero"),
        (Num::I(x), Num::I(y)) => Num::I(x.checked_rem(y).unwrap_or(0)),
        (x, y) => {
            let (x, y) = (f(x), f(y));
            if y == 0.0 {
                return err(ARITHMETIC, "Divide by zero");
            } else {
                Num::D(x % y)
            }
        }
    })
}

/// Clojure `mod`: result has the sign of the divisor.
pub fn modulo(a: Num, b: Num) -> RtResult<Num> {
    let r = rem(a, b)?;
    Ok(match (r, b) {
        (Num::I(r), Num::I(y)) if r != 0 && (r < 0) != (y < 0) => Num::I(r + y),
        (Num::D(r), y) if r != 0.0 && (r < 0.0) != (f(y) < 0.0) => Num::D(r + f(y)),
        (r, _) => r,
    })
}

pub fn neg(a: Num) -> RtResult<Num> {
    Ok(match a {
        Num::I(x) => Num::I(x.checked_neg().map_or_else(overflow, Ok)?),
        Num::D(d) => Num::D(-d),
    })
}

pub fn inc(a: Num) -> RtResult<Num> {
    add(a, Num::I(1))
}

pub fn dec(a: Num) -> RtResult<Num> {
    sub(a, Num::I(1))
}

/// Numeric comparison; `None` only when a NaN is involved.
pub fn cmp(a: Num, b: Num) -> Option<Ordering> {
    match (a, b) {
        (Num::I(x), Num::I(y)) => Some(x.cmp(&y)),
        (x, y) => f(x).partial_cmp(&f(y)),
    }
}

/// `==`: numeric equality across categories.
pub fn equiv(a: Num, b: Num) -> bool {
    cmp(a, b) == Some(Ordering::Equal)
}

pub fn is_zero(a: Num) -> bool {
    match a {
        Num::I(x) => x == 0,
        Num::D(d) => d == 0.0,
    }
}

pub fn sign(a: Num) -> Ordering {
    match a {
        Num::I(x) => x.cmp(&0),
        Num::D(d) => d.partial_cmp(&0.0).unwrap_or(Ordering::Equal),
    }
}

#[inline]
fn f(n: Num) -> f64 {
    match n {
        Num::I(i) => i as f64,
        Num::D(d) => d,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic() {
        assert_eq!(add(Num::I(1), Num::I(2)).unwrap(), Num::I(3));
        assert_eq!(add(Num::I(1), Num::D(0.5)).unwrap(), Num::D(1.5));
        assert!(add(Num::I(i64::MAX), Num::I(1)).is_err());
        crate::exc::take();
        assert_eq!(div(Num::I(6), Num::I(3)).unwrap(), Num::I(2));
        assert_eq!(div(Num::I(1), Num::I(2)).unwrap(), Num::D(0.5));
        assert!(div(Num::I(1), Num::I(0)).is_err());
        crate::exc::take();
        assert_eq!(modulo(Num::I(-7), Num::I(3)).unwrap(), Num::I(2));
        assert_eq!(rem(Num::I(-7), Num::I(3)).unwrap(), Num::I(-1));
        assert_eq!(quot(Num::I(-7), Num::I(2)).unwrap(), Num::I(-3));
        assert!(equiv(Num::I(1), Num::D(1.0)));
        assert_eq!(cmp(Num::I(2), Num::D(1.5)), Some(Ordering::Greater));
    }
}
