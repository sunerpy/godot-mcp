use serde::{Deserialize, Deserializer, de};
use serde_json::Value;

pub(crate) fn non_negative_whole_u64(value: &Value) -> Option<u64> {
    let number = value.as_number()?;
    number.as_u64().or_else(|| {
        let value = number.as_f64()?;
        let upper_exclusive = (u64::MAX as f64) + 1.0;
        (value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value < upper_exclusive)
            .then_some(value as u64)
    })
}

pub(crate) fn deserialize_non_negative_whole_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    non_negative_whole_u64(&value).ok_or_else(|| {
        de::Error::custom("expected a finite, non-negative whole number that fits u64")
    })
}
