//! The JSON-RPC 2.0 envelope: requests, responses, errors, and notifications.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::error::ErrorInfo;
use crate::manifest::present;

use super::StreamFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JsonRpc2;
impl Serialize for JsonRpc2 {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("2.0")
    }
}
impl<'de> Deserialize<'de> for JsonRpc2 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "2.0" {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("jsonrpc must be \"2.0\""))
        }
    }
}
impl JsonSchema for JsonRpc2 {
    fn schema_name() -> Cow<'static, str> {
        "JsonRpc2".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"const": "2.0"})
    }
}

/// A request. `id` is a non-empty string and `params` an object (no batches, no positional params).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RpcRequest {
    pub jsonrpc: JsonRpc2,
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Map<String, Value>,
}

/// A host notification (no id). Only `stream.event` is defined.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RpcNotification {
    pub jsonrpc: JsonRpc2,
    pub method: String,
    pub params: StreamFrame,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(with = "ErrorInfo")]
    pub data: Option<ErrorInfo>,
}

impl RpcError {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// Handshake refused or required (workspace, api, or protocol hash mismatch).
    pub const HANDSHAKE: i64 = -32000;
    /// Method not allowed on this connection kind.
    pub const NOT_ALLOWED: i64 = -32001;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
    pub fn with_info(mut self, info: ErrorInfo) -> Self {
        self.data = Some(info);
        self
    }
}

/// A response carries exactly one of `result` or `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RpcResponse {
    Success {
        jsonrpc: JsonRpc2,
        id: String,
        result: Value,
    },
    Failure {
        jsonrpc: JsonRpc2,
        id: Option<String>,
        error: RpcError,
    },
}

/// Anything the host may send on a connection.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum HostMessage {
    Notification(RpcNotification),
    Response(RpcResponse),
}
