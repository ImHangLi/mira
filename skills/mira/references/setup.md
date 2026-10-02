# First setup

Goal: a few ordinary plugins that run this project's real commands, validated and loaded, so the human can open `mira` and use them at once.

1. **Workspace.** Use the directory the user named, otherwise the Git root of the current directory (`workspace.root` in `mira status --json`). On `NEEDS_PROJECT`, ask the user once. Never read outside the project.
2. **Existing state.** If `.mira/workspace.json` exists, read `mira catalog --json` and extend it. Never overwrite `.mira/local.json` (personal overrides).
3. **Read the repository.** Mira does not scan the project. Read the README, `docs/`, script files (`package.json`, `pyproject.toml`, `Makefile`, `justfile`, `Taskfile.yml`), Compose files, CI config, and `.env.example`. In a monorepo, also read the workspace members. Decide what each command does from its source, not its name: a `test` or `migrate` target can have side effects. Never run project code to learn what it does.
4. **Plugins.** Make the tools that are useful on day one: the dev servers, the dependencies that the docs say to start, and the common checks. One `dev` plugin with command actions is usually enough. For a long-running server, add a derived log view of its errors. The mira-extend skill has the manifest format. Do not change Mira itself.
5. **For each action, decide** the `mode` (`task` ends, `process` keeps running), the `cwd`, and the stop signal. Use `terminal: "pty"` only for interactive programs. Add a `cleanup` command for resources that outlive the process, such as detached containers; never one that deletes data (`down -v`).
   - **Plain commands.** `run.argv` is the project's own command. Do not add wrappers or prefixes from your own environment: the human and other agents run these tools too.
   - **Checks must not change files.** Many lint and format tools fix files by default. Use the check-only form (`--check`, `--no-fix`) unless the title says the action fixes.
   - **Executables.** An action gets the environment of the client that starts it: your shell from the CLI, the human's shell from the TUI. Prefer project-local paths (`node_modules/.bin/…`, `.venv/bin/…`), or `["/bin/zsh", "-lc", "…"]` for the login-shell `PATH`.
   - **Install step.** Add it as an action (for example `dev.install`) and run it only when the user agrees.
   - **Interactive setup scripts.** Do not run them from a task. Tell the user, or write the files they create when the docs make the values clear.
6. **Write and load.** Write `.mira/workspace.json` and `.mira/plugins/<id>/plugin.json`, then:
   1. `mira validate .mira --json`. Fix each error until it passes.
   2. `mira reload --json`.
   3. `mira doctor --json`: only `doctor` reports executables that are not installed.
7. **Default plugins.** `mira plugin add --json` lists them. Ask the user once which ones to add, with one short sentence for each. Add each with `mira plugin add NAME --json`.
8. **Share or keep personal.** Ask the user once, and recommend sharing: the next session and the next engineer then reuse the tools. If you cannot ask, keep it personal and tell the user how to share later.
   - **Share:** commit `.mira/workspace.json` and `.mira/plugins/`. Add `.mira/local.json` and `.mira/.drafts/` to the project's `.gitignore` without rewriting it.
   - **Personal:** add `.mira/` to `$(git rev-parse --git-common-dir)/info/exclude`. This covers all worktrees and changes no tracked file.
9. **Verify.** Run one quick task. If the user agrees, start the main service and read its log; a dev server can move to another port, so read the real URL from the log. Stop what you started unless the user wants it running.
10. **Report** in a few lines: the tools you made (refs), what you verified, what you did not verify, and how to stop what still runs. Tell the user to open `mira`.

Ask the user only for credentials, real ambiguity, or preferences that you cannot infer. The skills never go into the project; they live in the user's own skills folder.
