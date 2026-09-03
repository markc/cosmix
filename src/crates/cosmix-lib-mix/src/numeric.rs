//! Shared validation for numbers crossing into operational domains.
//!
//! Mix arithmetic is intentionally IEEE-754: existing numeric NaN/infinity
//! values propagate through ordinary maths. Operational sinks are different:
//! counts, indexes, durations, process statuses, timestamps and loop controls
//! must reject values that Rust's `as` casts would otherwise truncate or
//! saturate. This module is the single boundary between those two worlds.

use crate::error::{ErrorInfo, MixError, MixResult};
use crate::value::Value;

/// Largest integer magnitude for which every adjacent integer is exactly
/// representable by Mix's f64 number type.
pub(crate) const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0; // 2^53 - 1

/// Whether a numeric sink follows ordinary Mix coercion or requires a real
/// Number value. This says only which values may enter; each callsite retains
/// its existing error code/message when extraction fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputPolicy {
    StandardCoercion,
    NumberOnly,
}

pub(crate) fn extract_number(value: &Value, policy: InputPolicy) -> Option<f64> {
    match policy {
        InputPolicy::StandardCoercion => value.to_number(),
        InputPolicy::NumberOnly => match value {
            Value::Number(n) => Some(*n),
            _ => None,
        },
    }
}

/// Declared operational numeric domains. The variants are deliberately
/// semantic rather than Rust-type names: their contracts are what scripts
/// reason about, while the `as_*` extractors below own the checked conversion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum NumericDomain {
    Finite,
    Count {
        max: usize,
    },
    SignedIndex,
    ExactInteger {
        min: i64,
        max: i64,
    },
    Duration,
    ExitCode,
    // Consumed only by the date/time builtins, but the domain logic is
    // feature-independent and stays under test in every build.
    #[cfg_attr(not(feature = "datetime"), allow(dead_code))]
    Timestamp,
    LoopBound,
    LoopStep,
}

fn details(context: &str, value: f64, expected: &str, min: Option<f64>, max: Option<f64>) -> Value {
    let mut map = indexmap::IndexMap::new();
    map.insert("context".to_string(), Value::String(context.to_string()));
    map.insert("value".to_string(), Value::Number(value));
    map.insert("expected".to_string(), Value::String(expected.to_string()));
    map.insert("min".to_string(), min.map_or(Value::Nil, Value::Number));
    map.insert("max".to_string(), max.map_or(Value::Nil, Value::Number));
    Value::map(map)
}

fn out_of_range(
    context: &str,
    value: f64,
    expected: &str,
    min: Option<f64>,
    max: Option<f64>,
) -> MixError {
    MixError::Structured(Box::new(
        ErrorInfo::new(
            "VALUE_OUT_OF_RANGE",
            format!("{context} must be {expected}, got {value}"),
        )
        .with_details(details(context, value, expected, min, max)),
    ))
}

impl NumericDomain {
    pub(crate) fn validate(self, context: &str, value: f64) -> MixResult<f64> {
        // `expected` is a String, not a &str, so a domain whose bounds are
        // supplied by the CALLER can name them. ExactInteger is the case: its
        // range is per-call (0..63 for a shift count, 1..N for an index), so a
        // fixed description could only ever say "the declared range" and leave
        // the reader to guess which one. The bounds were already computed for
        // `details`; they simply never reached the sentence anyone reads.
        let (valid, expected, min, max): (bool, String, Option<f64>, Option<f64>) = match self {
            NumericDomain::Finite => (value.is_finite(), "a finite number".to_string(), None, None),
            NumericDomain::Count { max } => {
                let max = (max as f64).min(MAX_SAFE_INTEGER);
                (
                    value.is_finite() && value.fract() == 0.0 && (0.0..=max).contains(&value),
                    "a non-negative whole number within the supported count range".to_string(),
                    Some(0.0),
                    Some(max),
                )
            }
            NumericDomain::SignedIndex => (
                value.is_finite()
                    && value.fract() == 0.0
                    && (-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&value),
                "a whole number within the exact signed-index range".to_string(),
                Some(-MAX_SAFE_INTEGER),
                Some(MAX_SAFE_INTEGER),
            ),
            NumericDomain::ExactInteger { min, max } => {
                // Compare in i128, NOT via f64 bounds: `i64::MAX as f64`
                // rounds UP to 2^63, which would admit the very saturating
                // cast this domain exists to refuse -- and clamping the
                // declared bounds to +-2^53 (the first cut) silently
                // narrowed every caller's contract. A whole finite f64 is
                // an exact integer, and the f64->i128 cast preserves it
                // (saturating only far beyond any i64 bound, where the
                // comparison still answers correctly).
                let iv = value as i128;
                // Details render the largest value the domain ADMITS, not the
                // declared bound rounded to f64: `i64::MAX as f64` is 2^63,
                // one past i64 — reporting a max the validator itself rejects
                // would be a small lie in a documented details shape. (The
                // lower bound -2^63 is exactly representable, so it is safe.)
                let shown_max = if max == i64::MAX {
                    9_223_372_036_854_774_784.0 // largest f64 below 2^63
                } else {
                    max as f64
                };
                (
                    value.is_finite()
                        && value.fract() == 0.0
                        && iv >= min as i128
                        && iv <= max as i128,
                    format!("a whole number in {min}..={max}"),
                    Some(min as f64),
                    Some(shown_max),
                )
            }
            NumericDomain::Duration => (
                value.is_finite()
                    && value >= 0.0
                    && std::time::Duration::try_from_secs_f64(value).is_ok(),
                "a finite non-negative duration in seconds".to_string(),
                Some(0.0),
                None,
            ),
            NumericDomain::ExitCode => (
                value.is_finite() && value.fract() == 0.0 && (0.0..=255.0).contains(&value),
                "a whole process exit code in 0..=255".to_string(),
                Some(0.0),
                Some(255.0),
            ),
            NumericDomain::Timestamp => (
                value.is_finite()
                    && value.fract() == 0.0
                    && (-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&value),
                "a whole timestamp within Mix's exact-integer range".to_string(),
                Some(-MAX_SAFE_INTEGER),
                Some(MAX_SAFE_INTEGER),
            ),
            NumericDomain::LoopBound => (value.is_finite(), "a finite loop bound".to_string(), None, None),
            NumericDomain::LoopStep => (
                value.is_finite() && value != 0.0,
                "a finite non-zero loop step".to_string(),
                None,
                None,
            ),
        };
        if valid {
            Ok(value)
        } else {
            Err(out_of_range(context, value, &expected, min, max))
        }
    }
}

pub(crate) fn as_finite_number(context: &str, value: f64) -> MixResult<f64> {
    NumericDomain::Finite.validate(context, value)
}

pub(crate) fn as_count(context: &str, value: f64, max: usize) -> MixResult<usize> {
    Ok(NumericDomain::Count { max }.validate(context, value)? as usize)
}

pub(crate) fn as_signed_index(context: &str, value: f64) -> MixResult<i64> {
    Ok(NumericDomain::SignedIndex.validate(context, value)? as i64)
}

pub(crate) fn as_exact_integer(context: &str, value: f64, min: i64, max: i64) -> MixResult<i64> {
    Ok(NumericDomain::ExactInteger { min, max }.validate(context, value)? as i64)
}

pub(crate) fn as_duration(context: &str, value: f64) -> MixResult<std::time::Duration> {
    let value = NumericDomain::Duration.validate(context, value)?;
    std::time::Duration::try_from_secs_f64(value)
        .map_err(|_| out_of_range(context, value, "a representable duration", Some(0.0), None))
}

pub(crate) fn as_exit_code(context: &str, value: f64) -> MixResult<i32> {
    Ok(NumericDomain::ExitCode.validate(context, value)? as i32)
}

#[cfg_attr(not(feature = "datetime"), allow(dead_code))]
pub(crate) fn as_timestamp(context: &str, value: f64) -> MixResult<i64> {
    Ok(NumericDomain::Timestamp.validate(context, value)? as i64)
}

pub(crate) fn as_loop_bound(context: &str, value: f64) -> MixResult<f64> {
    NumericDomain::LoopBound.validate(context, value)
}

pub(crate) fn as_loop_step(context: &str, value: f64) -> MixResult<f64> {
    NumericDomain::LoopStep.validate(context, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(err: MixError) -> String {
        err.info()
            .expect("domain errors are structured")
            .code
            .clone()
    }

    #[test]
    fn input_policies_are_independent_of_domains() {
        let numeric_string = Value::String("2".into());
        assert_eq!(
            extract_number(&numeric_string, InputPolicy::StandardCoercion),
            Some(2.0)
        );
        assert_eq!(
            extract_number(&numeric_string, InputPolicy::NumberOnly),
            None
        );
        assert!(extract_number(&Value::Number(f64::NAN), InputPolicy::NumberOnly).is_some());
    }

    #[test]
    fn table_driven_domain_boundaries() {
        enum Check {
            Finite,
            Count,
            Index,
            Exact,
            Duration,
            Exit,
            Timestamp,
            LoopBound,
            LoopStep,
        }
        let cases = [
            (Check::Finite, 1.5, true),
            (Check::Finite, f64::NAN, false),
            (Check::Count, 0.0, true),
            (Check::Count, 10.0, true),
            (Check::Count, -1.0, false),
            (Check::Count, 1.5, false),
            (Check::Count, f64::INFINITY, false),
            (Check::Index, -3.0, true),
            (Check::Index, 3.5, false),
            (Check::Index, f64::NAN, false),
            (Check::Exact, -2.0, true),
            (Check::Exact, 3.0, false),
            (Check::Duration, 0.25, true),
            (Check::Duration, -0.25, false),
            (Check::Duration, f64::INFINITY, false),
            (Check::Exit, 0.0, true),
            (Check::Exit, 255.0, true),
            (Check::Exit, 256.0, false),
            (Check::Exit, -1.0, false),
            (Check::Timestamp, 1_700_000_000.0, true),
            (Check::Timestamp, f64::NAN, false),
            (Check::LoopBound, -1.25, true),
            (Check::LoopBound, f64::INFINITY, false),
            (Check::LoopStep, 0.5, true),
            (Check::LoopStep, 0.0, false),
            (Check::LoopStep, f64::NAN, false),
        ];
        for (check, value, valid) in cases {
            let result = match check {
                Check::Finite => as_finite_number("test", value).map(|_| ()),
                Check::Count => as_count("test", value, 10).map(|_| ()),
                Check::Index => as_signed_index("test", value).map(|_| ()),
                Check::Exact => as_exact_integer("test", value, -2, 2).map(|_| ()),
                Check::Duration => as_duration("test", value).map(|_| ()),
                Check::Exit => as_exit_code("test", value).map(|_| ()),
                Check::Timestamp => as_timestamp("test", value).map(|_| ()),
                Check::LoopBound => as_loop_bound("test", value).map(|_| ()),
                Check::LoopStep => as_loop_step("test", value).map(|_| ()),
            };
            assert_eq!(result.is_ok(), valid, "value={value}");
            if let Err(err) = result {
                assert_eq!(code(err), "VALUE_OUT_OF_RANGE");
            }
        }
    }

    #[test]
    fn domain_error_details_have_the_stable_shape() {
        let err = as_count("repeat(): argument 2", -1.0, 100).unwrap_err();
        let info = err.info().unwrap();
        let Value::Map(details) = &info.details else {
            panic!("details must be a map")
        };
        for key in ["context", "value", "expected", "min", "max"] {
            assert!(details.contains_key(key), "missing {key}: {details:?}");
        }
    }

    /// details.max reports the largest value the domain ADMITS. For a domain
    /// declared `..=i64::MAX`, `i64::MAX as f64` rounds up to 2^63 — a value
    /// the validator itself rejects — so the rendering clamps to the largest
    /// f64 below 2^63 instead of lying by one ulp.
    #[test]
    fn exact_integer_details_max_is_the_largest_admitted_value() {
        let err = as_exact_integer("t", 1e19, 0, i64::MAX).unwrap_err();
        let info = err.info().unwrap();
        let Value::Map(details) = &info.details else {
            panic!("details must be a map")
        };
        assert_eq!(
            details.get("max"),
            Some(&Value::Number(9_223_372_036_854_774_784.0))
        );
        // And that reported max is genuinely admitted.
        assert!(as_exact_integer("t", 9_223_372_036_854_774_784.0, 0, i64::MAX).is_ok());
    }
}
