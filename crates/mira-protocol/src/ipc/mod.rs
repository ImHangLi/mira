//! MIPC/1: JSON-RPC 2.0 single-object profile over a Unix socket.
//!
//! Method names, request DTOs, result DTOs, and their schemas are registered once in
//! [`methods!`]. Successful application answers are `PublicReply<Result>`; transport and
//! envelope failures use numeric JSON-RPC errors.

mod envelope;
mod params;
mod results;
mod stream;

use schemars::SchemaGenerator;
use serde_json::{Map, Value};

use crate::run::RunRecord;
use crate::view::ViewSnapshot;

pub use envelope::*;
pub use params::*;
pub use results::*;
pub use stream::*;

// ---------------------------------------------------------------------------
// Method registry
// ---------------------------------------------------------------------------

macro_rules! methods {
    ($($variant:ident = $name:literal, $params:ty => $result:ty, stream: $stream:literal;)*) => {
        /// Every MIPC/1 method.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Method { $($variant),* }

        impl Method {
            pub const ALL: &'static [Method] = &[$(Method::$variant),*];
            pub fn name(self) -> &'static str {
                match self { $(Method::$variant => $name),* }
            }
            pub fn parse(name: &str) -> Option<Self> {
                match name { $($name => Some(Method::$variant),)* _ => None }
            }
            /// Stream connections may only call subscribe after hello.
            pub fn allowed_on_stream(self) -> bool {
                match self { $(Method::$variant => $stream),* }
            }
        }

        /// Request/result schemas keyed by method name.
        pub fn method_schemas(generator: &mut SchemaGenerator) -> Map<String, Value> {
            let mut out = Map::new();
            $(
                let params = generator.subschema_for::<$params>();
                let result = generator.subschema_for::<$result>();
                out.insert($name.to_owned(), serde_json::json!({"params": params, "result": result}));
            )*
            out
        }
    };
}

methods! {
    Hello = "hello", HelloParams => HelloReply, stream: true;
    SessionAttach = "session.attach", SessionAttachParams => SessionData, stream: false;
    SessionOpen = "session.open", SessionOpenParams => SessionData, stream: false;
    SessionKeep = "session.keep", SessionKeepParams => SessionData, stream: false;
    SessionStop = "session.stop", Empty => SessionStopData, stream: false;
    WorkspaceStatus = "workspace.status", Empty => StatusData, stream: false;
    CatalogList = "catalog.list", CatalogListParams => CatalogList, stream: false;
    ItemDescribe = "item.describe", ItemDescribeParams => ItemDescription, stream: false;
    ActionInvoke = "action.invoke", ActionInvokeParams => InvokeAccepted, stream: false;
    ActionExec = "action.exec", ActionExecParams => InvokeAccepted, stream: false;
    RunStop = "run.stop", RunStopParams => StopAccepted, stream: false;
    RunGet = "run.get", RunGetParams => RunRecord, stream: false;
    RunList = "run.list", RunListParams => RunList, stream: false;
    LogRead = "log.read", LogReadParams => LogPage, stream: false;
    ViewRead = "view.read", ViewReadParams => ViewSnapshot, stream: false;
    ViewPublish = "view.publish", ViewPublishParams => PublishResult, stream: false;
    ViewAction = "view.action", ViewActionParams => InvokeAccepted, stream: false;
    ConfigApply = "config.apply", ConfigApplyParams => ConfigApplied, stream: false;
    ConfigReload = "config.reload", Empty => ConfigApplied, stream: false;
    ScheduleSet = "schedule.set", ScheduleSetParams => ScheduleData, stream: false;
    TerminalSnapshot = "terminal.snapshot", TerminalSnapshotParams => TerminalSnapshot, stream: false;
    TerminalAcquire = "terminal.acquire", TerminalRunParams => Ack, stream: false;
    TerminalRelease = "terminal.release", TerminalRunParams => Ack, stream: false;
    TerminalInput = "terminal.input", TerminalInputParams => TerminalSnapshot, stream: false;
    TerminalResize = "terminal.resize", TerminalResizeParams => Ack, stream: false;
    StreamSubscribe = "stream.subscribe", StreamSubscribeParams => Subscribed, stream: true;
    StorageStatus = "storage.status", StorageStatusParams => StorageStatusData, stream: false;
    StorageGc = "storage.gc", StorageGcParams => GcReport, stream: false;
    StorageClear = "storage.clear", StorageClearParams => Ack, stream: false;
    ArtifactList = "artifact.list", ArtifactListParams => ArtifactListData, stream: false;
    ArtifactRead = "artifact.read", ArtifactReadParams => ChunkData, stream: false;
    PayloadRead = "payload.read", PayloadReadParams => ChunkData, stream: false;
}

/// The notification method carrying [`StreamFrame`]s.
pub const STREAM_EVENT: &str = "stream.event";
