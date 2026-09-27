//! The workspace work session: controllers keep it alive; a background lease
//! keeps it alive without controllers until its deadline; observers never do.

use std::collections::HashSet;

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::{ClientId, SessionId};
use mira_protocol::ipc::*;
use mira_protocol::manifest::{ActionMode, TimeoutPolicy, TimeoutWire};
use mira_protocol::reply::ReplyMeta;
use mira_protocol::run::{Lifecycle, StopReason};
use mira_protocol::time::Timestamp;
use tokio::time::Instant;

use super::{Actor, Responder};
use crate::diag;

pub struct Session {
    pub id: SessionId,
    pub controllers: HashSet<ClientId>,
    /// Clients that are controllers only while their waiting task runs.
    pub temporary: HashSet<ClientId>,
    /// Background lease deadline; `Some(None)` means an explicit `ttl: none` lease.
    pub lease: Option<Option<(Instant, Timestamp)>>,
    pub stopping: bool,
    /// Environment captured from the controller that created the session; interval
    /// schedules run with it.
    pub env: ClientEnv,
}

impl Session {
    fn mode(&self) -> SessionMode {
        if self.controllers.is_empty() && self.lease.is_some() {
            SessionMode::Background
        } else {
            SessionMode::Foreground
        }
    }
}

fn lease_from(ttl: TimeoutWire) -> Result<Option<(Instant, Timestamp)>, ErrorInfo> {
    match TimeoutPolicy::from_wire(ttl)
        .map_err(|m| ErrorInfo::new(ErrorCode::INVALID_ARGUMENT, m))?
    {
        TimeoutPolicy::Unlimited => Ok(None),
        TimeoutPolicy::After(d) => {
            let wall = Timestamp::from_unix_ms(Timestamp::now().unix_ms() + d.as_millis() as i64);
            Ok(Some((Instant::now() + d, wall)))
        }
    }
}

impl Actor {
    pub(crate) fn session_info(&self) -> Option<SessionInfo> {
        self.session.as_ref().map(|s| SessionInfo {
            id: s.id.clone(),
            mode: s.mode(),
            controller_count: (s.controllers.len() + s.temporary.len()) as u32,
            background_lease: s.lease.is_some(),
            expires_at: s.lease.flatten().map(|(_, t)| t),
            state: if s.stopping {
                SessionState::Stopping
            } else {
                SessionState::Active
            },
        })
    }

    fn session_reply(&self, r: Responder) {
        r.send(self.ok(
            SessionData {
                session: self.session_info(),
            },
            ReplyMeta::default(),
        ));
    }

    /// Creates a session if none exists. Returns true when a new one was created.
    pub(crate) fn ensure_session(&mut self, env: &ClientEnv) -> Result<bool, ErrorInfo> {
        match &self.session {
            Some(s) if s.stopping => Err(ErrorInfo::new(
                ErrorCode::BUSY,
                "the session is stopping; retry when it has stopped",
            )
            .retryable(true)),
            Some(_) => Ok(false),
            None => {
                self.session = Some(Session {
                    id: SessionId::random(),
                    controllers: HashSet::new(),
                    temporary: HashSet::new(),
                    lease: None,
                    stopping: false,
                    env: env.clone(),
                });
                self.arm_enabled_schedules();
                Ok(true)
            }
        }
    }

    pub(super) fn session_attach(
        &mut self,
        client: &ClientId,
        p: SessionAttachParams,
        r: Responder,
    ) {
        let created = match self.ensure_session(&p.client_env) {
            Ok(c) => c,
            Err(e) => return r.send(self.fail(e)),
        };
        if let Some(s) = self.session.as_mut() {
            s.controllers.insert(client.clone());
            s.temporary.remove(client);
        }
        if created && p.autostart {
            self.run_autostart(client, &p.client_env);
        }
        self.state_changed();
        self.session_reply(r);
    }

    fn run_autostart(&mut self, client: &ClientId, env: &ClientEnv) {
        let Ok(set) = self.accepted() else { return };
        for action_ref in set.workspace.autostart.clone() {
            let enabled_process = set
                .action(&action_ref)
                .is_some_and(|(lp, a)| lp.plugin.enabled && a.mode == ActionMode::Process);
            if !enabled_process {
                continue;
            }
            let p = ActionInvokeParams {
                action_ref,
                input: Default::default(),
                client_env: env.clone(),
                request_key: None,
                foreground: false,
            };
            self.invoke_internal(client, p, None);
        }
    }

    pub(super) fn session_open(&mut self, _client: &ClientId, p: SessionOpenParams, r: Responder) {
        let lease = match lease_from(p.ttl) {
            Ok(l) => l,
            Err(e) => return r.send(self.fail(e)),
        };
        if let Err(e) = self.ensure_session(&p.client_env) {
            return r.send(self.fail(e));
        }
        if let Some(s) = self.session.as_mut() {
            s.lease = Some(lease);
        }
        self.state_changed();
        self.session_reply(r);
    }

    pub(super) fn session_keep(&mut self, p: SessionKeepParams, r: Responder) {
        let lease = match lease_from(p.ttl) {
            Ok(l) => l,
            Err(e) => return r.send(self.fail(e)),
        };
        match self.session.as_mut() {
            Some(s) if !s.stopping => s.lease = Some(lease),
            _ => {
                return r.send(
                    self.fail(
                        ErrorInfo::new(
                            ErrorCode::SESSION_REQUIRED,
                            "no active session to keep in the background",
                        )
                        .with_next_action(
                            &["mira", "up", "--background"],
                            "Create a background session explicitly.",
                        ),
                    ),
                );
            }
        }
        self.state_changed();
        self.session_reply(r);
    }

    pub(super) fn session_stop_request(&mut self, r: Responder) {
        let stopped_session = self.session.as_ref().map(|s| s.id.clone());
        let stopped_runs = if stopped_session.is_some() {
            self.runs
                .values()
                .filter(|run| {
                    run.record.lifecycle.is_active()
                        && !matches!(run.record.lifecycle, Lifecycle::Stopping { .. })
                })
                .count() as u32
        } else {
            0
        };
        self.stop_session(StopReason::SessionClosed);
        r.send(self.ok(
            SessionStopData {
                session: self.session_info(),
                stopped_session,
                stopped_runs,
            },
            ReplyMeta::default(),
        ));
    }

    /// A controller disconnected or detached: stop owned work when it was the last one.
    pub(crate) fn controller_left(&mut self, client: &ClientId) {
        let Some(s) = self.session.as_mut() else {
            return;
        };
        let removed = s.controllers.remove(client) | s.temporary.remove(client);
        if removed {
            self.state_changed();
            self.maybe_close_session();
        }
    }

    pub(crate) fn maybe_close_session(&mut self) {
        let Some(s) = &self.session else { return };
        if !s.stopping && s.controllers.is_empty() && s.temporary.is_empty() && s.lease.is_none() {
            self.stop_session(StopReason::SessionClosed);
        }
    }

    /// Stops every run the session owns and disables new invocations until it ends.
    pub(crate) fn stop_session(&mut self, reason: StopReason) {
        let Some(s) = self.session.as_mut() else {
            return;
        };
        s.stopping = true;
        self.disarm_schedules();
        let ids: Vec<_> = self.runs.keys().cloned().collect();
        for id in ids {
            self.stop_run(&id, reason);
        }
        self.end_session_if_idle();
        self.state_changed();
    }

    pub(crate) fn end_session_if_idle(&mut self) {
        if self.session.as_ref().is_some_and(|s| s.stopping) && self.runs.is_empty() {
            self.session = None;
            self.clear_session_views();
            self.disarm_schedules();
        }
    }

    pub(super) fn session_tick(&mut self) {
        let Some(s) = &self.session else { return };
        if s.stopping {
            return;
        }
        // Wall clock, not monotonic time: macOS monotonic time pauses during sleep, and the
        // lease is an absolute deadline that must be checked after wake.
        if let Some(Some((_, wall))) = s.lease
            && Timestamp::now() >= wall
        {
            diag("background lease expired: stopping the session");
            self.stop_session(StopReason::TtlExpired);
        }
    }

    /// Adds a waiting client as a temporary controller for the duration of its task.
    pub(crate) fn add_temporary_controller(&mut self, client: &ClientId) {
        if let Some(s) = self.session.as_mut()
            && !s.controllers.contains(client)
        {
            s.temporary.insert(client.clone());
        }
    }
}
