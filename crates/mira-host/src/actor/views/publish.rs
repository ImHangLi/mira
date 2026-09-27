//! `view.publish` and table row actions (`view.action`).

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::*;
use mira_protocol::ipc::*;
use mira_protocol::manifest::{Persistence, wire_from_value};
use mira_protocol::mpp::{PluginEvent, PluginFrame};
use mira_protocol::view::*;
use serde_json::{Value, json};

use crate::actor::{Actor, Responder};
use crate::storage::{KeyClaim, KeyScope};

use super::{PublishReq, ViewUpdate};

impl Actor {
    pub(in crate::actor) fn view_publish(&mut self, p: ViewPublishParams, r: Responder) {
        match self.prepare_publish(&p) {
            Ok((op, data, claim)) => {
                self.views.queue.push_back(ViewUpdate {
                    view_ref: p.view_ref,
                    op,
                    data,
                    source_run_id: None,
                    source_kind: p.source_kind,
                    publish: Some(PublishReq {
                        responder: r,
                        expected: p.expected_view_revision,
                        claim,
                    }),
                });
                self.process_views();
            }
            Err(e) => r.send(self.fail(e)),
        }
    }

    fn prepare_publish(
        &self,
        p: &ViewPublishParams,
    ) -> Result<(ViewOp, ViewData, Option<KeyClaim>), ErrorInfo> {
        let set = self.accepted()?;
        let (_, def) = set
            .view(&p.view_ref)
            .ok_or_else(|| ErrorInfo::item_not_found("view", &p.view_ref))?;
        if let Some(src) = &def.source {
            return Err(ErrorInfo::new(
                ErrorCode::INVALID_ARGUMENT,
                format!(
                    "`{}` is derived from the logs of `{}`; it cannot be published to",
                    p.view_ref, src.logs
                ),
            )
            .with_next_action(
                &["mira", "view", &p.view_ref.to_string()],
                "Read the derived view instead.",
            ));
        }
        let frame_value = Value::Object(p.frame.clone());
        let event = wire_from_value::<PluginFrame>(frame_value.clone())
            .and_then(PluginFrame::validate)
            .map_err(|i| i.to_error_info())?;
        let PluginEvent::View { view_id, op, data } = event else {
            return Err(ErrorInfo::new(
                ErrorCode::INVALID_ARGUMENT,
                "publish input must be one MPP `view` frame",
            ));
        };
        if view_id != p.view_ref.view {
            return Err(ErrorInfo::new(
                ErrorCode::INVALID_ARGUMENT,
                format!(
                    "frame.view_id `{view_id}` does not match the local ID of `{}`",
                    p.view_ref
                ),
            ));
        }
        if data.kind() != def.kind {
            return Err(ErrorInfo::new(
                ErrorCode::SCHEMA_INVALID,
                format!(
                    "view `{}` is a {:?} view; the frame carries {:?} data",
                    p.view_ref,
                    def.kind,
                    data.kind()
                )
                .to_lowercase(),
            ));
        }
        if def.persistence == Persistence::Session && self.session.is_none() {
            return Err(ErrorInfo::new(
                ErrorCode::SESSION_REQUIRED,
                format!(
                    "`{}` is a session view; publishing it needs an active session",
                    p.view_ref
                ),
            )
            .with_next_action(
                &["mira", "up", "--background"],
                "Start a session explicitly, or publish to a `last` view.",
            ));
        }
        let storage = self.storage.as_ref().map_err(|e| e.to_error_info())?;
        let claim = p.request_key.clone().map(|key| KeyClaim {
            scope: KeyScope::Publish(p.view_ref.clone()),
            key,
            fingerprint: storage.fingerprint(&json!({
                "frame": frame_value,
                "definition_hash": def.definition_hash,
                "expected_view_revision": p.expected_view_revision,
            })),
        });
        Ok((op, data, claim))
    }

    /// A table row action: binds input from the row at `expected_view_revision`, then
    /// invokes the action on the same path as `action.invoke`. Any drift is VIEW_CHANGED.
    pub(in crate::actor) fn view_action(
        &mut self,
        client: &ClientId,
        p: ViewActionParams,
        r: Responder,
    ) {
        let prepared = (|| {
            let set = self.accepted()?;
            let (_, def) = set
                .view(&p.view_ref)
                .ok_or_else(|| ErrorInfo::item_not_found("view", &p.view_ref))?;
            let ra = def
                .row_actions
                .iter()
                .find(|ra| ra.action == p.action)
                .ok_or_else(|| {
                    ErrorInfo::new(
                        ErrorCode::NOT_FOUND,
                        format!("view `{}` has no row action `{}`", p.view_ref, p.action),
                    )
                })?;
            let changed = |msg: String| {
                ErrorInfo::new(ErrorCode::VIEW_CHANGED, msg).with_next_action(
                    &["mira", "view", &p.view_ref.to_string()],
                    "Read the current view, then retry with its view_revision.",
                )
            };
            let e = self
                .views
                .entries
                .get(&p.view_ref)
                .ok_or_else(|| changed("the view has no data".into()))?;
            if e.revision != p.expected_view_revision {
                return Err(changed(format!(
                    "the view is at revision {}, not {}; nothing was run",
                    e.revision, p.expected_view_revision
                )));
            }
            if e.definition_hash != def.definition_hash {
                return Err(changed(
                    "the view or its bound action definition changed; nothing was run".into(),
                ));
            }
            let ViewData::Table { rows, .. } = &e.data else {
                return Err(changed("the view holds no table".into()));
            };
            let row = rows
                .iter()
                .find(|row| row.id == p.row)
                .ok_or_else(|| changed(format!("row `{}` no longer exists", p.row)))?;
            let mut input = serde_json::Map::new();
            for (param, col) in &ra.bindings {
                match row.values.get(col) {
                    Some(Value::Null) | None => {}
                    Some(v) => {
                        input.insert(param.clone(), v.clone());
                    }
                }
            }
            Ok(ActionInvokeParams {
                action_ref: ActionRef::new(p.view_ref.plugin.clone(), ra.action.clone()),
                input,
                client_env: p.client_env.clone(),
                request_key: None,
                foreground: true,
            })
        })();
        match prepared {
            Ok(invoke) => self.invoke_internal(client, invoke, Some(r)),
            Err(e) => r.send(self.fail(e)),
        }
    }
}
