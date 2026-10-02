# MPP/1 and views

A structured plugin reads one JSON line (the invocation) from stdin, then writes one JSON object per line to stdout. stdout is the protocol; stderr is free-form diagnostics.

## Invocation (stdin)

`{"api":1,"run_id":"r_…","action":"scan","input":{...},"config":{...},"context":{"workspace_id","workspace_root","cwd","plugin_dir","state_dir","cache_dir","artifact_dir"}}`. Run project commands with `context.cwd` explicitly; the process cwd is the plugin directory.

## Frames (stdout)

Every frame has `"api": 1` and `"type"`. At most 1 MiB per line.

| type | fields |
|---|---|
| `log` | `level` (debug/info/warn/error), `text` ≤ 8 KiB, optional `fields` |
| `progress` | `message`, optional `current` and `total` together |
| `status` | `state` (healthy/warn/error/unknown), `message` — reported health, not the run's lifecycle |
| `view` | `view_id`, `op` (`replace`, or `append` for log views), `data` |
| `notify` | `title` ≤ 200 bytes, `message` ≤ 2 KiB, both without control characters. An open Mira TUI shows it and asks the terminal for a desktop notification (OSC 9: Ghostty, iTerm2, WezTerm). It is also written to the run log. Do not call `osascript` or `notify-send` yourself. A program in a PTY (not MPP/1) runs `"$MIRA_BIN" notify --title TITLE MESSAGE` instead |
| `artifact` | `path`, `mime`, `label`, `ownership` (`managed` = inside `artifact_dir`; `external` = reference only) |
| `result` | `ok`, `summary`, `data` (may be `null`), and `error` `{code, message, retryable}` exactly when `ok` is false |

A task must end with exactly one `result`. A success result followed by a non-zero exit is a failure. Process actions never emit `result`.

## View data

| kind | data |
|---|---|
| `text` | `{"kind":"text","text":"…","format":"plain"\|"markdown"}` |
| `table` | `{"kind":"table","columns":[{"id","label","type":"text\|number\|boolean\|timestamp"}],"rows":[{"id","values":{colId: value\|null}}]}` — every row has every column; unique IDs |
| `log` | `{"kind":"log","items":[{"id","text","level"}]}` — reuse an ID only for identical content |
| `tree` | `{"kind":"tree","nodes":[{"id","label","children":[...]}]}` |
| `json` | `{"kind":"json","value":…}` |

An invalid update is rejected whole and the previous data stays (marked stale). Timestamps are UTC like `2026-09-26T01:34:00.000Z`. Numbers must be finite; large integers go in strings.

## Following another run's logs

To only filter another action's log (for example, errors), a [derived log view](manifest.md#derived-log-view) needs no code. Follow the log from a plugin when you need logic on each line, such as grouping or parsing.

`"$MIRA_BIN" --project="$MIRA_WORKSPACE_ROOT" logs --follow --json [--grep=TEXT] REF` prints one JSON object per line: first `{"type":"ready",...}`, then `{"type":"log","data":{"run_id":"r_…","records":[{"log_seq","recorded_at","stream","level","text",...}]}}` for new records, and it exits after `{"type":"end",...}` or when the run ends. A line with `"ok": false` is an error reply instead (for example, the action never ran). Pass values as `--flag=value` and check REF, so input never becomes an option.

## Example

[templates/structured/main.py](../templates/structured/main.py) reads the invocation, publishes a table, and ends with a `result` frame for success and for failure. Start from it.
