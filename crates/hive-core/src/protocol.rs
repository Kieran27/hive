//! Wire protocol between the TUI (or `hive notify`) and the daemon. Frames are
//! MessagePack payloads behind a u32 length prefix (see `codec`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientRequest {
    Hello {
        version: u32,
    },
    /// Ask for a full `Snapshot` and start receiving entity deltas.
    Subscribe,
    AddProject {
        path: PathBuf,
    },
    /// Make a new folder, `git init` it, add it as a project and optionally
    /// open an agent in it.
    CreateProject(CreateProject),
    RemoveProject {
        project: String,
    },
    ReloadConfig,
    ListBranches {
        project: String,
    },
    CreateWorktree(CreateWorktree),
    RemoveWorktree {
        project: String,
        path: PathBuf,
        force: bool,
    },
    /// Re-run setup steps in an existing worktree.
    RunSetup {
        project: String,
        worktree: PathBuf,
        steps: Vec<String>,
        env: Option<String>,
    },
    SpawnSession(SpawnSession),
    /// Respawn a dead agent session with its resume flags.
    ResumeSession {
        session: String,
        cols: u16,
        rows: u16,
    },
    StartRun(StartRun),
    StopRun {
        project: String,
        worktree: PathBuf,
        target: String,
    },
    KillSession {
        session: String,
    },
    /// Kill (if alive) and forget a session.
    CloseSession {
        session: String,
    },
    Attach {
        session: String,
        from_seq: u64,
        cols: u16,
        rows: u16,
    },
    Detach {
        session: String,
    },
    Input {
        session: String,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    Resize {
        session: String,
        cols: u16,
        rows: u16,
    },
    OpenVscode {
        project: String,
        worktree: PathBuf,
    },
    /// Sent by `hive notify` from inside agent hooks.
    Notify(Notify),
    MarkSeen {
        session: String,
    },
    SaveUiState {
        state: String,
    },
    /// Resolve the choices for a run target's `ask` prompt.
    AskChoices {
        project: String,
        worktree: PathBuf,
        target: String,
        name: String,
    },
    Shutdown,
    /// Source control for one worktree.
    GitStatus {
        worktree: PathBuf,
    },
    GitStage {
        worktree: PathBuf,
        paths: Vec<String>,
    },
    GitUnstage {
        worktree: PathBuf,
        paths: Vec<String>,
    },
    /// Throw away unstaged changes (and delete untracked files).
    GitDiscard {
        worktree: PathBuf,
        paths: Vec<String>,
    },
    GitDiff {
        worktree: PathBuf,
        path: String,
        staged: bool,
    },
    /// Commit what is staged, running hooks through the project's shell/node.
    GitCommit {
        worktree: PathBuf,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProject {
    /// Folder the project is created in.
    pub parent: PathBuf,
    pub name: String,
    /// Session to open once it exists (`None` = just add it).
    pub open: Option<SessionKind>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateWorktree {
    pub project: String,
    pub mode: WorktreeMode,
    /// Branch to check out / create, or the ref for a detached worktree.
    pub branch: String,
    /// Base ref for `WorktreeMode::NewBranch`.
    pub base: Option<String>,
    /// Setup step ids to run after creation.
    pub steps: Vec<String>,
    pub env: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorktreeMode {
    NewBranch,
    Local,
    Remote,
    Detached,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnSession {
    pub project: String,
    pub worktree: PathBuf,
    pub kind: SessionKind,
    /// Working directory; defaults to the worktree root.
    pub cwd: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartRun {
    pub project: String,
    pub worktree: PathBuf,
    pub target: String,
    /// Answers to the target's `ask` prompts.
    pub answers: BTreeMap<String, String>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notify {
    pub session: String,
    pub agent: String,
    pub event: String,
    /// Raw JSON the agent handed its hook (stdin for claude, argv for codex).
    pub payload: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionKind {
    Shell,
    Claude,
    Codex,
    Run { target: String, proc_name: String },
    Setup,
}

impl SessionKind {
    pub fn is_agent(&self) -> bool {
        matches!(self, SessionKind::Claude | SessionKind::Codex)
    }

    pub fn label(&self) -> String {
        match self {
            SessionKind::Shell => "shell".into(),
            SessionKind::Claude => "claude".into(),
            SessionKind::Codex => "codex".into(),
            SessionKind::Run { target, proc_name } if target == proc_name => {
                format!("run:{target}")
            }
            SessionKind::Run { target, proc_name } => format!("run:{target}·{proc_name}"),
            SessionKind::Setup => "setup".into(),
        }
    }
}

/// Agent / process status, ordered by how much it wants attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum Status {
    #[default]
    Idle,
    Exited,
    Working,
    Done,
    Waiting,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerEvent {
    Hello {
        version: u32,
        pid: u32,
    },
    Snapshot {
        projects: Vec<ProjectInfo>,
        sessions: Vec<SessionInfo>,
        ui_state: Option<String>,
    },
    Projects(Vec<ProjectInfo>),
    SessionUpsert(SessionInfo),
    SessionRemoved {
        session: String,
    },
    /// Replay: if `base_seq` matches the client's next seq, append; otherwise
    /// reset the client's emulator and feed `data`.
    Scrollback {
        session: String,
        base_seq: u64,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        kitty_flags: u8,
    },
    Output {
        session: String,
        seq: u64,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    KittyFlags {
        session: String,
        flags: u8,
    },
    Branches {
        project: String,
        local: Vec<String>,
        remote: Vec<String>,
    },
    AskChoices {
        target: String,
        name: String,
        choices: Vec<String>,
    },
    /// Ask the TUI to focus this session (e.g. a freshly spawned one).
    Focus {
        session: String,
    },
    Toast {
        level: ToastLevel,
        message: String,
    },
    Ok,
    GitStatus(GitStatusInfo),
    GitDiff {
        worktree: PathBuf,
        path: String,
        staged: bool,
        text: String,
    },
    /// A commit finished; `output` is the git/hook output (shown on failure).
    GitCommitted {
        worktree: PathBuf,
        ok: bool,
        output: String,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GitStatusInfo {
    pub worktree: PathBuf,
    /// Current branch (None when detached).
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<GitFile>,
}

/// One path from `git status`: `staged` / `unstaged` hold the porcelain
/// status letter for each side (`M`, `A`, `D`, `R`, `?` for untracked…).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitFile {
    pub path: String,
    pub orig_path: Option<String>,
    pub staged: Option<char>,
    pub unstaged: Option<char>,
}

impl GitFile {
    pub fn untracked(&self) -> bool {
        self.unstaged == Some('?')
    }

    pub fn conflicted(&self) -> bool {
        matches!(
            (self.staged, self.unstaged),
            (Some('U'), _) | (_, Some('U')) | (Some('A'), Some('A')) | (Some('D'), Some('D'))
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToastLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    pub worktree_root: PathBuf,
    pub base_branch: String,
    pub config_source: Option<PathBuf>,
    pub config_error: Option<String>,
    pub worktrees: Vec<WorktreeInfo>,
    pub packages: Vec<PackageInfo>,
    pub runs: Vec<RunInfo>,
    pub steps: Vec<StepInfo>,
    pub profiles: Vec<ProfileInfo>,
    pub env_choices: Vec<String>,
    pub port_stride: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub is_main: bool,
    pub slot: u16,
    pub dirty: bool,
    pub prunable: bool,
}

impl WorktreeInfo {
    pub fn label(&self) -> String {
        match (&self.branch, &self.head) {
            (Some(b), _) => b.clone(),
            (None, Some(h)) => format!("detached@{}", &h[..h.len().min(8)]),
            _ => "detached".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageInfo {
    pub name: String,
    /// Relative to the worktree root.
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunInfo {
    pub name: String,
    pub procs: Vec<RunProcInfo>,
    pub asks: Vec<AskInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunProcInfo {
    pub name: String,
    pub base_port: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskInfo {
    pub name: String,
    pub prompt: String,
    pub default: Option<String>,
    pub has_choices: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepInfo {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileInfo {
    pub name: String,
    pub patterns: Vec<String>,
    pub env: Option<String>,
    pub steps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub project: String,
    pub worktree: PathBuf,
    pub kind: SessionKind,
    pub title: String,
    pub cwd: PathBuf,
    pub status: Status,
    pub alive: bool,
    pub exit_code: Option<i32>,
    pub ports: Vec<u16>,
    /// Agent-side session id (claude session_id / codex thread-id).
    pub agent_session: Option<String>,
    pub created_at: i64,
    pub last_prompt: Option<String>,
}

impl SessionInfo {
    pub fn resumable(&self) -> bool {
        !self.alive && self.kind.is_agent() && self.agent_session.is_some()
    }

    /// Run sessions of one target share a tab.
    pub fn group(&self) -> Option<&str> {
        match &self.kind {
            SessionKind::Run { target, .. } => Some(target),
            _ => None,
        }
    }
}
