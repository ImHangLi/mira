---
name: mira-extend
description: Create or change Mira plugins, then validate and apply them without changing Mira itself. Covers saving a command as a tool, adding a table or log panel, and changing an action. Use when the user wants a new or changed project tool in `.mira/`. To run and read existing tools, use the mira skill.
---

# Extend Mira

A plugin is a folder with `plugin.json` and optional scripts. A new tool never needs a change to Mira or a rebuild.

## 1. Reuse first

```sh
mira catalog --search "one or two distinctive words" --json
mira describe PLUGIN.ITEM --include-schema --json
```

If a tool already does most of the job, change that plugin instead of adding a near-duplicate. Tell the user which one you reused.

## 2. A plain command: save it in one step

```sh
mira save "Unit tests" --json -- npm test              # a task that ends
mira save "Web app" --service --json -- npm run dev    # a long-running service
```

This adds a command action to the `tools` plugin (`--plugin ID` for another), validates the project, and loads it. The reply has the new ref (`tools.unit-tests`). From a subfolder, that folder is the tool's `cwd`. Use `--id` when the title clashes and `--description` for one clear sentence.

Write `plugin.json` yourself only for inputs, schedules, views, or structured output.

## 3. Choose the kind

| Need | Use |
|---|---|
| An existing command; the output is logs | `"run": {"kind": "command", "argv": [...]}`; no script |
| An interactive or full-screen program | a command with `"terminal": "pty"` |
| Tables, trees, grouped logs, row actions | `"run": {"kind": "plugin"}` and a plugin `entry` that writes MPP/1 to stdout |
| Messages from agents or hooks; nothing runs | a plugin with only views, and `mira publish` |
| A part of another tool's log, such as its errors | a derived log view: `"source": {"logs": "PLUGIN.ACTION", "grep": "error"}`; no process |

- `argv` is a list, with no shell. If you need one, use `["/bin/zsh", "-lc", "…"]`, and never put a placeholder or user input into that string.
- For a value that changes between uses (a port, a test path), put `{input.NAME}` in `argv` and declare NAME in `input_schema`. Use a wrapper such as [with_input.py](templates/command/with_input.py) only when the arguments need logic.

Details: [manifest](references/manifest.md) for every field, and [MPP/1 and views](references/protocol.md) for structured output. Start from the [command template](templates/command/plugin.json) or the [structured template](templates/structured/).

## 4. Write and apply

- **Directly** (one plugin): write the folder, in `.mira/plugins/<id>/` or elsewhere, then `mira validate DIR --json` and `mira apply DIR --json`. `apply` copies an outside folder into `.mira/plugins/<id>/` and adds a new plugin to `.mira/workspace.json`.
- **As a draft** (when others can change the catalog at the same time): copy `workspace.json` and `plugins/` from `.mira` to `.mira/.drafts/<name>/`, edit there, then:

  ```sh
  mira validate .mira/.drafts/<name> --json
  mira apply .mira/.drafts/<name> --expected-revision N --json   # N = the catalog_revision you read
  ```

  Delete the draft after it is applied.
- **Remove a plugin:** `mira plugin remove PLUGIN_ID --json`. Its folder stays; delete the folder only if the user asks.

`REVISION_CONFLICT` means someone applied first: read the catalog again, merge, and apply again. Never overwrite blindly. `CONFIG_APPLY_INCOMPLETE` means some files were written: fix them and run `mira reload`.

## 5. Rules

- Action IDs and view IDs share one namespace in a plugin.
- A command's `argv` and `cwd` resolve from the workspace root. A structured plugin's `entry` runs in the plugin folder; use `MIRA_WORKSPACE_ROOT` for project paths.
- A structured plugin writes only protocol frames to stdout, and debug output to stderr. A task ends with exactly one `result` frame; a process writes none.
- Keep secrets out of stdout, views, and manifests. Name the required env variables in the docs; values come from the user's environment or `env_files`.
- Write private files to `MIRA_STATE_DIR`, rebuildable files to `MIRA_CACHE_DIR`, and run outputs to `MIRA_ARTIFACT_DIR`. Never write logs into the repository.
- Long-running or polling work is a `process`, or a `schedule` that the user turns on. The host never starts it on its own.
- Use `"show": "on_select"` only for a PTY app page that is safe to open with default inputs, such as a timer or a game. Keep servers and shells on explicit start.
- Give anything that creates external resources (containers, tunnels) a `cleanup` that removes only what it created.
- Prefer a standard view (table with row actions, log, tree), so the human uses the same tool in the TUI.

## 6. Prove it

```sh
mira run PLUGIN.ACTION --input sample.json --json   # the normal case
mira run PLUGIN.ACTION --input bad.json --json      # bad input fails clearly (SCHEMA_INVALID)
mira view PLUGIN.VIEW --json                        # the structured output is readable
```

Also check a missing dependency, such as a tool that is not installed, and make that error readable. A small input sample next to the plugin is enough; do not build a test suite for a plugin.

## 7. Report

Give the refs you created or changed, the shortest invocation, what you verified, and the limits. Say that the human sees the same tool in the TUI. Do not paste the implementation.

To give a plugin to someone else, see [sharing](references/sharing.md).
