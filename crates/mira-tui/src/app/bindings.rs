//! Commands and key bindings. The footer, the help screen, and the key router all read
//! the same [`App::bindings`] set.

use mira_protocol::ipc::*;
use mira_protocol::manifest::{ActionMode, ViewKind};

use crate::form;
use crate::logs::LogPane;
use crate::views::ViewPane;

use super::{App, Focus, Intent, Modal, OpenKind, Tab, start_word};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    Bottom,
    Open,
    Toggle,
    Restart,
    Focus,
    /// Return to the tools from any pane, modal, or program.
    Tools,
    Search,
    Quit,
    Keep,
    Left,
    Right,
    Wrap,
    Copy,
    Select,
    Escape,
    NextMatch,
    PrevMatch,
    Help,
    /// Attach to the selected PTY run (or retry taking its input).
    Attach,
    /// Leave the terminal view.
    Detach,
    /// Keys go to the attached program (footer entry only).
    Forward,
    Command,
    Schedule,
    Mouse,
    CopyAll,
    /// Open the run history of the selected action.
    History,
    /// Switch the main pane tab (`[` and `]`).
    NextTab,
    PrevTab,
    /// Go to one tab by number (`1`, `2`, `3`).
    GoTab(u8),
    /// Open the view the last row action wrote (`o`).
    OpenWritten,
    /// Remove the selected item's plugin from the project, after a confirmation (`x`).
    Remove,
    /// Choose a default plugin to add to the project (`+`).
    AddPlugin,
    /// Load `.mira` again from disk and refresh the tools, runs, and views (`R`).
    Refresh,
}

pub struct Binding {
    pub keys: &'static str,
    pub label: String,
    pub cmd: Cmd,
    pub footer: bool,
}

pub(crate) fn bind(keys: &'static str, label: impl Into<String>, cmd: Cmd) -> Binding {
    Binding {
        keys,
        label: label.into(),
        cmd,
        footer: true,
    }
}

fn hidden(keys: &'static str, label: impl Into<String>, cmd: Cmd) -> Binding {
    Binding {
        footer: false,
        ..bind(keys, label, cmd)
    }
}

impl App {
    /// Every key that works now. The footer shows the `footer` ones; the router accepts
    /// only these.
    pub fn bindings(&self) -> Vec<Binding> {
        let mut v = self.context_bindings();
        let away = !matches!(self.modal, Modal::None)
            || self.term.is_focused()
            || self.focus != Focus::List;
        if away {
            // One key back from anywhere; other keys that also go back stay in help only.
            for b in &mut v {
                if matches!(b.cmd, Cmd::Focus | Cmd::Escape | Cmd::Detach)
                    && matches!(b.label.as_str(), "tools" | "back" | "back to tools")
                {
                    b.footer = false;
                }
            }
            v.insert(0, bind("Ctrl-T", "back to tools", Cmd::Tools));
        } else {
            v.insert(0, hidden("Ctrl-T", "back to tools", Cmd::Tools));
        }
        // When Enter already opens the logs, Tab says the same thing; keep one in the footer.
        if v.iter()
            .any(|b| b.footer && b.cmd == Cmd::Open && b.label == "logs")
        {
            for b in &mut v {
                if b.cmd == Cmd::Focus && b.label == "logs" {
                    b.footer = false;
                }
            }
        }
        v
    }

    fn context_bindings(&self) -> Vec<Binding> {
        if let Some(v) = self.term.bindings() {
            return v;
        }
        let mut v = Vec::new();
        match &self.modal {
            Modal::Search { logs, .. } => {
                v.push(bind(
                    "type",
                    if *logs { "find text" } else { "filter" },
                    Cmd::Search,
                ));
                v.push(bind("Enter", "apply", Cmd::Open));
                v.push(bind("Esc", "cancel", Cmd::Escape));
                return v;
            }
            Modal::Form(f) => {
                v.push(bind("Tab/Up/Down", "field", Cmd::Down));
                if f.focused()
                    .is_some_and(|x| matches!(x.kind, form::Kind::Boolean | form::Kind::Enum(_)))
                {
                    v.push(bind("Space", "choose", Cmd::Toggle));
                } else {
                    v.push(bind("Ctrl-U", "clear", Cmd::Escape));
                }
                if !f.pending {
                    // Only a process restarts; a task runs again; an app page reloads.
                    let item = self.item(&f.action_ref);
                    let w = match f.intent {
                        Intent::Restart if item.is_some_and(|i| i.is_page()) => "reload",
                        Intent::Restart if item.is_some_and(|i| i.mode == ActionMode::Process) => {
                            "restart"
                        }
                        _ => "run",
                    };
                    v.push(bind("Enter", w, Cmd::Open));
                }
                v.push(bind("Esc", "cancel", Cmd::Escape));
                return v;
            }
            Modal::Command { .. } => {
                v.push(bind("Enter", "run mira command", Cmd::Open));
                v.push(bind("Esc", "cancel", Cmd::Escape));
                return v;
            }
            Modal::Output(_) => {
                v.push(bind("j/k", "scroll", Cmd::Down));
                v.push(bind("y", "copy all", Cmd::Copy));
                v.push(bind("Esc/q", "close", Cmd::Escape));
                return v;
            }
            Modal::AddPlugin { .. } => {
                v.push(bind("j/k", "choose", Cmd::Down));
                v.push(bind("Enter", "add", Cmd::Open));
                v.push(bind("Esc", "cancel", Cmd::Escape));
                return v;
            }
            Modal::Confirm { .. } => {
                v.push(bind("y/Enter", "yes", Cmd::Open));
                v.push(bind("n/Esc", "cancel", Cmd::Escape));
                return v;
            }
            Modal::RowAction { .. } => {
                v.push(bind("j/k", "choose", Cmd::Down));
                v.push(bind("Enter", "run for this row", Cmd::Open));
                v.push(bind("Esc", "cancel", Cmd::Escape));
                return v;
            }
            Modal::History { runs, .. } => {
                if runs.as_ref().is_some_and(|r| !r.is_empty()) {
                    v.push(bind("j/k", "choose", Cmd::Down));
                    v.push(bind("Enter", "open logs", Cmd::Open));
                }
                v.push(bind("[ ]", "tab", Cmd::NextTab));
                v.push(bind("Esc", "close", Cmd::Escape));
                return v;
            }
            Modal::Help { max, .. } => {
                if *max > 0 {
                    v.push(bind("j/k", "scroll", Cmd::Down));
                }
                v.push(bind("Esc/?", "close help", Cmd::Escape));
                return v;
            }
            Modal::None => {}
        }
        self.normal_bindings()
    }

    /// The bindings of normal mode for the current focus and selection.
    pub fn normal_bindings(&self) -> Vec<Binding> {
        if self.selected_view().is_some() {
            return self.view_bindings();
        }
        if self.selected_oneoff().is_some() {
            return self.oneoff_bindings();
        }
        let mut v = Vec::new();
        // The log keys act on the Logs tab only.
        let output = self.tab == Tab::Output;
        let pane = self
            .selected_pane()
            .filter(|p| !p.records.is_empty() && !output);
        match self.focus {
            Focus::List => {
                if self.visible.len() > 1 {
                    v.push(bind("j/k", "move", Cmd::Down));
                    v.push(hidden("PgUp/PgDn", "page", Cmd::PageUp));
                    v.push(hidden("g/Home", "first", Cmd::Top));
                    v.push(hidden("G/End", "last", Cmd::Bottom));
                }
            }
            Focus::Logs => {
                if output {
                    v.push(bind("j/k", "scroll", Cmd::Down));
                }
                if let Some(p) = pane {
                    scroll_keys(&mut v, p);
                }
                if pane.is_none_or(|p| p.anchor.is_none())
                    && self
                        .selected_ref()
                        .is_some_and(|a| self.viewing.contains_key(&a))
                {
                    v.push(bind("Esc", "latest run", Cmd::Escape));
                }
            }
        }
        if let Some(item) = self.selected_item() {
            let on_screen = self.attach_target().is_some();
            match self.open_kind(item) {
                _ if on_screen => v.push(bind("Enter", "use the program", Cmd::Open)),
                Some(OpenKind::Start) if item.is_page() => v.push(bind("Enter", "open", Cmd::Open)),
                Some(OpenKind::Start) => v.push(bind("Enter", start_word(item), Cmd::Open)),
                Some(OpenKind::Logs) if self.focus == Focus::List => {
                    v.push(bind("Enter", "logs", Cmd::Open))
                }
                Some(OpenKind::NeedsInput) => v.push(bind("Enter", "form", Cmd::Open)),
                Some(OpenKind::Form) => {
                    v.push(bind("Enter", format!("{}...", start_word(item)), Cmd::Open))
                }
                _ => {}
            }
            if on_screen {
                v.push(hidden("a", "use the program", Cmd::Attach));
            }
            match self.toggle_intent(item) {
                Some(Intent::Stop) => v.push(bind("s", "stop", Cmd::Toggle)),
                // When Enter already shows the same word, keep `s` in help only.
                Some(_)
                    if v.iter()
                        .any(|b| b.keys == "Enter" && b.label == start_word(item)) =>
                {
                    v.push(hidden("s", start_word(item), Cmd::Toggle))
                }
                Some(_) => v.push(bind("s", start_word(item), Cmd::Toggle)),
                None => {}
            }
            if self.restart_ok(item) {
                let w = if item.is_page() {
                    "reload"
                } else if item.mode == ActionMode::Process {
                    "restart"
                } else {
                    "rerun"
                };
                v.push(bind("r", w, Cmd::Restart));
            }
            if self.has_history(&item.action_ref) {
                v.push(bind("H", "history", Cmd::History));
            }
            if self.has_schedule(&item.action_ref)
                && self.control_lost.is_none()
                && !self.pending.contains_key(&item.action_ref)
            {
                let on = self
                    .schedule_of(&item.action_ref)
                    .is_some_and(|s| s.enabled);
                let w = if on { "schedule off" } else { "schedule on" };
                v.push(bind("t", w, Cmd::Schedule));
            }
            v.push(hidden("x", "remove plugin", Cmd::Remove));
        }
        if self.selected_item().is_some() {
            if self.term.is_open() {
                v.push(hidden("[ ]", "switch tab", Cmd::NextTab));
            } else {
                v.push(bind("[ ]", "switch tab", Cmd::NextTab));
            }
            v.push(hidden(
                "1 2 3",
                "Logs, History, or Output tab",
                Cmd::GoTab(0),
            ));
            let to = if self.focus == Focus::List {
                "logs"
            } else {
                "tools"
            };
            if self.term.is_open() {
                v.push(hidden("Tab", "go to the program", Cmd::Focus));
            } else {
                v.push(bind("Tab", to, Cmd::Focus));
            }
        }
        if self.focus == Focus::Logs
            && let Some(p) = pane
        {
            self.pane_keys(&mut v, p);
        }
        self.list_keys(&mut v);
        self.global_bindings(&mut v);
        v
    }

    /// Copy, select, find, pan, and wrap keys of a log panel with lines.
    fn pane_keys(&self, v: &mut Vec<Binding>, p: &LogPane) {
        let n = p.selection_text().map_or(1, |(_, n)| n);
        let what = if n == 1 {
            "copy".to_owned()
        } else {
            format!("copy {n} lines")
        };
        v.push(bind("y", what, Cmd::Copy));
        if p.anchor.is_none() {
            v.push(bind("v", "select", Cmd::Select));
        } else {
            v.push(bind("Esc", "unselect", Cmd::Escape));
        }
        v.push(bind("/", "find", Cmd::Search));
        if self.log_query.is_some() {
            v.push(bind("n/N", "next/prev", Cmd::NextMatch));
        }
        if !p.wrap {
            v.push(bind("h/l", "pan", Cmd::Right));
        }
        v.push(bind(
            "w",
            if p.wrap { "no wrap" } else { "wrap" },
            Cmd::Wrap,
        ));
    }

    /// Search and filter keys while the list has the focus.
    fn list_keys(&self, v: &mut Vec<Binding>) {
        if self.focus == Focus::List {
            if !self.items.is_empty() || !self.oneoffs.is_empty() {
                v.push(bind("/", "search", Cmd::Search));
            }
            if !self.filter.is_empty() {
                v.push(bind("Esc", "clear filter", Cmd::Escape));
            }
        }
    }

    /// Keys for a selected one-off run. A one-off run is never run again from here:
    /// its record does not keep the command line.
    fn oneoff_bindings(&self) -> Vec<Binding> {
        let mut v = Vec::new();
        let pane = self
            .selected_pane()
            .filter(|p| !p.records.is_empty() && !self.oneoff_details);
        // First, so the footer keeps it at narrow widths.
        if self.oneoff_stoppable() {
            v.push(bind("s", "stop", Cmd::Toggle));
        }
        match self.focus {
            Focus::List => {
                if self.visible.len() > 1 {
                    v.push(bind("j/k", "move", Cmd::Down));
                    v.push(hidden("PgUp/PgDn", "page", Cmd::PageUp));
                    v.push(hidden("g/Home", "first", Cmd::Top));
                    v.push(hidden("G/End", "last", Cmd::Bottom));
                }
                v.push(bind("Enter", "logs", Cmd::Open));
            }
            Focus::Logs => {
                if pane.is_none_or(|p| p.anchor.is_none()) {
                    v.push(bind("Esc", "back", Cmd::Escape));
                }
                if self.oneoff_details {
                    v.push(bind("j/k", "scroll", Cmd::Down));
                    v.push(hidden("PgUp/PgDn", "page", Cmd::PageUp));
                    v.push(hidden("g/Home", "first", Cmd::Top));
                    v.push(hidden("G/End", "last", Cmd::Bottom));
                } else if let Some(p) = pane {
                    scroll_keys(&mut v, p);
                }
            }
        }
        v.push(bind("[ ]", "switch tab", Cmd::NextTab));
        v.push(hidden("1 2", "Logs or Details tab", Cmd::GoTab(0)));
        let to = if self.focus == Focus::List {
            "logs"
        } else {
            "tools"
        };
        v.push(bind("Tab", to, Cmd::Focus));
        if self.focus == Focus::Logs
            && let Some(p) = pane
        {
            self.pane_keys(&mut v, p);
        }
        self.list_keys(&mut v);
        self.global_bindings(&mut v);
        v
    }

    fn global_bindings(&self, v: &mut Vec<Binding>) {
        v.push(hidden(
            "Shift-Esc",
            "back to tools (also Ctrl-])",
            Cmd::Tools,
        ));
        if self.focus == Focus::List && !self.filter.is_empty() && self.selected_view().is_some() {
            v.push(bind("Esc", "clear filter", Cmd::Escape));
        }
        if let Some(title) = self
            .notice
            .as_ref()
            .and_then(|n| n.open.as_ref())
            .and_then(|r| self.view_title(r))
        {
            v.insert(0, bind("o", format!("open {title}"), Cmd::OpenWritten));
        }
        v.push(bind(":", "command", Cmd::Command));
        v.push(hidden("R", "refresh (load .mira again)", Cmd::Refresh));
        if !self.missing_defaults().is_empty() {
            v.push(hidden("+", "add a default plugin", Cmd::AddPlugin));
        }
        v.push(hidden(
            "m",
            if self.mouse {
                "mouse mode off (terminal selection)"
            } else {
                "mouse mode on (wheel scrolls)"
            },
            Cmd::Mouse,
        ));
        if self.control_lost.is_none()
            && !self.keeping
            && self.is_controller()
            && self
                .session
                .as_ref()
                .is_some_and(|s| s.state == SessionState::Active)
        {
            v.push(bind("b", "background", Cmd::Keep));
        }
        v.push(bind("?", "help", Cmd::Help));
        v.push(bind("q", self.quit_label(), Cmd::Quit));
    }

    /// Keys for a selected view (list or view focus).
    fn view_bindings(&self) -> Vec<Binding> {
        let mut v = Vec::new();
        let pane = self.selected_view_pane();
        let rows = pane.map_or(0, ViewPane::len);
        match self.focus {
            Focus::List => {
                if self.visible.len() > 1 {
                    v.push(bind("j/k", "move", Cmd::Down));
                    v.push(hidden("PgUp/PgDn", "page", Cmd::PageUp));
                    v.push(hidden("g/Home", "first", Cmd::Top));
                    v.push(hidden("G/End", "last", Cmd::Bottom));
                }
                v.push(bind("Enter", "open view", Cmd::Open));
                v.push(bind("Tab", "view", Cmd::Focus));
                v.push(bind("r", "refresh", Cmd::Restart));
                if !self.items.is_empty() || !self.views.is_empty() {
                    v.push(bind("/", "search", Cmd::Search));
                }
            }
            Focus::Logs => {
                // First, so the footer keeps it at narrow widths.
                v.push(bind("Esc", "back", Cmd::Escape));
                if rows > 0 {
                    v.push(bind("j/k", "move", Cmd::Down));
                    v.push(hidden("PgUp/PgDn", "page", Cmd::PageUp));
                    v.push(hidden("g/Home", "first", Cmd::Top));
                    v.push(hidden("G/End", "last", Cmd::Bottom));
                }
                if let Some(p) = pane {
                    if p.kind == ViewKind::Table
                        && rows > 0
                        && !p.row_actions.is_empty()
                        && self.control_lost.is_none()
                    {
                        v.push(bind("Enter", "act on row", Cmd::Open));
                    }
                    if rows > 0 {
                        let (y, big) = match p.kind {
                            ViewKind::Table => ("copy cell", "copy row JSON"),
                            ViewKind::Log => ("copy text", "copy item JSON"),
                            ViewKind::Tree => ("copy label", "copy subtree JSON"),
                            ViewKind::Json => ("copy line", "copy all JSON"),
                            ViewKind::Text => ("copy line", "copy all text"),
                        };
                        v.push(bind("y", y, Cmd::Copy));
                        v.push(bind("Y", big, Cmd::CopyAll));
                        let lr = match p.kind {
                            ViewKind::Table => "column",
                            ViewKind::Tree => "fold",
                            _ => "pan",
                        };
                        if p.kind != ViewKind::Table || p.data().is_some() {
                            v.push(bind("h/l", lr, Cmd::Right));
                        }
                    }
                    if p.can_wrap() {
                        v.push(bind(
                            "w",
                            if p.wrap { "no wrap" } else { "wrap" },
                            Cmd::Wrap,
                        ));
                    }
                }
                v.push(bind("r", "refresh", Cmd::Restart));
                v.push(bind("Tab", "tools", Cmd::Focus));
            }
        }
        self.global_bindings(&mut v);
        v
    }
}

/// Scroll keys of a log panel with lines.
fn scroll_keys(v: &mut Vec<Binding>, p: &LogPane) {
    v.push(bind("j/k", "scroll", Cmd::Down));
    v.push(hidden("PgUp/PgDn", "page", Cmd::PageUp));
    v.push(hidden("g/Home", "oldest", Cmd::Top));
    let follow = if p.is_pinned() { "follow" } else { "bottom" };
    v.push(bind("G/End", follow, Cmd::Bottom));
}
