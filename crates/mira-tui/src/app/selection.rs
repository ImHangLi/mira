//! The selected list entry, the search filter and ranking, panes, and tabs.

use std::collections::HashMap;

use mira_protocol::catalog;
use mira_protocol::ids::ActionRef;
use mira_protocol::run::Lifecycle;

use crate::ipc::Read;
use crate::logs::LogPane;
use crate::views::ViewPane;

use super::{App, Cmd, Entry, Inputs, Item, Key, Modal, OneOff, Tab, ViewItem, stage};

const MAX_PANES: usize = 8;

impl App {
    pub fn item(&self, a: &ActionRef) -> Option<&Item> {
        self.items.iter().find(|i| &i.action_ref == a)
    }

    pub fn selected_entry(&self) -> Option<Entry> {
        self.visible.get(self.selected).copied()
    }

    /// The default plugins this project does not have yet, in `mira plugin add` order.
    pub fn missing_defaults(&self) -> Vec<&crate::cmdbar::DefaultPlugin> {
        self.defaults
            .iter()
            .filter(|d| {
                !self
                    .items
                    .iter()
                    .any(|i| i.action_ref.plugin.as_str() == d.id)
                    && !self
                        .views
                        .iter()
                        .any(|v| v.view_ref.plugin.as_str() == d.id)
            })
            .collect()
    }

    pub fn selected_item(&self) -> Option<&Item> {
        match self.selected_entry()? {
            Entry::Action(i) => self.items.get(i),
            _ => None,
        }
    }

    pub fn selected_view(&self) -> Option<&ViewItem> {
        match self.selected_entry()? {
            Entry::View(i) => self.views.get(i),
            _ => None,
        }
    }

    pub fn selected_oneoff_index(&self) -> Option<usize> {
        match self.selected_entry()? {
            Entry::OneOff(i) if i < self.oneoffs.len() => Some(i),
            _ => None,
        }
    }

    pub fn selected_oneoff(&self) -> Option<&OneOff> {
        self.selected_oneoff_index().map(|i| &self.oneoffs[i])
    }

    /// The selected one-off run can be stopped now.
    pub fn oneoff_stoppable(&self) -> bool {
        self.selected_oneoff().is_some_and(|o| {
            self.control_lost.is_none()
                && !o.stopping
                && matches!(o.lifecycle, Lifecycle::Starting | Lifecycle::Running)
        })
    }

    pub fn selected_ref(&self) -> Option<ActionRef> {
        self.selected_item().map(|i| i.action_ref.clone())
    }

    pub fn selected_view_pane(&self) -> Option<&ViewPane> {
        self.selected_view()
            .and_then(|v| self.view_panes.get(&v.view_ref))
    }

    pub(super) fn entry_key(&self, e: Entry) -> Option<Key> {
        match e {
            Entry::Action(i) => self
                .items
                .get(i)
                .map(|x| Key::Item(x.action_ref.to_item_ref())),
            Entry::View(i) => self
                .views
                .get(i)
                .map(|x| Key::Item(x.view_ref.to_item_ref())),
            Entry::OneOff(i) => self.oneoffs.get(i).map(|o| Key::Run(o.run_id.clone())),
        }
    }

    pub(super) fn selected_key(&self) -> Option<Key> {
        self.selected_entry().and_then(|e| self.entry_key(e))
    }

    /// Lists the entries that match the filter, best match first (see [`rank`]); without a
    /// filter, every entry in plugin order. Matches stay grouped under their plugin: plugins
    /// are ordered by their best match, entries within a plugin by their own rank, so each
    /// plugin heading appears once. One-off runs come first, newest first; while a filter
    /// is set, the matching ones come after the tools so the best tool match stays first.
    /// `keep` stays selected if it still matches.
    pub(super) fn refilter(&mut self, keep: Option<Key>) {
        let words = catalog::query_words(&self.filter);
        let mut out: Vec<(usize, catalog::Rank, Entry)> = Vec::new();
        for (pi, p) in self.plugin_order.iter().enumerate() {
            for (n, i) in self.items.iter().enumerate() {
                if i.action_ref.plugin.as_str() != p {
                    continue;
                }
                let item_ref = i.action_ref.to_string();
                let entry = catalog::Entry {
                    item_ref: &item_ref,
                    id: i.action_ref.action.as_str(),
                    title: &i.title,
                    tags: &i.tags,
                    description: &i.description,
                };
                if let Some(k) = catalog::rank(&words, &entry) {
                    out.push((pi, k, Entry::Action(n)));
                }
            }
            for (n, v) in self.views.iter().enumerate() {
                if v.view_ref.plugin.as_str() != p {
                    continue;
                }
                let item_ref = v.view_ref.to_string();
                // A view's kind word matches like a tag.
                let mut tags = v.tags.clone();
                tags.push(crate::views::kind_word(v.kind).to_owned());
                let entry = catalog::Entry {
                    item_ref: &item_ref,
                    id: v.view_ref.view.as_str(),
                    title: &v.title,
                    tags: &tags,
                    description: &v.description,
                };
                if let Some(k) = catalog::rank(&words, &entry) {
                    out.push((pi, k, Entry::View(n)));
                }
            }
        }
        let mut best: HashMap<usize, catalog::Rank> = HashMap::new();
        for (pi, k, _) in &out {
            best.entry(*pi)
                .and_modify(|b| *b = (*b).min(*k))
                .or_insert(*k);
        }
        // A stable sort keeps the list order among equal matches.
        out.sort_by_key(|(pi, k, _)| (best.get(pi).copied(), *pi, *k));
        let oneoffs = self.oneoffs.iter().enumerate().filter(|(_, o)| {
            let title = o.title();
            let entry = catalog::Entry {
                item_ref: &title,
                id: o.run_id.as_str(),
                title: &title,
                tags: &[],
                description: "",
            };
            catalog::rank(&words, &entry).is_some()
        });
        let oneoffs: Vec<Entry> = oneoffs.map(|(i, _)| Entry::OneOff(i)).collect();
        let tools = out.into_iter().map(|(_, _, e)| e);
        self.visible = if words.is_empty() {
            oneoffs.into_iter().chain(tools).collect()
        } else {
            tools.chain(oneoffs).collect()
        };
        self.selected = keep
            .and_then(|k| {
                self.visible
                    .iter()
                    .position(|&e| self.entry_key(e).as_ref() == Some(&k))
            })
            .unwrap_or(0)
            .min(self.visible.len().saturating_sub(1));
    }

    pub(super) fn on_select(&mut self) {
        self.output_top = 0;
        let sel = self.selected_ref();
        if self
            .notice
            .as_ref()
            .and_then(|n| n.about.as_ref())
            .is_some_and(|(a, _)| Some(a) != sel.as_ref())
        {
            self.notice = None;
        }
        if let Some(i) = self.selected_oneoff_index() {
            let o = &mut self.oneoffs[i];
            if o.pane.is_none() {
                let mut p = LogPane::new();
                p.run_id = Some(o.run_id.clone());
                p.loading = true;
                o.pane = Some(p);
                let _ = self.io.read.send(Read::RunTail(o.run_id.clone()));
            }
            return;
        }
        if let Some(v) = self.selected_view() {
            let r = v.view_ref.clone();
            if !self.view_panes.contains_key(&r) {
                self.load_view(&r);
                let _ = self.io.read.send(Read::DescribeView(r));
            }
            return;
        }
        let Some(item) = self.selected_item() else {
            return;
        };
        let a = item.action_ref.clone();
        let hash = item.definition_hash.clone();
        if self.inputs.get(&a).is_none_or(|(h, _)| *h != hash) {
            self.inputs.insert(a.clone(), (hash, Inputs::Loading));
            let _ = self.io.read.send(Read::Describe(a.clone()));
        }
        self.sync_pane(&a);
    }

    /// Makes the action's panel show the run chosen in the history list, else its current
    /// (or latest) run.
    pub(super) fn sync_pane(&mut self, a: &ActionRef) {
        let want = self
            .viewing
            .get(a)
            .map(|r| r.run_id.clone())
            .or_else(|| self.active.get(a).map(|r| r.run_id.clone()))
            .or_else(|| self.last.get(a).map(|l| l.run_id.clone()));
        self.pane_order.retain(|x| x != a);
        self.pane_order.push_back(a.clone());
        while self.pane_order.len() > MAX_PANES {
            if let Some(old) = self.pane_order.pop_front() {
                self.panes.remove(&old);
            }
        }
        match self.panes.get_mut(a) {
            None => {
                let mut p = LogPane::new();
                p.run_id = want.clone();
                p.loading = true;
                self.panes.insert(a.clone(), p);
                let _ = self.io.read.send(Read::Tail(a.clone(), want));
            }
            Some(p) => {
                if let Some(w) = want
                    && p.run_id.as_ref() != Some(&w)
                {
                    p.reset(Some(w.clone()));
                    p.loading = true;
                    let _ = self.io.read.send(Read::Tail(a.clone(), Some(w)));
                }
            }
        }
    }

    /// The log panel of the selected action or one-off run.
    pub fn selected_pane(&self) -> Option<&LogPane> {
        if let Some(o) = self.selected_oneoff() {
            return o.pane.as_ref();
        }
        self.selected_ref().and_then(|a| self.panes.get(&a))
    }

    pub fn selected_pane_mut(&mut self) -> Option<&mut LogPane> {
        if let Some(i) = self.selected_oneoff_index() {
            return self.oneoffs[i].pane.as_mut();
        }
        let a = self.selected_ref()?;
        self.panes.get_mut(&a)
    }

    /// The tab the main pane shows: `History` while the history list of the selected action
    /// is open, else the chosen tab.
    pub fn shown_tab(&self) -> Tab {
        match &self.modal {
            Modal::History { action_ref, .. }
                if Some(action_ref) == self.selected_ref().as_ref() =>
            {
                Tab::History
            }
            _ => self.tab,
        }
    }

    /// Reads the selected action's newest runs for the right rail when its run state changed
    /// since the last read. Cheap to call on every frame.
    pub fn want_recent(&mut self) {
        let Some(a) = self.selected_ref() else {
            return;
        };
        let key = match (self.active.get(&a), self.last.get(&a)) {
            (Some(r), _) => (Some(r.run_id.clone()), stage(r.lifecycle), false),
            (None, Some(l)) => (Some(l.run_id.clone()), 2, l.ended_at.is_some()),
            (None, None) => (None, 0, false),
        };
        if key.0.is_none() || self.recent_key.get(&a) == Some(&key) {
            return;
        }
        self.recent_key.insert(a.clone(), key);
        let _ = self.io.read.send(Read::History(a));
    }

    /// Moves to `tab`; `History` opens the history list of the selected action.
    pub(super) fn go_tab(&mut self, tab: Tab) {
        if matches!(self.modal, Modal::History { .. }) {
            self.modal = Modal::None;
        }
        match tab {
            Tab::History => self.exec(Cmd::History),
            t => self.tab = t,
        }
    }
}
