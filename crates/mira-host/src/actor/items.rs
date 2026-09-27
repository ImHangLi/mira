//! `catalog.list` and `item.describe`: ranked, paged catalog reads and item details.

use mira_protocol::catalog;
use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ipc::*;
use mira_protocol::manifest::{ActionMode, Runner};
use mira_protocol::reply::ReplyMeta;
use serde_json::Value;

use super::{Actor, Handled, MAX_LIMIT, budget, cursor};

const DEFAULT_CATALOG_LIMIT: usize = 30;

impl Actor {
    pub(super) fn catalog(&self, p: CatalogListParams) -> Handled {
        let set = match self.accepted() {
            Ok(s) => s,
            Err(e) => return self.fail(e),
        };
        let words = catalog::query_words(p.query.as_deref().unwrap_or_default());
        let source = self.paths.id.to_string();
        let filter = cursor::filter_hash(&serde_json::json!({ "query": words }));
        let revision = self.catalog_revision;
        // A cached catalog is valid only for the same workspace, query, and revision.
        let same_workspace = p.if_workspace.as_ref().is_none_or(|w| w == &self.paths.id);
        if p.cursor.is_none() && same_workspace && p.if_revision == Some(revision) {
            let meta = ReplyMeta {
                not_modified: true,
                ..ReplyMeta::default()
            };
            return self.ok(CatalogList { items: vec![] }, meta);
        }
        let offset = match p
            .cursor
            .as_deref()
            .map(|c| cursor::decode(c, cursor::Kind::Catalog, &source, &filter))
            .transpose()
        {
            Ok(None) => 0,
            Ok(Some(c)) if c.revision != Some(revision.get()) => {
                let mut argv = vec!["mira", "catalog"];
                let query = p.query.as_deref().unwrap_or_default();
                if !words.is_empty() {
                    argv.extend(["--search", query]);
                }
                return self.fail(
                    ErrorInfo::new(
                        ErrorCode::REVISION_CONFLICT,
                        format!(
                            "the catalog changed to revision {revision} after this cursor was issued; \
                             earlier pages may be out of date"
                        ),
                    )
                    .with_next_action(&argv, "Read the catalog again from the first page."),
                );
            }
            Ok(Some(c)) => c.pos.o.unwrap_or(0) as usize,
            Err(e) => return self.fail(e),
        };
        let limit = p
            .limit
            .map_or(DEFAULT_CATALOG_LIMIT, |l| (l as usize).clamp(1, MAX_LIMIT));
        let budget = budget::budget(p.max_bytes);
        let items = search(set.catalog(), &words);
        let start = offset.min(items.len());
        let candidates = &items[start..(start + limit).min(items.len())];
        let next = |taken: usize| {
            (start + taken < items.len()).then(|| {
                cursor::encode(
                    cursor::Kind::Catalog,
                    &source,
                    &filter,
                    cursor::Pos {
                        o: Some((start + taken) as u64),
                        ..cursor::Pos::default()
                    },
                    Some(revision.get()),
                )
            })
        };
        let sizes: Vec<usize> = candidates.iter().map(budget::json_len).collect();
        let fitted = budget::fit(&sizes, budget, |n| {
            let next_cursor = next(n);
            self.ok(
                CatalogList {
                    items: candidates[..n].to_vec(),
                },
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
                let first = &candidates[0];
                let payload = self.payloads.hold(
                    Some(format!("catalog:{revision}:{}", first.item_ref)),
                    serde_json::to_vec(first).unwrap_or_default(),
                );
                self.ok(
                    CatalogList { items: vec![] },
                    ReplyMeta {
                        truncated: true,
                        next_cursor: next(1),
                        not_modified: false,
                        payload: Some(payload),
                    },
                )
            }
        }
    }

    pub(super) fn describe(&self, p: ItemDescribeParams) -> Handled {
        let set = match self.accepted() {
            Ok(s) => s,
            Err(e) => return self.fail(e),
        };
        let not_found = || ErrorInfo::item_not_found("catalog item", &p.item_ref);
        let Some(lp) = set.plugin(&p.item_ref.plugin) else {
            return self.fail(not_found());
        };
        let Some(item) = set.catalog().into_iter().find(|i| i.item_ref == p.item_ref) else {
            return self.fail(not_found());
        };
        let plugin = &lp.plugin;
        let summary = PluginSummary {
            id: plugin.id.clone(),
            name: plugin.name.clone(),
            description: plugin.description.clone(),
            enabled: plugin.enabled,
            docs: plugin.docs.clone(),
        };
        let (action, view, hint) = if let Some(a) = plugin.action(p.item_ref.item.as_str()) {
            let desc = ActionDescription {
                mode: a.mode,
                runner: match a.run {
                    Runner::Command { .. } => "command".into(),
                    Runner::Plugin => "plugin".into(),
                },
                terminal: a.terminal,
                show: a.show,
                timeout: a.timeout.to_wire(),
                cwd: a.cwd.clone(),
                env_names: a.env.keys().cloned().collect(),
                env_files: a.env_files.clone(),
                effects: a.effects.clone(),
                has_schedule: a.schedule.is_some(),
                write_only_fields: a.input_schema.0.write_only_fields(),
                input_schema: p.include_schema.then(|| a.input_schema.0.as_map().clone()),
                output_schema: if p.include_schema {
                    a.output_schema.as_ref().map(|s| s.0.as_map().clone())
                } else {
                    None
                },
            };
            let verb = match a.mode {
                ActionMode::Task => "run",
                ActionMode::Process => "start",
            };
            let mut hint = vec!["mira".to_owned(), verb.to_owned(), p.item_ref.to_string()];
            if a.input_schema
                .0
                .as_map()
                .get("properties")
                .and_then(Value::as_object)
                .is_some_and(|m| !m.is_empty())
            {
                hint.extend(["--input".to_owned(), "input.json".to_owned()]);
            }
            (Some(desc), None, hint)
        } else if let Some(v) = plugin.view(p.item_ref.item.as_str()) {
            let desc = ViewDescription {
                view_kind: v.kind,
                persistence: v.persistence,
                row_actions: v.row_actions.iter().map(|r| r.action.clone()).collect(),
                source: v.source.as_ref().map(Into::into),
            };
            (
                None,
                Some(desc),
                vec!["mira".to_owned(), "view".to_owned(), p.item_ref.to_string()],
            )
        } else {
            return self.fail(not_found());
        };
        let mut desc = ItemDescription {
            item,
            plugin: summary,
            action,
            view,
            invoke_hint: hint,
        };
        // Schemas are compact by default; requested schemas above the budget become a payload.
        let mut meta = ReplyMeta::default();
        let budget = budget::budget(p.max_bytes);
        let reply = self.ok(&desc, ReplyMeta::default())?;
        if budget::json_len(&reply) <= budget {
            return Ok(reply);
        }
        if let Some(a) = desc.action.as_mut()
            && (a.input_schema.is_some() || a.output_schema.is_some())
        {
            let schemas = serde_json::json!({
                "input_schema": a.input_schema.take(),
                "output_schema": a.output_schema.take(),
            });
            meta.truncated = true;
            meta.payload = Some(self.payloads.hold(
                Some(format!(
                    "describe:{}:{}",
                    desc.item.item_ref, desc.item.definition_hash
                )),
                serde_json::to_vec(&schemas).unwrap_or_default(),
            ));
        }
        self.ok(desc, meta)
    }
}

/// Filters and ranks catalog items for the lowercase query `words`. An empty query keeps
/// all items; a stable sort keeps catalog order among equal ranks.
fn search(items: Vec<CatalogItem>, words: &[String]) -> Vec<CatalogItem> {
    let mut ranked: Vec<(catalog::Rank, CatalogItem)> = items
        .into_iter()
        .filter_map(|item| {
            let item_ref = item.item_ref.to_string();
            let entry = catalog::Entry {
                item_ref: &item_ref,
                id: item.item_ref.item.as_str(),
                title: &item.title,
                tags: &item.tags,
                description: &item.description,
            };
            catalog::rank(words, &entry).map(|r| (r, item))
        })
        .collect();
    ranked.sort_by_key(|(r, _)| *r);
    ranked.into_iter().map(|(_, item)| item).collect()
}
