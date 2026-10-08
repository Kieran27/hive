# hive

Run every git worktree from one terminal window. Each worktree gets its own Claude Code and Codex agents, shells, dev servers and VS Code window. hive replaces the setup of a Ghostty tab per worktree plus a VS Code window per worktree.

```
┌ hive ─────────────────────────┬ acme-app  feat/login  ~/code/acme-app-worktrees/feat-login ─────────────┐
│ ▾ acme-app                    │ ✓ 1 claude:fix the login redirect  ⠹ 2 codex  ○ 3 shell  ▶ 4 run:web 4010/3010│
│   ▸ develop                   │╭ claude · ~/…/feat-login ──────────────────────────────────────────────╮│
│   ▾ feat/login ✓ ▶            ││                                                                       ││
│       · shell                 ││   (the real Claude Code / Codex / shell / dev-server terminal)        ││
│       · store                 ││                                                                       ││
│       · mobile                ││                                                                       ││
│   ▸ native/camera ◐           ││                                                                       ││
│ ▸ my-side-project             ││                                                                       ││
│───────────────────────────────││                                                                       ││
│ slot 1 web:4010/3010 metro:8091││                                                                      ││
└───────────────────────────────┴───────────────────────────────────────────────────────────────────────┘
 NAV  c claude  x codex  t shell  r run  n new wt  e vscode  . next  / jump  ? help
```

- **Left:** projects, with their worktrees and the packages inside each worktree. Status dots roll up from the agents: `⠹` working, `◐` waiting for you, `✓` done. `▶` means a dev server is running and `✗` means a process failed.
- **Right:** one tab per session in the selected worktree. Run targets with several processes, such as a web app plus the API or micro-frontend it loads, share one tab split into panes.
- **Source control (`g`):** a panel on the right lists the worktree's changes as **Staged** and **Changes**. You can stage or unstage single files, view diffs, discard changes and commit only what's staged, so local tweaks stay out of the commit.
- **Sessions survive quitting.** A background daemon owns every process. Quitting the TUI (`q`) leaves them running, and the next `hive` re-attaches with scrollback.

Inspired by [Nebula](https://github.com/AgentSystemLabs/nebula). hive uses the same daemon + thin ratatui client design and the same PTY handling (vt100 replay ring, kitty keyboard, frame pacing). It is arranged around worktree → package → session, and adds run targets with ports per worktree, setup pipelines and VS Code windows.

## Install

```sh
cargo install --path crates/hive      # puts `hive` in ~/.cargo/bin
hive doctor                           # checks git, code, claude, codex, nvm, nc
```

## Quick start

```sh
hive add ~/code/my-repo     # or press `a` inside hive
hive                        # starts the daemon if needed and opens the TUI
```

A repo with no config still works: you get worktrees, agents and shells. Packages are detected from `package.json` workspaces. A `dev`/`start` script becomes a run target.

`hive init [path]` writes a starter config to `~/.config/hive/projects/<repo>.toml`. Pass `--in-repo` to write `.hive.toml` into the repo instead.

## Keys

The bottom bar always shows the keys that make sense right now, for example `u resume` on an ended agent or `s stop` on a dev server. Press **`?`** for the full list of keys and status symbols, and type to search it (e.g. `close`, `stage`, `scroll`).

hive has two modes. **NAV** is for moving around hive. **TERM** sends every key to the focused terminal, and `ctrl+q` (configurable) returns to NAV.

| key | NAV action |
|---|---|
| `j/k` `↑↓`, `h/l` `←→`, `space` | move, collapse/expand |
| `enter` | focus the terminal (opens a shell if the worktree has none) |
| `c` / `x` / `t` | new Claude / Codex / shell, started in the selected package |
| `r` · `R` · `s` | run a target · restart the active run · stop it |
| `1-9`, `tab`/`⇧tab`, `[ ]` | switch tabs, or the process inside a run tab |
| `.` | jump to the next session that is waiting or done |
| `/` | fuzzy-jump to any worktree or session |
| `e` | open (or focus) the worktree's VS Code window; marks it `[vscode]` |
| `g` | source-control panel (press again to hide) |
| `n` · `S` · `D` | new worktree (with setup) · re-run setup · remove/prune worktree |
| `w` · `u` | close tab (kills it) · resume an ended agent / restart a shell |
| `a` · `X` | add / remove a project |
| `pgup/pgdn` (`⇧` in TERM) | scroll back |
| `<` `>` | sidebar width |
| `ctrl+r` | reload config |
| `q` · `Q` | quit (everything keeps running) · quit and stop the daemon |

**In the source-control panel:**

| key | action |
|---|---|
| `j/k` | move |
| `space` | stage or unstage the file; on a section header, all of that section |
| `a` · `u` | stage all changes · unstage everything |
| `enter` (or click a selected file) | diff, in a scrollable view |
| `c` | commit the staged files (asks for a message) |
| `x` | discard unstaged changes to the file, after a confirm; untracked files are deleted |
| `r` · `esc` · `g` | refresh · back to the tree · hide |

Commits run `git commit` through your login shell with the project's node version, so pre-commit hooks (husky, lint-staged…) behave as they do in a terminal. If a hook fails, its output opens in the diff view. The panel refreshes every 2 seconds while it's open.

The mouse works too: click rows and tabs, click a pane to focus it, use the wheel to scroll, and drag the sidebar's right border to resize it. When an app in a pane copies with OSC 52 (for example over SSH), the text goes to the macOS clipboard. Mouse events are forwarded to apps that ask for them. To select text natively, hold Shift (Ghostty, iTerm2).

## Configuration

### Global: `~/.config/hive/config.toml` (optional)

```toml
shell = "/bin/zsh"                 # default: $SHELL
projects = ["~/code/other-repo"]   # extra projects (ones added with `a` live in the db)

[agents.claude]
cmd = "claude"
args = []                          # e.g. ["--model", "opus"]

[agents.codex]
cmd = "codex"
args = []

[keys]
unlock = "ctrl+q"                  # leave TERM mode

[keys.nav]                         # extra NAV bindings: action = key
palette = "ctrl+p"                 # actions: claude codex shell run restart_run stop_run
claude = "C"                       #   new_worktree setup remove_worktree add_project remove_project
                                   #   vscode close_tab resume next_attention palette help
                                   #   quit quit_stop_daemon reload

[ui]
sidebar_width = 34
desktop_notifications = false      # macOS notification when an agent finishes / needs you
mouse = true
```

### Per project: `<repo>/.hive.toml` or `~/.config/hive/projects/<repo-dir-name>.toml`

```toml
[project]
name = "my-app"
worktree_root = "~/code/my-app-worktrees"   # default: <repo>/../<repo>-worktrees
base_branch = "main"                        # default: origin/HEAD
port_stride = 10                            # slot N → base port + N*stride
copy_from_main = [".env"]                   # gitignored files copied into new worktrees

[node]
nvm = true                  # wrap commands: source nvm.sh; nvm use (.nvmrc | default_version)
default_version = "20"      # a [[setup_step]] / [[run]] / [[run.proc]] can pin `node = "22"`

[[package]]
name = "web"
path = "apps/web"
copy_from_main = [".env.local"]             # relative to the package

[vscode]
folders = ["."]                             # "." | relative path | "@package"

[[setup_step]]                              # checklist after `n`, pre-checked by profile
id = "install"
cmd = "pnpm install"
[[setup_step]]
id = "env"
cwd = "@web"
cmd = "pnpm env:{env}"                      # {env} choices come from env:* scripts

[[profile]]                                 # first match wins; the last is the fallback
name = "default"
match = ["*"]                               # `*` matches anything, including `/`
env = "staging"
steps = ["install", "env"]

[[run]]
name = "dev"
cwd = "@web"
port = 3000
cmd = "PORT={port} pnpm dev"

[[run]]                                     # several processes in one tab
name = "stack"
env = { BROWSER = "none" }
[[run.proc]]
name = "api"
port = 4000
cmd = "PORT={port} pnpm api"
[[run.proc]]
name = "web"
port = 3000
wait_for_port = "api"                       # wait until api accepts connections
cmd = "PORT={port} API_URL=http://localhost:{port.api} pnpm web"

[[run]]
name = "ios"
port = 8081
cmd = "npx expo run:ios --device \"{device}\" --port {port}"
[[run.ask]]                                 # asked before starting
name = "device"
prompt = "iOS simulator"
default = "iPhone 15"
choices_cmd = "xcrun simctl list devices available | grep -E '^ +iPhone' | sed -E 's/^ +//; s/ \\(.*//'"
```

**Placeholders** in `cmd` and `env`:

- `{port}`, `{port.<proc>}`
- `{slot}`, `{branch}`, `{worktree}`, `{project}`, `{env}`
- any `ask` name

`${VAR}` and `{a,b}` pass through to the shell unchanged.

**Ports:** the main checkout is slot 0 and uses the configured ports. Every other worktree gets a stable slot (1, 2, …), so its ports are `base + slot × stride`. Before starting a target, hive checks that every port it needs is free. If one is busy, it refuses with a clear error rather than letting a dev server drift to some other port.

A fuller example is in [`examples/web-and-mobile-monorepo.toml`](examples/web-and-mobile-monorepo.toml). It covers two web apps (one loading the other at runtime) plus an Expo app in one monorepo, with node versions pinned per step.

## Agents and status

- **Claude Code** gets `--settings ~/.local/state/hive/claude-hooks.json`. That file adds hooks (`UserPromptSubmit`, `Stop`, `Notification`, `PermissionRequest`, …), and each hook calls `hive notify`. Nothing in your repo or `~/.claude` is modified.
- **Codex** gets `-c notify=["…/hive","notify",…]` for that launch only. `~/.codex/config.toml` is untouched. Codex reports only "turn complete", so pressing Enter in a Codex pane marks it as working.
- **Tab titles:** the first prompt becomes the tab title.
- **Resume:** hive stores each agent's session id. If the daemon stops, those tabs come back as ended, and `u` resumes the conversation (`claude --resume <id>` / `codex resume <id>`).

## Migrating from portal-worktree-tui

```sh
hive import-portal-wt --repo ~/code/my-repo --app apps/mobile
```

This converts `~/.config/portal-wt/config.json` into a hive project config. `--repo` and `--app` override the file's `repoRoot` and `appSubdir`, and are required if there is no config file (only the default branch profiles are built in). The result includes:

- branch profiles
- nvm-wrapped yarn steps
- `.env.local` copying
- metro, ios and android run targets

Add web run targets afterwards; [`examples/web-and-mobile-monorepo.toml`](examples/web-and-mobile-monorepo.toml) shows how.

## CLI

```
hive                      open the TUI
hive add [path]           register a repo
hive ls                   projects, worktrees, sessions
hive init [path]          starter project config
hive import-portal-wt     convert portal-wt config
hive doctor               environment check
hive daemon status|stop|restart|run
```

## Files

| what | where |
|---|---|
| config | `~/.config/hive/config.toml`, `~/.config/hive/projects/*.toml` |
| state | `~/.local/state/hive/` (`hive.db`, `logs/daemon.log`, `workspaces/*.code-workspace`, `claude-hooks.json`) |
| socket | `$XDG_RUNTIME_DIR/hive/daemon.sock` or `/tmp/hive-$UID/daemon.sock` |

`HIVE_HOME=<dir>` moves all of these, which is handy for a test instance. `HIVE_SOCKET` moves just the socket.

## Architecture

```
crates/hive         CLI (clap): tui | daemon | notify | add | ls | init | import-portal-wt | doctor
crates/hive-core    protocol (rmp frames over a unix socket), config, templating, agent launch/hooks
crates/hive-daemon  PTYs (portable-pty, 1 MB replay ring, kitty/DA1/CPR answering, coalescing),
                    git worktrees, setup scripts, port slots, status machine, SQLite store, VS Code
crates/hive-tui     ratatui client: vt100 + tui-term panes, key/mouse encoding, overlays, paced redraws
vendor/vt100        vt100 with Nebula's scrollback patch (codex inline viewport)
```

**How the daemon and TUI work:**

- **The daemon owns everything.** Each PTY has its own reader thread, which feeds a pump task. Under one lock the pump:
  - answers terminal queries;
  - updates a headless screen;
  - appends to the ring;
  - broadcasts the output.

  Because those happen together, a client that attaches mid-stream gets a gap-free replay.
- **The TUI is disposable.** It attaches only to the visible panes, with `from_seq` deltas, and keeps a few recent terminals parsed. It redraws only when something changed, through a token bucket (a burst of 3, then at most ~60 fps).

## Development

```sh
cargo test                       # unit + e2e (real daemon, git repo, PTYs, ports)
cargo clippy --all-targets -- -D warnings
cargo test -p hive-daemon --release -- --ignored --nocapture   # throughput benches
# drive the real TUI headlessly and print the screen:
printf 'wait 1500\nkeys j\ndump\n' | cargo run -q -p hive-tui --example drive -- 120 30 target/debug/hive
```
