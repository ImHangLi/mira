//! Typed view data and view snapshot metadata.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{Issue, Issues};
use crate::ids::{Digest, RunId, ViewRef, ViewRevision};
use crate::limits::*;
use crate::manifest::{ViewKind, present};
use crate::strict_json;
use crate::time::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TextFormat {
    #[default]
    Plain,
    Markdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ColumnType {
    Text,
    Number,
    Boolean,
    Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Column {
    pub id: String,
    pub label: String,
    #[serde(rename = "type")]
    pub column_type: ColumnType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub id: String,
    /// Column ID → value; `null` means no value. Every column must be present.
    pub values: Map<String, Value>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogItem {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub level: LogLevel,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Timestamp")]
    pub producer_at: Option<Timestamp>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Map<String, Value>")]
    pub fields: Option<Map<String, Value>>,
    /// Set by the host when stored and returned by `view.read`. Producers must omit it:
    /// frame validation rejects a producer-supplied value.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Timestamp")]
    pub recorded_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TreeNode {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TreeNode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ViewData {
    Text {
        text: String,
        #[serde(default)]
        format: TextFormat,
    },
    Table {
        columns: Vec<Column>,
        rows: Vec<Row>,
    },
    Log {
        items: Vec<LogItem>,
    },
    Tree {
        nodes: Vec<TreeNode>,
    },
    Json {
        value: Value,
    },
}

impl ViewData {
    pub fn kind(&self) -> ViewKind {
        match self {
            Self::Text { .. } => ViewKind::Text,
            Self::Table { .. } => ViewKind::Table,
            Self::Log { .. } => ViewKind::Log,
            Self::Tree { .. } => ViewKind::Tree,
            Self::Json { .. } => ViewKind::Json,
        }
    }

    /// Validates the whole value; any issue rejects the entire update.
    pub fn validate(&self, pointer: &str) -> Issues {
        let mut issues = Issues::default();
        match self {
            Self::Text { text, .. } => {
                if text.len() > MAX_TEXT_VIEW_BYTES {
                    issues.push(Issue::schema(
                        format!("{pointer}/text"),
                        "text view exceeds 1 MiB",
                    ));
                }
            }
            Self::Table { columns, rows } => validate_table(columns, rows, pointer, &mut issues),
            Self::Log { items } => {
                let mut ids = BTreeSet::new();
                for (i, item) in items.iter().enumerate() {
                    let p = format!("{pointer}/items/{i}");
                    validate_log_item(item, &p, &mut issues);
                    if !ids.insert(item.id.as_str()) {
                        issues.push(Issue::schema(
                            format!("{p}/id"),
                            "duplicate log item id in one batch",
                        ));
                    }
                }
            }
            Self::Tree { nodes } => {
                let mut count = 0usize;
                let mut ids = BTreeSet::new();
                for (i, n) in nodes.iter().enumerate() {
                    validate_tree(
                        n,
                        1,
                        &format!("{pointer}/nodes/{i}"),
                        &mut count,
                        &mut ids,
                        &mut issues,
                    );
                }
                if count > MAX_TREE_NODES {
                    issues.push(Issue::schema(
                        format!("{pointer}/nodes"),
                        "tree exceeds 10000 nodes",
                    ));
                }
            }
            Self::Json { value } => {
                if strict_json::depth(value) > MAX_JSON_DEPTH {
                    issues.push(Issue::schema(
                        format!("{pointer}/value"),
                        "json value is deeper than 64",
                    ));
                }
            }
        }
        issues
    }
}

pub fn validate_log_item(item: &LogItem, p: &str, issues: &mut Issues) {
    if item.id.is_empty() || item.id.len() > MAX_ROW_ID_BYTES {
        issues.push(Issue::schema(
            format!("{p}/id"),
            "log item id must be 1-128 bytes",
        ));
    }
    if item.text.len() > MAX_LOG_TEXT_BYTES {
        issues.push(Issue::schema(
            format!("{p}/text"),
            "log item text exceeds 8 KiB",
        ));
    }
    if item.recorded_at.is_some() {
        issues.push(Issue::schema(
            format!("{p}/recorded_at"),
            "recorded_at is set by the host; producers use producer_at",
        ));
    }
}

fn validate_table(columns: &[Column], rows: &[Row], pointer: &str, issues: &mut Issues) {
    if columns.len() > MAX_TABLE_COLUMNS {
        issues.push(Issue::schema(
            format!("{pointer}/columns"),
            "table exceeds 64 columns",
        ));
    }
    if rows.len() > MAX_TABLE_ROWS {
        issues.push(Issue::schema(
            format!("{pointer}/rows"),
            "table exceeds 10000 rows",
        ));
    }
    let mut col_ids = BTreeSet::new();
    for (i, c) in columns.iter().enumerate() {
        if c.id.is_empty() || c.id.len() > MAX_ROW_ID_BYTES {
            issues.push(Issue::schema(
                format!("{pointer}/columns/{i}/id"),
                "column id must be 1-128 bytes",
            ));
        }
        if !col_ids.insert(c.id.as_str()) {
            issues.push(Issue::schema(
                format!("{pointer}/columns/{i}/id"),
                "duplicate column id",
            ));
        }
    }
    let mut row_ids = BTreeSet::new();
    for (r, row) in rows.iter().enumerate() {
        let p = format!("{pointer}/rows/{r}");
        if row.id.is_empty() || row.id.len() > MAX_ROW_ID_BYTES {
            issues.push(Issue::schema(
                format!("{p}/id"),
                "row id must be 1-128 bytes",
            ));
        }
        if !row_ids.insert(row.id.as_str()) {
            issues.push(Issue::schema(format!("{p}/id"), "duplicate row id"));
        }
        if row.values.len() != columns.len()
            || !columns.iter().all(|c| row.values.contains_key(&c.id))
        {
            issues.push(Issue::schema(
                format!("{p}/values"),
                "row values must have exactly one entry per column",
            ));
            continue;
        }
        for c in columns {
            let v = &row.values[&c.id];
            let ok = match (c.column_type, v) {
                (_, Value::Null) => true,
                (ColumnType::Text, Value::String(_)) => true,
                (ColumnType::Number, Value::Number(n)) => n.as_f64().is_some_and(f64::is_finite),
                (ColumnType::Boolean, Value::Bool(_)) => true,
                (ColumnType::Timestamp, Value::String(s)) => Timestamp::parse(s).is_ok(),
                _ => false,
            };
            if !ok {
                issues.push(Issue::schema(
                    format!("{p}/values/{}", crate::error::pointer_token(&c.id)),
                    format!("value does not match column type {:?}", c.column_type),
                ));
            }
        }
        if issues.0.len() > crate::error::MAX_REPORTED_ISSUES {
            return;
        }
    }
}

fn validate_tree(
    n: &TreeNode,
    depth: usize,
    p: &str,
    count: &mut usize,
    ids: &mut BTreeSet<String>,
    issues: &mut Issues,
) {
    *count += 1;
    if depth > MAX_TREE_DEPTH {
        issues.push(Issue::schema(p, "tree is deeper than 32 levels"));
        return;
    }
    if n.id.is_empty() || !ids.insert(n.id.clone()) {
        issues.push(Issue::schema(
            format!("{p}/id"),
            "tree node ids must be non-empty and unique",
        ));
    }
    for (i, c) in n.children.iter().enumerate() {
        validate_tree(
            c,
            depth + 1,
            &format!("{p}/children/{i}"),
            count,
            ids,
            issues,
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ViewOp {
    Replace,
    Append,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Plugin,
    Cli,
    Hook,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Current,
    Historical,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    Committed,
    Buffered,
    SessionOnly,
    Unavailable,
}

/// A large value returned by reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReferenceData {
    pub representation: ReferenceTag,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceTag {
    Reference,
}

/// Inline view data (possibly one page of it) or a reference to the full value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ViewBody {
    Inline(ViewData),
    Reference(ReferenceData),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewSnapshot {
    pub view_ref: ViewRef,
    /// `null` when the view never received data (or its data was cleaned up).
    pub view_revision: Option<ViewRevision>,
    pub kind: ViewKind,
    pub recorded_at: Option<Timestamp>,
    pub source_run_id: Option<RunId>,
    pub source_kind: Option<SourceKind>,
    pub definition_hash: Digest,
    pub freshness: Freshness,
    pub freshness_reason: Option<String>,
    pub durability: Durability,
    /// `null` only when no data exists; see `freshness_reason` for why.
    pub data: Option<ViewBody>,
}
