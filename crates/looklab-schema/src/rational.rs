// SPDX-License-Identifier: AGPL-3.0-or-later
//! Exact rational numbers for control step arithmetic.
//!
//! Control steps and values keep their written exact decimal value.

use core::cmp::Ordering;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer};

/// Constructors reduce the fraction; equality and ordering compare values.
#[derive(Debug, Clone, Copy)]
pub struct Rational {
    pub num: i64,
    /// Must be non-zero.
    pub den: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RationalError {
    #[error("empty number")]
    Empty,
    #[error("invalid number")]
    InvalidDigits,
    #[error("zero denominator")]
    ZeroDenominator,
    #[error("number out of range")]
    Overflow,
    #[error("step must be non-zero")]
    ZeroStep,
}

impl Rational {
    /// # Errors
    /// Returns [`RationalError::ZeroDenominator`] when `den` is zero.
    pub fn new(num: i64, den: u64) -> Result<Self, RationalError> {
        if den == 0 {
            return Err(RationalError::ZeroDenominator);
        }
        let g = gcd(num.unsigned_abs() as u128, den as u128);
        Ok(Self {
            num: (num as i128 / g as i128) as i64,
            den: (den as u128 / g) as u64,
        })
    }

    pub fn from_integer(n: i64) -> Self {
        Self { num: n, den: 1 }
    }

    /// Parses a decimal (`0.5`, `-2.0`) or a fraction (`1/3`).
    ///
    /// # Errors
    /// Rejects zero denominators, malformed digits and values that overflow the fields.
    pub fn parse(s: &str) -> Result<Self, RationalError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(RationalError::Empty);
        }
        if let Some((num, den)) = s.split_once('/') {
            if den.contains('/') {
                return Err(RationalError::InvalidDigits);
            }
            let num: i64 = num.parse().map_err(|_| RationalError::InvalidDigits)?;
            let den: u64 = den.parse().map_err(|_| RationalError::InvalidDigits)?;
            return Self::new(num, den);
        }
        Self::parse_decimal(s)
    }

    /// Parses a step, rejecting zero.
    ///
    /// # Errors
    /// As [`Rational::parse`], plus [`RationalError::ZeroStep`] for zero.
    pub fn parse_step(s: &str) -> Result<Self, RationalError> {
        let step = Self::parse(s)?;
        if step.is_zero() {
            return Err(RationalError::ZeroStep);
        }
        Ok(step)
    }

    fn parse_decimal(s: &str) -> Result<Self, RationalError> {
        let (negative, body) = match s.as_bytes()[0] {
            b'-' => (true, &s[1..]),
            b'+' => (false, &s[1..]),
            _ => (false, s),
        };
        if body.is_empty() {
            return Err(RationalError::Empty);
        }
        let (int_part, frac_part) = match body.split_once('.') {
            Some((int_part, frac_part)) if !frac_part.contains('.') => (int_part, frac_part),
            Some(_) => return Err(RationalError::InvalidDigits),
            None => (body, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(RationalError::Empty);
        }
        if !int_part.bytes().all(|b| b.is_ascii_digit())
            || !frac_part.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(RationalError::InvalidDigits);
        }
        let digits = format!("{int_part}{frac_part}");
        let magnitude: i128 = digits.parse().map_err(|_| RationalError::Overflow)?;
        let signed = if negative { -magnitude } else { magnitude };
        if signed < i64::MIN as i128 || signed > i64::MAX as i128 {
            return Err(RationalError::Overflow);
        }
        let den = 10u64
            .checked_pow(frac_part.len() as u32)
            .ok_or(RationalError::Overflow)?;
        Self::new(signed as i64, den)
    }

    pub fn is_zero(&self) -> bool {
        self.num == 0
    }

    pub fn div_exact(&self, divisor: &Self) -> Option<i64> {
        if divisor.num == 0 {
            return None;
        }
        let a = self.num as i128 * divisor.den as i128;
        let b = self.den as i128 * divisor.num as i128;
        if a % b != 0 {
            return None;
        }
        i64::try_from(a / b).ok()
    }

    pub fn checked_mul_i64(&self, factor: i64) -> Option<Self> {
        let num = (self.num as i128).checked_mul(factor as i128)?;
        if num < i64::MIN as i128 || num > i64::MAX as i128 {
            return None;
        }
        Self::new(num as i64, self.den).ok()
    }

    /// For display and fitting only.
    pub fn to_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }
}

impl PartialEq for Rational {
    fn eq(&self, other: &Self) -> bool {
        (self.num as i128) * (other.den as i128) == (other.num as i128) * (self.den as i128)
    }
}

impl Eq for Rational {}

impl PartialOrd for Rational {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Rational {
    fn cmp(&self, other: &Self) -> Ordering {
        ((self.num as i128) * (other.den as i128)).cmp(&((other.num as i128) * (self.den as i128)))
    }
}

impl Hash for Rational {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let g = gcd(self.num.unsigned_abs() as u128, self.den as u128) as i128;
        (self.num as i128 / g).hash(state);
        (self.den as u128 / g as u128).hash(state);
    }
}

impl fmt::Display for Rational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.den == 1 {
            write!(f, "{}", self.num)
        } else {
            write!(f, "{}/{}", self.num, self.den)
        }
    }
}

impl FromStr for Rational {
    type Err = RationalError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl<'de> Deserialize<'de> for Rational {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(RationalVisitor)
    }
}

struct RationalVisitor;

impl Visitor<'_> for RationalVisitor {
    type Value = Rational;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an exact decimal or fraction")
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        Rational::parse(&v.to_string()).map_err(E::custom)
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        Rational::parse(&v.to_string()).map_err(E::custom)
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
        if !v.is_finite() {
            return Err(E::custom("number is not finite"));
        }
        Rational::parse(&format!("{v}")).map_err(E::custom)
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Rational::parse(v).map_err(E::custom)
    }
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rational_steps() {
        assert_eq!(
            Rational::parse_step("1/3").unwrap(),
            Rational::new(1, 3).unwrap()
        );
        assert_eq!(
            Rational::parse_step("0.5").unwrap(),
            Rational::new(1, 2).unwrap()
        );
        assert_eq!(
            Rational::parse_step("0.25").unwrap(),
            Rational::new(1, 4).unwrap()
        );
        assert_eq!(
            Rational::parse_step("2/4").unwrap(),
            Rational::new(1, 2).unwrap()
        );
    }

    #[test]
    fn rejects_bad_steps() {
        assert_eq!(Rational::parse("0.1/3"), Err(RationalError::InvalidDigits));
        assert_eq!(Rational::parse_step("0"), Err(RationalError::ZeroStep));
        assert_eq!(Rational::parse("1/0"), Err(RationalError::ZeroDenominator));
    }
}
