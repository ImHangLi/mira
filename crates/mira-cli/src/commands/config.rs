//! `apply`, `reload`, and `plugin remove`: accept a validated definition set without
//! restarting running work. `apply` takes a `.mira` draft or one plugin folder.

use std::path::Path;
use std::process::ExitCode;

use mira_client::Client;
use mira_client::{ConnectOptions, MAINTENANCE_TIMEOUT};
use mira_protocol::config::{WORKSPACE_FILE, load_plugin_dir};
use mira_protocol::ids::{AbsolutePath, CatalogRevision, PluginId, RequestKey};
use mira_protocol::ipc::{ConfigApplied, ConfigApplyParams, Empty, Method};
use mira_protocol::reply::{PublicReply, ReplyContext};
use mira_protocol::{ErrorCode, ErrorInfo};
use serde::Serialize;

use super::ctx::{Ctx, block_on};
use super::plugin_dir::{self, PluginDraft};
use crate::output::invalid_argument;

fn text(a: &ConfigApplied) -> String {
    format!(
        "applied; tools {} (catalog revision {}); plugins: {}",
        if a.changed { "changed" } else { "unchanged" },
        a.catalog_revision,
        a.plugins
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

pub fn apply(
    ctx: &Ctx,
    draft: &Path,
    expected: Option<u64>,
    request_key: Option<String>,
) -> ExitCode {
    if plugin_dir::is_plugin_dir(draft) {
        return apply_plugin(ctx, draft, expected, request_key);
    }
    let Some(expected) = expected else {
        return ctx.fail(
            ReplyContext::default(),
            invalid_argument(
                "applying a .mira draft needs --expected-revision N (the catalog_revision you read)",
            )
            .with_next_action(&["mira", "catalog", "--json"], "Read the catalog revision."),
        );
    };
    block_on(async {
        let parsed = (|| {
            let real = std::fs::canonicalize(draft)
                .map_err(|e| invalid_argument(format!("{}: {e}", draft.display())))?;
            let draft_dir =
                AbsolutePath::from_path(&real).map_err(|e| invalid_argument(e.to_string()))?;
            let expected =
                CatalogRevision::new(expected).map_err(|e| invalid_argument(e.to_string()))?;
            let request_key = parse_key(request_key)?;
            Ok::<_, ErrorInfo>(ConfigApplyParams {
                draft_dir,
                expected_catalog_revision: expected,
                request_key,
            })
        })();
        let p = match parsed {
            Ok(p) => p,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let mut client = match ctx.client(&ConnectOptions::cli()).await {
            Ok(c) => c,
            Err((c, e)) => return ctx.fail(c, e),
        };
        match client
            .call_with_timeout::<_, ConfigApplied>(Method::ConfigApply, &p, MAINTENANCE_TIMEOUT)
            .await
        {
            Ok(r) => ctx.emit(&r, text),
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}

fn parse_key(key: Option<String>) -> Result<Option<RequestKey>, ErrorInfo> {
    key.map(RequestKey::parse)
        .transpose()
        .map_err(|e| invalid_argument(e.to_string()))
}

/// `mira apply PLUGIN_DIR`: adds or replaces one plugin. Without `--expected-revision` it
/// uses the revision at connect time, so a concurrent change still fails with
/// REVISION_CONFLICT.
pub fn apply_plugin(
    ctx: &Ctx,
    dir: &Path,
    expected: Option<u64>,
    request_key: Option<String>,
) -> ExitCode {
    block_on(async {
        match apply_plugin_call(ctx, dir, expected, request_key).await {
            Ok((reply, text)) => ctx.emit(&reply, |_| text),
            Err((c, e)) => ctx.fail(c, e),
        }
    })
}

/// Validates and applies one plugin folder; the reply and its default text line.
pub async fn apply_plugin_call(
    ctx: &Ctx,
    dir: &Path,
    expected: Option<u64>,
    request_key: Option<String>,
) -> Result<(PublicReply<ConfigApplied>, String), (ReplyContext, ErrorInfo)> {
    let prepared = (|| {
        let plugin = load_plugin_dir(dir, None).map_err(|i| i.to_error_info())?;
        let expected = expected
            .map(CatalogRevision::new)
            .transpose()
            .map_err(|e| invalid_argument(e.to_string()))?;
        let paths = ctx.paths()?;
        if !paths.mira_dir.join(WORKSPACE_FILE).is_file() {
            return Err(ErrorInfo::not_setup());
        }
        Ok::<_, ErrorInfo>((plugin, expected, paths, parse_key(request_key)?))
    })();
    let (plugin, expected, paths, request_key) =
        prepared.map_err(|e| (ReplyContext::default(), e))?;
    let mut client = ctx.client(&ConnectOptions::cli()).await?;
    let expected = expected.unwrap_or(client.hello().catalog_revision);
    let draft = PluginDraft::build(&paths.mira_dir, &plugin)
        .and_then(|d| {
            d.check()?;
            Ok(d)
        })
        .map_err(|e| (client.context(), e))?;
    let draft_dir = AbsolutePath::from_path(draft.path())
        .map_err(|e| (client.context(), invalid_argument(e.to_string())))?;
    let p = ConfigApplyParams {
        draft_dir,
        expected_catalog_revision: expected,
        request_key,
    };
    let text = format!(
        "applied plugin `{}` ({})",
        plugin.plugin.id,
        plugin_dir::tools(&plugin)
    );
    match client
        .call_with_timeout::<_, ConfigApplied>(Method::ConfigApply, &p, MAINTENANCE_TIMEOUT)
        .await
    {
        Ok(r) => Ok((r, text)),
        Err(e) => Err((client.context(), e.to_error_info())),
    }
}

pub fn reload(ctx: &Ctx) -> ExitCode {
    block_on(async {
        let mut client = match ctx.client(&ConnectOptions::cli()).await {
            Ok(c) => c,
            Err((c, e)) => return ctx.fail(c, e),
        };
        match call_reload(&mut client).await {
            Ok(r) => ctx.emit(&r, text),
            Err(e) => ctx.fail(client.context(), e),
        }
    })
}

/// The `config.reload` call that `mira reload` makes.
async fn call_reload(client: &mut Client) -> Result<PublicReply<ConfigApplied>, ErrorInfo> {
    client
        .call_with_timeout::<_, ConfigApplied>(Method::ConfigReload, &Empty {}, MAINTENANCE_TIMEOUT)
        .await
        .map_err(|e| e.to_error_info())
}

/// The reply data of `mira plugin remove`.
#[derive(Serialize)]
struct PluginRemoved {
    /// The removed plugin ID.
    plugin: PluginId,
    /// The `workspace.json` entry that was removed.
    entry: String,
    /// The plugin folder, which stays on disk.
    kept_dir: String,
    /// The result of the reload.
    reload: ConfigApplied,
}

/// `mira plugin remove ID`: removes the plugin's entry from `.mira/workspace.json`, keeps
/// its folder, and reloads. Refuses while the plugin has active runs.
pub fn remove_plugin(ctx: &Ctx, id: &str) -> ExitCode {
    block_on(async {
        let prepared = (|| {
            let id = id
                .parse::<PluginId>()
                .map_err(|e| invalid_argument(e.to_string()))?;
            let paths = ctx.paths()?;
            let ws_file = paths.mira_dir.join(WORKSPACE_FILE);
            if !ws_file.is_file() {
                return Err(ErrorInfo::not_setup());
            }
            let ws_text = std::fs::read_to_string(&ws_file).map_err(|e| {
                ErrorInfo::new(ErrorCode::INTERNAL, format!("{}: {e}", ws_file.display()))
            })?;
            let ws = serde_json::from_str(&ws_text).map_err(|e| {
                ErrorInfo::new(
                    ErrorCode::SCHEMA_INVALID,
                    format!("{}: {e}", ws_file.display()),
                )
            })?;
            let entries = plugin_dir::entries(&ws);
            let Some(entry) = plugin_dir::find_entry(&paths.mira_dir, &entries, id.as_str()) else {
                return Err(ErrorInfo::new(
                    ErrorCode::NOT_FOUND,
                    format!("no plugin `{id}` in .mira/{WORKSPACE_FILE}"),
                )
                .with_next_action(&["mira", "catalog"], "List the tools and their plugins."));
            };
            let new_text = plugin_dir::remove_entry(&ws_text, &entry)?;
            Ok::<_, ErrorInfo>((id, entry, ws_file, ws_text, new_text))
        })();
        let (id, entry, ws_file, old_text, new_text) = match prepared {
            Ok(v) => v,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let mut client = match ctx.client(&ConnectOptions::cli()).await {
            Ok(c) => c,
            Err((c, e)) => return ctx.fail(c, e),
        };
        let status = match client.status().await.map(PublicReply::into_data) {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return ctx.fail(client.context(), e),
            Err(e) => return ctx.fail(client.context(), e.to_error_info()),
        };
        let active: Vec<String> = status
            .runs
            .iter()
            .filter(|r| r.lifecycle.is_active())
            .filter(|r| r.action_ref.as_ref().is_some_and(|a| a.plugin == id))
            .map(|r| r.run_id.to_string())
            .collect();
        if let Some(first) = active.first() {
            let mut details = serde_json::Map::new();
            details.insert("plugin".into(), id.to_string().into());
            details.insert("active_runs".into(), active.clone().into());
            let e = ErrorInfo::new(
                ErrorCode::BUSY,
                format!(
                    "plugin `{id}` has {} active run(s): {}; stop them first",
                    active.len(),
                    active.join(", ")
                ),
            )
            .with_details(details)
            .with_next_action(
                &["mira", "stop", first],
                "Stop the active run, then remove again.",
            );
            return ctx.fail(client.context(), e);
        }
        if let Err(e) = std::fs::write(&ws_file, &new_text) {
            let e = ErrorInfo::new(ErrorCode::INTERNAL, format!("{}: {e}", ws_file.display()));
            return ctx.fail(client.context(), e);
        }
        let reply = match call_reload(&mut client).await {
            Ok(r) if r.is_ok() => r,
            res => {
                // Put the entry back, so a failed reload leaves the project as it was.
                let _ = std::fs::write(&ws_file, &old_text);
                return match res {
                    Ok(r) => ctx.emit(&r, text),
                    Err(e) => ctx.fail(client.context(), e),
                };
            }
        };
        let kept_dir = if entry.starts_with('/') {
            entry.clone()
        } else {
            format!(".mira/{entry}")
        };
        let reply = reply.map(|reload| PluginRemoved {
            plugin: id,
            entry,
            kept_dir,
            reload,
        });
        ctx.emit(&reply, |r| {
            format!(
                "removed plugin `{}` from .mira/{WORKSPACE_FILE}; its files stay in {} \
                 (catalog revision {})",
                r.plugin, r.kept_dir, r.reload.catalog_revision
            )
        })
    })
}
