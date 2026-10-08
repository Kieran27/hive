//! The hive daemon: owns PTYs, git worktrees, run targets and agent status.

pub mod commands;
pub mod daemon;
pub mod git;
pub mod ports;
pub mod pty;
pub mod scm;
pub mod server;
pub mod status;
pub mod store;
pub mod vscode;

use std::path::PathBuf;

use anyhow::Result;

/// Run the daemon in the foreground until `Shutdown` or a signal.
pub async fn run(hive_bin: PathBuf) -> Result<()> {
    let sock = hive_core::paths::socket_path();
    let listener = server::bind(&sock)?;
    let our_inode = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&sock)?);
    let store = store::Store::open(&hive_core::paths::db_file())?;
    let daemon = daemon::Daemon::new(store, hive_bin);
    daemon.bootstrap().await?;
    tracing::info!("hive daemon listening on {}", sock.display());

    let d = daemon.clone();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let res = tokio::select! {
        r = server::serve(daemon.clone(), listener) => r,
        _ = term.recv() => Ok(()),
        _ = int.recv() => Ok(()),
    };
    d.shutdown_all();
    // Only remove the socket if a newer daemon hasn't replaced it already.
    if std::fs::metadata(&sock)
        .map(|m| std::os::unix::fs::MetadataExt::ino(&m) == our_inode)
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(&sock);
    }
    // Give kill_tree's grace thread a moment before the process exits.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    res
}
