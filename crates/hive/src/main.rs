use std::io::{IsTerminal, Read};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use hive_core::protocol::{ClientRequest, Notify, ServerEvent};

mod import;

#[derive(Parser)]
#[command(
    name = "hive",
    version,
    about = "Worktrees, agents, dev servers and editors in one terminal"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Manage the background daemon that owns every session.
    Daemon {
        #[command(subcommand)]
        action: DaemonCmd,
    },
    /// Register a git repo as a project (defaults to the current directory).
    Add { path: Option<PathBuf> },
    /// List projects, worktrees and sessions.
    #[command(alias = "ls")]
    List,
    /// Write a starter project config to ~/.config/hive/projects/<name>.toml.
    Init {
        path: Option<PathBuf>,
        /// Write `<repo>/.hive.toml` instead (commit it to share with the team).
        #[arg(long)]
        in_repo: bool,
    },
    /// Convert ~/.config/portal-wt/config.json into a hive project config.
    ImportPortalWt {
        /// Repo root (defaults to the config's repoRoot).
        #[arg(long)]
        repo: Option<PathBuf>,
        /// App package path relative to the repo (defaults to the config's appSubdir).
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Check the environment (git, code, agents, nvm, daemon, configs).
    Doctor,
    /// Report an agent hook event to the daemon (run by agent hooks).
    #[command(hide = true)]
    Notify {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        event: String,
        /// Codex passes its JSON payload as the final argument.
        payload: Vec<String>,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Run in the foreground (the TUI starts this automatically).
    Run,
    /// Stop the daemon, killing every session.
    Stop,
    /// Stop and start again (e.g. after upgrading hive).
    Restart,
    Status,
}

fn hive_bin() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .unwrap_or_else(|| PathBuf::from("hive"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // `notify` must be fast and must never break the agent's hook.
    if let Some(Cmd::Notify {
        agent,
        event,
        payload,
    }) = &cli.cmd
    {
        let _ = notify(agent, event, payload);
        return Ok(());
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async_main(cli))
}

fn notify(agent: &str, event: &str, payload: &[String]) -> Result<()> {
    let Ok(session) = std::env::var("HIVE_SESSION_ID") else {
        return Ok(());
    };
    let payload = match payload.last() {
        Some(p) => p.clone(),
        None if !std::io::stdin().is_terminal() => {
            let mut s = String::new();
            std::io::stdin()
                .take(4 * 1024 * 1024)
                .read_to_string(&mut s)?;
            s
        }
        None => String::new(),
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let req = ClientRequest::Notify(Notify {
            session,
            agent: agent.into(),
            event: event.into(),
            payload,
        });
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            hive_tui::client::send_one(req),
        )
        .await;
    });
    Ok(())
}

async fn async_main(cli: Cli) -> Result<()> {
    match cli.cmd {
        None => hive_tui::run(&hive_bin()).await,
        Some(Cmd::Daemon { action }) => daemon_cmd(action).await,
        Some(Cmd::Add { path }) => {
            let path = absolute(path.unwrap_or_else(|| PathBuf::from(".")))?;
            hive_tui::client::connect_or_spawn(&hive_bin()).await?;
            hive_tui::client::send_one(ClientRequest::AddProject { path: path.clone() }).await?;
            println!("added {}", path.display());
            Ok(())
        }
        Some(Cmd::List) => list().await,
        Some(Cmd::Init { path, in_repo }) => import::init(
            &absolute(path.unwrap_or_else(|| PathBuf::from(".")))?,
            in_repo,
        ),
        Some(Cmd::ImportPortalWt { repo, app, config }) => {
            import::import_portal_wt(repo, app, config)
        }
        Some(Cmd::Doctor) => doctor().await,
        Some(Cmd::Notify { .. }) => unreachable!(),
    }
}

fn absolute(p: PathBuf) -> Result<PathBuf> {
    let p = hive_core::paths::expand_tilde(&p.to_string_lossy());
    std::fs::canonicalize(&p).with_context(|| format!("{} does not exist", p.display()))
}

async fn daemon_running() -> bool {
    matches!(
        hive_tui::client::request(ClientRequest::Hello {
            version: hive_core::PROTOCOL_VERSION
        })
        .await,
        Ok(Some(_))
    )
}

async fn daemon_cmd(action: DaemonCmd) -> Result<()> {
    match action {
        DaemonCmd::Run => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_env("HIVE_LOG")
                        .unwrap_or_else(|_| "info".into()),
                )
                .init();
            hive_daemon::run(hive_bin()).await
        }
        DaemonCmd::Stop => stop().await,
        DaemonCmd::Restart => {
            stop().await?;
            hive_tui::client::connect_or_spawn(&hive_bin()).await?;
            println!("daemon started");
            Ok(())
        }
        DaemonCmd::Status => {
            match hive_tui::client::request(ClientRequest::Hello {
                version: hive_core::PROTOCOL_VERSION,
            })
            .await
            {
                Ok(Some(ServerEvent::Hello { version, pid })) => {
                    println!(
                        "running: pid {pid}, protocol v{version}, socket {}",
                        hive_core::paths::socket_path().display()
                    )
                }
                _ => println!("not running"),
            }
            Ok(())
        }
    }
}

async fn stop() -> Result<()> {
    let pid = match hive_tui::client::request(ClientRequest::Hello {
        version: hive_core::PROTOCOL_VERSION,
    })
    .await
    {
        Ok(Some(ServerEvent::Hello { pid, .. })) => pid as i32,
        _ => {
            println!("daemon not running");
            return Ok(());
        }
    };
    hive_tui::client::send_one(ClientRequest::Shutdown).await?;
    // Wait for the process itself to exit, not just stop accepting.
    for _ in 0..100 {
        if unsafe { libc::kill(pid, 0) } != 0 {
            println!("daemon stopped");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    bail!("daemon (pid {pid}) did not stop")
}

async fn snapshot() -> Result<(
    Vec<hive_core::protocol::ProjectInfo>,
    Vec<hive_core::protocol::SessionInfo>,
)> {
    let mut s = tokio::net::UnixStream::connect(hive_core::paths::socket_path())
        .await
        .context("hive daemon is not running")?;
    hive_core::codec::write_frame(&mut s, &ClientRequest::Subscribe).await?;
    match hive_core::codec::read_frame(&mut s).await? {
        Some(ServerEvent::Snapshot {
            projects, sessions, ..
        }) => Ok((projects, sessions)),
        _ => bail!("unexpected reply from daemon"),
    }
}

async fn list() -> Result<()> {
    let (projects, sessions) = snapshot().await?;
    for p in projects {
        println!("{}  {}", p.name, hive_core::paths::tildify(&p.root));
        if let Some(e) = &p.config_error {
            println!("  ⚠ {e}");
        }
        for w in &p.worktrees {
            let dirty = if w.dirty { "*" } else { "" };
            println!(
                "  {}{dirty}  slot {}  {}",
                w.label(),
                w.slot,
                hive_core::paths::tildify(&w.path)
            );
            for se in sessions.iter().filter(|s| s.worktree == w.path) {
                let state = if se.alive {
                    format!("{:?}", se.status)
                } else {
                    "ended".into()
                };
                let ports = se
                    .ports
                    .iter()
                    .map(|p| format!(":{p}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                println!(
                    "      {:<16} {:<8} {} {ports}",
                    se.kind.label(),
                    state,
                    se.title
                );
            }
        }
    }
    Ok(())
}

fn which(cmd: &str) -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let out = std::process::Command::new(shell)
        .args(["-l", "-i", "-c", &format!("command -v {cmd}")])
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()
        .unwrap_or("")
        .trim()
        .to_string();
    (out.status.success() && !path.is_empty()).then_some(path)
}

async fn doctor() -> Result<()> {
    let ok = |b: bool| if b { "✓" } else { "✗" };
    let global = hive_core::config::GlobalConfig::load();
    println!(
        "{} config {}",
        ok(global.is_ok()),
        hive_core::paths::config_file().display()
    );
    if let Err(e) = &global {
        println!("    {e:#}");
    }
    let global = global.unwrap_or_default();
    let checks = [
        ("git", "git".to_string()),
        ("VS Code", "code".into()),
        ("claude", global.agents.claude.cmd.clone()),
        ("codex", global.agents.codex.cmd.clone()),
        ("nc (used by wait_for_port)", "nc".into()),
    ];
    for (name, cmd) in checks {
        match which(&cmd) {
            Some(p) => println!("✓ {name}: {p}"),
            None => println!("✗ {name}: `{cmd}` not found on PATH (interactive login shell)"),
        }
    }
    let nvm = hive_core::paths::expand_tilde("~/.nvm/nvm.sh");
    println!("{} nvm: {}", ok(nvm.exists()), nvm.display());
    println!("  shell: {}", global.shell());
    let running = daemon_running().await;
    println!(
        "{} daemon {} ({})",
        ok(running),
        if running { "running" } else { "not running" },
        hive_core::paths::socket_path().display()
    );
    if running {
        for p in snapshot().await?.0 {
            let src = p
                .config_source
                .as_deref()
                .map(hive_core::paths::tildify)
                .unwrap_or_else(|| "defaults".into());
            println!(
                "{} project {} — config: {src}",
                ok(p.config_error.is_none()),
                p.name
            );
            if let Some(e) = p.config_error {
                println!("    {e}");
            }
            let runs: Vec<&str> = p.runs.iter().map(|r| r.name.as_str()).collect();
            println!(
                "    {} worktrees, {} packages, runs: {}",
                p.worktrees.len(),
                p.packages.len(),
                runs.join(", ")
            );
        }
    }
    println!(
        "  claude hooks: {}",
        hive_core::paths::claude_settings_file().display()
    );
    println!("  logs: {}", hive_core::paths::log_dir().display());
    Ok(())
}
