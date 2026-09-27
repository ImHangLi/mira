//! Published view snapshots and view revision blocks.

use mira_protocol::ids::{Digest, RunId, ViewRef, ViewRevision};
use mira_protocol::limits::MAX_SAFE_INTEGER;
use mira_protocol::time::Timestamp;
use rusqlite::{OptionalExtension, params};

use crate::storage::{StorageError, StoredView, VIEW_REVISION_BLOCK};

use super::{Db, Result, corrupt, enum_from_name, enum_name, i64_of, sql, u64_col};

impl Db {
    pub(crate) fn reserve_view_block(&mut self) -> Result<(u64, u64)> {
        let tx = self.immediate("begin view block reservation")?;
        let next: i64 = tx
            .query_row(
                "SELECT next_view_revision FROM workspace_meta WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .map_err(|e| sql("read next_view_revision", e))?;
        let first = u64_col(next, "next_view_revision")?;
        let end = first
            .checked_add(VIEW_REVISION_BLOCK)
            .filter(|end| end - 1 <= MAX_SAFE_INTEGER)
            .ok_or(StorageError::CounterExhausted)?;
        tx.execute(
            "UPDATE workspace_meta SET next_view_revision = ?1 WHERE id = 1",
            params![i64_of(end, "next_view_revision")?],
        )
        .map_err(|e| sql("write next_view_revision", e))?;
        tx.commit().map_err(|e| sql("commit view block", e))?;
        Ok((first, end))
    }

    pub(crate) fn save_view(&mut self, v: &StoredView) -> Result<()> {
        let tx = self.immediate("begin view save")?;
        let stored: Option<i64> = tx
            .query_row(
                "SELECT revision FROM views WHERE view_ref = ?1",
                params![v.view_ref.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| sql("read view", e))?;
        let rev = i64_of(v.revision.get(), "view revision")?;
        if let Some(s) = stored
            && s >= rev
        {
            return Err(StorageError::Unavailable(format!(
                "view {} revision {rev} is not newer than stored revision {s}",
                v.view_ref
            )));
        }
        let data_hash = Digest::of_bytes(v.data_json.as_bytes());
        tx.execute(
            "INSERT INTO views (view_ref, revision, kind, recorded_at, source_run_id, source_kind,
                                definition_hash, data_hash, data_bytes, data_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT (view_ref) DO UPDATE SET
                revision = excluded.revision, kind = excluded.kind,
                recorded_at = excluded.recorded_at, source_run_id = excluded.source_run_id,
                source_kind = excluded.source_kind, definition_hash = excluded.definition_hash,
                data_hash = excluded.data_hash, data_bytes = excluded.data_bytes,
                data_json = excluded.data_json",
            params![
                v.view_ref.to_string(),
                rev,
                enum_name(&v.kind)?,
                v.recorded_at.unix_ms(),
                v.source_run_id.as_ref().map(RunId::as_str),
                enum_name(&v.source_kind)?,
                v.definition_hash.as_str(),
                data_hash.as_str(),
                v.data_json.len() as i64,
                v.data_json,
            ],
        )
        .map_err(|e| sql("write view", e))?;
        tx.commit().map_err(|e| sql("commit view", e))
    }

    pub(crate) fn load_view(&self, view: &ViewRef) -> Result<Option<StoredView>> {
        type Row = (
            i64,
            String,
            i64,
            Option<String>,
            String,
            String,
            String,
            String,
        );
        let row: Option<Row> = self
            .conn
            .query_row(
                "SELECT revision, kind, recorded_at, source_run_id, source_kind,
                        definition_hash, data_hash, data_json
                 FROM views WHERE view_ref = ?1",
                params![view.to_string()],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| sql("read view", e))?;
        let Some((rev, kind, at, run, source_kind, def, data_hash, data_json)) = row else {
            return Ok(None);
        };
        if Digest::of_bytes(data_json.as_bytes()).as_str() != data_hash {
            return Err(StorageError::Corrupt(format!(
                "view {view} body does not match its stored hash"
            )));
        }
        Ok(Some(StoredView {
            view_ref: view.clone(),
            revision: ViewRevision::new(u64_col(rev, "view revision")?)
                .map_err(|e| corrupt("view revision", e))?,
            kind: enum_from_name("view kind", kind)?,
            recorded_at: Timestamp::from_unix_ms(at),
            source_run_id: run
                .map(RunId::parse)
                .transpose()
                .map_err(|e| corrupt("view source_run_id", e))?,
            source_kind: enum_from_name("view source_kind", source_kind)?,
            definition_hash: Digest::parse(def).map_err(|e| corrupt("view definition_hash", e))?,
            data_json,
        }))
    }
}
