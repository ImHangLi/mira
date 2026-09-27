//! Socket ownership and connection handling (§9).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::time::Duration;

use mira_protocol::error::ErrorCode;
use mira_protocol::ids::ClientId;
use mira_protocol::ipc::{
    ConnectionKind, HelloParams, JsonRpc2, Method, RpcError, RpcRequest, RpcResponse,
};
use mira_protocol::limits::MAX_IPC_FRAME_BYTES;
use mira_protocol::mpp::LineDecoder;
use mira_protocol::paths::{WorkspacePaths, current_uid};
use mira_protocol::{ErrorInfo, schemas, strict_json};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use crate::actor::{Actor, Msg, Outbound};
use crate::diag;

const HELLO_DEADLINE: Duration = Duration::from_secs(2);
const PARTIAL_FRAME_DEADLINE: Duration = Duration::from_secs(15);
const EVENT_QUEUE: usize = 256;

/// Owns the runtime directory entry for this workspace while the host lives.
struct Ownership {
    _lock: File,
    paths: WorkspacePaths,
}

impl Drop for Ownership {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.paths.socket());
        let _ = std::fs::remove_file(self.paths.owner());
    }
}

fn prepare_runtime_dir(paths: &WorkspacePaths) -> Result<(), String> {
    let dir = &paths.runtime_dir;
    match std::fs::symlink_metadata(dir) {
        Ok(m) => {
            if m.file_type().is_symlink() || !m.is_dir() {
                return Err(format!(
                    "{} must be a real directory, not a symlink",
                    dir.display()
                ));
            }
            if m.uid() != current_uid() {
                return Err(format!("{} is owned by another user", dir.display()));
            }
            if m.mode() & 0o077 != 0 {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                    .map_err(|e| e.to_string())?;
            }
        }
        Err(_) => {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Takes the exclusive host lock. `Ok(None)` means another live host holds it.
fn acquire(paths: &WorkspacePaths) -> Result<Option<Ownership>, String> {
    prepare_runtime_dir(paths)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(paths.lock())
        .map_err(|e| format!("cannot open lock: {e}"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(std::fs::TryLockError::Error(e)) => return Err(format!("cannot lock: {e}")),
    }
    // Holding the lock proves no live owner: a leftover socket is stale.
    let socket = paths.socket();
    if let Ok(m) = std::fs::symlink_metadata(&socket) {
        if m.file_type().is_symlink() {
            return Err("socket path is a symlink; refusing to use it".into());
        }
        std::fs::remove_file(&socket).map_err(|e| format!("cannot remove stale socket: {e}"))?;
    }
    Ok(Some(Ownership {
        _lock: lock,
        paths: paths.clone(),
    }))
}

fn write_owner(paths: &WorkspacePaths, epoch: &str) {
    let record = serde_json::json!({
        "pid": std::process::id(),
        "host_epoch": epoch,
        "protocol_hash": schemas::protocol_hash(),
        "started_at": mira_protocol::Timestamp::now(),
    });
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(paths.owner())
    {
        let _ = writeln!(f, "{record}");
    }
}

pub async fn serve(paths: WorkspacePaths) -> u8 {
    let ownership = match acquire(&paths) {
        Ok(Some(o)) => o,
        Ok(None) => return 0,
        Err(e) => {
            diag(format!("host start refused: {e}"));
            return 7;
        }
    };
    let listener = match UnixListener::bind(paths.socket()) {
        Ok(l) => l,
        Err(e) => {
            diag(format!("cannot bind socket: {e}"));
            return 7;
        }
    };
    let _ = std::fs::set_permissions(paths.socket(), std::fs::Permissions::from_mode(0o600));

    let (tx, rx) = mpsc::channel::<Msg>(1024);
    let actor = Actor::new(paths.clone(), rx, tx.clone());
    write_owner(&paths, actor.epoch().as_str());
    diag(format!(
        "host started pid={} epoch={} root={}",
        std::process::id(),
        actor.epoch(),
        paths.root
    ));
    let actor_task = tokio::spawn(actor.run());

    // Installing handlers (instead of inheriting SIG_IGN from the spawning shell) makes every
    // child start with default SIGINT/SIGQUIT dispositions, so `stop_signal: interrupt` works.
    // The host has its own process group, so terminal Ctrl-C never reaches it.
    let _int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();
    let _quit = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::quit()).ok();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
    let mut actor_task = actor_task;
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => { tokio::spawn(connection(stream, tx.clone())); }
                Err(e) => diag(format!("accept failed: {e}")),
            },
            _ = async { match term.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending().await } } => {
                let _ = tx.send(Msg::Shutdown).await;
            }
            _ = async { match hup.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending().await } } => {
                let _ = tx.send(Msg::Shutdown).await;
            }
            _ = &mut actor_task => break,
        }
    }
    drop(ownership);
    diag("host stopped");
    0
}

fn response_line(resp: &RpcResponse) -> Vec<u8> {
    let mut v = serde_json::to_vec(resp).unwrap_or_else(|_| b"{}".to_vec());
    v.push(b'\n');
    v
}

pub fn error_line(id: Option<String>, error: RpcError) -> Vec<u8> {
    response_line(&RpcResponse::Failure {
        jsonrpc: JsonRpc2,
        id,
        error,
    })
}

pub fn result_line(id: String, result: Value) -> Vec<u8> {
    response_line(&RpcResponse::Success {
        jsonrpc: JsonRpc2,
        id,
        result,
    })
}

enum Parsed {
    Request(RpcRequest),
    Invalid(Option<String>, RpcError),
}

fn parse_request(line: &[u8]) -> Parsed {
    let value = match strict_json::parse(line, MAX_IPC_FRAME_BYTES) {
        Ok(v) => v,
        Err(e) => {
            return Parsed::Invalid(None, RpcError::new(RpcError::PARSE_ERROR, e.to_string()));
        }
    };
    if !value.is_object() {
        return Parsed::Invalid(
            None,
            RpcError::new(
                RpcError::INVALID_REQUEST,
                "request must be one JSON object; batches are not supported",
            ),
        );
    }
    let id = value.get("id").and_then(Value::as_str).map(str::to_owned);
    match serde_json::from_value::<RpcRequest>(value) {
        Ok(r) if r.id.is_empty() => Parsed::Invalid(
            None,
            RpcError::new(RpcError::INVALID_REQUEST, "id must be a non-empty string"),
        ),
        Ok(r) => Parsed::Request(r),
        Err(e) => Parsed::Invalid(id, RpcError::new(RpcError::INVALID_REQUEST, e.to_string())),
    }
}

async fn connection(stream: UnixStream, actor: mpsc::Sender<Msg>) {
    match stream.peer_cred() {
        Ok(c) if c.uid() == current_uid() => {}
        _ => return,
    }
    let (mut reader, mut writer) = stream.into_split();
    let (resp_tx, mut resp_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (event_tx, mut event_rx) = mpsc::channel::<Vec<u8>>(EVENT_QUEUE);
    let out = Outbound::new(resp_tx.clone(), event_tx);
    let writer_task = tokio::spawn(async move {
        loop {
            let line = tokio::select! {
                biased;
                r = resp_rx.recv() => r,
                e = event_rx.recv() => e,
            };
            let Some(line) = line else { break };
            if writer.write_all(&line).await.is_err() {
                break;
            }
        }
    });

    let mut decoder = LineDecoder::new(MAX_IPC_FRAME_BYTES);
    let mut buf = vec![0u8; 64 * 1024];
    let mut client: Option<(ClientId, ConnectionKind)> = None;
    let started = tokio::time::Instant::now();
    'conn: loop {
        let wait = if client.is_none() {
            HELLO_DEADLINE.saturating_sub(started.elapsed())
        } else if decoder.has_partial() {
            PARTIAL_FRAME_DEADLINE
        } else {
            Duration::from_secs(3600)
        };
        let n = match tokio::time::timeout(wait, reader.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Ok(Ok(n)) => n,
            Err(_) if client.is_none() || decoder.has_partial() => break,
            Err(_) => continue,
        };
        for line in decoder.push(&buf[..n]) {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    let _ = resp_tx.send(error_line(
                        None,
                        RpcError::new(RpcError::INVALID_REQUEST, e.to_string()),
                    ));
                    break 'conn;
                }
            };
            let req = match parse_request(&line) {
                Parsed::Request(r) => r,
                Parsed::Invalid(id, e) => {
                    let _ = resp_tx.send(error_line(id, e));
                    continue;
                }
            };
            let method = Method::parse(&req.method);
            match (&client, method) {
                (None, Some(Method::Hello)) => {
                    let params: HelloParams =
                        match serde_json::from_value(Value::Object(req.params)) {
                            Ok(p) => p,
                            Err(e) => {
                                let _ = resp_tx.send(error_line(
                                    Some(req.id),
                                    RpcError::new(RpcError::INVALID_PARAMS, e.to_string()),
                                ));
                                break 'conn;
                            }
                        };
                    let kind = params.connection_kind;
                    let (reply_tx, reply_rx) = oneshot::channel();
                    if actor
                        .send(Msg::Hello {
                            params,
                            out: out.clone(),
                            reply: reply_tx,
                        })
                        .await
                        .is_err()
                    {
                        break 'conn;
                    }
                    match reply_rx.await {
                        Ok(Ok((id, hello))) => {
                            client = Some((id, kind));
                            let _ = resp_tx.send(result_line(
                                req.id,
                                serde_json::to_value(hello).unwrap_or(Value::Null),
                            ));
                        }
                        Ok(Err(e)) => {
                            let _ = resp_tx.send(error_line(Some(req.id), e));
                            break 'conn;
                        }
                        Err(_) => break 'conn,
                    }
                }
                (None, _) => {
                    let info =
                        ErrorInfo::new(ErrorCode::PROTOCOL_MISMATCH, "hello is required first");
                    let _ = resp_tx.send(error_line(
                        Some(req.id),
                        RpcError::new(RpcError::HANDSHAKE, "hello is required first")
                            .with_info(info),
                    ));
                    break 'conn;
                }
                (Some(_), None) => {
                    let _ = resp_tx.send(error_line(
                        Some(req.id),
                        RpcError::new(
                            RpcError::METHOD_NOT_FOUND,
                            format!("unknown method `{}`", req.method),
                        ),
                    ));
                }
                (Some(_), Some(Method::Hello)) => {
                    let _ = resp_tx.send(error_line(
                        Some(req.id),
                        RpcError::new(RpcError::INVALID_REQUEST, "hello was already completed"),
                    ));
                }
                (Some((id, kind)), Some(m)) => {
                    if *kind == ConnectionKind::Stream && !m.allowed_on_stream() {
                        let _ = resp_tx.send(error_line(
                            Some(req.id),
                            RpcError::new(
                                RpcError::NOT_ALLOWED,
                                "stream connections may only subscribe",
                            ),
                        ));
                        continue;
                    }
                    if actor
                        .send(Msg::Request {
                            client: id.clone(),
                            id: req.id,
                            method: m,
                            params: req.params,
                        })
                        .await
                        .is_err()
                    {
                        break 'conn;
                    }
                }
            }
        }
    }
    if let Some((id, _)) = client {
        let _ = actor.send(Msg::Closed { client: id }).await;
    }
    drop(out);
    drop(resp_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), writer_task).await;
}
