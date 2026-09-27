//! Bounded history reads: run provenance, run records that stay small,
//! and log pages in both cursor directions.

use mira_protocol::config::ConfigSet;
use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::RunId;
use mira_protocol::ipc::LogPage;
use mira_protocol::limits::KIB;
use mira_protocol::reply::{ReplyContext, ReplyMeta};
use mira_protocol::run::{Lifecycle, LogRecord, Outcome, RunProvenance, RunRecord};
use mira_protocol::view::Freshness;
use serde_json::{Map, Value};

use super::budget::{self, Fit};
use super::cursor::{self, Kind, Pos};
use super::payloads::Payloads;
use super::{Handled, reply_fail, reply_ok};
use crate::logs::RunLog;

/// Result data above this size is kept by payload reference, not inline in the run record.
pub(crate) const INLINE_RESULT_BYTES: usize = 8 * KIB;

/// Read-time provenance of one run against the current definitions.
pub(crate) fn provenance(rec: &RunRecord, set: Option<&ConfigSet>) -> RunProvenance {
    let current = match &rec.action_ref {
        // Ad-hoc exec runs are defined by their argv, which cannot change afterwards.
        None => Some(rec.definition_hash.clone()),
        Some(a) => set.and_then(|s| s.action(a).map(|(_, act)| act.definition_hash.clone())),
    };
    let definition_current = current.as_ref() == Some(&rec.definition_hash);
    let (freshness, reason) = match (rec.lifecycle, &current) {
        (_, None) => (
            Freshness::Stale,
            "the action is no longer in the accepted catalog; this run is not evidence for any current tool".to_owned(),
        ),
        (_, Some(now)) if !definition_current => (
            Freshness::Stale,
            format!(
                "the action definition changed after this run (run used {}, current is {now}); \
                 its result is not evidence for the current definition",
                rec.definition_hash
            ),
        ),
        (l, _) if l.is_active() => (
            Freshness::Current,
            "the run is active with the current definition; its state is live".to_owned(),
        ),
        (
            Lifecycle::Finished {
                outcome: Outcome::Interrupted,
            },
            _,
        ) => (
            Freshness::Stale,
            "the host stopped before this run finished; its real outcome is unknown".to_owned(),
        ),
        _ => (
            Freshness::Historical,
            format!(
                "one past execution{}; it proves that execution only, not the current state",
                rec.ended_at
                    .map(|t| format!(" that ended at {t}"))
                    .unwrap_or_default()
            ),
        ),
    };
    RunProvenance {
        freshness,
        freshness_reason: reason,
        definition_current,
        current_definition_hash: current,
    }
}

/// Prepares a record for a reply: provenance added, and inline result data above the inline
/// bound (records written before payload references existed) moved into a session payload.
pub(crate) fn prepare_run(rec: &mut RunRecord, set: Option<&ConfigSet>, payloads: &Payloads) {
    rec.provenance = Some(provenance(rec, set));
    if let Some(res) = rec.result.as_mut()
        && res.payload.is_none()
        && !res.data.is_null()
    {
        let bytes = serde_json::to_vec(&res.data).unwrap_or_default();
        if bytes.len() > INLINE_RESULT_BYTES {
            let key = format!("run-result:{}", rec.run_id);
            res.payload = Some(payloads.hold(Some(key), bytes));
            res.data = Value::Null;
        }
    }
}

#[derive(Clone, Copy)]
enum Direction {
    /// Newest records first read, continuing toward older ones: `log_seq < before`.
    Backward { before: Option<u64> },
    /// Oldest first within `[from, upto]`, continuing toward newer ones (gap resume).
    Forward { from: u64, upto: u64 },
}

fn expired(run_id: &RunId, first: Option<u64>, resume: Option<String>, what: &str) -> ErrorInfo {
    let mut details = Map::new();
    details.insert("run_id".into(), Value::String(run_id.to_string()));
    details.insert(
        "first_available_seq".into(),
        first.map_or(Value::Null, Value::from),
    );
    let e = ErrorInfo::new(
        ErrorCode::CURSOR_EXPIRED,
        match first {
            Some(f) => {
                format!("{what} were removed by log retention; the earliest readable record is {f}")
            }
            None => format!("{what} were removed by log retention; no records remain"),
        },
    )
    .with_details(details);
    match resume {
        Some(c) => e.with_next_action(
            &["mira", "logs", run_id.as_str(), "--after", &c],
            "Continue from the earliest record still kept.",
        ),
        None => e.with_next_action(
            &["mira", "logs", run_id.as_str()],
            "Read the current tail instead.",
        ),
    }
}

/// A forward log cursor over `[from, upto]`, as used by stream gap events.
pub(crate) fn forward_cursor(run_id: &RunId, from: u64, upto: u64) -> String {
    cursor::encode(
        Kind::Logs,
        run_id.as_str(),
        "",
        Pos {
            a: Some(from),
            u: Some(upto),
            ..Pos::default()
        },
        None,
    )
}

fn backward_cursor(run_id: &RunId, before: u64) -> String {
    cursor::encode(
        Kind::Logs,
        run_id.as_str(),
        "",
        Pos {
            b: Some(before),
            ..Pos::default()
        },
        None,
    )
}

/// One page of a run log within the reply budget (runs on a blocking thread).
pub(crate) fn log_page(
    log: &mut RunLog,
    run_id: RunId,
    raw_cursor: Option<&str>,
    limit: usize,
    max_bytes: Option<u32>,
    ctx: ReplyContext,
    payloads: &Payloads,
) -> Handled {
    let budget = budget::budget(max_bytes);
    let dir = match raw_cursor
        .map(|c| cursor::decode(c, Kind::Logs, run_id.as_str(), ""))
        .transpose()
    {
        Err(e) => return reply_fail(ctx, e),
        Ok(None) => Direction::Backward { before: None },
        Ok(Some(c)) => match (c.pos.b, c.pos.a, c.pos.u) {
            (Some(b), None, None) => Direction::Backward { before: Some(b) },
            (None, Some(from), Some(upto)) => Direction::Forward { from, upto },
            _ => {
                return reply_fail(
                    ctx,
                    ErrorInfo::new(ErrorCode::INVALID_ARGUMENT, "invalid cursor: no position"),
                );
            }
        },
    };
    let first = log.first_available().map(|s| s.get());
    let last = log.last_available();
    // History that the cursor still expects but retention removed is reported, never skipped.
    match dir {
        Direction::Backward { before: Some(b) } if first.is_none_or(|f| f >= b) => {
            return reply_fail(ctx, expired(&run_id, first, None, "the older records"));
        }
        Direction::Forward { from, upto } if first.is_none_or(|f| f > from) => {
            let resume = first
                .filter(|f| *f <= upto)
                .map(|f| forward_cursor(&run_id, f, upto));
            return reply_fail(
                ctx,
                expired(&run_id, first, resume, "some of these records"),
            );
        }
        _ => {}
    }
    // Candidates in the order they are taken: newest first (backward) or oldest first.
    let (candidates, backward) = match dir {
        Direction::Backward { before } => {
            let mut v = log.read_before(before, limit);
            v.reverse();
            (v, true)
        }
        Direction::Forward { from, upto } => (log.read_range(from, upto, limit), false),
    };
    let page_items = |n: usize| -> Vec<LogRecord> {
        let mut items: Vec<LogRecord> = candidates[..n].to_vec();
        if backward {
            items.reverse();
        }
        items
    };
    // Where to continue after taking `n` candidates, if anything remains.
    let next_after = |n: usize| -> Option<String> {
        let taken = candidates.get(n.checked_sub(1)?)?;
        let seq = taken.log_seq.get();
        match dir {
            Direction::Backward { .. } => first
                .is_some_and(|f| seq > f)
                .then(|| backward_cursor(&run_id, seq)),
            Direction::Forward { upto, .. } => {
                (seq < upto).then(|| forward_cursor(&run_id, seq + 1, upto))
            }
        }
    };
    let page = |items: Vec<LogRecord>| LogPage {
        run_id: run_id.clone(),
        items,
        first_available_seq: log_seq(first),
        last_available_seq: last,
    };
    let sizes: Vec<usize> = candidates.iter().map(budget::json_len).collect();
    let fitted = budget::fit(&sizes, budget, |n| {
        let next = next_after(n);
        reply_ok(
            ctx.clone(),
            page(page_items(n)),
            ReplyMeta {
                truncated: next.is_some(),
                next_cursor: next,
                ..ReplyMeta::default()
            },
        )
    });
    match fitted {
        Ok(Fit::Items { reply, .. }) => Ok(reply),
        Ok(Fit::FirstTooLarge) => {
            // One record alone exceeds the budget: reference it and move past it.
            let rec = &candidates[0];
            let bytes = serde_json::to_vec(rec).unwrap_or_default();
            let payload = payloads.hold(Some(format!("log:{run_id}:{}", rec.log_seq.get())), bytes);
            reply_ok(
                ctx,
                page(Vec::new()),
                ReplyMeta {
                    truncated: true,
                    next_cursor: next_after(1),
                    not_modified: false,
                    payload: Some(payload),
                },
            )
        }
        Err(e) => Err(e),
    }
}

fn log_seq(v: Option<u64>) -> Option<mira_protocol::ids::LogSeq> {
    v.and_then(|s| mira_protocol::ids::LogSeq::new(s).ok())
}
