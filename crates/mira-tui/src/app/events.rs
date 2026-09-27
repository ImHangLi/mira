//! [`App::handle`]: one event from the input thread, a worker, or the host stream.

use mira_protocol::time::Timestamp;

use crate::ipc::{Event, LogChunk, Read};

use super::{App, Inputs, Modal, Quit, inputs_of};

/// How far back the TUI lists finished one-off runs when it opens.
const ONEOFF_RECENT_MS: i64 = 24 * 60 * 60 * 1000;

impl App {
    pub fn handle(&mut self, ev: Event) {
        // A busy event stream can starve the idle tick; its checks are rate-limited.
        self.tick();
        match ev {
            Event::Input(crossterm::event::Event::Key(k)) => self.key(k),
            Event::Input(crossterm::event::Event::Paste(t)) => {
                if self.term.is_open() {
                    self.term.paste(t)
                } else {
                    self.paste(&t)
                }
            }
            Event::Input(crossterm::event::Event::Mouse(m)) => self.mouse_event(m),
            Event::Input(_) => {}
            Event::Terminal(m) => self.term.handle(m),
            Event::Frame(frame) => self.frame(*frame),
            Event::StreamReset => {
                self.info("event stream reset (this TUI fell behind); state and log tail reloaded");
                self.reload_selected_tail();
            }
            Event::StreamDown(m) => self.stream_issue = Some(format!("event stream lost: {m}")),
            Event::Invoked(a, res, joined) => {
                self.pending.remove(&a);
                if let Modal::Form(f) = &mut self.modal
                    && f.action_ref == a
                {
                    match &res {
                        Ok(_) => self.modal = Modal::None,
                        Err(e) => {
                            f.apply_error(e);
                            self.restart_after.remove(&a);
                            return;
                        }
                    }
                }
                if joined.is_some() {
                    self.attached_to = joined;
                }
                match res {
                    Ok(acc) => {
                        let what = if acc.reused {
                            format!("{a} is already running ({})", acc.run_id)
                        } else {
                            format!("started {a} ({})", acc.run_id)
                        };
                        self.info_run(&a, &acc.run_id, what);
                        self.viewing.remove(&a);
                        if let Some(p) = self.panes.get_mut(&a)
                            && p.run_id.as_ref() != Some(&acc.run_id)
                        {
                            p.reset(Some(acc.run_id.clone()));
                            p.loading = true;
                            let _ = self.io.read.send(Read::Tail(a.clone(), Some(acc.run_id)));
                        }
                    }
                    Err(e) => {
                        self.restart_after.remove(&a);
                        self.error_info(&format!("{a} did not start"), &e);
                    }
                }
            }
            Event::Stopped(a, res) => {
                self.pending.remove(&a);
                match res {
                    Ok(s) => {
                        let what = format!("stopping {a} ({})", s.run_id);
                        self.info_run(&a, &s.run_id, what)
                    }
                    Err(e) => {
                        self.restart_after.remove(&a);
                        self.error_info(&format!("{a} did not stop"), &e);
                    }
                }
            }
            Event::Kept(res) => {
                self.keeping = false;
                match res {
                    Ok(d) => self.quit = Some(Quit::Kept(d.session.and_then(|s| s.expires_at))),
                    Err(e) => self.error_info("could not keep the session", &e),
                }
            }
            Event::Described(a, res) => {
                self.term.note_described(&a, &res);
                let hash = res
                    .as_ref()
                    .map(|d| d.item.definition_hash.clone())
                    .ok()
                    .or_else(|| self.item(&a).map(|i| i.definition_hash.clone()));
                let inputs = match res {
                    Ok(d) => {
                        if d.action.as_ref().is_some_and(|x| x.has_schedule) {
                            self.scheduled.insert(a.clone());
                        }
                        inputs_of(d.action.as_ref().and_then(|x| x.input_schema.as_ref()))
                    }
                    Err(e) => Inputs::Unknown(e.message),
                };
                if let Some(h) = hash {
                    self.inputs.insert(a.clone(), (h, inputs));
                }
                if let Some((qa, intent)) = self.queued.take() {
                    if qa == a {
                        self.intent(&a, intent);
                    } else {
                        self.queued = Some((qa, intent));
                    }
                }
            }
            Event::Tail(a, res) => self.tail(&a, res),
            Event::RunTail(run_id, res) => {
                let Some(p) = self
                    .oneoffs
                    .iter_mut()
                    .find(|o| o.run_id == run_id)
                    .and_then(|o| o.pane.as_mut())
                else {
                    return;
                };
                match res {
                    Ok(LogChunk { page, older_cursor }) => p.apply_tail(page, older_cursor),
                    Err(e) => {
                        p.loading = false;
                        if e.code != mira_protocol::ErrorCode::NOT_FOUND {
                            p.error = Some(e.message);
                        }
                    }
                }
            }
            Event::RunStopped(run_id, res) => {
                let o = self.oneoffs.iter_mut().find(|o| o.run_id == run_id);
                let label = o.as_ref().map_or_else(|| run_id.to_string(), |o| o.title());
                if let (Some(o), Err(_)) = (o, &res) {
                    o.stopping = false;
                }
                match res {
                    Ok(_) => self.info(format!("stopping {label}")),
                    Err(e) => self.error_info(&format!("{label} did not stop"), &e),
                }
            }
            Event::Older(run_id, res) => {
                if let Some(p) = self
                    .panes
                    .values_mut()
                    .chain(self.oneoffs.iter_mut().filter_map(|o| o.pane.as_mut()))
                    .find(|p| p.run_id.as_ref() == Some(&run_id))
                {
                    match res {
                        Ok(LogChunk { page, older_cursor }) => p.apply_older(page, older_cursor),
                        Err(e) => {
                            p.loading_older = false;
                            p.older_cursor = None;
                            p.error = Some(e.message);
                        }
                    }
                }
            }
            Event::Run(Ok(rec)) => {
                self.row_run_ended(&rec);
                self.record(*rec)
            }
            Event::Run(Err(_)) => {}
            Event::Recent(Ok(list)) => {
                // One-off runs that are still running or started in the last day are listed
                // too, whoever started them: an agent's run shows up when the TUI opens.
                let since = Timestamp::now().unix_ms() - ONEOFF_RECENT_MS;
                for rec in list.runs.iter().filter(|r| {
                    r.action_ref.is_none()
                        && (r.lifecycle.is_active() || r.started_at.unix_ms() >= since)
                }) {
                    self.oneoff_record(rec, true);
                }
                for rec in list.runs {
                    if let Some(a) = rec.action_ref.clone()
                        && !self.last.contains_key(&a)
                        && !rec.lifecycle.is_active()
                    {
                        self.record(rec);
                    }
                }
            }
            Event::Recent(Err(_)) => {}
            Event::History(a, res) => {
                if let Ok(list) = &res {
                    self.recent.insert(a.clone(), list.runs.clone());
                }
                if let Modal::History {
                    action_ref,
                    runs,
                    error,
                    index,
                } = &mut self.modal
                    && *action_ref == a
                {
                    match res {
                        Ok(list) => {
                            // Keep the choice on the run whose logs the pane shows.
                            let shown = self.panes.get(&a).and_then(|p| p.run_id.clone());
                            *index = list
                                .runs
                                .iter()
                                .position(|r| Some(&r.run_id) == shown.as_ref())
                                .unwrap_or(0);
                            *runs = Some(list.runs);
                        }
                        Err(e) => *error = Some(format!("[{}] {}", e.code, e.message)),
                    }
                }
            }
            Event::Catalog(Ok(list)) => self.set_catalog(list),
            Event::Catalog(Err(e)) => self.catalog_error = Some(e),
            Event::CatalogChanged => {
                self.inputs.clear();
                let _ = self.io.read.send(Read::Catalog);
            }
            Event::ConnectionLost(which, m) => {
                if which == "control" {
                    self.control_lost = Some(m.clone());
                }
                self.error(format!("host {which} connection lost: {m}"));
            }
            Event::Copied(Ok(m)) => self.info(m),
            Event::Copied(Err(m)) => self.error(m),
            Event::Signal(name) => self.quit = Some(Quit::Normal(Some(name))),
            Event::View(r, res) => {
                let Some(p) = self.view_panes.get_mut(&r) else {
                    return;
                };
                p.loading = false;
                match res {
                    Ok(load) => p.apply(load.snapshot, load.truncated),
                    Err(e) => p.error = Some(format!("[{}] {}", e.code, e.message)),
                }
                if std::mem::take(&mut p.reload) {
                    self.load_view(&r);
                }
            }
            Event::ViewDescribed(r, res) => {
                if let (Some(p), Ok(d)) = (self.view_panes.get_mut(&r), res) {
                    let view = d.view;
                    p.source = view.as_ref().and_then(|v| v.source.clone());
                    p.row_actions = view.map(|v| v.row_actions).unwrap_or_default();
                }
            }
            Event::ViewActed(r, action, res) => match res {
                Ok(acc) => {
                    self.info(format!("running {action} for this row…"));
                    if let Some(rr) = &mut self.row_run
                        && rr.view_ref == r
                        && rr.action == action
                    {
                        rr.run_id = Some(acc.run_id.clone());
                    }
                    // A quick run can end before any state event names it.
                    let _ = self.io.read.send(Read::RunGet(acc.run_id));
                }
                Err(e) if e.code == mira_protocol::ErrorCode::VIEW_CHANGED => {
                    self.row_run = None;
                    if let Some(p) = self.view_panes.get_mut(&r) {
                        p.accept_current();
                    }
                    self.load_view(&r);
                    self.error(format!(
                        "VIEW_CHANGED, nothing was run: {} Showing the current revision; check the row, then press Enter again.",
                        e.message
                    ));
                }
                Err(e) => {
                    self.row_run = None;
                    self.error_info(&format!("{action} did not start"), &e)
                }
            },
            Event::ScheduleSet(a, res) => {
                self.pending.remove(&a);
                match res {
                    Ok(d) => {
                        self.info(format!(
                            "schedule for {a} is {}",
                            if d.enabled { "on" } else { "off" }
                        ));
                        self.status_at = None;
                        self.request_status();
                    }
                    Err(e) => self.error_info(&format!("could not change the {a} schedule"), &e),
                }
            }
            Event::Status(Ok(st)) => {
                self.schedules = st.schedules;
                self.config_warnings = st.config_warnings;
            }
            Event::Status(Err(_)) => {}
            Event::Screen(run_id, line) => match line {
                Some(l) => {
                    self.screens.insert(run_id, l);
                }
                None => {
                    self.screens.remove(&run_id);
                }
            },
            Event::CommandDone(out) => {
                if matches!(self.modal, Modal::Command { .. } | Modal::None) {
                    self.modal = Modal::Output(Box::new(out));
                }
            }
        }
    }
}
