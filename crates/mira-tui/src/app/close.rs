//! What closing this window does to the runs, and the words that say it.

use mira_protocol::ipc::*;

use super::App;

impl App {
    pub fn is_controller(&self) -> bool {
        self.control_lost.is_none()
            && self
                .session
                .as_ref()
                .is_some_and(|s| Some(&s.id) == self.attached_to.as_ref())
    }

    /// What closing this window does to the runs.
    pub fn close_effect(&self) -> Close {
        let Some(s) = &self.session else {
            return Close::NoSession;
        };
        if !self.is_controller() {
            return Close::Watching;
        }
        if s.state == SessionState::Stopping {
            return Close::Stopping;
        }
        if s.controller_count > 1 {
            return Close::OtherWindows(s.controller_count as usize - 1);
        }
        if s.background_lease || s.expires_at.is_some() {
            return Close::Kept(s.expires_at.map(|t| self.clock.hm(t)));
        }
        Close::Last
    }

    /// Runs that closing this window stops or leaves running.
    pub fn run_count(&self) -> usize {
        self.active.len() + self.adhoc_runs
    }

    /// The footer label of `q`: `quit (stops 2 runs)`, `quit (keeps running until 16:07)`.
    pub fn quit_label(&self) -> String {
        close_label(&self.close_effect(), self.run_count())
    }

    /// The line printed after the window closes.
    pub fn quit_message(&self) -> String {
        close_message(&self.close_effect(), self.run_count())
    }
}

/// What closing this window does to the runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Close {
    NoSession,
    /// This window does not control the runs.
    Watching,
    Stopping,
    /// This many other windows stay open.
    OtherWindows(usize),
    /// The runs keep running in the background, until this local time when set.
    Kept(Option<String>),
    /// The last window: its runs stop.
    Last,
}

/// `1 run`, `2 runs`.
pub fn runs_word(n: usize) -> String {
    format!("{n} run{}", if n == 1 { "" } else { "s" })
}

pub fn close_label(c: &Close, runs: usize) -> String {
    match c {
        Close::Kept(Some(t)) => format!("quit (keeps running until {t})"),
        Close::Kept(None) => "quit (keeps running)".into(),
        Close::Watching | Close::OtherWindows(_) if runs > 0 => "quit (keeps running)".into(),
        Close::Last if runs > 0 => format!("quit (stops {})", runs_word(runs)),
        _ => "quit".into(),
    }
}

pub fn close_message(c: &Close, runs: usize) -> String {
    let (keep, them) = if runs == 1 {
        ("1 run keeps running".to_owned(), "it")
    } else {
        (format!("{runs} runs keep running"), "them")
    };
    match c {
        Close::NoSession => "Nothing was running.".into(),
        Close::Stopping => "Mira was already stopping its runs.".into(),
        Close::Watching | Close::Last if runs == 0 => "Nothing was running.".into(),
        Close::Watching => format!("{keep}. `mira down` stops {them}."),
        Close::Last => format!("Stopping {}.", runs_word(runs)),
        Close::OtherWindows(n) => {
            let open = if *n == 1 {
                "Another Mira window is open.".to_owned()
            } else {
                format!("{n} other Mira windows are open.")
            };
            if runs == 0 {
                open
            } else {
                format!("{open} {keep}.")
            }
        }
        Close::Kept(until) => {
            let until = until
                .as_ref()
                .map_or(String::new(), |t| format!(" until {t}"));
            if runs == 0 {
                format!("Mira keeps running in the background{until}. `mira down` stops it.")
            } else {
                format!("{keep}{until}. `mira down` stops {them}.")
            }
        }
    }
}
