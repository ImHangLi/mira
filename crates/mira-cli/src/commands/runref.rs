//! Run references on the command line: a full run ID, a unique prefix of one (such as the
//! `r_fb60aacf` the TUI shows), or an action ref.

use mira_client::Client;
use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::{ActionRef, RunId};
use mira_protocol::ipc::{Method, RunList, RunListParams, RunTarget};
use mira_protocol::reply::PublicReply;

/// Pages read while resolving a prefix; the run list is newest first.
const MAX_PAGES: usize = 50;
const PAGE_LIMIT: u32 = 100;
const PAGE_BYTES: u32 = 256 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum PrefixMatch {
    One(RunId),
    None,
    Many(usize),
}

/// Whether `s` looks like a (possibly shortened) run ID: `r_` and 1 to 32 lowercase hex digits.
pub fn is_run_prefix(s: &str) -> bool {
    s.strip_prefix("r_").is_some_and(|hex| {
        (1..=32).contains(&hex.len()) && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Matches `prefix` against run IDs; duplicates of one ID count once.
pub fn match_prefix<'a>(prefix: &str, ids: impl IntoIterator<Item = &'a RunId>) -> PrefixMatch {
    let mut found: Vec<&RunId> = Vec::new();
    for id in ids {
        if id.as_str().starts_with(prefix) && !found.contains(&id) {
            found.push(id);
        }
    }
    match found.as_slice() {
        [] => PrefixMatch::None,
        [one] => PrefixMatch::One((*one).clone()),
        many => PrefixMatch::Many(many.len()),
    }
}

pub fn no_run(s: &str) -> ErrorInfo {
    ErrorInfo::new(
        ErrorCode::NOT_FOUND,
        format!("No run `{s}`. See `mira runs`."),
    )
    .with_next_action(&["mira", "runs"], "List recent runs and their IDs.")
}

async fn list(
    client: &mut Client,
    action_ref: Option<ActionRef>,
    cursor: Option<String>,
    limit: u32,
) -> Result<PublicReply<RunList>, ErrorInfo> {
    client
        .call(
            Method::RunList,
            &RunListParams {
                action_ref,
                outcome: None,
                cursor,
                limit: Some(limit),
                max_bytes: Some(PAGE_BYTES),
            },
        )
        .await
        .map_err(|e| e.to_error_info())
}

/// Resolves a full run ID or a unique prefix of one through the host's run list.
pub async fn resolve_run_id(client: &mut Client, s: &str) -> Result<RunId, ErrorInfo> {
    if !is_run_prefix(s) {
        return Err(no_run(s));
    }
    if let Ok(id) = RunId::parse(s.to_owned()) {
        return Ok(id);
    }
    let mut ids: Vec<RunId> = Vec::new();
    let mut cursor = None;
    for _ in 0..MAX_PAGES {
        let page = list(client, None, cursor.take(), PAGE_LIMIT).await?;
        if let Some(e) = page.error() {
            return Err(e.clone());
        }
        ids.extend(
            page.data()
                .into_iter()
                .flat_map(|l| l.runs.iter().map(|r| r.run_id.clone())),
        );
        match page.meta().next_cursor.clone() {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    match match_prefix(s, &ids) {
        PrefixMatch::One(id) => Ok(id),
        PrefixMatch::None => Err(no_run(s)),
        PrefixMatch::Many(n) => Err(ErrorInfo::new(
            ErrorCode::INVALID_ARGUMENT,
            format!("`{s}` matches {n} runs. Type more of the run ID. See `mira runs`."),
        )
        .with_next_action(&["mira", "runs"], "List recent runs and their IDs.")),
    }
}

/// A run ID, a unique run ID prefix, or an action ref.
pub async fn resolve_target(client: &mut Client, s: &str) -> Result<RunTarget, ErrorInfo> {
    if s.starts_with("r_") {
        return resolve_run_id(client, s)
            .await
            .map(|run_id| RunTarget::Run { run_id });
    }
    s.parse::<ActionRef>()
        .map(|action_ref| RunTarget::Action { action_ref })
        .map_err(|e| crate::output::invalid_argument(format!("{e}: `{s}`")))
}

/// Like [`resolve_target`], but an action ref becomes its current or latest run.
pub async fn resolve_run(client: &mut Client, s: &str) -> Result<RunId, ErrorInfo> {
    match resolve_target(client, s).await? {
        RunTarget::Run { run_id } => Ok(run_id),
        RunTarget::Action { action_ref } => {
            let page = list(client, Some(action_ref.clone()), None, 1).await?;
            if let Some(e) = page.error() {
                return Err(e.clone());
            }
            page.data()
                .and_then(|l| l.runs.first())
                .map(|r| r.run_id.clone())
                .ok_or_else(|| {
                    ErrorInfo::new(
                        ErrorCode::NOT_FOUND,
                        format!("`{action_ref}` has not run yet."),
                    )
                    .with_next_action(
                        &["mira", "run", &action_ref.to_string(), "--no-wait"],
                        "Start it first.",
                    )
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> RunId {
        RunId::parse(s.to_owned()).unwrap()
    }

    #[test]
    fn a_prefix_matches_one_none_or_many() {
        let a = id("r_fb60aacf5a8647da941eb0044d0caeaa");
        let b = id("r_fb60aacf5a8647da941eb0044d0caeab");
        let c = id("r_0123456789ab4def8123456789abcdef");
        let ids = [a.clone(), b.clone(), c.clone(), a.clone()];
        assert_eq!(match_prefix("r_0123", &ids), PrefixMatch::One(c));
        assert_eq!(match_prefix("r_fb60aacf", &ids), PrefixMatch::Many(2));
        assert_eq!(
            match_prefix("r_fb60aacf5a8647da941eb0044d0caeaa", &ids),
            PrefixMatch::One(a)
        );
        assert_eq!(match_prefix("r_ffff", &ids), PrefixMatch::None);
    }

    #[test]
    fn only_hex_after_r_is_a_run_prefix() {
        assert!(is_run_prefix("r_fb60aacf"));
        assert!(!is_run_prefix("r_"));
        assert!(!is_run_prefix("r_XYZ"));
        assert!(!is_run_prefix("dev.web"));
        assert!(!is_run_prefix(&format!("r_{}", "a".repeat(33))));
    }
}
