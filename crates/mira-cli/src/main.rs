//! `mira`: one binary for the human TUI and the agent CLI.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use mira_protocol::reply::ReplyContext;

mod commands;
mod human;
mod output;

use output::Mode;

#[derive(Parser)]
#[command(
    name = "mira",
    version,
    about = "A local control center for everything you run, built entirely from plugins."
)]
struct Cli {
    /// Select the project explicitly.
    #[arg(long, global = true, value_name = "PATH")]
    project: Option<PathBuf>,
    /// Print exactly one JSON reply object (default when stdout is not a TTY).
    #[arg(long, global = true, conflicts_with = "text")]
    json: bool,
    /// Print human-readable text.
    #[arg(long, global = true)]
    text: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print a raw JSON Schema: workspace, plugin, local, invocation, plugin-event, cli-reply, ipc.
    #[command(hide = true)]
    Schema { name: String },
    /// Check a .mira draft, workspace.json, a plugin folder, or plugin.json without running
    /// anything. Inside a project, a plugin is also checked against the project's .mira.
    Validate {
        #[arg(value_name = "PATH")]
        path: PathBuf,
    },
    /// Show the session, active runs, schedules, and warnings.
    Status,
    /// List this project's tools.
    Catalog {
        /// Search words. An item matches when any word matches its ref, id, title, tags,
        /// or description. Results rank by exact ref or id, exact title, ref/id/title
        /// prefix, ref/id/title substring, tag, then description; more matched words rank
        /// higher; ties keep catalog order.
        #[arg(long, value_name = "WORDS")]
        search: Option<String>,
        /// Return not_modified when the catalog revision still equals N.
        #[arg(long, value_name = "N")]
        if_revision: Option<u64>,
        /// With --if-revision: the project ID the cached catalog came from.
        #[arg(long, value_name = "WORKSPACE_ID", requires = "if_revision")]
        if_workspace: Option<String>,
        /// Items per page (default 30).
        #[arg(long)]
        limit: Option<u32>,
        /// Continue from the previous reply's meta.next_cursor.
        #[arg(long, value_name = "CURSOR")]
        after: Option<String>,
        /// Reply budget in bytes, envelope included (default 32 KiB, up to 256 KiB).
        #[arg(long)]
        max_bytes: Option<u32>,
    },
    /// Show what one tool does, its inputs, and how to run it.
    Describe {
        /// The tool as plugin.item, such as dev.web.
        #[arg(value_name = "REF")]
        item: String,
        /// Include the input and output JSON Schemas.
        #[arg(long)]
        include_schema: bool,
        /// Reply budget; schemas that do not fit are returned by payload reference.
        #[arg(long)]
        max_bytes: Option<u32>,
    },
    /// Show where this project's config, state, logs, cache, and runtime files are.
    #[command(hide = true)]
    Paths,
    /// Check the project, plugins, and required programs.
    Doctor,
    /// Run a task and wait for the result (--no-wait needs a session).
    Run {
        #[arg(value_name = "ACTION")]
        action: String,
        /// JSON object input file, or `-` for stdin.
        #[arg(long, value_name = "FILE")]
        input: Option<String>,
        /// Return once the run started; it keeps running in the session.
        #[arg(long)]
        no_wait: bool,
        /// Run at most once per key; a repeat returns the first run.
        #[arg(long, value_name = "KEY")]
        request_key: Option<String>,
    },
    /// Start a service, or reuse the running one with the same input.
    Start {
        #[arg(value_name = "ACTION")]
        action: String,
        /// JSON object input file, or `-` for stdin.
        #[arg(long, value_name = "FILE")]
        input: Option<String>,
        /// Start at most once per key; a repeat returns the first run.
        #[arg(long, value_name = "KEY")]
        request_key: Option<String>,
    },
    /// Stop one run (by run ID or action ref).
    Stop {
        /// A run ID, a unique prefix of one (such as r_fb60aacf), or an action ref.
        #[arg(value_name = "RUN_OR_ACTION")]
        target: String,
        /// Wait until the run and its cleanup finished.
        #[arg(long)]
        wait: bool,
    },
    /// Stop the action's current run, then start it with the current definition.
    Restart {
        #[arg(value_name = "ACTION")]
        action: String,
        /// JSON object input file, or `-` for stdin.
        #[arg(long, value_name = "FILE")]
        input: Option<String>,
    },
    /// Run a one-off command (not saved as a plugin).
    Exec {
        #[arg(long)]
        label: String,
        #[arg(long, value_name = "KEY")]
        request_key: Option<String>,
        #[arg(last = true, required = true, value_name = "ARGV")]
        argv: Vec<String>,
    },
    /// Keep work running without an open window, until the time limit or `mira down`.
    ///
    /// When a session is already running, this sets its time limit again, counted from now.
    Up {
        /// Required: run the session in the background.
        #[arg(long)]
        background: bool,
        /// How long to keep it, from now: 30m, 2h, 1d, or none.
        #[arg(long, default_value = "2h")]
        ttl: String,
    },
    /// Stop everything this project runs (no data is deleted).
    Down {
        /// Wait until everything stopped.
        #[arg(long)]
        wait: bool,
    },
    /// List recent runs, or show one run.
    Runs {
        /// Show one run: its ID or a unique prefix of it (such as r_fb60aacf).
        #[arg(value_name = "RUN")]
        run: Option<String>,
        /// Only runs of this action.
        #[arg(long, value_name = "REF")]
        action: Option<String>,
        /// Only runs with this outcome: succeeded, failed, cancelled, timed_out, interrupted.
        #[arg(long)]
        outcome: Option<String>,
        /// Runs per page.
        #[arg(long)]
        limit: Option<u32>,
        /// Continue from the previous reply's meta.next_cursor.
        #[arg(long, value_name = "CURSOR")]
        after: Option<String>,
        /// Reply budget in bytes (default 32 KiB).
        #[arg(long)]
        max_bytes: Option<u32>,
    },
    /// Show a run's output: the end of the current or latest run by default.
    Logs {
        /// A run ID, a unique prefix of one (such as r_fb60aacf), or an action ref for its
        /// current or latest run.
        #[arg(value_name = "RUN_OR_ACTION")]
        target: String,
        /// Read forward from this cursor instead of the end.
        #[arg(long, value_name = "CURSOR")]
        after: Option<String>,
        /// Records per page.
        #[arg(long)]
        limit: Option<u32>,
        /// Reply budget in bytes (default 32 KiB).
        #[arg(long)]
        max_bytes: Option<u32>,
        /// Keep printing new records until the run ends.
        #[arg(long)]
        follow: bool,
        /// Keep only records whose text contains PATTERN (case-insensitive), in the page and
        /// with --follow.
        #[arg(long, value_name = "PATTERN")]
        grep: Option<String>,
        /// Keep only records from one output stream.
        #[arg(long, value_enum, value_name = "STREAM")]
        stream: Option<commands::runtime::StreamArg>,
    },
    /// Show a view with its source and age (never re-runs it).
    View {
        #[arg(value_name = "VIEW")]
        view: String,
        /// Continue from the previous reply's meta.next_cursor.
        #[arg(long, value_name = "CURSOR")]
        after: Option<String>,
        /// Rows or items per page.
        #[arg(long)]
        limit: Option<u32>,
        /// Reply budget in bytes (default 32 KiB).
        #[arg(long)]
        max_bytes: Option<u32>,
    },
    /// Publish an update to a view. No session needed.
    Publish {
        #[arg(value_name = "VIEW")]
        view: String,
        /// The frame file, or `-` for stdin.
        #[arg(long, value_name = "FILE")]
        input: String,
        /// Refuse unless the view is still at revision N.
        #[arg(long, value_name = "N")]
        expected_view_revision: Option<u64>,
        /// Publish at most once per key.
        #[arg(long, value_name = "KEY")]
        request_key: Option<String>,
    },
    /// Run a table row action with input taken from that row.
    #[command(hide = true)]
    ViewAction {
        /// The table view, as plugin.view.
        #[arg(value_name = "VIEW")]
        view: String,
        /// The row action ID, as listed by `mira describe VIEW`.
        #[arg(value_name = "ACTION")]
        action: String,
        /// The row ID.
        #[arg(long, value_name = "ROW")]
        row: String,
        /// The view revision you read the row at; a changed view is refused.
        #[arg(long, value_name = "N")]
        expected_view_revision: u64,
    },
    /// List saved run outputs (artifacts), or read one.
    #[command(args_conflicts_with_subcommands = true, hide = true)]
    Artifacts {
        #[arg(value_name = "RUN")]
        run: Option<String>,
        #[command(subcommand)]
        read: Option<ArtifactsCommand>,
    },
    /// Read the rest of a large reply.
    #[command(hide = true)]
    Payload {
        #[command(subcommand)]
        command: PayloadCommand,
    },
    /// Apply a validated draft of .mira, or add or replace one plugin folder.
    ///
    /// A plugin folder (with plugin.json) outside .mira is copied to .mira/plugins/<id>/,
    /// and its path is added to .mira/workspace.json when it is new.
    #[command(hide = true)]
    Apply {
        #[arg(value_name = "DRAFT_OR_PLUGIN_DIR")]
        draft: PathBuf,
        /// The catalog revision your change is based on. Required for a draft; for a plugin
        /// folder it defaults to the current revision.
        #[arg(long, value_name = "N")]
        expected_revision: Option<u64>,
        #[arg(long, value_name = "KEY")]
        request_key: Option<String>,
    },
    /// Load .mira again from disk; keep the old plugins if it is not valid.
    Reload,
    /// Manage this project's plugins.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Turn an action's schedule on or off (it runs only while a session is open).
    Schedule {
        #[arg(value_name = "ACTION")]
        action: String,
        #[arg(value_enum)]
        switch: commands::schedule::Switch,
    },
    /// Read the screen of an interactive (PTY) run as plain text with its screen revision.
    ///
    /// This is the current virtual screen, not a log; `mira logs RUN` holds the transcript.
    /// RUN is a run ID, a unique prefix of one (such as r_fb60aacf), or an action ref for its
    /// current or latest run.
    /// Agent flow: `mira up --background`, `mira run ACTION --no-wait` for a PTY action, then
    /// alternate `mira terminal RUN` and `mira input RUN ...` until the program finishes.
    #[command(verbatim_doc_comment)]
    Terminal {
        #[arg(value_name = "RUN_OR_ACTION")]
        run: String,
        /// Reply byte budget (default 32 KiB); rows past it are cut and meta.truncated is set.
        #[arg(long, value_name = "N")]
        max_bytes: Option<u32>,
    },
    /// Type into an interactive (PTY) run; prints the screen after the program reacts.
    ///
    /// Inside `input`, --text is the input text, not the output-mode flag; output is JSON
    /// unless stdout is a terminal.
    /// Each call takes the input lock for that one write and fails with INPUT_BUSY while
    /// another client (for example the TUI) holds it. --text is sent as UTF-8 exactly as
    /// given and never adds Enter; send `--key enter` separately.
    /// Keys: enter, tab, escape, backspace, delete, up, down, left, right, ctrl-c, ctrl-d,
    /// ctrl-z, ctrl-right-bracket.
    /// With --expected-screen-revision N nothing is written and SCREEN_CHANGED is returned
    /// when the screen moved past revision N (read it with `mira terminal RUN`).
    /// Example: mira run dev.prompt --no-wait; mira terminal RUN;
    ///          mira input RUN --text Ada; mira input RUN --key enter
    #[command(
        verbatim_doc_comment,
        override_usage = "mira input <RUN_OR_ACTION> (--text TEXT | --key KEY) [--expected-screen-revision N]"
    )]
    Input {
        /// A run ID, a unique prefix of one (such as r_fb60aacf), or an action ref.
        #[arg(value_name = "RUN_OR_ACTION")]
        run: String,
        /// Given as `--text TEXT` (see the usage).
        #[arg(
            long = "input-text",
            value_name = "TEXT",
            conflicts_with = "key",
            required_unless_present = "key",
            hide = true
        )]
        input_text: Option<String>,
        /// One named key (see the list above).
        #[arg(long, value_name = "KEY")]
        key: Option<String>,
        /// Refuse to write unless the screen is still at revision N.
        #[arg(long, value_name = "N")]
        expected_screen_revision: Option<u64>,
    },
    /// Show disk use, clean up old data, or clear a plugin's state.
    #[command(hide = true)]
    Storage {
        #[command(subcommand)]
        command: StorageCommand,
    },
    /// Export the agent skills of this Mira version to your own skills folder.
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
    /// Internal: serve the project host.
    #[command(name = "__host", hide = true)]
    Host {
        #[arg(long)]
        root: PathBuf,
    },
}

#[derive(Subcommand)]
enum ArtifactsCommand {
    /// Read a bounded UTF-8 text chunk of one artifact.
    Read {
        #[arg(value_name = "ID")]
        id: String,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long)]
        max_bytes: Option<u32>,
    },
}

/// `mira input RUN --text TEXT` spells its text option like the global `--text`
/// output flag. After the `input` subcommand, `--text` means the input text.
fn rewrite_input_text(args: Vec<String>) -> Vec<String> {
    // With `--key`, or as the last argument, `--text` is the output-mode flag.
    let has_key = args.iter().any(|a| a == "--key" || a.starts_with("--key="));
    let mut out = Vec::with_capacity(args.len());
    let mut iter = args.into_iter().peekable();
    out.extend(iter.next());
    let mut in_input = false;
    while let Some(a) = iter.next() {
        if a == "--" {
            out.push(a);
            out.extend(iter);
            break;
        }
        if !in_input {
            let project = a == "--project";
            let positional = !a.starts_with('-');
            out.push(a);
            if project {
                out.extend(iter.next());
            } else if positional {
                if out.last().is_some_and(|s| s == "input") {
                    in_input = true;
                } else {
                    out.extend(iter);
                    break;
                }
            }
            continue;
        }
        match a.strip_prefix("--text") {
            Some("") if has_key || iter.peek().is_none() => out.push(a),
            Some("") => out.push("--input-text".to_owned()),
            Some(v) if v.starts_with('=') => out.push(format!("--input-text{v}")),
            _ => out.push(a),
        }
    }
    out
}

#[derive(Subcommand)]
enum PayloadCommand {
    /// Read one bounded UTF-8 chunk (default 16 KiB); never re-runs the producing action.
    Read {
        #[arg(value_name = "TOKEN")]
        token: String,
        /// JSON Pointer selecting a subvalue first, e.g. /rows/0.
        #[arg(long)]
        pointer: Option<String>,
        /// Byte offset of the selected serialization; continue with next_offset.
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long)]
        max_bytes: Option<u32>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum GcKindArg {
    Cache,
    Logs,
    History,
    Artifacts,
    All,
}

#[derive(Subcommand)]
enum StorageCommand {
    /// Show disk use by category; `--all` also lists other projects.
    Status {
        #[arg(long)]
        all: bool,
    },
    /// Plan retention cleanup; `--apply` deletes what the current policy allows.
    Gc {
        #[arg(long, value_enum, default_value = "all")]
        kind: GcKindArg,
        #[arg(long)]
        apply: bool,
    },
    /// Delete one plugin's private state (only when it has no active run).
    Clear {
        #[arg(long)]
        plugin: String,
        /// Only `state` is supported.
        #[arg(long, default_value = "state")]
        kind: String,
    },
}

#[derive(Subcommand)]
enum PluginCommand {
    /// Remove a plugin from .mira/workspace.json and reload; its folder stays on disk.
    /// Refuses while the plugin has active runs.
    Remove {
        #[arg(value_name = "PLUGIN_ID")]
        id: String,
    },
}

#[derive(Subcommand)]
enum SkillsCommand {
    /// Copy `mira` and `mira-extend` into DIR (for example ~/.claude/skills or
    /// ~/.agents/skills). Refuses to overwrite existing files without `--force` and refuses
    /// a DIR inside the current Git work tree.
    Export {
        #[arg(value_name = "DIR")]
        dir: PathBuf,
        /// Overwrite existing skill files.
        #[arg(long)]
        force: bool,
    },
}

fn main() -> ExitCode {
    // Read the local UTC offset while the process is still single-threaded.
    human::init_local_offset();
    let args = rewrite_input_text(std::env::args().collect());
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(e) => {
            use clap::error::ErrorKind;
            if matches!(
                e.kind(),
                ErrorKind::DisplayHelp
                    | ErrorKind::DisplayVersion
                    | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) {
                let _ = e.print();
                return ExitCode::SUCCESS;
            }
            let mode = Mode::resolve(
                args.iter().any(|a| a == "--json"),
                args.iter().any(|a| a == "--text"),
            );
            if mode == Mode::Text {
                let _ = e.print();
                return ExitCode::from(2);
            }
            // Keep the details (such as the missing flag names) but not the usage block.
            let message = e.render().to_string();
            let first = message
                .lines()
                .take_while(|l| !l.starts_with("Usage:") && !l.starts_with("For more information"))
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
                .trim_start_matches("error: ")
                .replace("--input-text", "--text");
            let first = if first.is_empty() {
                "invalid arguments".to_owned()
            } else {
                first
            };
            return output::fail(
                mode,
                ReplyContext::default(),
                output::invalid_argument(first),
            );
        }
    };
    let mode = Mode::resolve(cli.json, cli.text);
    let ctx = commands::ctx::Ctx {
        mode,
        project: cli.project.clone(),
    };
    match cli.command {
        Some(Command::Status) => commands::inspect::status(&ctx),
        Some(Command::Catalog {
            search,
            if_revision,
            if_workspace,
            limit,
            after,
            max_bytes,
        }) => commands::inspect::catalog(
            &ctx,
            commands::inspect::CatalogArgs {
                search,
                if_revision,
                if_workspace,
                limit,
                after,
                max_bytes,
            },
        ),
        Some(Command::Describe {
            item,
            include_schema,
            max_bytes,
        }) => commands::inspect::describe(&ctx, item, include_schema, max_bytes),
        Some(Command::Run {
            action,
            input,
            no_wait,
            request_key,
        }) => commands::runtime::run(&ctx, &action, input.as_deref(), no_wait, request_key),
        Some(Command::Start {
            action,
            input,
            request_key,
        }) => commands::runtime::start(&ctx, &action, input.as_deref(), request_key),
        Some(Command::Stop { target, wait }) => commands::runtime::stop(&ctx, &target, wait),
        Some(Command::Restart { action, input }) => {
            commands::runtime::restart(&ctx, &action, input.as_deref())
        }
        Some(Command::Exec {
            label,
            request_key,
            argv,
        }) => commands::runtime::exec(&ctx, label, argv, request_key),
        Some(Command::Up { background, ttl }) => commands::runtime::up(&ctx, background, &ttl),
        Some(Command::Down { wait }) => commands::runtime::down(&ctx, wait),
        Some(Command::Runs {
            run,
            action,
            outcome,
            limit,
            after,
            max_bytes,
        }) => commands::runtime::runs(&ctx, run, action, outcome, limit, after, max_bytes),
        Some(Command::Logs {
            target,
            after,
            limit,
            max_bytes,
            follow,
            grep,
            stream,
        }) => commands::runtime::logs(
            &ctx,
            &target,
            commands::runtime::LogArgs {
                after,
                limit,
                max_bytes,
                follow,
                filter: commands::runtime::LogFilter::new(grep.as_deref(), stream),
            },
        ),
        Some(Command::View {
            view,
            after,
            limit,
            max_bytes,
        }) => commands::views::view(&ctx, &view, after, limit, max_bytes),
        Some(Command::Publish {
            view,
            input,
            expected_view_revision,
            request_key,
        }) => commands::views::publish(&ctx, &view, &input, expected_view_revision, request_key),
        Some(Command::ViewAction {
            view,
            action,
            row,
            expected_view_revision,
        }) => commands::views::view_action(&ctx, &view, &action, row, expected_view_revision),
        Some(Command::Artifacts { run, read: None }) => commands::views::artifacts(&ctx, run),
        Some(Command::Artifacts {
            read:
                Some(ArtifactsCommand::Read {
                    id,
                    offset,
                    max_bytes,
                }),
            ..
        }) => commands::views::artifact_read(&ctx, id, offset, max_bytes),
        Some(Command::Terminal { run, max_bytes }) => {
            commands::terminal::terminal(&ctx, &run, max_bytes)
        }
        Some(Command::Input {
            run,
            input_text: text,
            key,
            expected_screen_revision,
        }) => commands::terminal::input(&ctx, &run, text, key, expected_screen_revision),
        Some(Command::Payload {
            command:
                PayloadCommand::Read {
                    token,
                    pointer,
                    offset,
                    max_bytes,
                },
        }) => commands::payload::read(&ctx, token, pointer, offset, max_bytes),
        Some(Command::Paths) => commands::inspect::paths(&ctx),
        Some(Command::Doctor) => commands::inspect::doctor(&ctx),
        Some(Command::Host { root }) => match mira_protocol::ids::AbsolutePath::from_path(&root) {
            Ok(root) => ExitCode::from(mira_host::run(root)),
            Err(_) => ExitCode::from(2),
        },
        Some(Command::Schema { name }) => commands::contract::schema(mode, &name),
        Some(Command::Validate { path }) => commands::contract::validate(&ctx, &path),
        Some(Command::Apply {
            draft,
            expected_revision,
            request_key,
        }) => commands::config::apply(&ctx, &draft, expected_revision, request_key),
        Some(Command::Schedule { action, switch }) => {
            commands::schedule::set(&ctx, &action, switch)
        }
        Some(Command::Storage { command }) => match command {
            StorageCommand::Status { all } => commands::storage::status(&ctx, all),
            StorageCommand::Gc { kind, apply } => {
                use mira_protocol::ipc::GcKind;
                let kind = match kind {
                    GcKindArg::Cache => GcKind::Cache,
                    GcKindArg::Logs => GcKind::Logs,
                    GcKindArg::History => GcKind::History,
                    GcKindArg::Artifacts => GcKind::Artifacts,
                    GcKindArg::All => GcKind::All,
                };
                commands::storage::gc(&ctx, kind, apply)
            }
            StorageCommand::Clear { plugin, kind } if kind == "state" => {
                commands::storage::clear(&ctx, &plugin)
            }
            StorageCommand::Clear { .. } => output::fail(
                mode,
                ReplyContext::default(),
                output::invalid_argument("only --kind state is supported"),
            ),
        },
        Some(Command::Skills {
            command: SkillsCommand::Export { dir, force },
        }) => commands::skills::export(&ctx, &dir, force),
        Some(Command::Reload) => commands::config::reload(&ctx),
        Some(Command::Plugin {
            command: PluginCommand::Remove { id },
        }) => commands::config::remove_plugin(&ctx, &id),
        None => commands::tui::open(&ctx),
    }
}

#[cfg(test)]
mod tests {
    use super::rewrite_input_text;

    fn rewrite(args: &[&str]) -> Vec<String> {
        rewrite_input_text(args.iter().map(|s| (*s).to_owned()).collect())
    }

    #[test]
    fn input_text_value_becomes_input_text() {
        assert_eq!(
            rewrite(&["mira", "input", "r_1", "--text", "hi"]),
            ["mira", "input", "r_1", "--input-text", "hi"]
        );
    }

    #[test]
    fn text_with_key_or_last_is_the_output_flag() {
        assert_eq!(
            rewrite(&["mira", "input", "r_1", "--key", "enter", "--text"]),
            ["mira", "input", "r_1", "--key", "enter", "--text"]
        );
        assert_eq!(
            rewrite(&["mira", "input", "r_1", "--text"]),
            ["mira", "input", "r_1", "--text"]
        );
    }

    #[test]
    fn other_commands_are_unchanged() {
        assert_eq!(
            rewrite(&["mira", "status", "--text"]),
            ["mira", "status", "--text"]
        );
    }

    #[test]
    fn keep_is_gone_and_apply_revision_is_optional() {
        use clap::Parser;
        assert!(super::Cli::try_parse_from(["mira", "keep"]).is_err());
        assert!(super::Cli::try_parse_from(["mira", "up", "--background", "--ttl", "30m"]).is_ok());
        assert!(super::Cli::try_parse_from(["mira", "apply", "plugins/errors"]).is_ok());
    }
}
