//! JSON Schema 2020-12 값 동치(`json_equal`)와 정확한 숫자 비교·정수 추출.
//! `serde_json::Value`의 기본 비교는 `1`과 `1.0`을 다르게 보고, `f64` 변환은 `2^53`을 넘는 정수의 정밀도를 잃는다.
use serde_json::{Number, Value};
use std::cmp::Ordering;

/// `-2^63` (정확한 IEEE 754 `f64` 값).
const MIN_I64_F64: f64 = -9_223_372_036_854_775_808.0;
/// `2^63` (정확한 IEEE 754 `f64` 값).
const BOUND_I64_F64: f64 = 9_223_372_036_854_775_808.0;
/// `2^64` (정확한 IEEE 754 `f64` 값).
const BOUND_U64_F64: f64 = 18_446_744_073_709_551_616.0;

/// JSON Schema 2020-12 의미의 값 동치(`1 == 1.0`, 객체 키 순서 무관).
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

/// `Number`가 정확히 정수(`i64`·`u64`)로 저장돼 있으면 `i128`로 꺼낸다.
pub(super) fn as_int(n: &Number) -> Option<i128> {
    n.as_i64()
        .map(i128::from)
        .or_else(|| n.as_u64().map(i128::from))
}

/// JSON Schema `integer` 타입 판정: 정수 표기이거나 소수부가 0인 유한 부동소수점.
pub(super) fn is_integer(n: &Number) -> bool {
    n.is_i64()
        || n.is_u64()
        || n.as_f64()
            .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
}

/// 정수(`i128`, `[-2^63, 2^64 - 1]`)와 부동소수점(`f64`)의 정확한 수학적 크기 비교.
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
    // [-2^63, 2^64) 안의 모든 유한 `f64`의 정수부는 `i128`로 정밀도 손실 없이 표현된다.
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

/// 두 JSON 숫자의 정확한 크기 비교(양쪽 정수면 `i128` 비교, 정수·실수 혼합도 반올림 오차 없음).
pub(super) fn num_cmp(a: &Number, b: &Number) -> Option<Ordering> {
    match (as_int(a), as_int(b)) {
        (Some(x), Some(y)) => Some(x.cmp(&y)),
        (Some(x), None) => cmp_int_f64(x, b.as_f64()?),
        (None, Some(y)) => cmp_int_f64(y, a.as_f64()?).map(Ordering::reverse),
        (None, None) => a.as_f64()?.partial_cmp(&b.as_f64()?),
    }
}

/// 정수로 쓰인 JSON 값(`300` 또는 YAML float-integer `300.0`)을 정확한 `u64`로 읽는다.
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

/// 정수로 쓰인 JSON 값(`5`·`-3` 또는 YAML float-integer `5.0`)을 정확한 `i64`로 읽는다.
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
