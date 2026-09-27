//! Method results.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::*;
use crate::manifest::{
    ActionMode, JsonObject, Persistence, TerminalMode, TimeoutWire, ViewKind, present,
};
use crate::mpp::ArtifactOwnership;
use crate::reply::WorkspaceRef;
use crate::run::{Lifecycle, LogRecord, RunRecord, RunSummary};
use crate::time::Timestamp;
use crate::view::Durability;

use super::GcKind;

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
    pub show: crate::manifest::ShowPolicy,
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
