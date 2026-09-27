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

It is for everyone who runs things: frontend and backend, TypeScript, Python, and Rust, product, infra, research, growth, and operations. Add any plugin you need. It lives right in the view, and you and your agent read it the same way.

- **One screen** for your servers, tests, and scripts.
- **Customize everything.** Every tool is a plugin. Build your own, or let your agent do it.
- **Your agent sees what you see.** Same runs, same logs.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/mira-demo-dark.gif">
    <img src=".github/assets/mira-demo-light.gif" alt="Mira on a Next.js app: the dev server is running, the unit tests pass, a live view shows only the server errors, an agent run checks the env setup, and a system monitor plugin shows CPU, memory, and disk." width="100%">
  </picture>
</p>

## Getting started

```sh
curl -fsSL https://raw.githubusercontent.com/ImHangLi/mira/main/scripts/install.sh | sh
```

Then ask your agent to set up Mira for your repo.

*Are you an agent? [Start here.](docs/agents.md)*

To write a plugin yourself, add a small `plugin.json` under `.mira/plugins/`, list it in `.mira/workspace.json` ([format](skills/mira-extend/references/manifest.md)), and run `mira reload`. See the [example plugins](examples/plugins/). For how Mira is built, see the [architecture](docs/architecture.md).

## Use cases

| The pain | With Mira |
|---|---|
| Six terminal tabs, and you can't find the dev server. | One screen, every status. |
| Your agent says the tests pass. You can't check. | You open the same run. |
| Every new session relearns how to run the repo. | The commands are saved as plugins in the repo. |
| The view you need doesn't exist. | Build it as a plugin, or ask your agent to. |
| Every morning, the same Slack updates, one prompt at a time. | Your agent saves the routine. Tomorrow, press `Enter`. |
| You open Activity Monitor, or install another app, to watch CPU and memory. | Ask your agent for a monitor plugin, with the bars and settings you want. |
