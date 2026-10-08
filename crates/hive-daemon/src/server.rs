//! Unix-socket server: one task pair per connection (reader + writer), plus
//! a forwarder task per attached session.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use hive_core::codec::{read_frame, write_frame};
use hive_core::protocol::{ClientRequest, ServerEvent};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use crate::daemon::Daemon;
use crate::pty::PtyEvent;

pub fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            anyhow::bail!("a hive daemon is already listening on {}", path.display());
        }
        std::fs::remove_file(path)?;
    }
    UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))
}

pub async fn serve(daemon: Arc<Daemon>, listener: UnixListener) -> Result<()> {
    // Periodic worktree polling picks up worktrees made outside hive.
    let poller = daemon.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        let mut n = 0u64;
        loop {
            tick.tick().await;
            n += 1;
            poller.refresh_all(n.is_multiple_of(3)).await;
        }
    });
    loop {
        tokio::select! {
            accept = listener.accept() => {
                let (stream, _) = accept?;
                let d = daemon.clone();
                tokio::spawn(async move {
                    if let Err(e) = connection(d, stream).await {
                        tracing::debug!("connection ended: {e:#}");
                    }
                });
            }
            _ = daemon.shutdown.notified() => return Ok(()),
        }
    }
}

fn is_slow(req: &ClientRequest) -> bool {
    matches!(
        req,
        ClientRequest::CreateWorktree(_)
            | ClientRequest::RemoveWorktree { .. }
            | ClientRequest::ListBranches { .. }
            | ClientRequest::StartRun(_)
            | ClientRequest::AddProject { .. }
            | ClientRequest::ReloadConfig
            | ClientRequest::AskChoices { .. }
            | ClientRequest::OpenVscode { .. }
    )
}

/// Git requests run off the read loop but strictly in order, so "stage,
/// then commit" (or several quick stage presses) can't overtake each other.
fn is_git(req: &ClientRequest) -> bool {
    matches!(
        req,
        ClientRequest::GitStatus { .. }
            | ClientRequest::GitStage { .. }
            | ClientRequest::GitUnstage { .. }
            | ClientRequest::GitDiscard { .. }
            | ClientRequest::GitDiff { .. }
            | ClientRequest::GitCommit { .. }
    )
}

async fn connection(daemon: Arc<Daemon>, stream: UnixStream) -> Result<()> {
    let (mut rd, mut wr) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerEvent>();
    let writer = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if write_frame(&mut wr, &ev).await.is_err() {
                break;
            }
        }
    });

    let mut client_id = None;
    let mut attached: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut size = (120u16, 40u16);
    let (git_tx, mut git_rx) = mpsc::unbounded_channel::<ClientRequest>();
    let git_worker = {
        let d = daemon.clone();
        let t = tx.clone();
        tokio::spawn(async move {
            while let Some(req) = git_rx.recv().await {
                d.handle(req, &t, (0, 0)).await;
            }
        })
    };

    while let Some(req) = read_frame::<_, ClientRequest>(&mut rd).await? {
        match req {
            ClientRequest::Subscribe => {
                // Register before the snapshot so no delta is missed.
                if client_id.is_none() {
                    client_id = Some(daemon.add_client(tx.clone()));
                }
                daemon.handle(ClientRequest::Subscribe, &tx, size).await;
            }
            ClientRequest::Attach {
                session,
                from_seq,
                cols,
                rows,
            } => {
                size = (cols, rows);
                if let Some(h) = attached.remove(&session) {
                    h.abort();
                }
                if let Some(att) = daemon.attach(&session, from_seq, cols, rows) {
                    let _ = tx.send(ServerEvent::Scrollback {
                        session: session.clone(),
                        base_seq: att.base_seq,
                        data: att.data,
                        kitty_flags: att.kitty_flags,
                    });
                    let h =
                        tokio::spawn(forward(daemon.clone(), session.clone(), att.rx, tx.clone()));
                    attached.insert(session, h);
                }
            }
            ClientRequest::Detach { session } => {
                if let Some(h) = attached.remove(&session) {
                    h.abort();
                }
            }
            ClientRequest::Resize {
                ref session,
                cols,
                rows,
            } => {
                size = (cols, rows);
                let _ = session;
                daemon.handle(req, &tx, size).await;
            }
            req if is_git(&req) => {
                let _ = git_tx.send(req);
            }
            req if is_slow(&req) => {
                let d = daemon.clone();
                let t = tx.clone();
                tokio::spawn(async move { d.handle(req, &t, size).await });
            }
            req => daemon.handle(req, &tx, size).await,
        }
    }

    for (_, h) in attached {
        h.abort();
    }
    drop(git_tx);
    let _ = git_worker.await;
    if let Some(id) = client_id {
        daemon.remove_client(id);
    }
    drop(tx);
    let _ = writer.await;
    Ok(())
}

/// Relay one session's PTY events to a client. On lag, resend from where
/// the client is (the ring still has it, or a full replay).
async fn forward(
    daemon: Arc<Daemon>,
    session: String,
    mut rx: tokio::sync::broadcast::Receiver<PtyEvent>,
    tx: mpsc::UnboundedSender<ServerEvent>,
) {
    let mut next_seq: Option<u64> = None;
    loop {
        match rx.recv().await {
            Ok(PtyEvent::Output { seq, data }) => {
                next_seq = Some(seq + data.len() as u64);
                if tx
                    .send(ServerEvent::Output {
                        session: session.clone(),
                        seq,
                        data: data.to_vec(),
                    })
                    .is_err()
                {
                    return;
                }
            }
            Ok(PtyEvent::KittyFlags(flags)) => {
                let _ = tx.send(ServerEvent::KittyFlags {
                    session: session.clone(),
                    flags,
                });
            }
            Ok(PtyEvent::Exited(_)) => {}
            Err(RecvError::Lagged(_)) => {
                let from = next_seq.unwrap_or(0);
                let Some(att) = daemon
                    .resync(&session, from)
                    .map(|a| (a.base_seq, a.data, a.kitty_flags, a.rx))
                else {
                    return;
                };
                let (base_seq, data, kitty_flags, new_rx) = att;
                next_seq = Some(base_seq + data.len() as u64);
                rx = new_rx;
                let _ = tx.send(ServerEvent::Scrollback {
                    session: session.clone(),
                    base_seq,
                    data,
                    kitty_flags,
                });
            }
            Err(RecvError::Closed) => return,
        }
    }
}
