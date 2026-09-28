//! Mira protocol contract: IDs, wire DTOs, validated domain types, errors, and schemas.
//!
//! Pipeline: bytes → wire DTO ([`strict_json`], serde) → validated domain ([`manifest`],
//! [`mpp`], [`view`]) → effects in the host. This crate performs no async IO and never
//! executes plugin code.

pub mod catalog;
pub mod clock;
pub mod config;
pub mod error;
pub mod hash;
pub mod ids;
pub mod ipc;
pub mod limits;
pub mod manifest;
pub mod mpp;
pub mod paths;
pub mod reply;
pub mod run;
pub mod schema_profile;
pub mod schemas;
pub mod strict_json;
pub mod template;
pub mod time;
pub mod update;
pub mod view;
pub mod workspace;

/// The single supported API version for manifests, MPP/1, MIPC/1 and CLI replies.
pub const API_VERSION: u64 = 1;

/// The `mira` package version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use error::{ErrorCode, ErrorInfo, Issue, Issues};
pub use ids::*;
pub use time::Timestamp;
