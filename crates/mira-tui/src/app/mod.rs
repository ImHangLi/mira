//! Presentation state and the single key router. The host owns every fact; this
//! state is a projection of `state`/`log` events plus replies, and every action goes through
//! the typed client. Footer and key handling read the same [`App::bindings`] set.
//!
//! The state types live in [`state`]; each other submodule adds one group of `App`
//! methods: projection of host events, notices, selection and search, item actions, key
//! bindings, the key router, command execution, and what closing the window does.

mod actions;
mod bindings;
mod close;
mod commands;
mod events;
mod keys;
mod notices;
mod projection;
mod selection;
mod state;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use mira_protocol::clock::LocalClock;
use mira_protocol::error::ErrorInfo;
use mira_protocol::ids::{ActionRef, Digest, RunId, SessionId, ViewRef};
use mira_protocol::ipc::*;
use mira_protocol::manifest::{ActionMode, JsonObject};
use mira_protocol::run::{Lifecycle, RunRecord, RunSummary};
use mira_protocol::time::Timestamp;
use serde_json::{Map, Value};
use tokio::sync::mpsc::UnboundedSender;

use crate::form;
use crate::ipc::{Control, Read, Tx};
use crate::logs::LogPane;
use crate::terminal::Terminals;
use crate::views::ViewPane;

use state::{Key, LastRun, Notice, OpenKind, RowRun, keep_oneoffs};

pub(crate) use bindings::bind;
pub use bindings::{Binding, Cmd};
pub use close::{Close, close_message};
pub use state::{Entry, Focus, Inputs, Intent, Item, Modal, OneOff, Tab, ViewItem};

pub enum Quit {
    Normal(Option<&'static str>),
    Kept(Option<Timestamp>),
}

pub struct Io {
    pub control: UnboundedSender<Control>,
    pub read: UnboundedSender<Read>,
    pub events: Tx,
    pub paths: mira_protocol::paths::WorkspacePaths,
}

pub struct App {
    pub root: String,
    /// `name` from `.mira/workspace.json`, for the header.
    pub workspace_name: Option<String>,
    /// The workspace's git branch (or short commit); `None` outside a git repository.
    pub branch: Option<String>,
    branch_at: Instant,
    view_poll_at: Instant,
    /// Actions whose log pane shows an older run chosen in the history list.
    pub viewing: HashMap<ActionRef, RunRecord>,
    pub clock: LocalClock,
    pub items: Vec<Item>,
    pub views: Vec<ViewItem>,
    plugin_order: Vec<String>,
    pub view_panes: HashMap<ViewRef, ViewPane>,
    pub schedules: Vec<ScheduleData>,
    /// Last visible (never write-only) form values per action, to prefill the next form.
    last_inputs: HashMap<ActionRef, Map<String, Value>>,
    /// Application mouse mode; off keeps the terminal's own selection.
    pub mouse: bool,
    /// A mouse-mode change the render loop still has to apply.
    pub mouse_changed: Option<bool>,
    /// Plugin notifications the render loop still has to pass to the terminal.
    pub notifications: Vec<(String, String)>,
    /// The default plugins this `mira` ships; see [`App::missing_defaults`].
    pub defaults: Vec<crate::cmdbar::DefaultPlugin>,
    status_at: Option<Instant>,
    /// Actions whose description declares a schedule.
    scheduled: std::collections::HashSet<ActionRef>,
    pub catalog_error: Option<ErrorInfo>,
    pub filter: String,
    pub visible: Vec<Entry>,
    pub selected: usize,
    pub list_offset: usize,
    pub focus: Focus,
    pub modal: Modal,
    pub session: Option<SessionInfo>,
    /// The session this TUI is a controller of.
    pub attached_to: Option<SessionId>,
    pub active: HashMap<ActionRef, RunSummary>,
    pub adhoc_runs: usize,
    /// One-off runs, newest first (see [`keep_oneoffs`]).
    pub oneoffs: Vec<OneOff>,
    /// A one-off run shows its Details tab instead of its logs.
    pub oneoff_details: bool,
    pub storage_warnings: Vec<Warning>,
    pub config_warnings: Vec<Warning>,
    pub last: HashMap<ActionRef, LastRun>,
    pub pending: HashMap<ActionRef, Intent>,
    restart_after: HashMap<ActionRef, (RunId, JsonObject)>,
    pub inputs: HashMap<ActionRef, (Digest, Inputs)>,
    queued: Option<(ActionRef, Intent)>,
    pub panes: HashMap<ActionRef, LogPane>,
    pane_order: VecDeque<ActionRef>,
    pub log_query: Option<String>,
    pub notice: Option<Notice>,
    pub stream_issue: Option<String>,
    pub control_lost: Option<String>,
    pub keeping: bool,
    pub quit: Option<Quit>,
    pub narrow: bool,
    /// The terminal is wide enough for the right rail; drawing sets it.
    pub wide: bool,
    /// The main pane tab for actions (`Logs` or `Output`; `History` is the open list).
    pub tab: Tab,
    /// First shown line of the Output tab.
    pub output_top: usize,
    /// The newest runs per action, for the right rail and a quick history tab.
    pub recent: HashMap<ActionRef, Vec<RunRecord>>,
    /// The run state each `recent` entry was requested for; a change reads it again.
    recent_key: HashMap<ActionRef, (Option<RunId>, u8, bool)>,
    /// The PTY attach view.
    pub term: Terminals,
    /// The last non-empty screen line of active PTY runs whose log is still empty.
    pub screens: HashMap<RunId, String>,
    screen_at: Option<Instant>,
    row_run: Option<RowRun>,
    io: Io,
}

impl App {
    pub fn new(root: String, clock: LocalClock, io: Io) -> Self {
        let branch = crate::git::head_label(std::path::Path::new(&root));
        Self {
            workspace_name: workspace_name(&root),
            branch,
            branch_at: Instant::now(),
            view_poll_at: Instant::now(),
            viewing: HashMap::new(),
            root,
            clock,
            items: Vec::new(),
            views: Vec::new(),
            plugin_order: Vec::new(),
            view_panes: HashMap::new(),
            schedules: Vec::new(),
            last_inputs: HashMap::new(),
            mouse: false,
            mouse_changed: None,
            notifications: Vec::new(),
            defaults: Vec::new(),
            status_at: None,
            scheduled: Default::default(),
            catalog_error: None,
            filter: String::new(),
            visible: Vec::new(),
            selected: 0,
            list_offset: 0,
            focus: Focus::List,
            modal: Modal::None,
            session: None,
            attached_to: None,
            active: HashMap::new(),
            adhoc_runs: 0,
            oneoffs: Vec::new(),
            oneoff_details: false,
            storage_warnings: Vec::new(),
            config_warnings: Vec::new(),
            last: HashMap::new(),
            pending: HashMap::new(),
            restart_after: HashMap::new(),
            inputs: HashMap::new(),
            queued: None,
            panes: HashMap::new(),
            pane_order: VecDeque::new(),
            log_query: None,
            notice: None,
            stream_issue: None,
            control_lost: None,
            keeping: false,
            quit: None,
            narrow: false,
            wide: false,
            tab: Tab::Logs,
            output_top: 0,
            recent: HashMap::new(),
            recent_key: HashMap::new(),
            term: Terminals::new(io.paths.clone()),
            screens: HashMap::new(),
            screen_at: None,
            row_run: None,
            io,
        }
    }
}

/// Lifecycle stages a run notice outlives: a started run stays "started" while it runs.
fn stage(l: Lifecycle) -> u8 {
    match l {
        Lifecycle::Starting | Lifecycle::Running => 0,
        Lifecycle::Stopping { .. } => 1,
        Lifecycle::Finished { .. } => 2,
    }
}

/// The non-empty `name` of the workspace file, if it has one.
fn workspace_name(root: &str) -> Option<String> {
    let path = std::path::Path::new(root)
        .join(".mira")
        .join("workspace.json");
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
}

fn start_word(item: &Item) -> &'static str {
    match item.mode {
        ActionMode::Process => "start",
        ActionMode::Task => "run",
    }
}

fn inputs_of(schema: Option<&JsonObject>) -> Inputs {
    match schema {
        Some(s) if form::has_fields(s) => Inputs::Form(s.clone()),
        _ => Inputs::Free,
    }
}
