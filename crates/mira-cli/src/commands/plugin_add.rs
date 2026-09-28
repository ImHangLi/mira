//! `mira plugin add [NAME]`: the default plugins that ship with this Mira version. Without
//! NAME it lists them; with NAME it copies one into `.mira/plugins/NAME/` through the same
//! path as `mira apply PLUGIN_DIR`. After that it is an ordinary project plugin.

use std::path::Path;
use std::process::ExitCode;

use mira_protocol::config::WORKSPACE_FILE;
use mira_protocol::reply::{PublicReply, ReplyContext, ReplyMeta};
use mira_protocol::{ErrorCode, ErrorInfo};
use serde::Serialize;
use serde_json::Value;

use super::config;
use super::ctx::Ctx;
use super::plugin_dir;

macro_rules! bundled {
    ($($plugin:literal => [$($file:literal),* $(,)?]),* $(,)?) => {
        const BUNDLED: &[(&str, &[(&str, &str)])] = &[
            $(($plugin, &[$(($file, include_str!(concat!("../../../../plugins/", $plugin, "/", $file)))),*])),*
        ];
    };
}

bundled! {
    "notes" => ["plugin.json", "main.py"],
    "system" => ["plugin.json", "main.py"],
    "pomodoro" => ["plugin.json", "main.py"],
    "snake" => ["plugin.json", "main.py"],
}

#[derive(Serialize)]
pub struct Bundled {
    pub id: &'static str,
    pub name: String,
    pub description: String,
}

pub fn bundled() -> Vec<Bundled> {
    BUNDLED
        .iter()
        .map(|(id, files)| {
            let manifest = files
                .iter()
                .find(|(f, _)| *f == "plugin.json")
                .and_then(|(_, text)| serde_json::from_str::<Value>(text).ok());
            let field = |k: &str| {
                manifest
                    .as_ref()
                    .and_then(|m| m.get(k))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            Bundled {
                id,
                name: field("name"),
                description: field("description"),
            }
        })
        .collect()
}

pub fn list(ctx: &Ctx) -> ExitCode {
    let reply = PublicReply::success(ReplyContext::default(), bundled(), ReplyMeta::default());
    ctx.emit(&reply, |list| {
        let w = list.iter().map(|p| p.id.len()).max().unwrap_or(0);
        let mut out = String::from("Default plugins (add one with `mira plugin add NAME`):\n");
        for p in list {
            out.push_str(&format!("  {:w$}  {}: {}\n", p.id, p.name, p.description));
        }
        out.trim_end().to_owned()
    })
}

pub fn add(ctx: &Ctx, name: &str) -> ExitCode {
    let Some((id, files)) = BUNDLED.iter().find(|(id, _)| *id == name) else {
        let known: Vec<&str> = BUNDLED.iter().map(|(id, _)| *id).collect();
        let e = ErrorInfo::new(
            ErrorCode::NOT_FOUND,
            format!(
                "no default plugin `{name}`; the default plugins are {}",
                known.join(", ")
            ),
        )
        .with_next_action(&["mira", "plugin", "add"], "List the default plugins.");
        return ctx.fail(ReplyContext::default(), e);
    };
    let prepared = (|| {
        let paths = ctx.paths()?;
        let ws_file = paths.mira_dir.join(WORKSPACE_FILE);
        if ws_file.is_file() {
            let ws_text = std::fs::read_to_string(&ws_file).map_err(|e| io(&ws_file, e))?;
            if let Ok(ws) = serde_json::from_str::<Value>(&ws_text) {
                let entries = plugin_dir::entries(&ws);
                if plugin_dir::find_entry(&paths.mira_dir, &entries, id).is_some() {
                    return Err(ErrorInfo::new(
                        ErrorCode::INVALID_ARGUMENT,
                        format!("plugin `{id}` is already in this project"),
                    )
                    .with_next_action(&["mira", "catalog"], "List the tools."));
                }
            }
        } else {
            create_workspace(&paths.mira_dir, paths.root.as_str())?;
        }
        let dir = plugin_dir::make_temp_dir()?.join(id);
        for (file, text) in *files {
            let path = dir.join(file);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
            }
            std::fs::write(&path, text).map_err(|e| io(&path, e))?;
        }
        Ok::<_, ErrorInfo>(dir)
    })();
    let dir = match prepared {
        Ok(d) => d,
        Err(e) => return ctx.fail(ReplyContext::default(), e),
    };
    let code = config::apply_plugin(ctx, &dir, None, None);
    if let Some(tmp) = dir.parent() {
        let _ = std::fs::remove_dir_all(tmp);
    }
    code
}

/// A minimal `.mira/workspace.json`, so `mira plugin add` works in a project without one.
pub(super) fn create_workspace(mira_dir: &Path, root: &str) -> Result<(), ErrorInfo> {
    let name = Path::new(root).file_name().map_or_else(
        || "My project".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let name = serde_json::to_string(&name)
        .map_err(|e| ErrorInfo::new(ErrorCode::INTERNAL, e.to_string()))?;
    let text = format!(
        "{{\n  \"api\": 1,\n  \"name\": {name},\n  \"plugins\": [],\n  \"autostart\": []\n}}\n"
    );
    std::fs::create_dir_all(mira_dir).map_err(|e| io(mira_dir, e))?;
    let file = mira_dir.join(WORKSPACE_FILE);
    std::fs::write(&file, text).map_err(|e| io(&file, e))
}

fn io(what: &Path, e: std::io::Error) -> ErrorInfo {
    ErrorInfo::new(ErrorCode::INTERNAL, format!("{}: {e}", what.display()))
}
