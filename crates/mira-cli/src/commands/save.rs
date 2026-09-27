//! `mira save TITLE -- ARGV...`: save a command as a tool in one step. It adds a command
//! action to a project plugin (`tools` unless `--plugin` names another), then validates and
//! applies it through the same path as `mira apply PLUGIN_DIR`. The plugin stays ordinary
//! files; only the new action is added, and the rest of an existing `plugin.json` is kept.

use std::path::Path;
use std::process::ExitCode;

use mira_protocol::config::{PLUGIN_FILE, WORKSPACE_FILE};
use mira_protocol::ids::{ActionId, ActionRef, PluginId};
use mira_protocol::reply::ReplyContext;
use mira_protocol::{ErrorCode, ErrorInfo};
use serde::Serialize;
use serde_json::Value;

use super::config;
use super::ctx::{Ctx, block_on};
use super::plugin_add::create_workspace;
use super::plugin_dir;
use crate::output::invalid_argument;

pub struct SaveArgs {
    pub title: String,
    pub argv: Vec<String>,
    pub id: Option<String>,
    pub plugin: String,
    pub service: bool,
    pub description: Option<String>,
}

#[derive(Serialize)]
struct Run<'a> {
    kind: &'static str,
    argv: &'a [String],
}

/// A command action, with its fields in the order people read them.
#[derive(Serialize)]
struct Action<'a> {
    id: &'a str,
    title: &'a str,
    description: &'a str,
    mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<&'a str>,
    run: Run<'a>,
}

/// The reply data of `mira save`.
#[derive(Serialize)]
struct Saved {
    action_ref: ActionRef,
    mode: &'static str,
    plugin_file: String,
    catalog_revision: mira_protocol::ids::CatalogRevision,
}

/// `Unit tests (fast)` -> `unit-tests-fast`: a valid action ID, or `None`.
fn slug(title: &str) -> Option<String> {
    let mut s = String::new();
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    let s: String = s.trim_end_matches('-').chars().take(48).collect();
    let s = s.trim_end_matches('-').to_owned();
    s.parse::<ActionId>().is_ok().then_some(s)
}

/// The command as a person would type it, for the default description.
fn shown(argv: &[String]) -> String {
    let words: Vec<String> = argv
        .iter()
        .map(|a| {
            if a.is_empty()
                || a.chars()
                    .any(|c| c.is_whitespace() || "'\"$`\\".contains(c))
            {
                format!("'{}'", a.replace('\'', "'\\''"))
            } else {
                a.clone()
            }
        })
        .collect();
    let s = words.join(" ");
    if s.chars().count() > 200 {
        format!("{}…", s.chars().take(200).collect::<String>())
    } else {
        s
    }
}

/// One-line JSON with a space after `:` and `,`, like the rest of a hand-written manifest.
struct Spaced;

impl serde_json::ser::Formatter for Spaced {
    fn begin_object_key<W: ?Sized + std::io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }

    fn begin_object_value<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        w.write_all(b": ")
    }

    fn begin_array_value<W: ?Sized + std::io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }
}

fn spaced(v: &impl Serialize) -> Result<String, ErrorInfo> {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, Spaced);
    v.serialize(&mut ser)
        .map_err(|e| ErrorInfo::new(ErrorCode::INTERNAL, e.to_string()))?;
    String::from_utf8(out).map_err(|e| ErrorInfo::new(ErrorCode::INTERNAL, e.to_string()))
}

/// Errors name the plugin file in `.mira`, not the scratch copy that was checked.
fn in_mira(e: ErrorInfo, scratch: &Path, own: &str) -> ErrorInfo {
    let from = scratch.display().to_string();
    let to = format!(".mira/{own}");
    serde_json::to_string(&e)
        .ok()
        .map(|s| s.replace(&from, &to))
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(e)
}

fn io(what: &Path, e: std::io::Error) -> ErrorInfo {
    ErrorInfo::new(ErrorCode::INTERNAL, format!("{}: {e}", what.display()))
}

/// Adds `action` (one JSON object) at the end of the top-level `actions` array of `text`,
/// keeping the rest of the file as it is.
fn add_action(text: &str, action: &str) -> Option<String> {
    let (o, c) = plugin_dir::top_level_array(text, "actions")?;
    let inner = &text[o + 1..c];
    let out = if inner.trim().is_empty() {
        format!("{}\n    {action}\n  {}", &text[..=o], &text[c..])
    } else {
        let end = o + 1 + inner.trim_end().len();
        let sep = if inner.contains('\n') {
            // One action per line, indented like the first one.
            let indent: String = inner
                .trim_start_matches(['\r', '\n'])
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect();
            format!(",\n{indent}")
        } else {
            ", ".to_owned()
        };
        format!("{}{sep}{action}{}", &text[..end], &text[end..])
    };
    Some(out)
}

pub fn save(ctx: &Ctx, a: SaveArgs) -> ExitCode {
    block_on(async {
        let prepared = (|| {
            if a.argv.is_empty() {
                return Err(invalid_argument(
                    "give the command after `--`, for example: mira save \"Unit tests\" -- npm test",
                ));
            }
            let plugin = a
                .plugin
                .parse::<PluginId>()
                .map_err(|e| invalid_argument(format!("--plugin: {e}")))?;
            let id = match &a.id {
                Some(id) => id
                    .parse::<ActionId>()
                    .map_err(|e| invalid_argument(format!("--id: {e}")))?
                    .to_string(),
                None => slug(&a.title).ok_or_else(|| {
                    invalid_argument(format!(
                        "cannot make an ID from the title `{}`; pass --id, for example --id unit-tests",
                        a.title
                    ))
                })?,
            };
            let paths = ctx.paths()?;
            if !paths.mira_dir.join(WORKSPACE_FILE).is_file() {
                create_workspace(&paths.mira_dir, paths.root.as_str())?;
            }
            // The command runs where it was saved: a subfolder of the project stays its cwd.
            let here = std::env::current_dir()
                .and_then(std::fs::canonicalize)
                .map_err(|e| invalid_argument(format!("cannot read the current folder: {e}")))?;
            let root = std::fs::canonicalize(paths.root.as_str())
                .map_err(|e| io(Path::new(paths.root.as_str()), e))?;
            let cwd = here
                .strip_prefix(&root)
                .ok()
                .map(|r| r.to_string_lossy().into_owned())
                .filter(|r| !r.is_empty());

            // Find the plugin, or start a new one in plugins/<id>.
            let ws_file = paths.mira_dir.join(WORKSPACE_FILE);
            let ws_text = std::fs::read_to_string(&ws_file).map_err(|e| io(&ws_file, e))?;
            let ws: Value = serde_json::from_str(&ws_text).map_err(|e| {
                ErrorInfo::new(ErrorCode::SCHEMA_INVALID, format!("{WORKSPACE_FILE}: {e}"))
            })?;
            let entries = plugin_dir::entries(&ws);
            let existing = plugin_dir::find_entry(&paths.mira_dir, &entries, plugin.as_str());
            let own = format!("plugins/{plugin}");
            if let Some(e) = &existing
                && *e != own
            {
                return Err(invalid_argument(format!(
                    "plugin `{plugin}` lives in .mira/{e}; add the action there by hand, or save to another plugin with --plugin"
                )));
            }
            let description = a
                .description
                .clone()
                .unwrap_or_else(|| format!("Runs `{}`.", shown(&a.argv)));
            let mode = if a.service { "process" } else { "task" };
            let action = spaced(&Action {
                id: &id,
                title: &a.title,
                description: &description,
                mode,
                cwd: cwd.as_deref(),
                run: Run {
                    kind: "command",
                    argv: &a.argv,
                },
            })?;

            let draft = plugin_dir::make_temp_dir()?.join(plugin.as_str());
            let text = match &existing {
                Some(entry) => {
                    let src = paths.mira_dir.join(entry);
                    plugin_dir::copy_tree(&src, &draft)?;
                    let file = draft.join(PLUGIN_FILE);
                    let text = std::fs::read_to_string(&file).map_err(|e| io(&file, e))?;
                    let v: Value = serde_json::from_str(&text).map_err(|e| {
                        ErrorInfo::new(ErrorCode::SCHEMA_INVALID, format!("{PLUGIN_FILE}: {e}"))
                    })?;
                    // Actions and views share one namespace in a plugin.
                    let taken = ["actions", "views"].iter().any(|k| {
                        v.get(k).and_then(Value::as_array).is_some_and(|l| {
                            l.iter()
                                .any(|x| x.get("id").and_then(Value::as_str) == Some(&id))
                        })
                    });
                    if taken {
                        return Err(ErrorInfo::new(
                            ErrorCode::INVALID_ARGUMENT,
                            format!("`{plugin}.{id}` already exists; pass --id with another ID"),
                        )
                        .with_next_action(
                            &["mira", "describe", &format!("{plugin}.{id}")],
                            "See the tool that has this ID.",
                        ));
                    }
                    let out = add_action(&text, &action).ok_or_else(|| {
                        invalid_argument(format!(
                            "{PLUGIN_FILE} of `{plugin}` has no top-level `actions` list; save to another plugin with --plugin"
                        ))
                    })?;
                    // The edit must add exactly this action and change nothing else.
                    let after: Value = serde_json::from_str(&out).map_err(|e| {
                        ErrorInfo::new(ErrorCode::INTERNAL, format!("edited {PLUGIN_FILE}: {e}"))
                    })?;
                    let last = after
                        .get("actions")
                        .and_then(Value::as_array)
                        .and_then(|l| l.last())
                        .and_then(|x| x.get("id"))
                        .and_then(Value::as_str);
                    if last != Some(id.as_str()) {
                        return Err(ErrorInfo::new(
                            ErrorCode::INTERNAL,
                            format!("could not add the action to {PLUGIN_FILE} of `{plugin}`"),
                        ));
                    }
                    out
                }
                None => {
                    std::fs::create_dir_all(&draft).map_err(|e| io(&draft, e))?;
                    let name = if plugin.as_str() == "tools" {
                        "Tools".to_owned()
                    } else {
                        plugin.to_string()
                    };
                    format!(
                        "{{\n  \"api\": 1,\n  \"id\": {},\n  \"name\": {},\n  \"description\": \"Commands saved with `mira save`.\",\n  \"actions\": [\n    {action}\n  ]\n}}\n",
                        serde_json::to_string(plugin.as_str()).unwrap_or_default(),
                        serde_json::to_string(&name).unwrap_or_default(),
                    )
                }
            };
            let file = draft.join(PLUGIN_FILE);
            std::fs::write(&file, text).map_err(|e| io(&file, e))?;
            let action_ref: ActionRef = format!("{plugin}.{id}")
                .parse()
                .map_err(|e| ErrorInfo::new(ErrorCode::INTERNAL, format!("{e}")))?;
            Ok::<_, ErrorInfo>((draft, action_ref, mode, own))
        })();
        let (draft, action_ref, mode, own) = match prepared {
            Ok(v) => v,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let result = config::apply_plugin_call(ctx, &draft, None, None).await;
        if let Some(tmp) = draft.parent() {
            let _ = std::fs::remove_dir_all(tmp);
        }
        match result {
            Ok((reply, _)) => {
                let reply = reply.map(|applied| Saved {
                    action_ref,
                    mode,
                    plugin_file: format!(".mira/{own}/{PLUGIN_FILE}"),
                    catalog_revision: applied.catalog_revision,
                });
                ctx.emit(&reply, |s| {
                    let (verb, kind) = if s.mode == "process" {
                        ("start", "service")
                    } else {
                        ("run", "task")
                    };
                    format!(
                        "saved `{}` ({kind}) in {}\nrun it: mira {verb} {}   or select it in `mira`",
                        s.action_ref, s.plugin_file, s.action_ref
                    )
                })
            }
            Err((c, e)) => ctx.fail(c, in_mira(e, &draft, &own)),
        }
    })
}
