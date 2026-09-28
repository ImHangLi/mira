//! Release checks and per-user update state, shared by the CLI and TUI.

use std::io;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::VERSION;
use crate::paths::user_bases;

pub const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// Release versions have exactly three numeric components.
pub fn version(value: &str) -> Option<[u64; 3]> {
    let mut parts = value.split('.');
    let mut out = [0; 3];
    for part in &mut out {
        let text = parts.next()?;
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *part = text.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

pub fn newer(latest: &str, running: &str) -> bool {
    match (version(latest), version(running)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u64::MAX as u128) as u64)
}

#[derive(Deserialize, Serialize)]
pub struct Cache {
    pub checked_at_ms: u64,
    pub latest: String,
}

impl Cache {
    pub fn read() -> Option<Self> {
        let bytes = std::fs::read(user_bases().2.join("update-check.json")).ok()?;
        let cache: Self = serde_json::from_slice(&bytes).ok()?;
        version(&cache.latest)?;
        Some(cache)
    }

    pub fn write(&self) -> io::Result<()> {
        write_json(&user_bases().2.join("update-check.json"), self)
    }
}

/// Replace JSON state only after the complete contents have been written.
pub fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".mira-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        serde_json::to_writer(file, value)?;
        std::fs::rename(&temp, path)
    })();
    let _ = std::fs::remove_file(temp);
    result
}

#[derive(Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub body: Option<String>,
}

/// All update requests use the same timeout and identify the running version.
pub fn curl(timeout: u8) -> Command {
    let mut command = Command::new("/usr/bin/curl");
    command.args([
        "-fsSL",
        "--max-time",
        &timeout.to_string(),
        "--user-agent",
        &format!("mira/{VERSION}"),
    ]);
    command
}

pub fn latest() -> Result<Release, String> {
    let output = curl(5)
        .arg("https://api.github.com/repos/ImHangLi/mira/releases/latest")
        .output()
        .map_err(|e| format!("network: release check failed: {e}"))?;
    if !output.status.success() {
        return Err(format!("network: release check failed ({})", output.status));
    }
    let mut release: Release = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("network: invalid release reply: {e}"))?;
    release.tag_name = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name)
        .to_owned();
    if version(&release.tag_name).is_none() {
        return Err("network: release tag must be a numeric major.minor.patch version".into());
    }
    Ok(release)
}
