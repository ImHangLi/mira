//! Workspace, plugin, and local manifests. Wire DTOs are strict (`deny_unknown_fields`);
//! domain types are only produced by the validators in this module.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::error::{ErrorCode, Issue, Issues, pointer_token};
use crate::hash::canonical_digest;
use crate::ids::{ActionId, ActionRef, Api1, Digest, ItemRef, PluginId, ViewId};
use crate::limits::*;
use crate::schema_profile::SchemaDoc;
use crate::strict_json;

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

// ---------------------------------------------------------------------------
// Wire DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActionMode {
    Task,
    Process,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TerminalMode {
    #[default]
    Pipe,
    Pty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopSignal {
    #[default]
    Term,
    Interrupt,
}

/// `{"kind":"none"}` or `{"kind":"after","ms":N}`. Also used for session TTLs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TimeoutWire {
    None,
    After { ms: u64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerWire {
    Command { argv: Vec<String> },
    Plugin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandRunnerWire {
    Command { argv: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleWire {
    pub every_ms: u64,
    #[serde(default)]
    pub params: JsonObject,
    #[serde(default)]
    pub run_on_start: bool,
}

fn default_cwd() -> String {
    ".".into()
}
fn default_stop_grace() -> u64 {
    DEFAULT_STOP_GRACE_MS
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionWire {
    pub id: ActionId,
    pub title: String,
    pub description: String,
    pub mode: ActionMode,
    pub run: RunnerWire,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default)]
    pub env_files: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "JsonObject")]
    pub input_schema: Option<JsonObject>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "JsonObject")]
    pub output_schema: Option<JsonObject>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "TimeoutWire")]
    pub timeout: Option<TimeoutWire>,
    #[serde(default)]
    pub terminal: TerminalMode,
    #[serde(default)]
    pub stop_signal: StopSignal,
    #[serde(default = "default_stop_grace")]
    pub stop_grace_ms: u64,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "CommandRunnerWire")]
    pub cleanup: Option<CommandRunnerWire>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "ScheduleWire")]
    pub schedule: Option<ScheduleWire>,
    #[serde(default)]
    pub effects: Vec<String>,
    #[serde(default)]
    pub meta: JsonObject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ViewKind {
    Text,
    Table,
    Log,
    Tree,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Persistence {
    #[default]
    Last,
    Session,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RowActionWire {
    pub action: ActionId,
    /// Top-level input parameter name → column ID. No expressions or interpolation.
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
}

/// Which output streams a derived log view keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceStream {
    Stdout,
    Stderr,
}

/// A log view the host derives from another action's run log; no plugin process runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewSourceWire {
    /// `PLUGIN.ACTION` whose current run (or latest run) supplies the lines.
    pub logs: ItemRef,
    /// Keep only lines that contain this text, ignoring case.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "String")]
    pub grep: Option<String>,
    /// Keep only lines from this stream.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "SourceStream")]
    pub stream: Option<SourceStream>,
}

impl ViewSourceWire {
    /// Human words for the filter: ` · filter "error" on stderr`, or empty.
    pub fn filter_words(&self) -> String {
        let stream = self.stream.map(|s| match s {
            SourceStream::Stdout => "stdout",
            SourceStream::Stderr => "stderr",
        });
        match (&self.grep, stream) {
            (Some(g), Some(s)) => format!(" · filter \"{g}\" on {s}"),
            (Some(g), None) => format!(" · filter \"{g}\""),
            (None, Some(s)) => format!(" · {s} only"),
            (None, None) => String::new(),
        }
    }
}

impl From<&ViewSource> for ViewSourceWire {
    fn from(s: &ViewSource) -> Self {
        Self {
            logs: s.logs.to_item_ref(),
            grep: s.grep.clone(),
            stream: s.stream,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewDefinitionWire {
    pub id: ViewId,
    pub title: String,
    pub kind: ViewKind,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub persistence: Persistence,
    #[serde(default)]
    pub row_actions: Vec<RowActionWire>,
    /// Log views only: derive the view from another action's run log.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "ViewSourceWire")]
    pub source: Option<ViewSourceWire>,
    #[serde(default)]
    pub meta: JsonObject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntryWire {
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginWire {
    pub api: Api1,
    pub id: PluginId,
    pub name: String,
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "EntryWire")]
    pub entry: Option<EntryWire>,
    #[serde(default)]
    pub actions: Vec<ActionWire>,
    #[serde(default)]
    pub views: Vec<ViewDefinitionWire>,
    #[serde(default)]
    pub config: JsonObject,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "JsonObject")]
    pub config_schema: Option<JsonObject>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "String")]
    pub docs: Option<String>,
    #[serde(default)]
    pub meta: JsonObject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UiTheme {
    #[default]
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UiWire {
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "UiTheme")]
    pub theme: Option<UiTheme>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "bool")]
    pub mouse: Option<bool>,
}

macro_rules! storage_policy {
    ($($field:ident: $default:expr, $min:expr, $max:expr;)*) => {
        /// Retention overrides. Every field is an optional positive integer.
        #[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct StoragePolicyWire {
            $(
                #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
                #[schemars(with = "u64")]
                pub $field: Option<u64>,
            )*
        }

        /// Resolved retention policy with contract defaults.
        #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
        pub struct StoragePolicy {
            $(pub $field: u64,)*
        }

        impl Default for StoragePolicy {
            fn default() -> Self {
                Self { $($field: $default,)* }
            }
        }

        impl StoragePolicy {
            /// Applies overrides in order (workspace, then local) after range checks.
            pub fn apply(&mut self, wire: &StoragePolicyWire, pointer: &str, issues: &mut Issues) {
                $(
                    if let Some(v) = wire.$field {
                        if ($min..=$max).contains(&v) {
                            self.$field = v;
                        } else {
                            issues.push(Issue::schema(
                                format!("{pointer}/{}", stringify!($field)),
                                format!("must be between {} and {}", $min, $max),
                            ));
                        }
                    }
                )*
            }
        }
    };
}

storage_policy! {
    history_days: 14, 1, 365;
    runs_per_action: 100, 1, 10_000;
    runs_per_workspace: 10_000, 100, 100_000;
    result_bytes_per_workspace: 64 * MIB as u64, MIB as u64, 512 * MIB as u64;
    log_days: 7, 1, 90;
    log_bytes_per_run: 8 * MIB as u64, MIB as u64, 256 * MIB as u64;
    log_bytes_per_workspace: 128 * MIB as u64, 8 * MIB as u64, 2048 * MIB as u64;
    view_days: 7, 1, 90;
    view_log_items: 1000, 10, 10_000;
    view_bytes_per_workspace: 32 * MIB as u64, MIB as u64, 256 * MIB as u64;
    artifact_days: 7, 1, 90;
    artifact_bytes_per_workspace: 256 * MIB as u64, MIB as u64, 4096 * MIB as u64;
    cache_days: 7, 1, 90;
    cache_bytes_per_workspace: 128 * MIB as u64, MIB as u64, 2048 * MIB as u64;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceWire {
    pub api: Api1,
    pub name: String,
    #[serde(default)]
    pub plugins: Vec<String>,
    #[serde(default)]
    pub autostart: Vec<ActionRef>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "UiWire")]
    pub ui: Option<UiWire>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "StoragePolicyWire")]
    pub storage: Option<StoragePolicyWire>,
    #[serde(default)]
    pub meta: JsonObject,
}

/// Personal overrides in `.mira/local.json`; never committed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalWire {
    pub api: Api1,
    /// Plugin ID → RFC 7396 JSON Merge Patch applied to that plugin manifest.
    #[serde(default)]
    pub plugin_patches: BTreeMap<PluginId, JsonObject>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "UiWire")]
    pub ui: Option<UiWire>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "StoragePolicyWire")]
    pub storage: Option<StoragePolicyWire>,
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// Non-empty argv without NUL, ≤64 KiB serialized. Executed directly, never via a shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Argv(Vec<String>);

impl Argv {
    pub fn parse(argv: Vec<String>, pointer: &str, issues: &mut Issues) -> Option<Self> {
        let before = issues.0.len();
        if argv.first().is_none_or(|a| a.is_empty()) {
            issues.push(Issue::schema(
                pointer,
                "argv needs a non-empty executable as its first element",
            ));
        }
        for (i, a) in argv.iter().enumerate() {
            if a.contains('\0') {
                issues.push(Issue::schema(
                    format!("{pointer}/{i}"),
                    "argv strings must not contain NUL",
                ));
            }
        }
        if serde_json::to_vec(&argv)
            .map(|v| v.len())
            .unwrap_or(usize::MAX)
            > MAX_ARGV_BYTES
        {
            issues.push(Issue::schema(pointer, "argv exceeds 64 KiB"));
        }
        (issues.0.len() == before).then_some(Self(argv))
    }
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
    pub fn program(&self) -> &str {
        self.0.first().map(String::as_str).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutPolicy {
    Unlimited,
    After(Duration),
}

impl TimeoutPolicy {
    pub fn resolve(mode: ActionMode, wire: Option<TimeoutWire>) -> Result<Self, &'static str> {
        let wire = wire.unwrap_or(match mode {
            ActionMode::Task => TimeoutWire::After {
                ms: DEFAULT_TASK_TIMEOUT_MS,
            },
            ActionMode::Process => TimeoutWire::None,
        });
        Self::from_wire(wire)
    }
    pub fn from_wire(wire: TimeoutWire) -> Result<Self, &'static str> {
        match wire {
            TimeoutWire::None => Ok(Self::Unlimited),
            TimeoutWire::After { ms } if (1..=TIMEOUT_AFTER_MAX_MS).contains(&ms) => {
                Ok(Self::After(Duration::from_millis(ms)))
            }
            TimeoutWire::After { .. } => Err("timeout.ms must be between 1 and 604800000"),
        }
    }
    pub fn to_wire(self) -> TimeoutWire {
        match self {
            Self::Unlimited => TimeoutWire::None,
            Self::After(d) => TimeoutWire::After {
                ms: d.as_millis() as u64,
            },
        }
    }
}

impl Serialize for TimeoutPolicy {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.to_wire().serialize(s)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Runner {
    Command { argv: Argv },
    Plugin,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Schedule {
    pub every_ms: u64,
    pub params: JsonObject,
    pub run_on_start: bool,
}

/// A validated action. `env` values may hold secrets: never log or echo them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Action {
    pub id: ActionId,
    pub title: String,
    pub description: String,
    pub mode: ActionMode,
    pub run: Runner,
    pub cwd: String,
    pub env_files: Vec<String>,
    #[serde(skip)]
    pub env: BTreeMap<String, String>,
    pub input_schema: SchemaDocSer,
    pub output_schema: Option<SchemaDocSer>,
    pub timeout: TimeoutPolicy,
    pub terminal: TerminalMode,
    pub stop_signal: StopSignal,
    pub stop_grace_ms: u64,
    pub cleanup: Option<Argv>,
    pub schedule: Option<Schedule>,
    pub effects: Vec<String>,
    pub meta: JsonObject,
    /// JCS hash of the normalized definition, env, and plugin config.
    #[serde(skip)]
    pub definition_hash: Digest,
}

/// Serializable wrapper so domain actions can be described without re-exposing internals.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaDocSer(pub SchemaDoc);
impl Serialize for SchemaDocSer {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.as_map().serialize(s)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RowAction {
    pub action: ActionId,
    pub bindings: BTreeMap<String, String>,
}

/// A validated log source. `logs` is checked against the whole catalog by `config`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ViewSource {
    pub logs: ActionRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grep: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<SourceStream>,
}

impl ViewSource {
    /// True when a log line from `stream` with `text` belongs in the view.
    pub fn keeps(&self, stream: crate::run::LogStream, text: &str) -> bool {
        use crate::run::LogStream;
        let stream_ok = match self.stream {
            None => true,
            Some(SourceStream::Stdout) => stream == LogStream::Stdout,
            Some(SourceStream::Stderr) => stream == LogStream::Stderr,
        };
        stream_ok
            && self
                .grep
                .as_deref()
                .is_none_or(|g| text.to_lowercase().contains(&g.to_lowercase()))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ViewDefinition {
    pub id: ViewId,
    pub title: String,
    pub kind: ViewKind,
    pub description: String,
    pub persistence: Persistence,
    pub row_actions: Vec<RowAction>,
    /// Set for log views the host derives from another action's run log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<ViewSource>,
    pub meta: JsonObject,
    /// Includes the definitions of actions referenced by `row_actions`.
    #[serde(skip)]
    pub definition_hash: Digest,
}

/// A validated plugin manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct Plugin {
    pub id: PluginId,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub tags: Vec<String>,
    pub entry: Option<Argv>,
    pub actions: Vec<Action>,
    pub views: Vec<ViewDefinition>,
    pub config: JsonObject,
    pub config_schema: Option<SchemaDoc>,
    pub docs: Option<String>,
    pub meta: JsonObject,
}

impl Plugin {
    pub fn action(&self, id: &str) -> Option<&Action> {
        self.actions.iter().find(|a| a.id.as_str() == id)
    }
    pub fn view(&self, id: &str) -> Option<&ViewDefinition> {
        self.views.iter().find(|v| v.id.as_str() == id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct UiPrefs {
    pub theme: UiTheme,
    pub mouse: bool,
}

impl UiPrefs {
    pub fn apply(&mut self, wire: &UiWire) {
        if let Some(t) = wire.theme {
            self.theme = t;
        }
        if let Some(m) = wire.mouse {
            self.mouse = m;
        }
    }
}

/// A validated workspace manifest. Cross-plugin references are checked by `config`.
#[derive(Debug, Clone, PartialEq)]
pub struct Workspace {
    pub name: String,
    pub plugins: Vec<String>,
    pub autostart: Vec<ActionRef>,
    pub ui: UiPrefs,
    pub storage_wire: Option<StoragePolicyWire>,
    pub meta: JsonObject,
}

// ---------------------------------------------------------------------------
// Parsing and validation
// ---------------------------------------------------------------------------

/// Bytes → strict JSON → wire DTO, with JSON Pointer locations for serde errors.
pub fn parse_wire<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
    max_bytes: usize,
) -> Result<T, Issues> {
    let value = strict_json::parse(bytes, max_bytes).map_err(|e| {
        let code = match e {
            strict_json::JsonError::TooLarge(_) => ErrorCode::FRAME_TOO_LARGE,
            _ => ErrorCode::SCHEMA_INVALID,
        };
        Issues(vec![Issue::new(code, "", e.to_string())])
    })?;
    wire_from_value(value)
}

/// JSON value → wire DTO. An `api` other than 1 reports UNSUPPORTED_API first.
pub fn wire_from_value<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, Issues> {
    if let Some(api) = value.get("api")
        && api.as_u64() != Some(1)
    {
        return Err(Issues(vec![Issue::new(
            ErrorCode::UNSUPPORTED_API,
            "/api",
            "unsupported api version; expected 1",
        )]));
    }
    serde_path_to_error::deserialize(value).map_err(|e| {
        let pointer = path_to_pointer(e.path());
        Issues(vec![Issue::schema(pointer, e.into_inner().to_string())])
    })
}

fn path_to_pointer(path: &serde_path_to_error::Path) -> String {
    use serde_path_to_error::Segment;
    let mut out = String::new();
    for seg in path.iter() {
        match seg {
            Segment::Seq { index } => out.push_str(&format!("/{index}")),
            Segment::Map { key } => out.push_str(&format!("/{}", pointer_token(key))),
            Segment::Enum { .. } | Segment::Unknown => {}
        }
    }
    out
}

fn check_text(s: &str, max: usize, required: bool, pointer: &str, issues: &mut Issues) {
    if required && s.is_empty() {
        issues.push(Issue::schema(pointer, "must not be empty"));
    }
    if s.len() > max {
        issues.push(Issue::schema(pointer, format!("exceeds {max} bytes")));
    }
}

fn check_object_size(m: &JsonObject, max: usize, pointer: &str, issues: &mut Issues) {
    if serde_json::to_vec(m).map(|v| v.len()).unwrap_or(usize::MAX) > max {
        issues.push(Issue::schema(pointer, format!("exceeds {max} bytes")));
    }
}

fn check_rel_path(s: &str, pointer: &str, allow_absolute: bool, issues: &mut Issues) {
    if s.is_empty() || s.contains('\0') {
        issues.push(Issue::schema(pointer, "path must be non-empty without NUL"));
    } else if s.starts_with('/') && !allow_absolute {
        issues.push(Issue::schema(pointer, "path must be relative"));
    }
}

fn check_env(env: &BTreeMap<String, String>, pointer: &str, issues: &mut Issues) {
    for (k, v) in env {
        let p = format!("{pointer}/{}", pointer_token(k));
        if k.is_empty() || k.contains('=') || k.contains('\0') {
            issues.push(Issue::schema(
                p.clone(),
                "invalid environment variable name",
            ));
        }
        if k.starts_with("MIRA_") {
            issues.push(Issue::schema(
                p.clone(),
                "MIRA_* variables are reserved by the host",
            ));
        }
        if v.contains('\0') {
            issues.push(Issue::schema(p, "environment values must not contain NUL"));
        }
    }
}

fn check_schema_doc(
    m: &Option<JsonObject>,
    pointer: &str,
    object_root: bool,
    issues: &mut Issues,
) -> Option<SchemaDoc> {
    let m = m.as_ref()?;
    match SchemaDoc::check(m, pointer, object_root) {
        Ok(doc) => Some(doc),
        Err(e) => {
            issues.extend(e);
            None
        }
    }
}

/// Validates a plugin wire DTO into the domain type.
pub fn validate_plugin(w: PluginWire) -> Result<Plugin, Issues> {
    let mut issues = Issues::default();
    check_text(&w.name, MAX_NAME_BYTES, true, "/name", &mut issues);
    check_text(
        &w.description,
        MAX_DESCRIPTION_BYTES,
        true,
        "/description",
        &mut issues,
    );
    if w.tags.len() > MAX_TAGS {
        issues.push(Issue::schema("/tags", "at most 12 tags"));
    }
    for (i, t) in w.tags.iter().enumerate() {
        check_text(t, MAX_TAG_BYTES, true, &format!("/tags/{i}"), &mut issues);
    }
    if w.actions.is_empty() && w.views.is_empty() {
        issues.push(Issue::schema(
            "",
            "a plugin needs at least one action or view",
        ));
    }
    if w.actions.len() > MAX_ACTIONS {
        issues.push(Issue::schema("/actions", "at most 128 actions"));
    }
    if w.views.len() > MAX_VIEWS {
        issues.push(Issue::schema("/views", "at most 64 views"));
    }
    let entry = w
        .entry
        .as_ref()
        .and_then(|e| Argv::parse(e.argv.clone(), "/entry/argv", &mut issues));
    let config_schema = check_schema_doc(&w.config_schema, "/config_schema", false, &mut issues);
    if let (Some(doc), true) = (&config_schema, issues.is_empty())
        && let Ok(v) = doc.compile()
    {
        issues.extend(doc.validate(&v, &Value::Object(w.config.clone()), "/config"));
    }
    check_object_size(&w.meta, MAX_META_BYTES, "/meta", &mut issues);
    if let Some(d) = &w.docs {
        check_rel_path(d, "/docs", false, &mut issues);
        if d.split('/').any(|seg| seg == "..") {
            issues.push(Issue::schema(
                "/docs",
                "docs must stay inside the plugin directory",
            ));
        }
    }

    let mut names = BTreeSet::new();
    for (i, a) in w.actions.iter().enumerate() {
        if !names.insert(a.id.as_str().to_owned()) {
            issues.push(Issue::schema(
                format!("/actions/{i}/id"),
                "duplicate action or view id",
            ));
        }
    }
    for (i, v) in w.views.iter().enumerate() {
        if !names.insert(v.id.as_str().to_owned()) {
            issues.push(Issue::schema(
                format!("/views/{i}/id"),
                "duplicate action or view id",
            ));
        }
    }

    let mut actions = Vec::new();
    for (i, a) in w.actions.iter().enumerate() {
        if let Some(action) = validate_action(
            a,
            &w,
            entry.is_some(),
            &format!("/actions/{i}"),
            &mut issues,
        ) {
            actions.push(action);
        }
    }
    let mut views = Vec::new();
    for (i, v) in w.views.iter().enumerate() {
        let p = format!("/views/{i}");
        check_text(
            &v.title,
            MAX_NAME_BYTES,
            true,
            &format!("{p}/title"),
            &mut issues,
        );
        check_text(
            &v.description,
            MAX_DESCRIPTION_BYTES,
            false,
            &format!("{p}/description"),
            &mut issues,
        );
        if !v.row_actions.is_empty() && v.kind != ViewKind::Table {
            issues.push(Issue::schema(
                format!("{p}/row_actions"),
                "row_actions are only allowed on table views",
            ));
        }
        let source = v.source.as_ref().map(|src| {
            if v.kind != ViewKind::Log {
                issues.push(Issue::schema(
                    format!("{p}/source"),
                    "source is only allowed on log views",
                ));
            }
            if let Some(g) = &src.grep {
                check_text(
                    g,
                    MAX_NAME_BYTES,
                    true,
                    &format!("{p}/source/grep"),
                    &mut issues,
                );
            }
            ViewSource {
                logs: src.logs.as_action(),
                grep: src.grep.clone(),
                stream: src.stream,
            }
        });
        let mut bound = Vec::new();
        for (j, ra) in v.row_actions.iter().enumerate() {
            match w.actions.iter().find(|a| a.id == ra.action) {
                Some(a) => bound.push(a),
                None => issues.push(Issue::schema(
                    format!("{p}/row_actions/{j}/action"),
                    "row action refers to an unknown action",
                )),
            }
            for (k, col) in &ra.bindings {
                if k.is_empty() || col.is_empty() {
                    issues.push(Issue::schema(
                        format!("{p}/row_actions/{j}/bindings"),
                        "binding names and column ids must be non-empty",
                    ));
                }
            }
        }
        let hash = canonical_digest(&serde_json::json!({
            "view": v, "row_actions": bound, "config": w.config, "plugin": w.id,
        }));
        match hash {
            Ok(definition_hash) => views.push(ViewDefinition {
                id: v.id.clone(),
                title: v.title.clone(),
                kind: v.kind,
                description: v.description.clone(),
                persistence: v.persistence,
                row_actions: v
                    .row_actions
                    .iter()
                    .map(|r| RowAction {
                        action: r.action.clone(),
                        bindings: r.bindings.clone(),
                    })
                    .collect(),
                source,
                meta: v.meta.clone(),
                definition_hash,
            }),
            Err(e) => issues.push(Issue::new(ErrorCode::INTERNAL, p, e)),
        }
    }

    let plugin = Plugin {
        id: w.id.clone(),
        name: w.name.clone(),
        description: w.description.clone(),
        enabled: w.enabled,
        tags: w.tags.clone(),
        entry,
        actions,
        views,
        config: w.config.clone(),
        config_schema,
        docs: w.docs.clone(),
        meta: w.meta.clone(),
    };
    issues.into_result(plugin)
}

fn validate_action(
    a: &ActionWire,
    plugin: &PluginWire,
    has_entry: bool,
    p: &str,
    issues: &mut Issues,
) -> Option<Action> {
    let before = issues.0.len();
    check_text(
        &a.title,
        MAX_NAME_BYTES,
        true,
        &format!("{p}/title"),
        issues,
    );
    check_text(
        &a.description,
        MAX_DESCRIPTION_BYTES,
        true,
        &format!("{p}/description"),
        issues,
    );
    let run = match &a.run {
        RunnerWire::Command { argv } => Argv::parse(argv.clone(), &format!("{p}/run/argv"), issues)
            .map(|argv| Runner::Command { argv }),
        RunnerWire::Plugin => {
            if !has_entry {
                issues.push(Issue::schema(
                    format!("{p}/run"),
                    "a plugin runner needs the plugin `entry`",
                ));
            }
            Some(Runner::Plugin)
        }
    };
    check_rel_path(&a.cwd, &format!("{p}/cwd"), true, issues);
    if a.env_files.len() > MAX_ENV_FILES {
        issues.push(Issue::schema(
            format!("{p}/env_files"),
            "at most 16 env files",
        ));
    }
    for (i, f) in a.env_files.iter().enumerate() {
        check_rel_path(f, &format!("{p}/env_files/{i}"), true, issues);
    }
    check_env(&a.env, &format!("{p}/env"), issues);
    let input_schema = match &a.input_schema {
        Some(_) => check_schema_doc(&a.input_schema, &format!("{p}/input_schema"), true, issues),
        None => Some(SchemaDoc::empty_object()),
    };
    if let (RunnerWire::Command { argv }, Some(doc)) = (&a.run, &input_schema) {
        crate::template::check_argv(argv, doc, &format!("{p}/run/argv"), issues);
    }
    let output_schema = check_schema_doc(
        &a.output_schema,
        &format!("{p}/output_schema"),
        false,
        issues,
    );
    let timeout = match TimeoutPolicy::resolve(a.mode, a.timeout) {
        Ok(t) => Some(t),
        Err(m) => {
            issues.push(Issue::schema(format!("{p}/timeout"), m));
            None
        }
    };
    if a.terminal == TerminalMode::Pty && !matches!(a.run, RunnerWire::Command { .. }) {
        issues.push(Issue::schema(
            format!("{p}/terminal"),
            "pty is only allowed with the command runner",
        ));
    }
    let (lo, hi) = STOP_GRACE_RANGE_MS;
    if !(lo..=hi).contains(&a.stop_grace_ms) {
        issues.push(Issue::schema(
            format!("{p}/stop_grace_ms"),
            "must be between 100 and 60000",
        ));
    }
    let cleanup = a.cleanup.as_ref().and_then(|c| match c {
        CommandRunnerWire::Command { argv } => {
            Argv::parse(argv.clone(), &format!("{p}/cleanup/argv"), issues)
        }
    });
    let schedule = match &a.schedule {
        None => None,
        Some(s) => {
            if a.mode != ActionMode::Task {
                issues.push(Issue::schema(
                    format!("{p}/schedule"),
                    "schedules are only allowed on task actions",
                ));
            }
            if s.every_ms < MIN_SCHEDULE_EVERY_MS || s.every_ms > MAX_SAFE_INTEGER {
                issues.push(Issue::schema(
                    format!("{p}/schedule/every_ms"),
                    "every_ms must be at least 1000",
                ));
            }
            if let Some(doc) = &input_schema
                && let Ok(v) = doc.compile()
            {
                let eff = Value::Object(doc.effective_input(&s.params));
                issues.extend(doc.validate(&v, &eff, &format!("{p}/schedule/params")));
            }
            Some(Schedule {
                every_ms: s.every_ms,
                params: s.params.clone(),
                run_on_start: s.run_on_start,
            })
        }
    };
    if issues.0.len() != before {
        return None;
    }
    let definition_hash = canonical_digest(&serde_json::json!({
        "action": a, "config": plugin.config, "plugin": plugin.id, "entry": plugin.entry,
    }));
    let definition_hash = match definition_hash {
        Ok(h) => h,
        Err(e) => {
            issues.push(Issue::new(ErrorCode::INTERNAL, p, e));
            return None;
        }
    };
    Some(Action {
        id: a.id.clone(),
        title: a.title.clone(),
        description: a.description.clone(),
        mode: a.mode,
        run: run?,
        cwd: a.cwd.clone(),
        env_files: a.env_files.clone(),
        env: a.env.clone(),
        input_schema: SchemaDocSer(input_schema?),
        output_schema: output_schema.map(SchemaDocSer),
        timeout: timeout?,
        terminal: a.terminal,
        stop_signal: a.stop_signal,
        stop_grace_ms: a.stop_grace_ms,
        cleanup,
        schedule,
        effects: a.effects.clone(),
        meta: a.meta.clone(),
        definition_hash,
    })
}

/// Validates a workspace wire DTO. Autostart targets are checked against plugins in `config`.
pub fn validate_workspace(w: WorkspaceWire) -> Result<Workspace, Issues> {
    let mut issues = Issues::default();
    check_text(&w.name, MAX_NAME_BYTES, true, "/name", &mut issues);
    if w.plugins.len() > MAX_PLUGINS {
        issues.push(Issue::schema("/plugins", "at most 256 plugins"));
    }
    let mut seen = BTreeSet::new();
    for (i, p) in w.plugins.iter().enumerate() {
        check_rel_path(p, &format!("/plugins/{i}"), true, &mut issues);
        if !seen.insert(p) {
            issues.push(Issue::schema(
                format!("/plugins/{i}"),
                "duplicate plugin path",
            ));
        }
    }
    check_object_size(&w.meta, MAX_META_BYTES, "/meta", &mut issues);
    if let Some(s) = &w.storage {
        StoragePolicy::default().apply(s, "/storage", &mut issues);
    }
    let mut ui = UiPrefs::default();
    if let Some(u) = &w.ui {
        ui.apply(u);
    }
    issues.into_result(Workspace {
        name: w.name,
        plugins: w.plugins,
        autostart: w.autostart,
        ui,
        storage_wire: w.storage,
        meta: w.meta,
    })
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
