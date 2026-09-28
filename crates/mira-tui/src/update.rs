//! Best-effort daily release check. File and network I/O stay off the UI task.

use mira_protocol::update::{Cache, DAY_MS, latest, now_ms};

use crate::ipc::{Event, Tx};

pub fn start(tx: Tx) {
    if std::env::var_os("CI").is_some()
        || std::env::var_os("MIRA_NO_UPDATE_CHECK").is_some_and(|v| !v.is_empty())
    {
        return;
    }
    tokio::task::spawn_blocking(move || {
        if let Some(cache) = Cache::read() {
            let _ = tx.send(Event::LatestVersion(cache.latest));
            if now_ms().saturating_sub(cache.checked_at_ms) < DAY_MS {
                return;
            }
        }
        if let Ok(release) = latest() {
            let cache = Cache {
                checked_at_ms: now_ms(),
                latest: release.tag_name,
            };
            let _ = cache.write();
            let _ = tx.send(Event::LatestVersion(cache.latest));
        }
    });
}
