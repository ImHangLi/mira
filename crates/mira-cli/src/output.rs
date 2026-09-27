//! Output rules (§10.2): with `--json` (default when stdout is not a TTY) stdout holds exactly
//! one JSON object plus LF. Text output is a rendering of the same typed data.

use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use mira_protocol::reply::{PublicReply, ReplyContext};
use mira_protocol::{ErrorCode, ErrorInfo};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Json,
    Text,
}

impl Mode {
    pub fn resolve(json: bool, text: bool) -> Self {
        match (json, text) {
            (true, _) => Self::Json,
            (_, true) => Self::Text,
            _ if std::io::stdout().is_terminal() => Self::Text,
            _ => Self::Json,
        }
    }
}

/// Prints a reply and returns its fixed exit code.
pub fn emit<T: Serialize>(
    mode: Mode,
    reply: &PublicReply<T>,
    text: impl FnOnce(&T) -> String,
) -> ExitCode {
    let hinted = reply
        .error()
        .and_then(item_hint)
        .map(|e| PublicReply::<T>::failure(reply.context(), e));
    let reply = hinted.as_ref().unwrap_or(reply);
    let mut out = std::io::stdout().lock();
    let written = match mode {
        Mode::Json => match serde_json::to_string(reply) {
            Ok(line) => writeln!(out, "{line}"),
            Err(e) => writeln!(
                out,
                "{{\"api\":1,\"ok\":false,\"error\":{{\"code\":\"INTERNAL\",\"message\":{:?},\"retryable\":false}}}}",
                e.to_string()
            ),
        },
        Mode::Text => match (reply.data(), reply.error()) {
            (_, Some(e)) => {
                let _ = writeln!(std::io::stderr(), "{}", render_error(e));
                Ok(())
            }
            (Some(d), None) => writeln!(out, "{}", text(d)),
            (None, None) => Ok(()),
        },
    };
    if written.is_err() {
        return ExitCode::from(7);
    }
    ExitCode::from(reply.exit_code())
}

pub fn fail(mode: Mode, ctx: ReplyContext, error: ErrorInfo) -> ExitCode {
    emit::<()>(mode, &PublicReply::failure(ctx, error), |_| String::new())
}

/// A string field of `details`.
fn detail<'a>(e: &'a ErrorInfo, key: &str) -> Option<&'a str> {
    e.details.as_ref()?.get(key)?.as_str()
}

/// A NOT_FOUND reply for a catalog item gets a search hint when it has none.
fn item_hint(e: &ErrorInfo) -> Option<ErrorInfo> {
    if e.code != ErrorCode::NOT_FOUND || e.next_action.is_some() {
        return None;
    }
    let item = detail(e, "item_ref")?;
    let word = item.rsplit('.').next().filter(|w| !w.is_empty())?;
    Some(e.clone().with_next_action(
        &["mira", "catalog", "--search", word],
        "Find the tool by a word from its name.",
    ))
}

/// Plain wording for a few host messages; JSON keeps the original text.
fn text_message(e: &ErrorInfo) -> String {
    if e.code == ErrorCode::SESSION_REQUIRED {
        return "Nothing is running in the background. Open `mira` or run `mira up --background`."
            .into();
    }
    if e.code == ErrorCode::NOT_FOUND
        && let Some(id) = detail(e, "run_id")
    {
        return format!("No run `{id}`. See `mira runs`.");
    }
    e.message.clone()
}

pub fn render_error(e: &ErrorInfo) -> String {
    let message = text_message(e);
    let mut s = format!("error[{}]: {message}", e.code);
    if let Some(details) = &e.details
        && let Some(issues) = details.get("issues").and_then(|v| v.as_array())
    {
        for i in issues.iter().skip(1) {
            let file = i.get("file").and_then(|v| v.as_str()).unwrap_or("");
            let ptr = i.get("pointer").and_then(|v| v.as_str()).unwrap_or("");
            let msg = i.get("message").and_then(|v| v.as_str()).unwrap_or("");
            s.push_str(&format!("\n  {file} `{ptr}`: {msg}"));
        }
    }
    if let Some(n) = &e.next_action {
        let command = n.argv.join(" ");
        // Skip the hint when the message already names the command (its first words).
        let head = n.argv.iter().take(3).cloned().collect::<Vec<_>>().join(" ");
        if !message.contains(&format!("`{head}")) {
            s.push_str(&format!("\n  next: {command} ({})", n.reason));
        }
    }
    s
}

pub fn invalid_argument(message: impl Into<String>) -> ErrorInfo {
    ErrorInfo::new(ErrorCode::INVALID_ARGUMENT, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_items_get_a_catalog_search_hint() {
        let e = ErrorInfo::item_not_found("action", "dev.webb");
        let hinted = item_hint(&e).expect("hint");
        let argv = hinted.next_action.expect("next").argv;
        assert_eq!(argv, ["mira", "catalog", "--search", "webb"]);
        assert!(item_hint(&ErrorInfo::run_not_found("r_1")).is_none());
    }
}
