# Architecture

Mira is one binary, `mira`. It runs as three kinds of process: the TUI for people, the CLI for agents and scripts, and one host per workspace. The host owns every fact. The TUI and the CLI are two views of the same host, over the same protocol.

## Processes

```mermaid
flowchart LR
  human([You]) --> tui["mira<br/>TUI"]
  agent([Your agent]) --> cli["mira run · logs · view …<br/>CLI, JSON"]
  tui -- "MIPC/1<br/>Unix socket" --> host
  cli -- "MIPC/1<br/>Unix socket" --> host
  subgraph host["mira __host · one per workspace"]
    actor["workspace actor"]
  end
  actor -- "MPP/1<br/>stdin JSON, stdout frames" --> plugins["plugin actions<br/>.mira/plugins/*"]
  actor --> procs["processes and PTYs<br/>dev servers, tests, one-off runs"]
  actor --> disk[("SQLite ledger<br/>run logs · view data")]
```

- **MIPC/1** is JSON-RPC 2.0 over a Unix socket in the runtime folder (`mira paths`). A client starts the host when none is running. The host exits when idle, unless a session keeps it: an open TUI, or a background lease from `mira up --background`.
- **MPP/1** is how a plugin action reports results, health, progress, and view data. A plain command needs no protocol at all.
- Nothing starts unless a person or an agent asks. The host records every child process group on disk, so it can clean up after a crash.

## Crates

```mermaid
flowchart TD
  cli["mira-cli<br/>the mira binary"] --> tui["mira-tui"]
  cli --> client["mira-client"]
  cli --> host["mira-host"]
  tui --> client
  client --> protocol["mira-protocol"]
  host --> protocol
  tui --> protocol
  cli --> protocol
```

The graph has no cycles. `mira-protocol` depends on no other Mira crate. The TUI and the CLI reach the host only through `mira-client`. `mira-cli` links `mira-host` only to run it as `mira __host`.

| Crate | Owns |
|---|---|
| `mira-protocol` | The contract: IDs, manifests, MIPC/1 and MPP/1 messages, run and view types, errors, limits, and the generated JSON Schemas in `schemas/`. Also the small rules both sides share: catalog ranking and clock text. |
| `mira-host` | The per-workspace host: the socket, the workspace actor, the process and PTY runners, run logs, and storage. |
| `mira-client` | A typed MIPC/1 client, shared by the CLI and the TUI. It finds or starts the host. |
| `mira-tui` | The human TUI: a projection of host state. It holds presentation state only. |
| `mira-cli` | Argument parsing, one command module per command group, and the output rules (`--json` for agents, text for people). |

## Inside the host

```mermaid
flowchart LR
  server["server<br/>socket, connections"] --> actor
  subgraph actor["actor · the only writer"]
    direction TB
    configure["configure<br/>validate, apply"]
    items["items<br/>catalog, describe"]
    runs["runs<br/>prepare, start, launch, lifecycle"]
    views["views<br/>derived, queue, persist, publish"]
    streams["streams<br/>bounded subscriptions"]
    terminal["terminal<br/>PTY snapshot, input"]
    retention["retention<br/>history, GC"]
  end
  runs --> runner["runner<br/>pipe process groups"]
  runs --> pty["pty<br/>virtual screen, transcript"]
  runner --> prunner["plugin_runner<br/>MPP/1 frames"]
  runs --> logs["logs<br/>JSONL segments"]
  actor --> storage[("storage<br/>SQLite on one thread")]
  runner --> groups["groups<br/>live process ledger"]
  pty --> groups
```

One actor task owns all workspace state, so there are no locks around it. Runners report facts to the actor as messages. Storage runs on its own thread, and only the actor calls it.

## Inside the TUI

```mermaid
flowchart LR
  keys["term<br/>keys, resize"] --> app
  ipc["ipc<br/>worker tasks"] -- "host events" --> app
  subgraph app["app · state and one key router"]
    direction TB
    state["state · projection"]
    input["keys · bindings · commands"]
    act["actions · selection · notices"]
  end
  app -- "requests" --> ipc
  app --> ui["ui<br/>header, sidebar, main pane, panels, footer"]
  ui --> theme["theme<br/>light and dark palettes"]
```

The UI never waits on IPC. Requests go to worker tasks, and their results come back as events. Each frame draws from the current state only.

## Repository

| Path | Contents |
|---|---|
| `crates/` | The five crates above. |
| `schemas/` | JSON Schemas generated from `mira-protocol`. `scripts/check-contract.sh` fails when they drift. |
| `skills/` | The agent skills that `mira skills export` copies: `mira` (use tools) and `mira-extend` (write plugins). |
| `plugins/` | The default plugins that `mira plugin add NAME` copies into a project: `pomodoro`, `ports`, and `snake`. Built into the binary like the skills. |
| `examples/plugins/` | A workspace of example plugins: a command with inputs, a table with a row action, a service, a derived log view, and a routine. |
| `tests/fixtures/` | The workspace the tests run on, and invalid manifests the validator must reject. |
| `scripts/` | The installer, the release packager, and the contract check. |
| `docs/agents.md` | Setup steps for an agent. |
