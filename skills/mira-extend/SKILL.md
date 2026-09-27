---
name: mira-extend
description: Create or change Mira plugins — save a one-off command as a reusable tool, add a structured table/log panel, or adjust an existing action — then validate and apply without changing Mira itself. Use when the user wants a new or modified project tool in `.mira/`. For running and reading existing tools, use the mira skill.
---

# Extend Mira

A plugin is a directory with `plugin.json` and optional scripts. New capability never needs a change to Mira itself or a rebuild.

## 1. Reuse before creating

```sh
mira catalog --search "one or two distinctive words" --json
mira describe PLUGIN.ITEM --include-schema --json
```

If a tool already does most of it, change that plugin (new input, new action next to it) instead of adding a near-duplicate. Tell the user which one you reused.

## 2. Choose the runner

| Need | Use |
|---|---|
| Run an existing command; output is just logs | `"run": {"kind": "command", "argv": [...]}` — manifest only, no script |
| Interactive program (prompts, full-screen) | command runner with `"terminal": "pty"` |
| Structured results: tables, trees, grouped logs, row actions | `"run": {"kind": "plugin"}` + plugin `entry`, speaking MPP/1 on stdout |
| Messages published by agents or hooks, nothing to execute | a view-only plugin (no actions) and `mira publish` |
| Filter or combine another tool's logs (errors only, one stream) | a derived log view: `"kind": "log", "source": {"logs": "PLUGIN.ACTION", "grep": "error"}` — manifest only, no process |

Pass argv as a list; there is no implicit shell. If you need a shell, write `["/bin/zsh", "-lc", "…"]` and never put a placeholder or user input into that string — take input from `MIRA_INPUT_FILE`.

Saving a command that worked (`mira exec … -- ARGV`): turn it into a command action with a clear `title`, a `description` that says what problem it solves, the right `cwd`, and an `input_schema` for the parts that change between uses.

When arguments change between uses (a port, a test path), put `{input.NAME}` in `run.argv`: `["npm", "run", "dev", "--", "--port={input.port}"]`. NAME must be a top-level property of `input_schema`; the host fills in the effective input (defaults included) before it starts the command, and never splits or interprets the value. See the `serve-on-port` action in the [command template](templates/command/plugin.json). Use a small wrapper such as [with_input.py](templates/command/with_input.py) only when the arguments need logic: optional flags, lists, or computed values. `MIRA_INPUT_FILE` always holds the full input JSON.

To show only part of another action's output (errors, one stream), declare a derived log view instead of writing a plugin that follows logs. The host filters the source run's log, so the TUI and `mira view` show the same lines. See [manifest](references/manifest.md#derived-log-view).

## 3. Write it

Two ways to write it:
- **Directly** (simplest, for one plugin): write the plugin folder (in `.mira/plugins/<id>/` or anywhere else), then `mira validate DIR --json` and `mira apply DIR --json`; apply copies an outside folder to `.mira/plugins/<id>/` and adds it to `.mira/workspace.json` when it is new.
- **As a draft** (safe when others may change the catalog at the same time): copy `workspace.json` and `plugins/` from `.mira` into `.mira/.drafts/<name>/`, edit there, then validate and apply the draft as in step 4. Delete the draft after it is applied. Field reference: [manifest](references/manifest.md). Structured output: [MPP/1 and views](references/protocol.md). Starting points: [command template](templates/command/plugin.json), [structured template](templates/structured/).
- **Remove a plugin:** `mira plugin remove PLUGIN_ID --json`. It takes the plugin out of `.mira/workspace.json` and keeps its folder. Delete the folder only if the user asks.

Rules that matter:
- Action IDs and view IDs share one namespace inside a plugin: an action `slowest` and a view `slowest` conflict.
- Relative paths: a command's `argv` and `cwd` resolve from the workspace root (`cwd` defaults to `.`); a structured plugin's `entry` runs in the plugin directory. Use `MIRA_WORKSPACE_ROOT` for project paths inside scripts.
- stdout of a structured plugin carries only protocol frames; debug output goes to stderr. Exactly one `result` frame, last, for tasks; none for processes.
- Keep secrets out of stdout, views, and manifests. Put required env names in the docs; values come from the user's environment or `env_files`.
- Write private files to `MIRA_STATE_DIR`, rebuildable files to `MIRA_CACHE_DIR`, run outputs to `MIRA_ARTIFACT_DIR`. Never scatter logs in the repo.
- Long-running or polling work must be an explicit `process` or a `schedule` the user enables; saving a plugin never starts it. Give anything that creates external resources (containers, tunnels) a `cleanup` that removes only what it created.
- Prefer a standard view (table with row actions, log, tree) so the human can use the same tool in the TUI. Do not build a second implementation for humans.

## 4. Validate, apply, prove

```sh
mira validate .mira/.drafts/<name> --json
mira apply .mira/.drafts/<name> --expected-revision N --json   # N = catalog_revision you read
mira run PLUGIN.ACTION --input sample.json --json    # the normal case
mira run PLUGIN.ACTION --input bad.json --json       # bad input must fail clearly (SCHEMA_INVALID)
mira view PLUGIN.VIEW --json                          # structured output is readable
```

- `REVISION_CONFLICT`: someone else applied first. Re-read the catalog, merge onto the current files, apply again. Never overwrite blindly.
- `CONFIG_APPLY_INCOMPLETE`: some files were written. Fix them and run `mira reload`.
- Check a missing dependency path too (for example the tool is not installed) and make the error readable.
- A small input sample next to the plugin is fine; do not build a test suite for each plugin.

## 5. Report

Return the refs you created or changed, the shortest invocation (`mira run …`), what you verified, and the limits. Mention that the human sees the same tool in the TUI (`mira`). Do not paste the whole implementation.
