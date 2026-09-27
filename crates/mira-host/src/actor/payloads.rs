//! Payload references: large host-held values returned by reference and read in
//! bounded UTF-8 chunks with `payload.read`.
//!
//! Two sources exist. `session` payloads are serialized copies of bounded values (view bodies,
//! single oversized rows or records) kept in a bounded in-memory LRU; they are gone when the
//! session ends, the LRU evicts them, or the host exits. `retained` payloads are large run
//! results written once to the workspace state directory within `result_bytes_per_workspace`.
//! A read never re-runs anything: missing data is PAYLOAD_GONE.

use std::collections::VecDeque;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::{Digest, RunId};
use mira_protocol::ipc::{ChunkData, PayloadReadParams};
use mira_protocol::reply::{PayloadAvailability, PayloadMime, PayloadRef, ReplyMeta};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Actor, Responder, reply_fail, reply_ok};
use crate::diag;

/// In-memory payloads kept per host (bytes and entries, whichever limit is hit first).
const MEM_BYTES: usize = 64 * 1024 * 1024;
const MEM_ENTRIES: usize = 512;
pub(crate) const DEFAULT_CHUNK_BYTES: u32 = 16 * 1024;
const MAX_CHUNK_BYTES: u32 = 128 * 1024;
const TOKEN_VERSION: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    /// Held in host memory.
    Mem,
    /// A retained run result file.
    Run,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenWire {
    v: u64,
    k: String,
    m: Source,
    id: String,
    /// First 16 hex characters of the content hash.
    h: String,
}

struct Entry {
    token: String,
    /// Deduplication key, e.g. a view ref and revision.
    key: Option<String>,
    data: Arc<Vec<u8>>,
    session: bool,
}

#[derive(Default)]
struct Inner {
    entries: VecDeque<Entry>,
    bytes: usize,
}

impl Inner {
    fn insert(&mut self, e: Entry) {
        self.bytes += e.data.len();
        self.entries.push_back(e);
        while self.bytes > MEM_BYTES || self.entries.len() > MEM_ENTRIES {
            match self.entries.pop_front() {
                Some(old) => self.bytes = self.bytes.saturating_sub(old.data.len()),
                None => break,
            }
        }
    }

    fn get(&mut self, token: &str) -> Option<Arc<Vec<u8>>> {
        let i = self.entries.iter().position(|e| e.token == token)?;
        let e = self.entries.remove(i)?;
        let data = e.data.clone();
        self.entries.push_back(e);
        Some(data)
    }
}

/// Shared handle: the actor and its spawned read tasks use the same store.
#[derive(Clone)]
pub(crate) struct Payloads {
    inner: Arc<Mutex<Inner>>,
    results_dir: PathBuf,
}

fn token(source: Source, id: &str, sha: &Digest) -> String {
    let hash = sha.as_str().trim_start_matches("sha256:");
    let wire = TokenWire {
        v: TOKEN_VERSION,
        k: "payload".into(),
        m: source,
        id: id.to_owned(),
        h: hash.chars().take(16).collect(),
    };
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&wire).unwrap_or_default())
}

fn parse_token(s: &str) -> Result<TokenWire, ErrorInfo> {
    let bad = || ErrorInfo::new(ErrorCode::INVALID_ARGUMENT, "invalid payload token");
    if s.is_empty() || s.len() > mira_protocol::limits::MAX_CURSOR_BYTES {
        return Err(bad());
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|_| bad())?;
    let w: TokenWire = serde_json::from_slice(&bytes).map_err(|_| bad())?;
    if w.k != "payload" || w.v != TOKEN_VERSION || w.h.len() != 16 {
        return Err(bad());
    }
    Ok(w)
}

fn payload_ref(token: String, data: &[u8], availability: PayloadAvailability) -> PayloadRef {
    PayloadRef {
        token,
        mime: PayloadMime::Json,
        size_bytes: data.len() as u64,
        sha256: Digest::of_bytes(data),
        availability,
    }
}

fn gone(message: String, next: Option<(Vec<String>, &str)>) -> ErrorInfo {
    let e = ErrorInfo::new(ErrorCode::PAYLOAD_GONE, message);
    match next {
        Some((argv, reason)) => {
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            e.with_next_action(&argv, reason)
        }
        None => e,
    }
}

fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Removes the oldest result files until the directory fits `cap` bytes.
fn enforce_cap(dir: &Path, cap: u64) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            if !m.is_file() {
                return None;
            }
            Some((m.modified().ok()?, m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort();
    for (_, len, path) in files {
        if total <= cap {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

/// One bounded chunk on UTF-8 code point boundaries.
fn chunk(data: &[u8], offset: u64, max: usize) -> Result<(String, Option<u64>, bool), ErrorInfo> {
    let text = std::str::from_utf8(data)
        .map_err(|_| ErrorInfo::new(ErrorCode::INTERNAL, "payload is not UTF-8"))?;
    let len = text.len();
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    if start > len {
        return Err(ErrorInfo::new(
            ErrorCode::INVALID_ARGUMENT,
            format!("offset {offset} is past the end ({len} bytes)"),
        ));
    }
    if !text.is_char_boundary(start) {
        return Err(ErrorInfo::new(
            ErrorCode::INVALID_ARGUMENT,
            "offset is not at a UTF-8 code point boundary; continue from next_offset",
        ));
    }
    let mut end = start.saturating_add(max).min(len);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == start && start < len {
        // max is smaller than one code point: return that one code point to make progress.
        end = start + 1;
        while !text.is_char_boundary(end) {
            end += 1;
        }
    }
    let done = end >= len;
    Ok((
        text[start..end].to_owned(),
        (!done).then_some(end as u64),
        done,
    ))
}

/// Selects a JSON Pointer inside the payload and returns its serialization.
fn select(data: &[u8], pointer: Option<&str>) -> Result<Arc<Vec<u8>>, ErrorInfo> {
    let Some(p) = pointer else {
        return Ok(Arc::new(data.to_vec()));
    };
    if !p.is_empty() && !p.starts_with('/') {
        return Err(ErrorInfo::new(
            ErrorCode::INVALID_ARGUMENT,
            "pointer must be a JSON Pointer such as /rows/0 (or empty for the whole value)",
        ));
    }
    let value: Value = serde_json::from_slice(data)
        .map_err(|e| ErrorInfo::new(ErrorCode::INTERNAL, format!("payload is not JSON: {e}")))?;
    let sub = value.pointer(p).ok_or_else(|| {
        ErrorInfo::new(
            ErrorCode::NOT_FOUND,
            format!("the payload has no value at pointer `{p}`"),
        )
        .with_pointer(p)
    })?;
    serde_json::to_vec(sub)
        .map(Arc::new)
        .map_err(|e| ErrorInfo::new(ErrorCode::INTERNAL, e.to_string()))
}

impl Payloads {
    pub fn new(results_dir: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            results_dir,
        }
    }

    /// Holds a serialized value in memory for this session. The same `key` returns the same
    /// token while the entry lives, so repeated reads of one view revision do not copy again.
    pub fn hold(&self, key: Option<String>, data: Vec<u8>) -> PayloadRef {
        let Ok(mut inner) = self.inner.lock() else {
            return payload_ref(String::new(), &data, PayloadAvailability::Session);
        };
        if let Some(k) = &key
            && let Some(e) = inner.entries.iter().find(|e| e.key.as_ref() == Some(k))
        {
            return payload_ref(e.token.clone(), &e.data, PayloadAvailability::Session);
        }
        let mut id = [0u8; 16];
        let _ = getrandom::fill(&mut id);
        let r = payload_ref(String::new(), &data, PayloadAvailability::Session);
        let t = token(Source::Mem, &hex::encode(id), &r.sha256);
        inner.insert(Entry {
            token: t.clone(),
            key,
            data: Arc::new(data),
            session: true,
        });
        PayloadRef { token: t, ..r }
    }

    /// Keeps a large run result: written once to the state directory (bounded by `cap`
    /// bytes per workspace) and cached in memory until the write is done.
    pub fn retain_result(&self, run_id: &RunId, data: Vec<u8>, cap: u64) -> PayloadRef {
        let r = payload_ref(String::new(), &data, PayloadAvailability::Retained);
        let t = token(Source::Run, run_id.as_str(), &r.sha256);
        let data = Arc::new(data);
        if let Ok(mut inner) = self.inner.lock() {
            inner.insert(Entry {
                token: t.clone(),
                key: None,
                data: data.clone(),
                session: false,
            });
        }
        let path = self.results_dir.join(format!("{run_id}.json"));
        let dir = self.results_dir.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = write_private(&path, &data) {
                diag(format!("could not retain a run result: {e}"));
            }
            enforce_cap(&dir, cap);
        });
        PayloadRef { token: t, ..r }
    }

    /// Drops cached copies of retained results whose runs retention removed, so reads of
    /// their tokens return PAYLOAD_GONE instead of serving a cleaned-up result.
    pub fn forget_runs(&self, runs: &[RunId]) {
        let ids: std::collections::HashSet<&str> = runs.iter().map(RunId::as_str).collect();
        if let Ok(mut inner) = self.inner.lock() {
            inner.entries.retain(|e| {
                parse_token(&e.token).map_or(true, |w| {
                    !(matches!(w.m, Source::Run) && ids.contains(w.id.as_str()))
                })
            });
            inner.bytes = inner.entries.iter().map(|e| e.data.len()).sum();
        }
    }

    /// Session payloads end with the session.
    pub fn clear_session(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            let before = inner.entries.len();
            inner.entries.retain(|e| !e.session);
            if inner.entries.len() != before {
                inner.bytes = inner.entries.iter().map(|e| e.data.len()).sum();
            }
        }
    }

    async fn load(&self, t: &str) -> Result<Arc<Vec<u8>>, ErrorInfo> {
        let w = parse_token(t)?;
        if let Some(data) = self.inner.lock().ok().and_then(|mut i| i.get(t)) {
            return Ok(data);
        }
        match w.m {
            Source::Mem => Err(gone(
                "this payload was held in host memory and is gone (the session ended, the host \
                 restarted, or newer payloads evicted it); nothing was re-run"
                    .into(),
                None,
            )),
            Source::Run => {
                let run_id = RunId::parse(w.id.clone()).map_err(|_| {
                    ErrorInfo::new(ErrorCode::INVALID_ARGUMENT, "invalid payload token")
                })?;
                let path = self.results_dir.join(format!("{run_id}.json"));
                let read = tokio::task::spawn_blocking(move || std::fs::read(path))
                    .await
                    .map_err(|e| ErrorInfo::new(ErrorCode::INTERNAL, e.to_string()))?;
                let next = Some((
                    vec!["mira".to_owned(), "runs".to_owned(), run_id.to_string()],
                    "Read the run record and summary; run the action again explicitly if you need new data.",
                ));
                let data = read.map_err(|_| {
                    gone(
                        format!(
                            "the retained result of run {run_id} was cleaned up; nothing was re-run"
                        ),
                        next.clone(),
                    )
                })?;
                let sha = Digest::of_bytes(&data);
                if !sha.as_str().trim_start_matches("sha256:").starts_with(&w.h) {
                    return Err(gone(
                        format!(
                            "the retained result of run {run_id} no longer matches this token; nothing was re-run"
                        ),
                        next,
                    ));
                }
                Ok(Arc::new(data))
            }
        }
    }

    pub async fn read(&self, p: PayloadReadParams) -> Result<ChunkData, ErrorInfo> {
        let data = self.load(&p.token).await?;
        let selected = select(&data, p.pointer.as_deref())?;
        let max = p
            .max_bytes
            .unwrap_or(DEFAULT_CHUNK_BYTES)
            .clamp(1, MAX_CHUNK_BYTES) as usize;
        let (text, next_offset, done) = chunk(&selected, p.offset, max)?;
        Ok(ChunkData {
            token: p.token,
            pointer: p.pointer,
            offset: p.offset,
            text,
            next_offset,
            done,
            sha256: Digest::of_bytes(&selected),
        })
    }
}

impl Actor {
    pub(super) fn payload_read(&mut self, p: PayloadReadParams, r: Responder) {
        let payloads = self.payloads.clone();
        let ctx = self.ctx();
        tokio::spawn(async move {
            r.send(match payloads.read(p).await {
                Ok(c) => reply_ok(ctx, c, ReplyMeta::default()),
                Err(e) => reply_fail(ctx, e),
            });
        });
    }
}
