//! Lenient JSON field reading for records the plugin doesn't own (herdr's
//! output, agent session registries).

use serde::Deserialize;
use serde::de::{DeserializeOwned, Deserializer};
use serde_json::Value;

/// Reads a field that may hold anything. A value of the wrong type becomes
/// `None` instead of an error, so one odd field never discards the record
/// around it. Use with `#[serde(default, deserialize_with = "lenient")]`.
pub fn lenient<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    Ok(serde_json::from_value(Value::deserialize(d)?).ok())
}
