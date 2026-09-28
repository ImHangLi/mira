//! Run lifecycle: validate → reserve durably → spawn → observe → stop → finalize and
//! commit. The actor owns every transition.

mod launch;
mod lifecycle;
mod prepare;
mod queries;
mod start;

use std::collections::BTreeMap;
use std::time::Duration;

use mira_protocol::error::ErrorInfo;
use mira_protocol::ids::*;
use mira_protocol::ipc::*;
use mira_protocol::manifest::{ActionMode, StopSignal, TerminalMode, TimeoutPolicy};
use mira_protocol::run::*;
use serde_json::{Map, Value};

use crate::actor::plugin::MppRun;
use crate::actor::{Actor, Responder};
use crate::logs::SharedLog;
use crate::runner::StopKind;
use crate::storage::KeyScope;

pub enum Reservation {
    New,
    Same { reference: RunId },
}

/// Everything needed to start the child once the reservation is committed.
pub struct Launch {
    argv: Vec<String>,
    cwd: String,
    env_files: Vec<String>,
    action_env: BTreeMap<String, String>,
    client_env: ClientEnv,
    timeout: TimeoutPolicy,
    stop_signal: StopSignal,
    grace: Duration,
    cleanup: Option<Vec<String>>,
    terminal: TerminalMode,
    input: Map<String, Value>,
    config: Map<String, Value>,
    plugin: Option<(PluginId, AbsolutePath)>,
    /// MPP/1 plugin runs: the local action ID and mode for the invocation.
    protocol: Option<(ActionId, ActionMode)>,
}

pub enum Phase {
    Reserving {
        waiters: Vec<Responder>,
        launch: Box<Launch>,
    },
    Live,
    Finalizing,
}

pub struct ActiveRun {
    pub record: RunRecord,
    pub fingerprint: Digest,
    pub request_key: Option<RequestKey>,
    pub log: SharedLog,
    pub stop_tx: Option<tokio::sync::mpsc::Sender<StopKind>>,
    pub phase: Phase,
    pub temp_controller: Option<ClientId>,
    pub requested_stop: Option<StopReason>,
    /// MPP/1 state; `None` for command runs.
    pub mpp: Option<Box<MppRun>>,
}

/// A validated invocation, ready to reserve.
struct Prepared {
    /// The agent that asked for a one-off run.
    requester: Option<mira_protocol::run::Requester>,
    source: Option<RunSource>,
    action_ref: Option<ActionRef>,
    label: String,
    mode: ActionMode,
    definition_hash: Digest,
    fingerprint: Digest,
    request_key: Option<RequestKey>,
    scope: KeyScope,
    foreground: bool,
    cleanup_configured: bool,
    launch: Launch,
    mpp: Option<MppRun>,
}

impl Actor {
    fn fail_opt(&self, r: Option<Responder>, e: ErrorInfo) {
        if let Some(r) = r {
            r.send(self.fail(e));
        }
    }
}
