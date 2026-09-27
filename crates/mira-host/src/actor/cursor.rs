//! Opaque paging cursors: base64url of a small JSON object that names the data source, a
//! hash of the filter, the position, and the revision it is bound to.
//!
//! Cursors are not credentials and never carry secrets, env values, or raw query text: the
//! filter is stored only as a short hash. Decoding is strict: anything else is invalid.

use base64::Engine as _;
use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::limits::MAX_CURSOR_BYTES;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Catalog,
    Runs,
    Logs,
    View,
}

impl Kind {
    fn tag(self) -> &'static str {
        match self {
            Self::Catalog => "catalog",
            Self::Runs => "runs",
            Self::Logs => "logs",
            Self::View => "view",
        }
    }
}

/// Kind-specific position. Unused fields stay absent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pos {
    /// Offset into a fixed, revision-bound list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub o: Option<u64>,
    /// Keyset: start time (unix ms) and ID of the last returned run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Logs, backward: read records with `log_seq < b`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b: Option<u64>,
    /// Logs, forward: read records with `a <= log_seq <= u`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub u: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    k: String,
    /// Source ID: workspace, run, or view.
    s: String,
    /// Filter hash (16 hex characters).
    f: String,
    p: Pos,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    r: Option<u64>,
}

/// A decoded cursor whose kind, source, and filter already matched the request.
#[derive(Debug, Clone)]
pub(crate) struct Cursor {
    pub pos: Pos,
    pub revision: Option<u64>,
}

/// Short hash of a canonical filter value; the raw filter never enters the cursor.
pub(crate) fn filter_hash(filter: &Value) -> String {
    let digest = Sha256::digest(filter.to_string().as_bytes());
    hex::encode(&digest[..8])
}

pub(crate) fn encode(
    kind: Kind,
    source: &str,
    filter: &str,
    pos: Pos,
    revision: Option<u64>,
) -> String {
    let wire = Wire {
        k: kind.tag().to_owned(),
        s: source.to_owned(),
        f: filter.to_owned(),
        p: pos,
        r: revision,
    };
    let json = serde_json::to_vec(&wire).unwrap_or_default();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

fn invalid(msg: impl Into<String>) -> ErrorInfo {
    ErrorInfo::new(ErrorCode::INVALID_ARGUMENT, msg)
}

/// Decodes and checks a cursor against the request it continues.
pub(crate) fn decode(s: &str, kind: Kind, source: &str, filter: &str) -> Result<Cursor, ErrorInfo> {
    if s.is_empty() || s.len() > MAX_CURSOR_BYTES {
        return Err(invalid("invalid cursor: expected 1-2048 bytes"));
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|_| invalid("invalid cursor: not a Mira cursor"))?;
    let wire: Wire =
        serde_json::from_slice(&bytes).map_err(|_| invalid("invalid cursor: not a Mira cursor"))?;
    if wire.k != kind.tag() {
        return Err(invalid(format!(
            "the cursor continues a `{}` read, not a `{}` read",
            wire.k,
            kind.tag()
        )));
    }
    if wire.s != source {
        return Err(invalid(format!(
            "the cursor belongs to another {} (`{}`)",
            match kind {
                Kind::Catalog | Kind::Runs => "workspace",
                Kind::Logs => "run",
                Kind::View => "view",
            },
            wire.s
        )));
    }
    if wire.f != filter {
        return Err(invalid(
            "the cursor was made for a different query or filter; repeat the original arguments",
        ));
    }
    Ok(Cursor {
        pos: wire.p,
        revision: wire.r,
    })
}
