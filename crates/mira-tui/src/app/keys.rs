//! The key router: modal keys first, then normal keys mapped to bound commands.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::clip;
use crate::cmdbar;
use crate::form::{Form, Outcome};

use super::{App, Cmd, Modal, Tab};

impl App {
    pub fn key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        if self.term.is_open() {
            return self.term.key(k);
        }
        let repeat = k.kind == KeyEventKind::Repeat;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
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
                            cmdbar::run(self.root.clone(), words, self.io.events.clone());
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
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => self.modal = Modal::None,
                    KeyCode::Char('c') if ctrl => self.modal = Modal::None,
                    KeyCode::Char('j') | KeyCode::Down => {
                        o.top = (o.top + 1).min(o.lines.len().saturating_sub(1))
                    }
                    KeyCode::Char('k') | KeyCode::Up => o.top = o.top.saturating_sub(1),
                    KeyCode::PageDown => o.top = (o.top + 10).min(o.lines.len().saturating_sub(1)),
                    KeyCode::PageUp => o.top = o.top.saturating_sub(10),
                    KeyCode::Char('y') => {
                        let n = o.lines.len();
                        clip::copy(o.text.clone(), n, self.io.events.clone());
                    }
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
                    KeyCode::PageDown | KeyCode::Char(' ') => *top = (*top + 10).min(*max),
                    KeyCode::PageUp => *top = top.saturating_sub(10),
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

    fn cancel_search(&mut self) {
        if let Modal::Search {
            logs, prev_filter, ..
        } = std::mem::replace(&mut self.modal, Modal::None)
            && !logs
        {
            self.filter = prev_filter;
            let keep = self.selected_key();
            self.refilter(keep);
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
    /// Mouse mode: the wheel scrolls whatever has the focus.
    pub fn mouse_event(&mut self, m: crossterm::event::MouseEvent) {
        use crossterm::event::MouseEventKind;
        if !self.mouse || !matches!(self.modal, Modal::None) {
            return;
        }
        let cmd = match m.kind {
            MouseEventKind::ScrollDown => Cmd::Down,
            MouseEventKind::ScrollUp => Cmd::Up,
            _ => return,
        };
        for _ in 0..3 {
            self.exec(cmd);
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
}
