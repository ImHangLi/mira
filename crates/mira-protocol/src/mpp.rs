//! MPP/1 plugin protocol: invocation, output frames, and bounded LF framing.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{ErrorCode, ErrorInfo, Issue, Issues};
use crate::ids::{AbsolutePath, ActionId, Api1, RunId, ViewId, WorkspaceId};
use crate::limits::*;
use crate::manifest::{parse_wire, present};
use crate::view::{LogLevel, ViewData, ViewOp};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InvocationContext {
    pub workspace_id: WorkspaceId,
    pub workspace_root: AbsolutePath,
    pub cwd: AbsolutePath,
    pub plugin_dir: AbsolutePath,
    pub state_dir: AbsolutePath,
    pub cache_dir: AbsolutePath,
    pub artifact_dir: AbsolutePath,
}

/// The single line written to a plugin's stdin before EOF. All fields are required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Invocation {
    pub api: Api1,
    pub run_id: RunId,
    pub action: ActionId,
    pub input: Map<String, Value>,
    pub config: Map<String, Value>,
    pub context: InvocationContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Unknown,
    Healthy,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactOwnership {
    Managed,
    External,
}

/// One plugin output frame as written on stdout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginFrame {
    Log {
        api: Api1,
        level: LogLevel,
        text: String,
        #[serde(default)]
        fields: Map<String, Value>,
    },
    Progress {
        api: Api1,
        message: String,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        #[schemars(with = "f64")]
        current: Option<f64>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        #[schemars(with = "f64")]
        total: Option<f64>,
    },
    Status {
        api: Api1,
        state: HealthState,
        message: String,
    },
    View {
        api: Api1,
        view_id: ViewId,
        op: ViewOp,
        data: ViewData,
    },
    /// A desktop notification for the user, such as "the timer is done".
    Notify {
        api: Api1,
        title: String,
        message: String,
    },
    Artifact {
        api: Api1,
        path: String,
        mime: String,
        label: String,
        ownership: ArtifactOwnership,
    },
    Result {
        api: Api1,
        ok: bool,
        summary: String,
        /// Required; may be `null`.
        data: Value,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        #[schemars(with = "ErrorInfo")]
        error: Option<ErrorInfo>,
    },
}

/// A plugin's self-reported result. The domain type has no "failed without error" state.
#[derive(Debug, Clone, PartialEq)]
pub enum PluginResult {
    Success {
        summary: String,
        data: Value,
    },
    Failure {
        summary: String,
        data: Value,
        error: ErrorInfo,
    },
}

/// A validated plugin event.
#[derive(Debug, Clone, PartialEq)]
pub enum PluginEvent {
    Log {
        level: LogLevel,
        text: String,
        fields: Map<String, Value>,
    },
    Progress {
        message: String,
        current: Option<(f64, f64)>,
    },
    Status {
        state: HealthState,
        message: String,
    },
    View {
        view_id: ViewId,
        op: ViewOp,
        data: ViewData,
    },
    Notify {
        title: String,
        message: String,
    },
    Artifact {
        path: String,
        mime: String,
        label: String,
        ownership: ArtifactOwnership,
    },
    Result(PluginResult),
}

impl PluginFrame {
    /// Wire frame → validated event (cross-field rules and byte limits).
    pub fn validate(self) -> Result<PluginEvent, Issues> {
        let mut issues = Issues::default();
        let ev = match self {
            Self::Log {
                level,
                text,
                fields,
                ..
            } => {
                if text.len() > MAX_LOG_TEXT_BYTES {
                    issues.push(Issue::new(
                        ErrorCode::INVALID_FRAME,
                        "/text",
                        "log text exceeds 8 KiB",
                    ));
                }
                PluginEvent::Log {
                    level,
                    text,
                    fields,
                }
            }
            Self::Progress {
                message,
                current,
                total,
                ..
            } => {
                if message.len() > MAX_MESSAGE_BYTES {
                    issues.push(Issue::new(
                        ErrorCode::INVALID_FRAME,
                        "/message",
                        "message exceeds 2 KiB",
                    ));
                }
                let pair = match (current, total) {
                    (None, None) => None,
                    (Some(c), Some(t))
                        if c.is_finite() && t.is_finite() && t > 0.0 && (0.0..=t).contains(&c) =>
                    {
                        Some((c, t))
                    }
                    _ => {
                        issues.push(Issue::new(ErrorCode::INVALID_FRAME, "/current", "current and total must both be present with 0 <= current <= total and total > 0"));
                        None
                    }
                };
                PluginEvent::Progress {
                    message,
                    current: pair,
                }
            }
            Self::Status { state, message, .. } => {
                if message.len() > MAX_MESSAGE_BYTES {
                    issues.push(Issue::new(
                        ErrorCode::INVALID_FRAME,
                        "/message",
                        "message exceeds 2 KiB",
                    ));
                }
                PluginEvent::Status { state, message }
            }
            Self::View {
                view_id, op, data, ..
            } => {
                if op == ViewOp::Append && !matches!(data, ViewData::Log { .. }) {
                    issues.push(Issue::new(
                        ErrorCode::INVALID_FRAME,
                        "/op",
                        "append is only allowed for log views",
                    ));
                }
                issues.extend(data.validate("/data"));
                PluginEvent::View { view_id, op, data }
            }
            Self::Notify { title, message, .. } => {
                issues.extend(check_notify(&title, &message, ErrorCode::INVALID_FRAME));
                PluginEvent::Notify { title, message }
            }
            Self::Artifact {
                path,
                mime,
                label,
                ownership,
                ..
            } => {
                if path.is_empty() || path.contains('\0') {
                    issues.push(Issue::new(
                        ErrorCode::INVALID_FRAME,
                        "/path",
                        "artifact path must be non-empty without NUL",
                    ));
                }
                PluginEvent::Artifact {
                    path,
                    mime,
                    label,
                    ownership,
                }
            }
            Self::Result {
                ok,
                summary,
                data,
                error,
                ..
            } => {
                if summary.len() > MAX_MESSAGE_BYTES {
                    issues.push(Issue::new(
                        ErrorCode::INVALID_FRAME,
                        "/summary",
                        "summary exceeds 2 KiB",
                    ));
                }
                match (ok, error) {
                    (true, None) => PluginEvent::Result(PluginResult::Success { summary, data }),
                    (false, Some(error)) => {
                        if let Err(m) = error.check_limits() {
                            issues.push(Issue::new(ErrorCode::INVALID_FRAME, "/error", m));
                        }
                        PluginEvent::Result(PluginResult::Failure {
                            summary,
                            data,
                            error,
                        })
                    }
                    (true, Some(_)) => {
                        issues.push(Issue::new(
                            ErrorCode::INVALID_FRAME,
                            "/error",
                            "ok=true must not carry an error",
                        ));
                        return Err(issues);
                    }
                    (false, None) => {
                        issues.push(Issue::new(
                            ErrorCode::INVALID_FRAME,
                            "/error",
                            "ok=false requires an error",
                        ));
                        return Err(issues);
                    }
                }
            }
        };
        issues.into_result(ev)
    }
}

/// The rules for a desktop notification, from an MPP/1 `notify` frame or `run.notify`.
/// Clients pass both texts to the terminal, so no control characters get through.
pub fn check_notify(title: &str, message: &str, code: ErrorCode) -> Issues {
    let mut issues = Issues::default();
    for (pointer, text, max) in [
        ("/title", title, MAX_NOTIFY_TITLE_BYTES),
        ("/message", message, MAX_MESSAGE_BYTES),
    ] {
        if text.trim().is_empty() || text.len() > max || text.chars().any(char::is_control) {
            issues.push(Issue::new(
                code.clone(),
                pointer,
                format!("must be non-empty text of at most {max} bytes without control characters"),
            ));
        }
    }
    issues
}

/// Parses one frame line (without its LF) into a validated event.
pub fn parse_frame(line: &[u8]) -> Result<PluginEvent, Issues> {
    let frame: PluginFrame = parse_wire(line, MAX_LPP_FRAME_BYTES).map_err(|mut i| {
        for issue in &mut i.0 {
            if issue.code == ErrorCode::SCHEMA_INVALID {
                issue.code = ErrorCode::INVALID_FRAME;
            }
        }
        i
    })?;
    frame.validate()
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LineError {
    #[error("frame exceeds {0} bytes")]
    TooLarge(usize),
    #[error("empty line")]
    Empty,
    #[error("frame starts with a byte order mark")]
    Bom,
    #[error("input ended inside a frame")]
    Truncated,
}

impl LineError {
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::TooLarge(_) => ErrorCode::FRAME_TOO_LARGE,
            Self::Truncated => ErrorCode::TRUNCATED_FRAME,
            Self::Empty | Self::Bom => ErrorCode::INVALID_FRAME,
        }
    }
}

/// Sans-IO bounded LF splitter. Accepts CRLF, emits lines without terminators, and
/// fails as soon as the buffered partial line exceeds the limit.
#[derive(Debug)]
pub struct LineDecoder {
    buf: Vec<u8>,
    max: usize,
    failed: bool,
}

impl LineDecoder {
    pub fn new(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            max,
            failed: false,
        }
    }

    /// True while a partial line is buffered.
    pub fn has_partial(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Feeds bytes. After the first error the decoder yields nothing more.
    pub fn push(&mut self, mut chunk: &[u8]) -> Vec<Result<Vec<u8>, LineError>> {
        let mut out = Vec::new();
        while !self.failed && !chunk.is_empty() {
            match chunk.iter().position(|b| *b == b'\n') {
                Some(pos) => {
                    self.buf.extend_from_slice(&chunk[..pos]);
                    chunk = &chunk[pos + 1..];
                    let mut line = std::mem::take(&mut self.buf);
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    let r = if line.len() > self.max {
                        Err(LineError::TooLarge(self.max))
                    } else if line.is_empty() {
                        Err(LineError::Empty)
                    } else if line.starts_with(&[0xEF, 0xBB, 0xBF]) {
                        Err(LineError::Bom)
                    } else {
                        Ok(line)
                    };
                    if r.is_err() {
                        self.failed = true;
                    }
                    out.push(r);
                }
                None => {
                    self.buf.extend_from_slice(chunk);
                    chunk = &[];
                    // +1 tolerates a trailing CR that belongs to a CRLF terminator.
                    if self.buf.len() > self.max + 1 {
                        self.failed = true;
                        self.buf.clear();
                        out.push(Err(LineError::TooLarge(self.max)));
                    }
                }
            }
        }
        out
    }

    /// Signals EOF. A non-empty partial line is a truncated frame.
    pub fn finish(&mut self) -> Result<(), LineError> {
        if !self.failed && !self.buf.is_empty() {
            self.buf.clear();
            self.failed = true;
            return Err(LineError::Truncated);
        }
        Ok(())
    }
}
