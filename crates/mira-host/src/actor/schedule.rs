//! Deterministic interval schedules (§5.2, §6.4): explicit on/off, only inside the current
//! session, never overlapping, and no catch-up of ticks missed while busy or asleep.

use std::collections::HashMap;
use std::time::Duration;

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::{ActionRef, ClientId};
use mira_protocol::ipc::*;
use mira_protocol::manifest::ActionMode;
use mira_protocol::reply::ReplyMeta;
use mira_protocol::run::RunSource;
use mira_protocol::time::Timestamp;
use tokio::time::Instant;

use super::{Actor, Msg, Responder, reply_fail, reply_ok};

#[derive(Default)]
pub struct Schedules {
    /// Persisted on/off switches.
    pub enabled: HashMap<ActionRef, bool>,
    /// Armed schedules of the current session.
    pub armed: HashMap<ActionRef, Armed>,
}

pub struct Armed {
    every: Duration,
    next_at: Instant,
    next_wall: Timestamp,
    /// Environment of the controller that enabled it (or the session's), this session only.
    env: ClientEnv,
    pub missed: u64,
}

fn wall_after(d: Duration) -> Timestamp {
    Timestamp::from_unix_ms(Timestamp::now().unix_ms() + d.as_millis() as i64)
}

impl Actor {
    pub(super) async fn load_schedules(&mut self) {
        if let Ok(storage) = &self.storage {
            match storage.list_schedules().await {
                Ok(rows) => self.schedules.enabled = rows.into_iter().collect(),
                Err(e) => self.storage_warning(&e),
            }
        }
    }

    fn schedule_spec(&self, action_ref: &ActionRef) -> Result<(Duration, bool), ErrorInfo> {
        let set = self.accepted()?;
        let (_, action) = set
            .action(action_ref)
            .ok_or_else(|| ErrorInfo::item_not_found("action", action_ref))?;
        match (&action.schedule, action.mode) {
            (Some(s), ActionMode::Task) => Ok((Duration::from_millis(s.every_ms), s.run_on_start)),
            _ => Err(ErrorInfo::new(
                ErrorCode::INVALID_ARGUMENT,
                format!("`{action_ref}` declares no interval schedule"),
            )),
        }
    }

    fn arm(&mut self, action_ref: &ActionRef, env: ClientEnv) {
        let Ok((every, run_on_start)) = self.schedule_spec(action_ref) else {
            return;
        };
        let first = if run_on_start { Duration::ZERO } else { every };
        self.schedules.armed.insert(
            action_ref.clone(),
            Armed {
                every,
                next_at: Instant::now() + first,
                next_wall: wall_after(first),
                env,
                missed: 0,
            },
        );
    }

    /// A new session arms every enabled schedule with the session's starting environment.
    pub(crate) fn arm_enabled_schedules(&mut self) {
        let Some(env) = self.session.as_ref().map(|s| s.env.clone()) else {
            return;
        };
        let enabled: Vec<ActionRef> = self
            .schedules
            .enabled
            .iter()
            .filter(|(_, on)| **on)
            .map(|(a, _)| a.clone())
            .collect();
        for a in enabled {
            self.arm(&a, env.clone());
        }
    }

    pub(crate) fn disarm_schedules(&mut self) {
        self.schedules.armed.clear();
    }

    pub(crate) fn schedule_data(&self) -> Vec<ScheduleData> {
        let mut out: Vec<ScheduleData> = self
            .schedules
            .enabled
            .iter()
            .filter_map(|(a, on)| {
                let (every, _) = self.schedule_spec(a).ok()?;
                let armed = self.schedules.armed.get(a);
                Some(ScheduleData {
                    action_ref: a.clone(),
                    enabled: *on,
                    every_ms: every.as_millis() as u64,
                    missed_ticks: armed.map_or(0, |x| x.missed),
                    next_at: armed.map(|x| x.next_wall),
                })
            })
            .collect();
        out.sort_by(|a, b| a.action_ref.cmp(&b.action_ref));
        out
    }

    pub(super) fn schedule_set(&mut self, client: &ClientId, p: ScheduleSetParams, r: Responder) {
        if let Err(e) = self.schedule_spec(&p.action_ref) {
            return r.send(self.fail(e));
        }
        let Ok(storage) = self.storage.clone() else {
            return r.send(self.fail(ErrorInfo::new(
                ErrorCode::STORAGE_UNAVAILABLE,
                "cannot persist the schedule switch",
            )));
        };
        let tx = self.tx.clone();
        let _ = client;
        tokio::spawn(async move {
            let saved = storage
                .set_schedule(p.action_ref.clone(), p.enabled)
                .await
                .map_err(|e| e.to_error_info());
            let _ = tx
                .send(Msg::ScheduleSaved {
                    params: p,
                    saved,
                    responder: r,
                })
                .await;
        });
    }

    pub(super) fn schedule_saved(
        &mut self,
        p: ScheduleSetParams,
        saved: Result<(), ErrorInfo>,
        r: Responder,
    ) {
        if let Err(e) = saved {
            return r.send(self.fail(e));
        }
        self.schedules
            .enabled
            .insert(p.action_ref.clone(), p.enabled);
        if !p.enabled {
            self.schedules.armed.remove(&p.action_ref);
        } else if self.session.as_ref().is_some_and(|s| !s.stopping) {
            self.arm(&p.action_ref, p.client_env.clone());
        }
        self.state_changed();
        let data = self
            .schedule_data()
            .into_iter()
            .find(|d| d.action_ref == p.action_ref);
        let ctx = self.ctx();
        r.send(match data {
            Some(d) => reply_ok(ctx, d, ReplyMeta::default()),
            None => reply_fail(
                ctx,
                ErrorInfo::new(ErrorCode::INTERNAL, "schedule state missing after update"),
            ),
        });
    }

    /// Fires due schedules. A tick is skipped (and counted) while the previous run is active;
    /// ticks missed during sleep are not replayed.
    pub(super) fn schedule_tick(&mut self) {
        if self.session.as_ref().is_none_or(|s| s.stopping) || self.schedules.armed.is_empty() {
            return;
        }
        let now = Instant::now();
        let due: Vec<ActionRef> = self
            .schedules
            .armed
            .iter()
            .filter(|(_, a)| a.next_at <= now)
            .map(|(r, _)| r.clone())
            .collect();
        for action_ref in due {
            let busy = self.by_action.contains_key(&action_ref);
            let params = self.accepted().ok().and_then(|s| {
                s.action(&action_ref)
                    .and_then(|(_, a)| a.schedule.as_ref().map(|x| x.params.clone()))
            });
            let Some(armed) = self.schedules.armed.get_mut(&action_ref) else {
                continue;
            };
            armed.next_at = now + armed.every;
            armed.next_wall = wall_after(armed.every);
            if busy {
                armed.missed += 1;
                continue;
            }
            let client_env = armed.env.clone();
            let p = ActionInvokeParams {
                action_ref: action_ref.clone(),
                input: params.unwrap_or_default(),
                client_env,
                request_key: None,
                foreground: false,
            };
            self.invoke_from(RunSource::Schedule, p);
        }
    }
}
