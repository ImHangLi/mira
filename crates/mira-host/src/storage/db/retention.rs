//! Retention: selecting and removing expired runs, views, and request keys.

use mira_protocol::ids::{RunId, ViewRef};
use rusqlite::params;

use crate::storage::{GcPolicy, GcSelection};

use super::{Db, Result, i64_of, sql};

impl Db {
    fn run_ids(
        &self,
        sql_text: &str,
        args: &[&dyn rusqlite::ToSql],
        what: &str,
    ) -> Result<Vec<RunId>> {
        let mut stmt = self.conn.prepare(sql_text).map_err(|e| sql(what, e))?;
        let rows = stmt
            .query_map(args, |r| r.get::<_, String>(0))
            .map_err(|e| sql(what, e))?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(id) = RunId::parse(row.map_err(|e| sql(what, e))?) {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// Selects finished runs, views, and request keys beyond retention.
    /// Active runs are excluded; request keys referring to active runs are kept.
    pub(crate) fn gc_select(
        &self,
        p: &GcPolicy,
        now_ms: i64,
        active: &[RunId],
    ) -> Result<GcSelection> {
        const DAY: i64 = 86_400_000;
        let cut = |days: u64| {
            now_ms.saturating_sub(
                i64::try_from(days)
                    .unwrap_or(i64::MAX / DAY)
                    .saturating_mul(DAY),
            )
        };
        let active: std::collections::HashSet<&str> = active.iter().map(RunId::as_str).collect();
        let keep_active = |v: Vec<RunId>| -> Vec<RunId> {
            v.into_iter()
                .filter(|r| !active.contains(r.as_str()))
                .collect()
        };
        let per_action = i64_of(p.runs_per_action, "runs_per_action")?;
        let per_ws = i64_of(p.runs_per_workspace, "runs_per_workspace")?;
        let mut runs = self.run_ids(
            "SELECT run_id FROM runs WHERE lifecycle = 'finished' AND COALESCE(ended_at, started_at) < ?1
             UNION
             SELECT run_id FROM (SELECT run_id, lifecycle, ROW_NUMBER() OVER (PARTITION BY action_ref ORDER BY started_at DESC) AS rn
                                 FROM runs WHERE action_ref IS NOT NULL) WHERE rn > ?2 AND lifecycle = 'finished'
             UNION
             SELECT run_id FROM (SELECT run_id, lifecycle, ROW_NUMBER() OVER (ORDER BY started_at DESC) AS rn FROM runs)
                          WHERE rn > ?3 AND lifecycle = 'finished'",
            &[&cut(p.history_days), &per_action, &per_ws],
            "select expired runs",
        )?;
        runs = keep_active(runs);
        let doomed: std::collections::HashSet<&str> = runs.iter().map(RunId::as_str).collect();
        let older = |days: u64, what: &str| -> Result<Vec<RunId>> {
            Ok(self
                .run_ids(
                    "SELECT run_id FROM runs WHERE lifecycle = 'finished' AND COALESCE(ended_at, started_at) < ?1",
                    &[&cut(days)],
                    what,
                )?
                .into_iter()
                .filter(|r| !doomed.contains(r.as_str()) && !active.contains(r.as_str()))
                .collect())
        };
        let old_logs = older(p.log_days, "select old logs")?;
        let old_artifacts = older(p.artifact_days, "select old artifacts")?;
        let finished_oldest_first = keep_active(self.run_ids(
            "SELECT run_id FROM runs WHERE lifecycle = 'finished' ORDER BY COALESCE(ended_at, started_at) ASC",
            &[],
            "list finished runs",
        )?);
        let mut views = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT view_ref, revision FROM views WHERE recorded_at < ?1")
                .map_err(|e| sql("select expired views", e))?;
            let rows = stmt
                .query_map(params![cut(p.view_days)], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .map_err(|e| sql("select expired views", e))?;
            for row in rows {
                let (v, rev) = row.map_err(|e| sql("select expired view", e))?;
                if let Ok(v) = v.parse::<ViewRef>() {
                    views.push((v, u64::try_from(rev).unwrap_or(0)));
                }
            }
        }
        let request_keys: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM request_keys WHERE created_at < ?1",
                params![now_ms.saturating_sub(DAY)],
                |r| r.get(0),
            )
            .map_err(|e| sql("count expired request keys", e))?;
        Ok(GcSelection {
            runs,
            old_logs,
            old_artifacts,
            finished_oldest_first,
            views,
            request_keys: u64::try_from(request_keys).unwrap_or(0),
        })
    }

    /// Removes the selected index rows in one transaction; managed files are removed by the
    /// caller first. Expired request keys go too, except those pointing at active runs.
    pub(crate) fn gc_apply(
        &mut self,
        sel: &GcSelection,
        now_ms: i64,
        active: &[RunId],
    ) -> Result<()> {
        let tx = self.immediate("begin retention cleanup")?;
        for id in &sel.runs {
            tx.execute(
                "DELETE FROM runs WHERE run_id = ?1 AND lifecycle = 'finished'",
                params![id.as_str()],
            )
            .map_err(|e| sql("delete expired run", e))?;
        }
        for (v, rev) in &sel.views {
            let rev = i64_of(*rev, "view revision")?;
            tx.execute(
                "DELETE FROM views WHERE view_ref = ?1 AND revision = ?2",
                params![v.to_string(), rev],
            )
            .map_err(|e| sql("delete expired view", e))?;
            tx.execute(
                "INSERT INTO cleaned_views (view_ref, revision, cleaned_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (view_ref) DO UPDATE SET revision = excluded.revision, cleaned_at = excluded.cleaned_at",
                params![v.to_string(), rev, now_ms],
            )
            .map_err(|e| sql("record cleaned view", e))?;
        }
        let active: Vec<String> = active.iter().map(ToString::to_string).collect();
        let placeholders = if active.is_empty() {
            "''".to_owned()
        } else {
            vec!["?"; active.len()].join(",")
        };
        let cutoff = now_ms.saturating_sub(86_400_000);
        let q = format!(
            "DELETE FROM request_keys WHERE created_at < {cutoff} AND reference NOT IN ({placeholders})"
        );
        tx.execute(&q, rusqlite::params_from_iter(active.iter()))
            .map_err(|e| sql("delete expired request keys", e))?;
        tx.commit()
            .map_err(|e| sql("commit retention cleanup", e))?;
        // Return freed pages gradually; a full VACUUM never runs on the hot path.
        let _ = self.conn.execute_batch("PRAGMA incremental_vacuum(256);");
        Ok(())
    }

    /// Views removed by retention: (view, last revision, cleaned_at ms).
    pub(crate) fn cleaned_views(&self) -> Result<Vec<(ViewRef, u64, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT view_ref, revision, cleaned_at FROM cleaned_views")
            .map_err(|e| sql("read cleaned views", e))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| sql("read cleaned views", e))?;
        let mut out = Vec::new();
        for row in rows {
            let (v, rev, at) = row.map_err(|e| sql("read cleaned view", e))?;
            if let Ok(v) = v.parse::<ViewRef>() {
                out.push((v, u64::try_from(rev).unwrap_or(0), at));
            }
        }
        Ok(out)
    }
}
