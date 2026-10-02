# Mira by hand

This guide is for people. It shows how to set up Mira, write plugins, and share them without an agent. An agent can do all of this too, but you never need one.

## Install and open

```sh
curl -fsSL https://raw.githubusercontent.com/ImHangLi/mira/main/scripts/install.sh | sh
```

Open a new terminal, go to your project, and run `mira`. Without a `.mira/` folder, Mira tells you what to create.

| Key | Does |
|---|---|
| `Ctrl-T` | Return to tools from any pane, dialog, or program; release the program's input lock |
| `j` / `k` | Move through the tools |
| `Enter` | Run a task, start a service, or open a view |
| `s` | Start or stop a service |
| `Enter` on a terminal program | Type into it. Its screen shows in the main pane while it runs and is selected |
| `R` | Refresh: load `.mira` again and show new tools (Mira also does this by itself) |
| `x` | Remove the selected plugin from the project (its files stay) |
| `+` | Add a default plugin (shown at the bottom of the tool list until you have them all) |
| `/` | Search the tools |
| `:` | Run a `mira` command, for example `:exec --label "disk usage" -- du -sh .` |
| `b` | Keep services running after you close the window |
| `?` | Show every key |
| Mouse | Click to select, double-click to open, wheel scrolls what is under the pointer; hold Shift to select text (`m` turns the mouse off). |

`Shift+Esc` also returns to tools in terminals that support the enhanced keyboard protocol. `Ctrl-]` works too. Plain Esc and Tab stay available to the program while you type into it.

A service starts when you start it. It keeps running while you look at other tools. It stops when you press `s`, or when you close the last Mira window (unless you pressed `b`).

An app page (`show: "on_select"`) is a program that is simply there, such as Notes, System, Pomodoro, and Snake. Its row shows `▣` and no run state or time, and it has no start or stop key. Selecting it opens its screen, without giving it the keys. It keeps running while you look at other tools, so a timer keeps counting. Press `r` to reload it. After it exits, select it again or press Enter to open it again.

## Update

Run `mira update` to install the latest release and refresh the skills you exported with
`mira skills export`. It checks the download checksum and keeps `mira.previous` beside
the installed binary. `mira update --rollback` swaps the two binaries.

Use `mira update --check` to check the latest version without installing it. The TUI
checks once a day in the background and shows a notice when an update is available.
Set `MIRA_NO_UPDATE_CHECK=1` to turn off this check; it is also off when `CI` is set.
`mira doctor` shows the running version and the latest version in the local cache.

Close and reopen Mira windows to use the new version. Project hosts switch after their
work stops and their sessions end. An open window or background lease can keep a host
alive. The update reply lists existing hosts and the command to stop each one with
`mira.previous`. Skill export or Skillshare sync failures appear as warnings; they do
not undo the binary update.

## Start with a default plugin

Mira ships with a few plugins that work in any project:

```sh
mira plugin add            # list them
mira plugin add notes      # a live Markdown notes page; you or your agent edit it
mira plugin add system     # a live system monitor: CPU, memory, disk, network, processes
mira plugin add pomodoro   # a focus timer that counts your pomodoros
mira plugin add snake      # a game of Snake, for the build that takes too long
```

In the TUI, press `+` for the same list. Each one is copied to `.mira/plugins/NAME/` as ordinary files. Read them, change them, or remove them like any other plugin. In a project without `.mira/`, the first `mira plugin add` also creates `.mira/workspace.json`.

## Your first plugin

The fastest way: save a command you already use.

```sh
mira save "Unit tests" -- npm test
mira save "Web app" --service -- npm run dev
```

Each command becomes a tool in the `tools` plugin, is checked, and shows in `mira` at once. Run `mira save` from a subfolder and the tool runs there. Inside the TUI, type `:save "Unit tests" -- npm test`.

For inputs, views, schedules, or anything else, write the plugin yourself. A plugin is a folder in `.mira/plugins/` with a `plugin.json`. The project lists its plugins in `.mira/workspace.json`:

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
| A CLI inside Mira (top, a REPL, a TUI app) | `"terminal": "pty"` on the action. Its running screen shows when selected; Enter starts it or gives it the keys. For a safe app page, add `"show": "on_select"`. | `shell` |
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

## Remove Mira from a project

Run `mira remove` in the project. It shows what it stops and deletes: `.mira/` and the project's run history, logs, and cache. Nothing is deleted until you run `mira remove --yes`. Close the project's Mira windows first.

The `mira` program, your agent skills, and your other projects stay as they are.

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
| `mira save TITLE -- COMMAND` | Save a command as a tool (`--service` for a server) |
| `mira doctor` | Check the project and the programs it needs |
| `mira logs --host` | Read Mira's own log when something goes wrong |
| `mira plugin add NAME` / `mira plugin remove ID` | Add a default plugin, or remove a plugin |
| `mira remove` | Remove Mira from this project (`--yes` to delete) |
