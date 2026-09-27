//! Validating an action invocation or an ad-hoc command into a [`Prepared`] run.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::hash::canonical_digest;
use mira_protocol::ids::*;
use mira_protocol::ipc::*;
use mira_protocol::manifest::{
    Action, ActionMode, Argv, Runner, StopSignal, TerminalMode, TimeoutPolicy,
};
use mira_protocol::run::*;
use serde_json::{Map, Value, json};

use crate::actor::plugin::MppRun;
use crate::actor::{Actor, Responder};
use crate::diag;
use crate::storage::KeyScope;

use super::{Launch, Prepared};

impl Actor {
    fn validator(&mut self, action: &Action) -> Result<Arc<jsonschema::Validator>, ErrorInfo> {
        if let Some(v) = self.validators.get(&action.definition_hash) {
            return Ok(v.clone());
        }
        let v = Arc::new(
            action
                .input_schema
                .0
                .compile()
                .map_err(|e| ErrorInfo::new(ErrorCode::SCHEMA_INVALID, e))?,
        );
        self.validators
            .insert(action.definition_hash.clone(), v.clone());
        Ok(v)
    }

    pub(in crate::actor) fn invoke(
        &mut self,
        client: &ClientId,
        p: ActionInvokeParams,
        r: Responder,
    ) {
        self.invoke_internal(client, p, Some(r));
    }

    pub(crate) fn invoke_internal(
        &mut self,
        client: &ClientId,
        p: ActionInvokeParams,
        r: Option<Responder>,
    ) {
        match self.prepare_action(&p) {
            Ok(prepared) => self.start(client, prepared, r),
            Err(e) => self.fail_opt(r, e),
        }
    }

    /// Invocations started by the host itself (schedules, autostart) rather than a client.
    pub(crate) fn invoke_from(&mut self, source: RunSource, p: ActionInvokeParams) {
        match self.prepare_action(&p) {
            Ok(mut prepared) => {
                prepared.source = Some(source);
                self.start(&ClientId::random(), prepared, None);
            }
            Err(e) => diag(format!(
                "{source:?} invocation of {} refused: {}",
                p.action_ref, e.message
            )),
        }
    }

    fn prepare_action(&mut self, p: &ActionInvokeParams) -> Result<Prepared, ErrorInfo> {
        let set = self.accepted()?;
        let (lp, action) = set
            .action(&p.action_ref)
            .ok_or_else(|| ErrorInfo::item_not_found("action", &p.action_ref))?;
        self.check_blocked(&lp.plugin.id)?;
        if !lp.plugin.enabled {
            return Err(ErrorInfo::new(
                ErrorCode::INVALID_ARGUMENT,
                format!("plugin `{}` is disabled", lp.plugin.id),
            ));
        }
        let (argv, protocol, mpp) = match &action.run {
            Runner::Command { argv } => (argv.as_slice().to_vec(), None, None),
            Runner::Plugin => {
                let entry = lp.plugin.entry.as_ref().ok_or_else(|| {
                    ErrorInfo::new(
                        ErrorCode::EXECUTION_FAILED,
                        format!("plugin `{}` declares no entry to run", lp.plugin.id),
                    )
                })?;
                (
                    entry.as_slice().to_vec(),
                    Some((action.id.clone(), action.mode)),
                    Some(MppRun::new(
                        action.mode,
                        lp.plugin.id.clone(),
                        action.output_schema.as_ref().map(|s| s.0.clone()),
                    )),
                )
            }
        };
        let schema = &action.input_schema.0;
        let effective = schema.effective_input(&p.input);
        let validator = self.validator(action)?;
        let issues = schema.validate(&validator, &Value::Object(effective.clone()), "/input");
        if !issues.is_empty() {
            return Err(issues.to_error_info());
        }
        // `{input.NAME}` placeholders in a command argv (the plugin entry is never templated).
        let argv = match &action.run {
            Runner::Command { .. } => {
                let rendered = mira_protocol::template::render_argv(&argv, &effective)
                    .map_err(|i| i.to_error_info())?;
                let mut issues = mira_protocol::Issues::default();
                Argv::parse(rendered, "/input", &mut issues)
                    .ok_or_else(|| issues.to_error_info())?
                    .as_slice()
                    .to_vec()
            }
            Runner::Plugin => argv,
        };
        let storage = self.storage.as_ref().map_err(|e| e.to_error_info())?;
        let fingerprint = storage
            .fingerprint(&json!({"input": effective, "definition_hash": action.definition_hash}));
        Ok(Prepared {
            source: None,
            action_ref: Some(p.action_ref.clone()),
            label: action.title.clone(),
            mode: action.mode,
            definition_hash: action.definition_hash.clone(),
            fingerprint,
            request_key: p.request_key.clone(),
            scope: KeyScope::Action(p.action_ref.clone()),
            foreground: p.foreground,
            cleanup_configured: action.cleanup.is_some(),
            launch: Launch {
                argv,
                cwd: action.cwd.clone(),
                env_files: action.env_files.clone(),
                action_env: action.env.clone(),
                client_env: p.client_env.clone(),
                timeout: action.timeout,
                stop_signal: action.stop_signal,
                grace: Duration::from_millis(action.stop_grace_ms),
                cleanup: action.cleanup.as_ref().map(|c| c.as_slice().to_vec()),
                terminal: action.terminal,
                input: effective,
                config: lp.plugin.config.clone(),
                plugin: Some((lp.plugin.id.clone(), lp.dir.clone())),
                protocol,
            },
            mpp,
        })
    }

    pub(in crate::actor) fn exec(&mut self, client: &ClientId, p: ActionExecParams, r: Responder) {
        let prepared = (|| {
            if p.label.is_empty() || p.label.len() > mira_protocol::limits::MAX_NAME_BYTES {
                return Err(ErrorInfo::new(
                    ErrorCode::INVALID_ARGUMENT,
                    "--label must be 1-128 bytes",
                ));
            }
            let mut issues = mira_protocol::Issues::default();
            let argv = Argv::parse(p.argv.clone(), "/argv", &mut issues)
                .ok_or_else(|| issues.to_error_info())?;
            let storage = self.storage.as_ref().map_err(|e| e.to_error_info())?;
            let definition_hash = canonical_digest(&json!({"exec": argv.as_slice()}))
                .map_err(|e| ErrorInfo::new(ErrorCode::INTERNAL, e))?;
            let fingerprint = storage.fingerprint(&json!({"exec": argv.as_slice()}));
            Ok(Prepared {
                source: None,
                action_ref: None,
                label: p.label.clone(),
                mode: ActionMode::Task,
                definition_hash,
                fingerprint,
                request_key: p.request_key.clone(),
                scope: KeyScope::Exec,
                foreground: p.foreground,
                cleanup_configured: false,
                launch: Launch {
                    argv: argv.as_slice().to_vec(),
                    cwd: ".".into(),
                    env_files: vec![],
                    action_env: BTreeMap::new(),
                    client_env: p.client_env.clone(),
                    timeout: TimeoutPolicy::After(Duration::from_millis(
                        mira_protocol::limits::DEFAULT_TASK_TIMEOUT_MS,
                    )),
                    stop_signal: StopSignal::Term,
                    grace: Duration::from_millis(mira_protocol::limits::DEFAULT_STOP_GRACE_MS),
                    cleanup: None,
                    terminal: TerminalMode::Pipe,
                    input: Map::new(),
                    config: Map::new(),
                    plugin: None,
                    protocol: None,
                },
                mpp: None,
            })
        })();
        match prepared {
            Ok(prep) => self.start(client, prep, Some(r)),
            Err(e) => r.send(self.fail(e)),
        }
    }
}
