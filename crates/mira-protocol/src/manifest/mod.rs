//! Workspace, plugin, and local manifests. Wire DTOs are strict (`deny_unknown_fields`);
//! domain types are only produced by the validators in this module.

mod domain;
mod validate;
mod wire;

use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

pub use domain::*;
pub use validate::{parse_wire, validate_plugin, validate_workspace, wire_from_value};
pub use wire::*;

pub type JsonObject = Map<String, Value>;

/// Field deserializer for optional fields: missing → `None` (via `#[serde(default)]`),
/// explicit `null` → error, because `T` itself rejects null.
pub fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

/// RFC 7396 JSON Merge Patch: objects merge recursively, arrays replace, null deletes.
pub fn merge_patch(target: &mut Value, patch: &Value) {
    match patch {
        Value::Object(p) => {
            if !target.is_object() {
                *target = Value::Object(Map::new());
            }
            if let Value::Object(t) = target {
                for (k, v) in p {
                    if v.is_null() {
                        t.remove(k);
                    } else {
                        merge_patch(t.entry(k.clone()).or_insert(Value::Null), v);
                    }
                }
            }
        }
        other => *target = other.clone(),
    }
}
