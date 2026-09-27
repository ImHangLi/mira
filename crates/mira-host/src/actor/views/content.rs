//! Building new view content from one update: kind, revision, and binding checks, then
//! replace or append.

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::*;
use mira_protocol::manifest::ViewDefinition;
use mira_protocol::time::Timestamp;
use mira_protocol::view::*;

use super::ViewEntry;

fn same_item(a: &LogItem, b: &LogItem) -> bool {
    a.id == b.id
        && a.text == b.text
        && a.level == b.level
        && a.producer_at == b.producer_at
        && a.fields == b.fields
}

fn stamp(items: &mut [LogItem]) {
    let now = Timestamp::now();
    for i in items {
        i.recorded_at = Some(now);
    }
}

/// Checks one update against the definition and the current content, and builds the new
/// content. `Ok(None)` is a no-op (every appended log item already exists unchanged).
pub(super) fn build(
    def: &ViewDefinition,
    current: Option<&ViewEntry>,
    op: ViewOp,
    data: ViewData,
    expected: Option<ViewRevision>,
    log_cap: usize,
) -> Result<Option<ViewData>, ErrorInfo> {
    if data.kind() != def.kind {
        return Err(ErrorInfo::new(
            ErrorCode::SCHEMA_INVALID,
            format!(
                "view `{}` is a {:?} view; the update carries {:?} data",
                def.id,
                def.kind,
                data.kind()
            )
            .to_lowercase(),
        ));
    }
    if let Some(exp) = expected
        && current.map(|c| c.revision) != Some(exp)
    {
        return Err(ErrorInfo::new(
            ErrorCode::REVISION_CONFLICT,
            match current {
                Some(c) => format!(
                    "view is at revision {}, not {exp}; nothing was written",
                    c.revision
                ),
                None => format!("view has no data yet, not revision {exp}; nothing was written"),
            },
        ));
    }
    if let ViewData::Table { columns, .. } = &data {
        for ra in &def.row_actions {
            for (param, col) in &ra.bindings {
                if !columns.iter().any(|c| &c.id == col) {
                    return Err(ErrorInfo::new(
                        ErrorCode::SCHEMA_INVALID,
                        format!(
                            "row action `{}` binds `{param}` to column `{col}`, which this table does not have",
                            ra.action
                        ),
                    ));
                }
            }
        }
    }
    match (op, data) {
        (ViewOp::Replace, ViewData::Log { mut items }) => {
            stamp(&mut items);
            if items.len() > log_cap {
                items.drain(..items.len() - log_cap);
            }
            Ok(Some(ViewData::Log { items }))
        }
        (ViewOp::Replace, data) => Ok(Some(data)),
        (ViewOp::Append, ViewData::Log { items: incoming }) => {
            let mut items = match current.map(|c| &c.data) {
                Some(ViewData::Log { items }) => items.clone(),
                _ => Vec::new(),
            };
            let mut added = Vec::new();
            for (i, item) in incoming.into_iter().enumerate() {
                match items.iter().find(|e| e.id == item.id) {
                    Some(e) if same_item(e, &item) => {}
                    Some(_) => {
                        return Err(ErrorInfo::new(
                            ErrorCode::ITEM_ID_CONFLICT,
                            format!(
                                "log item `{}` already exists with different content; the batch was rejected",
                                item.id
                            ),
                        )
                        .with_pointer(format!("/data/items/{i}/id")));
                    }
                    None => added.push(item),
                }
            }
            if added.is_empty() {
                return Ok(None);
            }
            stamp(&mut added);
            items.extend(added);
            if items.len() > log_cap {
                items.drain(..items.len() - log_cap);
            }
            Ok(Some(ViewData::Log { items }))
        }
        (ViewOp::Append, _) => Err(ErrorInfo::new(
            ErrorCode::INVALID_FRAME,
            "append is only allowed for log views",
        )),
    }
}
