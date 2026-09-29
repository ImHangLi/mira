//! State marks: the glyph, word, and tone for runs, items, one-off runs, and views.

use mira_protocol::manifest::ViewKind;
use mira_protocol::run::{Lifecycle, Outcome};
use mira_protocol::time::Timestamp;
use mira_protocol::view::Freshness;

use crate::app::{App, Entry, Intent, Item, OneOff, ViewItem};
use crate::theme::{self, Mark, Tone};

pub(super) fn life_mark(l: Lifecycle) -> Mark {
    match l {
        Lifecycle::Starting => theme::STARTING,
        Lifecycle::Running => theme::RUNNING,
        Lifecycle::Stopping { .. } => theme::STOPPING,
        Lifecycle::Finished { outcome } => match outcome {
            Outcome::Succeeded => theme::OK,
            Outcome::Cancelled => theme::STOPPED,
            Outcome::Failed => theme::FAILED,
            Outcome::TimedOut => Mark::new("✗", "timed out", Some(Tone::Rose)),
            Outcome::Interrupted => Mark::new("✗", "interrupted", Some(Tone::Rose)),
        },
    }
}

pub(super) fn item_mark(app: &App, item: &Item) -> Mark {
    // An app page shows what it is, not whether its program runs.
    if item.is_page() {
        return if item.enabled {
            Mark::new(if theme::ascii() { "::" } else { "▣" }, "page", None)
        } else {
            theme::DISABLED
        };
    }
    if let Some(i) = app.pending.get(&item.action_ref) {
        return match i {
            Intent::Stop => theme::STOPPING,
            _ => theme::STARTING,
        };
    }
    if let Some(r) = app.active.get(&item.action_ref) {
        return life_mark(r.lifecycle);
    }
    if !item.enabled {
        return theme::DISABLED;
    }
    match app.last.get(&item.action_ref).map(|l| l.lifecycle) {
        Some(Lifecycle::Finished { outcome }) => life_mark(Lifecycle::Finished { outcome }),
        Some(_) => Mark::new("■", "ended", None),
        None => theme::NOT_RUN,
    }
}

pub(super) fn oneoff_mark(o: &OneOff) -> Mark {
    if o.stopping {
        theme::STOPPING
    } else {
        life_mark(o.lifecycle)
    }
}

pub(super) fn kind_glyph(k: ViewKind) -> &'static str {
    match (k, theme::ascii()) {
        (ViewKind::Table, false) => "▦",
        (ViewKind::Table, true) => "[]",
        (ViewKind::Log, false) => "≡",
        (ViewKind::Log, true) => ">_",
        (ViewKind::Tree, _) => "#",
        (ViewKind::Text, false) => "¶",
        (ViewKind::Text, true) => "Aa",
        (ViewKind::Json, _) => "{}",
    }
}

/// The word a sidebar view row shows for a state its tone also shows, so the state never
/// depends on color alone.
pub(super) fn view_flag(app: &App, v: &ViewItem) -> Option<&'static str> {
    let p = app.view_panes.get(&v.view_ref)?;
    if p.error.is_some() {
        return Some("error");
    }
    let m = p.meta.as_ref()?;
    p.revision()?;
    (m.freshness == Freshness::Stale).then_some("stale")
}

pub(super) fn view_tone(app: &App, v: &ViewItem) -> Option<Tone> {
    let p = app.view_panes.get(&v.view_ref)?;
    if p.error.is_some() {
        return Some(Tone::Rose);
    }
    let m = p.meta.as_ref()?;
    p.revision()?;
    match m.freshness {
        Freshness::Current => Some(Tone::Leaf),
        Freshness::Stale => Some(Tone::Amber),
        Freshness::Historical => None,
    }
}

/// When the entry last changed: the active run's start, the last run's end, or a view's
/// recorded time. An app page has no time.
pub(super) fn entry_time(app: &App, e: Entry) -> Option<Timestamp> {
    match e {
        Entry::Action(i) => {
            let item = app.items.get(i)?;
            if item.is_page() {
                return None;
            }
            let a = &item.action_ref;
            app.active
                .get(a)
                .map(|r| r.started_at)
                .or_else(|| app.last.get(a).and_then(|l| l.ended_at))
        }
        Entry::View(i) => {
            let v = app.views.get(i)?;
            app.view_panes
                .get(&v.view_ref)
                .and_then(|p| p.meta.as_ref())
                .and_then(|m| m.recorded_at)
        }
        Entry::OneOff(i) => {
            let o = app.oneoffs.get(i)?;
            Some(o.ended_at.unwrap_or(o.started_at))
        }
    }
}
