//! Daemon state and request handling.
//!
//! Locks are std mutexes and are never held across an await. Slow work (git,
//! port probes, `choices_cmd`) runs on the blocking pool.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use hive_core::config::{GlobalConfig, ProjectConfig};
use hive_core::protocol::*;
use hive_core::template::{render, shell_join, Vars};
use tokio::sync::mpsc::UnboundedSender;

use crate::commands;
use crate::git;
use crate::ports;
use crate::pty::{PtyEvent, PtySession, SpawnSpec};
use crate::status;
use crate::store::{Store, StoredSession};

pub type Outbox = UnboundedSender<ServerEvent>;

pub struct Project {
    pub id: String,
    pub cfg: ProjectConfig,
    pub worktrees: Vec<WorktreeInfo>,
    pub base_branch: String,
    pub env_choices: Vec<String>,
}

pub struct Session {
    pub info: SessionInfo,
    pub pty: Option<Arc<PtySession>>,
}

#[derive(Default)]
pub struct State {
    pub projects: Vec<Project>,
    pub sessions: Vec<Session>,
}

pub struct Daemon {
    pub store: Store,
    pub global: RwLock<GlobalConfig>,
    pub state: Mutex<State>,
    clients: Mutex<HashMap<u64, Outbox>>,
    next_client: AtomicU64,
    pub hive_bin: PathBuf,
    pub shutdown: tokio::sync::Notify,
    /// Serialises git operations across clients (avoids index.lock races).
    git_lock: tokio::sync::Mutex<()>,
}

impl Daemon {
    pub fn new(store: Store, hive_bin: PathBuf) -> Arc<Self> {
        let global = GlobalConfig::load().unwrap_or_else(|e| {
            tracing::warn!("config: {e:#}");
            GlobalConfig::default()
        });
        Arc::new(Self {
            store,
            global: RwLock::new(global),
            state: Mutex::new(State::default()),
            clients: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(1),
            hive_bin,
            shutdown: tokio::sync::Notify::new(),
            git_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Load projects and resumable sessions, write the claude hook settings.
    pub async fn bootstrap(self: &Arc<Self>) -> Result<()> {
        let settings = hive_core::paths::claude_settings_file();
        std::fs::create_dir_all(settings.parent().unwrap())?;
        std::fs::write(
            &settings,
            serde_json::to_string_pretty(&hive_core::agents::claude_hook_settings())?,
        )?;

        let extra: Vec<String> = self.global.read().unwrap().projects.clone();
        for p in extra {
            let path = hive_core::paths::expand_tilde(&p);
            if let Ok(root) = git::main_root(&path) {
                self.store.add_project(&root)?;
            }
        }
        let mut projects = Vec::new();
        for (id, root) in self.store.projects()? {
            projects.push(Project {
                id,
                cfg: ProjectConfig::load(&root),
                worktrees: vec![],
                base_branch: String::new(),
                env_choices: vec![],
            });
        }
        let mut sessions = Vec::new();
        for s in self.store.sessions()? {
            // Only conversations worth resuming come back after a restart.
            if s.kind.is_agent() && s.agent_session.is_some() && s.last_prompt.is_some() {
                sessions.push(Session {
                    info: stored_to_info(&s),
                    pty: None,
                });
            } else {
                let _ = self.store.delete_session(&s.id);
            }
        }
        {
            let mut st = self.state.lock().unwrap();
            st.projects = projects;
            st.sessions = sessions;
        }
        // Worktrees are scanned by the server's poller right after it starts
        // accepting, so a slow `git status` never delays the first client.
        Ok(())
    }

    // ------------------------------------------------------------ clients

    pub fn add_client(&self, tx: Outbox) -> u64 {
        let id = self.next_client.fetch_add(1, Ordering::Relaxed);
        self.clients.lock().unwrap().insert(id, tx);
        id
    }

    pub fn remove_client(&self, id: u64) {
        self.clients.lock().unwrap().remove(&id);
    }

    pub fn broadcast(&self, ev: ServerEvent) {
        let clients = self.clients.lock().unwrap();
        for tx in clients.values() {
            let _ = tx.send(ev.clone());
        }
    }

    fn toast(&self, level: ToastLevel, message: impl Into<String>) {
        self.broadcast(ServerEvent::Toast {
            level,
            message: message.into(),
        });
    }

    pub fn snapshot(&self) -> ServerEvent {
        let st = self.state.lock().unwrap();
        ServerEvent::Snapshot {
            projects: st.projects.iter().map(project_info).collect(),
            sessions: st.sessions.iter().map(|s| s.info.clone()).collect(),
            ui_state: self.store.get_kv("ui_state").ok().flatten(),
        }
    }

    fn broadcast_projects(&self) {
        let infos = {
            let st = self.state.lock().unwrap();
            st.projects.iter().map(project_info).collect()
        };
        self.broadcast(ServerEvent::Projects(infos));
    }

    fn upsert(&self, id: &str) {
        let info = {
            let st = self.state.lock().unwrap();
            st.sessions
                .iter()
                .find(|s| s.info.id == id)
                .map(|s| s.info.clone())
        };
        if let Some(info) = info {
            if info.kind.is_agent() {
                let _ = self.store.save_session(&info_to_stored(&info));
            }
            self.broadcast(ServerEvent::SessionUpsert(info));
        }
    }

    // ------------------------------------------------------------ projects

    /// Re-list worktrees (and optionally dirty state) for every project.
    pub async fn refresh_all(self: &Arc<Self>, with_dirty: bool) {
        let roots: Vec<(String, PathBuf, Vec<WorktreeInfo>)> = {
            let st = self.state.lock().unwrap();
            st.projects
                .iter()
                .map(|p| (p.id.clone(), p.cfg.root.clone(), p.worktrees.clone()))
                .collect()
        };
        let mut changed = false;
        for (id, root, prev) in roots {
            let me = self.clone();
            let id2 = id.clone();
            let res = tokio::task::spawn_blocking(move || {
                me.scan_worktrees(&id2, &root, &prev, with_dirty)
            })
            .await;
            let Ok(Ok((wts, base, envs))) = res else {
                continue;
            };
            let mut st = self.state.lock().unwrap();
            if let Some(p) = st.projects.iter_mut().find(|p| p.id == id) {
                if p.worktrees != wts || p.base_branch != base || p.env_choices != envs {
                    p.worktrees = wts;
                    p.base_branch = base;
                    p.env_choices = envs;
                    changed = true;
                }
            }
        }
        if changed {
            self.broadcast_projects();
        }
    }

    fn scan_worktrees(
        &self,
        project: &str,
        root: &Path,
        prev: &[WorktreeInfo],
        with_dirty: bool,
    ) -> Result<(Vec<WorktreeInfo>, String, Vec<String>)> {
        let raw = git::list_worktrees(root)?;
        let mut slots = self.store.slots(project)?;
        let mut out = Vec::new();
        for (i, w) in raw.iter().enumerate() {
            let is_main = i == 0;
            let slot = if is_main {
                0
            } else if let Some((_, s)) = slots.iter().find(|(p, _)| p == &w.path) {
                *s
            } else {
                let used: Vec<u16> = slots.iter().map(|(_, s)| *s).collect();
                let s = ports::next_free_slot(&used);
                self.store.set_slot(project, &w.path, s)?;
                slots.push((w.path.clone(), s));
                s
            };
            let dirty = if with_dirty || !prev.iter().any(|p| p.path == w.path) {
                !w.prunable && git::is_dirty(&w.path)
            } else {
                prev.iter()
                    .find(|p| p.path == w.path)
                    .map(|p| p.dirty)
                    .unwrap_or(false)
            };
            out.push(WorktreeInfo {
                path: w.path.clone(),
                branch: w.branch.clone(),
                head: w.head.clone(),
                is_main,
                slot,
                dirty,
                prunable: w.prunable,
            });
        }
        // Forget slots of worktrees that no longer exist.
        for (p, _) in &slots {
            if !raw.iter().any(|w| &w.path == p) {
                let _ = self.store.free_slot(p);
            }
        }
        let (base, envs) = {
            let st = self.state.lock().unwrap();
            let p = st.projects.iter().find(|p| p.id == project);
            (
                p.and_then(|p| p.cfg.base_branch.clone()),
                p.map(|p| p.cfg.env_choices()).unwrap_or_default(),
            )
        };
        let base = base.unwrap_or_else(|| git::default_branch(root));
        Ok((out, base, envs))
    }

    fn project_cfg(&self, id: &str) -> Result<ProjectConfig> {
        let st = self.state.lock().unwrap();
        st.projects
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.cfg.clone())
            .ok_or_else(|| anyhow!("unknown project"))
    }

    fn worktree(&self, project: &str, path: &Path) -> Result<WorktreeInfo> {
        let st = self.state.lock().unwrap();
        st.projects
            .iter()
            .find(|p| p.id == project)
            .and_then(|p| p.worktrees.iter().find(|w| w.path == path))
            .cloned()
            .ok_or_else(|| anyhow!("unknown worktree {}", path.display()))
    }

    async fn add_project(self: &Arc<Self>, path: PathBuf) -> Result<()> {
        let path = if path.is_relative() {
            std::env::current_dir()?.join(path)
        } else {
            path
        };
        let root = tokio::task::spawn_blocking(move || git::main_root(&path)).await??;
        let id = self.store.add_project(&root)?;
        {
            let mut st = self.state.lock().unwrap();
            if st.projects.iter().any(|p| p.id == id) {
                bail!("{} is already added", root.display());
            }
            st.projects.push(Project {
                id,
                cfg: ProjectConfig::load(&root),
                worktrees: vec![],
                base_branch: String::new(),
                env_choices: vec![],
            });
        }
        self.refresh_all(true).await;
        self.broadcast_projects();
        self.toast(
            ToastLevel::Info,
            format!("added {}", hive_core::paths::tildify(&root)),
        );
        Ok(())
    }

    fn remove_project(&self, id: &str) -> Result<()> {
        let ids: Vec<String> = {
            let st = self.state.lock().unwrap();
            st.sessions
                .iter()
                .filter(|s| s.info.project == id)
                .map(|s| s.info.id.clone())
                .collect()
        };
        for s in ids {
            self.close_session(&s);
        }
        self.store.remove_project(id)?;
        self.state.lock().unwrap().projects.retain(|p| p.id != id);
        self.broadcast_projects();
        Ok(())
    }

    async fn reload_config(self: &Arc<Self>) {
        match GlobalConfig::load() {
            Ok(g) => *self.global.write().unwrap() = g,
            Err(e) => self.toast(ToastLevel::Error, format!("{e:#}")),
        }
        let errors: Vec<String> = {
            let mut st = self.state.lock().unwrap();
            for p in st.projects.iter_mut() {
                p.cfg = ProjectConfig::load(&p.cfg.root);
            }
            st.projects
                .iter()
                .filter_map(|p| p.cfg.error.clone())
                .collect()
        };
        for e in errors {
            self.toast(ToastLevel::Error, e);
        }
        // Force a rebroadcast even if worktrees are unchanged.
        self.state
            .lock()
            .unwrap()
            .projects
            .iter_mut()
            .for_each(|p| p.base_branch.clear());
        self.refresh_all(true).await;
        self.toast(ToastLevel::Info, "config reloaded");
    }

    // ------------------------------------------------------------ sessions

    fn base_env(&self, session: &str, project: &str, worktree: &Path) -> Vec<(String, String)> {
        vec![
            ("HIVE_SESSION_ID".into(), session.into()),
            ("HIVE_BIN".into(), self.hive_bin.display().to_string()),
            (
                "HIVE_SOCKET".into(),
                hive_core::paths::socket_path().display().to_string(),
            ),
            ("HIVE_PROJECT".into(), project.into()),
            ("HIVE_WORKTREE".into(), worktree.display().to_string()),
        ]
    }

    fn launch(
        &self,
        info: &SessionInfo,
        program: String,
        args: Vec<String>,
        extra_env: Vec<(String, String)>,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<PtySession>> {
        let mut env = self.base_env(&info.id, &info.project, &info.worktree);
        env.extend(extra_env);
        let cwd = if info.cwd.is_dir() {
            info.cwd.clone()
        } else {
            info.worktree.clone()
        };
        PtySession::spawn(SpawnSpec {
            program,
            args,
            cwd,
            env,
            cols,
            rows,
        })
    }

    /// Add (or replace) a session entry with a live PTY and watch it.
    fn register(self: &Arc<Self>, info: SessionInfo, pty: Arc<PtySession>) {
        let id = info.id.clone();
        let mut rx = pty.subscribe();
        {
            let mut st = self.state.lock().unwrap();
            match st.sessions.iter_mut().find(|s| s.info.id == id) {
                Some(s) => {
                    s.info = info;
                    s.pty = Some(pty.clone());
                }
                None => st.sessions.push(Session {
                    info,
                    pty: Some(pty.clone()),
                }),
            }
        }
        self.upsert(&id);
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(PtyEvent::Exited(code)) => break me.on_exit(&id, &pty, code),
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break me.on_exit(&id, &pty, pty.exit_code().flatten()),
                }
            }
        });
    }

    fn on_exit(&self, id: &str, pty: &Arc<PtySession>, code: Option<i32>) {
        {
            let mut st = self.state.lock().unwrap();
            let Some(s) = st.sessions.iter_mut().find(|s| s.info.id == id) else {
                return;
            };
            // A resumed session may have replaced the PTY already.
            if !s.pty.as_ref().map(|p| Arc::ptr_eq(p, pty)).unwrap_or(false) {
                return;
            }
            s.info.alive = false;
            s.info.exit_code = code;
            s.info.status = Status::Exited;
        }
        self.upsert(id);
    }

    fn new_info(
        &self,
        project: &str,
        worktree: &Path,
        kind: SessionKind,
        cwd: PathBuf,
        title: String,
    ) -> SessionInfo {
        SessionInfo {
            id: hive_core::new_id(),
            project: project.into(),
            worktree: worktree.to_path_buf(),
            kind,
            title,
            cwd,
            status: Status::Idle,
            alive: true,
            exit_code: None,
            ports: vec![],
            agent_session: None,
            created_at: hive_core::now_ms(),
            last_prompt: None,
        }
    }

    fn command_for(&self, info: &SessionInfo, resume: Option<&str>) -> (String, Vec<String>) {
        let g = self.global.read().unwrap();
        let shell = g.shell();
        match &info.kind {
            SessionKind::Claude | SessionKind::Codex => {
                let cmd = if info.kind == SessionKind::Claude {
                    &g.agents.claude
                } else {
                    &g.agents.codex
                };
                let argv = hive_core::agents::launch_argv(
                    &info.kind,
                    cmd,
                    &self.hive_bin,
                    &hive_core::paths::claude_settings_file(),
                    resume,
                );
                commands::interactive_login_shell(&shell, format!("exec {}", shell_join(&argv)))
            }
            _ => (shell, vec!["-l".into()]),
        }
    }

    fn spawn_session(self: &Arc<Self>, req: SpawnSession) -> Result<String> {
        self.worktree(&req.project, &req.worktree)?;
        let cwd = req.cwd.clone().unwrap_or_else(|| req.worktree.clone());
        let info = self.new_info(
            &req.project,
            &req.worktree,
            req.kind.clone(),
            cwd,
            req.kind.label(),
        );
        let (program, args) = self.command_for(&info, None);
        let pty = self.launch(&info, program, args, vec![], req.cols, req.rows)?;
        let id = info.id.clone();
        self.register(info, pty);
        Ok(id)
    }

    fn resume_session(self: &Arc<Self>, id: &str, cols: u16, rows: u16) -> Result<()> {
        let mut info = {
            let st = self.state.lock().unwrap();
            st.sessions
                .iter()
                .find(|s| s.info.id == id)
                .map(|s| s.info.clone())
                .ok_or_else(|| anyhow!("unknown session"))?
        };
        if info.alive {
            return Ok(());
        }
        let resume = info.agent_session.clone();
        let (program, args) = match &info.kind {
            SessionKind::Claude | SessionKind::Codex | SessionKind::Shell => {
                self.command_for(&info, resume.as_deref())
            }
            _ => bail!("only agent and shell sessions can be resumed; restart runs with R"),
        };
        info.alive = true;
        info.exit_code = None;
        info.status = Status::Idle;
        let pty = self.launch(&info, program, args, vec![], cols, rows)?;
        self.register(info, pty);
        Ok(())
    }

    pub fn kill_session(&self, id: &str) {
        let pty = {
            let st = self.state.lock().unwrap();
            st.sessions
                .iter()
                .find(|s| s.info.id == id)
                .and_then(|s| s.pty.clone())
        };
        if let Some(p) = pty {
            if p.is_alive() {
                p.kill();
            }
        }
    }

    pub fn close_session(&self, id: &str) {
        self.kill_session(id);
        let removed = {
            let mut st = self.state.lock().unwrap();
            let before = st.sessions.len();
            st.sessions.retain(|s| s.info.id != id);
            before != st.sessions.len()
        };
        let _ = self.store.delete_session(id);
        if removed {
            self.broadcast(ServerEvent::SessionRemoved { session: id.into() });
        }
    }

    fn pty(&self, id: &str) -> Option<Arc<PtySession>> {
        let st = self.state.lock().unwrap();
        st.sessions
            .iter()
            .find(|s| s.info.id == id)
            .and_then(|s| s.pty.clone())
    }

    pub fn attach(
        &self,
        id: &str,
        from_seq: u64,
        cols: u16,
        rows: u16,
    ) -> Option<crate::pty::Attachment> {
        let pty = self.pty(id)?;
        pty.resize(cols, rows, true);
        Some(pty.attach(from_seq))
    }

    /// Re-snapshot for a client that fell behind; leaves the size alone.
    pub fn resync(&self, id: &str, from_seq: u64) -> Option<crate::pty::Attachment> {
        Some(self.pty(id)?.attach(from_seq))
    }

    fn input(&self, id: &str, data: Vec<u8>) {
        let (pty, is_codex_submit) = {
            let mut st = self.state.lock().unwrap();
            let Some(s) = st.sessions.iter_mut().find(|s| s.info.id == id) else {
                return;
            };
            // Codex only reports turn completion; treat Enter as "working".
            let submit = s.info.kind == SessionKind::Codex
                && s.info.alive
                && data.contains(&b'\r')
                && s.info.status != Status::Working;
            if submit {
                s.info.status = Status::Working;
            }
            (s.pty.clone(), submit)
        };
        if let Some(p) = pty {
            p.write(data);
        }
        if is_codex_submit {
            self.upsert(id);
        }
    }

    fn resize(&self, id: &str, cols: u16, rows: u16) {
        if let Some(p) = self.pty(id) {
            p.resize(cols, rows, false);
        }
    }

    fn notify(&self, n: Notify) {
        let payload: serde_json::Value =
            serde_json::from_str(&n.payload).unwrap_or(serde_json::Value::Null);
        {
            let mut st = self.state.lock().unwrap();
            let Some(s) = st.sessions.iter_mut().find(|s| s.info.id == n.session) else {
                return;
            };
            let u = status::interpret(&n.agent, &n.event, &payload, s.info.status);
            if let Some(a) = u.agent_session {
                s.info.agent_session = Some(a);
            }
            if let Some(p) = u.prompt {
                let t = status::title_from_prompt(&p);
                if !t.is_empty() {
                    s.info.title = t;
                }
                s.info.last_prompt = Some(p);
            }
            if let Some(stt) = u.status {
                if s.info.alive {
                    s.info.status = stt;
                }
            }
        }
        self.upsert(&n.session);
    }

    fn mark_seen(&self, id: &str) {
        let changed = {
            let mut st = self.state.lock().unwrap();
            match st.sessions.iter_mut().find(|s| s.info.id == id) {
                Some(s) if s.info.status == Status::Done => {
                    s.info.status = Status::Idle;
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.upsert(id);
        }
    }

    // ------------------------------------------------------------ worktrees & setup

    async fn create_worktree(
        self: &Arc<Self>,
        req: CreateWorktree,
        cols: u16,
        rows: u16,
    ) -> Result<()> {
        let cfg = self.project_cfg(&req.project)?;
        let (repo, root) = (cfg.root.clone(), cfg.worktree_root.clone());
        let (mode, branch, base) = (req.mode, req.branch.clone(), req.base.clone());
        self.toast(ToastLevel::Info, format!("creating worktree {branch}…"));
        let (path, _) = tokio::task::spawn_blocking(move || {
            git::add_worktree(&repo, &root, mode, &branch, base.as_deref())
        })
        .await??;
        self.refresh_all(false).await;
        self.toast(
            ToastLevel::Info,
            format!("created {}", hive_core::paths::tildify(&path)),
        );
        if !req.steps.is_empty()
            || !cfg.copy_from_main.is_empty()
            || cfg.packages.iter().any(|p| !p.copy_from_main.is_empty())
        {
            self.run_setup(&req.project, &path, &req.steps, req.env.clone(), cols, rows)?;
        }
        Ok(())
    }

    /// Copy gitignored files from the main checkout, then run setup steps in
    /// a visible "setup" session.
    fn run_setup(
        self: &Arc<Self>,
        project: &str,
        worktree: &Path,
        steps: &[String],
        env: Option<String>,
        cols: u16,
        rows: u16,
    ) -> Result<()> {
        let cfg = self.project_cfg(project)?;
        let wt = self.worktree(project, worktree)?;
        let mut notes = Vec::new();
        if !wt.is_main {
            let mut files: Vec<PathBuf> = cfg.copy_from_main.iter().map(PathBuf::from).collect();
            for p in &cfg.packages {
                files.extend(p.copy_from_main.iter().map(|f| p.path.join(f)));
            }
            for rel in files {
                let (src, dst) = (cfg.root.join(&rel), worktree.join(&rel));
                if dst.exists() {
                    notes.push(format!("• {} already present", rel.display()));
                } else if src.exists() {
                    if let Some(d) = dst.parent() {
                        std::fs::create_dir_all(d)?;
                    }
                    std::fs::copy(&src, &dst)
                        .with_context(|| format!("copying {}", rel.display()))?;
                    notes.push(format!("• copied {} from main checkout", rel.display()));
                } else {
                    notes.push(format!(
                        "• {} not found in main checkout, skipped",
                        rel.display()
                    ));
                }
            }
        }
        let mut vars = self.vars_for(&cfg, &wt);
        if let Some(e) = env {
            vars.insert("env".into(), e);
        }
        let plan = commands::plan_setup(&cfg, worktree, steps, &vars)?;
        let script = commands::setup_script(&cfg.node, &notes, &plan);
        let shell = self.global.read().unwrap().shell();
        let (program, args) = commands::login_shell(&shell, script);
        let info = self.new_info(
            project,
            worktree,
            SessionKind::Setup,
            worktree.to_path_buf(),
            "setup".into(),
        );
        let pty = self.launch(&info, program, args, vec![], cols, rows)?;
        let id = info.id.clone();
        self.register(info, pty);
        self.broadcast(ServerEvent::Focus { session: id });
        Ok(())
    }

    async fn remove_worktree(
        self: &Arc<Self>,
        project: &str,
        path: PathBuf,
        force: bool,
    ) -> Result<()> {
        let cfg = self.project_cfg(project)?;
        let ids: Vec<String> = {
            let st = self.state.lock().unwrap();
            st.sessions
                .iter()
                .filter(|s| s.info.worktree == path)
                .map(|s| s.info.id.clone())
                .collect()
        };
        for id in &ids {
            self.close_session(id);
        }
        if !ids.is_empty() {
            // Give dev servers a moment to let go of files.
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        self.toast(
            ToastLevel::Info,
            format!("removing {}…", hive_core::paths::tildify(&path)),
        );
        let repo = cfg.root.clone();
        let p2 = path.clone();
        tokio::task::spawn_blocking(move || git::remove_worktree(&repo, &p2, force)).await??;
        let _ = self.store.free_slot(&path);
        self.refresh_all(false).await;
        self.toast(
            ToastLevel::Info,
            format!("removed {}", hive_core::paths::tildify(&path)),
        );
        Ok(())
    }

    // ------------------------------------------------------------ runs

    fn vars_for(&self, cfg: &ProjectConfig, wt: &WorktreeInfo) -> Vars {
        let mut v = Vars::new();
        v.insert("slot".into(), wt.slot.to_string());
        v.insert("branch".into(), wt.label());
        v.insert("worktree".into(), wt.path.display().to_string());
        v.insert("project".into(), cfg.name.clone());
        let env = wt
            .branch
            .as_deref()
            .and_then(|b| cfg.profile_for_branch(b))
            .and_then(|p| p.env.clone())
            .unwrap_or_else(|| "staging".into());
        v.insert("env".into(), env);
        v
    }

    async fn start_run(self: &Arc<Self>, req: StartRun) -> Result<()> {
        let cfg = self.project_cfg(&req.project)?;
        let wt = self.worktree(&req.project, &req.worktree)?;
        let target = cfg
            .run(&req.target)
            .ok_or_else(|| anyhow!("unknown run target {:?}", req.target))?
            .clone();

        // Restart semantics: stop this target's existing procs first.
        let old = self.run_sessions(&req.project, &req.worktree, &req.target);
        let had_live = old.iter().any(|(_, alive)| *alive);
        for (id, _) in &old {
            self.close_session(id);
        }

        let mut vars = self.vars_for(&cfg, &wt);
        for (k, v) in &req.answers {
            vars.insert(k.clone(), v.clone());
        }
        let mut ports_by_proc: BTreeMap<String, u16> = BTreeMap::new();
        for p in &target.procs {
            if let Some(base) = p.port {
                let port = ports::port_for(base, wt.slot, cfg.port_stride);
                ports_by_proc.insert(p.name.clone(), port);
                vars.insert(format!("port.{}", p.name), port.to_string());
            }
        }

        // Strict port check (wait a little for a restart to free them).
        let wanted: Vec<(String, u16)> =
            ports_by_proc.iter().map(|(k, v)| (k.clone(), *v)).collect();
        let busy = tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now()
                + if had_live {
                    Duration::from_secs(4)
                } else {
                    Duration::ZERO
                };
            loop {
                let busy: Vec<(String, u16)> = wanted
                    .iter()
                    .filter(|(_, p)| ports::in_use(*p))
                    .cloned()
                    .collect();
                if busy.is_empty() || std::time::Instant::now() >= deadline {
                    return busy;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        })
        .await?;
        if !busy.is_empty() {
            let list = busy
                .iter()
                .map(|(n, p)| format!(":{p} ({n})"))
                .collect::<Vec<_>>()
                .join(", ");
            bail!("port {list} already in use — stop whatever is listening there (another worktree's run?) and retry");
        }

        let shell = self.global.read().unwrap().shell();
        let mut first = None;
        for p in &target.procs {
            let mut v = vars.clone();
            if let Some(port) = ports_by_proc.get(&p.name) {
                v.insert("port".into(), port.to_string());
            }
            let cmd =
                render(&p.cmd, &v).with_context(|| format!("run {}·{}", target.name, p.name))?;
            let mut env = BTreeMap::new();
            for (k, val) in &p.env {
                env.insert(k.clone(), render(val, &v)?);
            }
            let wait = match &p.wait_for_port {
                Some(other) => Some((
                    other.clone(),
                    *ports_by_proc
                        .get(other)
                        .ok_or_else(|| anyhow!("wait_for_port: {other} has no port"))?,
                )),
                None => None,
            };
            let script = commands::run_proc_script(&cfg.node, p.node.as_deref(), &env, wait, &cmd);
            let (program, args) = commands::login_shell(&shell, script);
            let cwd = cfg.resolve_dir(&req.worktree, p.cwd.as_deref());
            let kind = SessionKind::Run {
                target: target.name.clone(),
                proc_name: p.name.clone(),
            };
            let mut info =
                self.new_info(&req.project, &req.worktree, kind.clone(), cwd, kind.label());
            info.ports = ports_by_proc.get(&p.name).copied().into_iter().collect();
            let pty = self.launch(&info, program, args, vec![], req.cols, req.rows)?;
            first.get_or_insert(info.id.clone());
            self.register(info, pty);
        }
        if let Some(id) = first {
            self.broadcast(ServerEvent::Focus { session: id });
        }
        Ok(())
    }

    fn run_sessions(&self, project: &str, worktree: &Path, target: &str) -> Vec<(String, bool)> {
        let st = self.state.lock().unwrap();
        st.sessions
            .iter()
            .filter(|s| {
                s.info.project == project
                    && s.info.worktree == worktree
                    && s.info.group() == Some(target)
            })
            .map(|s| (s.info.id.clone(), s.info.alive))
            .collect()
    }

    fn stop_run(&self, project: &str, worktree: &Path, target: &str) {
        for (id, _) in self.run_sessions(project, worktree, target) {
            self.kill_session(&id);
        }
    }

    async fn ask_choices(
        self: &Arc<Self>,
        project: &str,
        worktree: &Path,
        target: &str,
        name: &str,
    ) -> Result<Vec<String>> {
        let cfg = self.project_cfg(project)?;
        let ask = cfg
            .run(target)
            .and_then(|r| r.asks.iter().find(|a| a.name == name))
            .cloned()
            .ok_or_else(|| anyhow!("unknown prompt {name}"))?;
        let mut choices = ask.choices.clone();
        if let Some(cmd) = ask.choices_cmd {
            let dir = worktree.to_path_buf();
            let shell = self.global.read().unwrap().shell();
            let out = tokio::task::spawn_blocking(move || {
                std::process::Command::new(shell)
                    .args(["-l", "-c", &cmd])
                    .current_dir(dir)
                    .output()
            })
            .await??;
            choices.extend(
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(String::from),
            );
        }
        let mut seen = std::collections::HashSet::new();
        choices.retain(|c| seen.insert(c.clone()));
        Ok(choices)
    }

    // ------------------------------------------------------------ source control

    /// The project config owning a known worktree (requests for paths hive
    /// doesn't manage are refused).
    fn cfg_for_worktree(&self, worktree: &Path) -> Result<ProjectConfig> {
        let st = self.state.lock().unwrap();
        st.projects
            .iter()
            .find(|p| p.worktrees.iter().any(|w| w.path == worktree))
            .map(|p| p.cfg.clone())
            .ok_or_else(|| anyhow!("unknown worktree {}", worktree.display()))
    }

    async fn git_op<T: Send + 'static>(
        &self,
        worktree: &Path,
        f: impl FnOnce(&Path) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.cfg_for_worktree(worktree)?;
        let wt = worktree.to_path_buf();
        let _guard = self.git_lock.lock().await;
        tokio::task::spawn_blocking(move || f(&wt)).await?
    }

    async fn send_git_status(self: &Arc<Self>, worktree: &Path, reply: &Outbox) -> Result<()> {
        let info = self.git_op(worktree, crate::scm::status).await?;
        let _ = reply.send(ServerEvent::GitStatus(info));
        Ok(())
    }

    /// After a change: fresh status for the panel, and the sidebar's dirty
    /// marker in the background.
    async fn after_git_change(self: &Arc<Self>, worktree: &Path, reply: &Outbox) -> Result<()> {
        self.send_git_status(worktree, reply).await?;
        let me = self.clone();
        tokio::spawn(async move { me.refresh_all(true).await });
        Ok(())
    }

    async fn git_commit(
        self: &Arc<Self>,
        worktree: &Path,
        message: &str,
        reply: &Outbox,
    ) -> Result<()> {
        let cfg = self.cfg_for_worktree(worktree)?;
        if message.trim().is_empty() {
            bail!("commit message is empty");
        }
        let staged = self
            .git_op(worktree, crate::scm::status)
            .await?
            .files
            .iter()
            .any(|f| f.staged.is_some());
        if !staged {
            bail!("nothing staged — stage files with space first");
        }
        let dir = hive_core::paths::state_dir().join("commit-msgs");
        std::fs::create_dir_all(&dir)?;
        let msg_file = dir.join(format!("{}.txt", hive_core::new_id()));
        std::fs::write(&msg_file, message)?;
        // Through a login shell with the project's node, so hooks (husky,
        // lint-staged…) find the same tools as in a terminal.
        let script = format!(
            "{}git commit -F {}",
            commands::nvm_prelude(&cfg.node, None),
            hive_core::template::shell_quote(&msg_file.display().to_string())
        );
        let shell = self.global.read().unwrap().shell();
        let wt = worktree.to_path_buf();
        self.toast(ToastLevel::Info, "committing…");
        let guard = self.git_lock.lock().await;
        let out = tokio::task::spawn_blocking(move || {
            std::process::Command::new(shell)
                .args(["-l", "-c", &script])
                .current_dir(&wt)
                .env("GIT_TERMINAL_PROMPT", "0")
                .stdin(std::process::Stdio::null())
                .output()
        })
        .await??;
        drop(guard);
        let _ = std::fs::remove_file(&msg_file);
        let ok = out.status.success();
        let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
        output.push_str(&String::from_utf8_lossy(&out.stderr));
        let _ = reply.send(ServerEvent::GitCommitted {
            worktree: worktree.to_path_buf(),
            ok,
            output,
        });
        self.after_git_change(worktree, reply).await
    }

    // ------------------------------------------------------------ dispatch

    /// Handle one request. Attach/Detach are handled by the connection.
    pub async fn handle(self: &Arc<Self>, req: ClientRequest, reply: &Outbox, size: (u16, u16)) {
        let (cols, rows) = size;
        let res: Result<()> = async {
            match req {
                ClientRequest::Hello { .. } => {
                    let _ = reply.send(ServerEvent::Hello {
                        version: hive_core::PROTOCOL_VERSION,
                        pid: std::process::id(),
                    });
                }
                ClientRequest::Subscribe => {
                    let _ = reply.send(self.snapshot());
                }
                ClientRequest::AddProject { path } => self.add_project(path).await?,
                ClientRequest::RemoveProject { project } => self.remove_project(&project)?,
                ClientRequest::ReloadConfig => self.reload_config().await,
                ClientRequest::ListBranches { project } => {
                    let root = self.project_cfg(&project)?.root;
                    let r2 = root.clone();
                    let _ = tokio::task::spawn_blocking(move || git::fetch(&r2)).await;
                    let (local, remote) =
                        tokio::task::spawn_blocking(move || git::branches(&root)).await??;
                    let _ = reply.send(ServerEvent::Branches {
                        project,
                        local,
                        remote,
                    });
                }
                ClientRequest::CreateWorktree(c) => self.create_worktree(c, cols, rows).await?,
                ClientRequest::RemoveWorktree {
                    project,
                    path,
                    force,
                } => self.remove_worktree(&project, path, force).await?,
                ClientRequest::RunSetup {
                    project,
                    worktree,
                    steps,
                    env,
                } => self.run_setup(&project, &worktree, &steps, env, cols, rows)?,
                ClientRequest::SpawnSession(s) => {
                    let id = self.spawn_session(s)?;
                    let _ = reply.send(ServerEvent::Focus { session: id });
                }
                ClientRequest::ResumeSession {
                    session,
                    cols,
                    rows,
                } => self.resume_session(&session, cols, rows)?,
                ClientRequest::StartRun(r) => self.start_run(r).await?,
                ClientRequest::StopRun {
                    project,
                    worktree,
                    target,
                } => self.stop_run(&project, &worktree, &target),
                ClientRequest::KillSession { session } => self.kill_session(&session),
                ClientRequest::CloseSession { session } => self.close_session(&session),
                ClientRequest::Input { session, data } => self.input(&session, data),
                ClientRequest::Resize {
                    session,
                    cols,
                    rows,
                } => self.resize(&session, cols, rows),
                ClientRequest::OpenVscode { project, worktree } => {
                    let cfg = self.project_cfg(&project)?;
                    let wt = self.worktree(&project, &worktree)?;
                    crate::vscode::open(&cfg, &worktree, &wt.label())?;
                    self.toast(ToastLevel::Info, format!("VS Code: «{}»", wt.label()));
                }
                ClientRequest::Notify(n) => self.notify(n),
                ClientRequest::MarkSeen { session } => self.mark_seen(&session),
                ClientRequest::SaveUiState { state } => self.store.set_kv("ui_state", &state)?,
                ClientRequest::AskChoices {
                    project,
                    worktree,
                    target,
                    name,
                } => {
                    let choices = self
                        .ask_choices(&project, &worktree, &target, &name)
                        .await?;
                    let _ = reply.send(ServerEvent::AskChoices {
                        target,
                        name,
                        choices,
                    });
                }
                ClientRequest::Shutdown => {
                    self.shutdown_all();
                    self.shutdown.notify_waiters();
                }
                ClientRequest::GitStatus { worktree } => {
                    self.send_git_status(&worktree, reply).await?
                }
                ClientRequest::GitStage { worktree, paths } => {
                    self.git_op(&worktree, move |w| crate::scm::stage(w, &paths))
                        .await?;
                    self.after_git_change(&worktree, reply).await?;
                }
                ClientRequest::GitUnstage { worktree, paths } => {
                    self.git_op(&worktree, move |w| crate::scm::unstage(w, &paths))
                        .await?;
                    self.after_git_change(&worktree, reply).await?;
                }
                ClientRequest::GitDiscard { worktree, paths } => {
                    self.git_op(&worktree, move |w| crate::scm::discard(w, &paths))
                        .await?;
                    self.after_git_change(&worktree, reply).await?;
                }
                ClientRequest::GitDiff {
                    worktree,
                    path,
                    staged,
                } => {
                    let p = path.clone();
                    let text = self
                        .git_op(&worktree, move |w| crate::scm::diff(w, &p, staged))
                        .await?;
                    let _ = reply.send(ServerEvent::GitDiff {
                        worktree,
                        path,
                        staged,
                        text,
                    });
                }
                ClientRequest::GitCommit { worktree, message } => {
                    self.git_commit(&worktree, &message, reply).await?
                }
                ClientRequest::Attach { .. } | ClientRequest::Detach { .. } => {}
            }
            Ok(())
        }
        .await;
        if let Err(e) = res {
            let _ = reply.send(ServerEvent::Toast {
                level: ToastLevel::Error,
                message: format!("{e:#}"),
            });
        }
    }

    pub fn shutdown_all(&self) {
        let ptys: Vec<Arc<PtySession>> = {
            let st = self.state.lock().unwrap();
            st.sessions.iter().filter_map(|s| s.pty.clone()).collect()
        };
        for p in ptys {
            if p.is_alive() {
                p.kill();
            }
        }
    }
}

pub fn project_info(p: &Project) -> ProjectInfo {
    let c = &p.cfg;
    ProjectInfo {
        id: p.id.clone(),
        name: c.name.clone(),
        root: c.root.clone(),
        worktree_root: c.worktree_root.clone(),
        base_branch: p.base_branch.clone(),
        config_source: c.source.clone(),
        config_error: c.error.clone(),
        worktrees: p.worktrees.clone(),
        packages: c
            .packages
            .iter()
            .map(|k| PackageInfo {
                name: k.name.clone(),
                path: k.path.clone(),
            })
            .collect(),
        runs: c
            .runs
            .iter()
            .map(|r| RunInfo {
                name: r.name.clone(),
                procs: r
                    .procs
                    .iter()
                    .map(|p| RunProcInfo {
                        name: p.name.clone(),
                        base_port: p.port,
                    })
                    .collect(),
                asks: r
                    .asks
                    .iter()
                    .map(|a| AskInfo {
                        name: a.name.clone(),
                        prompt: a.prompt.clone().unwrap_or_else(|| a.name.clone()),
                        default: a.default.clone(),
                        has_choices: !a.choices.is_empty() || a.choices_cmd.is_some(),
                    })
                    .collect(),
            })
            .collect(),
        steps: c
            .steps
            .iter()
            .map(|s| StepInfo {
                id: s.id.clone(),
                label: s.label.clone().unwrap_or_else(|| s.cmd.clone()),
            })
            .collect(),
        profiles: c
            .profiles
            .iter()
            .map(|p| ProfileInfo {
                name: p.name.clone(),
                patterns: p.patterns.clone(),
                env: p.env.clone(),
                steps: p.steps.clone(),
            })
            .collect(),
        env_choices: p.env_choices.clone(),
        port_stride: c.port_stride,
    }
}

fn stored_to_info(s: &StoredSession) -> SessionInfo {
    SessionInfo {
        id: s.id.clone(),
        project: s.project.clone(),
        worktree: s.worktree.clone(),
        kind: s.kind.clone(),
        title: s.title.clone(),
        cwd: s.cwd.clone(),
        status: Status::Exited,
        alive: false,
        exit_code: None,
        ports: vec![],
        agent_session: s.agent_session.clone(),
        created_at: s.created_at,
        last_prompt: s.last_prompt.clone(),
    }
}

fn info_to_stored(i: &SessionInfo) -> StoredSession {
    StoredSession {
        id: i.id.clone(),
        project: i.project.clone(),
        worktree: i.worktree.clone(),
        kind: i.kind.clone(),
        title: i.title.clone(),
        cwd: i.cwd.clone(),
        agent_session: i.agent_session.clone(),
        last_prompt: i.last_prompt.clone(),
        created_at: i.created_at,
    }
}
