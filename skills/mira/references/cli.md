# CLI reference

All commands accept `--project PATH`, `--json`, `--text`. `REF` is `plugin.item`; `RUN` is a run ID (`r_…`) or a unique prefix of one, such as `r_fb60aacf`; `logs`, `stop`, `terminal`, and `input` also take an action ref.

`mira --help` does not list `apply`, `view-action`, `artifacts`, `payload`, `storage`, `paths`, and `schema`, but they work as shown here.

| Command | Notes |
|---|---|
| `mira` | TUI for humans (needs a TTY; otherwise `TTY_REQUIRED`) |
| `status` | session, controller count, background expiry, active runs, warnings |
| `catalog [--search TEXT] [--if-revision N --if-workspace W] [--limit N] [--after CURSOR] [--max-bytes N]` | bounded tool list, paged by cursor |
| `describe REF [--include-schema]` | purpose, runner, cwd, env names (never values), effects, invoke hint |
| `run ACTION [--input FILE\|-] [--no-wait] [--request-key K]` | task; waits unless `--no-wait` (needs a session) |
| `start ACTION [--input FILE\|-] [--request-key K]` | process; reuses the same running instance |
| `stop RUN_OR_ACTION [--wait]` | TERM (or INT), then KILL after the grace period; cleanup runs once |
| `restart ACTION [--input FILE\|-]` | stop and wait, then start with the current definition |
| `exec --label TEXT -- ARGV...` | ad-hoc task through the managed path; not added to the catalog |
| `up --background [--ttl 30m\|2h\|none]` | explicit background lease (default 2 h); run it again to set a new limit, counted from now |
| `down [--wait]` | stop the session and its runs; no data is deleted |
| `runs [RUN] [--action REF] [--outcome VALUE] [--limit N] [--after CURSOR]` | newest first |
| `logs RUN_OR_ACTION [--after CURSOR] [--limit N] [--max-bytes N] [--follow] [--grep PATTERN] [--stream stdout\|stderr]` | tail by default; `--grep` keeps records that contain PATTERN (case-insensitive) in the page and with `--follow` |
| `view VIEW [--after CURSOR] [--limit N]`, `view-action VIEW ACTION --row ROW --expected-view-revision N` | typed data; row actions refuse stale rows (`VIEW_CHANGED`) |
| `publish VIEW --input FILE\|- [--expected-view-revision N] [--request-key K]` | write one view frame without running a plugin |
| `validate PATH`, `apply DRAFT_DIR --expected-revision N`, `apply PLUGIN_DIR [--expected-revision N]`, `reload` | config changes (see mira-extend). A plugin folder is checked alone and, inside a project, against its `.mira`; `apply` copies an outside folder to `.mira/plugins/<id>/` and lists it in `workspace.json`. Without `--expected-revision` it uses the current revision |
| `plugin remove PLUGIN_ID` | removes the plugin's entry from `.mira/workspace.json` and reloads; the plugin folder stays on disk. `BUSY` while the plugin has active runs (stop them first); `NOT_FOUND` for an unknown ID. People use `x` in the TUI |
| `payload read TOKEN [--pointer P] [--offset N] [--max-bytes N]` | continue reading a large result or view in ≤16 KiB chunks; `PAYLOAD_GONE` = cleaned up |
| `terminal RUN`, `input RUN --text TEXT \| --key KEY [--expected-screen-revision N]` | PTY screen and serial input (keys: enter, tab, escape, backspace, up/down/left/right, ctrl-c, ctrl-d, ctrl-z) |
| `schedule ACTION on\|off` | persisted interval switch; runs only inside a session |
| `artifacts [RUN]`, `artifacts read ID` | registered run outputs, bounded text reads |
| `storage status [--all]`, `storage gc [--kind K] [--apply]`, `storage clear --plugin ID --kind state` | usage and retention; gc only plans without `--apply` |
| `skills export DIR [--force]` | copy these skills to `DIR/mira/` and `DIR/mira-extend/` (a user skills folder, never inside the current Git work tree); refuses to overwrite existing files without `--force` |
| `doctor`, `paths`, `schema NAME` | diagnostics, locations, JSON Schemas |

## Exit codes

0 success · 1 `doctor` found a failing check · 2 invalid argument/schema/frame · 3 not found / not set up · 4 conflict (session, busy, revision, view changed) · 5 plugin or command failed · 6 timeout · 7 IPC/Core error · 8 required storage unavailable · 130 cancelled.

## Frequent error codes

| Code | Meaning | Do |
|---|---|---|
| `SESSION_REQUIRED` | no owner for long-running work | ask whether to `up --background`, or let the human open the TUI |
| `BUSY` | the task already runs | wait for it (`runs RUN`) or stop it |
| `ALREADY_RUNNING_DIFFERENT_INPUT` | a service runs with other input/definition | `restart` if intended |
| `REQUEST_KEY_CONFLICT` | same key, different input | use the original input or a new key for new work |
| `REVISION_CONFLICT` | catalog changed since you read it | re-read catalog, rebase your draft, apply again |
| `VIEW_CHANGED` | the table you acted on changed | re-read the view and choose again |
| `PROTOCOL_MISMATCH` | a host from another Mira build still runs for this workspace | run the error's `next_action` (`kill PID`), or `mira down` with that build; the next command starts this build |
| `OUTCOME_UNKNOWN` | the host stopped before the result was known | inspect state before repeating side effects |
| `STORAGE_UNAVAILABLE` | a required record could not be saved; nothing new started | report it; `stop`/`down` still work |
| `INPUT_BUSY` / `SCREEN_CHANGED` | someone else holds the terminal / the screen moved on | re-read `terminal RUN`, then retry |
| `PAYLOAD_GONE` / `CURSOR_EXPIRED` | the data was cleaned up / the page no longer exists | read the run summary; do not rerun just to recreate history |
