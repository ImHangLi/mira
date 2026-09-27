//! The plugin JSON Schema profile: Draft 2020-12, local `#` references only,
//! ≤256 KiB, structure depth ≤32, object roots. No network or file resolution.

use serde_json::{Map, Value};

use crate::error::{ErrorCode, Issue, Issues, pointer_token};
use crate::limits::{MAX_SCHEMA_BYTES, MAX_SCHEMA_DEPTH};
use crate::strict_json;

const DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

/// A JSON Schema document that passed the profile checks and compiles.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaDoc(Map<String, Value>);

impl SchemaDoc {
    /// The default action input schema: a strict empty object.
    pub fn empty_object() -> Self {
        let mut m = Map::new();
        m.insert("type".into(), Value::String("object".into()));
        m.insert("additionalProperties".into(), Value::Bool(false));
        Self(m)
    }

    /// Checks the profile. `pointer` locates the schema inside its manifest.
    pub fn check(
        value: &Map<String, Value>,
        pointer: &str,
        object_root: bool,
    ) -> Result<Self, Issues> {
        let mut issues = Issues::default();
        let size = serde_json::to_vec(value)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if size > MAX_SCHEMA_BYTES {
            issues.push(Issue::schema(pointer, "schema exceeds 256 KiB"));
        }
        let as_value = Value::Object(value.clone());
        if strict_json::depth(&as_value) > MAX_SCHEMA_DEPTH {
            issues.push(Issue::schema(pointer, "schema structure is deeper than 32"));
        }
        if let Some(s) = value.get("$schema")
            && s.as_str() != Some(DRAFT_2020_12)
        {
            issues.push(Issue::schema(
                format!("{pointer}/$schema"),
                "only JSON Schema Draft 2020-12 is supported",
            ));
        }
        walk_refs(&as_value, pointer, &mut issues);
        if object_root && value.get("type").and_then(Value::as_str) != Some("object") {
            issues.push(Issue::schema(
                format!("{pointer}/type"),
                "the root of an input schema must be \"type\": \"object\"",
            ));
        }
        if issues.is_empty()
            && let Err(e) = jsonschema::draft202012::new(&as_value)
        {
            issues.push(Issue::schema(pointer, format!("invalid JSON Schema: {e}")));
        }
        issues.into_result(Self(value.clone()))
    }

    pub fn as_map(&self) -> &Map<String, Value> {
        &self.0
    }

    pub fn to_value(&self) -> Value {
        Value::Object(self.0.clone())
    }

    /// Compiles a validator. Callers cache it by the schema's canonical hash.
    pub fn compile(&self) -> Result<jsonschema::Validator, String> {
        jsonschema::draft202012::new(&self.to_value()).map_err(|e| e.to_string())
    }

    /// Top-level property names marked `writeOnly: true`; never echoed in logs or errors.
    pub fn write_only_fields(&self) -> Vec<String> {
        self.properties()
            .map(|props| {
                props
                    .iter()
                    .filter(|(_, s)| s.get("writeOnly").and_then(Value::as_bool) == Some(true))
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// True when `name` is a top-level property of this schema.
    pub fn has_property(&self, name: &str) -> bool {
        self.properties().is_some_and(|p| p.contains_key(name))
    }

    fn properties(&self) -> Option<&Map<String, Value>> {
        self.0.get("properties").and_then(Value::as_object)
    }

    /// The single effective-input rule shared by CLI and TUI: fill missing top-level
    /// properties that declare a `default`. Nested and `oneOf`/`anyOf` defaults are not guessed.
    pub fn effective_input(&self, input: &Map<String, Value>) -> Map<String, Value> {
        let mut out = input.clone();
        if let Some(props) = self.properties() {
            for (name, prop) in props {
                if !out.contains_key(name)
                    && let Some(d) = prop.get("default")
                {
                    out.insert(name.clone(), d.clone());
                }
            }
        }
        out
    }

    /// Validates an instance, hiding values of write-only fields in messages.
    pub fn validate(
        &self,
        validator: &jsonschema::Validator,
        instance: &Value,
        pointer: &str,
    ) -> Issues {
        let hidden = self.write_only_fields();
        let mut issues = Issues::default();
        for e in validator
            .iter_errors(instance)
            .take(crate::error::MAX_REPORTED_ISSUES)
        {
            let mut at = e.instance_path().to_string();
            // A missing property is located at the property itself, not at its parent.
            if let jsonschema::error::ValidationErrorKind::Required {
                property: Value::String(name),
            } = e.kind()
            {
                at = format!("{at}/{}", pointer_token(name));
            }
            let top = at.trim_start_matches('/').split('/').next().unwrap_or("");
            let message = if hidden.iter().any(|h| h == top) {
                "value does not match the schema (write-only field hidden)".to_owned()
            } else {
                e.to_string()
            };
            issues.push(Issue::new(
                ErrorCode::SCHEMA_INVALID,
                format!("{pointer}{at}"),
                message,
            ));
        }
        issues
    }
}

fn walk_refs(v: &Value, pointer: &str, issues: &mut Issues) {
    match v {
        Value::Object(m) => {
            for (k, child) in m {
                let here = format!("{pointer}/{}", pointer_token(k));
                match k.as_str() {
                    "$ref" | "$dynamicRef" | "$recursiveRef" => {
                        if !child.as_str().is_some_and(|s| s.starts_with('#')) {
                            issues.push(Issue::schema(
                                here.clone(),
                                "only local `#...` references are allowed",
                            ));
                        }
                    }
                    "$id" => issues.push(Issue::schema(
                        here.clone(),
                        "`$id` is not allowed; use `$defs` and local references",
                    )),
                    _ => {}
                }
                walk_refs(child, &here, issues);
            }
        }
        Value::Array(a) => {
            for (i, child) in a.iter().enumerate() {
                walk_refs(child, &format!("{pointer}/{i}"), issues);
            }
        }
        _ => {}
    }
}
