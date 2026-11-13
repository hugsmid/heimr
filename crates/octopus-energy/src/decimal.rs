//! Kraken `Decimal` scalars arrive as JSON strings (`"123.4"`), sometimes as
//! numbers. Accept both.

use serde::{Deserialize, Deserializer};

#[derive(Deserialize)]
#[serde(untagged)]
enum StrOrNum {
    Num(f64),
    Str(String),
}

pub(crate) fn opt_f64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    match Option::<StrOrNum>::deserialize(d)? {
        None => Ok(None),
        Some(StrOrNum::Num(n)) => Ok(Some(n)),
        Some(StrOrNum::Str(s)) if s.trim().is_empty() => Ok(None),
        Some(StrOrNum::Str(s)) => s
            .trim()
            .parse::<f64>()
            .map(Some)
            .map_err(|e| serde::de::Error::custom(format!("decimal {s:?}: {e}"))),
    }
}
