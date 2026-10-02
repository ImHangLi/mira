# Manifest reference (api 1)

Strict JSON: unknown fields, duplicate keys, and `null` for optional fields are errors. IDs match `^[a-z][a-z0-9-]{0,47}$`, and action and view IDs share one namespace per plugin. `mira schema plugin` prints the full JSON Schema.

## `.mira/workspace.json`

```json
{"api": 1, "name": "My Project", "plugins": ["plugins/dev"], "autostart": [], "meta": {}}
```

`plugins` are paths relative to `.mira` (or absolute). `autostart` lists process actions started when a human opens a new TUI session; leave it empty unless the user asks.

## `plugin.json`

| Field | Notes |
|---|---|
| `api`, `id`, `name`, `description` | required; the description says what problem the plugin solves |
| `enabled` | default `true`; disabled plugins stay visible but cannot run |
| `tags` | ≤12 short words that help `catalog --search` |
| `entry` | `{"argv": [...]}`; required when any action uses `{"kind":"plugin"}`; runs with cwd = plugin directory |
| `actions`, `views` | at least one of them |
| `config` / `config_schema` | plugin settings (validated); users override them in `.mira/local.json` |
| `docs` | relative Markdown path inside the plugin, read on demand |

## Action

| Field | Default / rule |
|---|---|
| `id`, `title`, `description`, `mode` | required; `mode` is `task` (ends) or `process` (keeps running) |
| `run` | `{"kind":"command","argv":[...]}` or `{"kind":"plugin"}`; command argv may hold `{input.NAME}` placeholders (see below) |
| `cwd` | `.` = workspace root; relative to the root |
| `env_files`, `env` | layered after the caller's env (the CLI's or the TUI's shell, including `PATH`); `MIRA_*` names are reserved |
| `input_schema` | JSON Schema 2020-12 with an object root; top-level `default`s are filled for CLI and TUI alike; `writeOnly` fields are never echoed |
| `output_schema` | validates a successful structured result's `data` |
| `timeout` | tasks default `{"kind":"after","ms":300000}`, processes `{"kind":"none"}`; never `null` |
| `terminal` | `pipe` (default) or `pty` (command runner only) |
| `show` | `on_run` (default) or `on_select`; `on_select` requires a `process` with `terminal: "pty"` and valid input defaults |
| `stop_signal`, `stop_grace_ms` | `term`/`interrupt`, 100–60000 ms (default 5000), then SIGKILL |
| `cleanup` | `{"kind":"command","argv":[...]}`; runs once after the main process with `MIRA_STOP_REASON` |
| `schedule` | tasks only: `{"every_ms": ≥1000, "params": {...}, "run_on_start": false}`; off until the user enables it |
| `effects` | descriptive tags such as `read-files`, `writes-state`, `network` |

The child receives `MIRA_WORKSPACE_ROOT`, `MIRA_PLUGIN_DIR`, `MIRA_STATE_DIR`, `MIRA_CACHE_DIR`, `MIRA_ARTIFACT_DIR`, `MIRA_RUN_ID`, `MIRA_INPUT_FILE` (the effective input JSON), and `MIRA_CONFIG_FILE`. To call Mira from a plugin, run `"$MIRA_BIN"`: it reaches the same host.

### Open a program on selection

`"show": "on_select"` makes an app page, such as a timer or a game: the TUI opens it when the human selects it, with the schema's default inputs and no input form. The page shows no run state and no start or stop key; `r` reloads it. The program keeps running while the human looks at other tools, and stops with the session or with `mira stop`.

Keep `on_run` for servers, shells, and commands whose start has effects. Only the TUI acts on `show`; the host and the CLI do not.

### Argv placeholders

In a command action's `run.argv`, `{input.NAME}` becomes the effective input value, after schema defaults: a string as it is, a number or boolean as its JSON text (`8080`, `true`). It can be a whole argument (`"{input.port}"`) or part of one (`"--port={input.port}"`). There is no shell, so a value is never split or interpreted.

- NAME must be a top-level property of `input_schema`.
- A missing, `null`, object, or array value fails the run with `SCHEMA_INVALID` before anything starts.
- Only arguments that contain `{input.` are templated. In them, write `{{` and `}}` for literal braces.
- `cwd`, `env`, `cleanup`, and a plugin `entry` are never templated.

For optional flags, lists, or computed values, use a wrapper script that reads `MIRA_INPUT_FILE`.

## View

`{"id","title","kind"}` with kind `text|table|log|tree|json`; `persistence` `last` (kept, bounded by retention) or `session`; tables may add `row_actions: [{"action": "show", "bindings": {"path": "path"}}]` mapping input names to column IDs.

### Derived log view

A log view with `source` shows another action's log lines, filtered by the host; no plugin process runs:

```json
{
  "id": "errors",
  "title": "Web errors",
  "kind": "log",
  "source": {"logs": "dev.web", "grep": "error", "stream": "stderr"}
}
```

- `logs` (required) is the `PLUGIN.ACTION` of any plugin in the workspace.
- `grep` (optional) keeps lines that contain the text, ignoring case. `stream` (optional) is `stdout` or `stderr`.
- The view follows the action's current run, or its latest run. It keeps at most 1000 lines.
- Plugins and `mira publish` cannot write it, and `persistence` does not apply.

## `.mira/local.json` (personal, not committed)

`{"api": 1, "plugin_patches": {"dev": {"actions": [...]}}}` — RFC 7396 merge patches per plugin ID (arrays replace; `null` deletes). Never edit another person's local file.
