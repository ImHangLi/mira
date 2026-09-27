//! Clipboard copy through `/usr/bin/pbcopy`: asynchronous, at most 1 s, at most
//! 1 MiB, never silently truncated.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

use crate::ipc::{Event, Tx};

const MAX_COPY_BYTES: usize = 1024 * 1024;
const PBCOPY: &str = "/usr/bin/pbcopy";

pub fn copy(text: String, lines: usize, tx: Tx) {
    if text.len() > MAX_COPY_BYTES {
        let _ = tx.send(Event::Copied(Err(format!(
            "selection is {} bytes; the copy limit is 1 MiB. Select fewer lines or use `mira logs`.",
            text.len()
        ))));
        return;
    }
    tokio::spawn(async move {
        let res = tokio::time::timeout(Duration::from_secs(1), pbcopy(&text)).await;
        let msg = match res {
            Ok(Ok(())) => {
                let first = text.lines().next().unwrap_or("");
                let preview: String = first.chars().take(48).collect();
                let more = if preview.len() < first.len() {
                    "..."
                } else {
                    ""
                };
                Ok(format!(
                    "copied {lines} line(s), {} bytes: \"{preview}{more}\"",
                    text.len()
                ))
            }
            Ok(Err(e)) => Err(format!("copy failed: {e}")),
            Err(_) => Err("copy failed: pbcopy did not finish within 1 s".into()),
        };
        let _ = tx.send(Event::Copied(msg));
    });
}

async fn pbcopy(text: &str) -> std::io::Result<()> {
    let mut child = tokio::process::Command::new(PBCOPY)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes()).await?;
        stdin.shutdown().await?;
    }
    let status = child.wait().await?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "pbcopy exited with {status}"
        )))
    }
}
