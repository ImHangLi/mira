//! Artifact registration and bounded reads. `managed` files must resolve inside the
//! run's artifact directory; `external` ones are references with size and existence only.
//! Nothing is opened at registration; `artifact.read` is the only explicit read.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::{AbsolutePath, Digest, RunId};
use mira_protocol::ipc::*;
use mira_protocol::mpp::ArtifactOwnership;
use mira_protocol::reply::ReplyMeta;
use serde_json::{Map, Value};

use super::{Actor, Responder, reply_fail, reply_ok};

/// Registrations kept per host lifetime; the oldest are forgotten first.
const MAX_ARTIFACTS: usize = 4096;
const DEFAULT_READ_BYTES: u32 = 16 * 1024;
const MAX_READ_BYTES: u32 = 256 * 1024;
const MAX_MIME_BYTES: usize = 128;

struct Entry {
    info: ArtifactInfo,
    /// Size and mtime at registration, to report `changed` later.
    seen: Option<(u64, Option<SystemTime>)>,
}

#[derive(Default)]
pub struct Artifacts {
    entries: VecDeque<Entry>,
}

fn invalid(msg: impl Into<String>) -> ErrorInfo {
    ErrorInfo::new(ErrorCode::INVALID_FRAME, msg)
}

fn stat(path: &Path) -> Option<(u64, Option<SystemTime>)> {
    let m = std::fs::metadata(path).ok()?;
    m.is_file().then(|| (m.len(), m.modified().ok()))
}

fn state_of(e: &Entry) -> (ArtifactState, Option<u64>) {
    match (stat(e.info.path.as_path()), e.seen) {
        (None, _) => (ArtifactState::NotFound, None),
        (Some((size, _)), None) => (ArtifactState::Changed, Some(size)),
        (Some(now), Some(then)) if now == then => (ArtifactState::Present, Some(now.0)),
        (Some((size, _)), Some(_)) => (ArtifactState::Changed, Some(size)),
    }
}

impl Artifacts {
    /// Validates and records one `artifact` frame.
    #[allow(clippy::too_many_arguments)]
    pub fn register(
        &mut self,
        run_id: &RunId,
        n: u32,
        cwd: &Path,
        artifact_dir: &Path,
        path: &str,
        mime: String,
        label: String,
        ownership: ArtifactOwnership,
    ) -> Result<ArtifactInfo, ErrorInfo> {
        if mime.is_empty() || mime.len() > MAX_MIME_BYTES {
            return Err(invalid("artifact mime must be 1-128 bytes"));
        }
        if label.len() > mira_protocol::limits::MAX_NAME_BYTES {
            return Err(invalid("artifact label exceeds 128 bytes"));
        }
        let given = PathBuf::from(path);
        let joined = if given.is_absolute() {
            given
        } else {
            cwd.join(given)
        };
        let resolved = match ownership {
            ArtifactOwnership::Managed => {
                let real = std::fs::canonicalize(&joined).map_err(|e| {
                    invalid(format!("managed artifact `{path}` cannot be resolved: {e}"))
                })?;
                let root = std::fs::canonicalize(artifact_dir).map_err(|e| {
                    invalid(format!("the run's artifact directory is unavailable: {e}"))
                })?;
                if !real.starts_with(&root) || real == root {
                    return Err(invalid(format!(
                        "managed artifact `{path}` resolves outside this run's artifact directory; \
                         write it under MIRA_ARTIFACT_DIR or register it as external"
                    )));
                }
                if !real.is_file() {
                    return Err(invalid(format!(
                        "managed artifact `{path}` is not a regular file"
                    )));
                }
                real
            }
            // External: a reference only. The path is kept as given (made absolute).
            ArtifactOwnership::External => joined,
        };
        let abs = AbsolutePath::from_path(&resolved)
            .map_err(|e| invalid(format!("artifact path is not usable: {e}")))?;
        let seen = stat(&resolved);
        let info = ArtifactInfo {
            id: format!("{run_id}.{n}"),
            run_id: run_id.clone(),
            path: abs,
            mime,
            label,
            ownership,
            size_bytes: seen.map(|s| s.0),
            state: if seen.is_some() {
                ArtifactState::Present
            } else {
                ArtifactState::NotFound
            },
        };
        self.entries.push_back(Entry {
            info: info.clone(),
            seen,
        });
        if self.entries.len() > MAX_ARTIFACTS {
            self.entries.pop_front();
        }
        Ok(info)
    }

    fn list(&self, run_id: Option<&RunId>) -> Vec<ArtifactInfo> {
        self.entries
            .iter()
            .filter(|e| run_id.is_none_or(|r| &e.info.run_id == r))
            .map(|e| {
                let (state, size) = state_of(e);
                ArtifactInfo {
                    state,
                    size_bytes: size.or(e.info.size_bytes),
                    ..e.info.clone()
                }
            })
            .collect()
    }
}

fn read_chunk(path: &Path, offset: u64, max: u32) -> Result<(String, u64, bool), ErrorInfo> {
    let mut f = std::fs::File::open(path).map_err(|e| {
        ErrorInfo::new(
            ErrorCode::NOT_FOUND,
            format!("cannot open the artifact: {e}"),
        )
    })?;
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    if offset > size {
        return Err(ErrorInfo::new(
            ErrorCode::INVALID_ARGUMENT,
            format!("offset {offset} is past the end ({size} bytes)"),
        ));
    }
    f.seek(SeekFrom::Start(offset))
        .map_err(|e| ErrorInfo::new(ErrorCode::STORAGE_UNAVAILABLE, e.to_string()))?;
    let mut buf = Vec::new();
    f.take(u64::from(max))
        .read_to_end(&mut buf)
        .map_err(|e| ErrorInfo::new(ErrorCode::STORAGE_UNAVAILABLE, e.to_string()))?;
    if buf.first().is_some_and(|b| b & 0xC0 == 0x80) {
        return Err(ErrorInfo::new(
            ErrorCode::INVALID_ARGUMENT,
            "offset is not at a UTF-8 character boundary; continue from next_offset",
        ));
    }
    let text = match std::str::from_utf8(&buf) {
        Ok(s) => s.to_owned(),
        // Only an incomplete character at the end of the chunk is cut; continue next time.
        Err(e) if e.error_len().is_none() && e.valid_up_to() > 0 => {
            String::from_utf8_lossy(&buf[..e.valid_up_to()]).into_owned()
        }
        Err(_) => {
            return Err(ErrorInfo::new(
                ErrorCode::INVALID_ARGUMENT,
                "the artifact is not UTF-8 text; binary content is not encoded into replies",
            ));
        }
    };
    let next = offset + text.len() as u64;
    Ok((text, next, next >= size))
}

impl Actor {
    pub(super) fn artifact_list(&mut self, p: ArtifactListParams, r: Responder) {
        let artifacts = self.artifacts.list(p.run_id.as_ref());
        r.send(self.ok(ArtifactListData { artifacts }, ReplyMeta::default()));
    }

    pub(super) fn artifact_read(&mut self, p: ArtifactReadParams, r: Responder) {
        let Some(entry) = self
            .artifacts
            .entries
            .iter()
            .find(|e| e.info.id == p.artifact_id)
        else {
            return r.send(self.fail(ErrorInfo::new(
                ErrorCode::NOT_FOUND,
                format!(
                    "no artifact `{}` is registered in this host session",
                    p.artifact_id
                ),
            )));
        };
        let (state, _) = state_of(entry);
        let mut details = Map::new();
        details.insert("artifact_id".into(), Value::String(p.artifact_id.clone()));
        details.insert(
            "state".into(),
            serde_json::to_value(state).unwrap_or(Value::Null),
        );
        match state {
            ArtifactState::Present => {}
            ArtifactState::NotFound | ArtifactState::Deleted => {
                return r.send(
                    self.fail(
                        ErrorInfo::new(ErrorCode::NOT_FOUND, "the artifact file no longer exists")
                            .with_details(details),
                    ),
                );
            }
            ArtifactState::Changed => {
                return r.send(self.fail(
                    ErrorInfo::new(
                        ErrorCode::PAYLOAD_GONE,
                        "the artifact file changed after it was registered; the registered content is gone",
                    )
                    .with_details(details)
                    .with_next_action(
                        &["mira", "artifacts", entry.info.run_id.as_str()],
                        "See the current metadata of this run's artifacts.",
                    ),
                ));
            }
        }
        let path = entry.info.path.as_path().to_path_buf();
        let max = p
            .max_bytes
            .unwrap_or(DEFAULT_READ_BYTES)
            .clamp(1, MAX_READ_BYTES);
        let ctx = self.ctx();
        tokio::spawn(async move {
            let id = p.artifact_id.clone();
            let offset = p.offset;
            let res = tokio::task::spawn_blocking(move || read_chunk(&path, offset, max)).await;
            let reply = match res {
                Ok(Ok((text, next, done))) => reply_ok(
                    ctx,
                    ChunkData {
                        token: id,
                        pointer: None,
                        offset,
                        sha256: Digest::of_bytes(text.as_bytes()),
                        text,
                        next_offset: (!done).then_some(next),
                        done,
                    },
                    ReplyMeta::default(),
                ),
                Ok(Err(e)) => reply_fail(ctx, e),
                Err(e) => reply_fail(ctx, ErrorInfo::new(ErrorCode::INTERNAL, e.to_string())),
            };
            r.send(reply);
        });
    }
}
