//! Parsing and validation from wire DTOs into domain types.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::Value;

use crate::error::{ErrorCode, Issue, Issues, pointer_token};
use crate::hash::canonical_digest;
use crate::limits::*;
use crate::schema_profile::SchemaDoc;
use crate::strict_json;

use super::JsonObject;
use super::domain::{
    Action, Argv, Plugin, RowAction, Runner, Schedule, SchemaDocSer, TimeoutPolicy, UiPrefs,
    ViewDefinition, ViewSource, Workspace,
};
use super::wire::{
    ActionMode, ActionWire, CommandRunnerWire, PluginWire, RunnerWire, StoragePolicy, TerminalMode,
    ViewKind, WorkspaceWire,
};

/// Bytes → strict JSON → wire DTO, with JSON Pointer locations for serde errors.
pub fn parse_wire<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
    max_bytes: usize,
) -> Result<T, Issues> {
    let value = strict_json::parse(bytes, max_bytes).map_err(|e| {
        let code = match e {
            strict_json::JsonError::TooLarge(_) => ErrorCode::FRAME_TOO_LARGE,
            _ => ErrorCode::SCHEMA_INVALID,
        };
        Issues(vec![Issue::new(code, "", e.to_string())])
    })?;
    wire_from_value(value)
}

/// JSON value → wire DTO. An `api` other than 1 reports UNSUPPORTED_API first.
pub fn wire_from_value<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, Issues> {
    if let Some(api) = value.get("api")
        && api.as_u64() != Some(1)
    {
        return Err(Issues(vec![Issue::new(
            ErrorCode::UNSUPPORTED_API,
            "/api",
            "unsupported api version; expected 1",
        )]));
    }
    serde_path_to_error::deserialize(value).map_err(|e| {
        let pointer = path_to_pointer(e.path());
        Issues(vec![Issue::schema(pointer, e.into_inner().to_string())])
    })
}

fn path_to_pointer(path: &serde_path_to_error::Path) -> String {
    use serde_path_to_error::Segment;
    let mut out = String::new();
    for seg in path.iter() {
        match seg {
            Segment::Seq { index } => out.push_str(&format!("/{index}")),
            Segment::Map { key } => out.push_str(&format!("/{}", pointer_token(key))),
            Segment::Enum { .. } | Segment::Unknown => {}
        }
    }
    out
}

fn check_text(s: &str, max: usize, required: bool, pointer: &str, issues: &mut Issues) {
    if required && s.is_empty() {
        issues.push(Issue::schema(pointer, "must not be empty"));
    }
    if s.len() > max {
        issues.push(Issue::schema(pointer, format!("exceeds {max} bytes")));
    }
}

fn check_object_size(m: &JsonObject, max: usize, pointer: &str, issues: &mut Issues) {
    if serde_json::to_vec(m).map(|v| v.len()).unwrap_or(usize::MAX) > max {
        issues.push(Issue::schema(pointer, format!("exceeds {max} bytes")));
    }
}

fn check_rel_path(s: &str, pointer: &str, allow_absolute: bool, issues: &mut Issues) {
    if s.is_empty() || s.contains('\0') {
        issues.push(Issue::schema(pointer, "path must be non-empty without NUL"));
    } else if s.starts_with('/') && !allow_absolute {
        issues.push(Issue::schema(pointer, "path must be relative"));
    }
}

fn check_env(env: &BTreeMap<String, String>, pointer: &str, issues: &mut Issues) {
    for (k, v) in env {
        let p = format!("{pointer}/{}", pointer_token(k));
        if k.is_empty() || k.contains('=') || k.contains('\0') {
            issues.push(Issue::schema(
                p.clone(),
                "invalid environment variable name",
            ));
        }
        if k.starts_with("MIRA_") {
            issues.push(Issue::schema(
                p.clone(),
                "MIRA_* variables are reserved by the host",
            ));
        }
        if v.contains('\0') {
            issues.push(Issue::schema(p, "environment values must not contain NUL"));
        }
    }
}

fn check_schema_doc(
    m: &Option<JsonObject>,
    pointer: &str,
    object_root: bool,
    issues: &mut Issues,
) -> Option<SchemaDoc> {
    let m = m.as_ref()?;
    match SchemaDoc::check(m, pointer, object_root) {
        Ok(doc) => Some(doc),
        Err(e) => {
            issues.extend(e);
            None
        }
    }
}

/// Validates a plugin wire DTO into the domain type.
pub fn validate_plugin(w: PluginWire) -> Result<Plugin, Issues> {
    let mut issues = Issues::default();
    check_text(&w.name, MAX_NAME_BYTES, true, "/name", &mut issues);
    check_text(
        &w.description,
        MAX_DESCRIPTION_BYTES,
        true,
        "/description",
        &mut issues,
    );
    if w.tags.len() > MAX_TAGS {
        issues.push(Issue::schema("/tags", "at most 12 tags"));
    }
    for (i, t) in w.tags.iter().enumerate() {
        check_text(t, MAX_TAG_BYTES, true, &format!("/tags/{i}"), &mut issues);
    }
    if w.actions.is_empty() && w.views.is_empty() {
        issues.push(Issue::schema(
            "",
            "a plugin needs at least one action or view",
        ));
    }
    if w.actions.len() > MAX_ACTIONS {
        issues.push(Issue::schema("/actions", "at most 128 actions"));
    }
    if w.views.len() > MAX_VIEWS {
        issues.push(Issue::schema("/views", "at most 64 views"));
    }
    let entry = w
        .entry
        .as_ref()
        .and_then(|e| Argv::parse(e.argv.clone(), "/entry/argv", &mut issues));
    let config_schema = check_schema_doc(&w.config_schema, "/config_schema", false, &mut issues);
    if let (Some(doc), true) = (&config_schema, issues.is_empty())
        && let Ok(v) = doc.compile()
    {
        issues.extend(doc.validate(&v, &Value::Object(w.config.clone()), "/config"));
    }
    check_object_size(&w.meta, MAX_META_BYTES, "/meta", &mut issues);
    if let Some(d) = &w.docs {
        check_rel_path(d, "/docs", false, &mut issues);
        if d.split('/').any(|seg| seg == "..") {
            issues.push(Issue::schema(
                "/docs",
                "docs must stay inside the plugin directory",
            ));
        }
    }

    let mut names = BTreeSet::new();
    for (i, a) in w.actions.iter().enumerate() {
        if !names.insert(a.id.as_str().to_owned()) {
            issues.push(Issue::schema(
                format!("/actions/{i}/id"),
                "duplicate action or view id",
            ));
        }
    }
    for (i, v) in w.views.iter().enumerate() {
        if !names.insert(v.id.as_str().to_owned()) {
            issues.push(Issue::schema(
                format!("/views/{i}/id"),
                "duplicate action or view id",
            ));
        }
    }

    let mut actions = Vec::new();
    for (i, a) in w.actions.iter().enumerate() {
        if let Some(action) = validate_action(
            a,
            &w,
            entry.is_some(),
            &format!("/actions/{i}"),
            &mut issues,
        ) {
            actions.push(action);
        }
    }
    let mut views = Vec::new();
    for (i, v) in w.views.iter().enumerate() {
        let p = format!("/views/{i}");
        check_text(
            &v.title,
            MAX_NAME_BYTES,
            true,
            &format!("{p}/title"),
            &mut issues,
        );
        check_text(
            &v.description,
            MAX_DESCRIPTION_BYTES,
            false,
            &format!("{p}/description"),
            &mut issues,
        );
        if !v.row_actions.is_empty() && v.kind != ViewKind::Table {
            issues.push(Issue::schema(
                format!("{p}/row_actions"),
                "row_actions are only allowed on table views",
            ));
        }
        let source = v.source.as_ref().map(|src| {
            if v.kind != ViewKind::Log {
                issues.push(Issue::schema(
                    format!("{p}/source"),
                    "source is only allowed on log views",
                ));
            }
            if let Some(g) = &src.grep {
                check_text(
                    g,
                    MAX_NAME_BYTES,
                    true,
                    &format!("{p}/source/grep"),
                    &mut issues,
                );
            }
            ViewSource {
                logs: src.logs.as_action(),
                grep: src.grep.clone(),
                stream: src.stream,
            }
        });
        let mut bound = Vec::new();
        for (j, ra) in v.row_actions.iter().enumerate() {
            match w.actions.iter().find(|a| a.id == ra.action) {
                Some(a) => bound.push(a),
                None => issues.push(Issue::schema(
                    format!("{p}/row_actions/{j}/action"),
                    "row action refers to an unknown action",
                )),
            }
            for (k, col) in &ra.bindings {
                if k.is_empty() || col.is_empty() {
                    issues.push(Issue::schema(
                        format!("{p}/row_actions/{j}/bindings"),
                        "binding names and column ids must be non-empty",
                    ));
                }
            }
        }
        let hash = canonical_digest(&serde_json::json!({
            "view": v, "row_actions": bound, "config": w.config, "plugin": w.id,
        }));
        match hash {
            Ok(definition_hash) => views.push(ViewDefinition {
                id: v.id.clone(),
                title: v.title.clone(),
                kind: v.kind,
                description: v.description.clone(),
                persistence: v.persistence,
                row_actions: v
                    .row_actions
                    .iter()
                    .map(|r| RowAction {
                        action: r.action.clone(),
                        bindings: r.bindings.clone(),
                    })
                    .collect(),
                source,
                meta: v.meta.clone(),
                definition_hash,
            }),
            Err(e) => issues.push(Issue::new(ErrorCode::INTERNAL, p, e)),
        }
    }

    let plugin = Plugin {
        id: w.id.clone(),
        name: w.name.clone(),
        description: w.description.clone(),
        enabled: w.enabled,
        tags: w.tags.clone(),
        entry,
        actions,
        views,
        config: w.config.clone(),
        config_schema,
        docs: w.docs.clone(),
        meta: w.meta.clone(),
    };
    issues.into_result(plugin)
}

fn validate_action(
    a: &ActionWire,
    plugin: &PluginWire,
    has_entry: bool,
    p: &str,
    issues: &mut Issues,
) -> Option<Action> {
    let before = issues.0.len();
    check_text(
        &a.title,
        MAX_NAME_BYTES,
        true,
        &format!("{p}/title"),
        issues,
    );
    check_text(
        &a.description,
        MAX_DESCRIPTION_BYTES,
        true,
        &format!("{p}/description"),
        issues,
    );
    let run = match &a.run {
        RunnerWire::Command { argv } => Argv::parse(argv.clone(), &format!("{p}/run/argv"), issues)
            .map(|argv| Runner::Command { argv }),
        RunnerWire::Plugin => {
            if !has_entry {
                issues.push(Issue::schema(
                    format!("{p}/run"),
                    "a plugin runner needs the plugin `entry`",
                ));
            }
            Some(Runner::Plugin)
        }
    };
    check_rel_path(&a.cwd, &format!("{p}/cwd"), true, issues);
    if a.env_files.len() > MAX_ENV_FILES {
        issues.push(Issue::schema(
            format!("{p}/env_files"),
            "at most 16 env files",
        ));
    }
    for (i, f) in a.env_files.iter().enumerate() {
        check_rel_path(f, &format!("{p}/env_files/{i}"), true, issues);
    }
    check_env(&a.env, &format!("{p}/env"), issues);
    let input_schema = match &a.input_schema {
        Some(_) => check_schema_doc(&a.input_schema, &format!("{p}/input_schema"), true, issues),
        None => Some(SchemaDoc::empty_object()),
    };
    if let (RunnerWire::Command { argv }, Some(doc)) = (&a.run, &input_schema) {
        crate::template::check_argv(argv, doc, &format!("{p}/run/argv"), issues);
    }
    let output_schema = check_schema_doc(
        &a.output_schema,
        &format!("{p}/output_schema"),
        false,
        issues,
    );
    let timeout = match TimeoutPolicy::resolve(a.mode, a.timeout) {
        Ok(t) => Some(t),
        Err(m) => {
            issues.push(Issue::schema(format!("{p}/timeout"), m));
            None
        }
    };
    if a.terminal == TerminalMode::Pty && !matches!(a.run, RunnerWire::Command { .. }) {
        issues.push(Issue::schema(
            format!("{p}/terminal"),
            "pty is only allowed with the command runner",
        ));
    }
    let (lo, hi) = STOP_GRACE_RANGE_MS;
    if !(lo..=hi).contains(&a.stop_grace_ms) {
        issues.push(Issue::schema(
            format!("{p}/stop_grace_ms"),
            "must be between 100 and 60000",
        ));
    }
    let cleanup = a.cleanup.as_ref().and_then(|c| match c {
        CommandRunnerWire::Command { argv } => {
            Argv::parse(argv.clone(), &format!("{p}/cleanup/argv"), issues)
        }
    });
    let schedule = match &a.schedule {
        None => None,
        Some(s) => {
            if a.mode != ActionMode::Task {
                issues.push(Issue::schema(
                    format!("{p}/schedule"),
                    "schedules are only allowed on task actions",
                ));
            }
            if s.every_ms < MIN_SCHEDULE_EVERY_MS || s.every_ms > MAX_SAFE_INTEGER {
                issues.push(Issue::schema(
                    format!("{p}/schedule/every_ms"),
                    "every_ms must be at least 1000",
                ));
            }
            if let Some(doc) = &input_schema
                && let Ok(v) = doc.compile()
            {
                let eff = Value::Object(doc.effective_input(&s.params));
                issues.extend(doc.validate(&v, &eff, &format!("{p}/schedule/params")));
            }
            Some(Schedule {
                every_ms: s.every_ms,
                params: s.params.clone(),
                run_on_start: s.run_on_start,
            })
        }
    };
    if issues.0.len() != before {
        return None;
    }
    let definition_hash = canonical_digest(&serde_json::json!({
        "action": a, "config": plugin.config, "plugin": plugin.id, "entry": plugin.entry,
    }));
    let definition_hash = match definition_hash {
        Ok(h) => h,
        Err(e) => {
            issues.push(Issue::new(ErrorCode::INTERNAL, p, e));
            return None;
        }
    };
    Some(Action {
        id: a.id.clone(),
        title: a.title.clone(),
        description: a.description.clone(),
        mode: a.mode,
        run: run?,
        cwd: a.cwd.clone(),
        env_files: a.env_files.clone(),
        env: a.env.clone(),
        input_schema: SchemaDocSer(input_schema?),
        output_schema: output_schema.map(SchemaDocSer),
        timeout: timeout?,
        terminal: a.terminal,
        stop_signal: a.stop_signal,
        stop_grace_ms: a.stop_grace_ms,
        cleanup,
        schedule,
        effects: a.effects.clone(),
        meta: a.meta.clone(),
        definition_hash,
    })
}

/// Validates a workspace wire DTO. Autostart targets are checked against plugins in `config`.
pub fn validate_workspace(w: WorkspaceWire) -> Result<Workspace, Issues> {
    let mut issues = Issues::default();
    check_text(&w.name, MAX_NAME_BYTES, true, "/name", &mut issues);
    if w.plugins.len() > MAX_PLUGINS {
        issues.push(Issue::schema("/plugins", "at most 256 plugins"));
    }
    let mut seen = BTreeSet::new();
    for (i, p) in w.plugins.iter().enumerate() {
        check_rel_path(p, &format!("/plugins/{i}"), true, &mut issues);
        if !seen.insert(p) {
            issues.push(Issue::schema(
                format!("/plugins/{i}"),
                "duplicate plugin path",
            ));
        }
    }
    check_object_size(&w.meta, MAX_META_BYTES, "/meta", &mut issues);
    if let Some(s) = &w.storage {
        StoragePolicy::default().apply(s, "/storage", &mut issues);
    }
    let mut ui = UiPrefs::default();
    if let Some(u) = &w.ui {
        ui.apply(u);
    }
    issues.into_result(Workspace {
        name: w.name,
        plugins: w.plugins,
        autostart: w.autostart,
        ui,
        storage_wire: w.storage,
        meta: w.meta,
    })
}
