//! Distinct validated ID, revision, and path types.
//!
//! Every type deserializes through its validating constructor, so an unvalidated string
//! can never become a domain ID.

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};

use crate::limits::{MAX_REQUEST_KEY_BYTES, MAX_SAFE_INTEGER};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind}: {reason}")]
pub struct IdError {
    pub kind: &'static str,
    pub reason: &'static str,
}

fn err(kind: &'static str, reason: &'static str) -> IdError {
    IdError { kind, reason }
}

macro_rules! string_newtype_common {
    ($name:ident, $pattern:expr) => {
        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl std::str::FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s.to_owned())
            }
        }
        impl TryFrom<String> for $name {
            type Error = IdError;
            fn try_from(s: String) -> Result<Self, Self::Error> {
                Self::parse(s)
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                Self::parse(s).map_err(serde::de::Error::custom)
            }
        }
        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                stringify!($name).into()
            }
            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({"type": "string", "pattern": $pattern})
            }
        }
    };
}

fn is_local_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 48
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

macro_rules! local_id {
    ($name:ident, $kind:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);
        impl $name {
            pub fn parse(s: String) -> Result<Self, IdError> {
                if is_local_id(&s) {
                    Ok(Self(s))
                } else {
                    Err(err($kind, "expected ^[a-z][a-z0-9-]{0,47}$"))
                }
            }
        }
        string_newtype_common!($name, "^[a-z][a-z0-9-]{0,47}$");
    };
}

local_id!(PluginId, "plugin id");
local_id!(ActionId, "action id");
local_id!(ViewId, "view id");
// A local item ID whose kind (action or view) is not yet resolved.
local_id!(LocalId, "item id");

impl From<ActionId> for LocalId {
    fn from(v: ActionId) -> Self {
        Self(v.0)
    }
}
impl From<ViewId> for LocalId {
    fn from(v: ViewId) -> Self {
        Self(v.0)
    }
}

macro_rules! item_ref {
    ($name:ident, $local:ident, $field:ident, $kind:literal) => {
        /// `plugin.item` reference (exactly one dot).
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name {
            pub plugin: PluginId,
            pub $field: $local,
        }
        impl $name {
            pub fn new(plugin: PluginId, $field: $local) -> Self {
                Self { plugin, $field }
            }
            pub fn parse(s: String) -> Result<Self, IdError> {
                let (p, i) = split_ref(&s).ok_or(err($kind, "expected plugin.item"))?;
                Ok(Self {
                    plugin: PluginId::parse(p.to_owned()).map_err(|_| err($kind, "bad plugin id"))?,
                    $field: $local::parse(i.to_owned()).map_err(|_| err($kind, "bad item id"))?,
                })
            }
            pub fn to_item_ref(&self) -> ItemRef {
                ItemRef {
                    plugin: self.plugin.clone(),
                    item: LocalId(self.$field.as_str().to_owned()),
                }
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}.{}", self.plugin, self.$field)
            }
        }
        impl std::str::FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s.to_owned())
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                Self::parse(String::deserialize(d)?).map_err(serde::de::Error::custom)
            }
        }
        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                stringify!($name).into()
            }
            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({"type": "string", "pattern": ITEM_REF_PATTERN})
            }
        }
    };
}

const ITEM_REF_PATTERN: &str = "^[a-z][a-z0-9-]{0,47}\\.[a-z][a-z0-9-]{0,47}$";

fn split_ref(s: &str) -> Option<(&str, &str)> {
    let (p, i) = s.split_once('.')?;
    if i.contains('.') {
        return None;
    }
    Some((p, i))
}

item_ref!(ActionRef, ActionId, action, "action ref");
item_ref!(ViewRef, ViewId, view, "view ref");
item_ref!(ItemRef, LocalId, item, "item ref");

impl ItemRef {
    pub fn as_action(&self) -> ActionRef {
        ActionRef::new(self.plugin.clone(), ActionId(self.item.0.clone()))
    }
    pub fn as_view(&self) -> ViewRef {
        ViewRef::new(self.plugin.clone(), ViewId(self.item.0.clone()))
    }
}

macro_rules! uuid_id {
    ($name:ident, $prefix:literal, $kind:literal, $pattern:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);
        impl $name {
            /// A fresh random ID (`prefix` + UUIDv4 as 32 lowercase hex characters).
            pub fn random() -> Self {
                Self(format!("{}{}", $prefix, uuid::Uuid::new_v4().simple()))
            }
            pub fn parse(s: String) -> Result<Self, IdError> {
                let hex = s
                    .strip_prefix($prefix)
                    .ok_or(err($kind, concat!("expected prefix ", $prefix)))?;
                let b = hex.as_bytes();
                let valid = b.len() == 32
                    && b.iter()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
                    && b[12] == b'4'
                    && matches!(b[16], b'8' | b'9' | b'a' | b'b');
                if valid {
                    Ok(Self(s))
                } else {
                    Err(err(
                        $kind,
                        "expected 32 lowercase hex characters of a UUIDv4",
                    ))
                }
            }
        }
        string_newtype_common!($name, $pattern);
    };
}

uuid_id!(RunId, "r_", "run id", "^r_[0-9a-f]{32}$");
uuid_id!(SessionId, "s_", "session id", "^s_[0-9a-f]{32}$");
uuid_id!(ClientId, "c_", "client id", "^c_[0-9a-f]{32}$");
uuid_id!(HostEpoch, "h_", "host epoch", "^h_[0-9a-f]{32}$");
uuid_id!(SubscriptionId, "u_", "subscription id", "^u_[0-9a-f]{32}$");

/// `w_` + first 24 lowercase hex characters of SHA-256 over the canonical root bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceId(String);
impl WorkspaceId {
    pub fn for_root(root: &AbsolutePath) -> Self {
        let digest = Sha256::digest(root.as_str().as_bytes());
        Self(format!("w_{}", &hex::encode(digest)[..24]))
    }
    pub fn parse(s: String) -> Result<Self, IdError> {
        let ok = s.strip_prefix("w_").is_some_and(|h| {
            h.len() == 24
                && h.bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        });
        if ok {
            Ok(Self(s))
        } else {
            Err(err("workspace id", "expected w_ + 24 lowercase hex"))
        }
    }
}
string_newtype_common!(WorkspaceId, "^w_[0-9a-f]{24}$");

/// `sha256:` + 64 lowercase hex characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest(String);
impl Digest {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
    }
    pub fn parse(s: String) -> Result<Self, IdError> {
        let ok = s.strip_prefix("sha256:").is_some_and(|h| {
            h.len() == 64
                && h.bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        });
        if ok {
            Ok(Self(s))
        } else {
            Err(err("digest", "expected sha256: + 64 lowercase hex"))
        }
    }
}
string_newtype_common!(Digest, "^sha256:[0-9a-f]{64}$");

/// Opaque 1–128 byte idempotency key supplied by a user or agent.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestKey(String);
impl RequestKey {
    pub fn parse(s: String) -> Result<Self, IdError> {
        if s.is_empty() || s.len() > MAX_REQUEST_KEY_BYTES || s.contains('\0') {
            return Err(err("request key", "expected 1-128 bytes without NUL"));
        }
        Ok(Self(s))
    }
}
impl fmt::Debug for RequestKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RequestKey({} bytes)", self.0.len())
    }
}
string_newtype_common!(RequestKey, "^[^\\u0000]{1,128}$");

/// An absolute, valid UTF-8 path without NUL. Canonicalization is the host's job.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AbsolutePath(String);
impl AbsolutePath {
    pub fn parse(s: String) -> Result<Self, IdError> {
        if !s.starts_with('/') || s.contains('\0') {
            return Err(err("absolute path", "expected an absolute UTF-8 path"));
        }
        Ok(Self(s))
    }
    /// Converts an OS path, rejecting non-UTF-8 names (`UNSUPPORTED_PATH_ENCODING`).
    pub fn from_path(p: &std::path::Path) -> Result<Self, IdError> {
        let s = p
            .to_str()
            .ok_or(err("absolute path", "path is not valid UTF-8"))?;
        Self::parse(s.to_owned())
    }
    pub fn as_path(&self) -> &std::path::Path {
        std::path::Path::new(&self.0)
    }
    pub fn join(&self, rel: &str) -> Self {
        Self(self.as_path().join(rel).to_string_lossy().into_owned())
    }
}
string_newtype_common!(AbsolutePath, "^/");

macro_rules! counter {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
        pub struct $name(u64);
        impl $name {
            pub const ZERO: Self = Self(0);
            pub fn new(v: u64) -> Result<Self, IdError> {
                if v > MAX_SAFE_INTEGER {
                    Err(err(stringify!($name), "exceeds 2^53-1"))
                } else {
                    Ok(Self(v))
                }
            }
            pub fn get(self) -> u64 {
                self.0
            }
            /// The next value, or `None` when the safe integer range is exhausted.
            pub fn next(self) -> Option<Self> {
                Self::new(self.0 + 1).ok()
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_u64(self.0)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                Self::new(u64::deserialize(d)?).map_err(serde::de::Error::custom)
            }
        }
        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                stringify!($name).into()
            }
            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({"type": "integer", "minimum": 0, "maximum": 9007199254740991u64})
            }
        }
    };
}

counter!(
    CatalogRevision,
    "Persisted; +1 when a new valid definition set is accepted."
);
counter!(
    StateRevision,
    "Per host epoch; +1 on control state, run lifecycle, or health change."
);
counter!(
    ViewRevision,
    "Strictly increasing per view; allocated from persisted blocks."
);
counter!(LogSeq, "Increasing sequence per run or log view.");
counter!(
    EventSeq,
    "Host receive order of stream events within one epoch."
);
counter!(ScreenRevision, "PTY screen revision.");

/// The literal API version `1`. Any other value is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Api1;
impl Serialize for Api1 {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(1)
    }
}
impl<'de> Deserialize<'de> for Api1 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match u64::deserialize(d)? {
            1 => Ok(Api1),
            _ => Err(serde::de::Error::custom(
                "unsupported api version; expected 1",
            )),
        }
    }
}
impl JsonSchema for Api1 {
    fn schema_name() -> Cow<'static, str> {
        "Api1".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"const": 1})
    }
}
