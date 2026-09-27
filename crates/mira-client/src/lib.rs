//! Typed MIPC/1 client shared by the CLI and the TUI.
//!
//! One [`Client`] owns one connection. Control connections carry requests; stream
//! connections carry subscriptions. The client never spawns project commands; it only
//! starts the workspace host when none is running.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use mira_protocol::ipc::*;
use mira_protocol::limits::MAX_IPC_FRAME_BYTES;
use mira_protocol::mpp::LineDecoder;
use mira_protocol::paths::{WorkspacePaths, current_uid};
use mira_protocol::reply::{PublicReply, ReplyContext};
use mira_protocol::{ErrorCode, ErrorInfo, schemas};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
pub const RPC_TIMEOUT: Duration = Duration::from_secs(5);
pub const MAINTENANCE_TIMEOUT: Duration = Duration::from_secs(30);
const SPAWN_WAIT: Duration = Duration::from_secs(4);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot reach the workspace host: {0}")]
    Connect(String),
    #[error("{}", .0.message)]
    Refused(ErrorInfo),
    #[error("host returned a JSON-RPC error {}: {}", .0.code, .0.message)]
    Rpc(RpcError),
    #[error("host connection failed: {0}")]
    Io(String),
    #[error("invalid host message: {0}")]
    Protocol(String),
    #[error("the host did not answer within {0:?}")]
    Timeout(Duration),
}

impl ClientError {
    /// Public error for CLI output. Accepted side effects are not undone by a client timeout.
    pub fn to_error_info(&self) -> ErrorInfo {
        match self {
            Self::Refused(info) => info.clone(),
            Self::Rpc(e) => e.data.clone().unwrap_or_else(|| {
                let code = match e.code {
                    RpcError::INVALID_PARAMS | RpcError::INVALID_REQUEST => {
                        ErrorCode::INVALID_ARGUMENT
                    }
                    RpcError::METHOD_NOT_FOUND => ErrorCode::NOT_FOUND,
                    _ => ErrorCode::INTERNAL,
                };
                ErrorInfo::new(code, e.message.clone())
            }),
            Self::Timeout(_) => ErrorInfo::new(ErrorCode::TIMEOUT, self.to_string())
                .retryable(true)
                .with_next_action(
                    &["mira", "status"],
                    "Check whether the request was accepted before retrying.",
                ),
            _ => ErrorInfo::new(ErrorCode::INTERNAL, self.to_string()).retryable(true),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub client_kind: ClientKind,
    pub connection_kind: ConnectionKind,
    /// Start a host when none is listening.
    pub spawn: bool,
    /// Executable that serves `__host`; defaults to the current executable.
    pub host_exe: Option<PathBuf>,
}

impl ConnectOptions {
    pub fn cli() -> Self {
        Self {
            client_kind: ClientKind::Cli,
            connection_kind: ConnectionKind::Control,
            spawn: true,
            host_exe: None,
        }
    }
    pub fn kind(mut self, client_kind: ClientKind, connection_kind: ConnectionKind) -> Self {
        self.client_kind = client_kind;
        self.connection_kind = connection_kind;
        self
    }
}

/// A connected client after a successful handshake.
pub struct Client {
    conn: Connection,
    hello: HelloReply,
}

/// The framed JSON-RPC connection under a [`Client`].
struct Connection {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    decoder: LineDecoder,
    lines: VecDeque<Vec<u8>>,
    events: VecDeque<StreamFrame>,
    next_id: u64,
}

async fn try_connect(socket: &Path) -> std::io::Result<UnixStream> {
    match tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(socket)).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "connect timed out",
        )),
    }
}

fn spawn_host(paths: &WorkspacePaths, exe: &Path) -> Result<(), ClientError> {
    use std::os::unix::process::CommandExt;
    std::fs::create_dir_all(&paths.logs_dir)
        .map_err(|e| ClientError::Connect(format!("cannot create log dir: {e}")))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.host_log())
        .map_err(|e| ClientError::Connect(format!("cannot open host log: {e}")))?;
    let mut child = std::process::Command::new(exe)
        .arg("__host")
        .arg("--root")
        .arg(paths.root.as_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()
        .map_err(|e| ClientError::Connect(format!("cannot start host: {e}")))?;
    // Reap the host if it exits while this client still runs.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Connects to the workspace host, starting one if allowed and needed.
pub async fn connect(paths: &WorkspacePaths, opts: &ConnectOptions) -> Result<Client, ClientError> {
    let socket = paths.socket();
    let stream = match try_connect(&socket).await {
        Ok(s) => s,
        Err(first) if opts.spawn => {
            let exe = match &opts.host_exe {
                Some(e) => e.clone(),
                None => std::env::current_exe().map_err(|e| ClientError::Connect(e.to_string()))?,
            };
            spawn_host(paths, &exe)?;
            let deadline = tokio::time::Instant::now() + SPAWN_WAIT;
            loop {
                tokio::time::sleep(Duration::from_millis(25)).await;
                match try_connect(&socket).await {
                    Ok(s) => break s,
                    Err(e) if tokio::time::Instant::now() >= deadline => {
                        return Err(ClientError::Connect(format!(
                            "host did not start ({e}; first error: {first}); read `mira logs --host` ({})",
                            paths.host_log().display()
                        )));
                    }
                    Err(_) => {}
                }
            }
        }
        Err(e) => return Err(ClientError::Connect(e.to_string())),
    };
    let peer = stream
        .peer_cred()
        .map_err(|e| ClientError::Connect(format!("cannot read peer credentials: {e}")))?;
    if peer.uid() != current_uid() {
        return Err(ClientError::Refused(ErrorInfo::new(
            ErrorCode::PERMISSION_DENIED,
            format!(
                "the host socket {} is owned by another user",
                socket.display()
            ),
        )));
    }
    let (reader, writer) = stream.into_split();
    let mut conn = Connection {
        reader,
        writer,
        decoder: LineDecoder::new(MAX_IPC_FRAME_BYTES),
        lines: VecDeque::new(),
        events: VecDeque::new(),
        next_id: 0,
    };
    let params = HelloParams {
        api: mira_protocol::ids::Api1,
        protocol_hash: schemas::protocol_hash().clone(),
        workspace_id: paths.id.clone(),
        workspace_root: paths.root.clone(),
        client_kind: opts.client_kind,
        connection_kind: opts.connection_kind,
    };
    let value = conn
        .raw_call(Method::Hello, &params, HELLO_TIMEOUT)
        .await
        .map_err(|e| match e {
            ClientError::Rpc(r) if r.data.is_some() => ClientError::Refused(
                r.data
                    .unwrap_or_else(|| ErrorInfo::new(ErrorCode::INTERNAL, "")),
            ),
            other => other,
        })?;
    let hello = serde_json::from_value(value)
        .map_err(|e| ClientError::Protocol(format!("bad hello reply: {e}")))?;
    Ok(Client { conn, hello })
}

impl Client {
    pub fn hello(&self) -> &HelloReply {
        &self.hello
    }

    /// Reply context from the handshake, for errors produced on the client side.
    pub fn context(&self) -> ReplyContext {
        ReplyContext {
            workspace: Some(self.hello.workspace.clone()),
            host_epoch: Some(self.hello.host_epoch.clone()),
            catalog_revision: Some(self.hello.catalog_revision),
            state_revision: Some(self.hello.state_revision),
        }
    }

    /// Calls a method whose successful answer is a `PublicReply<R>`.
    pub async fn call<P: Serialize, R: DeserializeOwned>(
        &mut self,
        method: Method,
        params: &P,
    ) -> Result<PublicReply<R>, ClientError> {
        self.call_with_timeout(method, params, RPC_TIMEOUT).await
    }

    pub async fn call_with_timeout<P: Serialize, R: DeserializeOwned>(
        &mut self,
        method: Method,
        params: &P,
        timeout: Duration,
    ) -> Result<PublicReply<R>, ClientError> {
        let value = self.conn.raw_call(method, params, timeout).await?;
        serde_json::from_value(value)
            .map_err(|e| ClientError::Protocol(format!("bad {} reply: {e}", method.name())))
    }

    /// The next stream frame, waiting for one if none is buffered.
    pub async fn next_event(&mut self) -> Result<StreamFrame, ClientError> {
        self.conn.next_event().await
    }

    pub async fn status(&mut self) -> Result<PublicReply<StatusData>, ClientError> {
        self.call(Method::WorkspaceStatus, &Empty {}).await
    }
}

impl Connection {
    async fn read_line(&mut self) -> Result<Vec<u8>, ClientError> {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if let Some(line) = self.lines.pop_front() {
                return Ok(line);
            }
            let n = self
                .reader
                .read(&mut buf)
                .await
                .map_err(|e| ClientError::Io(e.to_string()))?;
            if n == 0 {
                return Err(ClientError::Io("host closed the connection".into()));
            }
            for line in self.decoder.push(&buf[..n]) {
                self.lines
                    .push_back(line.map_err(|e| ClientError::Protocol(e.to_string()))?);
            }
        }
    }

    async fn write_json<T: Serialize>(&mut self, value: &T) -> Result<(), ClientError> {
        let mut line =
            serde_json::to_vec(value).map_err(|e| ClientError::Protocol(e.to_string()))?;
        line.push(b'\n');
        self.writer
            .write_all(&line)
            .await
            .map_err(|e| ClientError::Io(e.to_string()))
    }

    async fn raw_call<P: Serialize>(
        &mut self,
        method: Method,
        params: &P,
        timeout: Duration,
    ) -> Result<serde_json::Value, ClientError> {
        self.next_id += 1;
        let id = format!("q{}", self.next_id);
        let params =
            match serde_json::to_value(params).map_err(|e| ClientError::Protocol(e.to_string()))? {
                serde_json::Value::Object(m) => m,
                _ => {
                    return Err(ClientError::Protocol(
                        "params must serialize to an object".into(),
                    ));
                }
            };
        let req = RpcRequest {
            jsonrpc: JsonRpc2,
            id: id.clone(),
            method: method.name().to_owned(),
            params,
        };
        self.write_json(&req).await?;
        let wait = async {
            loop {
                let line = self.read_line().await?;
                let msg: HostMessage = serde_json::from_slice(&line)
                    .map_err(|e| ClientError::Protocol(e.to_string()))?;
                match msg {
                    HostMessage::Notification(n) => self.events.push_back(n.params),
                    HostMessage::Response(RpcResponse::Success {
                        id: rid, result, ..
                    }) if rid == id => return Ok(result),
                    HostMessage::Response(RpcResponse::Failure { id: rid, error, .. })
                        if rid.as_deref() == Some(id.as_str()) || rid.is_none() =>
                    {
                        return Err(ClientError::Rpc(error));
                    }
                    HostMessage::Response(_) => {}
                }
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .map_err(|_| ClientError::Timeout(timeout))?
    }

    async fn next_event(&mut self) -> Result<StreamFrame, ClientError> {
        loop {
            if let Some(ev) = self.events.pop_front() {
                return Ok(ev);
            }
            let line = self.read_line().await?;
            match serde_json::from_slice::<HostMessage>(&line)
                .map_err(|e| ClientError::Protocol(e.to_string()))?
            {
                HostMessage::Notification(n) => return Ok(n.params),
                HostMessage::Response(_) => {}
            }
        }
    }
}
