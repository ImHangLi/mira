//! Strict JSON decoding: no BOM, no duplicate keys, bounded depth, and
//! integers limited to ±(2^53-1). `NaN`/`Infinity` are already invalid JSON.

use std::fmt;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

use crate::limits::{MAX_JSON_DEPTH, MAX_SAFE_INTEGER};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JsonError {
    #[error("input starts with a UTF-8 byte order mark")]
    Bom,
    #[error("input is empty")]
    Empty,
    #[error("input exceeds {0} bytes")]
    TooLarge(usize),
    #[error("{0}")]
    Invalid(String),
}

/// Parses one complete JSON document with the strict profile.
pub fn parse(bytes: &[u8], max_bytes: usize) -> Result<Value, JsonError> {
    parse_with_depth(bytes, max_bytes, MAX_JSON_DEPTH)
}

pub fn parse_with_depth(
    bytes: &[u8],
    max_bytes: usize,
    max_depth: usize,
) -> Result<Value, JsonError> {
    if bytes.len() > max_bytes {
        return Err(JsonError::TooLarge(max_bytes));
    }
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Err(JsonError::Bom);
    }
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(JsonError::Empty);
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| JsonError::Invalid("input is not valid UTF-8".into()))?;
    let mut de = serde_json::Deserializer::from_str(text);
    let value = Seed {
        depth: 1,
        max_depth,
    }
    .deserialize(&mut de)
    .map_err(|e| JsonError::Invalid(e.to_string()))?;
    de.end()
        .map_err(|e| JsonError::Invalid(format!("trailing content: {e}")))?;
    Ok(value)
}

/// Depth of a JSON value (scalars have depth 1).
pub fn depth(v: &Value) -> usize {
    match v {
        Value::Array(a) => 1 + a.iter().map(depth).max().unwrap_or(0),
        Value::Object(o) => 1 + o.values().map(depth).max().unwrap_or(0),
        _ => 1,
    }
}

#[derive(Clone, Copy)]
struct Seed {
    depth: usize,
    max_depth: usize,
}

impl<'de> DeserializeSeed<'de> for Seed {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        if self.depth > self.max_depth {
            return Err(de::Error::custom(format!(
                "nesting deeper than {}",
                self.max_depth
            )));
        }
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Seed {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }
    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        if v > MAX_SAFE_INTEGER {
            return Err(E::custom(
                "integer outside the interoperable range ±(2^53-1)",
            ));
        }
        Ok(Value::Number(v.into()))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        if v.unsigned_abs() > MAX_SAFE_INTEGER {
            return Err(E::custom(
                "integer outside the interoperable range ±(2^53-1)",
            ));
        }
        Ok(Value::Number(v.into()))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite number"))
    }
    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }
    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let inner = Seed {
            depth: self.depth + 1,
            ..self
        };
        let mut out = Vec::new();
        while let Some(v) = seq.next_element_seed(inner)? {
            out.push(v);
        }
        Ok(Value::Array(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let inner = Seed {
            depth: self.depth + 1,
            ..self
        };
        let mut out = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if out.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate object key `{key}`")));
            }
            let v = map.next_value_seed(inner)?;
            out.insert(key, v);
        }
        Ok(Value::Object(out))
    }
}
