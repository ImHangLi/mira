# First setup

Goal: a small set of ordinary plugins that run this project's real commands, validated and applied, so the human can open `mira` and use them immediately.

1. **Workspace.** Use the directory the user named. Otherwise use the Git repository root of the current directory; `mira status --json` shows the selected root (`workspace.root`). If the current directory is not a project (`NEEDS_PROJECT`), ask the user once which one to use. Never read outside the project.
2. **Existing state.** If `.mira/workspace.json` exists, read `mira catalog --json` first and extend it; never overwrite `.mira/local.json` (personal overrides).
3. **Read the repository.** Mira does not scan the project; read the files yourself. Places to look: README and CONTRIBUTING files and `docs/`; `package.json` scripts and `pyproject.toml` scripts; `Makefile`, `justfile`, `Taskfile.yml`; Compose files (`compose.yaml`, `docker-compose*.yml`); CI config (`.github/workflows/`, `.gitlab-ci.yml`); env examples (`.env.example`). In a monorepo, also check the workspace members. Never run project code to find out what it does.
4. **Meaning.** Decide what each command does from its source, not from its name. A `test` or `migrate` target can have side effects.
5. **Plugins.** Create a few tools that are useful on day one: the dev server(s), required dependencies (e.g. Compose services the docs say to start), and the common checks. Usually one `dev` plugin with command actions is enough. See the mira-extend skill for the manifest format. Do not change Mira itself.
6. **Per action, decide:** `mode` (`task` ends, `process` keeps running), `cwd`, whether it needs a real terminal (`terminal: "pty"` only for interactive programs), the stop signal and grace, and a `cleanup` command for resources the process starts outside its own process group (for example detached containers). Never use `down -v` or any data-deleting cleanup by default.
   - **Checks must not change files.** Many lint and format configs fix files by default (for example ruff `fix = true`). Use the check-only form (`--no-fix`, `--check`) unless the action is clearly titled as a fixer.
   - **Executables.** An action inherits the environment of the client that starts it: your shell for the CLI, the human's shell for the TUI. Tools in per-user locations (`~/.bun/bin`, nvm, pyenv) can be missing from one of them. Prefer project-local paths (`node_modules/.bin/…`, `.venv/bin/…`), or `["/bin/zsh", "-lc", "…"]` to get the login-shell PATH.
   - **Dependencies.** If the project has an install step, add it as an action (for example `dev.install`) and run it only when the user agrees.
   - **Interactive setup scripts** (prompts that create `.env` or config): do not run them from a task. Tell the user, or write the files they would create when the docs make the values clear.
7. **Write, validate, and load.** Write `.mira/workspace.json` and the plugins in `.mira/plugins/<id>/plugin.json`, then:
   1. Run `mira validate .mira --json`. Fix each error and run it again until it passes.
   2. Run `mira reload --json` to load the new plugins.
   3. Run `mira doctor --json`: `validate` checks the manifests, but only `doctor` reports executables that are not installed.

   Ask the user once whether to share `.mira/` with the team or keep it personal:
   - **Share:** commit `.mira/workspace.json` and `.mira/plugins/`. Add ignore rules for personal and generated files to the project's `.gitignore` without rewriting it: `.mira/local.json`, `.mira/.drafts/`.
   - **Personal:** add `.mira/` to `.git/info/exclude` (the path is `$(git rev-parse --git-common-dir)/info/exclude`), so it applies to all worktrees of the repository and changes no tracked file.

   The skills are never part of the project: they live in the user's own skills folder (`mira skills export DIR`).
8. **Verify.** Run one quick task (`mira run dev.check`) and, if the user agrees, start the main service and read its logs. A dev server can move to another port when its default is taken: read the real URL from the log before you check it. Stop what you started unless the user wants it running.
9. **Report** in a few lines: the tools created (refs), what was verified to start, what was not verified (missing credentials, services not tried), and how to stop anything still running. Tell the user to open `mira` for the TUI.

Ask the user only for credentials, real ambiguity, or preferences you cannot infer. Do not ask which framework or package manager they use when the repository already shows it.
