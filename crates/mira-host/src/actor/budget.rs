//! Reply byte budgets shared by catalog, runs, logs, and views.
//!
//! The budget counts the whole serialized PublicReply, envelope included. Lists return only
//! whole items that fit. When the first item alone is too large, the caller returns its short
//! metadata and a payload reference instead, so every call makes progress.

use mira_protocol::ipc::RpcError;
use mira_protocol::limits::{DEFAULT_REPLY_BUDGET_BYTES, KIB, MAX_REPLY_BUDGET_BYTES};
use serde::Serialize;
use serde_json::Value;

use super::Handled;

/// Smallest accepted `max_bytes`: room for the envelope, metadata, and a payload reference.
pub(crate) const MIN_BUDGET_BYTES: usize = 4 * KIB;

/// The effective budget for a request (default 32 KiB, at most 256 KiB).
pub(crate) fn budget(max_bytes: Option<u32>) -> usize {
    max_bytes.map_or(DEFAULT_REPLY_BUDGET_BYTES, |b| {
        (b as usize).clamp(MIN_BUDGET_BYTES, MAX_REPLY_BUDGET_BYTES)
    })
}

/// Serialized size of a value in bytes.
pub(crate) fn json_len<T: Serialize + ?Sized>(v: &T) -> usize {
    serde_json::to_vec(v).map_or(usize::MAX, |b| b.len())
}

/// The outcome of fitting a list into a reply.
pub(crate) enum Fit {
    /// Whole items fit; the reply is ready. It is empty only when there were no candidates.
    Items { reply: Value },
    /// Candidates exist, but not even the first one fits.
    FirstTooLarge,
}

/// Finds the largest `n <= sizes.len()` whose reply `build(n)` serializes within `budget`.
/// `sizes` are the serialized sizes of the candidate items in reply order; they give the first
/// estimate, and the real serialization decides.
pub(crate) fn fit(
    sizes: &[usize],
    budget: usize,
    build: impl Fn(usize) -> Handled,
) -> Result<Fit, RpcError> {
    let empty = json_len(&build(0)?);
    let mut total = empty;
    let mut n = 0;
    for s in sizes {
        // One separator per item.
        let next = total.saturating_add(s.saturating_add(1));
        if next > budget {
            break;
        }
        total = next;
        n += 1;
    }
    loop {
        let reply = build(n)?;
        if json_len(&reply) <= budget {
            if n == 0 && !sizes.is_empty() {
                return Ok(Fit::FirstTooLarge);
            }
            return Ok(Fit::Items { reply });
        }
        if n == 0 {
            // Even the empty page is over budget; the caller's metadata is too large.
            return if sizes.is_empty() {
                Ok(Fit::Items { reply })
            } else {
                Ok(Fit::FirstTooLarge)
            };
        }
        n -= 1;
    }
}
