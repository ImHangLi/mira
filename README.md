<p align="center">
  <img src=".github/assets/mira-hero.png" alt="Mira" width="100%">
</p>

<h1 align="center">Mira</h1>

<p align="center">
  <em>Everything in one view, shared with (or without) your agent.</em>
</p>

<p align="center">
  <a href="https://github.com/ImHangLi/mira/releases/latest"><img alt="Release" src="https://img.shields.io/github/v/release/ImHangLi/mira?style=flat-square&color=f26b3a"></a>
  <img alt="Platform" src="https://img.shields.io/badge/macOS-arm64-f26b3a?style=flat-square">
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-f26b3a?style=flat-square"></a>
</p>

---

Mira is a local control center for everything you run, built entirely from plugins.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/mira-demo-dark.gif">
    <img src=".github/assets/mira-demo-light.gif" alt="Mira on a Next.js app: the dev server is running, the unit tests pass, a live view shows only the server errors, an agent run checks the env setup, and a system monitor plugin shows CPU, memory, and disk." width="100%">
  </picture>
</p>

## What if…

**…everything you run lived in one place?** Your dev server, your tests, `top`, a Docker log with only the errors. One screen, no tab switching, no commands to remember.

**…the tool you need took one file to make?** A log filter is six lines of JSON. A dev server is one command. A timer, a price table, a whole CLI inside Mira: each one is a plugin, and Mira is built entirely from them.

**…your agent saw exactly what you see, natively?** Your agent reads the same runs, logs, and views through the `mira` CLI. No extra protocol, no server to set up. When it runs a migration or a test, you watch it happen. When a command is worth keeping, it saves it as a plugin, and it never spends tokens finding it again.

**…you could share a tool as easily as a sentence?** Plugins are plain files in your repo. Commit them for your team, or ask your agent to write a prompt that rebuilds a plugin for someone else, fitted to their project.

**…it was not only for code?** A timer, a scheduled search, your morning update. Everything you run in the terminal, with you, or with you and your agent.

Mira itself runs no AI, sends nothing, and makes no network calls. A plugin does only what you write it to do.

## Get started

```sh
curl -fsSL https://raw.githubusercontent.com/ImHangLi/mira/main/scripts/install.sh | sh
```

Then ask your agent: *"Set up Mira for this repo."* Your agent follows the [setup steps](docs/agents.md), writes the plugins, and checks them. Then run `mira`.

**Setting it up yourself?** [Read the guide for people](docs/guide.md). It covers your first plugin, every kind of plugin, and how to remove and share them. See the [example plugins](examples/plugins/) and the [architecture](docs/architecture.md).

## Share a plugin

- **With your team:** commit `.mira/workspace.json` and `.mira/plugins/`.
- **With anyone:** ask your agent to *"write a prompt that rebuilds this Mira plugin"*, and send the prompt. Their agent makes the same tool for their project.
