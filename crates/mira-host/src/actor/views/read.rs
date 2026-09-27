//! `view.read`: freshness, paging, and payload references for oversized content.

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::*;
use mira_protocol::ipc::*;
use mira_protocol::manifest::{Persistence, ViewDefinition};
use mira_protocol::reply::ReplyMeta;
use mira_protocol::view::*;

use crate::actor::cursor::{self, Kind, Pos};
use crate::actor::{Actor, Handled, MAX_LIMIT, Responder, budget};

use super::ViewEntry;

const DEFAULT_PAGE: usize = 100;

/// The request side of one view page.
pub(in crate::actor) struct Page<'a> {
    pub(in crate::actor) source: &'a str,
    pub(in crate::actor) revision: ViewRevision,
    pub(in crate::actor) offset: usize,
    pub(in crate::actor) limit: usize,
    pub(in crate::actor) budget: usize,
}

impl Actor {
    fn freshness(&self, e: &ViewEntry, def: &ViewDefinition) -> (Freshness, String) {
        if e.definition_hash != def.definition_hash {
            return (
                Freshness::Stale,
                "the view definition changed after this data was recorded".into(),
            );
        }
        if let Some(r) = &e.stale {
            return (Freshness::Stale, r.clone());
        }
        match &e.source_run_id {
            Some(id)
                if self
                    .runs
                    .get(id)
                    .is_some_and(|r| r.record.lifecycle.is_active()) =>
            {
                (
                    Freshness::Current,
                    format!("the producing run {id} is still running"),
                )
            }
            Some(id) => (
                Freshness::Historical,
                format!("recorded by run {id}, which has ended"),
            ),
            None => (
                Freshness::Historical,
                format!(
                    "published explicitly (source {})",
                    serde_json::to_value(e.source_kind)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default()
                ),
            ),
        }
    }

    pub(in crate::actor) fn view_read(&mut self, p: ViewReadParams, r: Responder) {
        let derived = self.accepted().ok().and_then(|set| {
            set.view(&p.view_ref)
                .filter(|(_, d)| d.source.is_some())
                .map(|(_, d)| d.clone())
        });
        let read = match derived {
            Some(def) => self.derived_read(p, &def),
            None => self.view_read_inner(p),
        };
        let reply = match read {
            Ok(h) => h,
            Err(e) => self.fail(e),
        };
        r.send(reply);
    }

    fn view_read_inner(&self, p: ViewReadParams) -> Result<Handled, ErrorInfo> {
        let set = self.accepted()?;
        let (_, def) = set
            .view(&p.view_ref)
            .ok_or_else(|| ErrorInfo::item_not_found("view", &p.view_ref))?;
        let limit = p
            .limit
            .map_or(DEFAULT_PAGE, |l| (l as usize).clamp(1, MAX_LIMIT));
        let budget = budget::budget(p.max_bytes);
        let Some(e) = self.views.entries.get(&p.view_ref) else {
            let (durability, reason) = match def.persistence {
                Persistence::Session if self.session.is_none() => (
                    Durability::SessionOnly,
                    "session views exist only while a session is active",
                ),
                Persistence::Session => (Durability::SessionOnly, "no data in this session yet"),
                Persistence::Last => (Durability::Unavailable, "no data has been recorded"),
            };
            let cleaned = self.views.cleaned.get(&p.view_ref).map(|(rev, at)| {
                format!(
                    "cleaned up by retention at {} (last revision {rev}); this is not an empty result",
                    mira_protocol::time::Timestamp::from_unix_ms(*at)
                )
            });
            return Ok(self.ok(
                ViewSnapshot {
                    view_ref: p.view_ref,
                    view_revision: None,
                    kind: def.kind,
                    recorded_at: None,
                    source_run_id: None,
                    source_kind: None,
                    definition_hash: def.definition_hash.clone(),
                    freshness: Freshness::Historical,
                    freshness_reason: Some(cleaned.unwrap_or_else(|| reason.into())),
                    durability,
                    data: None,
                },
                ReplyMeta::default(),
            ));
        };
        let source = p.view_ref.to_string();
        let offset = match p.cursor.as_deref() {
            None => 0,
            Some(c) => {
                let c = cursor::decode(c, Kind::View, &source, "")?;
                if c.revision != Some(e.revision.get()) {
                    return Err(ErrorInfo::new(
                        ErrorCode::VIEW_CHANGED,
                        format!(
                            "the view changed to revision {}; restart from the first page",
                            e.revision
                        ),
                    )
                    .with_next_action(
                        &["mira", "view", &source],
                        "Read the current revision from the start.",
                    ));
                }
                c.pos.o.unwrap_or(0) as usize
            }
        };
        let (freshness, reason) = self.freshness(e, def);
        let snap = |data: ViewBody| ViewSnapshot {
            view_ref: p.view_ref.clone(),
            view_revision: Some(e.revision),
            kind: e.data.kind(),
            recorded_at: Some(e.recorded_at),
            source_run_id: e.source_run_id.clone(),
            source_kind: Some(e.source_kind),
            definition_hash: e.definition_hash.clone(),
            freshness,
            freshness_reason: Some(reason.clone()),
            durability: e.durability(self.storage.is_ok()),
            data: Some(data),
        };
        let page = Page {
            source: &source,
            revision: e.revision,
            offset,
            limit,
            budget,
        };
        Ok(match &e.data {
            ViewData::Table { columns, rows } => self.view_page(
                &page,
                rows,
                "row",
                |rows| ViewData::Table {
                    columns: columns.clone(),
                    rows,
                },
                &snap,
            ),
            ViewData::Log { items } => self.view_page(
                &page,
                items,
                "log item",
                |items| ViewData::Log { items },
                &snap,
            ),
            other => {
                let reply = self.ok(snap(ViewBody::Inline(other.clone())), ReplyMeta::default());
                match reply {
                    Ok(v) if budget::json_len(&v) <= budget => Ok(v),
                    Ok(_) => {
                        let bytes = serde_json::to_vec(other).unwrap_or_default();
                        let size = bytes.len();
                        let payload = self
                            .payloads
                            .hold(Some(format!("view:{source}@{}", e.revision)), bytes);
                        let summary = format!(
                            "{} view data is {size} bytes, above the {budget}-byte reply budget; \
                             read it with `mira payload read <meta.payload.token>`",
                            format!("{:?}", other.kind()).to_lowercase()
                        );
                        self.ok(
                            snap(ViewBody::Reference(ReferenceData {
                                representation: ReferenceTag::Reference,
                                summary,
                            })),
                            ReplyMeta {
                                truncated: true,
                                payload: Some(payload),
                                ..ReplyMeta::default()
                            },
                        )
                    }
                    Err(err) => Err(err),
                }
            }
        })
    }

    /// One page of table rows or log items within the budget. A first item that alone is
    /// too large is returned by payload reference, and the cursor moves past it.
    pub(in crate::actor) fn view_page<T: Clone + serde::Serialize>(
        &self,
        page: &Page<'_>,
        all: &[T],
        label: &str,
        wrap: impl Fn(Vec<T>) -> ViewData,
        snap: &dyn Fn(ViewBody) -> ViewSnapshot,
    ) -> Handled {
        let start = page.offset.min(all.len());
        let candidates = &all[start..(start + page.limit).min(all.len())];
        let next = |pos: usize| {
            (pos < all.len()).then(|| {
                cursor::encode(
                    Kind::View,
                    page.source,
                    "",
                    Pos {
                        o: Some(pos as u64),
                        ..Pos::default()
                    },
                    Some(page.revision.get()),
                )
            })
        };
        let sizes: Vec<usize> = candidates.iter().map(budget::json_len).collect();
        let fitted = budget::fit(&sizes, page.budget, |n| {
            let next_cursor = next(start + n);
            self.ok(
                snap(ViewBody::Inline(wrap(candidates[..n].to_vec()))),
                ReplyMeta {
                    truncated: next_cursor.is_some(),
                    next_cursor,
                    ..ReplyMeta::default()
                },
            )
        })?;
        match fitted {
            budget::Fit::Items { reply, .. } => Ok(reply),
            budget::Fit::FirstTooLarge => {
                let bytes = serde_json::to_vec(&candidates[0]).unwrap_or_default();
                let size = bytes.len();
                let payload = self.payloads.hold(
                    Some(format!("view:{}@{}#{start}", page.source, page.revision)),
                    bytes,
                );
                let summary = format!(
                    "{label} {start} is {size} bytes, above the {}-byte reply budget; read it with \
                     `mira payload read <meta.payload.token>` and continue with next_cursor",
                    page.budget
                );
                self.ok(
                    snap(ViewBody::Reference(ReferenceData {
                        representation: ReferenceTag::Reference,
                        summary,
                    })),
                    ReplyMeta {
                        truncated: true,
                        next_cursor: next(start + 1),
                        not_modified: false,
                        payload: Some(payload),
                    },
                )
            }
        }
    }
}
