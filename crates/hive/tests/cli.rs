//! The agent-facing CLI (`hive wt`, `setup`, `run`, `stop`, `ls --json`)
//! against a real daemon on a temp socket.

use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::Value;

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

/// A port with `port + 10` (slot 1) also free, below the ephemeral range so
/// nothing else grabs it mid-test.
fn free_port() -> u16 {
    let free = |p: u16| std::net::TcpListener::bind(("0.0.0.0", p)).is_ok();
    let seed = std::process::id() as u16 % 5000;
    (0..5000)
        .map(|i| 20000 + (seed + i * 7) % 20000)
        .find(|&p| free(p) && free(p + 10))
        .expect("no free port pair")
}

/// Run the hive binary off the async runtime (it talks to our daemon).
async fn hive(args: &[&str]) -> Output {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_hive"))
            .args(&args)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_flow() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    std::env::set_var("HIVE_HOME", &home);
    std::env::set_var("HIVE_SOCKET", home.join("d.sock"));
    std::env::set_var("SHELL", "/bin/sh");

    let repo = home.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    let port = free_port();
    std::fs::write(
        repo.join(".hive.toml"),
        format!(
            r#"
[project]
name = "app"
port_stride = 10

[[setup_step]]
id = "hello"
cmd = "echo setup-ran-{{env}} > setup.txt"

[[setup_step]]
id = "boom"
cmd = "echo something broke; exit 3"

[[run]]
name = "web"
[[run.proc]]
name = "api"
port = {port}
cmd = "python3 -m http.server {{port}} --bind 127.0.0.1"
[[run.proc]]
name = "client"
cmd = "echo client-{{mode}}; sleep 30"
[[run.ask]]
name = "mode"
"#
        ),
    )
    .unwrap();
    sh(&repo, "git init -q -b main && git add . && git -c user.email=a@b -c user.name=a commit -q -m init");

    let listener = hive_daemon::server::bind(&hive_core::paths::socket_path()).unwrap();
    let store = hive_daemon::store::Store::open(&hive_core::paths::db_file()).unwrap();
    let daemon = hive_daemon::daemon::Daemon::new(store, PathBuf::from("/bin/echo"));
    daemon.bootstrap().await.unwrap();
    tokio::spawn(hive_daemon::server::serve(daemon.clone(), listener));

    let out = hive(&["add", repo.to_str().unwrap()]).await;
    assert!(out.status.success(), "{out:?}");
    // `add` doesn't wait for the daemon; poll until the project shows up.
    for _ in 0..100 {
        let v = json(&hive(&["ls", "--json"]).await);
        if v["projects"][0]["worktrees"]
            .as_array()
            .is_some_and(|w| !w.is_empty())
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Usage errors exit 2.
    let out = hive(&["wt", "new", "app", "feature/x", "--setup", "nope", "--json"]).await;
    assert_eq!(code(&out), 2, "{out:?}");
    assert!(json(&out)["error"].as_str().unwrap().contains("nope"));

    // 1. Create with setup, wait, check slot and ports.
    let out = hive(&[
        "wt",
        "new",
        "app",
        "feature/x",
        "--setup",
        "hello",
        "--env",
        "prod",
        "--wait",
        "--json",
    ])
    .await;
    assert_eq!(code(&out), 0, "{out:?}");
    let v = json(&out);
    assert_eq!(v["setup"], "ok");
    assert_eq!(v["slot"], 1);
    assert_eq!(v["branch"], "feature/x");
    assert_eq!(v["ports"]["api"], port + 10);
    let wt = PathBuf::from(v["path"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(wt.join("setup.txt"))
            .unwrap()
            .trim(),
        "setup-ran-prod"
    );
    let wt_s = wt.to_str().unwrap();

    // A failing setup exits 1 with its output.
    let out = hive(&["setup", "app", wt_s, "--steps", "boom", "--wait", "--json"]).await;
    assert_eq!(code(&out), 1, "{out:?}");
    let v = json(&out);
    assert_eq!(v["setup"], "failed");
    assert_eq!(v["exit_code"], 3);
    assert!(
        v["output"].as_str().unwrap().contains("something broke"),
        "{v}"
    );

    // 2. Run: a missing ask is refused, then the port comes up.
    let out = hive(&["run", "app", wt_s, "web", "--json"]).await;
    assert_eq!(code(&out), 2, "{out:?}");
    assert!(json(&out)["error"].as_str().unwrap().contains("mode"));
    let out = hive(&[
        "run",
        "app",
        "feature/x",
        "web",
        "--ask",
        "mode=dev",
        "--wait-ports",
        "--json",
    ])
    .await;
    assert_eq!(code(&out), 0, "{out:?}");
    let v = json(&out);
    assert_eq!(v["target"], "web");
    assert_eq!(v["ports"]["api"], port + 10);
    assert_eq!(v["sessions"].as_array().unwrap().len(), 2);
    assert!(std::net::TcpStream::connect(("127.0.0.1", port + 10)).is_ok());

    let out = hive(&["ls", "--json"]).await;
    let v = json(&out);
    let w = &v["projects"][0]["worktrees"][1];
    assert_eq!(w["slot"], 1);
    assert!(w["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["kind"] == "run:web·api" && s["status"] != "exited"));

    // 3. Stop.
    let out = hive(&["stop", "app", wt_s, "web", "--json"]).await;
    assert_eq!(code(&out), 0, "{out:?}");
    for _ in 0..50 {
        if std::net::TcpStream::connect(("127.0.0.1", port + 10)).is_err() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(std::net::TcpStream::connect(("127.0.0.1", port + 10)).is_err());

    // 4. Remove; the slot is free for the next worktree.
    let out = hive(&["wt", "rm", "app", wt_s, "--force", "--json"]).await;
    assert_eq!(code(&out), 0, "{out:?}");
    assert!(!wt.exists());
    let out = hive(&["ls", "--json"]).await;
    assert_eq!(
        json(&out)["projects"][0]["worktrees"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let out = hive(&["wt", "new", "app", "feature/y", "--json"]).await;
    assert_eq!(code(&out), 0, "{out:?}");
    let v = json(&out);
    assert_eq!(v["slot"], 1, "slot reused");
    assert_eq!(v["setup"], "skipped");

    daemon.shutdown_all();
}
