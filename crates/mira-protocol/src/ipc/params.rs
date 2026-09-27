//! Request parameters for every method, and the request types they share.

use std::collections::BTreeMap;
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::ids::*;
use crate::limits::MAX_CLIENT_ENV_BYTES;
use crate::manifest::{JsonObject, TimeoutWire, present};
use crate::run::Outcome;
use crate::view::SourceKind;

/// Client environment for execution. Values may be secrets: `Debug` never prints them.
#[derive(Clone, PartialEq, Eq, Default, Serialize, JsonSchema)]
pub struct ClientEnv(BTreeMap<String, String>);

impl ClientEnv {
    pub fn new(vars: BTreeMap<String, String>) -> Result<Self, &'static str> {
        let mut total = 0usize;
        for (k, v) in &vars {
            if k.is_empty() || k.contains('=') || k.contains('\0') || v.contains('\0') {
                return Err("client environment contains an invalid name or a NUL byte");
            }
            total += k.len() + v.len() + 2;
        }
        if total > MAX_CLIENT_ENV_BYTES {
            return Err("client environment exceeds 256 KiB");
        }
        Ok(Self(vars))
    }
    /// Captures the current process environment, skipping non-UTF-8 entries.
    pub fn capture() -> Result<Self, &'static str> {
        Self::new(
            std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect(),
        )
    }
    pub fn vars(&self) -> &BTreeMap<String, String> {
        &self.0
    }
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
}
impl fmt::Debug for ClientEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ClientEnv({} vars)", self.0.len())
    }
}
impl<'de> Deserialize<'de> for ClientEnv {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(BTreeMap::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Tui,
    Cli,
    Hook,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionKind {
    Control,
    Stream,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelloParams {
    pub api: Api1,
    pub protocol_hash: Digest,
    pub workspace_id: WorkspaceId,
    pub workspace_root: AbsolutePath,
    pub client_kind: ClientKind,
    pub connection_kind: ConnectionKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionAttachParams {
    pub client_env: ClientEnv,
    pub autostart: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpenMode {
    Background,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionOpenParams {
    pub mode: OpenMode,
    pub client_env: ClientEnv,
    pub ttl: TimeoutWire,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionKeepParams {
    pub ttl: TimeoutWire,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CatalogListParams {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "String")]
    pub query: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "CatalogRevision")]
    pub if_revision: Option<CatalogRevision>,
    /// With `if_revision`: the workspace the cached catalog came from. A different workspace
    /// never gets `not_modified`, even at an equal revision number.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "WorkspaceId")]
    pub if_workspace: Option<WorkspaceId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "String")]
    pub cursor: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "u32")]
    pub limit: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "u32")]
    pub max_bytes: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ItemDescribeParams {
    #[serde(rename = "ref")]
    pub item_ref: ItemRef,
    #[serde(default)]
    pub include_schema: bool,
    /// Reply budget; schemas that do not fit are returned by payload reference.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "u32")]
    pub max_bytes: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionInvokeParams {
    pub action_ref: ActionRef,
    pub input: JsonObject,
    pub client_env: ClientEnv,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "RequestKey")]
    pub request_key: Option<RequestKey>,
    /// Lets a waiting CLI run a task without an existing session.
    #[serde(default)]
    pub foreground: bool,
}

/// Ad-hoc command through the same command/task path; never a catalog entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionExecParams {
    pub label: String,
    pub argv: Vec<String>,
    pub client_env: ClientEnv,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "RequestKey")]
    pub request_key: Option<RequestKey>,
    #[serde(default)]
    pub foreground: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunTarget {
    Run { run_id: RunId },
    Action { action_ref: ActionRef },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunStopParams {
    pub target: RunTarget,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunGetParams {
    pub run_id: RunId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunListParams {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "ActionRef")]
    pub action_ref: Option<ActionRef>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "Outcome")]
    pub outcome: Option<Outcome>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "String")]
    pub cursor: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "u32")]
    pub limit: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "u32")]
    pub max_bytes: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogReadParams {
    pub target: RunTarget,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "String")]
    pub cursor: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "u32")]
    pub limit: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "u32")]
    pub max_bytes: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewReadParams {
    pub view_ref: ViewRef,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "String")]
    pub cursor: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "u32")]
    pub limit: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "u32")]
    pub max_bytes: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewPublishParams {
    pub view_ref: ViewRef,
    /// One MPP `view` frame; its `view_id` must equal `view_ref`'s local ID.
    pub frame: JsonObject,
    pub source_kind: SourceKind,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "ViewRevision")]
    pub expected_view_revision: Option<ViewRevision>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "RequestKey")]
    pub request_key: Option<RequestKey>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewActionParams {
    pub view_ref: ViewRef,
    pub action: ActionId,
    pub row: String,
    pub expected_view_revision: ViewRevision,
    pub client_env: ClientEnv,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigApplyParams {
    pub draft_dir: AbsolutePath,
    pub expected_catalog_revision: CatalogRevision,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "RequestKey")]
    pub request_key: Option<RequestKey>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleSetParams {
    pub action_ref: ActionRef,
    pub enabled: bool,
    pub client_env: ClientEnv,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalRunParams {
    pub run_id: RunId,
}

/// `terminal.snapshot`: one consistent read of the virtual screen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalSnapshotParams {
    pub run_id: RunId,
    /// First screen row to return (0-based); default 0.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "u16")]
    pub row_start: Option<u16>,
    /// Rows to return; default all remaining rows.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "u16")]
    pub row_count: Option<u16>,
    /// Include per-row style runs for the returned rows.
    #[serde(default)]
    pub include_style: bool,
    /// Reply byte budget; rows past it are omitted and `meta.truncated` is set.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "u32")]
    pub max_bytes: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TerminalInput {
    /// Literal text; no Enter is appended.
    Text { text: String },
    /// A named key: enter, tab, escape, backspace, delete, up, down, left, right, ctrl-c,
    /// ctrl-d, ctrl-z, ctrl-right-bracket.
    Key { key: String },
    /// Bracketed paste when the child enabled it.
    Paste { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalInputParams {
    pub run_id: RunId,
    pub input: TerminalInput,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "ScreenRevision")]
    pub expected_screen_revision: Option<ScreenRevision>,
    /// Reply with the current screen at once instead of waiting for the program to react
    /// (interactive clients that follow `terminal` stream events).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reply_now: bool,
}

/// A desktop notification from a running program, such as a timer in a PTY.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunNotifyParams {
    pub run_id: RunId,
    pub title: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalResizeParams {
    pub run_id: RunId,
    pub cols: u16,
    pub rows: u16,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    State,
    Log,
    View,
    Terminal,
    Progress,
    /// Desktop notifications from plugins.
    Notify,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StreamSubscribeParams {
    pub kinds: Vec<StreamKind>,
    /// Run IDs or item refs that bound log/view/terminal/progress events.
    #[serde(default)]
    pub refs: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "String")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageStatusParams {
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GcKind {
    Cache,
    Logs,
    History,
    Artifacts,
    All,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageGcParams {
    pub kind: GcKind,
    #[serde(default)]
    pub apply: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClearKind {
    State,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageClearParams {
    pub plugin: PluginId,
    pub kind: ClearKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactListParams {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "RunId")]
    pub run_id: Option<RunId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReadParams {
    pub artifact_id: String,
    #[serde(default)]
    pub offset: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "u32")]
    pub max_bytes: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PayloadReadParams {
    pub token: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "String")]
    pub pointer: Option<String>,
    #[serde(default)]
    pub offset: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "u32")]
    pub max_bytes: Option<u32>,
}
