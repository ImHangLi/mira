//! The key router: modal keys first, then normal keys mapped to bound commands.

use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::layout::Position;

use crate::clip;
use crate::cmdbar;
use crate::form::{Form, Outcome};

use super::{App, Cmd, Focus, Hit, Modal, Tab};

impl App {
    pub fn key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let modal_page = self
            .modal_rect
            .map_or(1, |r| r.height.saturating_sub(3).max(1) as usize);
        // Back to the tools from anywhere. Ctrl-T ("tools") is easy to type; function keys
        // are avoided because many people use them for dictation.
        if (ctrl && k.code == KeyCode::Char('t'))
            || (k.code == KeyCode::Esc && k.modifiers.contains(KeyModifiers::SHIFT))
            || (ctrl && matches!(k.code, KeyCode::Char(']') | KeyCode::Char('5')))
        {
            self.return_to_tools();
            return;
        }
        if self.term.is_focused() {
            return self.term.key(k);
        }
        let repeat = k.kind == KeyEventKind::Repeat;
        match &self.modal {
            Modal::Search { logs, .. } => {
                let logs = *logs;
                match k.code {
                    KeyCode::Esc => self.cancel_search(),
                    KeyCode::Char('c') if ctrl => self.cancel_search(),
                    KeyCode::Enter => self.apply_search(),
                    KeyCode::Backspace | KeyCode::Char(_) => {
                        let Modal::Search { text, .. } = &mut self.modal else {
                            return;
                        };
                        match k.code {
                            KeyCode::Backspace => {
                                text.pop();
                            }
                            KeyCode::Char(c) if !ctrl => text.push(c),
                            _ => return,
                        }
                        let text = text.clone();
                        if !logs {
                            // The best match is selected as the filter changes.
                            self.filter = text;
                            self.refilter(None);
                            self.on_select();
                        }
                    }
                    _ => {}
                }
                return;
            }
            Modal::Form(_) => {
                if repeat && k.code == KeyCode::Enter {
                    return;
                }
                let Modal::Form(f) = &mut self.modal else {
                    return;
                };
                match f.key(k) {
                    Outcome::Stay => {}
                    Outcome::Cancel => self.modal = Modal::None,
                    Outcome::Submit(input) => {
                        let a = f.action_ref.clone();
                        let intent = f.intent;
                        let keep = Form::remembered(&input, &f.fields);
                        self.last_inputs.insert(a.clone(), keep);
                        self.act(&a, intent, input, true);
                    }
                }
                return;
            }
            Modal::Command { .. } => {
                let Modal::Command { text, error } = &mut self.modal else {
                    return;
                };
                match k.code {
                    KeyCode::Esc => self.modal = Modal::None,
                    KeyCode::Char('c') if ctrl => self.modal = Modal::None,
                    KeyCode::Char('u') if ctrl => text.clear(),
                    KeyCode::Backspace => {
                        text.pop();
                        *error = None;
                    }
                    KeyCode::Enter if !repeat => match cmdbar::split(text) {
                        Ok(words) => {
                            let t = format!("mira {}", words.join(" "));
                            *error = Some(format!("running {t}..."));
                            cmdbar::run(self.root.clone(), words, None, self.io.events.clone());
                        }
                        Err(e) => *error = Some(e),
                    },
                    KeyCode::Char(c) if !ctrl => {
                        text.push(c);
                        *error = None;
                    }
                    _ => {}
                }
                return;
            }
            Modal::Output(_) => {
                let Modal::Output(o) = &mut self.modal else {
                    return;
                };
                let max = o.lines.len().saturating_sub(modal_page + 1);
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => self.modal = Modal::None,
                    KeyCode::Char('c') if ctrl => self.modal = Modal::None,
                    KeyCode::Char('j') | KeyCode::Down => o.top = (o.top + 1).min(max),
                    KeyCode::Char('k') | KeyCode::Up => o.top = o.top.saturating_sub(1),
                    KeyCode::PageDown => o.top = (o.top + modal_page).min(max),
                    KeyCode::Char('d') if ctrl => o.top = (o.top + modal_page).min(max),
                    KeyCode::Char('u') if ctrl => o.top = o.top.saturating_sub(modal_page),
                    KeyCode::PageUp => o.top = o.top.saturating_sub(modal_page),
                    KeyCode::Home => o.top = 0,
                    KeyCode::End => o.top = max,
                    KeyCode::Char('y') => {
                        let n = o.lines.len();
                        clip::copy(o.text.clone(), n, self.io.events.clone());
                    }
                    _ => {}
                }
                return;
            }
            Modal::AddPlugin { .. } => {
                let n = self.missing_defaults().len();
                let Modal::AddPlugin { index } = &mut self.modal else {
                    return;
                };
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.modal = Modal::None,
                    KeyCode::Char('c') if ctrl => self.modal = Modal::None,
                    KeyCode::Char('j') | KeyCode::Down => {
                        *index = (*index + 1).min(n.saturating_sub(1))
                    }
                    KeyCode::Char('k') | KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Enter if !repeat => {
                        let i = *index;
                        self.modal = Modal::None;
                        let chosen = self.missing_defaults().get(i).map(|p| (*p).clone());
                        if let Some(p) = chosen {
                            self.info(format!("adding {}...", p.name));
                            cmdbar::run(
                                self.root.clone(),
                                vec!["plugin".into(), "add".into(), p.id.clone()],
                                Some(format!(
                                    "Added {}. Its files are in .mira/plugins/{}/.",
                                    p.name, p.id
                                )),
                                self.io.events.clone(),
                            );
                        }
                    }
                    _ => {}
                }
                return;
            }
            Modal::Confirm { .. } => {
                match k.code {
                    KeyCode::Char('y') | KeyCode::Enter if !repeat => {
                        if let Modal::Confirm { words, done, .. } =
                            std::mem::replace(&mut self.modal, Modal::None)
                        {
                            self.info(format!("running mira {}...", words.join(" ")));
                            cmdbar::run(
                                self.root.clone(),
                                words,
                                Some(done),
                                self.io.events.clone(),
                            );
                        }
                    }
                    KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                        self.modal = Modal::None
                    }
                    KeyCode::Char('c') if ctrl => self.modal = Modal::None,
                    _ => {}
                }
                return;
            }
            Modal::RowAction { .. } => {
                let Modal::RowAction { choices, index, .. } = &mut self.modal else {
                    return;
                };
                match k.code {
                    KeyCode::Esc => self.modal = Modal::None,
                    KeyCode::Char('c') if ctrl => self.modal = Modal::None,
                    KeyCode::Char('j') | KeyCode::Down => {
                        *index = (*index + 1).min(choices.len().saturating_sub(1))
                    }
                    KeyCode::Char('k') | KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Enter if !repeat => {
                        if let Modal::RowAction {
                            view_ref,
                            choices,
                            index,
                        } = std::mem::replace(&mut self.modal, Modal::None)
                            && let Some(a) = choices.get(index)
                        {
                            self.run_row_action(view_ref, a.clone());
                        }
                    }
                    _ => {}
                }
                return;
            }
            Modal::History { .. } => {
                let Modal::History { runs, index, .. } = &mut self.modal else {
                    return;
                };
                let n = runs.as_ref().map_or(0, Vec::len);
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('H') => {
                        self.modal = Modal::None
                    }
                    KeyCode::Char('[') | KeyCode::Char('1') => self.go_tab(Tab::Logs),
                    KeyCode::Char(']') | KeyCode::Char('3') => self.go_tab(Tab::Output),
                    KeyCode::Tab | KeyCode::BackTab => {
                        self.modal = Modal::None;
                        self.exec(Cmd::Focus);
                    }
                    KeyCode::Char('c') if ctrl => self.modal = Modal::None,
                    KeyCode::Char('j') | KeyCode::Down => {
                        *index = (*index + 1).min(n.saturating_sub(1))
                    }
                    KeyCode::Char('k') | KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::PageUp => {
                        *index = index.saturating_sub(self.pane_height.saturating_sub(2).max(1))
                    }
                    KeyCode::PageDown => {
                        *index = (*index + self.pane_height.saturating_sub(2).max(1))
                            .min(n.saturating_sub(1))
                    }
                    KeyCode::Home => *index = 0,
                    KeyCode::End => *index = n.saturating_sub(1),
                    KeyCode::Char('u') if ctrl => {
                        *index = index.saturating_sub(self.pane_height.saturating_sub(2).max(1))
                    }
                    KeyCode::Char('d') if ctrl => {
                        *index = (*index + self.pane_height.saturating_sub(2).max(1))
                            .min(n.saturating_sub(1))
                    }
                    KeyCode::Enter if !repeat && n > 0 => {
                        if let Modal::History {
                            action_ref,
                            runs: Some(runs),
                            index,
                            ..
                        } = std::mem::replace(&mut self.modal, Modal::None)
                            && let Some(rec) = runs.into_iter().nth(index)
                        {
                            self.open_history_run(action_ref, rec);
                        }
                    }
                    _ => {}
                }
                return;
            }
            Modal::Help { .. } => {
                let Modal::Help { top, max } = &mut self.modal else {
                    return;
                };
                match k.code {
                    KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter => {
                        self.modal = Modal::None
                    }
                    KeyCode::Char('c') if ctrl => self.modal = Modal::None,
                    KeyCode::Char('j') | KeyCode::Down => *top = (*top + 1).min(*max),
                    KeyCode::Char('k') | KeyCode::Up => *top = top.saturating_sub(1),
                    KeyCode::Char('d') if ctrl => *top = (*top + modal_page).min(*max),
                    KeyCode::Char('u') if ctrl => *top = top.saturating_sub(modal_page),
                    KeyCode::PageDown | KeyCode::Char(' ') => *top = (*top + modal_page).min(*max),
                    KeyCode::PageUp => *top = top.saturating_sub(modal_page),
                    KeyCode::Char('g') | KeyCode::Home => *top = 0,
                    KeyCode::Char('G') | KeyCode::End => *top = *max,
                    _ => {}
                }
                return;
            }
            Modal::None => {}
        }
        let cmd = match (k.code, ctrl) {
            (KeyCode::Char('c'), true) => Cmd::Quit,
            (KeyCode::Char('d'), true) => Cmd::PageDown,
            (KeyCode::Char('u'), true) => Cmd::PageUp,
            (_, true) => return,
            (KeyCode::Char('j') | KeyCode::Down, _) => Cmd::Down,
            (KeyCode::Char('k') | KeyCode::Up, _) => Cmd::Up,
            (KeyCode::PageDown, _) => Cmd::PageDown,
            (KeyCode::PageUp, _) => Cmd::PageUp,
            (KeyCode::Char('g') | KeyCode::Home, _) => Cmd::Top,
            (KeyCode::Char('G') | KeyCode::End, _) => Cmd::Bottom,
            (KeyCode::Enter, _) => Cmd::Open,
            (KeyCode::Char('s'), _) => Cmd::Toggle,
            (KeyCode::Char('r'), _) => Cmd::Restart,
            (KeyCode::Tab | KeyCode::BackTab, _) => Cmd::Focus,
            (KeyCode::Char('/'), _) => Cmd::Search,
            (KeyCode::Char('q'), _) => Cmd::Quit,
            (KeyCode::Char('b'), _) => Cmd::Keep,
            (KeyCode::Char('h') | KeyCode::Left, _) => Cmd::Left,
            (KeyCode::Char('l') | KeyCode::Right, _) => Cmd::Right,
            (KeyCode::Char('w'), _) => Cmd::Wrap,
            (KeyCode::Char('y'), _) => Cmd::Copy,
            (KeyCode::Char('v'), _) => Cmd::Select,
            (KeyCode::Esc, _) => Cmd::Escape,
            (KeyCode::Char('n'), _) => Cmd::NextMatch,
            (KeyCode::Char('N'), _) => Cmd::PrevMatch,
            (KeyCode::Char('?'), _) => Cmd::Help,
            (KeyCode::Char('a'), _) => Cmd::Attach,
            (KeyCode::Char(':'), _) => Cmd::Command,
            (KeyCode::Char('t'), _) => Cmd::Schedule,
            (KeyCode::Char('m'), _) => Cmd::Mouse,
            (KeyCode::Char('Y'), _) => Cmd::CopyAll,
            (KeyCode::Char('H'), _) => Cmd::History,
            (KeyCode::Char('o'), _) => Cmd::OpenWritten,
            (KeyCode::Char('x'), _) => Cmd::Remove,
            (KeyCode::Char('+'), _) => Cmd::AddPlugin,
            (KeyCode::Char('R'), _) => Cmd::Refresh,
            (KeyCode::Char(']'), _) => Cmd::NextTab,
            (KeyCode::Char('['), _) => Cmd::PrevTab,
            (KeyCode::Char(c @ '1'..='3'), _) => Cmd::GoTab(c as u8 - b'1'),
            _ => return,
        };
        let navigation = matches!(
            cmd,
            Cmd::Up | Cmd::Down | Cmd::PageUp | Cmd::PageDown | Cmd::Left | Cmd::Right
        );
        // Auto-repeat moves; it never repeats stop, restart, or other effects.
        if repeat && !navigation {
            return;
        }
        // One binding covers both directions of a pair (j/k, PgUp/PgDn, h/l, n/N).
        let bound = self.bindings().iter().any(|b| {
            b.cmd == cmd
                || (cmd == Cmd::Up && b.cmd == Cmd::Down)
                || (cmd == Cmd::PageDown && b.cmd == Cmd::PageUp)
                || (cmd == Cmd::Left && b.cmd == Cmd::Right)
                || (cmd == Cmd::PrevMatch && b.cmd == Cmd::NextMatch)
                || (cmd == Cmd::PrevTab && b.cmd == Cmd::NextTab)
                || matches!((cmd, b.cmd), (Cmd::GoTab(_), Cmd::GoTab(_)))
        });
        if bound {
            self.exec(cmd);
        }
    }

    pub(super) fn return_to_tools(&mut self) {
        self.term.unfocus();
        self.focus_on_start = None;
        self.queued = None;
        self.cancel_search();
        self.modal = Modal::None;
        self.focus = Focus::List;
        if self.tab == Tab::History {
            self.tab = Tab::Logs;
        }
    }

    fn cancel_search(&mut self) {
        if let Modal::Search {
            logs,
            prev_filter,
            prev_selection,
            ..
        } = std::mem::replace(&mut self.modal, Modal::None)
            && !logs
        {
            self.filter = prev_filter;
            self.refilter(prev_selection);
            self.on_select();
        }
    }

    fn apply_search(&mut self) {
        let Modal::Search { logs, text, .. } = std::mem::replace(&mut self.modal, Modal::None)
        else {
            return;
        };
        if !logs {
            // Enter puts the cursor on the best match.
            self.selected = 0;
            self.on_select();
            return;
        }
        if text.is_empty() {
            self.log_query = None;
            return;
        }
        self.log_query = Some(text.clone());
        self.find(&text, true);
    }

    pub(super) fn find(&mut self, q: &str, older: bool) {
        let found = self.selected_pane_mut().is_some_and(|p| p.find(q, older));
        if !found {
            let dir = if older { "above" } else { "below" };
            self.info(format!(
                "no match for \"{q}\" {dir} the cursor in the loaded lines"
            ));
        }
    }
}

impl App {
    pub fn mouse_event(&mut self, m: crossterm::event::MouseEvent) {
        // Plain pointer movement is reported too; nothing here reacts to it.
        if !self.mouse || m.kind == MouseEventKind::Moved {
            return;
        }
        let point = Position::new(m.column, m.row);
        let delta = match m.kind {
            MouseEventKind::ScrollUp => -3,
            MouseEventKind::ScrollDown => 3,
            _ => 0,
        };
        let click = m.kind == MouseEventKind::Down(MouseButton::Left);
        match &mut self.modal {
            Modal::Help { top, max } => {
                *top = top.saturating_add_signed(delta).min(*max);
            }
            Modal::Output(o) => {
                let height = self
                    .modal_rect
                    .map_or(1, |r| r.height.saturating_sub(2) as usize);
                o.top = o
                    .top
                    .saturating_add_signed(delta)
                    .min(o.lines.len().saturating_sub(height));
            }
            Modal::None | Modal::History { .. } => {}
            _ => return,
        }
        if matches!(self.modal, Modal::Help { .. } | Modal::Output(_)) {
            if click && self.modal_rect.is_some_and(|r| !r.contains(point)) {
                self.modal = Modal::None;
            }
            return;
        }
        let hit = self
            .hits
            .iter()
            .rev()
            .find(|(r, _)| r.contains(point))
            .copied();
        let Some((rect, hit)) = hit else { return };
        if delta != 0 {
            match hit {
                Hit::Sidebar | Hit::Entry(_) => {
                    self.list_manual = true;
                    self.list_offset = self.list_offset.saturating_add_signed(delta);
                }
                Hit::Pane => {
                    if let Modal::History { runs, index, .. } = &mut self.modal {
                        *index = index
                            .saturating_add_signed(delta)
                            .min(runs.as_ref().map_or(0, |r| r.len().saturating_sub(1)));
                    } else if !self.wheel_logs(delta) {
                        let focus = self.focus;
                        self.focus = Focus::Logs;
                        for _ in 0..3 {
                            self.exec(if delta < 0 { Cmd::Up } else { Cmd::Down });
                        }
                        self.focus = focus;
                    }
                }
                Hit::Program => {
                    if !self.term.is_focused() {
                        self.sync_terminal();
                        self.term.focus();
                    }
                    self.term.mouse(m, rect);
                }
                _ => {}
            }
        } else if click {
            match hit {
                Hit::Entry(i) => {
                    if self.term.is_focused() {
                        self.return_to_tools();
                    }
                    let double = self.last_click.is_some_and(|(last, at)| {
                        last == i && at.elapsed() <= Duration::from_millis(400)
                    });
                    self.last_click = if double {
                        None
                    } else {
                        Some((i, Instant::now()))
                    };
                    self.modal = Modal::None;
                    self.selected = i;
                    self.on_select();
                    self.focus = Focus::List;
                    if double {
                        self.exec(Cmd::Open);
                    }
                }
                Hit::Tab(i) => self.exec(Cmd::GoTab(i as u8)),
                Hit::Pane => {
                    if self.term.is_focused() {
                        self.return_to_tools();
                    }
                    self.focus = Focus::Logs;
                }
                Hit::Key(cmd) => {
                    if matches!(self.modal, Modal::History { .. }) {
                        let code = match cmd {
                            Cmd::Down => Some(KeyCode::Down),
                            Cmd::Open => Some(KeyCode::Enter),
                            Cmd::Escape => Some(KeyCode::Esc),
                            _ => None,
                        };
                        if let Some(code) = code {
                            self.key(KeyEvent::new(code, KeyModifiers::NONE));
                        } else {
                            self.exec(cmd);
                        }
                    } else {
                        self.exec(cmd);
                    }
                }
                Hit::Program => {
                    if self.term.is_focused() {
                        self.term.mouse(m, rect);
                    } else {
                        self.sync_terminal();
                        self.term.focus();
                    }
                }
                Hit::Sidebar => {}
            }
            if !matches!(hit, Hit::Entry(_)) {
                self.last_click = None;
            }
        } else if matches!(hit, Hit::Program) && self.term.is_focused() {
            self.term.mouse(m, rect);
        }
    }

    /// Bracketed paste goes to the text field that has the focus.
    pub fn paste(&mut self, text: &str) {
        match &mut self.modal {
            Modal::Form(f) => f.paste(text),
            Modal::Command { text: t, .. } => t.push_str(&text.replace(['\r', '\n'], " ")),
            Modal::Search { .. } => {
                for c in text.chars().filter(|c| !c.is_control()) {
                    self.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
                }
            }
            _ => {}
        }
    }

    /// The wheel over a log pane moves the view, not the line cursor, like a GUI log
    /// viewer; scrolling back to the end follows new lines again. Returns false when the
    /// pane under the pointer is not a log (Output, Details, and views scroll by keys).
    fn wheel_logs(&mut self, delta: isize) -> bool {
        let text_pane = (self.tab == Tab::Output && self.selected_oneoff().is_none())
            || (self.selected_oneoff().is_some() && self.oneoff_details)
            || self.selected_view().is_some();
        if text_pane {
            return false;
        }
        let Some(p) = self.selected_pane_mut() else {
            return false;
        };
        if delta > 0 && !p.is_pinned() {
            return true;
        }
        p.page(delta);
        if delta > 0
            && p.visible()
                .last()
                .is_some_and(|(i, _)| i + 1 >= p.records.len())
        {
            p.follow();
        }
        true
    }
}
