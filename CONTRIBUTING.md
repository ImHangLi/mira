# Contributing to Mira

Thanks for being here. Mira is a small project, and every bug report, idea, and pull request helps.

## Say hello

- **A question, or a plugin to show:** start a [discussion](https://github.com/ImHangLi/mira/discussions).
- **A bug:** open a [bug report](https://github.com/ImHangLi/mira/issues/new?template=bug.yml).
- **An idea:** open an [idea](https://github.com/ImHangLi/mira/issues/new?template=idea.yml).
- **A security problem:** report it privately. See [SECURITY.md](SECURITY.md).

## Send code

Fixes and features are both welcome, and you do not need to ask first. For a large change, a short idea issue can save you time.

Mira runs on macOS with Apple silicon. Install [rustup](https://rustup.rs), then:

```sh
cargo build --workspace --locked
```

Try your build in a scratch folder:

```sh
mkdir /tmp/mira-try && cd /tmp/mira-try
/path/to/mira/target/debug/mira
```

Before you open a pull request, run the checks that CI runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
scripts/check-contract.sh
```

A good pull request:

- Does one thing.
- Has a [Conventional Commits](https://www.conventionalcommits.org) title, such as `fix(tui): keep the selection after a reload`.
- Says what changed, why, and how you tried it, in a few lines of English.

You do not need to add tests. [AGENTS.md](AGENTS.md) has the code rules, and [docs/architecture.md](docs/architecture.md) shows how the parts fit.

## Use of AI

Mira is built for coding agents, so use one if you like. Read and try what it wrote before you send it.

## Be kind

Everyone here follows the [Code of Conduct](CODE_OF_CONDUCT.md).

## License

Mira uses the [MIT license](LICENSE). Your contribution is under the same license.
