//! Strict wire DTOs, exactly as they appear in manifest files.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Issue, Issues};
use crate::ids::{ActionId, ActionRef, Api1, ItemRef, PluginId, ViewId};
use crate::limits::*;

use super::{JsonObject, ViewSource, present};

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
