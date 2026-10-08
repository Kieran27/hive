//! Connecting to (and if needed starting) the daemon.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use hive_core::codec::{read_frame, write_frame};
use hive_core::protocol::{ClientRequest, ServerEvent};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

pub type ReqTx = mpsc::UnboundedSender<ClientRequest>;

/// Spawn `hive daemon` detached (own session, output to the log file).
pub fn spawn_daemon(hive_bin: &Path) -> Result<()> {
    let log_dir = hive_core::paths::log_dir();
    std::fs::create_dir_all(&log_dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("daemon.log"))?;
    let mut cmd = std::process::Command::new(hive_bin);
    cmd.arg("daemon")
        .arg("run")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .current_dir(std::env::var_os("HOME").unwrap_or_else(|| "/".into()));
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("starting hive daemon")?;
    Ok(())
}

pub async fn connect_or_spawn(hive_bin: &Path) -> Result<UnixStream> {
    let sock = hive_core::paths::socket_path();
    if let Ok(s) = UnixStream::connect(&sock).await {
        return Ok(s);
    }
    spawn_daemon(hive_bin)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(s) = UnixStream::connect(&sock).await {
            return Ok(s);
        }
        if Instant::now() > deadline {
            bail!(
                "daemon did not come up; see {}",
                hive_core::paths::log_dir().join("daemon.log").display()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Split a connection into a request sender and an event receiver.
pub fn split(stream: UnixStream) -> (ReqTx, mpsc::UnboundedReceiver<ServerEvent>) {
    let (mut rd, mut wr) = stream.into_split();
    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<ClientRequest>();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel::<ServerEvent>();
    tokio::spawn(async move {
        while let Some(r) = req_rx.recv().await {
            if write_frame(&mut wr, &r).await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        while let Ok(Some(ev)) = read_frame::<_, ServerEvent>(&mut rd).await {
            if ev_tx.send(ev).is_err() {
                break;
            }
        }
    });
    (req_tx, ev_rx)
}

/// Fire-and-forget one request (used by `hive notify` and CLI commands).
pub async fn send_one(req: ClientRequest) -> Result<()> {
    let mut s = UnixStream::connect(hive_core::paths::socket_path())
        .await
        .context("hive daemon is not running")?;
    write_frame(&mut s, &req).await?;
    Ok(())
}

/// Send one request and wait for the first event (e.g. `Hello`).
pub async fn request(req: ClientRequest) -> Result<Option<ServerEvent>> {
    let mut s = UnixStream::connect(hive_core::paths::socket_path())
        .await
        .context("hive daemon is not running")?;
    write_frame(&mut s, &req).await?;
    Ok(
        tokio::time::timeout(Duration::from_secs(5), read_frame(&mut s))
            .await
            .ok()
            .and_then(|r| r.ok())
            .flatten(),
    )
}
