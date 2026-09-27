# Mira by hand

This guide is for people. It shows how to set up Mira, write plugins, and share them without an agent. An agent can do all of this too, but you never need one.

## Install and open

```sh
curl -fsSL https://raw.githubusercontent.com/ImHangLi/mira/main/scripts/install.sh | sh
```

Open a new terminal, go to your project, and run `mira`. Without a `.mira/` folder, Mira tells you what to create.

| Key | Does |
|---|---|
| `j` / `k` | Move through the tools |
| `Enter` | Run a task, start a service, or open a view |
| `s` | Start or stop a service |
| `a` | Type into a terminal program; `Ctrl-]` goes back to Mira |
| `x` | Remove the selected plugin from the project (its files stay) |
| `/` | Search the tools |
| `:` | Run a `mira` command, for example `:exec --label "disk usage" -- du -sh .` |
| `b` | Keep services running after you close the window |
| `?` | Show every key |

A service starts only when you start it. It keeps running while you look at other tools. It stops when you press `s` again, or when you close the last Mira window (unless you pressed `b`).

## Your first plugin

A plugin is a folder in `.mira/plugins/` with a `plugin.json`. The project lists its plugins in `.mira/workspace.json`:

```json
{"api": 1, "name": "My project", "plugins": ["plugins/dev"], "autostart": []}
```

`.mira/plugins/dev/plugin.json`: a dev server and a test run.

```json
{
  "api": 1, "id": "dev", "name": "Dev", "description": "The dev server and the tests.",
  "actions": [
    {"id": "web", "title": "Web app", "description": "The dev server.", "mode": "process",
     "run": {"kind": "command", "argv": ["npm", "run", "dev"]}},
    {"id": "test", "title": "Unit tests", "description": "Run the tests once.", "mode": "task",
     "run": {"kind": "command", "argv": ["npm", "test"]}}
  ]
}
```

- `mode: "process"` keeps running (a server). `mode: "task"` ends (a check).
- `argv` is the command as a list of words. It does not go through a shell.

Check it with `mira validate .mira`. Mira reloads the files by itself when you save them; `mira reload` does it at once.

## Kinds of plugins

Each kind is one small change to `plugin.json`. The [example plugins](../examples/plugins/) have a working version of each.

| You want | Add | Example |
|---|---|---|
| Only the errors of a log, live | A view with `"source": {"logs": "dev.web", "grep": "error"}`. No code. | `errors` |
| A form for inputs | An `input_schema`, and `{input.NAME}` in `argv` | `find` |
| A CLI inside Mira (top, a REPL, a TUI app) | `"terminal": "pty"` on the action. Press `a` to type into it. | `shell` |
| A table, a text panel, or a tree | A script that prints view frames as JSON lines (`"run": {"kind": "plugin"}`) | `todos`, `disk` |
| Something that runs on a timer | `"schedule": {"every_ms": 600000}`. Turn it on with `t` in the TUI or `mira schedule REF on`. | `daily`, `disk` |
| A view that stays after a restart | `"persistence": "last"` on the view | `disk` |
| A setting people can change | `config` and `config_schema`. A person overrides it in `.mira/local.json`. | |

The full format is in the [manifest reference](../skills/mira-extend/references/manifest.md). The JSON lines a script prints are in the [protocol reference](../skills/mira-extend/references/protocol.md).

## Run anything

A command that is not a tool yet can still run in Mira, so you can see it and read its log later:

```sh
mira exec --label "migrate the local database" -- npm run db:migrate
```

It shows under ONE-OFF RUNS in the TUI. If you run it often, make it a tool.

## Remove a plugin

In the TUI, select any tool of the plugin and press `x`, then confirm. From a shell: `mira plugin remove ID`. Both remove the plugin from `.mira/workspace.json` and keep its folder, so you can add it back later.

## Share a plugin

A plugin is plain files. To share it:

- **With your team:** commit `.mira/workspace.json` and `.mira/plugins/`. Keep `.mira/local.json` (personal settings) out of Git.
- **With anyone:** send the plugin folder. They copy it to their `.mira/plugins/` and add it to their `workspace.json`.
- **With an agent:** ask your agent to "write a prompt that rebuilds this Mira plugin for someone else". Their agent reads the prompt and makes the same tool, fitted to their project.

## Commands

`mira --help` lists every command. The ones you use most:

| Command | Does |
|---|---|
| `mira` | Open the TUI |
| `mira run REF` / `mira start REF` / `mira stop REF` | Run a task, start a service, stop a run |
| `mira logs REF` | Show the latest output of a tool |
| `mira view REF` | Show a view |
| `mira status` | Show what is running |
| `mira down` | Stop everything in this project |
| `mira doctor` | Check the project and the programs it needs |
