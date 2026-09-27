//! MPP/1 stdout handling: bounded LF framing, per-frame validation, and
//! frame order rules. Process spawn, stop, and cleanup stay in [`crate::runner`]; this module
//! only turns plugin stdout bytes into validated events or one protocol error.

use std::time::Duration;

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::ViewId;
use mira_protocol::limits::MAX_LPP_FRAME_BYTES;
use mira_protocol::manifest::ActionMode;
use mira_protocol::mpp::{LineDecoder, PluginEvent, parse_frame};
use mira_protocol::view::LogLevel;
use serde_json::{Map, Value};
use tokio::time::Instant;

/// A buffered partial frame with no new bytes for this long is a truncated frame.
pub const PARTIAL_FRAME_IDLE: Duration = Duration::from_secs(15);

/// What the runner needs to speak MPP/1 with one child.
pub struct Protocol {
    /// The invocation JSON plus LF, written to stdin before EOF.
    pub invocation_line: Vec<u8>,
    pub mode: ActionMode,
}

/// One result of feeding stdout bytes.
pub enum FrameOut {
    /// A `log` frame; the runner writes it to the run log as stream `plugin`.
    Log {
        level: LogLevel,
        text: String,
        fields: Map<String, Value>,
    },
    /// Any other validated frame, in receive order.
    Event(PluginEvent),
    /// The first protocol error. Nothing is parsed after it.
    Error {
        error: ErrorInfo,
        /// The `view_id` of a rejected view frame, when the line was readable JSON.
        view_hint: Option<ViewId>,
    },
}

pub struct FrameReader {
    decoder: LineDecoder,
    mode: ActionMode,
    result_seen: bool,
    failed: bool,
    last_byte: Instant,
}

fn view_hint(line: &[u8]) -> Option<ViewId> {
    let v: Value = serde_json::from_slice(line).ok()?;
    if v.get("type")?.as_str()? != "view" {
        return None;
    }
    ViewId::parse(v.get("view_id")?.as_str()?.to_owned()).ok()
}

impl FrameReader {
    pub fn new(mode: ActionMode) -> Self {
        Self {
            decoder: LineDecoder::new(MAX_LPP_FRAME_BYTES),
            mode,
            result_seen: false,
            failed: false,
            last_byte: Instant::now(),
        }
    }

    fn error(&mut self, code: ErrorCode, message: String, view_hint: Option<ViewId>) -> FrameOut {
        self.failed = true;
        FrameOut::Error {
            error: ErrorInfo::new(code, message),
            view_hint,
        }
    }

    /// Feeds stdout bytes of any size and split. After an error the rest is discarded.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<FrameOut> {
        let mut out = Vec::new();
        if self.failed {
            return out;
        }
        self.last_byte = Instant::now();
        for line in self.decoder.push(bytes) {
            if self.failed {
                break;
            }
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    out.push(self.error(e.code(), format!("invalid MPP/1 output: {e}"), None));
                    break;
                }
            };
            match parse_frame(&line) {
                Ok(ev) => out.push(self.accept(ev)),
                Err(issues) => {
                    let hint = view_hint(&line);
                    self.failed = true;
                    out.push(FrameOut::Error {
                        error: issues.to_error_info(),
                        view_hint: hint,
                    });
                }
            }
        }
        out
    }

    /// Frame order rules: a task's result is its last frame; processes never send one.
    fn accept(&mut self, ev: PluginEvent) -> FrameOut {
        if self.result_seen {
            let what = if matches!(ev, PluginEvent::Result(_)) {
                "a second result frame"
            } else {
                "a frame after the result frame"
            };
            return self.error(
                ErrorCode::INVALID_FRAME,
                format!("the plugin sent {what}; the result must be the last frame"),
                None,
            );
        }
        match ev {
            PluginEvent::Result(_) if self.mode == ActionMode::Process => self.error(
                ErrorCode::INVALID_FRAME,
                "process actions must not send a result frame".into(),
                None,
            ),
            PluginEvent::Log {
                level,
                text,
                fields,
            } => FrameOut::Log {
                level,
                text,
                fields,
            },
            ev => {
                if matches!(ev, PluginEvent::Result(_)) {
                    self.result_seen = true;
                }
                FrameOut::Event(ev)
            }
        }
    }

    /// stdout reached EOF: a non-empty partial line is a truncated frame.
    pub fn finish(&mut self) -> Option<FrameOut> {
        if self.failed {
            return None;
        }
        match self.decoder.finish() {
            Ok(()) => None,
            Err(e) => Some(self.error(
                e.code(),
                "stdout ended inside a frame (no final LF)".into(),
                None,
            )),
        }
    }

    /// True while a partial frame waits for more bytes.
    pub fn partial_pending(&self) -> bool {
        !self.failed && self.decoder.has_partial()
    }

    pub fn partial_deadline(&self) -> Instant {
        self.last_byte + PARTIAL_FRAME_IDLE
    }

    /// The partial frame got no new bytes for [`PARTIAL_FRAME_IDLE`].
    pub fn partial_timeout(&mut self) -> Option<FrameOut> {
        if !self.partial_pending() {
            return None;
        }
        Some(self.error(
            ErrorCode::TRUNCATED_FRAME,
            format!(
                "a partial frame received no new bytes for {} s",
                PARTIAL_FRAME_IDLE.as_secs()
            ),
            None,
        ))
    }
}
