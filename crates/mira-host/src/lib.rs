//! The per-workspace host: one socket owner, one workspace actor, runners, and storage.
//!
//! Started as `mira __host --root PATH` by the first client that finds no running host.

mod actor;
mod env;
mod groups;
mod logs;
mod plugin_runner;
mod pty;
mod runner;
mod server;
mod storage;

use mira_protocol::ids::AbsolutePath;
use mira_protocol::paths::WorkspacePaths;

/// Appends one diagnostic line to the host log (stderr is redirected there by the client).
/// Never pass env values, inputs, or plugin config.
pub(crate) fn diag(msg: impl AsRef<str>) {
    eprintln!("{} {}", mira_protocol::Timestamp::now(), msg.as_ref());
}

/// Runs the host for `root` until it idles out or receives SIGTERM. Returns an exit code.
pub fn run(root: AbsolutePath) -> u8 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            diag(format!("cannot start runtime: {e}"));
            return 7;
        }
    };
    let paths = WorkspacePaths::new(root);
    runtime.block_on(server::serve(paths))
}
