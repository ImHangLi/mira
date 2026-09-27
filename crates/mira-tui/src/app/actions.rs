//! Starting, stopping, and restarting actions, row actions, and the run history.

use mira_protocol::ids::{ActionId, ActionRef, RunId, ViewRef};
use mira_protocol::manifest::{ActionMode, JsonObject};
use mira_protocol::run::{Lifecycle, RunRecord};

use crate::form::{self, Form};
use crate::ipc::{Control, Read};

use super::{App, Focus, Inputs, Intent, Item, Modal, OpenKind, RowRun, Tab};

impl App {
    fn schema(&self, a: &ActionRef) -> Option<&JsonObject> {
        match self.inputs.get(a) {
            Some((_, Inputs::Form(s))) => Some(s),
            _ => None,
        }
    }

    fn required(&self, a: &ActionRef) -> bool {
        self.schema(a)
            .is_some_and(|s| !form::required_names(s).is_empty())
    }

    fn can_act(&self, item: &Item) -> bool {
        item.enabled && self.control_lost.is_none() && !self.pending.contains_key(&item.action_ref)
    }

    pub(super) fn open_kind(&self, item: &Item) -> Option<OpenKind> {
        if self.active.contains_key(&item.action_ref) || self.pending.contains_key(&item.action_ref)
        {
            return Some(OpenKind::Logs);
        }
        if !self.can_act(item) {
            return None;
        }
        if self.required(&item.action_ref) {
            Some(OpenKind::NeedsInput)
        } else if self.schema(&item.action_ref).is_some() {
            Some(OpenKind::Form)
        } else {
            Some(OpenKind::Start)
        }
    }

    /// The `s` action: start/run when idle, stop when active; `None` while stopping.
    pub(super) fn toggle_intent(&self, item: &Item) -> Option<Intent> {
        if !self.can_act(item) {
            return None;
        }
        match self.active.get(&item.action_ref) {
            Some(r) if matches!(r.lifecycle, Lifecycle::Stopping { .. }) => None,
            Some(_) => Some(Intent::Stop),
            None => Some(Intent::Start),
        }
    }

    pub(super) fn restart_ok(&self, item: &Item) -> bool {
        if !self.can_act(item) {
            return false;
        }
        match self.active.get(&item.action_ref) {
            Some(r) => !matches!(r.lifecycle, Lifecycle::Stopping { .. }),
            None => item.mode == ActionMode::Task && self.last.contains_key(&item.action_ref),
        }
    }

    pub(super) fn intent(&mut self, a: &ActionRef, intent: Intent) {
        if intent != Intent::Stop {
            match self.inputs.get(a) {
                None | Some((_, Inputs::Loading)) => {
                    self.queued = Some((a.clone(), intent));
                    if !self.inputs.contains_key(a) {
                        let hash = self.item(a).map(|i| i.definition_hash.clone());
                        if let Some(h) = hash {
                            self.inputs.insert(a.clone(), (h, Inputs::Loading));
                        }
                        let _ = self.io.read.send(Read::Describe(a.clone()));
                    }
                    self.info(format!("reading {a} inputs..."));
                    return;
                }
                Some((_, Inputs::Form(schema))) => {
                    let form = Form::new(a.clone(), intent, schema, self.last_inputs.get(a));
                    self.modal = Modal::Form(Box::new(form));
                    return;
                }
                Some((_, Inputs::Free | Inputs::Unknown(_))) => {}
            }
        }
        self.act(a, intent, JsonObject::new(), false);
    }

    /// `from_form`: the input came from a submitted form, which waits for the answer.
    pub(super) fn act(
        &mut self,
        a: &ActionRef,
        intent: Intent,
        input: JsonObject,
        from_form: bool,
    ) {
        let run = self.active.get(a).map(|r| r.run_id.clone());
        match (intent, run) {
            (Intent::Stop, Some(run_id)) => self.stop(a, run_id),
            (Intent::Restart, Some(run_id)) => {
                self.restart_after
                    .insert(a.clone(), (run_id.clone(), input));
                self.stop(a, run_id);
            }
            (Intent::Stop, None) => {}
            (Intent::Start, Some(_)) if !from_form => self.focus = Focus::Logs,
            (Intent::Start | Intent::Restart, _) => self.invoke(a.clone(), input),
        }
    }

    fn stop(&mut self, a: &ActionRef, run_id: RunId) {
        self.pending.insert(a.clone(), Intent::Stop);
        let _ = self.io.control.send(Control::Stop {
            action_ref: a.clone(),
            run_id,
        });
    }

    pub(super) fn invoke(&mut self, a: ActionRef, input: JsonObject) {
        self.pending.insert(a.clone(), Intent::Start);
        let _ = self.io.control.send(Control::Invoke {
            action_ref: a,
            input,
        });
    }

    pub(super) fn run_row_action(&mut self, view_ref: ViewRef, action: ActionId) {
        let Some(p) = self.view_panes.get(&view_ref) else {
            return;
        };
        let (Some(row), Some(expected)) = (p.selected_row_id(), p.sel_rev) else {
            self.error("select a row first");
            return;
        };
        self.info(format!("running {action} for this row…"));
        self.row_run = Some(RowRun {
            view_ref: view_ref.clone(),
            action: action.clone(),
            run_id: None,
            wrote: Vec::new(),
        });
        let _ = self.io.control.send(Control::ViewAction {
            view_ref,
            action,
            row,
            expected,
        });
    }
}

impl App {
    /// The action has at least one run the history list can show.
    pub(super) fn has_history(&self, a: &ActionRef) -> bool {
        self.active.contains_key(a) || self.last.contains_key(a) || self.viewing.contains_key(a)
    }

    /// Shows `rec`'s logs in the action's pane. The newest run clears the historical choice,
    /// so the pane follows new runs again.
    pub(super) fn open_history_run(&mut self, a: ActionRef, rec: RunRecord) {
        let latest = self
            .active
            .get(&a)
            .map(|r| &r.run_id)
            .or_else(|| self.last.get(&a).map(|l| &l.run_id));
        if latest == Some(&rec.run_id) {
            self.viewing.remove(&a);
        } else {
            self.viewing.insert(a.clone(), rec);
        }
        self.sync_pane(&a);
        self.focus = Focus::Logs;
        self.tab = Tab::Logs;
    }

    /// The selected item's active PTY run, if it can be attached.
    pub(super) fn attach_target(&self) -> Option<(ActionRef, RunId)> {
        let item = self.selected_item()?;
        let run = self.active.get(&item.action_ref)?;
        (self.term.is_pty(&item.action_ref) && run.lifecycle.is_active())
            .then(|| (item.action_ref.clone(), run.run_id.clone()))
    }
}
