# hive

Rust TUI + daemon for running git worktrees, coding agents, dev servers and
editors from one terminal. Inspired by [Nebula](https://github.com/AgentSystemLabs/nebula):
same daemon-owns-the-PTYs design, rearranged around worktrees and packages.

## Layout

- `crates/hive` — CLI entry point (`hive`, `hive daemon …`, `hive notify`, …)
- `crates/hive-core` — wire protocol, config, templating, agent launch flags
- `crates/hive-daemon` — PTYs, git/worktrees, setup, ports, status, SQLite
- `crates/hive-tui` — ratatui client (sidebar, tabs, panels, overlays)
- `vendor/vt100` — patched terminal emulator (don't edit casually)

## Before you finish

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

## Conventions

- Daemon does the work; the TUI only renders and sends requests.
- Changing `hive-core/src/protocol.rs`? Bump `PROTOCOL_VERSION`.
- Keep key bindings in sync with the glossary in `hive-tui/src/help.rs`.
- Check TUI changes headlessly:
  `printf 'wait 1500\ndump\n' | cargo run -q -p hive-tui --example drive -- 120 30 target/debug/hive`
