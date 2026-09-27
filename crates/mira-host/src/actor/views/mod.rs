//! The view store: one typed ViewData per view, updated
//! atomically in host receive order with strictly increasing revisions from persisted blocks.
//!
//! Every update (plugin frame or explicit publish) goes through one FIFO queue. The queue
//! stalls only while a revision block is being reserved or a publish request key is being
//! claimed, so no update to a view can overtake another. `last` views are written through one
//! ordered writer task: publishes at once, plugin updates coalesced to once per second and on
//! run end. `session` views live in memory until the session ends.

mod content;
mod persist;
mod publish;
mod queue;
mod read;

use std::collections::{HashMap, VecDeque};

use mira_protocol::ids::*;
use mira_protocol::ipc::*;
use mira_protocol::manifest::Persistence;
use mira_protocol::time::Timestamp;
use mira_protocol::view::*;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::actor::Responder;
use crate::storage::{KeyClaim, StoredView};

pub(super) use read::Page;

pub struct ViewEntry {
    pub data: ViewData,
    pub revision: ViewRevision,
    pub recorded_at: Timestamp,
    pub source_run_id: Option<RunId>,
    pub source_kind: SourceKind,
    pub definition_hash: Digest,
    pub persistence: Persistence,
    /// Why the content is stale (source failed, or restored without a confirmed end).
    pub stale: Option<String>,
    /// Highest revision confirmed committed.
    pub saved: Option<ViewRevision>,
    /// Highest revision handed to the writer.
    pub requested: Option<ViewRevision>,
    pub last_write: Option<Instant>,
    pub save_error: Option<String>,
}

struct PublishReq {
    responder: Responder,
    expected: Option<ViewRevision>,
    claim: Option<KeyClaim>,
}

pub struct ViewUpdate {
    view_ref: ViewRef,
    op: ViewOp,
    data: ViewData,
    source_run_id: Option<RunId>,
    source_kind: SourceKind,
    publish: Option<PublishReq>,
}

enum Wait {
    Block,
    Claim,
}

#[derive(Default)]
pub struct ViewStore {
    pub entries: HashMap<ViewRef, ViewEntry>,
    queue: VecDeque<ViewUpdate>,
    wait: Option<Wait>,
    /// A publish whose request key is being claimed: (update, new data or no-op, revision).
    claiming: Option<(ViewUpdate, Option<ViewData>, ViewRevision)>,
    /// Reserved revisions `[next, end)`.
    block: Option<(u64, u64)>,
    writer: Option<mpsc::UnboundedSender<StoredView>>,
    commit_waiters: Vec<(ViewRef, ViewRevision, Responder, PublishResult)>,
    /// Views removed by retention: last revision and when (ms), so reads say "cleaned up".
    pub cleaned: HashMap<ViewRef, (u64, i64)>,
}

impl ViewEntry {
    fn durability(&self, storage_ok: bool) -> Durability {
        match self.persistence {
            Persistence::Session => Durability::SessionOnly,
            Persistence::Last if self.saved.is_some_and(|s| s >= self.revision) => {
                Durability::Committed
            }
            Persistence::Last if self.save_error.is_some() || !storage_ok => {
                Durability::Unavailable
            }
            Persistence::Last => Durability::Buffered,
        }
    }

    fn stored(&self, view_ref: &ViewRef) -> Result<StoredView, String> {
        Ok(StoredView {
            view_ref: view_ref.clone(),
            revision: self.revision,
            kind: self.data.kind(),
            recorded_at: self.recorded_at,
            source_run_id: self.source_run_id.clone(),
            source_kind: self.source_kind,
            definition_hash: self.definition_hash.clone(),
            data_json: serde_json::to_string(&self.data).map_err(|e| e.to_string())?,
        })
    }
}
