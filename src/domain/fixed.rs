//! 倍率的定点表示（§20.3）。
//!
//! 倍率的硬性比较绝不能使用浮点数：`0.1 * 3 > 0.3` 这类误差会让"有效倍率
//! 等于分组上限"的请求被错误拒绝。这里用放大 10^6 倍的整数表示，乘法在定点
//! 域内完成并向上取整——对成本保护取保守方向。

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// 定点缩放因子，保留 6 位小数。
pub const SCALE: i64 = 1_000_000;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MultiplierError {
    #[error("倍率不是合法数值：{0}")]
    Malformed(String),
    #[error("倍率不能为负数")]
    Negative,
    #[error("倍率超出可表示范围")]
    OutOfRange,
    #[error("倍率最多保留 6 位小数")]
    TooPrecise,
}

/// 放大 10^6 倍保存的倍率。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Multiplier(i64);

impl Multiplier {
    pub const ONE: Self = Self(SCALE);
    pub const ZERO: Self = Self(0);

    /// 从数据库中的原始定点整数构造。
    pub fn from_raw(raw: i64) -> Self {
        Self(raw)
    }

    /// 取出定点整数，用于持久化。
    pub fn raw(self) -> i64 {
        self.0
    }

    /// 解析人类输入的十进制倍率，例如 `"0.5"`、`"1"`、`"1.250000"`。
    pub fn parse(raw: &str) -> Result<Self, MultiplierError> {
        let text = raw.trim();
        if text.is_empty() {
            return Err(MultiplierError::Malformed(raw.to_string()));
        }
        if text.starts_with('-') {
            return Err(MultiplierError::Negative);
        }
        let (int_part, frac_part) = match text.split_once('.') {
            Some((i, f)) => (i, f),
            None => (text, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(MultiplierError::Malformed(raw.to_string()));
        }
        if !int_part.chars().all(|c| c.is_ascii_digit())
            || !frac_part.chars().all(|c| c.is_ascii_digit())
        {
            return Err(MultiplierError::Malformed(raw.to_string()));
        }
        if frac_part.len() > 6 {
            return Err(MultiplierError::TooPrecise);
        }

        let units: i64 = if int_part.is_empty() {
            0
        } else {
            int_part.parse().map_err(|_| MultiplierError::OutOfRange)?
        };
        let mut fraction: i64 = if frac_part.is_empty() {
            0
        } else {
            frac_part.parse().unwrap_or(0)
        };
        for _ in frac_part.len()..6 {
            fraction *= 10;
        }
        units
            .checked_mul(SCALE)
            .and_then(|v| v.checked_add(fraction))
            .map(Self)
            .ok_or(MultiplierError::OutOfRange)
    }

    /// 定点乘法，向上取整。用于 `有效倍率 = 上游倍率 × 校准系数`。
    ///
    /// 向上取整而非四舍五入：宁可把成本估高一点被倍率门拦住，也不要估低
    /// 一点越过分组上限。
    pub fn mul_ceil(self, other: Self) -> Self {
        let product = (self.0 as i128) * (other.0 as i128);
        let scale = SCALE as i128;
        let rounded = product.div_euclid(scale) + i128::from(product.rem_euclid(scale) != 0);
        Self(rounded.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
    }

    /// 以浮点形式给出，仅用于展示与层内评分，绝不用于硬性比较。
    pub fn to_f64(self) -> f64 {
        self.0 as f64 / SCALE as f64
    }
}

impl fmt::Display for Multiplier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let units = self.0 / SCALE;
        let fraction = (self.0 % SCALE).abs();
        if fraction == 0 {
            write!(f, "{units}")
        } else {
            let text = format!("{fraction:06}");
            write!(f, "{units}.{}", text.trim_end_matches('0'))
        }
    }
}

impl Serialize for Multiplier {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Multiplier {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(deserializer)?;
        let text = match &raw {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            other => {
                return Err(serde::de::Error::custom(format!(
                    "倍率必须是字符串或数值，收到 {other}"
                )));
            }
        };
        Multiplier::parse(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        assert_eq!(Multiplier::parse("1").unwrap(), Multiplier::ONE);
        assert_eq!(Multiplier::parse("0.5").unwrap().raw(), 500_000);
        assert_eq!(Multiplier::parse(" 1.250000 ").unwrap().raw(), 1_250_000);
        assert_eq!(Multiplier::parse(".5").unwrap().raw(), 500_000);
    }

    #[test]
    fn rejects_invalid_input() {
        assert_eq!(Multiplier::parse("-1"), Err(MultiplierError::Negative));
        assert_eq!(
            Multiplier::parse("abc"),
            Err(MultiplierError::Malformed("abc".into()))
        );
        assert_eq!(
            Multiplier::parse("0.1234567"),
            Err(MultiplierError::TooPrecise)
        );
    }

    #[test]
    fn equality_at_the_group_limit_is_exact() {
        // §26.4：有效倍率等于分组上限必须允许调用。
        let limit = Multiplier::parse("0.3").unwrap();
        let effective = Multiplier::parse("0.1")
            .unwrap()
            .mul_ceil(Multiplier::parse("3").unwrap());
        assert!(effective <= limit);
        assert_eq!(effective, limit);
    }

    #[test]
    fn multiplication_rounds_up_toward_cost_safety() {
        // 0.333333 × 0.5 = 0.1666665，向上取整到 0.166667。
        let product = Multiplier::parse("0.333333")
            .unwrap()
            .mul_ceil(Multiplier::parse("0.5").unwrap());
        assert_eq!(product.raw(), 166_667);
    }

    #[test]
    fn identity_multiplication_is_lossless() {
        let value = Multiplier::parse("0.823456").unwrap();
        assert_eq!(value.mul_ceil(Multiplier::ONE), value);
    }

    #[test]
    fn display_is_stable() {
        assert_eq!(Multiplier::parse("1").unwrap().to_string(), "1");
        assert_eq!(Multiplier::parse("0.50").unwrap().to_string(), "0.5");
        assert_eq!(Multiplier::parse("1.234500").unwrap().to_string(), "1.2345");
    }
}
