//! Input forms built from an action's `input_schema` (§6.6, §8.3).
//!
//! Top-level string, number, integer, boolean, and enum properties get simple fields; every
//! other property (object, array, unions) gets a JSON field with a live syntax check. Empty
//! fields are left out, so the host fills declared defaults with the same effective-input
//! rule as the CLI, then validates. `writeOnly` values are masked and never echoed; the form
//! is dropped after a successful run.

use mira_protocol::error::ErrorInfo;
use mira_protocol::ids::ActionRef;
use mira_protocol::manifest::JsonObject;
use serde_json::{Map, Number, Value};

use crate::app::Intent;
use crate::logs::display;

#[derive(Clone, PartialEq)]
pub enum Kind {
    Text,
    Number,
    Integer,
    Boolean,
    Enum(Vec<Value>),
    Json,
}

pub struct Field {
    pub name: String,
    pub title: String,
    pub description: String,
    pub kind: Kind,
    pub required: bool,
    pub default: Option<Value>,
    pub write_only: bool,
    pub buf: String,
    /// Boolean (0 = false, 1 = true) or enum index; `None` leaves the field out.
    pub choice: Option<usize>,
    pub error: Option<String>,
}

pub struct Form {
    pub action_ref: ActionRef,
    pub intent: Intent,
    pub fields: Vec<Field>,
    pub focus: usize,
    pub pending: bool,
    pub error: Option<String>,
}

pub enum Outcome {
    Stay,
    Submit(Map<String, Value>),
    Cancel,
}

fn show(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

impl Field {
    /// What the field shows; write-only values are masked.
    pub fn shown(&self) -> String {
        match &self.kind {
            Kind::Boolean => match self.choice {
                Some(1) => "true".into(),
                Some(_) => "false".into(),
                None => String::new(),
            },
            Kind::Enum(vals) => self
                .choice
                .and_then(|i| vals.get(i))
                .map(show)
                .unwrap_or_default(),
            _ if self.write_only => "*".repeat(self.buf.chars().count()),
            // Pasted JSON may hold newlines; show them as spaces (valid JSON whitespace).
            _ => display(&self.buf),
        }
    }

    pub fn hint(&self) -> String {
        let mut h = Vec::new();
        match &self.kind {
            Kind::Boolean => h.push("Space: true/false/unset".to_owned()),
            Kind::Enum(vals) => h.push(format!(
                "Space/Left/Right: {} · Backspace: unset",
                vals.iter().map(show).collect::<Vec<_>>().join(" | ")
            )),
            Kind::Json => h.push("JSON".to_owned()),
            Kind::Number => h.push("number".to_owned()),
            Kind::Integer => h.push("integer".to_owned()),
            Kind::Text => {}
        }
        if let Some(d) = &self.default {
            if self.write_only {
                h.push("has a default".into());
            } else {
                h.push(format!("default {}", show(d)));
            }
        }
        if self.write_only {
            h.push("write-only, never shown".into());
        }
        h.join(" · ")
    }

    fn cycle(&mut self, forward: bool) {
        let n = match &self.kind {
            Kind::Boolean => {
                // unset → true → false → unset
                self.choice = match (self.choice, forward) {
                    (None, true) | (Some(0), false) => Some(1),
                    (Some(1), true) | (None, false) => Some(0),
                    _ => None,
                };
                self.error = None;
                return;
            }
            Kind::Enum(v) => v.len(),
            _ => return,
        };
        if n == 0 {
            return;
        }
        // The values form a ring. An unset field stands for its default, so it moves on
        // from the default's position; Backspace makes it unset again.
        let at = self.choice.or_else(|| match (&self.kind, &self.default) {
            (Kind::Enum(v), Some(d)) => v.iter().position(|x| x == d),
            _ => None,
        });
        self.choice = Some(match (at, forward) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
        });
        self.error = None;
    }

    fn text_like(&self) -> bool {
        !matches!(self.kind, Kind::Boolean | Kind::Enum(_))
    }

    /// The value to send, `Ok(None)` to leave the field out.
    fn value(&self) -> Result<Option<Value>, String> {
        match &self.kind {
            Kind::Boolean => Ok(self.choice.map(|c| Value::Bool(c == 1))),
            Kind::Enum(vals) => Ok(self.choice.and_then(|i| vals.get(i).cloned())),
            _ if self.buf.trim().is_empty() => Ok(None),
            Kind::Text => Ok(Some(Value::String(self.buf.clone()))),
            Kind::Integer => self
                .buf
                .trim()
                .parse::<i64>()
                .map(|n| Some(Value::from(n)))
                .map_err(|_| "enter a whole number".into()),
            Kind::Number => self
                .buf
                .trim()
                .parse::<f64>()
                .ok()
                .and_then(Number::from_f64)
                .map(|n| Some(Value::Number(n)))
                .ok_or_else(|| "enter a finite number".into()),
            Kind::Json => serde_json::from_str::<Value>(&self.buf)
                .map(Some)
                .map_err(|e| {
                    if self.write_only {
                        "invalid JSON".to_owned()
                    } else {
                        format!("invalid JSON at line {} column {}", e.line(), e.column())
                    }
                }),
        }
    }

    /// Live syntax check for JSON fields.
    fn check(&mut self) {
        if self.kind == Kind::Json {
            self.error = self.value().err();
        } else {
            self.error = None;
        }
    }
}

fn kind_of(prop: &Map<String, Value>) -> Kind {
    if let Some(Value::Array(vals)) = prop.get("enum")
        && !vals.is_empty()
    {
        return Kind::Enum(vals.clone());
    }
    match prop.get("type").and_then(Value::as_str) {
        Some("string") => Kind::Text,
        Some("integer") => Kind::Integer,
        Some("number") => Kind::Number,
        Some("boolean") => Kind::Boolean,
        _ => Kind::Json,
    }
}

/// Whether the schema declares any top-level property (a form is worth showing).
pub fn has_fields(schema: &JsonObject) -> bool {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|p| !p.is_empty())
}

pub fn required_names(schema: &JsonObject) -> Vec<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

impl Form {
    /// Builds the fields; `last` restores earlier visible (never write-only) values.
    pub fn new(
        action_ref: ActionRef,
        intent: Intent,
        schema: &JsonObject,
        last: Option<&Map<String, Value>>,
    ) -> Self {
        let required = required_names(schema);
        let mut fields = Vec::new();
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            for (name, prop) in props {
                let Some(prop) = prop.as_object() else {
                    continue;
                };
                let kind = kind_of(prop);
                let write_only = prop.get("writeOnly").and_then(Value::as_bool) == Some(true);
                let mut f = Field {
                    name: name.clone(),
                    title: prop
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or(name)
                        .to_owned(),
                    description: prop
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    kind,
                    required: required.contains(name),
                    default: prop.get("default").cloned(),
                    write_only,
                    buf: String::new(),
                    choice: None,
                    error: None,
                };
                if !write_only && let Some(v) = last.and_then(|l| l.get(name)) {
                    match &f.kind {
                        Kind::Boolean => f.choice = v.as_bool().map(usize::from),
                        Kind::Enum(vals) => f.choice = vals.iter().position(|x| x == v),
                        Kind::Json => {
                            f.buf = serde_json::to_string(v).unwrap_or_default();
                        }
                        _ => f.buf = show(v),
                    }
                }
                fields.push(f);
            }
        }
        // Required fields first, then schema order.
        fields.sort_by_key(|f| !f.required);
        Self {
            action_ref,
            intent,
            fields,
            focus: 0,
            pending: false,
            error: None,
        }
    }

    pub fn focused(&self) -> Option<&Field> {
        self.fields.get(self.focus)
    }

    /// Builds the input; marks local problems on the fields.
    fn input(&mut self) -> Option<Map<String, Value>> {
        let mut out = Map::new();
        let mut ok = true;
        for f in &mut self.fields {
            match f.value() {
                Ok(Some(v)) => {
                    out.insert(f.name.clone(), v);
                    f.error = None;
                }
                Ok(None) if f.required && f.default.is_none() => {
                    f.error = Some("required".into());
                    ok = false;
                }
                Ok(None) => f.error = None,
                Err(e) => {
                    f.error = Some(e);
                    ok = false;
                }
            }
        }
        if !ok {
            self.focus = self
                .fields
                .iter()
                .position(|f| f.error.is_some())
                .unwrap_or(0);
        }
        ok.then_some(out)
    }

    /// Visible values to remember for the next form of this action (never write-only).
    pub fn remembered(input: &Map<String, Value>, fields: &[Field]) -> Map<String, Value> {
        input
            .iter()
            .filter(|(k, _)| fields.iter().any(|f| &f.name == *k && !f.write_only))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn key(&mut self, k: crossterm::event::KeyEvent) -> Outcome {
        use crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if self.pending {
            return match k.code {
                KeyCode::Esc => Outcome::Cancel,
                KeyCode::Char('c') if ctrl => Outcome::Cancel,
                _ => Outcome::Stay,
            };
        }
        let n = self.fields.len();
        match k.code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Char('c') if ctrl => return Outcome::Cancel,
            KeyCode::Enter => {
                return match self.input() {
                    Some(input) => {
                        self.pending = true;
                        self.error = None;
                        Outcome::Submit(input)
                    }
                    None => Outcome::Stay,
                };
            }
            KeyCode::Tab | KeyCode::Down if n > 0 => self.focus = (self.focus + 1) % n,
            KeyCode::BackTab | KeyCode::Up if n > 0 => self.focus = (self.focus + n - 1) % n,
            _ => {}
        }
        let Some(f) = self.fields.get_mut(self.focus) else {
            return Outcome::Stay;
        };
        match k.code {
            KeyCode::Char(' ') if !f.text_like() => f.cycle(true),
            KeyCode::Right if !f.text_like() => f.cycle(true),
            KeyCode::Left if !f.text_like() => f.cycle(false),
            KeyCode::Char('u') if ctrl => {
                f.buf.clear();
                f.choice = None;
                f.check();
            }
            KeyCode::Backspace if f.text_like() => {
                f.buf.pop();
                f.check();
            }
            KeyCode::Backspace => {
                f.choice = None;
            }
            KeyCode::Char(c) if !ctrl && f.text_like() => {
                f.buf.push(c);
                f.check();
            }
            _ => {}
        }
        Outcome::Stay
    }

    pub fn paste(&mut self, text: &str) {
        if self.pending {
            return;
        }
        if let Some(f) = self.fields.get_mut(self.focus)
            && f.text_like()
        {
            // Terminals send CR for newlines inside a paste.
            f.buf
                .push_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
            f.check();
        }
    }

    /// Places the host's validation errors on their fields (`/input/<name>/...`).
    pub fn apply_error(&mut self, e: &ErrorInfo) {
        self.pending = false;
        let mut placed = false;
        let issues: Vec<(String, String)> = e
            .details
            .as_ref()
            .and_then(|d| d.get("issues"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|i| {
                        (
                            i.get("pointer")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                            i.get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_else(|| vec![(e.pointer.clone().unwrap_or_default(), e.message.clone())]);
        for (pointer, message) in issues {
            let rest = pointer.strip_prefix("/input").unwrap_or(&pointer);
            let mut parts = rest.trim_start_matches('/').splitn(2, '/');
            let top = parts
                .next()
                .unwrap_or("")
                .replace("~1", "/")
                .replace("~0", "~");
            let inner = parts.next().map(|p| format!("/{p}")).unwrap_or_default();
            if let Some(field) = self.fields.iter_mut().find(|f| f.name == top) {
                field.error = Some(if field.write_only {
                    "rejected by the schema (value hidden)".into()
                } else if inner.is_empty() {
                    message
                } else {
                    format!("at {inner}: {message}")
                });
                placed = true;
            }
        }
        if let Some(i) = self.fields.iter().position(|f| f.error.is_some()) {
            self.focus = i;
        }
        self.error = Some(if placed {
            format!(
                "[{}] the host rejected the input; see the marked fields",
                e.code
            )
        } else {
            format!("[{}] {}", e.code, e.message)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice_field(default: Option<&str>) -> Field {
        Field {
            name: "scope".into(),
            title: "Scope".into(),
            description: String::new(),
            kind: Kind::Enum(vec!["all".into(), "changed".into(), "staged".into()]),
            required: false,
            default: default.map(Value::from),
            write_only: false,
            buf: String::new(),
            choice: None,
            error: None,
        }
    }

    #[test]
    fn space_moves_to_the_next_choice() {
        let mut f = choice_field(Some("changed"));
        // Unset means the default, so the next value follows it.
        f.cycle(true);
        assert_eq!(f.shown(), "staged");
        // The last value wraps to the first, never to unset.
        f.cycle(true);
        assert_eq!(f.shown(), "all");
        f.cycle(true);
        assert_eq!(f.shown(), "changed");
        f.cycle(false);
        assert_eq!(f.shown(), "all");
        let mut f = choice_field(None);
        f.cycle(true);
        assert_eq!(f.shown(), "all");
    }
}
