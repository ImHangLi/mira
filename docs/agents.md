# Set up Mira (for agents)

You are setting up Mira for the user. Do the five steps in order. Mira installs nothing into a project on its own: you choose each location, and skills and notes never go into a repository.

## 1. Install Mira

Skip this step if `mira --version` works.

```sh
curl -fsSL https://raw.githubusercontent.com/ImHangLi/mira/main/scripts/install.sh | sh
```

If `mira` is still not found, use the `export PATH=...` line that the installer printed.

## 2. Export the skills

Export the skills to the user's own skills folder. Never export them into a project folder.

| The user's agent | Command |
|---|---|
| Claude Code | `mira skills export ~/.claude/skills` |
| Codex and other agents (Codex reads `~/.agents/skills`) | `mira skills export ~/.agents/skills` |
| Skills managed with Skillshare (`~/.config/skillshare/skills` exists) | `mira skills export ~/.config/skillshare/skills`, then `skillshare sync` |

If Skillshare is in use, use only the Skillshare row. Ask the user only if the correct folder is not clear. The command writes `mira/` and `mira-extend/` into that folder: a copy of the skills for this Mira version. If the folders already exist, for example after a Mira update, add `--force` to replace them.

## 3. Add a global note

Write a file `MIRA.md` next to the user's global instructions, and add one reference line to them. Do this for each agent in the table that is installed (its folder exists) and has the skills from step 2. With Skillshare, that is usually both. Do not add the line if it is already there.

| Agent | Note file | Reference line | Added to |
|---|---|---|---|
| Claude Code | `~/.claude/MIRA.md` | `@MIRA.md` | `~/.claude/CLAUDE.md` |
| Codex | `~/.codex/MIRA.md` | `Read ~/.codex/MIRA.md.` | `~/.codex/AGENTS.md` |

Write this exact text into `MIRA.md`:

```markdown
# Mira

Mira is installed. In a repository with a `.mira/` folder, run project commands through
Mira, so the user sees them in the Mira TUI: dev servers, tests, checks, builds, and
one-off scripts. Use a Mira tool when one fits (`mira catalog --search WORDS`), otherwise
`mira exec --label "what it does" -- COMMAND`. Read the mira skill before the first
command. To add or change a tool, use the mira-extend skill. If `mira` is not found,
use `~/.mira/bin/mira`.
```

For Claude Code, for example:

```sh
f=~/.claude/CLAUDE.md
grep -qxF '@MIRA.md' "$f" 2>/dev/null || printf '\n@MIRA.md\n' >> "$f"
```

## 4. Set up the repository

Follow the setup reference of the mira skill (`mira/references/setup.md` in the folder from step 2). In short:

1. Read the README, docs, scripts, Makefile, Compose files, and CI config. Never run project code to find out what it does.
2. Write `.mira/workspace.json` and the plugins in `.mira/plugins/<id>/plugin.json`. A plain command needs no JSON: `mira save "Unit tests" --json -- npm test` writes, checks, and loads it in one step.
3. Run `mira validate .mira --json`. Fix each error and run it again until it passes.
4. Run `mira reload --json` to load the new plugins.
5. Run `mira doctor --json`, then verify one tool, for example `mira run dev.check`.
6. Offer the default plugins. Run `mira plugin add --json` to list them. Ask the user once which ones to add, and give each one's name and its description in one short sentence. Add each chosen one with `mira plugin add NAME --json`. They become ordinary plugins in `.mira/plugins/`.

Plugins run the project's own commands. Do not add wrappers or prefixes from your own environment to them.

Ask the user one question: share `.mira/` with the team, or keep it personal? Recommend sharing, so the next session and the next engineer reuse the tools. If you cannot ask, keep it personal and tell the user how to share it later.

- **Share:** commit `.mira/workspace.json` and `.mira/plugins/`. Add `.mira/local.json` and `.mira/.drafts/` to `.gitignore`.
- **Personal:** add `.mira/` to `.git/info/exclude` (find it with `git rev-parse --git-common-dir`). This applies to all worktrees of the repository and changes no tracked file.

## 5. Report

Tell the user what you installed, where, and how to undo each item:

| Item | Where | Undo |
|---|---|---|
| Mira | the path the installer printed | the `Uninstall:` line the installer printed |
| Skills | the folder from step 2 (`mira/`, `mira-extend/`) | delete those two folders (run `skillshare sync` if you used Skillshare) |
| Global note | `MIRA.md` and the reference line from step 3 | delete the file and the line |
| Plugins | `.mira/` in the repository | delete `.mira/`, and the line in `.git/info/exclude` if you added it |

Also list the tools you created, what you verified, what you did not verify, and anything that is still running. Tell the user to run `mira` to open the TUI.
