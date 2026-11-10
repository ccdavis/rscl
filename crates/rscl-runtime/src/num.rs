//! Numeric tower: 64-bit integers (fixnum or boxed), arbitrary-precision integers
//! (`BigInt`, printed with an `N` suffix) and doubles. Integer overflow throws
//! `ArithmeticException` like Clojure's unprimed operators; the primed operators
//! (`+'`, `-'`, `*'`, `inc'`, `dec'`) promote to `BigInt` instead. A `BigInt` is
//! contagious: an operation with a `BigInt` operand gives a `BigInt`. `/` on integers
//! that do not divide exactly yields a double (documented divergence until Ratio).
use std::cmp::Ordering;

use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{FromPrimitive, Signed, ToPrimitive, Zero};

use crate::exc::{RtResult, err};
use crate::obj::exception::{ARITHMETIC, CLASS_CAST};
use crate::obj::{Kind, bigint, long};
use crate::value::Value;

#[derive(Clone, Debug, PartialEq)]
pub enum Num {
    I(i64),
    D(f64),
    B(BigInt),
}

pub fn num(v: &Value) -> Option<Num> {
    if let Some(n) = v.as_fixnum() {
        return Some(Num::I(n));
    }
    match v.kind() {
        Some(Kind::Long) => Some(Num::I(long::get(v))),
        Some(Kind::Double) => Some(Num::D(v.as_double()?)),
        Some(Kind::BigInt) => Some(Num::B(bigint::get(v).clone())),
        _ => None,
    }
}

pub fn to_value(n: Num) -> Value {
    match n {
        Num::I(i) => Value::int(i),
        Num::D(d) => Value::double(d),
        Num::B(b) => bigint::new(b),
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

fn big(i: i64) -> BigInt {
    BigInt::from(i)
}

fn into_big(n: Num) -> BigInt {
    match n {
        Num::I(i) => big(i),
        Num::B(b) => b,
        Num::D(_) => unreachable!("doubles are handled before integers"),
    }
}

fn is_double(a: &Num, b: &Num) -> bool {
    matches!(a, Num::D(_)) || matches!(b, Num::D(_))
}

pub fn add(a: Num, b: Num) -> RtResult<Num> {
    Ok(match (a, b) {
        (Num::I(x), Num::I(y)) => Num::I(x.checked_add(y).map_or_else(overflow, Ok)?),
        (a, b) if is_double(&a, &b) => Num::D(f(&a) + f(&b)),
        (a, b) => Num::B(into_big(a) + into_big(b)),
    })
}

pub fn sub(a: Num, b: Num) -> RtResult<Num> {
    Ok(match (a, b) {
        (Num::I(x), Num::I(y)) => Num::I(x.checked_sub(y).map_or_else(overflow, Ok)?),
        (a, b) if is_double(&a, &b) => Num::D(f(&a) - f(&b)),
        (a, b) => Num::B(into_big(a) - into_big(b)),
    })
}

pub fn mul(a: Num, b: Num) -> RtResult<Num> {
    Ok(match (a, b) {
        (Num::I(x), Num::I(y)) => Num::I(x.checked_mul(y).map_or_else(overflow, Ok)?),
        (a, b) if is_double(&a, &b) => Num::D(f(&a) * f(&b)),
        (a, b) => Num::B(into_big(a) * into_big(b)),
    })
}

/// `+'`: like `add`, but a 64-bit overflow promotes to `BigInt`.
pub fn add_p(a: Num, b: Num) -> RtResult<Num> {
    match (&a, &b) {
        (Num::I(x), Num::I(y)) => Ok(match x.checked_add(*y) {
            Some(n) => Num::I(n),
            None => Num::B(big(*x) + big(*y)),
        }),
        _ => add(a, b),
    }
}

pub fn sub_p(a: Num, b: Num) -> RtResult<Num> {
    match (&a, &b) {
        (Num::I(x), Num::I(y)) => Ok(match x.checked_sub(*y) {
            Some(n) => Num::I(n),
            None => Num::B(big(*x) - big(*y)),
        }),
        _ => sub(a, b),
    }
}

pub fn mul_p(a: Num, b: Num) -> RtResult<Num> {
    match (&a, &b) {
        (Num::I(x), Num::I(y)) => Ok(match x.checked_mul(*y) {
            Some(n) => Num::I(n),
            None => Num::B(big(*x) * big(*y)),
        }),
        _ => mul(a, b),
    }
}

/// `x / y` as a double for integers that do not divide exactly. Converting each
/// operand first would give infinity or NaN beyond the double range, so the quotient
/// is computed to 64 significant bits as an integer and scaled back.
fn big_ratio(x: &BigInt, y: &BigInt) -> f64 {
    let shift = 64 - (x.bits() as i64 - y.bits() as i64);
    let q = if shift >= 0 {
        (x << shift as usize) / y
    } else {
        x / (y << (-shift) as usize)
    };
    let q = q.to_f64().unwrap_or(f64::INFINITY);
    // Two steps keep the scale factor itself inside the double range.
    let half = -shift / 2;
    q * 2f64.powi(half as i32) * 2f64.powi((-shift - half) as i32)
}

pub fn div(a: Num, b: Num) -> RtResult<Num> {
    Ok(match (a, b) {
        (a, b) if is_double(&a, &b) => Num::D(f(&a) / f(&b)),
        (Num::I(_), Num::I(0)) => return err(ARITHMETIC, "Divide by zero"),
        (Num::I(x), Num::I(y)) => match x.checked_rem(y) {
            Some(0) => Num::I(x.checked_div(y).map_or_else(overflow, Ok)?),
            Some(_) => Num::D(x as f64 / y as f64),
            None => return overflow(),
        },
        (a, b) => {
            let (x, y) = (into_big(a), into_big(b));
            if y.is_zero() {
                return err(ARITHMETIC, "Divide by zero");
            }
            let (q, r) = x.div_rem(&y);
            if r.is_zero() {
                Num::B(q)
            } else {
                Num::D(big_ratio(&x, &y))
            }
        }
    })
}

pub fn quot(a: Num, b: Num) -> RtResult<Num> {
    Ok(match (a, b) {
        (a, b) if is_double(&a, &b) => {
            let q = f(&a) / f(&b);
            if q.is_finite() {
                Num::D(q.trunc())
            } else {
                return err(ARITHMETIC, "Divide by zero");
            }
        }
        (Num::I(_), Num::I(0)) => return err(ARITHMETIC, "Divide by zero"),
        (Num::I(x), Num::I(y)) => Num::I(x.checked_div(y).map_or_else(overflow, Ok)?),
        (a, b) => {
            let (x, y) = (into_big(a), into_big(b));
            if y.is_zero() {
                return err(ARITHMETIC, "Divide by zero");
            }
            Num::B(x.div_rem(&y).0)
        }
    })
}

pub fn rem(a: Num, b: Num) -> RtResult<Num> {
    Ok(match (a, b) {
        (a, b) if is_double(&a, &b) => {
            let (x, y) = (f(&a), f(&b));
            if y == 0.0 {
                return err(ARITHMETIC, "Divide by zero");
            } else {
                Num::D(x % y)
            }
        }
        (Num::I(_), Num::I(0)) => return err(ARITHMETIC, "Divide by zero"),
        (Num::I(x), Num::I(y)) => Num::I(x.checked_rem(y).unwrap_or(0)),
        (a, b) => {
            let (x, y) = (into_big(a), into_big(b));
            if y.is_zero() {
                return err(ARITHMETIC, "Divide by zero");
            }
            Num::B(x.div_rem(&y).1)
        }
    })
}

/// Clojure `mod`: result has the sign of the divisor.
pub fn modulo(a: Num, b: Num) -> RtResult<Num> {
    match (a, b) {
        (Num::I(_), Num::I(0)) => err(ARITHMETIC, "Divide by zero"),
        (Num::I(x), Num::I(y)) => {
            let r = x.checked_rem(y).unwrap_or(0);
            Ok(Num::I(if r != 0 && (r < 0) != (y < 0) {
                r + y
            } else {
                r
            }))
        }
        (a, b) if is_double(&a, &b) => {
            let y = f(&b);
            let r = match rem(a, b)? {
                Num::D(r) => r,
                _ => unreachable!("double rem"),
            };
            Ok(Num::D(if r != 0.0 && (r < 0.0) != (y < 0.0) {
                r + y
            } else {
                r
            }))
        }
        (a, b) => {
            let (x, y) = (into_big(a), into_big(b));
            if y.is_zero() {
                return err(ARITHMETIC, "Divide by zero");
            }
            Ok(Num::B(x.mod_floor(&y)))
        }
    }
}

pub fn neg(a: Num) -> RtResult<Num> {
    Ok(match a {
        Num::I(x) => Num::I(x.checked_neg().map_or_else(overflow, Ok)?),
        Num::D(d) => Num::D(-d),
        Num::B(b) => Num::B(-b),
    })
}

/// Unary `-'`: only the smallest long needs promoting.
pub fn neg_p(a: Num) -> Num {
    match a {
        Num::I(x) => x.checked_neg().map_or_else(|| Num::B(-big(x)), Num::I),
        Num::D(d) => Num::D(-d),
        Num::B(b) => Num::B(-b),
    }
}

pub fn inc(a: Num) -> RtResult<Num> {
    add(a, Num::I(1))
}

pub fn dec(a: Num) -> RtResult<Num> {
    sub(a, Num::I(1))
}

pub fn inc_p(a: Num) -> RtResult<Num> {
    add_p(a, Num::I(1))
}

pub fn dec_p(a: Num) -> RtResult<Num> {
    sub_p(a, Num::I(1))
}

/// Numeric comparison; `None` only when a NaN is involved.
pub fn cmp(a: &Num, b: &Num) -> Option<Ordering> {
    match (a, b) {
        (Num::I(x), Num::I(y)) => Some(x.cmp(y)),
        (Num::D(_), _) | (_, Num::D(_)) => f(a).partial_cmp(&f(b)),
        (Num::B(x), Num::B(y)) => Some(x.cmp(y)),
        (Num::B(x), Num::I(y)) => Some(x.cmp(&big(*y))),
        (Num::I(x), Num::B(y)) => Some(big(*x).cmp(y)),
    }
}

/// `==`: numeric equality across categories.
pub fn equiv(a: &Num, b: &Num) -> bool {
    cmp(a, b) == Some(Ordering::Equal)
}

pub fn is_zero(a: &Num) -> bool {
    match a {
        Num::I(x) => *x == 0,
        Num::D(d) => *d == 0.0,
        Num::B(b) => b.is_zero(),
    }
}

pub fn sign(a: &Num) -> Ordering {
    match a {
        Num::I(x) => x.cmp(&0),
        Num::D(d) => d.partial_cmp(&0.0).unwrap_or(Ordering::Equal),
        Num::B(b) => {
            if b.is_zero() {
                Ordering::Equal
            } else if b.is_positive() {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
    }
}

pub fn is_even(a: &Num) -> Option<bool> {
    match a {
        Num::I(x) => Some(x % 2 == 0),
        Num::B(b) => Some(b.is_even()),
        Num::D(_) => None,
    }
}

/// The value as an `f64` (`BigInt`s beyond the double range become infinite).
pub fn f(n: &Num) -> f64 {
    match n {
        Num::I(i) => *i as f64,
        Num::D(d) => *d,
        Num::B(b) => b.to_f64().unwrap_or(f64::INFINITY),
    }
}

/// `(long n)`: integers that fit, doubles truncated; a `BigInt` out of range is an
/// error.
pub fn to_long(n: &Num) -> RtResult<i64> {
    match n {
        Num::I(i) => Ok(*i),
        Num::D(d) => Ok(*d as i64),
        Num::B(b) => b.to_i64().map_or_else(
            || {
                err(
                    crate::obj::exception::ILLEGAL_ARGUMENT,
                    format!("Value out of range for long: {b}"),
                )
            },
            Ok,
        ),
    }
}

/// `(bigint n)`: integers exactly, doubles truncated.
pub fn to_big(n: &Num) -> RtResult<BigInt> {
    match n {
        Num::I(i) => Ok(big(*i)),
        Num::B(b) => Ok(b.clone()),
        Num::D(d) => BigInt::from_f64(d.trunc()).map_or_else(
            || {
                err(
                    crate::obj::exception::ILLEGAL_ARGUMENT,
                    format!("Cannot convert {d} to a BigInt"),
                )
            },
            Ok,
        ),
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
        assert!(equiv(&Num::I(1), &Num::D(1.0)));
        assert_eq!(cmp(&Num::I(2), &Num::D(1.5)), Some(Ordering::Greater));
    }

    #[test]
    fn bigints() {
        let max = Num::I(i64::MAX);
        let promoted = add_p(max.clone(), Num::I(1)).unwrap();
        assert_eq!(promoted, Num::B(big(i64::MAX) + big(1)));
        // Contagious, even when the result would fit.
        assert_eq!(
            sub(promoted.clone(), Num::I(1)).unwrap(),
            Num::B(big(i64::MAX))
        );
        assert_eq!(mul(Num::B(big(2)), Num::I(3)).unwrap(), Num::B(big(6)));
        assert_eq!(div(Num::B(big(6)), Num::I(3)).unwrap(), Num::B(big(2)));
        assert_eq!(div(Num::B(big(7)), Num::I(2)).unwrap(), Num::D(3.5));
        assert_eq!(modulo(Num::B(big(-7)), Num::I(3)).unwrap(), Num::B(big(2)));
        assert_eq!(rem(Num::B(big(-7)), Num::I(3)).unwrap(), Num::B(big(-1)));
        assert_eq!(quot(Num::B(big(-7)), Num::I(2)).unwrap(), Num::B(big(-3)));
        assert_eq!(cmp(&Num::B(big(5)), &Num::I(5)), Some(Ordering::Equal));
        assert_eq!(cmp(&promoted, &Num::D(1e30)), Some(Ordering::Less));
        assert!(equiv(&Num::B(big(2)), &Num::D(2.0)));
        assert_eq!(to_long(&Num::B(big(42))).unwrap(), 42);
        assert!(to_long(&promoted).is_err());
        crate::exc::take();
        assert_eq!(is_even(&Num::B(big(10))), Some(true));
        // A quotient of integers beyond the double range is still finite.
        let huge = Num::B(num_traits::pow(big(10), 400));
        let q = div(
            add(huge.clone(), Num::I(1)).unwrap(),
            Num::B(num_traits::pow(big(10), 399)),
        );
        assert!(matches!(q.unwrap(), Num::D(d) if (d - 10.0).abs() < 1e-9));
        assert_eq!(neg_p(Num::I(i64::MIN)), Num::B(-big(i64::MIN)));
        assert_eq!(neg_p(Num::D(0.0)), Num::D(-0.0));
        assert!(matches!(neg_p(Num::D(0.0)), Num::D(d) if d.is_sign_negative()));
        assert_eq!(modulo(Num::I(-7), Num::I(3)).unwrap(), Num::I(2));
        assert_eq!(modulo(Num::D(-7.0), Num::I(3)).unwrap(), Num::D(2.0));
        assert_eq!(sign(&Num::B(big(-3))), Ordering::Less);
    }
}
