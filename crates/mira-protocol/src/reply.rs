//! `PublicReply`: the one JSON object every non-streaming CLI command prints.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ErrorInfo;
use crate::ids::{
    AbsolutePath, Api1, CatalogRevision, Digest, HostEpoch, StateRevision, WorkspaceId,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRef {
    pub id: WorkspaceId,
    pub root: AbsolutePath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PayloadMime {
    #[serde(rename = "application/json")]
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PayloadAvailability {
    Session,
    Retained,
}

/// Reference to a bounded result or view serialization held by the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PayloadRef {
    pub token: String,
    pub mime: PayloadMime,
    pub size_bytes: u64,
    pub sha256: Digest,
    pub availability: PayloadAvailability,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplyMeta {
    pub truncated: bool,
    pub next_cursor: Option<String>,
    pub not_modified: bool,
    pub payload: Option<PayloadRef>,
}

/// Host context copied into every reply; all `None` before a workspace is resolved.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplyContext {
    pub workspace: Option<WorkspaceRef>,
    pub host_epoch: Option<HostEpoch>,
    pub catalog_revision: Option<CatalogRevision>,
    pub state_revision: Option<StateRevision>,
}

/// Wire form. `ok=true` ⇔ `error=null`; `ok=false` ⇒ `data=null`. Built only through
/// [`PublicReply::success`]/[`PublicReply::failure`] or checked deserialization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    deny_unknown_fields,
    try_from = "PublicReplyWire<T>",
    bound(deserialize = "T: Deserialize<'de>")
)]
pub struct PublicReply<T = Value> {
    api: Api1,
    ok: bool,
    workspace: Option<WorkspaceRef>,
    host_epoch: Option<HostEpoch>,
    catalog_revision: Option<CatalogRevision>,
    state_revision: Option<StateRevision>,
    data: Option<T>,
    error: Option<ErrorInfo>,
    meta: ReplyMeta,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PublicReplyWire<T> {
    api: Api1,
    ok: bool,
    workspace: Option<WorkspaceRef>,
    host_epoch: Option<HostEpoch>,
    catalog_revision: Option<CatalogRevision>,
    state_revision: Option<StateRevision>,
    data: Option<T>,
    error: Option<ErrorInfo>,
    meta: ReplyMeta,
}

impl<T> TryFrom<PublicReplyWire<T>> for PublicReply<T> {
    type Error = &'static str;
    fn try_from(w: PublicReplyWire<T>) -> Result<Self, Self::Error> {
        match (w.ok, &w.data, &w.error) {
            (true, _, None) | (false, None, Some(_)) => Ok(Self {
                api: w.api,
                ok: w.ok,
                workspace: w.workspace,
                host_epoch: w.host_epoch,
                catalog_revision: w.catalog_revision,
                state_revision: w.state_revision,
                data: w.data,
                error: w.error,
                meta: w.meta,
            }),
            _ => Err("reply must be ok with error=null, or failed with data=null and an error"),
        }
    }
}

/// Domain view of a reply outcome.
pub enum ReplyOutcome<'a, T> {
    Success(&'a T),
    SuccessEmpty,
    Failure(&'a ErrorInfo),
}

impl<T> PublicReply<T> {
    pub fn success(ctx: ReplyContext, data: T, meta: ReplyMeta) -> Self {
        Self {
            api: Api1,
            ok: true,
            workspace: ctx.workspace,
            host_epoch: ctx.host_epoch,
            catalog_revision: ctx.catalog_revision,
            state_revision: ctx.state_revision,
            data: Some(data),
            error: None,
            meta,
        }
    }
    pub fn failure(ctx: ReplyContext, error: ErrorInfo) -> Self {
        Self {
            api: Api1,
            ok: false,
            workspace: ctx.workspace,
            host_epoch: ctx.host_epoch,
            catalog_revision: ctx.catalog_revision,
            state_revision: ctx.state_revision,
            data: None,
            error: Some(error),
            meta: ReplyMeta::default(),
        }
    }
    pub fn outcome(&self) -> ReplyOutcome<'_, T> {
        match (&self.error, &self.data) {
            (Some(e), _) => ReplyOutcome::Failure(e),
            (None, Some(d)) => ReplyOutcome::Success(d),
            (None, None) => ReplyOutcome::SuccessEmpty,
        }
    }
    pub fn is_ok(&self) -> bool {
        self.ok
    }
    pub fn data(&self) -> Option<&T> {
        self.data.as_ref()
    }
    pub fn into_data(self) -> Result<T, ErrorInfo> {
        match (self.error, self.data) {
            (Some(e), _) => Err(e),
            (None, Some(d)) => Ok(d),
            (None, None) => Err(ErrorInfo::new(
                crate::ErrorCode::INTERNAL,
                "reply carried no data",
            )),
        }
    }
    pub fn error(&self) -> Option<&ErrorInfo> {
        self.error.as_ref()
    }
    pub fn meta(&self) -> &ReplyMeta {
        &self.meta
    }
    pub fn meta_mut(&mut self) -> &mut ReplyMeta {
        &mut self.meta
    }
    pub fn context(&self) -> ReplyContext {
        ReplyContext {
            workspace: self.workspace.clone(),
            host_epoch: self.host_epoch.clone(),
            catalog_revision: self.catalog_revision,
            state_revision: self.state_revision,
        }
    }
    /// The fixed CLI exit code.
    pub fn exit_code(&self) -> u8 {
        self.error.as_ref().map_or(0, |e| e.code.exit_code())
    }
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> PublicReply<U> {
        PublicReply {
            api: self.api,
            ok: self.ok,
            workspace: self.workspace,
            host_epoch: self.host_epoch,
            catalog_revision: self.catalog_revision,
            state_revision: self.state_revision,
            data: self.data.map(f),
            error: self.error,
            meta: self.meta,
        }
    }
}
