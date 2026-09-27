//! MIPC/1: JSON-RPC 2.0 single-object profile over a Unix socket.
//!
//! Method names, request DTOs, result DTOs, and their schemas are registered once in
//! [`methods!`]. Successful application answers are `PublicReply<Result>`; transport and
//! envelope failures use numeric JSON-RPC errors.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::error::ErrorInfo;
use crate::ids::*;
use crate::limits::MAX_CLIENT_ENV_BYTES;
use crate::manifest::{
    ActionMode, JsonObject, Persistence, TerminalMode, TimeoutWire, ViewKind, present,
};
use crate::mpp::ArtifactOwnership;
use crate::reply::WorkspaceRef;
use crate::run::{Lifecycle, LogRecord, Outcome, RunRecord, RunSummary};
use crate::time::Timestamp;
use crate::view::{Durability, SourceKind, ViewSnapshot};

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JsonRpc2;
impl Serialize for JsonRpc2 {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("2.0")
    }
}
impl<'de> Deserialize<'de> for JsonRpc2 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "2.0" {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("jsonrpc must be \"2.0\""))
        }
    }
}
impl JsonSchema for JsonRpc2 {
    fn schema_name() -> Cow<'static, str> {
        "JsonRpc2".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"const": "2.0"})
    }
}

/// A request. `id` is a non-empty string and `params` an object (no batches, no positional params).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RpcRequest {
    pub jsonrpc: JsonRpc2,
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Map<String, Value>,
}

/// A host notification (no id). Only `stream.event` is defined.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RpcNotification {
    pub jsonrpc: JsonRpc2,
    pub method: String,
    pub params: StreamFrame,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "ErrorInfo")]
    pub data: Option<ErrorInfo>,
}

impl RpcError {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// Handshake refused or required (workspace, api, or protocol hash mismatch).
    pub const HANDSHAKE: i64 = -32000;
    /// Method not allowed on this connection kind.
    pub const NOT_ALLOWED: i64 = -32001;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
    pub fn with_info(mut self, info: ErrorInfo) -> Self {
        self.data = Some(info);
        self
    }
}

/// A response carries exactly one of `result` or `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RpcResponse {
    Success {
        jsonrpc: JsonRpc2,
        id: String,
        result: Value,
    },
    Failure {
        jsonrpc: JsonRpc2,
        id: Option<String>,
        error: RpcError,
    },
}

/// Anything the host may send on a connection.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum HostMessage {
    Notification(RpcNotification),
    Response(RpcResponse),
}

// ---------------------------------------------------------------------------
// Shared request types
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelloReply {
    pub api: Api1,
    pub protocol_hash: Digest,
    pub host_epoch: HostEpoch,
    pub client_id: ClientId,
    pub workspace: WorkspaceRef,
    pub catalog_revision: CatalogRevision,
    pub state_revision: StateRevision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Foreground,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Active,
    Stopping,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionInfo {
    pub id: SessionId,
    pub mode: SessionMode,
    /// Controllers, including CLIs waiting on their own task.
    pub controller_count: u32,
    pub expires_at: Option<Timestamp>,
    pub state: SessionState,
    /// True when an explicit background lease keeps work running without controllers
    /// (`expires_at` is null for a `ttl: none` lease).
    #[serde(default)]
    pub background_lease: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionData {
    pub session: Option<SessionInfo>,
}

/// Result of `session.stop`: the resulting session state and what the call stopped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionStopData {
    /// The session after the call; `null` once it has ended.
    pub session: Option<SessionInfo>,
    /// The session this call stopped; `null` when no session was running.
    pub stopped_session: Option<SessionId>,
    /// Active runs this call asked to stop.
    pub stopped_runs: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Warning {
    pub code: crate::error::ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusData {
    pub session: Option<SessionInfo>,
    pub runs: Vec<RunSummary>,
    pub storage_warnings: Vec<Warning>,
    /// On-disk configuration problems (invalid on disk, incomplete apply); `status` is where
    /// disk configuration warnings are reported.
    pub config_warnings: Vec<Warning>,
    /// Interval schedules with a persisted switch, and their state in this session.
    #[serde(default)]
    pub schedules: Vec<ScheduleData>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CatalogItemKind {
    Action { mode: ActionMode },
    View { view_kind: ViewKind },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CatalogItem {
    #[serde(rename = "ref")]
    pub item_ref: ItemRef,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    pub enabled: bool,
    pub definition_hash: Digest,
    pub item: CatalogItemKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CatalogList {
    pub items: Vec<CatalogItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginSummary {
    pub id: PluginId,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    /// Plugin-relative docs path, read only on demand.
    pub docs: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionDescription {
    pub mode: ActionMode,
    pub runner: String,
    pub terminal: TerminalMode,
    pub timeout: TimeoutWire,
    pub cwd: String,
    /// Names only; values are never described.
    pub env_names: Vec<String>,
    pub env_files: Vec<String>,
    pub effects: Vec<String>,
    pub has_schedule: bool,
    pub write_only_fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<JsonObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<JsonObject>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewDescription {
    pub view_kind: ViewKind,
    pub persistence: Persistence,
    pub row_actions: Vec<ActionId>,
    /// Set for a log view the host derives from another action's run log.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "crate::manifest::ViewSourceWire")]
    pub source: Option<crate::manifest::ViewSourceWire>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ItemDescription {
    pub item: CatalogItem,
    pub plugin: PluginSummary,
    pub action: Option<ActionDescription>,
    pub view: Option<ViewDescription>,
    /// Example CLI invocation for agents.
    pub invoke_hint: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InvokeAccepted {
    pub run_id: RunId,
    pub state: Lifecycle,
    /// True when an existing process instance or request-key run was returned.
    pub reused: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StopAccepted {
    pub run_id: RunId,
    pub state: Lifecycle,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunList {
    pub runs: Vec<RunRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogPage {
    pub run_id: RunId,
    pub items: Vec<LogRecord>,
    pub first_available_seq: Option<LogSeq>,
    pub last_available_seq: Option<LogSeq>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PublishResult {
    pub view_ref: ViewRef,
    pub view_revision: ViewRevision,
    pub durability: Durability,
    pub reused: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigApplied {
    pub catalog_revision: CatalogRevision,
    pub changed: bool,
    pub plugins: Vec<PluginId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleData {
    pub action_ref: ActionRef,
    pub enabled: bool,
    pub every_ms: u64,
    pub missed_ticks: u64,
    pub next_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalCursor {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalSnapshot {
    pub run_id: RunId,
    pub screen_revision: ScreenRevision,
    pub cols: u16,
    pub rows: u16,
    /// Screen row of `lines[0]`.
    #[serde(default)]
    pub row_start: u16,
    /// Screen rows as plain text without control sequences, from `row_start` on.
    pub lines: Vec<String>,
    pub cursor: TerminalCursor,
    pub alternate_screen: bool,
    pub input_owner: Option<ClientId>,
    pub exited: bool,
    /// Style runs for the returned rows when `include_style` was set; cells without a run
    /// use the terminal default style.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub styles: Option<Vec<TerminalStyleRow>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalStyleRow {
    pub row: u16,
    pub runs: Vec<TerminalStyleRun>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalStyleRun {
    pub start_cell: u16,
    pub cell_count: u16,
    pub fg: TerminalColor,
    pub bg: TerminalColor,
    pub modifiers: Vec<TerminalModifier>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TerminalColor {
    Default,
    Indexed { index: u8 },
    Rgb { r: u8, g: u8, b: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TerminalModifier {
    Bold,
    Dim,
    Italic,
    Underline,
    Reverse,
    Hidden,
    Strikethrough,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Ack {
    pub ok: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Subscribed {
    pub subscription_id: SubscriptionId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PathClass {
    WorkspaceConfig,
    Plugins,
    LocalConfig,
    Drafts,
    StateDb,
    FingerprintKey,
    PluginState,
    Artifacts,
    Logs,
    HostLog,
    Cache,
    Runtime,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathEntry {
    pub class: PathClass,
    pub path: AbsolutePath,
    pub purpose: String,
    pub auto_delete: bool,
    pub agent_reads: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathsData {
    pub workspace: WorkspaceRef,
    pub entries: Vec<PathEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageUsage {
    pub class: PathClass,
    pub bytes: u64,
    pub records: Option<u64>,
    pub budget_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageStatusData {
    pub schema_version: u32,
    pub sqlite_version: String,
    pub usage: Vec<StorageUsage>,
    pub warnings: Vec<Warning>,
    /// Other workspaces on this machine (`--all`); observed only, never started or cleaned.
    #[serde(default)]
    pub other_workspaces: Vec<OtherWorkspace>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OtherWorkspace {
    pub id: String,
    pub state_bytes: u64,
    pub log_bytes: u64,
    pub cache_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GcEntry {
    pub kind: GcKind,
    pub records: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GcReport {
    pub applied: bool,
    pub entries: Vec<GcEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactInfo {
    pub id: String,
    pub run_id: RunId,
    pub path: AbsolutePath,
    pub mime: String,
    pub label: String,
    pub ownership: ArtifactOwnership,
    pub size_bytes: Option<u64>,
    pub state: ArtifactState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactState {
    Present,
    NotFound,
    Changed,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactListData {
    pub artifacts: Vec<ArtifactInfo>,
}

/// A bounded UTF-8 chunk of an artifact or payload. For payloads, `sha256` is the
/// digest of the whole selected value (after `pointer`), so reassembled chunks can be checked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChunkData {
    pub token: String,
    pub pointer: Option<String>,
    pub offset: u64,
    pub text: String,
    pub next_offset: Option<u64>,
    pub done: bool,
    pub sha256: Digest,
}

// ---------------------------------------------------------------------------
// Stream frames
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StreamSource {
    pub workspace_id: WorkspaceId,
    pub run_id: Option<RunId>,
    pub item_ref: Option<ItemRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    SourceEnded,
    ResetRequired,
    SessionEnded,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum StreamEvent {
    Ready {
        subscription_id: SubscriptionId,
        catalog_revision: CatalogRevision,
        state_revision: StateRevision,
    },
    Snapshot(StatusData),
    State {
        state_revision: StateRevision,
        session: Option<SessionInfo>,
        runs: Vec<RunSummary>,
        storage_warnings: Vec<Warning>,
    },
    Log {
        run_id: RunId,
        records: Vec<LogRecord>,
    },
    View {
        view_ref: ViewRef,
        view_revision: ViewRevision,
    },
    Terminal {
        run_id: RunId,
        screen_revision: ScreenRevision,
    },
    Progress {
        run_id: RunId,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total: Option<f64>,
    },
    Gap {
        reason: String,
        dropped_records: Option<u64>,
        resume_cursor: Option<String>,
    },
    End {
        reason: EndReason,
    },
}

/// Host-generated outer fields plus one event. Plugins can never set these fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StreamFrame {
    pub api: Api1,
    pub host_epoch: HostEpoch,
    pub event_seq: EventSeq,
    pub recorded_at: Timestamp,
    pub cursor: String,
    pub source: StreamSource,
    #[serde(flatten)]
    pub event: StreamEvent,
}

// ---------------------------------------------------------------------------
// Method registry
// ---------------------------------------------------------------------------

macro_rules! methods {
    ($($variant:ident = $name:literal, $params:ty => $result:ty, stream: $stream:literal;)*) => {
        /// Every MIPC/1 method.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Method { $($variant),* }

        impl Method {
            pub const ALL: &'static [Method] = &[$(Method::$variant),*];
            pub fn name(self) -> &'static str {
                match self { $(Method::$variant => $name),* }
            }
            pub fn parse(name: &str) -> Option<Self> {
                match name { $($name => Some(Method::$variant),)* _ => None }
            }
            /// Stream connections may only call subscribe after hello.
            pub fn allowed_on_stream(self) -> bool {
                match self { $(Method::$variant => $stream),* }
            }
        }

        /// Request/result schemas keyed by method name.
        pub fn method_schemas(generator: &mut SchemaGenerator) -> Map<String, Value> {
            let mut out = Map::new();
            $(
                let params = generator.subschema_for::<$params>();
                let result = generator.subschema_for::<$result>();
                out.insert($name.to_owned(), serde_json::json!({"params": params, "result": result}));
            )*
            out
        }
    };
}

methods! {
    Hello = "hello", HelloParams => HelloReply, stream: true;
    SessionAttach = "session.attach", SessionAttachParams => SessionData, stream: false;
    SessionOpen = "session.open", SessionOpenParams => SessionData, stream: false;
    SessionKeep = "session.keep", SessionKeepParams => SessionData, stream: false;
    SessionStop = "session.stop", Empty => SessionStopData, stream: false;
    WorkspaceStatus = "workspace.status", Empty => StatusData, stream: false;
    CatalogListM = "catalog.list", CatalogListParams => CatalogList, stream: false;
    ItemDescribe = "item.describe", ItemDescribeParams => ItemDescription, stream: false;
    ActionInvoke = "action.invoke", ActionInvokeParams => InvokeAccepted, stream: false;
    ActionExec = "action.exec", ActionExecParams => InvokeAccepted, stream: false;
    RunStop = "run.stop", RunStopParams => StopAccepted, stream: false;
    RunGet = "run.get", RunGetParams => RunRecord, stream: false;
    RunListM = "run.list", RunListParams => RunList, stream: false;
    LogRead = "log.read", LogReadParams => LogPage, stream: false;
    ViewRead = "view.read", ViewReadParams => ViewSnapshot, stream: false;
    ViewPublish = "view.publish", ViewPublishParams => PublishResult, stream: false;
    ViewAction = "view.action", ViewActionParams => InvokeAccepted, stream: false;
    ConfigApply = "config.apply", ConfigApplyParams => ConfigApplied, stream: false;
    ConfigReload = "config.reload", Empty => ConfigApplied, stream: false;
    ScheduleSet = "schedule.set", ScheduleSetParams => ScheduleData, stream: false;
    TerminalSnapshotM = "terminal.snapshot", TerminalSnapshotParams => TerminalSnapshot, stream: false;
    TerminalAcquire = "terminal.acquire", TerminalRunParams => Ack, stream: false;
    TerminalRelease = "terminal.release", TerminalRunParams => Ack, stream: false;
    TerminalInputM = "terminal.input", TerminalInputParams => TerminalSnapshot, stream: false;
    TerminalResize = "terminal.resize", TerminalResizeParams => Ack, stream: false;
    StreamSubscribe = "stream.subscribe", StreamSubscribeParams => Subscribed, stream: true;
    StorageStatus = "storage.status", StorageStatusParams => StorageStatusData, stream: false;
    StorageGc = "storage.gc", StorageGcParams => GcReport, stream: false;
    StorageClear = "storage.clear", StorageClearParams => Ack, stream: false;
    ArtifactList = "artifact.list", ArtifactListParams => ArtifactListData, stream: false;
    ArtifactRead = "artifact.read", ArtifactReadParams => ChunkData, stream: false;
    PayloadRead = "payload.read", PayloadReadParams => ChunkData, stream: false;
}

/// The notification method carrying [`StreamFrame`]s.
pub const STREAM_EVENT: &str = "stream.event";
