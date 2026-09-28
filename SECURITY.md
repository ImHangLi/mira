# Security

## Report a vulnerability

Report it privately through [GitHub security advisories](https://github.com/ImHangLi/mira/security/advisories/new). Do not open a public issue. You get a reply within a few days.

## What Mira does

- **No AI, no account, no telemetry.** Mira runs no models and sends nothing about your projects.
- **One network call.** The TUI checks the latest release on GitHub at most once a day. `MIRA_NO_UPDATE_CHECK=1` turns it off. `mira update` downloads a release only when you run it.
- **Local only.** The CLI and the TUI talk to a host process over a Unix socket that only your user can open. Run records and logs stay on your Mac.
- **Nothing runs by itself.** A plugin runs only when you or your agent start it. A table row action asks before it runs.

## Plugins are code

A plugin runs its commands with your permissions, like a script in `package.json`. Read a plugin before you add it, the same as any script from someone else. The default plugins are in [`plugins/`](plugins/).

## The installer

`install.sh` downloads a release archive from GitHub, checks its SHA-256 checksum, and puts `mira` in `~/.mira/bin`. It needs no sudo, never replaces a `mira` it did not install, and never removes macOS quarantine attributes or bypasses Gatekeeper. Read it before you run it: [`scripts/install.sh`](scripts/install.sh). `mira update` checks the checksum the same way and keeps the previous binary for `mira update --rollback`.

The binaries are not signed or notarized with an Apple Developer ID.
