//! Scriptable commands for agents: create/remove worktrees, run setup,
//! start/stop run targets and list state. Each one sends a single request
//! and waits for the daemon's reply; `--json` prints one object on stdout.

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use clap::{Args, Subcommand, ValueEnum};
use hive_core::protocol::{
    ClientRequest, CreateWorktree, ProjectInfo, ServerEvent, SessionInfo, StartRun, ToastLevel,
    WorktreeInfo, WorktreeMode,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// PTY size for sessions started from the CLI (the TUI resizes on attach).
const COLS: u16 = 120;
const ROWS: u16 = 40;
/// Lines of output shown when setup or a run proc fails.
const TAIL_LINES: u16 = 50;
const PORT_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Subcommand)]
pub enum WtCmd {
    /// Create a worktree (and optionally run setup steps in it).
    New {
        project: String,
        branch: String,
        /// Base ref for a new branch (defaults to the project's base branch).
        #[arg(long)]
        base: Option<String>,
        #[arg(long, value_enum, default_value_t = Mode::New)]
        mode: Mode,
        /// Setup step ids to run (none by default).
        #[arg(long, value_delimiter = ',')]
        setup: Vec<String>,
        /// Value for `{env}` in setup steps.
        #[arg(long)]
        env: Option<String>,
        /// Wait for setup to finish; exit 1 if it fails.
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        json: bool,
    },
    /// Remove a worktree, stopping its sessions and freeing its slot.
    Rm {
        project: String,
        /// Worktree path (or branch name).
        path: String,
        /// Remove even with uncommitted changes.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Mode {
    /// Create a new branch from --base.
    New,
    /// Check out an existing local branch.
    Local,
    /// Track a remote branch.
    Remote,
    /// Detached HEAD at the given ref.
    Detached,
}

#[derive(Args)]
pub struct SetupArgs {
    project: String,
    /// Worktree path (or branch name).
    worktree: String,
    #[arg(long, value_delimiter = ',', required = true)]
    steps: Vec<String>,
    #[arg(long)]
    env: Option<String>,
    /// Wait for setup to finish and exit with its status.
    #[arg(long)]
    wait: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct RunArgs {
    project: String,
    /// Worktree path (or branch name).
    worktree: String,
    target: String,
    /// Answer a `[[run.ask]]` prompt (repeatable).
    #[arg(long = "ask", value_name = "NAME=VALUE", value_parser = parse_ask)]
    asks: Vec<(String, String)>,
    /// Wait until every proc's port accepts connections (180s timeout).
    #[arg(long)]
    wait_ports: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct StopArgs {
    project: String,
    /// Worktree path (or branch name).
    worktree: String,
    target: String,
    #[arg(long)]
    pub json: bool,
}

fn parse_ask(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.into(), v.into())),
        _ => Err(format!("expected NAME=VALUE, got {s:?}")),
    }
}

/// A usage mistake (unknown step, missing `--ask`): exit code 2.
#[derive(Debug)]
pub struct Usage(pub String);

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Usage {}

fn usage(msg: impl Into<String>) -> anyhow::Error {
    Usage(msg.into()).into()
}

/// Run `f`, print its result (or error) and exit with 0, 1 or 2.
pub async fn finish(json: bool, f: impl std::future::Future<Output = Result<Outcome>>) -> ! {
    let (code, out) = match f.await {
        Ok(o) => (if o.ok { 0 } else { 1 }, Ok(o)),
        Err(e) => (if e.is::<Usage>() { 2 } else { 1 }, Err(e)),
    };
    match out {
        Ok(o) if json => println!("{}", o.json),
        Ok(o) => {
            if !o.text.is_empty() {
                println!("{}", o.text);
            }
            if !o.output.is_empty() {
                eprintln!("{}", o.output);
            }
        }
        Err(e) if json => println!("{}", json!({ "error": format!("{e:#}") })),
        Err(e) => eprintln!("error: {e:#}"),
    }
    std::process::exit(code)
}

pub struct Outcome {
    ok: bool,
    json: Value,
    text: String,
    /// Failure output, printed to stderr in text mode.
    output: String,
}

impl Outcome {
    fn ok(json: Value, text: impl Into<String>) -> Self {
        Self {
            ok: true,
            json,
            text: text.into(),
            output: String::new(),
        }
    }
}

// ------------------------------------------------------------ connections

struct Conn {
    tx: mpsc::UnboundedSender<ClientRequest>,
    rx: mpsc::UnboundedReceiver<ServerEvent>,
}

impl Conn {
    async fn open(hive_bin: &Path) -> Result<Self> {
        let stream = hive_tui::client::connect_or_spawn(hive_bin).await?;
        let (tx, rx) = hive_tui::client::split(stream);
        let mut c = Self { tx, rx };
        let version = c
            .call(
                ClientRequest::Hello {
                    version: hive_core::PROTOCOL_VERSION,
                },
                |e| match e {
                    ServerEvent::Hello { version, .. } => Some(*version),
                    _ => None,
                },
            )
            .await?;
        if version != hive_core::PROTOCOL_VERSION {
            bail!(
                "daemon speaks protocol v{version}, this hive v{}; run `hive daemon restart`",
                hive_core::PROTOCOL_VERSION
            );
        }
        Ok(c)
    }

    /// Send `req` and wait for the reply `f` picks out. An error toast on
    /// this (unsubscribed) connection is the daemon refusing the request.
    async fn call<T>(
        &mut self,
        req: ClientRequest,
        mut f: impl FnMut(&ServerEvent) -> Option<T>,
    ) -> Result<T> {
        self.tx
            .send(req)
            .map_err(|_| anyhow!("daemon connection closed"))?;
        loop {
            let ev = self
                .rx
                .recv()
                .await
                .ok_or_else(|| anyhow!("daemon closed the connection"))?;
            if let ServerEvent::Toast {
                level: ToastLevel::Error,
                message,
            } = &ev
            {
                bail!("{message}");
            }
            if let Some(t) = f(&ev) {
                return Ok(t);
            }
        }
    }
}

/// A subscribed connection: the snapshot plus every later session change.
struct Watch {
    conn: Conn,
    projects: Vec<ProjectInfo>,
}

impl Watch {
    async fn open(hive_bin: &Path) -> Result<Self> {
        let mut conn = Conn::open(hive_bin).await?;
        let projects = conn
            .call(ClientRequest::Subscribe, |e| match e {
                ServerEvent::Snapshot { projects, .. } => Some(projects.clone()),
                _ => None,
            })
            .await?;
        Ok(Self { conn, projects })
    }

    fn project(&self, name: &str) -> Result<&ProjectInfo> {
        self.projects
            .iter()
            .find(|p| p.id == name || p.name == name)
            .ok_or_else(|| {
                let names: Vec<&str> = self.projects.iter().map(|p| p.name.as_str()).collect();
                anyhow!("unknown project {name:?} (have: {})", names.join(", "))
            })
    }

    /// Next session change, or None if the daemon went away.
    async fn next_session(&mut self) -> Option<SessionInfo> {
        loop {
            match self.conn.rx.recv().await? {
                ServerEvent::SessionUpsert(s) => return Some(s),
                ServerEvent::Projects(p) => self.projects = p,
                _ => {}
            }
        }
    }

    /// Wait for `session` to exit; returns its exit code.
    async fn wait_exit(&mut self, session: &str) -> Result<Option<i32>> {
        loop {
            let s = self
                .next_session()
                .await
                .ok_or_else(|| anyhow!("daemon closed the connection"))?;
            if s.id == session && !s.alive {
                return Ok(s.exit_code);
            }
        }
    }
}

/// Find a worktree by path (as given or canonical) or branch name.
fn find_worktree<'a>(p: &'a ProjectInfo, spec: &str) -> Result<&'a WorktreeInfo> {
    let raw = hive_core::paths::expand_tilde(spec);
    let canon = std::fs::canonicalize(&raw).ok();
    let same = |w: &WorktreeInfo| match &canon {
        Some(c) => &w.path == c || std::fs::canonicalize(&w.path).ok().as_ref() == Some(c),
        None => w.path == raw,
    };
    p.worktrees
        .iter()
        .find(|w| same(w))
        .or_else(|| {
            p.worktrees
                .iter()
                .find(|w| w.branch.as_deref() == Some(spec))
        })
        .ok_or_else(|| anyhow!("no worktree {spec:?} in {}", p.name))
}

/// Every run proc's port in `slot`, keyed by proc name (target name for
/// single-proc targets).
fn slot_ports(p: &ProjectInfo, slot: u16) -> BTreeMap<String, u16> {
    let mut out = BTreeMap::new();
    for r in &p.runs {
        for proc in &r.procs {
            if let Some(base) = proc.base_port {
                let key = if r.procs.len() == 1 {
                    &r.name
                } else {
                    &proc.name
                };
                out.insert(
                    key.clone(),
                    hive_daemon::ports::port_for(base, slot, p.port_stride),
                );
            }
        }
    }
    out
}

fn check_steps(p: &ProjectInfo, steps: &[String]) -> Result<()> {
    for s in steps {
        if !p.steps.iter().any(|x| &x.id == s) {
            let ids: Vec<&str> = p.steps.iter().map(|x| x.id.as_str()).collect();
            return Err(usage(format!(
                "unknown setup step {s:?} (have: {})",
                ids.join(", ")
            )));
        }
    }
    Ok(())
}

/// The last lines a session printed, as plain text.
async fn tail(hive_bin: &Path, session: &str) -> String {
    let Ok(mut c) = Conn::open(hive_bin).await else {
        return String::new();
    };
    let attach = c.call(
        ClientRequest::Attach {
            session: session.into(),
            from_seq: 0,
            cols: COLS,
            rows: TAIL_LINES,
        },
        |e| match e {
            ServerEvent::Scrollback { data, .. } => Some(data.clone()),
            _ => None,
        },
    );
    let Ok(Ok(data)) = tokio::time::timeout(Duration::from_secs(5), attach).await else {
        return String::new();
    };
    let mut parser = vt100::Parser::new(TAIL_LINES, COLS, 0);
    parser.process(&data);
    parser.screen().contents().trim_end().to_string()
}

// ------------------------------------------------------------ commands

pub async fn wt(hive_bin: &Path, cmd: WtCmd) -> Result<Outcome> {
    match cmd {
        WtCmd::New {
            project,
            branch,
            base,
            mode,
            setup,
            env,
            wait,
            ..
        } => wt_new(hive_bin, &project, branch, base, mode, setup, env, wait).await,
        WtCmd::Rm {
            project,
            path,
            force,
            ..
        } => wt_rm(hive_bin, &project, &path, force).await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn wt_new(
    hive_bin: &Path,
    project: &str,
    branch: String,
    base: Option<String>,
    mode: Mode,
    steps: Vec<String>,
    env: Option<String>,
    wait: bool,
) -> Result<Outcome> {
    let mut watch = Watch::open(hive_bin).await?;
    let p = watch.project(project)?.clone();
    check_steps(&p, &steps)?;
    let mode = match mode {
        Mode::New => WorktreeMode::NewBranch,
        Mode::Local => WorktreeMode::Local,
        Mode::Remote => WorktreeMode::Remote,
        Mode::Detached => WorktreeMode::Detached,
    };
    let mut conn = Conn::open(hive_bin).await?;
    let (path, slot, setup_session) = conn
        .call(
            ClientRequest::CreateWorktree(CreateWorktree {
                project: p.id.clone(),
                mode,
                branch: branch.clone(),
                base,
                steps: steps.clone(),
                env,
            }),
            |e| match e {
                ServerEvent::WorktreeCreated {
                    path, slot, setup, ..
                } => Some((path.clone(), *slot, setup.clone())),
                _ => None,
            },
        )
        .await?;
    // A setup session without steps only reports the copy_from_main files.
    let (setup, output) = match setup_session {
        Some(id) if !steps.is_empty() && wait => match watch.wait_exit(&id).await? {
            Some(0) => ("ok", String::new()),
            _ => ("failed", tail(hive_bin, &id).await),
        },
        Some(_) if !steps.is_empty() => ("running", String::new()),
        _ => ("skipped", String::new()),
    };
    let ports = slot_ports(&p, slot);
    let mut json = json!({
        "path": path,
        "branch": branch,
        "slot": slot,
        "ports": ports,
        "setup": setup,
    });
    if !output.is_empty() {
        json["output"] = output.clone().into();
    }
    let text = format!(
        "{}  slot {slot}  setup {setup}",
        hive_core::paths::tildify(&path)
    );
    Ok(Outcome {
        ok: setup != "failed",
        json,
        text,
        output,
    })
}

async fn wt_rm(hive_bin: &Path, project: &str, spec: &str, force: bool) -> Result<Outcome> {
    let watch = Watch::open(hive_bin).await?;
    let p = watch.project(project)?;
    let w = find_worktree(p, spec)?;
    if w.is_main {
        bail!("refusing to remove the main checkout");
    }
    let (path, slot) = (w.path.clone(), w.slot);
    Conn::open(hive_bin)
        .await?
        .call(
            ClientRequest::RemoveWorktree {
                project: p.id.clone(),
                path: path.clone(),
                force,
            },
            |e| matches!(e, ServerEvent::Ok).then_some(()),
        )
        .await?;
    Ok(Outcome::ok(
        json!({ "path": path, "removed": true, "slot": slot }),
        format!("removed {}", hive_core::paths::tildify(&path)),
    ))
}

pub async fn setup(hive_bin: &Path, a: SetupArgs) -> Result<Outcome> {
    let mut watch = Watch::open(hive_bin).await?;
    let p = watch.project(&a.project)?.clone();
    check_steps(&p, &a.steps)?;
    let path = find_worktree(&p, &a.worktree)?.path.clone();
    let session = Conn::open(hive_bin)
        .await?
        .call(
            ClientRequest::RunSetup {
                project: p.id.clone(),
                worktree: path.clone(),
                steps: a.steps,
                env: a.env,
            },
            |e| match e {
                ServerEvent::SetupStarted { session } => Some(session.clone()),
                _ => None,
            },
        )
        .await?;
    if !a.wait {
        return Ok(Outcome::ok(
            json!({ "path": path, "session": session, "setup": "running" }),
            format!("setup running ({session})"),
        ));
    }
    let code = watch.wait_exit(&session).await?;
    let ok = code == Some(0);
    let output = if ok {
        String::new()
    } else {
        tail(hive_bin, &session).await
    };
    let status = if ok { "ok" } else { "failed" };
    let mut json = json!({
        "path": path,
        "session": session,
        "setup": status,
        "exit_code": code,
    });
    if !ok {
        json["output"] = output.clone().into();
    }
    Ok(Outcome {
        ok,
        json,
        text: format!("setup {status}"),
        output,
    })
}

pub async fn run(hive_bin: &Path, a: RunArgs) -> Result<Outcome> {
    let mut watch = Watch::open(hive_bin).await?;
    let p = watch.project(&a.project)?.clone();
    let path = find_worktree(&p, &a.worktree)?.path.clone();
    let target = p.runs.iter().find(|r| r.name == a.target).ok_or_else(|| {
        let names: Vec<&str> = p.runs.iter().map(|r| r.name.as_str()).collect();
        usage(format!(
            "unknown run target {:?} (have: {})",
            a.target,
            names.join(", ")
        ))
    })?;
    // Defaults first, then --ask; anything still unanswered is an error.
    let mut answers = BTreeMap::new();
    for ask in &target.asks {
        if let Some(d) = &ask.default {
            answers.insert(ask.name.clone(), d.clone());
        }
    }
    for (k, v) in a.asks {
        if !target.asks.iter().any(|x| x.name == k) {
            return Err(usage(format!("{} has no ask named {k:?}", target.name)));
        }
        answers.insert(k, v);
    }
    let missing: Vec<&str> = target
        .asks
        .iter()
        .filter(|x| !answers.contains_key(&x.name))
        .map(|x| x.name.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(usage(format!(
            "{} needs --ask {}",
            target.name,
            missing
                .iter()
                .map(|m| format!("{m}=…"))
                .collect::<Vec<_>>()
                .join(" --ask ")
        )));
    }
    let (sessions, ports) = Conn::open(hive_bin)
        .await?
        .call(
            ClientRequest::StartRun(StartRun {
                project: p.id.clone(),
                worktree: path,
                target: a.target.clone(),
                answers,
                cols: COLS,
                rows: ROWS,
            }),
            |e| match e {
                ServerEvent::RunStarted {
                    sessions, ports, ..
                } => Some((sessions.clone(), ports.clone())),
                _ => None,
            },
        )
        .await?;
    if a.wait_ports {
        wait_ports(hive_bin, &mut watch, &sessions, &ports).await?;
    }
    let list = ports
        .iter()
        .map(|(n, p)| format!("{n}:{p}"))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(Outcome::ok(
        json!({ "target": a.target, "ports": ports, "sessions": sessions }),
        format!("started {} {list}", a.target),
    ))
}

/// Poll every port until it accepts a connection. Fails if a proc exits
/// first or the timeout passes.
async fn wait_ports(
    hive_bin: &Path,
    watch: &mut Watch,
    sessions: &[String],
    ports: &BTreeMap<String, u16>,
) -> Result<()> {
    let deadline = Instant::now() + PORT_TIMEOUT;
    let mut pending: Vec<(String, u16)> = ports.iter().map(|(k, v)| (k.clone(), *v)).collect();
    loop {
        pending.retain(|(_, port)| !listening(*port));
        if pending.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let list: Vec<String> = pending.iter().map(|(n, p)| format!(":{p} ({n})")).collect();
            bail!(
                "timed out after {}s waiting for {}",
                PORT_TIMEOUT.as_secs(),
                list.join(", ")
            );
        }
        tokio::select! {
            s = watch.next_session() => {
                if let Some(s) = s.filter(|s| !s.alive && sessions.contains(&s.id)) {
                    let out = tail(hive_bin, &s.id).await;
                    bail!(
                        "{} exited (code {}) before its port came up\n{out}",
                        s.kind.label(),
                        s.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
                    );
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
}

fn listening(port: u16) -> bool {
    [
        SocketAddr::from(([127, 0, 0, 1], port)),
        SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port)),
    ]
    .iter()
    .any(|a| TcpStream::connect_timeout(a, Duration::from_millis(200)).is_ok())
}

pub async fn stop(hive_bin: &Path, a: StopArgs) -> Result<Outcome> {
    let watch = Watch::open(hive_bin).await?;
    let p = watch.project(&a.project)?;
    let path = find_worktree(p, &a.worktree)?.path.clone();
    if !p.runs.iter().any(|r| r.name == a.target) {
        return Err(usage(format!("unknown run target {:?}", a.target)));
    }
    Conn::open(hive_bin)
        .await?
        .call(
            ClientRequest::StopRun {
                project: p.id.clone(),
                worktree: path.clone(),
                target: a.target.clone(),
            },
            |e| matches!(e, ServerEvent::Ok).then_some(()),
        )
        .await?;
    Ok(Outcome::ok(
        json!({ "target": a.target, "path": path, "stopped": true }),
        format!("stopped {}", a.target),
    ))
}

/// `hive ls --json`.
pub fn list_json(projects: &[ProjectInfo], sessions: &[SessionInfo]) -> Value {
    let projects: Vec<Value> = projects
        .iter()
        .map(|p| {
            let worktrees: Vec<Value> = p
                .worktrees
                .iter()
                .map(|w| {
                    let sessions: Vec<Value> = sessions
                        .iter()
                        .filter(|s| s.worktree == w.path)
                        .map(|s| {
                            json!({
                                "id": s.id,
                                "kind": s.kind.label(),
                                "status": if s.alive {
                                    format!("{:?}", s.status).to_lowercase()
                                } else {
                                    "exited".into()
                                },
                                "alive": s.alive,
                                "exit_code": s.exit_code,
                                "title": s.title,
                                "ports": s.ports,
                            })
                        })
                        .collect();
                    json!({
                        "branch": w.branch,
                        "path": w.path,
                        "slot": w.slot,
                        "is_main": w.is_main,
                        "dirty": w.dirty,
                        "ports": slot_ports(p, w.slot),
                        "sessions": sessions,
                    })
                })
                .collect();
            json!({
                "id": p.id,
                "name": p.name,
                "root": p.root,
                "worktrees": worktrees,
            })
        })
        .collect();
    json!({ "projects": projects })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_core::protocol::{RunInfo, RunProcInfo};

    fn project() -> ProjectInfo {
        let proc = |name: &str, port| RunProcInfo {
            name: name.into(),
            base_port: port,
        };
        ProjectInfo {
            id: "id".into(),
            name: "app".into(),
            root: "/r".into(),
            worktree_root: "/w".into(),
            base_branch: "main".into(),
            config_source: None,
            config_error: None,
            worktrees: vec![],
            packages: vec![],
            runs: vec![
                RunInfo {
                    name: "web".into(),
                    procs: vec![proc("portal", Some(3000)), proc("wagering", Some(4000))],
                    asks: vec![],
                },
                RunInfo {
                    name: "ios".into(),
                    procs: vec![proc("metro", Some(8081))],
                    asks: vec![],
                },
                RunInfo {
                    name: "lint".into(),
                    procs: vec![proc("lint", None)],
                    asks: vec![],
                },
            ],
            steps: vec![],
            profiles: vec![],
            env_choices: vec![],
            port_stride: 10,
        }
    }

    #[test]
    fn ports_for_slot() {
        let ports = slot_ports(&project(), 7);
        assert_eq!(ports.get("portal"), Some(&3070));
        assert_eq!(ports.get("wagering"), Some(&4070));
        assert_eq!(ports.get("ios"), Some(&8151));
        assert_eq!(ports.len(), 3);
    }

    #[test]
    fn ask_args() {
        assert_eq!(
            parse_ask("script=start"),
            Ok(("script".into(), "start".into()))
        );
        assert_eq!(parse_ask("a=b=c"), Ok(("a".into(), "b=c".into())));
        assert!(parse_ask("=x").is_err() && parse_ask("x").is_err());
    }
}
