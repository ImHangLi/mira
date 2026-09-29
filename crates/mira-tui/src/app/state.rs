//! Presentation state types: list entries, one-off runs, tabs, focus, modals, and notices.

use std::collections::HashMap;
use std::time::Instant;

use mira_protocol::ids::{ActionId, ActionRef, Digest, ItemRef, RunId, ViewRef};
use mira_protocol::manifest::{ActionMode, JsonObject, ShowPolicy, ViewKind};
use mira_protocol::run::{CleanupState, ExitInfo, Lifecycle, RunRecord, RunResult};
use mira_protocol::time::Timestamp;

use crate::cmdbar;
use crate::form::Form;
use crate::logs::LogPane;

/// One-off (`mira exec`) runs listed besides the ones still running.
const ONEOFF_KEEP: usize = 5;

pub struct Item {
    pub action_ref: ActionRef,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    pub mode: ActionMode,
    pub show: ShowPolicy,
    pub enabled: bool,
    pub definition_hash: Digest,
}

impl Item {
    /// An app page (`show: "on_select"`), such as a timer or a game: it opens when selected
    /// and has no run state to show, start, or stop.
    pub fn is_page(&self) -> bool {
        self.show == ShowPolicy::OnSelect
    }
}

pub struct ViewItem {
    pub view_ref: ViewRef,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    pub kind: ViewKind,
}

/// One row of the tool list: an action or a view of some plugin, or a one-off run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Entry {
    Action(usize),
    View(usize),
    OneOff(usize),
}

/// What stays selected when the list changes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Key {
    Item(ItemRef),
    Run(RunId),
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Key::Item(r) => r.fmt(f),
            Key::Run(r) => r.fmt(f),
        }
    }
}

/// A one-off `mira exec` run (no action), from any client of this workspace.
pub struct OneOff {
    pub run_id: RunId,
    /// The `exec --label`; `None` until the run record is read.
    pub label: Option<String>,
    pub lifecycle: Lifecycle,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
    pub exit: Option<ExitInfo>,
    pub cleanup: Option<CleanupState>,
    pub source: Option<mira_protocol::run::RunSource>,
    /// The agent that asked for the run; `None` for a person.
    pub requester: Option<mira_protocol::run::Requester>,
    /// A stop was sent and not answered yet.
    pub stopping: bool,
    /// The log panel, made when the run is first selected.
    pub pane: Option<LogPane>,
}

impl OneOff {
    pub(super) fn new(run_id: RunId, lifecycle: Lifecycle, started_at: Timestamp) -> Self {
        Self {
            run_id,
            label: None,
            lifecycle,
            started_at,
            ended_at: None,
            exit: None,
            cleanup: None,
            source: None,
            requester: None,
            stopping: false,
            pane: None,
        }
    }

    /// The label, or a placeholder until the run record is read.
    pub fn title(&self) -> String {
        self.label
            .clone()
            .unwrap_or_else(|| format!("exec {}", &self.run_id.as_str()[..10]))
    }

    pub(super) fn apply(&mut self, rec: &RunRecord) {
        self.label = Some(rec.label.clone());
        self.lifecycle = rec.lifecycle;
        self.started_at = rec.started_at;
        self.ended_at = rec.ended_at;
        self.exit = rec.exit.clone();
        self.cleanup = Some(rec.cleanup.clone());
        self.source = Some(rec.source);
        self.requester = rec.requester.clone();
    }

    /// Runs of one agent thread share a group, and so do runs people start (`you`). Runs
    /// recorded before Mira 0.14 do not say who started them: they share `earlier`.
    pub fn group(&self) -> &str {
        self.requester.as_ref().map_or("earlier", |r| r.id.as_str())
    }
}

/// Orders one-off runs by group (the group with the newest run first), newest first within
/// a group, and keeps every active run plus the newest [`ONEOFF_KEEP`] finished runs of each
/// group. Returns how many finished runs each group hides.
pub fn keep_oneoffs(list: &mut Vec<OneOff>) -> HashMap<String, usize> {
    let mut newest: HashMap<String, Timestamp> = HashMap::new();
    for o in list.iter() {
        let e = newest.entry(o.group().to_owned()).or_insert(o.started_at);
        if o.started_at > *e {
            *e = o.started_at;
        }
    }
    list.sort_by(|a, b| {
        let (ga, gb) = (&newest[a.group()], &newest[b.group()]);
        gb.cmp(ga)
            .then_with(|| a.group().cmp(b.group()))
            .then_with(|| b.started_at.cmp(&a.started_at))
            .then_with(|| b.run_id.cmp(&a.run_id))
    });
    let mut shown: HashMap<String, usize> = HashMap::new();
    let mut hidden: HashMap<String, usize> = HashMap::new();
    list.retain(|o| {
        if o.lifecycle.is_active() {
            return true;
        }
        let n = shown.entry(o.group().to_owned()).or_default();
        *n += 1;
        if *n <= ONEOFF_KEEP {
            true
        } else {
            *hidden.entry(o.group().to_owned()).or_default() += 1;
            false
        }
    });
    hidden
}

pub enum Inputs {
    Loading,
    /// No input properties: runs directly.
    Free,
    /// The input schema; a form collects the values.
    Form(JsonObject),
    Unknown(String),
}

/// The tabs of the main pane for an action. `History` shows while the history list is open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Logs,
    History,
    Output,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Logs, Tab::History, Tab::Output];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Logs => "Logs",
            Tab::History => "History",
            Tab::Output => "Output",
        }
    }

    pub(super) fn index(self) -> usize {
        match self {
            Tab::Logs => 0,
            Tab::History => 1,
            Tab::Output => 2,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Logs,
}

pub enum Modal {
    None,
    Search {
        logs: bool,
        text: String,
        prev_filter: String,
        prev_selection: Option<Key>,
    },
    Form(Box<Form>),
    /// The `:` command bar.
    Command {
        text: String,
        error: Option<String>,
    },
    /// Output of a command-bar command.
    Output(Box<cmdbar::Output>),
    /// Ask before a command-bar command with a lasting effect: `y` or Enter runs `words`,
    /// and a success shows `done` as a notice.
    Confirm {
        title: String,
        lines: Vec<String>,
        words: Vec<String>,
        done: String,
    },
    /// Choose which row action to run on the selected table row.
    RowAction {
        view_ref: ViewRef,
        choices: Vec<ActionId>,
        index: usize,
    },
    /// Choose a default plugin to add (`+`); `index` is into [`App::missing_defaults`].
    AddPlugin {
        index: usize,
    },
    /// The newest runs of one action; `runs` is `None` while the host answers.
    History {
        action_ref: ActionRef,
        runs: Option<Vec<RunRecord>>,
        error: Option<String>,
        index: usize,
    },
    /// `top` is the first shown line; `max` the largest `top` (0 when all lines fit),
    /// which drawing sets.
    Help {
        top: usize,
        max: usize,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Intent {
    Start,
    Stop,
    Restart,
}

pub struct LastRun {
    pub run_id: RunId,
    pub lifecycle: Lifecycle,
    pub exit: Option<ExitInfo>,
    pub started_at: Option<Timestamp>,
    pub ended_at: Option<Timestamp>,
    /// The structured result of the run, once its final record is read.
    pub result: Option<RunResult>,
    /// The cleanup state, once the final record is read.
    pub cleanup: Option<CleanupState>,
}

pub struct Notice {
    pub text: String,
    pub error: bool,
    pub(super) at: Instant,
    /// The run an info notice is about; it goes when that run changes state or ends, or
    /// when another item is selected.
    pub(super) about: Option<(ActionRef, RunId)>,
    /// The run's [`stage`] when the notice was last checked.
    pub(super) seen: Option<u8>,
    /// A finished run succeeded: the notice reads with a check mark.
    pub ok: bool,
    /// A view the finished row action wrote; `o` opens it while the notice shows.
    pub open: Option<ViewRef>,
}

/// A row action started from a table view, followed until its run ends.
pub(super) struct RowRun {
    pub(super) view_ref: ViewRef,
    pub(super) action: ActionId,
    pub(super) run_id: Option<RunId>,
    /// Other views of the same plugin that changed while it ran.
    pub(super) wrote: Vec<ViewRef>,
}

pub(super) enum OpenKind {
    Start,
    Logs,
    NeedsInput,
    Form,
}
