//! Storage thread and handle.
//!
//! One std thread owns the only [`Db`] connection. [`Storage`] handles send typed [`Job`]s
//! over a bounded channel and await a oneshot reply, so async callers never block the
//! runtime on SQLite. When the last handle drops, the thread runs a bounded checkpoint and
//! closes the connection.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread::JoinHandle;

use hmac::{Hmac, KeyInit, Mac};
use mira_protocol::ids::{ActionRef, CatalogRevision, Digest, RunId, ViewRef};
use mira_protocol::ipc::StorageStatusData;
use mira_protocol::paths::WorkspacePaths;
use mira_protocol::run::RunRecord;
use serde_json::Value;
use sha2::Sha256;
use tokio::sync::{mpsc, oneshot};

use super::db::Db;
use super::{
    Claim, GcPolicy, GcSelection, KeyClaim, OpenReport, RunFilter, StorageError, StoredView,
};

/// Pending jobs beyond this make senders wait (backpressure).
const QUEUE_DEPTH: usize = 64;

type Reply<T> = oneshot::Sender<Result<T, StorageError>>;

enum Job {
    ClaimKey(KeyClaim, String, Reply<Claim>),
    InsertRun(Box<RunRecord>, Reply<()>),
    SaveRun(Box<RunRecord>, Reply<()>),
    GetRun(RunId, Reply<Option<RunRecord>>),
    ListRuns(RunFilter, Reply<Vec<RunRecord>>),
    Catalog(Reply<(CatalogRevision, Option<Digest>)>),
    AcceptCatalog(Digest, Reply<CatalogRevision>),
    ReserveViewBlock(Reply<(u64, u64)>),
    SaveView(Box<StoredView>, Reply<()>),
    LoadView(ViewRef, Reply<Option<StoredView>>),
    Status(Reply<StorageStatusData>),
    SetSchedule(ActionRef, bool, Reply<()>),
    GcSelect(GcPolicy, i64, Vec<RunId>, Reply<GcSelection>),
    GcApply(Box<GcSelection>, i64, Vec<RunId>, Reply<()>),
    CleanedViews(Reply<Vec<(ViewRef, u64, i64)>>),
    ListSchedules(Reply<Vec<(ActionRef, bool)>>),
}

impl Job {
    fn run(self, db: &mut Db) {
        // A caller that stopped waiting is not an error for the ledger.
        match self {
            Job::ClaimKey(c, r, tx) => drop(tx.send(db.claim_key(&c, &r))),
            Job::InsertRun(run, tx) => drop(tx.send(db.insert_run(&run))),
            Job::SaveRun(run, tx) => drop(tx.send(db.save_run(&run))),
            Job::GetRun(id, tx) => drop(tx.send(db.get_run(&id))),
            Job::ListRuns(f, tx) => drop(tx.send(db.list_runs(&f))),
            Job::Catalog(tx) => drop(tx.send(db.catalog())),
            Job::AcceptCatalog(h, tx) => drop(tx.send(db.accept_catalog(&h))),
            Job::ReserveViewBlock(tx) => drop(tx.send(db.reserve_view_block())),
            Job::SaveView(v, tx) => drop(tx.send(db.save_view(&v))),
            Job::LoadView(v, tx) => drop(tx.send(db.load_view(&v))),
            Job::Status(tx) => drop(tx.send(db.status())),
            Job::SetSchedule(a, e, tx) => drop(tx.send(db.set_schedule(&a, e))),
            Job::GcSelect(p, now, active, tx) => drop(tx.send(db.gc_select(&p, now, &active))),
            Job::GcApply(sel, now, active, tx) => drop(tx.send(db.gc_apply(&sel, now, &active))),
            Job::CleanedViews(tx) => drop(tx.send(db.cleaned_views())),
            Job::ListSchedules(tx) => drop(tx.send(db.list_schedules())),
        }
    }
}

fn serve(mut db: Db, mut jobs: mpsc::Receiver<Job>) {
    while let Some(job) = jobs.blocking_recv() {
        // A bug in one job must not take the ledger down; its caller sees a dropped reply.
        if catch_unwind(AssertUnwindSafe(|| job.run(&mut db))).is_err() {
            crate::diag("storage job panicked; the storage thread keeps serving");
        }
    }
    db.close();
}

struct Inner {
    jobs: Option<mpsc::Sender<Job>>,
    thread: Option<JoinHandle<()>>,
    key: [u8; 32],
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Closing the channel ends the loop; waiting keeps the checkpoint inside the process
        // lifetime. It is bounded by busy_timeout.
        drop(self.jobs.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[derive(Clone)]
pub struct Storage {
    inner: Arc<Inner>,
}

fn stopped() -> StorageError {
    StorageError::Unavailable("the storage thread has stopped".into())
}

impl Storage {
    /// Opens the workspace ledger and starts its thread. Blocking; call before serving or
    /// from `spawn_blocking`.
    pub fn open(paths: &WorkspacePaths) -> Result<(Storage, OpenReport), StorageError> {
        let (db, key, report) = Db::open(paths)?;
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        let thread = std::thread::Builder::new()
            .name("mira-storage".into())
            .spawn(move || serve(db, rx))
            .map_err(|e| StorageError::Unavailable(format!("start storage thread: {e}")))?;
        let storage = Storage {
            inner: Arc::new(Inner {
                jobs: Some(tx),
                thread: Some(thread),
                key,
            }),
        };
        Ok((storage, report))
    }

    /// HMAC-SHA-256 with the workspace key over the JCS form of `value`.
    pub fn fingerprint(&self, value: &Value) -> Digest {
        // A serde_json::Value always has a JCS form; the fallback only keeps this total.
        let bytes = mira_protocol::hash::canonical_bytes(value).unwrap_or_default();
        let mac = match Hmac::<Sha256>::new_from_slice(&self.inner.key) {
            Ok(mut m) => {
                m.update(&bytes);
                m.finalize().into_bytes().to_vec()
            }
            Err(_) => bytes,
        };
        Digest::parse(format!("sha256:{}", hex::encode(&mac)))
            .unwrap_or_else(|_| Digest::of_bytes(&mac))
    }

    async fn call<T>(&self, job: impl FnOnce(Reply<T>) -> Job) -> Result<T, StorageError> {
        let jobs = self.inner.jobs.as_ref().ok_or_else(stopped)?;
        let (tx, rx) = oneshot::channel();
        jobs.send(job(tx)).await.map_err(|_| stopped())?;
        rx.await.map_err(|_| {
            StorageError::Unavailable("the storage job ended without a result".into())
        })?
    }

    pub async fn claim_key(
        &self,
        claim: KeyClaim,
        reference: String,
    ) -> Result<Claim, StorageError> {
        self.call(|tx| Job::ClaimKey(claim, reference, tx)).await
    }
    pub async fn insert_run(&self, run: RunRecord) -> Result<(), StorageError> {
        self.call(|tx| Job::InsertRun(Box::new(run), tx)).await
    }
    pub async fn save_run(&self, run: RunRecord) -> Result<(), StorageError> {
        self.call(|tx| Job::SaveRun(Box::new(run), tx)).await
    }
    pub async fn get_run(&self, id: RunId) -> Result<Option<RunRecord>, StorageError> {
        self.call(|tx| Job::GetRun(id, tx)).await
    }
    pub async fn list_runs(&self, filter: RunFilter) -> Result<Vec<RunRecord>, StorageError> {
        self.call(|tx| Job::ListRuns(filter, tx)).await
    }
    pub async fn catalog(&self) -> Result<(CatalogRevision, Option<Digest>), StorageError> {
        self.call(Job::Catalog).await
    }
    pub async fn accept_catalog(&self, set_hash: Digest) -> Result<CatalogRevision, StorageError> {
        self.call(|tx| Job::AcceptCatalog(set_hash, tx)).await
    }
    pub async fn reserve_view_block(&self) -> Result<(u64, u64), StorageError> {
        self.call(Job::ReserveViewBlock).await
    }
    pub async fn save_view(&self, view: StoredView) -> Result<(), StorageError> {
        self.call(|tx| Job::SaveView(Box::new(view), tx)).await
    }
    pub async fn load_view(&self, view: ViewRef) -> Result<Option<StoredView>, StorageError> {
        self.call(|tx| Job::LoadView(view, tx)).await
    }
    pub async fn set_schedule(
        &self,
        action_ref: ActionRef,
        enabled: bool,
    ) -> Result<(), StorageError> {
        self.call(|tx| Job::SetSchedule(action_ref, enabled, tx))
            .await
    }
    pub async fn gc_select(
        &self,
        policy: GcPolicy,
        now_ms: i64,
        active: Vec<RunId>,
    ) -> Result<GcSelection, StorageError> {
        self.call(|tx| Job::GcSelect(policy, now_ms, active, tx))
            .await
    }
    pub async fn gc_apply(
        &self,
        selection: GcSelection,
        now_ms: i64,
        active: Vec<RunId>,
    ) -> Result<(), StorageError> {
        self.call(|tx| Job::GcApply(Box::new(selection), now_ms, active, tx))
            .await
    }
    pub async fn cleaned_views(&self) -> Result<Vec<(ViewRef, u64, i64)>, StorageError> {
        self.call(Job::CleanedViews).await
    }
    pub async fn list_schedules(&self) -> Result<Vec<(ActionRef, bool)>, StorageError> {
        self.call(Job::ListSchedules).await
    }
    pub async fn status(&self) -> Result<StorageStatusData, StorageError> {
        self.call(Job::Status).await
    }
}
