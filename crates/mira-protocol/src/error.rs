//! Public error codes, `ErrorInfo`, validation issues, and CLI exit codes.

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::limits::{MAX_ERROR_DETAILS_BYTES, MAX_ERROR_MESSAGE_BYTES};

/// Error code matching `^[A-Z][A-Z0-9_]{0,63}$`. Standard codes are associated constants;
/// plugins may use their own codes of the same form.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ErrorCode(Cow<'static, str>);

macro_rules! codes {
    ($($name:ident => $exit:expr),* $(,)?) => {
        impl ErrorCode {
            $(pub const $name: ErrorCode = ErrorCode(Cow::Borrowed(stringify!($name)));)*
            /// Every standard code, in contract order.
            pub const STANDARD: &'static [ErrorCode] = &[$(ErrorCode::$name),*];
            /// Fixed CLI exit code for this error class. Unknown (plugin) codes map to 5.
            pub fn exit_code(&self) -> u8 {
                match self.0.as_ref() {
                    $(stringify!($name) => $exit,)*
                    _ => 5,
                }
            }
        }
    };
}

codes! {
    INVALID_ARGUMENT => 2, SCHEMA_INVALID => 2, INVALID_FRAME => 2, FRAME_TOO_LARGE => 2,
    TRUNCATED_FRAME => 2, UNSUPPORTED_API => 4, PROTOCOL_MISMATCH => 4, NOT_FOUND => 3,
    NEEDS_PROJECT => 3, TTY_REQUIRED => 2, NOT_SETUP => 3, SESSION_REQUIRED => 4, BUSY => 4,
    ALREADY_RUNNING_DIFFERENT_INPUT => 4, REVISION_CONFLICT => 4, VIEW_CHANGED => 4,
    INPUT_BUSY => 4, ITEM_ID_CONFLICT => 4, CURSOR_EXPIRED => 4, RESET_REQUIRED => 4,
    PAYLOAD_GONE => 3, REQUEST_KEY_CONFLICT => 4, OUTCOME_UNKNOWN => 4, EXECUTION_FAILED => 5,
    TIMEOUT => 6, CANCELLED => 130, STORAGE_UNAVAILABLE => 8, CONFIG_APPLY_INCOMPLETE => 8,
    SCREEN_CHANGED => 4, PERMISSION_DENIED => 4,
    UNSUPPORTED_PATH_ENCODING => 2, WORKSPACE_ID_COLLISION => 4, COUNTER_EXHAUSTED => 7,
    INTERNAL => 7,
}

fn valid_code(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_uppercase()
        && b.iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

impl ErrorCode {
    pub fn parse(s: String) -> Result<Self, &'static str> {
        if valid_code(&s) {
            Ok(Self(Cow::Owned(s)))
        } else {
            Err("error code must match ^[A-Z][A-Z0-9_]{0,63}$")
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl Serialize for ErrorCode {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for ErrorCode {
    fn schema_name() -> Cow<'static, str> {
        "ErrorCode".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "string", "pattern": "^[A-Z][A-Z0-9_]{0,63}$"})
    }
}

/// A suggested follow-up command. Never executed automatically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NextAction {
    pub argv: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ErrorInfo {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::manifest::present"
    )]
    #[schemars(with = "String")]
    pub pointer: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::manifest::present"
    )]
    #[schemars(with = "Map<String, Value>")]
    pub details: Option<Map<String, Value>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::manifest::present"
    )]
    #[schemars(with = "NextAction")]
    pub next_action: Option<NextAction>,
}

impl ErrorInfo {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        truncate_utf8(&mut message, MAX_ERROR_MESSAGE_BYTES);
        Self {
            code,
            message,
            retryable: false,
            pointer: None,
            details: None,
            next_action: None,
        }
    }
    /// NOT_SETUP: the workspace has no `.mira/workspace.json`, with the setup steps.
    pub fn not_setup() -> Self {
        Self::new(
            ErrorCode::NOT_SETUP,
            "this workspace has no .mira/workspace.json yet",
        )
        .with_next_action(&["mira", "validate", ".mira"], NOT_SETUP_HINT)
    }
    /// NOT_FOUND for a catalog item. `kind` is `catalog item`, `action`, or `view`;
    /// `details.item_ref` names the missing item.
    pub fn item_not_found(kind: &str, item_ref: impl fmt::Display) -> Self {
        let item_ref = item_ref.to_string();
        let message = format!("no {kind} `{item_ref}`");
        let mut details = Map::new();
        details.insert("item_ref".into(), Value::String(item_ref));
        Self::new(ErrorCode::NOT_FOUND, message).with_details(details)
    }
    /// NOT_FOUND for a run; `details.run_id` names the missing run.
    pub fn run_not_found(run_id: impl fmt::Display) -> Self {
        let run_id = run_id.to_string();
        let message = format!("no run {run_id}");
        let mut details = Map::new();
        details.insert("run_id".into(), Value::String(run_id));
        Self::new(ErrorCode::NOT_FOUND, message).with_details(details)
    }
    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }
    pub fn with_pointer(mut self, pointer: impl Into<String>) -> Self {
        self.pointer = Some(pointer.into());
        self
    }
    /// Attaches details; oversized details are replaced by a size note.
    pub fn with_details(mut self, details: Map<String, Value>) -> Self {
        let size = serde_json::to_vec(&details)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        self.details = if size <= MAX_ERROR_DETAILS_BYTES {
            Some(details)
        } else {
            let mut m = Map::new();
            m.insert(
                "omitted".into(),
                Value::String(format!("details exceeded {MAX_ERROR_DETAILS_BYTES} bytes")),
            );
            Some(m)
        };
        self
    }
    pub fn with_next_action(mut self, argv: &[&str], reason: impl Into<String>) -> Self {
        self.next_action = Some(NextAction {
            argv: argv.iter().map(|s| (*s).to_owned()).collect(),
            reason: reason.into(),
        });
        self
    }
    /// Checks the byte limits of an ErrorInfo received from a plugin.
    pub fn check_limits(&self) -> Result<(), &'static str> {
        if self.message.len() > MAX_ERROR_MESSAGE_BYTES {
            return Err("error.message exceeds 4096 bytes");
        }
        if let Some(d) = &self.details
            && serde_json::to_vec(d).map(|v| v.len()).unwrap_or(usize::MAX)
                > MAX_ERROR_DETAILS_BYTES
        {
            return Err("error.details exceeds 8 KiB");
        }
        Ok(())
    }
}

/// How to set up a workspace that has no `.mira/workspace.json`.
pub const NOT_SETUP_HINT: &str = "Write .mira/workspace.json and a plugin in .mira/plugins/ \
     (mira skill, setup reference), validate them, then run `mira reload`.";

/// Cuts a string to at most `max` bytes on a character boundary.
pub fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

/// One validation finding with a JSON Pointer into the offending document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Issue {
    pub code: ErrorCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub pointer: String,
    pub message: String,
}

impl Issue {
    pub fn new(code: ErrorCode, pointer: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            file: None,
            pointer: pointer.into(),
            message: message.into(),
        }
    }
    pub fn schema(pointer: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(ErrorCode::SCHEMA_INVALID, pointer, message)
    }
}

/// A non-empty, ordered list of validation issues.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Issues(pub Vec<Issue>);

pub const MAX_REPORTED_ISSUES: usize = 50;

impl Issues {
    pub fn push(&mut self, issue: Issue) {
        self.0.push(issue);
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn extend(&mut self, other: Issues) {
        self.0.extend(other.0);
    }
    pub fn in_file(mut self, file: &str) -> Self {
        for i in &mut self.0 {
            if i.file.is_none() {
                i.file = Some(file.to_owned());
            }
        }
        self
    }
    /// Ok when empty; otherwise the issues become the error.
    pub fn into_result<T>(self, value: T) -> Result<T, Issues> {
        if self.0.is_empty() {
            Ok(value)
        } else {
            Err(self)
        }
    }
    /// Summarizes the issues as one public error. The first issue decides the code.
    pub fn to_error_info(&self) -> ErrorInfo {
        let Some(first) = self.0.first() else {
            return ErrorInfo::new(ErrorCode::INTERNAL, "validation failed without issues");
        };
        let at = if first.pointer.is_empty() {
            String::new()
        } else {
            format!(" at `{}`", first.pointer)
        };
        let message = match &first.file {
            Some(f) => format!("{f}: {}{at}", first.message),
            None => format!("{}{at}", first.message),
        };
        let mut shown = Vec::new();
        let mut budget = MAX_ERROR_DETAILS_BYTES - 256;
        for issue in self.0.iter().take(MAX_REPORTED_ISSUES) {
            let Ok(v) = serde_json::to_value(issue) else {
                continue;
            };
            let size = v.to_string().len() + 1;
            if size > budget {
                break;
            }
            budget -= size;
            shown.push(v);
        }
        let mut details = Map::new();
        details.insert("issue_count".into(), Value::from(self.0.len()));
        details.insert("issues".into(), Value::Array(shown));
        ErrorInfo::new(first.code.clone(), message)
            .with_pointer(first.pointer.clone())
            .with_details(details)
    }
}

/// Escapes one JSON Pointer reference token (RFC 6901).
pub fn pointer_token(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}
