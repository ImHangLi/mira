//! Loading a `.mira`-shaped directory into one validated configuration set.
//!
//! Synchronous std IO only. Nothing here executes plugin or project code.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::{ErrorCode, Issue, Issues};
use crate::hash::canonical_digest;
use crate::ids::{AbsolutePath, ActionRef, Digest, ItemRef, LocalId, PluginId, ViewRef};
use crate::ipc::{CatalogItem, CatalogItemKind};
use crate::limits::MAX_MANIFEST_BYTES;
use crate::manifest::*;

pub const WORKSPACE_FILE: &str = "workspace.json";
pub const PLUGIN_FILE: &str = "plugin.json";
pub const LOCAL_FILE: &str = "local.json";

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedPlugin {
    pub plugin: Plugin,
    /// Canonical plugin directory (the plugin process cwd).
    pub dir: AbsolutePath,
}

/// An accepted, immutable definition set. The actor only builds runs from one of these.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigSet {
    pub workspace: Workspace,
    pub plugins: Vec<LoadedPlugin>,
    pub storage: StoragePolicy,
    pub ui: UiPrefs,
    /// Hash of every definition in the set; equal sets never bump `catalog_revision`.
    pub set_hash: Digest,
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, Issue> {
    let meta = std::fs::metadata(path)
        .map_err(|e| Issue::new(ErrorCode::NOT_FOUND, "", format!("cannot read file: {e}")))?;
    if meta.len() > MAX_MANIFEST_BYTES as u64 {
        return Err(Issue::schema("", "manifest exceeds 1 MiB"));
    }
    std::fs::read(path)
        .map_err(|e| Issue::new(ErrorCode::NOT_FOUND, "", format!("cannot read file: {e}")))
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn canonical_dir(path: &Path) -> Result<AbsolutePath, Issue> {
    let real = std::fs::canonicalize(path).map_err(|e| {
        Issue::new(
            ErrorCode::NOT_FOUND,
            "",
            format!("cannot resolve directory: {e}"),
        )
    })?;
    AbsolutePath::from_path(&real).map_err(|_| {
        Issue::new(
            ErrorCode::UNSUPPORTED_PATH_ENCODING,
            "",
            "path is not valid UTF-8",
        )
    })
}

/// Loads `plugin.json` from `dir`, applying an optional local merge patch before validation.
pub fn load_plugin_dir(dir: &Path, patch: Option<&JsonObject>) -> Result<LoadedPlugin, Issues> {
    let file = dir.join(PLUGIN_FILE);
    let name = display(&file);
    let canonical = canonical_dir(dir).map_err(|i| Issues(vec![i]).in_file(&name))?;
    let bytes = read_bounded(&file).map_err(|i| Issues(vec![i]).in_file(&name))?;
    let mut value = crate::strict_json::parse(&bytes, MAX_MANIFEST_BYTES)
        .map_err(|e| Issues(vec![Issue::schema("", e.to_string())]).in_file(&name))?;
    if let Some(patch) = patch {
        let (id, api) = (value.get("id").cloned(), value.get("api").cloned());
        merge_patch(&mut value, &Value::Object(patch.clone()));
        if value.get("id") != id.as_ref() || value.get("api") != api.as_ref() {
            return Err(Issues(vec![Issue::schema(
                "/id",
                "local plugin_patches must not change the plugin id or api",
            )])
            .in_file(LOCAL_FILE));
        }
    }
    let wire: PluginWire = wire_from_value(value).map_err(|i| i.in_file(&name))?;
    let plugin = validate_plugin(wire).map_err(|i| i.in_file(&name))?;
    let mut issues = Issues::default();
    if let Some(docs) = &plugin.docs
        && !dir.join(docs).is_file()
    {
        issues.push(Issue::schema(
            "/docs",
            "docs file does not exist in the plugin directory",
        ));
    }
    issues.in_file(&name).into_result(LoadedPlugin {
        plugin,
        dir: canonical,
    })
}

/// Reads `local.json` if present.
pub fn load_local(mira_dir: &Path) -> Result<Option<LocalWire>, Issues> {
    let file = mira_dir.join(LOCAL_FILE);
    if !file.exists() {
        return Ok(None);
    }
    let name = display(&file);
    let bytes = read_bounded(&file).map_err(|i| Issues(vec![i]).in_file(&name))?;
    parse_wire::<LocalWire>(&bytes, MAX_MANIFEST_BYTES)
        .map(Some)
        .map_err(|i| i.in_file(&name))
}

/// Loads a `.mira`-shaped directory: `workspace.json` plus every listed plugin.
/// Plugin paths are relative to `mira_dir` unless absolute.
pub fn load_config_dir(mira_dir: &Path, local: Option<&LocalWire>) -> Result<ConfigSet, Issues> {
    let ws_file = mira_dir.join(WORKSPACE_FILE);
    let ws_name = display(&ws_file);
    let bytes = read_bounded(&ws_file).map_err(|i| Issues(vec![i]).in_file(&ws_name))?;
    let wire: WorkspaceWire =
        parse_wire(&bytes, MAX_MANIFEST_BYTES).map_err(|i| i.in_file(&ws_name))?;
    let workspace = validate_workspace(wire).map_err(|i| i.in_file(&ws_name))?;

    let mut issues = Issues::default();
    let mut plugins: Vec<LoadedPlugin> = Vec::new();
    for (i, rel) in workspace.plugins.iter().enumerate() {
        let dir: PathBuf = if rel.starts_with('/') {
            PathBuf::from(rel)
        } else {
            mira_dir.join(rel)
        };
        let patch = local.and_then(|l| {
            let id = peek_plugin_id(&dir)?;
            l.plugin_patches.get(&id)
        });
        match load_plugin_dir(&dir, patch) {
            Ok(p) => {
                if plugins.iter().any(|q| q.plugin.id == p.plugin.id) {
                    issues.push(
                        Issue::schema(
                            format!("/plugins/{i}"),
                            format!("duplicate plugin id `{}`", p.plugin.id),
                        )
                        .in_ws(&ws_name),
                    );
                } else {
                    plugins.push(p);
                }
            }
            Err(e) => issues.extend(e),
        }
    }
    for (i, r) in workspace.autostart.iter().enumerate() {
        let ok = plugins
            .iter()
            .find(|p| p.plugin.id == r.plugin)
            .is_some_and(|p| {
                p.plugin.enabled
                    && p.plugin
                        .action(r.action.as_str())
                        .is_some_and(|a| a.mode == ActionMode::Process)
            });
        if !ok {
            issues.push(
                Issue::schema(
                    format!("/autostart/{i}"),
                    "autostart must reference an enabled process action",
                )
                .in_ws(&ws_name),
            );
        }
    }

    check_view_sources(&plugins, &mut issues);

    let mut storage = StoragePolicy::default();
    let mut ui = workspace.ui.clone();
    if let Some(s) = &workspace.storage_wire {
        storage.apply(s, "/storage", &mut issues);
    }
    if let Some(l) = local {
        if let Some(s) = &l.storage {
            storage.apply(s, "/storage", &mut issues);
        }
        if let Some(u) = &l.ui {
            ui.apply(u);
        }
    }
    if !issues.is_empty() {
        return Err(issues);
    }
    let set_hash = set_digest(&workspace, &plugins)
        .map_err(|e| Issues(vec![Issue::new(ErrorCode::INTERNAL, "", e)]))?;
    Ok(ConfigSet {
        workspace,
        plugins,
        storage,
        ui,
        set_hash,
    })
}

/// A derived log view must name an action that exists in the whole loaded catalog.
fn check_view_sources(plugins: &[LoadedPlugin], issues: &mut Issues) {
    for lp in plugins {
        for (i, v) in lp.plugin.views.iter().enumerate() {
            let Some(src) = &v.source else { continue };
            let target = plugins.iter().find(|q| q.plugin.id == src.logs.plugin);
            let message = match target {
                None => format!(
                    "source.logs `{}` refers to plugin `{}`, which is not in the workspace",
                    src.logs, src.logs.plugin
                ),
                Some(q) if q.plugin.action(src.logs.action.as_str()).is_some() => continue,
                Some(q) if q.plugin.view(src.logs.action.as_str()).is_some() => format!(
                    "source.logs `{}` is a view; it must name an action",
                    src.logs
                ),
                Some(_) => format!("source.logs `{}` names no action in the catalog", src.logs),
            };
            let file = display(&lp.dir.as_path().join(PLUGIN_FILE));
            issues.push(Issue {
                file: Some(file),
                ..Issue::schema(format!("/views/{i}/source/logs"), message)
            });
        }
    }
}

fn peek_plugin_id(dir: &Path) -> Option<PluginId> {
    let bytes = read_bounded(&dir.join(PLUGIN_FILE)).ok()?;
    let v = crate::strict_json::parse(&bytes, MAX_MANIFEST_BYTES).ok()?;
    PluginId::parse(v.get("id")?.as_str()?.to_owned()).ok()
}

trait InWs {
    fn in_ws(self, file: &str) -> Issue;
}
impl InWs for Issue {
    fn in_ws(mut self, file: &str) -> Issue {
        self.file = Some(file.to_owned());
        self
    }
}

fn set_digest(ws: &Workspace, plugins: &[LoadedPlugin]) -> Result<Digest, String> {
    let items: Vec<Value> = plugins
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.plugin.id,
                "dir": p.dir,
                "enabled": p.plugin.enabled,
                "name": p.plugin.name,
                "description": p.plugin.description,
                "tags": p.plugin.tags,
                "actions": p.plugin.actions.iter().map(|a| a.definition_hash.as_str()).collect::<Vec<_>>(),
                "views": p.plugin.views.iter().map(|v| v.definition_hash.as_str()).collect::<Vec<_>>(),
            })
        })
        .collect();
    canonical_digest(&serde_json::json!({
        "name": ws.name, "autostart": ws.autostart, "plugins": items,
    }))
}

impl ConfigSet {
    pub fn plugin(&self, id: &PluginId) -> Option<&LoadedPlugin> {
        self.plugins.iter().find(|p| &p.plugin.id == id)
    }

    pub fn action(&self, r: &ActionRef) -> Option<(&LoadedPlugin, &Action)> {
        let p = self.plugin(&r.plugin)?;
        Some((p, p.plugin.action(r.action.as_str())?))
    }

    pub fn view(&self, r: &ViewRef) -> Option<(&LoadedPlugin, &ViewDefinition)> {
        let p = self.plugin(&r.plugin)?;
        Some((p, p.plugin.view(r.view.as_str())?))
    }

    /// The full catalog, disabled items included, in stable plugin/item order.
    pub fn catalog(&self) -> Vec<CatalogItem> {
        let mut out = Vec::new();
        for lp in &self.plugins {
            let p = &lp.plugin;
            for a in &p.actions {
                out.push(CatalogItem {
                    item_ref: ItemRef::new(p.id.clone(), LocalId::from(a.id.clone())),
                    title: a.title.clone(),
                    description: a.description.clone(),
                    tags: p.tags.clone(),
                    enabled: p.enabled,
                    definition_hash: a.definition_hash.clone(),
                    item: CatalogItemKind::Action {
                        mode: a.mode,
                        show: a.show,
                    },
                });
            }
            for v in &p.views {
                out.push(CatalogItem {
                    item_ref: ItemRef::new(p.id.clone(), LocalId::from(v.id.clone())),
                    title: v.title.clone(),
                    description: if v.description.is_empty() {
                        p.description.clone()
                    } else {
                        v.description.clone()
                    },
                    tags: p.tags.clone(),
                    enabled: p.enabled,
                    definition_hash: v.definition_hash.clone(),
                    item: CatalogItemKind::View { view_kind: v.kind },
                });
            }
        }
        out
    }
}

/// What `mira validate PATH` found at a path.
#[derive(Debug)]
pub enum Draft {
    Workspace(ConfigSet),
    Plugin(LoadedPlugin),
}

/// Validates a draft: a directory with `workspace.json` (a full set), a directory with
/// `plugin.json`, or a direct path to either file. Pure: never executes anything.
pub fn validate_draft(path: &Path) -> Result<Draft, Issues> {
    let (dir, file) = if path.is_file() {
        match path.file_name().and_then(|n| n.to_str()) {
            Some(WORKSPACE_FILE) | Some(PLUGIN_FILE) => (
                path.parent().unwrap_or(Path::new("/")).to_path_buf(),
                path.file_name().map(PathBuf::from),
            ),
            _ => {
                return Err(Issues(vec![Issue::new(
                    ErrorCode::INVALID_ARGUMENT,
                    "",
                    "expected a directory, workspace.json, or plugin.json",
                )])
                .in_file(&display(path)));
            }
        }
    } else {
        (path.to_path_buf(), None)
    };
    let want_ws = file
        .as_deref()
        .is_none_or(|f| f == Path::new(WORKSPACE_FILE))
        && dir.join(WORKSPACE_FILE).is_file();
    if want_ws {
        let local = load_local(&dir)?;
        return load_config_dir(&dir, local.as_ref()).map(Draft::Workspace);
    }
    if dir.join(PLUGIN_FILE).is_file() {
        return load_plugin_dir(&dir, None).map(Draft::Plugin);
    }
    Err(Issues(vec![Issue::new(
        ErrorCode::NOT_FOUND,
        "",
        "no workspace.json or plugin.json found",
    )])
    .in_file(&display(path)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Writes a workspace with `dev` (action `web`, view `feed`) and `watch` (one view).
    fn workspace(watch_view: Value) -> PathBuf {
        let root = std::env::temp_dir().join(format!("mira-config-test-{}", uuid::Uuid::new_v4()));
        let write = |rel: &str, v: Value| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, serde_json::to_vec(&v).unwrap()).unwrap();
        };
        write(
            "workspace.json",
            json!({"api": 1, "name": "t", "plugins": ["plugins/dev", "plugins/watch"]}),
        );
        write(
            "plugins/dev/plugin.json",
            json!({"api": 1, "id": "dev", "name": "Dev", "description": "d",
                "actions": [{"id": "web", "title": "Web", "description": "w", "mode": "process",
                    "run": {"kind": "command", "argv": ["python3", "web.py"]}}],
                "views": [{"id": "feed", "title": "Feed", "kind": "log"}]}),
        );
        write(
            "plugins/watch/plugin.json",
            json!({"api": 1, "id": "watch", "name": "Watch", "description": "w", "views": [watch_view]}),
        );
        root
    }

    fn load(watch_view: Value) -> Result<ConfigSet, Issues> {
        let root = workspace(watch_view);
        let out = load_config_dir(&root, None);
        let _ = std::fs::remove_dir_all(&root);
        out
    }

    fn first_issue(r: Result<ConfigSet, Issues>) -> Issue {
        r.expect_err("expected issues").0.remove(0)
    }

    #[test]
    fn derived_log_view_loads() {
        let set = load(json!({"id": "errors", "title": "Errors", "kind": "log",
            "source": {"logs": "dev.web", "grep": "Error", "stream": "stderr"}}))
        .unwrap();
        let (_, def) = set.view(&"watch.errors".parse().unwrap()).unwrap();
        let src = def.source.as_ref().unwrap();
        assert_eq!(src.logs.to_string(), "dev.web");
        assert!(src.keeps(crate::run::LogStream::Stderr, "an ERROR here"));
        assert!(!src.keeps(crate::run::LogStream::Stdout, "an error here"));
        assert!(!src.keeps(crate::run::LogStream::Stderr, "all fine"));
    }

    #[test]
    fn derived_source_must_exist_and_be_an_action() {
        let missing = first_issue(load(json!({"id": "errors", "title": "E", "kind": "log",
            "source": {"logs": "dev.api"}})));
        assert_eq!(missing.pointer, "/views/0/source/logs");
        assert!(missing.message.contains("names no action"));
        let plugin = first_issue(load(json!({"id": "errors", "title": "E", "kind": "log",
            "source": {"logs": "nope.web"}})));
        assert!(plugin.message.contains("not in the workspace"));
        let view = first_issue(load(json!({"id": "errors", "title": "E", "kind": "log",
            "source": {"logs": "dev.feed"}})));
        assert!(view.message.contains("is a view"));
    }

    #[test]
    fn derived_source_is_only_for_log_views() {
        let issue = first_issue(load(json!({"id": "errors", "title": "E", "kind": "table",
            "source": {"logs": "dev.web"}})));
        assert_eq!(issue.pointer, "/views/0/source");
        let empty = first_issue(load(json!({"id": "errors", "title": "E", "kind": "log",
            "source": {"logs": "dev.web", "grep": ""}})));
        assert_eq!(empty.pointer, "/views/0/source/grep");
        let unknown = first_issue(load(json!({"id": "errors", "title": "E", "kind": "log",
            "source": {"logs": "dev.web", "regex": "x"}})));
        assert!(unknown.message.contains("unknown field"));
    }

    #[test]
    fn argv_placeholders_must_name_input_properties() {
        let wire: PluginWire = wire_from_value(json!({"api": 1, "id": "p", "name": "P",
            "description": "d", "actions": [{"id": "serve", "title": "S", "description": "s",
            "mode": "process", "run": {"kind": "command", "argv": ["serve", "--port={input.port}", "{input.host}"]},
            "input_schema": {"type": "object", "properties": {"port": {"type": "integer"}}}}]}))
        .unwrap();
        let issues = validate_plugin(wire).unwrap_err();
        assert_eq!(issues.0.len(), 1);
        assert_eq!(issues.0[0].pointer, "/actions/0/run/argv/2");
        assert!(issues.0[0].message.contains("{input.host}"));
    }
}
