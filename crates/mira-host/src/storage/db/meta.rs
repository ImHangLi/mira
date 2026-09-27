//! Workspace metadata: the catalog revision and persisted schedule switches.

use mira_protocol::ids::{ActionRef, CatalogRevision, Digest};
use mira_protocol::time::Timestamp;
use rusqlite::params;

use crate::storage::StorageError;

use super::{Db, Result, corrupt, i64_of, sql, u64_col};

impl Db {
    pub(crate) fn catalog(&self) -> Result<(CatalogRevision, Option<Digest>)> {
        let (rev, hash): (i64, Option<String>) = self
            .conn
            .query_row(
                "SELECT catalog_revision, catalog_set_hash FROM workspace_meta WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| sql("read catalog revision", e))?;
        let rev = CatalogRevision::new(u64_col(rev, "catalog_revision")?)
            .map_err(|e| corrupt("catalog_revision", e))?;
        let hash = hash
            .map(Digest::parse)
            .transpose()
            .map_err(|e| corrupt("catalog_set_hash", e))?;
        Ok((rev, hash))
    }

    pub(crate) fn set_schedule(&mut self, action_ref: &ActionRef, enabled: bool) -> Result<()> {
        let tx = self.immediate("begin schedule update")?;
        tx.execute(
            "INSERT INTO schedule_settings (action_ref, enabled, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (action_ref) DO UPDATE SET enabled = excluded.enabled, updated_at = excluded.updated_at",
            params![action_ref.to_string(), i64::from(enabled), Timestamp::now().unix_ms()],
        )
        .map_err(|e| sql("write schedule setting", e))?;
        tx.commit().map_err(|e| sql("commit schedule setting", e))
    }

    pub(crate) fn list_schedules(&mut self) -> Result<Vec<(ActionRef, bool)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT action_ref, enabled FROM schedule_settings ORDER BY action_ref")
            .map_err(|e| sql("read schedule settings", e))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| sql("read schedule settings", e))?;
        let mut out = Vec::new();
        for row in rows {
            let (r, enabled) = row.map_err(|e| sql("read schedule setting", e))?;
            // Rows for refs that no longer parse are ignored rather than failing the host.
            if let Ok(action_ref) = r.parse::<ActionRef>() {
                out.push((action_ref, enabled == 1));
            }
        }
        Ok(out)
    }

    pub(crate) fn accept_catalog(&mut self, set_hash: &Digest) -> Result<CatalogRevision> {
        let (rev, hash) = self.catalog()?;
        if hash.as_ref() == Some(set_hash) {
            return Ok(rev);
        }
        let next = rev.next().ok_or(StorageError::CounterExhausted)?;
        let tx = self.immediate("begin catalog accept")?;
        tx.execute(
            "UPDATE workspace_meta SET catalog_revision = ?1, catalog_set_hash = ?2 WHERE id = 1",
            params![i64_of(next.get(), "catalog_revision")?, set_hash.as_str()],
        )
        .map_err(|e| sql("write catalog revision", e))?;
        tx.commit().map_err(|e| sql("commit catalog revision", e))?;
        Ok(next)
    }
}
