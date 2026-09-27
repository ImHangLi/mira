//! Validated domain types. Only the validators produce them.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;

use crate::error::{Issue, Issues};
use crate::ids::{ActionId, ActionRef, Digest, PluginId, ViewId};
use crate::limits::*;
use crate::schema_profile::SchemaDoc;

use super::JsonObject;
use super::wire::{
    ActionMode, Persistence, SourceStream, StopSignal, StoragePolicyWire, TerminalMode,
    TimeoutWire, UiTheme, UiWire, ViewKind,
};

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
    pub show: super::ShowPolicy,
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
