//! `apply` and `reload`: accept a validated definition set without restarting running work.
//! `apply` takes a `.mira` draft or one plugin folder.

use std::path::Path;
use std::process::ExitCode;

use mira_client::{ConnectOptions, MAINTENANCE_TIMEOUT};
use mira_protocol::ErrorInfo;
use mira_protocol::config::{WORKSPACE_FILE, load_plugin_dir};
use mira_protocol::ids::{AbsolutePath, CatalogRevision, RequestKey};
use mira_protocol::ipc::{ConfigApplied, ConfigApplyParams, Empty, Method};
use mira_protocol::reply::ReplyContext;

use super::ctx::{Ctx, block_on};
use super::plugin_dir::{self, PluginDraft};
use crate::output::invalid_argument;

fn text(a: &ConfigApplied) -> String {
    format!(
        "catalog revision {}{}; plugins: {}",
        a.catalog_revision,
        if a.changed {
            " (changed)"
        } else {
            " (no change)"
        },
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
fn apply_plugin(
    ctx: &Ctx,
    dir: &Path,
    expected: Option<u64>,
    request_key: Option<String>,
) -> ExitCode {
    block_on(async {
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
        let (plugin, expected, paths, request_key) = match prepared {
            Ok(v) => v,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let mut client = match ctx.client(&ConnectOptions::cli()).await {
            Ok(c) => c,
            Err((c, e)) => return ctx.fail(c, e),
        };
        let expected = expected.unwrap_or(client.hello().catalog_revision);
        let draft = match PluginDraft::build(&paths.mira_dir, &plugin).and_then(|d| {
            d.check()?;
            Ok(d)
        }) {
            Ok(d) => d,
            Err(e) => return ctx.fail(client.context(), e),
        };
        let draft_dir = match AbsolutePath::from_path(draft.path()) {
            Ok(p) => p,
            Err(e) => return ctx.fail(client.context(), invalid_argument(e.to_string())),
        };
        let p = ConfigApplyParams {
            draft_dir,
            expected_catalog_revision: expected,
            request_key,
        };
        let id = plugin.plugin.id.to_string();
        let tools = plugin_dir::tools(&plugin);
        match client
            .call_with_timeout::<_, ConfigApplied>(Method::ConfigApply, &p, MAINTENANCE_TIMEOUT)
            .await
        {
            Ok(r) => ctx.emit(&r, |_| format!("applied plugin `{id}` ({tools})")),
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}

pub fn reload(ctx: &Ctx) -> ExitCode {
    block_on(async {
        let mut client = match ctx.client(&ConnectOptions::cli()).await {
            Ok(c) => c,
            Err((c, e)) => return ctx.fail(c, e),
        };
        match client
            .call_with_timeout::<_, ConfigApplied>(
                Method::ConfigReload,
                &Empty {},
                MAINTENANCE_TIMEOUT,
            )
            .await
        {
            Ok(r) => ctx.emit(&r, text),
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}
