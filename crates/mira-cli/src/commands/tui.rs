//! `mira` with no command: the human TUI. Agents get `TTY_REQUIRED` at once instead
//! of a blocked process; an unconfigured workspace gets one screen of setup instructions.

use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::reply::ReplyContext;
use mira_tui::TuiEnd;

use super::ctx::Ctx;

pub fn open(ctx: &Ctx) -> ExitCode {
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return ctx.fail(
            ReplyContext::default(),
            ErrorInfo::new(
                ErrorCode::TTY_REQUIRED,
                "`mira` without a command opens the TUI and needs an interactive terminal",
            )
            .with_next_action(
                &["mira", "status", "--json"],
                "Agents use the structured CLI; `mira --help` lists every command.",
            ),
        );
    }
    let paths = match ctx.paths() {
        Ok(p) => p,
        Err(e) => return ctx.fail(ReplyContext::default(), e),
    };
    let root = paths.root.to_string();
    match mira_tui::run(paths) {
        Ok(TuiEnd::Closed(message)) => {
            // The window may already be gone (SIGHUP); nothing else to report then.
            let _ = writeln!(std::io::stderr(), "mira: {message}");
            ExitCode::SUCCESS
        }
        Ok(TuiEnd::NotSetup(e)) => {
            let _ = std::io::stdout().write_all(setup_screen(&root, &e).as_bytes());
            ExitCode::from(e.code.exit_code())
        }
        Err(e) => ctx.fail(ReplyContext::default(), e),
    }
}

fn setup_screen(root: &str, e: &ErrorInfo) -> String {
    format!(
        r#"Mira is not set up in {root}
{message}.

Mira never guesses commands. Ask your coding agent to set up Mira for this repo;
agents start here:

  https://github.com/ImHangLi/mira/blob/main/docs/agents.md

Or start with a plugin that ships with Mira, then run `mira`:

{defaults}
Or set it up yourself (guide: https://github.com/ImHangLi/mira/blob/main/docs/guide.md).
Write these two files, with your own command in "argv":

  .mira/workspace.json
    {{"api": 1, "name": "My project", "plugins": ["plugins/dev"], "autostart": []}}

  .mira/plugins/dev/plugin.json
    {{
      "api": 1, "id": "dev", "name": "Dev", "description": "Project commands.",
      "actions": [
        {{"id": "test", "title": "Unit tests", "description": "Run the tests once.",
         "mode": "task", "run": {{"kind": "command", "argv": ["npm", "test"]}}}}
      ]
    }}

Then run `mira validate .mira` and `mira` again.
"#,
        message = e.message,
        defaults = defaults_list(),
    )
}

/// `  mira plugin add NAME   Name: description`, one line per default plugin.
fn defaults_list() -> String {
    let list = super::plugin_add::bundled();
    let w = list.iter().map(|p| p.id.len()).max().unwrap_or(0);
    list.iter()
        .map(|p| {
            format!(
                "  mira plugin add {:w$}   {}: {}\n",
                p.id, p.name, p.description
            )
        })
        .collect()
}
