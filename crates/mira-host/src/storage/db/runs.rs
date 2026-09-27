//! Run records and request-key claims.

use mira_protocol::ids::{ActionRef, RunId};
use mira_protocol::run::{Lifecycle, RunRecord};
use mira_protocol::time::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};

use crate::storage::{Claim, KeyClaim, RunFilter, StorageError};

use super::{Db, Result, corrupt, enum_name, i64_of, lifecycle_name, sql, unavailable};

/// Request-key reservations are honored for this long.
const REQUEST_KEY_TTL_MS: i64 = 24 * 60 * 60 * 1000;
const DEFAULT_RUN_LIMIT: usize = 50;
const MAX_RUN_LIMIT: usize = 1000;

pub(super) fn upsert_run(conn: &Connection, rec: &RunRecord) -> Result<()> {
    let json = serde_json::to_string(rec).map_err(|e| unavailable("encode run", e))?;
    let outcome = match rec.lifecycle {
        Lifecycle::Finished { outcome } => Some(enum_name(&outcome)?),
        _ => None,
    };
    conn.execute(
        "INSERT INTO runs (run_id, action_ref, source, definition_hash, catalog_revision,
                           started_at, ended_at, lifecycle, outcome, record_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT (run_id) DO UPDATE SET
            action_ref = excluded.action_ref, source = excluded.source,
            definition_hash = excluded.definition_hash,
            catalog_revision = excluded.catalog_revision, started_at = excluded.started_at,
            ended_at = excluded.ended_at, lifecycle = excluded.lifecycle,
            outcome = excluded.outcome, record_json = excluded.record_json",
        params![
            rec.run_id.as_str(),
            rec.action_ref.as_ref().map(ToString::to_string),
            enum_name(&rec.source)?,
            rec.definition_hash.as_str(),
            i64_of(rec.catalog_revision.get(), "catalog_revision")?,
            rec.started_at.unix_ms(),
            rec.ended_at.map(Timestamp::unix_ms),
            lifecycle_name(rec.lifecycle),
            outcome,
            json,
        ],
    )
    .map(drop)
    .map_err(|e| sql("write run", e))
}

fn decode_run(json: &str) -> Result<RunRecord> {
    serde_json::from_str(json).map_err(|e| corrupt("stored run record", e))
}

impl Db {
    pub(crate) fn claim_key(&mut self, claim: &KeyClaim, reference: &str) -> Result<Claim> {
        let scope = claim.scope.as_key();
        let now = Timestamp::now().unix_ms();
        let tx = self.immediate("begin request-key claim")?;
        let existing: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT fingerprint, reference, created_at FROM request_keys
                 WHERE scope = ?1 AND request_key = ?2",
                params![scope, claim.key.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|e| sql("read request key", e))?;
        let result = match existing {
            None => {
                tx.execute(
                    "INSERT INTO request_keys (scope, request_key, fingerprint, reference, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![scope, claim.key.as_str(), claim.fingerprint.as_str(), reference, now],
                )
                .map_err(|e| sql("write request key", e))?;
                Claim::New
            }
            Some((fp, stored_ref, created_at)) => {
                // An active run keeps its reservation past 24 h.
                let still_active: bool = tx
                    .query_row(
                        "SELECT EXISTS (SELECT 1 FROM runs
                         WHERE run_id = ?1 AND lifecycle <> 'finished')",
                        params![stored_ref],
                        |r| r.get(0),
                    )
                    .map_err(|e| sql("read request key run", e))?;
                if now.saturating_sub(created_at) > REQUEST_KEY_TTL_MS && !still_active {
                    tx.execute(
                        "UPDATE request_keys SET fingerprint = ?3, reference = ?4, created_at = ?5
                         WHERE scope = ?1 AND request_key = ?2",
                        params![
                            scope,
                            claim.key.as_str(),
                            claim.fingerprint.as_str(),
                            reference,
                            now
                        ],
                    )
                    .map_err(|e| sql("replace expired request key", e))?;
                    Claim::New
                } else if fp == claim.fingerprint.as_str() {
                    Claim::Same {
                        reference: stored_ref,
                    }
                } else {
                    Claim::Conflict
                }
            }
        };
        tx.commit().map_err(|e| sql("commit request key", e))?;
        Ok(result)
    }

    pub(crate) fn insert_run(&mut self, rec: &RunRecord) -> Result<()> {
        let tx = self.immediate("begin run reservation")?;
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM runs WHERE run_id = ?1)",
                params![rec.run_id.as_str()],
                |r| r.get(0),
            )
            .map_err(|e| sql("read run", e))?;
        if exists {
            return Err(StorageError::Unavailable(format!(
                "run {} is already reserved",
                rec.run_id
            )));
        }
        upsert_run(&tx, rec)?;
        tx.commit().map_err(|e| sql("commit run reservation", e))
    }

    pub(crate) fn save_run(&mut self, rec: &RunRecord) -> Result<()> {
        let tx = self.immediate("begin run update")?;
        upsert_run(&tx, rec)?;
        tx.commit().map_err(|e| sql("commit run update", e))
    }

    pub(crate) fn get_run(&self, id: &RunId) -> Result<Option<RunRecord>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT record_json FROM runs WHERE run_id = ?1",
                params![id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| sql("read run", e))?;
        json.as_deref().map(decode_run).transpose()
    }

    pub(crate) fn list_runs(&self, f: &RunFilter) -> Result<Vec<RunRecord>> {
        let limit = match f.limit {
            0 => DEFAULT_RUN_LIMIT,
            n => n.min(MAX_RUN_LIMIT),
        };
        let action = f.action_ref.as_ref().map(ActionRef::to_string);
        let outcome = f.outcome.as_ref().map(enum_name).transpose()?;
        let (before_ms, before_id) = match &f.before {
            Some((ts, id)) => (Some(ts.unix_ms()), Some(id.as_str().to_owned())),
            None => (None, None),
        };
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT record_json FROM runs
                 WHERE (?1 IS NULL OR action_ref = ?1)
                   AND (?2 IS NULL OR outcome = ?2)
                   AND (?3 IS NULL OR started_at < ?3 OR (started_at = ?3 AND run_id < ?4))
                 ORDER BY started_at DESC, run_id DESC
                 LIMIT ?5",
            )
            .map_err(|e| sql("list runs", e))?;
        let rows: Vec<String> = stmt
            .query_map(
                params![action, outcome, before_ms, before_id, limit as i64],
                |r| r.get(0),
            )
            .and_then(|it| it.collect())
            .map_err(|e| sql("list runs", e))?;
        rows.iter().map(|j| decode_run(j)).collect()
    }
}
