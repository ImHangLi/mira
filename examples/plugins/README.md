# Example plugins

Small plugins you can copy into your own `.mira/plugins/` and list in `.mira/workspace.json`. They use only standard tools and the Python standard library, and they never use the network.

- `find`: a command plugin with no script. `plugin.json` puts the input values into `argv` (`{input.name}`, `{input.folder}`), and the TUI shows a form for them. The input schema accepts only a relative folder without `..` and values without a leading dash, so a value cannot leave the workspace or become a `find` option.
- `todos`: a structured plugin that shows TODO markers as a table. Select a row to run `todos.show`, which shows the lines around that match.
- `server`: a service that logs one request per second and, now and then, an error.
- `errors`: a view with no script. `plugin.json` declares a live log view over `server.run` that keeps only the lines with "error", so you and your agent read the same filtered log.
- `daily`: a scheduled task that drafts a short daily update from git and TODO markers. Turn it on with `mira schedule daily.draft on`.

Check them with `mira validate examples/plugins/.mira`.
