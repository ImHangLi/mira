//! Executing commands on the selected action, one-off run, or view, and navigation.

use crate::clip;
use crate::cmdbar;
use crate::ipc::{Control, Read};
use crate::logs::LogPane;

use super::{App, Cmd, Focus, Intent, Modal, OpenKind, Quit, Tab};

impl App {
    pub(super) fn exec(&mut self, cmd: Cmd) {
        if self.selected_view().is_some() && self.exec_view(cmd) {
            return;
        }
        if self.selected_oneoff().is_some() && self.exec_oneoff(cmd) {
            return;
        }
        match cmd {
            Cmd::Tools => self.return_to_tools(),
            Cmd::Command => {
                self.modal = Modal::Command {
                    text: String::new(),
                    error: None,
                }
            }
            Cmd::Mouse => {
                self.mouse = !self.mouse;
                self.mouse_changed = Some(self.mouse);
                self.info(if self.mouse {
                    "mouse mode on: the wheel scrolls; hold Option (or Shift) for terminal selection"
                } else {
                    "mouse mode off: the terminal's own selection works"
                });
            }
            Cmd::Schedule => {
                if let Some(a) = self.selected_ref()
                    && self.has_schedule(&a)
                {
                    let enabled = !self.schedule_of(&a).is_some_and(|s| s.enabled);
                    self.pending.insert(a.clone(), Intent::Start);
                    let _ = self.io.control.send(Control::Schedule {
                        action_ref: a,
                        enabled,
                    });
                }
            }
            Cmd::Refresh => {
                self.info("refreshing...");
                cmdbar::run(
                    self.root.clone(),
                    vec!["reload".into()],
                    Some("Refreshed: tools, runs, and views are up to date.".into()),
                    self.io.events.clone(),
                );
                let _ = self.io.read.send(Read::Catalog);
                let _ = self.io.read.send(Read::Status);
                if let Some(v) = self.selected_view().map(|v| v.view_ref.clone()) {
                    self.load_view(&v);
                }
            }
            Cmd::AddPlugin => {
                if !self.missing_defaults().is_empty() {
                    self.modal = Modal::AddPlugin { index: 0 };
                }
            }
            Cmd::Remove => {
                if let Some(item) = self.selected_item() {
                    let p = item.action_ref.plugin.to_string();
                    self.modal = Modal::Confirm {
                        title: "Remove plugin".into(),
                        lines: vec![
                            format!("Remove plugin `{p}` from this project?"),
                            String::new(),
                            "Mira takes it out of .mira/workspace.json and reloads.".into(),
                            "Its files stay in .mira/, so you can add it back.".into(),
                        ],
                        words: vec!["plugin".into(), "remove".into(), p.clone()],
                        done: format!("Removed plugin `{p}`. Its files stay in .mira/."),
                    };
                }
            }
            Cmd::CopyAll => {}
            Cmd::NextTab | Cmd::PrevTab | Cmd::GoTab(_) => {
                let at = self.shown_tab().index();
                let to = match cmd {
                    Cmd::NextTab => (at + 1) % 3,
                    Cmd::PrevTab => (at + 2) % 3,
                    Cmd::GoTab(n) => usize::from(n).min(2),
                    _ => at,
                };
                self.go_tab(Tab::ALL[to]);
            }
            Cmd::History => {
                if let Some(a) = self.selected_ref() {
                    // Cached runs show at once; the host's answer replaces them.
                    let runs = self.recent.get(&a).cloned();
                    let shown = self.panes.get(&a).and_then(|p| p.run_id.clone());
                    let index = runs
                        .as_ref()
                        .and_then(|r| r.iter().position(|x| Some(&x.run_id) == shown.as_ref()))
                        .unwrap_or(0);
                    self.modal = Modal::History {
                        action_ref: a.clone(),
                        runs,
                        error: None,
                        index,
                    };
                    let _ = self.io.read.send(Read::History(a));
                }
            }
            Cmd::Up | Cmd::Down | Cmd::PageUp | Cmd::PageDown | Cmd::Top | Cmd::Bottom => {
                self.navigate(cmd)
            }
            Cmd::Open => {
                let Some(item) = self.selected_item() else {
                    return;
                };
                let a = item.action_ref.clone();
                if self.attach_target().is_some() {
                    self.sync_terminal();
                    self.term.focus();
                    return;
                }
                match self.open_kind(item) {
                    Some(OpenKind::Logs) => self.focus = Focus::Logs,
                    Some(OpenKind::Start) => {
                        if self.term.is_pty(&a) {
                            self.focus_on_start = Some(a.clone());
                        }
                        self.intent(&a, Intent::Start)
                    }
                    Some(OpenKind::NeedsInput | OpenKind::Form) => self.intent(&a, Intent::Start),
                    None => {}
                }
            }
            Cmd::Toggle => {
                if let Some(item) = self.selected_item()
                    && let Some(i) = self.toggle_intent(item)
                {
                    let a = item.action_ref.clone();
                    self.intent(&a, i);
                }
            }
            Cmd::Restart => {
                if let Some(a) = self.selected_ref() {
                    self.intent(&a, Intent::Restart);
                }
            }
            Cmd::Focus => {
                self.focus = match self.focus {
                    Focus::List => Focus::Logs,
                    Focus::Logs => Focus::List,
                };
                self.refresh_branch(true);
            }
            Cmd::Search => {
                self.modal = Modal::Search {
                    logs: self.focus == Focus::Logs,
                    // Cancel restores the previous filter and selection.
                    text: String::new(),
                    prev_filter: self.filter.clone(),
                    prev_selection: self.selected_key(),
                }
            }
            Cmd::Quit => self.quit = Some(Quit::Normal(None)),
            Cmd::Keep => {
                self.keeping = true;
                self.info("keeping the session in the background...");
                let _ = self.io.control.send(Control::Keep);
            }
            Cmd::Left => {
                if let Some(p) = self.selected_pane_mut() {
                    p.hscroll = p.hscroll.saturating_sub(8);
                }
            }
            Cmd::Right => {
                if let Some(p) = self.selected_pane_mut() {
                    p.hscroll += 8;
                }
            }
            Cmd::Wrap => {
                if let Some(p) = self.selected_pane_mut() {
                    p.wrap = !p.wrap;
                    // Keep the anchor record; its row offset no longer applies.
                    if let crate::logs::Viewport::Pinned { top, .. } = p.view {
                        p.view = crate::logs::Viewport::Pinned {
                            top,
                            line_offset: 0,
                        };
                    }
                }
            }
            Cmd::Copy => {
                if let Some((text, n)) = self.selected_pane().and_then(LogPane::selection_text) {
                    clip::copy(text, n, self.io.events.clone());
                }
            }
            Cmd::Select => {
                if let Some(p) = self.selected_pane_mut() {
                    if !p.is_pinned() {
                        let _ = p.move_cursor(0);
                    }
                    p.anchor = p.cursor;
                }
            }
            Cmd::Escape => {
                if self.focus == Focus::Logs {
                    let Some(a) = self.selected_ref() else {
                        return;
                    };
                    if let Some(p) = self.panes.get_mut(&a)
                        && p.anchor.is_some()
                    {
                        p.anchor = None;
                    } else if self.viewing.remove(&a).is_some() {
                        self.sync_pane(&a);
                        self.info(format!("showing the latest run of {a}"));
                    }
                } else {
                    self.filter.clear();
                    let keep = self.selected_key();
                    self.refilter(keep);
                    self.on_select();
                }
            }
            Cmd::NextMatch | Cmd::PrevMatch => {
                if let Some(q) = self.log_query.clone() {
                    self.find(&q, cmd == Cmd::NextMatch);
                }
            }
            Cmd::Help => self.modal = Modal::Help { top: 0, max: 0 },
            Cmd::Attach => {
                if self.attach_target().is_some() {
                    self.sync_terminal();
                    self.term.focus();
                }
            }
            Cmd::OpenWritten => {
                if let Some(r) = self.notice.as_ref().and_then(|n| n.open.clone()) {
                    self.open_view(&r);
                }
            }
            Cmd::Detach | Cmd::Forward => {}
        }
    }

    /// Commands that act on a selected one-off run. Returns false for the ones shared with
    /// actions (log keys, focus, search) and global ones.
    fn exec_oneoff(&mut self, cmd: Cmd) -> bool {
        let Some(i) = self.selected_oneoff_index() else {
            return false;
        };
        match (cmd, self.focus) {
            (Cmd::Open, Focus::List) => self.focus = Focus::Logs,
            (Cmd::Open, Focus::Logs) => {}
            (Cmd::Toggle, _) => {
                if self.oneoff_stoppable() {
                    let o = &mut self.oneoffs[i];
                    o.stopping = true;
                    let _ = self.io.control.send(Control::StopRun(o.run_id.clone()));
                }
            }
            (Cmd::NextTab | Cmd::PrevTab, _) => self.oneoff_details = !self.oneoff_details,
            (Cmd::GoTab(n), _) => {
                if n < 2 {
                    self.oneoff_details = n == 1;
                }
            }
            (Cmd::Escape, Focus::Logs) => match self.oneoffs[i].pane.as_mut() {
                Some(p) if p.anchor.is_some() => p.anchor = None,
                _ => self.focus = Focus::List,
            },
            _ => return false,
        }
        true
    }

    /// Commands that act on a selected view. Returns false for global ones.
    fn exec_view(&mut self, cmd: Cmd) -> bool {
        let Some(v) = self.selected_view() else {
            return false;
        };
        let r = v.view_ref.clone();
        let focus = self.focus;
        match (cmd, focus) {
            (Cmd::Open, Focus::List) => self.focus = Focus::Logs,
            (Cmd::Open, Focus::Logs) => {
                let choices = self
                    .view_panes
                    .get(&r)
                    .map(|p| p.row_actions.clone())
                    .unwrap_or_default();
                // Always ask first, even for one action: a row action can stop a process
                // or delete a file, and Enter is easy to press by accident.
                if !choices.is_empty() {
                    self.modal = Modal::RowAction {
                        view_ref: r,
                        choices,
                        index: 0,
                    }
                }
            }
            // Esc leaves the view focus, like Tab.
            (Cmd::Escape, Focus::Logs) => self.focus = Focus::List,
            (Cmd::Restart, _) => {
                self.load_view(&r);
                self.info(format!("reading {r} again"));
            }
            (
                Cmd::Up | Cmd::Down | Cmd::PageUp | Cmd::PageDown | Cmd::Top | Cmd::Bottom,
                Focus::Logs,
            ) => {
                if let Some(p) = self.view_panes.get_mut(&r) {
                    match cmd {
                        Cmd::Up => p.move_by(-1),
                        Cmd::Down => p.move_by(1),
                        Cmd::PageUp => p.page(false),
                        Cmd::PageDown => p.page(true),
                        Cmd::Top => p.home(false),
                        _ => p.home(true),
                    }
                }
            }
            (Cmd::Left | Cmd::Right, Focus::Logs) => {
                if let Some(p) = self.view_panes.get_mut(&r) {
                    p.left_right(cmd == Cmd::Right);
                }
            }
            (Cmd::Wrap, Focus::Logs) => {
                if let Some(p) = self.view_panes.get_mut(&r) {
                    p.toggle_wrap();
                }
            }
            (Cmd::Copy | Cmd::CopyAll, Focus::Logs) => {
                let got = self.view_panes.get(&r).and_then(|p| {
                    if cmd == Cmd::Copy {
                        p.copy_value()
                    } else {
                        p.copy_whole()
                    }
                });
                if let Some((text, what)) = got {
                    let n = text.lines().count().max(1);
                    self.info(format!("copying the {what}..."));
                    clip::copy(text, n, self.io.events.clone());
                }
            }
            _ => return false,
        }
        true
    }
}

impl App {
    fn navigate(&mut self, cmd: Cmd) {
        match self.focus {
            Focus::List => {
                let n = self.visible.len();
                if n == 0 {
                    return;
                }
                let page = 10;
                self.selected = match cmd {
                    Cmd::Up => self.selected.saturating_sub(1),
                    Cmd::Down => (self.selected + 1).min(n - 1),
                    Cmd::PageUp => self.selected.saturating_sub(page),
                    Cmd::PageDown => (self.selected + page).min(n - 1),
                    Cmd::Top => 0,
                    _ => n - 1,
                };
                self.on_select();
            }
            Focus::Logs if self.tab == Tab::Output && self.selected_oneoff().is_none() => {
                self.output_top = match cmd {
                    Cmd::Up => self.output_top.saturating_sub(1),
                    Cmd::Down => self.output_top + 1,
                    Cmd::PageUp => self.output_top.saturating_sub(10),
                    Cmd::PageDown => self.output_top + 10,
                    Cmd::Top => 0,
                    // Drawing clamps this to the last page.
                    _ => usize::MAX / 2,
                };
            }
            Focus::Logs => {
                let Some(p) = self.selected_pane_mut() else {
                    return;
                };
                let page = p.height.saturating_sub(1).max(1) as isize;
                let at_top = match cmd {
                    Cmd::Up => p.move_cursor(-1),
                    Cmd::Down => p.move_cursor(1),
                    Cmd::PageUp => p.page(-page),
                    Cmd::PageDown => p.page(page),
                    Cmd::Top => {
                        p.top();
                        true
                    }
                    _ => {
                        p.follow();
                        false
                    }
                };
                // Reaching the first loaded record pages older history in, if the host has it.
                if at_top
                    && !p.loading_older
                    && let (Some(cursor), Some(run_id)) = (p.older_cursor.clone(), p.run_id.clone())
                {
                    p.loading_older = true;
                    let _ = self.io.read.send(Read::Older { run_id, cursor });
                }
            }
        }
    }
}
