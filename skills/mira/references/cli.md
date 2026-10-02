# CLI reference

All commands accept `--project PATH`, `--json`, and `--text`. `REF` is `plugin.item`. `RUN` is a run ID (`r_…`) or a unique prefix of one; `logs`, `stop`, `terminal`, and `input` also take an action ref.

`mira --help` does not list `apply`, `view-action`, `artifacts`, `payload`, `storage`, `paths`, and `schema`, but they work as shown here.

| Command | Notes |
|---|---|
| `mira` | the TUI for humans (needs a TTY) |
| `status` | session, active runs, warnings |
| `catalog [--search TEXT] [--limit N] [--after CURSOR] [--if-revision N --if-workspace W]` | the tool list, paged by cursor. With `--if-revision`, `meta.not_modified: true` means your copy of that workspace and query is still valid |
| `describe REF [--include-schema]` | purpose, runner, cwd, env names (never values), how to invoke |
| `run ACTION [--input FILE\|-] [--no-wait] [--request-key K]` | a task; waits unless `--no-wait` (needs a session) |
| `start ACTION [--input FILE\|-] [--request-key K]` | a process; reuses the running instance |
| `stop RUN_OR_ACTION [--wait]` | TERM (or INT), then KILL after the grace period; cleanup runs once |
| `restart ACTION [--input FILE\|-]` | stop, wait, then start with the current definition |
| `exec --task TEXT --label TEXT -- ARGV...` | a one-off command; not added to the catalog |
| `save TITLE [--service] [--id ID] [--plugin ID] [--description TEXT] -- ARGV...` | save a command as a tool in the `tools` plugin, validated and loaded. From a subfolder, that folder is the tool's `cwd` |
| `up --background [--ttl 30m\|2h\|none]` | keep work running without the TUI (default 2 h); run it again to set a new limit from now |
| `down [--wait]` | stop the session and its runs; deletes no data |
| `runs [RUN] [--action REF] [--outcome VALUE] [--limit N] [--after CURSOR]` | newest first |
| `logs RUN_OR_ACTION [--after CURSOR] [--limit N] [--follow] [--grep PATTERN] [--stream stdout\|stderr]` | the end of the log by default; `--grep` ignores case |
| `logs --host [--limit N] [--grep PATTERN]` | Mira's own log: rejected configuration, storage problems, crashes. Works when the host is down |
| `view VIEW [--after CURSOR] [--limit N]` | typed data |
| `view-action VIEW ACTION --row ROW --expected-view-revision N` | a table row action; refuses a changed table (`VIEW_CHANGED`) |
| `publish VIEW --input FILE\|- [--expected-view-revision N] [--request-key K]` | write one view frame without a plugin run; needs no session |
| `validate PATH` | check a `.mira`, a draft, or a plugin folder; runs nothing |
| `apply DIR [--expected-revision N]` | apply a draft of `.mira`, or copy a plugin folder to `.mira/plugins/<id>/` and list it in `workspace.json` |
| `reload` | load `.mira` again from disk |
| `plugin add [NAME]` | without NAME, list the default plugins; with NAME, copy one to `.mira/plugins/NAME/` and load it |
| `plugin remove PLUGIN_ID` | take the plugin out of `workspace.json`; its folder stays. `BUSY` while it has active runs |
| `remove [--yes]` | remove Mira from this project (see below) |
| `terminal RUN`, `input RUN --text TEXT \| --key KEY [--expected-screen-revision N]` | a PTY screen and its input. Keys: enter, tab, escape, backspace, delete, up, down, left, right, ctrl-c, ctrl-d, ctrl-z |
| `notify --title TITLE MESSAGE [--run RUN]` | a desktop notification from a running program; the run defaults to `MIRA_RUN_ID` |
| `schedule ACTION on\|off` | an interval switch; it runs only in a session |
| `payload read TOKEN [--offset N] [--max-bytes N]` | read a large result in chunks of at most 16 KiB |
| `artifacts [RUN]`, `artifacts read ID` | saved run outputs |
| `storage status [--all]`, `storage gc [--kind K] [--apply]`, `storage clear --plugin ID --kind state` | disk use and cleanup; `gc` only plans without `--apply` |
| `skills export DIR... [--force]` | copy these skills to a user skills folder, never into a Git work tree |
| `update [--check\|--rollback]` | install the latest release, only check for one, or go back to the previous binary |
| `doctor`, `paths`, `schema NAME` | checks, locations, JSON Schemas |

Without a session the host exits when idle, and the next command starts a new one with a new `host_epoch`. Compare `state_revision` values only within one `host_epoch`.

## Remove Mira from a project

Do this only when the user asks. It cannot be undone for files that are not in Git.

1. `mira remove --json` deletes nothing. It returns the `paths` it would delete (`.mira/` and the project's run history, logs, and cache) and the `active_runs` it would stop. Show them to the user.
2. `mira remove --yes --json` stops the project's work and deletes those paths. `BUSY` means a Mira window is open for the project: ask the user to close it, then run the command again.
3. Remove the `.mira` lines that setup added to `.gitignore` or `.git/info/exclude`. If `.mira/` was committed, tell the user that the deletion is an uncommitted change.

The `mira` program, the skills, and other projects stay as they are.

## Updates

`mira update` works for a binary that the installer placed. It checks the download, keeps the old binary as `mira.previous`, and exports the skills again; skill failures are warnings. The reply has the release notes and the project hosts that still run the old version, each with a stop command. A host changes version when its work stops; open windows keep the old version until they close. Set `MIRA_NO_UPDATE_CHECK` to stop the daily check in the TUI.

## Exit codes

0 success · 1 `doctor` found a failing check · 2 invalid argument, schema, or frame · 3 not found or not set up · 4 conflict (session, busy, revision, view changed) · 5 the plugin or command failed · 6 timeout · 7 IPC or Core error · 8 storage unavailable · 130 cancelled.

## Frequent error codes

| Code | Meaning | Do |
|---|---|---|
| `SESSION_REQUIRED` | long-running work has no owner | ask whether to `up --background`, or let the human open the TUI |
| `BUSY` | the task already runs | wait for it (`runs RUN`) or stop it |
| `ALREADY_RUNNING_DIFFERENT_INPUT` | a service runs with other input or an older definition | `restart` if intended |
| `REQUEST_KEY_CONFLICT` | same key, different input | use the first input, or a new key for new work |
| `REVISION_CONFLICT` | the catalog changed since you read it | read the catalog again, merge, apply again |
| `VIEW_CHANGED` | the table you acted on changed | read the view again and choose again |
| `PROTOCOL_MISMATCH` | a host from another Mira version runs for this workspace | run the error's `next_action` |
| `OUTCOME_UNKNOWN` | the host stopped before it knew the result | inspect the state before you repeat side effects |
| `STORAGE_UNAVAILABLE` | a record could not be saved; nothing started | report it; `stop` and `down` still work |
| `INPUT_BUSY` / `SCREEN_CHANGED` | another client holds the terminal / the screen moved on | read `terminal RUN` again, then retry |
| `PAYLOAD_GONE` / `CURSOR_EXPIRED` | the data was cleaned up / the page is gone | read the run summary; do not rerun only to recreate history |
