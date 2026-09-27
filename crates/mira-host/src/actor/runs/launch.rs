//! Spawning a reserved run: private input files, the child environment, and the runner.

use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::*;
use mira_protocol::ipc::*;
use mira_protocol::manifest::TerminalMode;
use mira_protocol::reply::{ReplyContext, ReplyMeta};
use mira_protocol::run::*;
use serde_json::Value;

use crate::actor::{Actor, reply_ok};
use crate::env::{self, HostVars};
use crate::plugin_runner::Protocol;
use crate::runner::{self, CommandSpec};

use super::Phase;

fn write_private_json(path: &Path, v: &Value) -> Result<(), String> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| e.to_string())?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| e.to_string())?;
    f.write_all(v.to_string().as_bytes())
        .map_err(|e| e.to_string())
}

impl Actor {
    pub(super) fn launch(&mut self, run_id: RunId) {
        let Some(run) = self.runs.get_mut(&run_id) else {
            return;
        };
        let Phase::Reserving { waiters, launch } = std::mem::replace(&mut run.phase, Phase::Live)
        else {
            return;
        };
        let state = run.record.lifecycle;
        let reply = reply_ok(
            ReplyContext {
                workspace: Some(mira_protocol::reply::WorkspaceRef {
                    id: self.paths.id.clone(),
                    root: self.paths.root.clone(),
                }),
                host_epoch: Some(self.epoch.clone()),
                catalog_revision: Some(self.catalog_revision),
                state_revision: Some(self.state_revision),
            },
            InvokeAccepted {
                run_id: run_id.clone(),
                state,
                reused: false,
            },
            ReplyMeta::default(),
        );
        for w in waiters {
            w.send(reply.clone());
        }
        if run.requested_stop.is_some() {
            // Stopped before it could start: the committed reservation still gets a final record.
            self.finalize(&run_id, None, None, CleanupState::NotNeeded);
            return;
        }
        let root = self.paths.root.as_path().to_path_buf();
        let (state_dir, cache_dir, plugin_dir) = match &launch.plugin {
            Some((id, dir)) => (
                self.paths.plugin_state(id),
                self.paths.plugin_cache(id),
                Some(dir.to_string()),
            ),
            None => (
                self.paths.state_dir.join("exec"),
                self.paths.cache_dir.join("exec"),
                None,
            ),
        };
        let artifact_dir = self.paths.artifacts(&run_id);
        let inputs = self.paths.temp_inputs();
        let input_file = inputs.join(format!("{run_id}.input.json"));
        let config_file = inputs.join(format!("{run_id}.config.json"));
        let action_cwd: PathBuf = if launch.cwd.starts_with('/') {
            PathBuf::from(&launch.cwd)
        } else {
            root.join(&launch.cwd)
        }
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect();
        let prepared = (|| -> Result<(env::ChildEnv, Option<Protocol>), ErrorInfo> {
            for d in [&state_dir, &cache_dir, &artifact_dir] {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(d)
                    .map_err(|e| {
                        ErrorInfo::new(
                            ErrorCode::STORAGE_UNAVAILABLE,
                            format!("cannot create {}: {e}", d.display()),
                        )
                    })?;
            }
            write_private_json(&input_file, &Value::Object(launch.input.clone())).map_err(|e| {
                ErrorInfo::new(
                    ErrorCode::STORAGE_UNAVAILABLE,
                    format!("cannot write input file: {e}"),
                )
            })?;
            write_private_json(&config_file, &Value::Object(launch.config.clone())).map_err(
                |e| {
                    ErrorInfo::new(
                        ErrorCode::STORAGE_UNAVAILABLE,
                        format!("cannot write config file: {e}"),
                    )
                },
            )?;
            let host = HostVars {
                workspace_root: self.paths.root.to_string(),
                plugin_dir,
                state_dir: state_dir.to_string_lossy().into_owned(),
                cache_dir: cache_dir.to_string_lossy().into_owned(),
                artifact_dir: artifact_dir.to_string_lossy().into_owned(),
                run_id: run_id.to_string(),
                input_file: input_file.to_string_lossy().into_owned(),
                config_file: config_file.to_string_lossy().into_owned(),
            };
            let env = env::compose(
                &launch.client_env,
                &root,
                &launch.env_files,
                &launch.action_env,
                &host,
            )?;
            let protocol = match (&launch.protocol, &launch.plugin) {
                (Some((action, mode)), Some((_, dir))) => {
                    let abs = |p: &Path| {
                        AbsolutePath::from_path(p).map_err(|e| {
                            ErrorInfo::new(
                                ErrorCode::EXECUTION_FAILED,
                                format!("path {} is not usable: {e}", p.display()),
                            )
                        })
                    };
                    let invocation = mira_protocol::mpp::Invocation {
                        api: Api1,
                        run_id: run_id.clone(),
                        action: action.clone(),
                        input: launch.input.clone(),
                        config: launch.config.clone(),
                        context: mira_protocol::mpp::InvocationContext {
                            workspace_id: self.paths.id.clone(),
                            workspace_root: self.paths.root.clone(),
                            cwd: abs(&action_cwd)?,
                            plugin_dir: dir.clone(),
                            state_dir: abs(&state_dir)?,
                            cache_dir: abs(&cache_dir)?,
                            artifact_dir: abs(&artifact_dir)?,
                        },
                    };
                    let mut line = serde_json::to_vec(&invocation)
                        .map_err(|e| ErrorInfo::new(ErrorCode::INTERNAL, e.to_string()))?;
                    line.push(b'\n');
                    Some(Protocol {
                        invocation_line: line,
                        mode: *mode,
                    })
                }
                _ => None,
            };
            Ok((env, protocol))
        })();
        let (env, protocol) = match prepared {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_file(&input_file);
                let _ = std::fs::remove_file(&config_file);
                if let Ok(mut log) = run.log.lock() {
                    log.push_line(
                        LogStream::Host,
                        mira_protocol::view::LogLevel::Error,
                        &e.message,
                        false,
                    );
                    log.flush();
                }
                self.finalize(&run_id, None, Some(e), CleanupState::NotNeeded);
                return;
            }
        };
        // Plugins run in their plugin dir; `context.cwd` tells them the action cwd.
        let cwd = match (&protocol, &launch.plugin) {
            (Some(_), Some((_, dir))) => dir.as_path().to_path_buf(),
            _ => action_cwd.clone(),
        };
        if let Some(mpp) = run.mpp.as_mut() {
            mpp.cwd = action_cwd.clone();
            mpp.artifact_dir = artifact_dir.clone();
        }
        let spec = CommandSpec {
            run_id: run_id.clone(),
            argv: launch.argv,
            cwd,
            cleanup_cwd: action_cwd,
            env,
            timeout: launch.timeout,
            stop_signal: launch.stop_signal,
            grace: launch.grace,
            cleanup: launch.cleanup,
            temp_files: vec![input_file, config_file],
            protocol,
        };
        let (stop_tx, stop_rx) = tokio::sync::mpsc::channel(4);
        run.stop_tx = Some(stop_tx);
        match launch.terminal {
            TerminalMode::Pipe => {
                tokio::spawn(runner::supervise(
                    spec,
                    run.log.clone(),
                    self.runner_tx.clone(),
                    stop_rx,
                ));
            }
            TerminalMode::Pty => {
                let log = run.log.clone();
                if let Some(h) = crate::pty::start(spec, log, self.runner_tx.clone(), stop_rx) {
                    self.pty_started(run_id, h);
                }
            }
        }
    }
}
