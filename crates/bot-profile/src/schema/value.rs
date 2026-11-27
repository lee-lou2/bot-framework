//! JSON Schema 2020-12 value equality and lossless numeric comparison and integer extraction.
use serde_json::{Number, Value};
use std::cmp::Ordering;

/// Exact IEEE 754 `f64` boundary values for `-2^63`, `2^63`, and `2^64`.
const MIN_I64_F64: f64 = -9_223_372_036_854_775_808.0;
const BOUND_I64_F64: f64 = 9_223_372_036_854_775_808.0;
const BOUND_U64_F64: f64 = 18_446_744_073_709_551_616.0;

/// JSON Schema 2020-12 equality (`1 == 1.0`, order-independent object keys).
pub(super) fn json_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => num_cmp(x, y) == Some(Ordering::Equal),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(l, r)| json_equal(l, r))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_equal(v, w)))
        }
        _ => false,
    }
}

pub(super) fn as_int(n: &Number) -> Option<i128> {
    n.as_i64()
        .map(i128::from)
        .or_else(|| n.as_u64().map(i128::from))
}

pub(super) fn is_integer(n: &Number) -> bool {
    n.is_i64()
        || n.is_u64()
        || n.as_f64()
            .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
}

fn cmp_int_f64(int: i128, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    if f.is_infinite() {
        return Some(if f.is_sign_positive() {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    if f < MIN_I64_F64 {
        return Some(Ordering::Greater);
    }
    if f >= BOUND_U64_F64 {
        return Some(Ordering::Less);
    }
    // Every finite `f64` in `[-2^63, 2^64)` has a lossless integer part in `i128`.
    let trunc = f.trunc() as i128;
    match int.cmp(&trunc) {
        Ordering::Equal => {
            let frac = f.fract();
            if frac == 0.0 {
                Some(Ordering::Equal)
            } else if frac > 0.0 {
                Some(Ordering::Less)
            } else {
                Some(Ordering::Greater)
            }
        }
        other => Some(other),
    }
}

/// Lossless comparison of JSON numbers across `i64`, `u64`, and `f64` (avoids `> 2^53` rounding).
pub(super) fn num_cmp(a: &Number, b: &Number) -> Option<Ordering> {
    match (as_int(a), as_int(b)) {
        (Some(x), Some(y)) => Some(x.cmp(&y)),
        (Some(x), None) => cmp_int_f64(x, b.as_f64()?),
        (None, Some(y)) => cmp_int_f64(y, a.as_f64()?).map(Ordering::reverse),
        (None, None) => a.as_f64()?.partial_cmp(&b.as_f64()?),
    }
}

/// Extracts an exact `u64` from an integer JSON value or whole-number float (`300.0`).
pub(crate) fn as_exact_u64(v: &Value) -> Option<u64> {
    let n = v.as_number()?;
    if let Some(u) = n.as_u64() {
        return Some(u);
    }
    if let Some(i) = n.as_i64() {
        return u64::try_from(i).ok();
    }
    let f = n.as_f64()?;
    (f.is_finite() && (0.0..BOUND_U64_F64).contains(&f) && f.fract() == 0.0).then_some(f as u64)
}

/// Extracts an exact `i64` from an integer JSON value or whole-number float (`5.0`).
pub(crate) fn as_exact_i64(v: &Value) -> Option<i64> {
    let n = v.as_number()?;
    if let Some(i) = n.as_i64() {
        return Some(i);
    }
    if let Some(u) = n.as_u64() {
        return i64::try_from(u).ok();
    }
    let f = n.as_f64()?;
    (f.is_finite() && (MIN_I64_F64..BOUND_I64_F64).contains(&f) && f.fract() == 0.0)
        .then_some(f as i64)
}
