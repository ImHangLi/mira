//! The `:` command bar: runs one public `mira` command for infrastructure work
//! (status, doctor, paths, validate, schedule, ...) through the same CLI parser agents use.
//! It is not a shell: no `;`, `&`, `|`, redirects, `$`, or backticks; quotes only group words.

use std::process::Stdio;
use std::time::Duration;

use crate::ipc::{Event, Tx};

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT: usize = 1024 * 1024;

/// Public commands, for the completion hint. The CLI parser stays the authority.
pub const COMMANDS: &[&str] = &[
    "status",
    "catalog",
    "describe",
    "paths",
    "doctor",
    "validate",
    "schema",
    "run",
    "start",
    "stop",
    "restart",
    "runs",
    "logs",
    "view",
    "view-action",
    "publish",
    "artifacts",
    "schedule",
    "up",
    "down",
    "exec",
    "reload",
    "plugin",
    "save",
];

pub struct Output {
    pub title: String,
    pub lines: Vec<String>,
    pub text: String,
    pub top: usize,
    pub failed: bool,
    /// The notice to show instead of this output when the command succeeds.
    pub done: Option<String>,
}

/// Splits a command tail into words. Rejects shell syntax outside quotes.
pub fn split(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                has = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    has = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if has {
                    words.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            (None, ';' | '&' | '|' | '<' | '>' | '`' | '$' | '(' | ')') => {
                return Err(format!(
                    "`{c}` is shell syntax; the command bar runs one mira command, not a shell (quote it to pass it as text)"
                ));
            }
            (None, c) => {
                cur.push(c);
                has = true;
            }
        }
    }
    if quote.is_some() {
        return Err("a quote is not closed".into());
    }
    if has {
        words.push(cur);
    }
    if words.first().is_some_and(|w| w == "mira") {
        words.remove(0);
    }
    match words.first().map(String::as_str) {
        None => Err("type a mira command, for example `status` or `doctor`".into()),
        Some(w) if w.starts_with("__") => Err(format!("`{w}` is internal")),
        Some(w) if w.starts_with('-') => Err("start with a command name".into()),
        _ if words.iter().any(|w| w == "--follow") => {
            Err("`--follow` streams without end; open the item's log panel instead".into())
        }
        _ => Ok(words),
    }
}

pub fn hint(prefix: &str) -> String {
    let first = prefix.split_whitespace().next().unwrap_or("");
    let matches: Vec<&str> = COMMANDS
        .iter()
        .copied()
        .filter(|c| c.starts_with(first))
        .collect();
    if prefix.contains(' ') || matches.is_empty() {
        "Enter runs it (mira … --json) · Esc cancels".into()
    } else if first.is_empty() {
        "for example: status · doctor · reload · plugin add · exec --label NAME -- CMD".into()
    } else {
        matches.join(" ")
    }
}

/// Runs `mira --project ROOT --json WORDS...` and reports its reply. On success, `done`
/// replaces the output with a notice.
pub fn run(root: String, words: Vec<String>, done: Option<String>, tx: Tx) {
    tokio::spawn(async move {
        let title = format!("mira {}", words.join(" "));
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                let _ = tx.send(Event::CommandDone(Output::failed(title, e.to_string())));
                return;
            }
        };
        let child = tokio::process::Command::new(exe)
            .arg("--project")
            .arg(&root)
            .arg("--json")
            .args(&words)
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let out = match child {
            Ok(c) => tokio::time::timeout(TIMEOUT, c.wait_with_output()).await,
            Err(e) => {
                let _ = tx.send(Event::CommandDone(Output::failed(title, e.to_string())));
                return;
            }
        };
        let result = match out {
            Err(_) => Output::failed(
                title,
                "no answer within 30 s; the command was stopped".into(),
            ),
            Ok(Err(e)) => Output::failed(title, e.to_string()),
            Ok(Ok(o)) => {
                let code = o.status.code().unwrap_or(-1);
                let mut stdout = String::from_utf8_lossy(&o.stdout).into_owned();
                stdout.truncate(MAX_OUTPUT);
                let pretty = serde_json::from_str::<serde_json::Value>(stdout.trim())
                    .ok()
                    .and_then(|v| serde_json::to_string_pretty(&v).ok())
                    .unwrap_or(stdout);
                let stderr = String::from_utf8_lossy(&o.stderr);
                let text = if stderr.trim().is_empty() {
                    pretty
                } else {
                    format!("{pretty}\n{}", stderr.trim_end())
                };
                Output {
                    title: format!("{title}  (exit {code})"),
                    lines: text.lines().map(str::to_owned).collect(),
                    text,
                    top: 0,
                    failed: code != 0,
                    done,
                }
            }
        };
        let _ = tx.send(Event::CommandDone(result));
    });
}

/// One default plugin that `mira plugin add NAME` can copy into the project.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct DefaultPlugin {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Reads the default plugins through the public command, so the list has one source.
pub fn defaults(root: String, tx: Tx) {
    tokio::spawn(async move {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let out = tokio::process::Command::new(exe)
            .args(["--project", &root, "--json", "plugin", "add"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output();
        let Ok(Ok(o)) = tokio::time::timeout(TIMEOUT, out).await else {
            return;
        };
        #[derive(serde::Deserialize)]
        struct Reply {
            data: Option<Vec<DefaultPlugin>>,
        }
        if let Ok(Reply { data: Some(list) }) = serde_json::from_slice(&o.stdout) {
            let _ = tx.send(Event::Defaults(list));
        }
    });
}

impl Output {
    pub fn failed(title: String, message: String) -> Self {
        Self {
            title,
            lines: vec![message.clone()],
            text: message,
            top: 0,
            failed: true,
            done: None,
        }
    }
}
