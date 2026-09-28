//! Which agent runs this `mira` command, so the TUI can group one-off runs per agent.
//!
//! The name comes from `MIRA_AGENT`, or from the nearest ancestor process that is a known
//! agent CLI (Claude Code, Codex, …). When no agent is found, a person runs it: `You`.
//! The ID tells two instances of the same agent apart: a
//! session ID the agent exports, else the agent's process ID. Only a short hash of it is sent,
//! never an environment value.

use std::collections::HashMap;
use std::process::Command;

use mira_protocol::hash::canonical_digest;
use mira_protocol::run::Requester;

/// Process names of agent CLIs, and the name people know them by.
const AGENTS: &[(&str, &str)] = &[
    ("claude", "Claude Code"),
    ("codex", "Codex"),
    ("cursor-agent", "Cursor"),
    ("gemini", "Gemini CLI"),
    ("opencode", "opencode"),
    ("aider", "Aider"),
    ("amp", "Amp"),
    ("droid", "Droid"),
    ("goose", "Goose"),
    ("crush", "Crush"),
    ("qwen", "Qwen Code"),
    ("copilot", "Copilot CLI"),
    ("grok", "Grok CLI"),
    ("kimi", "Kimi CLI"),
];

/// The requester of a run that a person starts; all of them share one section.
const PERSON: &str = "You";
const PERSON_ID: &str = "you";

/// Session IDs some agents export to the commands they run.
const SESSION_VARS: &[&str] = &[
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_SESSION_ID",
    "CODEX_THREAD_ID",
];

fn short_id(name: &str, key: &str) -> Option<String> {
    let d = canonical_digest(&(name, key)).ok()?.to_string();
    let hex: String = d
        .chars()
        .rev()
        .take(12)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    Some(hex.to_ascii_lowercase())
}

/// (parent PID, process name) for every process, from one `ps` call.
fn processes() -> HashMap<u32, (u32, String)> {
    let Ok(out) = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,comm="])
        .output()
    else {
        return HashMap::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let comm = it.collect::<Vec<_>>().join(" ");
            let base = comm
                .rsplit('/')
                .next()
                .unwrap_or(&comm)
                .to_ascii_lowercase();
            Some((pid, (ppid, base)))
        })
        .collect()
}

/// The nearest agent among this process's ancestors: (display name, its PID). `None` when a
/// person runs `mira` from a shell, or from the Mira TUI's command bar.
fn ancestor_agent() -> Option<(&'static str, u32)> {
    let procs = processes();
    let mut pid = std::os::unix::process::parent_id();
    for _ in 0..32 {
        let (ppid, base) = procs.get(&pid)?;
        if base == "mira" {
            return None;
        }
        if let Some((_, name)) = AGENTS
            .iter()
            .find(|(exe, _)| base == exe || base.starts_with(&format!("{exe}-")))
        {
            return Some((name, pid));
        }
        if *ppid <= 1 {
            return None;
        }
        pid = *ppid;
    }
    None
}

/// Always `Some` for a valid requester: an agent, or [`PERSON`] when no agent is found.
/// `task` is what the agent's thread works on (`--task`, else `MIRA_TASK`); it names the
/// thread's section in the TUI.
pub fn detect(task: Option<String>) -> Option<Requester> {
    let session = SESSION_VARS
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()));
    let (name, key) = match std::env::var("MIRA_AGENT")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        Some(name) => {
            let name: String = name
                .trim()
                .chars()
                .filter(|c| !c.is_control())
                .take(48)
                .collect();
            let key = session.unwrap_or_else(|| std::os::unix::process::parent_id().to_string());
            (name, key)
        }
        None => match ancestor_agent() {
            Some((name, pid)) => (name.to_owned(), session.unwrap_or_else(|| pid.to_string())),
            // No agent: a person runs it, from a shell or the TUI's command bar.
            None => (PERSON.to_owned(), String::new()),
        },
    };
    let id = if name == PERSON && key.is_empty() {
        PERSON_ID.to_owned()
    } else {
        short_id(&name, &key)?
    };
    let task = task
        .or_else(|| std::env::var("MIRA_TASK").ok())
        .map(|t| {
            t.trim()
                .chars()
                .filter(|c| !c.is_control())
                .take(60)
                .collect::<String>()
        })
        .filter(|t| !t.is_empty());
    let r = Requester { name, id, task };
    r.check().ok().map(|()| r)
}
