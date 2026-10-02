# Contributing to Mira

Thank you for your interest in Mira. This page tells you how to report a problem, propose a change, and send code.

Mira is a small project with one maintainer. Short, focused contributions get an answer fastest.

## Ways to help

| You have | Do this |
|---|---|
| A question, or a plugin to show | Start a [discussion](https://github.com/ImHangLi/mira/discussions). |
| A bug | Open a [bug report](https://github.com/ImHangLi/mira/issues/new?template=bug.yml). Include your Mira version, your terminal, and the steps. |
| An idea or a plugin request | Open an [idea](https://github.com/ImHangLi/mira/issues/new?template=idea.yml). Describe the problem first, then your idea. |
| A security problem | Report it privately. See [SECURITY.md](SECURITY.md). |

## Before you write code

- **Small fixes:** a bug fix, a typo, or a documentation fix needs no permission. Open a pull request.
- **Features and behavior changes:** open an idea first and wait for agreement. A feature is a long-term cost, so a pull request without an agreed issue is usually closed.
- **Plugins:** most new tools belong in your own project as a plugin, not in Mira. Mira's code has no knowledge of specific plugins, frameworks, or agents. To propose a new default plugin, open an idea.

## Set up

Mira supports macOS on Apple silicon only.

1. Install [rustup](https://rustup.rs). The repository pins the Rust version in `rust-toolchain.toml`, and rustup installs it for you.
2. Build:

   ```sh
   cargo build --workspace --locked
   ```

3. Run your build in a scratch project, not in a project you depend on:

   ```sh
   mkdir /tmp/mira-try && cd /tmp/mira-try
   /path/to/mira/target/debug/mira
   ```

Read [docs/architecture.md](docs/architecture.md) for the processes and crates, and [AGENTS.md](AGENTS.md) for the code rules. The rules in `AGENTS.md` apply to people and to coding agents. The check lock that it names is for the maintainer's machine; you can run `cargo` directly.

## Checks

Run the checks that CI runs before you open a pull request:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
scripts/check-contract.sh
```

If you change a wire type, regenerate the schema that `check-contract.sh` names. Also try your change in the real TUI or CLI, and say what you saw.

Do not add new permanent tests unless the maintainer asks for them. Fixture data for `mira validate` and the examples is welcome.

## Pull requests

- Make one change in each pull request, and keep it small.
- Use a [Conventional Commits](https://www.conventionalcommits.org) title, for example `fix(tui): keep the selection after a reload`.
- Fill in the template: what changed, why, and how you checked it. Write it yourself and keep it short.
- Write in English: code, comments, commits, issues, and pull requests.
- Update `docs/` and `skills/` when behavior that they describe changes.
- Mira has no backwards compatibility yet. Do not add migrations, aliases, or shims for older versions.
- Pull requests are squash merged.

## Use of AI

Mira is built for coding agents, and you are welcome to use one. You are responsible for the result:

- Understand every line that you send, and be able to explain it without the agent.
- Run the checks and try the change yourself.
- Do not send long generated descriptions. A pull request that its author did not read or test will be closed.

## License

Mira uses the [MIT license](LICENSE). Your contribution is under the same license.
