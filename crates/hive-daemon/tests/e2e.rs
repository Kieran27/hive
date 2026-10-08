//! End-to-end: a real daemon on a temp socket, a temp git repo, and a client
//! speaking the wire protocol.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hive_core::codec::{read_frame, write_frame};
use hive_core::protocol::*;
use tokio::net::UnixStream;

struct Client {
    stream: UnixStream,
}

impl Client {
    async fn connect() -> Self {
        let sock = hive_core::paths::socket_path();
        for _ in 0..100 {
            if let Ok(stream) = UnixStream::connect(&sock).await {
                return Self { stream };
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("no daemon");
    }

    async fn send(&mut self, r: ClientRequest) {
        write_frame(&mut self.stream, &r).await.unwrap();
    }

    async fn next(&mut self) -> ServerEvent {
        tokio::time::timeout(Duration::from_secs(20), read_frame(&mut self.stream))
            .await
            .expect("timeout")
            .unwrap()
            .expect("eof")
    }

    /// Read events until `f` returns Some.
    async fn until<T>(&mut self, mut f: impl FnMut(&ServerEvent) -> Option<T>) -> T {
        loop {
            let ev = self.next().await;
            if let ServerEvent::Toast {
                level: ToastLevel::Error,
                message,
            } = &ev
            {
                eprintln!("toast error: {message}");
            }
            if let Some(t) = f(&ev) {
                return t;
            }
        }
    }
}

fn sh(dir: &Path, cmd: &str) {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{cmd}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_flow() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    std::env::set_var("HIVE_HOME", &home);
    std::env::set_var("SHELL", "/bin/sh");

    // A repo with a run target that serves a port and a setup step.
    let repo = home.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    let port = free_port();
    std::fs::write(
        repo.join(".hive.toml"),
        format!(
            r#"
[project]
name = "app"
copy_from_main = ["secret.env"]

[[setup_step]]
id = "hello"
cmd = "echo setup-ran-{{env}} > setup.txt"

[[profile]]
name = "default"
match = ["*"]
env = "staging"
steps = ["hello"]

[[run]]
name = "web"
[[run.proc]]
name = "api"
port = {port}
cmd = "python3 -m http.server {{port}} --bind 127.0.0.1"
[[run.proc]]
name = "client"
wait_for_port = "api"
cmd = "echo api-is-up-on-{{port.api}}; sleep 30"
"#
        ),
    )
    .unwrap();
    sh(&repo, "git init -q -b main && git add . && git -c user.email=a@b -c user.name=a commit -q -m init && echo s3cret > secret.env");

    let sock = hive_core::paths::socket_path();
    let listener = hive_daemon::server::bind(&sock).unwrap();
    let store = hive_daemon::store::Store::open(&hive_core::paths::db_file()).unwrap();
    let daemon = hive_daemon::daemon::Daemon::new(store, PathBuf::from("/bin/echo"));
    daemon.bootstrap().await.unwrap();
    tokio::spawn(hive_daemon::server::serve(daemon.clone(), listener));

    let mut c = Client::connect().await;
    c.send(ClientRequest::Subscribe).await;
    match c.next().await {
        ServerEvent::Snapshot { projects, .. } => assert!(projects.is_empty()),
        e => panic!("{e:?}"),
    }

    // Add project.
    c.send(ClientRequest::AddProject { path: repo.clone() })
        .await;
    let project = c
        .until(|e| match e {
            ServerEvent::Projects(p) if !p.is_empty() && !p[0].worktrees.is_empty() => {
                Some(p[0].clone())
            }
            _ => None,
        })
        .await;
    assert_eq!(project.name, "app");
    assert_eq!(project.runs[0].procs.len(), 2);
    let main_wt = project.worktrees[0].path.clone();

    // Shell session: input → output.
    c.send(ClientRequest::SpawnSession(SpawnSession {
        project: project.id.clone(),
        worktree: main_wt.clone(),
        kind: SessionKind::Shell,
        cwd: None,
        cols: 80,
        rows: 24,
    }))
    .await;
    let shell = c
        .until(|e| {
            if let ServerEvent::Focus { session } = e {
                Some(session.clone())
            } else {
                None
            }
        })
        .await;
    c.send(ClientRequest::Attach {
        session: shell.clone(),
        from_seq: 0,
        cols: 80,
        rows: 24,
    })
    .await;
    c.send(ClientRequest::Input {
        session: shell.clone(),
        data: b"echo hi-$((40+2))\r".to_vec(),
    })
    .await;
    let mut seen = Vec::new();
    c.until(|e| {
        match e {
            ServerEvent::Output { session, data, .. }
            | ServerEvent::Scrollback { session, data, .. }
                if *session == shell =>
            {
                seen.extend_from_slice(data)
            }
            _ => {}
        }
        String::from_utf8_lossy(&seen)
            .contains("hi-42")
            .then_some(())
    })
    .await;

    // Notify flow drives status and captures the agent session id.
    c.send(ClientRequest::Notify(Notify {
        session: shell.clone(),
        agent: "claude".into(),
        event: "UserPromptSubmit".into(),
        payload: r#"{"session_id":"abc","prompt":"refactor the login page"}"#.into(),
    }))
    .await;
    c.until(|e| match e {
        ServerEvent::SessionUpsert(s) if s.id == shell && s.status == Status::Working => {
            assert_eq!(s.agent_session.as_deref(), Some("abc"));
            assert_eq!(s.title, "refactor the login page");
            Some(())
        }
        _ => None,
    })
    .await;

    // Source control: stage one of two changes, commit only that one.
    std::fs::write(repo.join("feature.txt"), "wanted\n").unwrap();
    std::fs::write(
        repo.join(".hive.toml"),
        std::fs::read_to_string(repo.join(".hive.toml")).unwrap() + "\n# local tweak\n",
    )
    .unwrap();
    c.send(ClientRequest::GitStatus {
        worktree: main_wt.clone(),
    })
    .await;
    let st = c
        .until(|e| match e {
            ServerEvent::GitStatus(s) => Some(s.clone()),
            _ => None,
        })
        .await;
    assert_eq!(st.branch.as_deref(), Some("main"));
    let paths: Vec<&str> = st.files.iter().map(|f| f.path.as_str()).collect();
    assert!(
        paths.contains(&"feature.txt") && paths.contains(&".hive.toml"),
        "{paths:?}"
    );
    c.send(ClientRequest::GitStage {
        worktree: main_wt.clone(),
        paths: vec!["feature.txt".into()],
    })
    .await;
    c.until(|e| match e {
        ServerEvent::GitStatus(s)
            if s.files
                .iter()
                .any(|f| f.path == "feature.txt" && f.staged == Some('A')) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    c.send(ClientRequest::GitCommit {
        worktree: main_wt.clone(),
        message: "add feature".into(),
    })
    .await;
    let (ok, output) = c
        .until(|e| match e {
            ServerEvent::GitCommitted { ok, output, .. } => Some((*ok, output.clone())),
            _ => None,
        })
        .await;
    assert!(ok, "{output}");
    let st = c
        .until(|e| match e {
            ServerEvent::GitStatus(s) => Some(s.clone()),
            _ => None,
        })
        .await;
    assert!(
        !st.files.iter().any(|f| f.path == "feature.txt"),
        "committed"
    );
    assert!(
        st.files
            .iter()
            .any(|f| f.path == ".hive.toml" && f.unstaged == Some('M') && f.staged.is_none()),
        "local tweak untouched"
    );
    let last = std::process::Command::new("git")
        .args([
            "-C",
            repo.to_str().unwrap(),
            "show",
            "--stat",
            "--format=%s",
            "HEAD",
        ])
        .output()
        .unwrap();
    let last = String::from_utf8_lossy(&last.stdout).to_string();
    assert!(
        last.starts_with("add feature")
            && last.contains("feature.txt")
            && !last.contains(".hive.toml"),
        "{last}"
    );

    // A failing pre-commit hook is reported with its output.
    std::fs::write(
        repo.join(".git/hooks/pre-commit"),
        "#!/bin/sh\necho 'lint: 2 problems' >&2\nexit 1\n",
    )
    .unwrap();
    sh(&repo, "chmod +x .git/hooks/pre-commit");
    c.send(ClientRequest::GitStage {
        worktree: main_wt.clone(),
        paths: vec![".hive.toml".into()],
    })
    .await;
    c.send(ClientRequest::GitCommit {
        worktree: main_wt.clone(),
        message: "tweak".into(),
    })
    .await;
    let (ok, output) = c
        .until(|e| match e {
            ServerEvent::GitCommitted { ok, output, .. } => Some((*ok, output.clone())),
            _ => None,
        })
        .await;
    assert!(!ok && output.contains("lint: 2 problems"), "{output}");
    std::fs::remove_file(repo.join(".git/hooks/pre-commit")).unwrap();
    // Undo the tweak so the rest of the flow starts clean.
    c.send(ClientRequest::GitUnstage {
        worktree: main_wt.clone(),
        paths: vec![".hive.toml".into()],
    })
    .await;
    c.send(ClientRequest::GitDiscard {
        worktree: main_wt.clone(),
        paths: vec![".hive.toml".into()],
    })
    .await;
    c.until(|e| match e {
        ServerEvent::GitStatus(s) if !s.files.iter().any(|f| f.path == ".hive.toml") => Some(()),
        _ => None,
    })
    .await;

    // New worktree with setup + copied file.
    c.send(ClientRequest::CreateWorktree(CreateWorktree {
        project: project.id.clone(),
        mode: WorktreeMode::NewBranch,
        branch: "feature/x".into(),
        base: None,
        steps: vec!["hello".into()],
        env: Some("staging".into()),
    }))
    .await;
    let setup = c
        .until(|e| match e {
            ServerEvent::SessionUpsert(s) if s.kind == SessionKind::Setup && !s.alive => {
                Some(s.clone())
            }
            _ => None,
        })
        .await;
    assert_eq!(setup.exit_code, Some(0));
    let wt = setup.worktree.clone();
    assert_eq!(
        std::fs::read_to_string(wt.join("setup.txt"))
            .unwrap()
            .trim(),
        "setup-ran-staging"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("secret.env"))
            .unwrap()
            .trim(),
        "s3cret"
    );

    // Run in the worktree: slot 1 → port + 10, client waits for api.
    c.send(ClientRequest::StartRun(StartRun {
        project: project.id.clone(),
        worktree: wt.clone(),
        target: "web".into(),
        answers: Default::default(),
        cols: 100,
        rows: 30,
    }))
    .await;
    let client_id = c
        .until(|e| match e {
            ServerEvent::SessionUpsert(s)
                if s.kind
                    == (SessionKind::Run {
                        target: "web".into(),
                        proc_name: "client".into(),
                    }) =>
            {
                Some(s.id.clone())
            }
            _ => None,
        })
        .await;
    c.send(ClientRequest::Attach {
        session: client_id.clone(),
        from_seq: 0,
        cols: 100,
        rows: 30,
    })
    .await;
    let want = format!("api-is-up-on-{}", port + 10);
    let mut seen = Vec::new();
    c.until(|e| {
        match e {
            ServerEvent::Output { session, data, .. }
            | ServerEvent::Scrollback { session, data, .. }
                if *session == client_id =>
            {
                seen.extend_from_slice(data)
            }
            _ => {}
        }
        String::from_utf8_lossy(&seen).contains(&want).then_some(())
    })
    .await;
    assert!(hive_daemon::ports::in_use(port + 10));

    // Starting the same target in the same worktree restarts it; a second
    // copy elsewhere with the port taken fails loudly.
    let blocker = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    c.send(ClientRequest::StartRun(StartRun {
        project: project.id.clone(),
        worktree: main_wt.clone(),
        target: "web".into(),
        answers: Default::default(),
        cols: 80,
        rows: 24,
    }))
    .await;
    let msg = c
        .until(|e| match e {
            ServerEvent::Toast {
                level: ToastLevel::Error,
                message,
            } => Some(message.clone()),
            _ => None,
        })
        .await;
    assert!(msg.contains(&format!(":{port}")), "{msg}");
    drop(blocker);

    // Stop the run; procs exit.
    c.send(ClientRequest::StopRun {
        project: project.id.clone(),
        worktree: wt.clone(),
        target: "web".into(),
    })
    .await;
    c.until(|e| match e {
        ServerEvent::SessionUpsert(s) if s.id == client_id && !s.alive => Some(()),
        _ => None,
    })
    .await;

    // Remove the worktree (dirty because of setup.txt → force).
    c.send(ClientRequest::RemoveWorktree {
        project: project.id.clone(),
        path: wt.clone(),
        force: true,
    })
    .await;
    c.until(|e| match e {
        ServerEvent::Projects(p) if p[0].worktrees.len() == 1 => Some(()),
        _ => None,
    })
    .await;
    assert!(!wt.exists());

    // Create a brand-new project from scratch and open a shell in it.
    let new_parent = home.join("repos");
    c.send(ClientRequest::CreateProject(CreateProject {
        parent: new_parent.clone(),
        name: "fresh-idea".into(),
        open: Some(SessionKind::Shell),
        cols: 80,
        rows: 24,
    }))
    .await;
    let focused = c
        .until(|e| {
            if let ServerEvent::Focus { session } = e {
                Some(session.clone())
            } else {
                None
            }
        })
        .await;
    assert!(new_parent.join("fresh-idea/.git").is_dir());
    c.send(ClientRequest::Subscribe).await;
    let (projects, sessions) = c
        .until(|e| match e {
            ServerEvent::Snapshot {
                projects, sessions, ..
            } => Some((projects.clone(), sessions.clone())),
            _ => None,
        })
        .await;
    let fresh = projects
        .iter()
        .find(|p| p.name == "fresh-idea")
        .expect("project added");
    assert_eq!(fresh.worktrees.len(), 1);
    let s = sessions.iter().find(|s| s.id == focused).unwrap();
    assert_eq!(s.project, fresh.id);
    assert_eq!(s.kind, SessionKind::Shell);
    // A second create with the same name is refused (folder not empty).
    c.send(ClientRequest::CreateProject(CreateProject {
        parent: new_parent.clone(),
        name: "fresh-idea".into(),
        open: None,
        cols: 80,
        rows: 24,
    }))
    .await;
    let msg = c
        .until(|e| match e {
            ServerEvent::Toast {
                level: ToastLevel::Error,
                message,
            } => Some(message.clone()),
            _ => None,
        })
        .await;
    assert!(msg.contains("already exists"), "{msg}");

    daemon.shutdown_all();
}

/// `cargo test -p hive-daemon --release --test e2e -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn client_throughput() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("HIVE_HOME", tmp.path());
    std::env::set_var("SHELL", "/bin/sh");
    let repo = tmp.path().join("r");
    std::fs::create_dir_all(&repo).unwrap();
    sh(
        &repo,
        "git init -q -b main && git -c user.email=a@b -c user.name=a commit -q --allow-empty -m i",
    );
    let sock = hive_core::paths::socket_path();
    let listener = hive_daemon::server::bind(&sock).unwrap();
    let store = hive_daemon::store::Store::open(&hive_core::paths::db_file()).unwrap();
    let daemon = hive_daemon::daemon::Daemon::new(store, PathBuf::from("/bin/echo"));
    daemon.bootstrap().await.unwrap();
    tokio::spawn(hive_daemon::server::serve(daemon.clone(), listener));
    let mut c = Client::connect().await;
    c.send(ClientRequest::Subscribe).await;
    c.send(ClientRequest::AddProject { path: repo.clone() })
        .await;
    let project = c
        .until(|e| match e {
            ServerEvent::Projects(p) if !p.is_empty() && !p[0].worktrees.is_empty() => {
                Some(p[0].clone())
            }
            _ => None,
        })
        .await;
    c.send(ClientRequest::SpawnSession(SpawnSession {
        project: project.id.clone(),
        worktree: project.worktrees[0].path.clone(),
        kind: SessionKind::Shell,
        cwd: None,
        cols: 120,
        rows: 30,
    }))
    .await;
    let id = c
        .until(|e| {
            if let ServerEvent::Focus { session } = e {
                Some(session.clone())
            } else {
                None
            }
        })
        .await;
    c.send(ClientRequest::Attach {
        session: id.clone(),
        from_seq: 0,
        cols: 120,
        rows: 30,
    })
    .await;
    c.send(ClientRequest::Input {
        session: id.clone(),
        data: b"yes | head -c 20000000; echo DONE-$((40+2))\r".to_vec(),
    })
    .await;
    let t = std::time::Instant::now();
    let mut parser = vt100::Parser::new(30, 120, 10_000);
    let (mut events, mut bytes, mut replays) = (0usize, 0usize, 0usize);
    let mut tail = Vec::new();
    loop {
        let ev = c.next().await;
        let data = match ev {
            ServerEvent::Output { data, .. } => data,
            ServerEvent::Scrollback { data, .. } => {
                replays += 1;
                data
            }
            _ => continue,
        };
        events += 1;
        bytes += data.len();
        parser.process(&data);
        tail.extend_from_slice(&data);
        if tail.len() > 64 {
            tail.drain(..tail.len() - 64);
        }
        if String::from_utf8_lossy(&tail).contains("DONE-42") {
            break;
        }
    }
    eprintln!(
        "{bytes} bytes, {events} events, {replays} replays in {:?}",
        t.elapsed()
    );
    daemon.shutdown_all();
}
