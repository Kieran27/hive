//! A child process on a PTY, owned by the daemon.
//!
//! - one reader thread per PTY feeding a bounded channel (backpressure);
//! - a pump task that answers terminal queries, keeps a headless vt100 screen
//!   (for cursor reports and replay modes), appends to the ring and
//!   broadcasts — all under one lock, so `attach` sees a gap-free stream;
//! - one writer thread, so input order is preserved and async code never
//!   blocks on a full PTY.

pub mod kitty;
pub mod ring;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tokio::sync::{broadcast, mpsc};

use kitty::{KittyScanner, Reply};
use ring::{Ring, RING_CAPACITY};

/// Output arriving within this long of the previous chunk is "streaming",
/// and gets held briefly so a flood becomes fewer, larger frames.
const COALESCE_HOLD: Duration = Duration::from_millis(5);
const STREAM_GAP: Duration = Duration::from_millis(10);
const COALESCE_MAX: usize = 128 * 1024;
const KILL_GRACE: Duration = Duration::from_secs(2);

pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone)]
pub enum PtyEvent {
    Output { seq: u64, data: Arc<Vec<u8>> },
    KittyFlags(u8),
    Exited(Option<i32>),
}

struct Inner {
    ring: Ring,
    kitty: KittyScanner,
    screen: vt100::Parser,
    size: (u16, u16),
    exit: Option<Option<i32>>,
}

pub struct PtySession {
    pub pid: Option<u32>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    input: std::sync::mpsc::Sender<Vec<u8>>,
    inner: Mutex<Inner>,
    tx: broadcast::Sender<PtyEvent>,
    alive: AtomicBool,
}

/// What an attaching client receives first.
pub struct Attachment {
    pub base_seq: u64,
    pub data: Vec<u8>,
    pub kitty_flags: u8,
    pub rx: broadcast::Receiver<PtyEvent>,
}

impl PtySession {
    pub fn spawn(spec: SpawnSpec) -> Result<Arc<Self>> {
        let size = PtySize {
            rows: spec.rows.max(2),
            cols: spec.cols.max(10),
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system().openpty(size).context("openpty")?;
        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        cmd.cwd(&spec.cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env_remove("NO_COLOR");
        // A daemon started from inside an agent must not leak that agent's
        // session markers into new panes (claude disables transcripts then).
        for (k, _) in std::env::vars_os() {
            let k = k.to_string_lossy();
            if k.starts_with("CLAUDE_CODE_") || k == "CLAUDECODE" || k == "HIVE_SESSION_ID" {
                cmd.env_remove(k.as_ref());
            }
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawning {}", spec.program))?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let pid = child.process_id();

        let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new()
            .name("pty-writer".into())
            .spawn(move || writer_thread(writer, input_rx))?;

        let (tx, _) = broadcast::channel(512);
        let session = Arc::new(Self {
            pid,
            master: Mutex::new(pair.master),
            input: input_tx,
            inner: Mutex::new(Inner {
                ring: Ring::new(RING_CAPACITY),
                kitty: KittyScanner::default(),
                screen: vt100::Parser::new(size.rows, size.cols, 0),
                size: (size.cols, size.rows),
                exit: None,
            }),
            tx,
            alive: AtomicBool::new(true),
        });

        let (chunk_tx, chunk_rx) = mpsc::channel::<Vec<u8>>(64);
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<Option<i32>>();
        std::thread::Builder::new()
            .name("pty-reader".into())
            .stack_size(256 * 1024)
            .spawn(move || reader_thread(reader, chunk_tx))?;
        std::thread::Builder::new()
            .name("pty-wait".into())
            .spawn(move || wait_thread(child, exit_tx))?;
        tokio::spawn(pump(session.clone(), chunk_rx, exit_rx));
        Ok(session)
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub fn exit_code(&self) -> Option<Option<i32>> {
        self.inner.lock().unwrap().exit
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PtyEvent> {
        self.tx.subscribe()
    }

    pub fn write(&self, data: Vec<u8>) {
        let _ = self.input.send(data);
    }

    /// Subscribe and snapshot atomically: every byte after `base_seq + data.len()`
    /// arrives on `rx`.
    pub fn attach(&self, from_seq: u64) -> Attachment {
        let inner = self.inner.lock().unwrap();
        let rx = self.tx.subscribe();
        let (base_seq, mut data) = inner.ring.snapshot_from(from_seq);
        if base_seq == inner.ring.start() && inner.ring.wrapped() {
            // The ring lost the start of the stream; restore the modes the
            // child set back then (alt screen, bracketed paste, mouse…).
            let screen = inner.screen.screen();
            let mut prefix = Vec::new();
            if screen.alternate_screen() {
                prefix.extend_from_slice(b"\x1b[?1049h");
            }
            prefix.extend_from_slice(&screen.input_mode_formatted());
            prefix.append(&mut data);
            data = prefix;
        }
        Attachment {
            base_seq,
            data,
            kitty_flags: inner.kitty.flags(),
            rx,
        }
    }

    /// Resize; at the same size, "jiggle" (rows-1 then back) so the child
    /// gets SIGWINCH and repaints for a newly attached client.
    pub fn resize(&self, cols: u16, rows: u16, jiggle: bool) {
        let cols = cols.max(10);
        let rows = rows.max(2);
        let mut inner = self.inner.lock().unwrap();
        let master = self.master.lock().unwrap();
        let same = inner.size == (cols, rows);
        if same && !jiggle {
            return;
        }
        if same && jiggle && self.is_alive() {
            let _ = master.resize(PtySize {
                rows: rows - 1,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        let _ = master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        inner.screen.screen_mut().set_size(rows, cols);
        inner.size = (cols, rows);
    }

    /// SIGHUP the process group and descendants, SIGKILL whatever is left
    /// after a grace period.
    pub fn kill(&self) {
        if let Some(pid) = self.pid {
            kill_tree(pid as i32);
        }
    }

    fn process(&self, chunk: &[u8]) {
        let mut inner = self.inner.lock().unwrap();
        let actions = inner.kitty.feed(chunk);
        let mut fed = 0usize;
        let mut replies: Vec<u8> = Vec::new();
        for r in actions.replies {
            match r {
                Reply::Bytes(b) => replies.extend_from_slice(&b),
                Reply::CursorPosition { at } => {
                    inner.screen.process(&chunk[fed..at]);
                    fed = at;
                    let (row, col) = inner.screen.screen().cursor_position();
                    replies.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
                }
            }
        }
        inner.screen.process(&chunk[fed..]);
        let seq = inner.ring.push(chunk);
        let _ = self.tx.send(PtyEvent::Output {
            seq,
            data: Arc::new(chunk.to_vec()),
        });
        if let Some(f) = actions.flags_changed {
            let _ = self.tx.send(PtyEvent::KittyFlags(f));
        }
        drop(inner);
        if !replies.is_empty() {
            self.write(replies);
        }
    }
}

fn writer_thread(mut w: Box<dyn Write + Send>, rx: std::sync::mpsc::Receiver<Vec<u8>>) {
    while let Ok(buf) = rx.recv() {
        if w.write_all(&buf).and_then(|_| w.flush()).is_err() {
            break;
        }
    }
}

fn reader_thread(mut r: Box<dyn Read + Send>, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.blocking_send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

fn wait_thread(
    mut child: Box<dyn Child + Send + Sync>,
    tx: tokio::sync::oneshot::Sender<Option<i32>>,
) {
    let code = child.wait().ok().map(|s| s.exit_code() as i32);
    let _ = tx.send(code);
}

async fn pump(
    session: Arc<PtySession>,
    mut rx: mpsc::Receiver<Vec<u8>>,
    mut exit_rx: tokio::sync::oneshot::Receiver<Option<i32>>,
) {
    let mut last_chunk = Instant::now() - STREAM_GAP * 2;
    let mut exit: Option<Option<i32>> = None;
    let mut eof = false;
    loop {
        if eof && exit.is_some() {
            break;
        }
        let chunk = if exit.is_some() {
            // The child is gone; drain what is left, but don't wait forever on
            // a grandchild still holding the PTY open.
            match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
                Ok(Some(c)) => c,
                _ => break,
            }
        } else if eof {
            match (&mut exit_rx).await {
                Ok(code) => exit = Some(code),
                Err(_) => exit = Some(None),
            }
            continue;
        } else {
            tokio::select! {
                c = rx.recv() => match c {
                    Some(c) => c,
                    None => { eof = true; continue; }
                },
                code = &mut exit_rx => {
                    exit = Some(code.unwrap_or(None));
                    continue;
                }
            }
        };
        let mut chunk = chunk;
        let streaming = last_chunk.elapsed() < STREAM_GAP;
        if streaming {
            let deadline = tokio::time::Instant::now() + COALESCE_HOLD;
            while chunk.len() < COALESCE_MAX {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(more)) => chunk.extend_from_slice(&more),
                    Ok(None) => {
                        eof = true;
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        last_chunk = Instant::now();
        session.process(&chunk);
    }
    let code = exit.unwrap_or(None);
    session.alive.store(false, Ordering::Relaxed);
    session.inner.lock().unwrap().exit = Some(code);
    let _ = session.tx.send(PtyEvent::Exited(code));
}

/// All descendants of `root` (not including it), from `ps`.
fn descendants(root: i32) -> Vec<i32> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
    else {
        return vec![];
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let pairs: Vec<(i32, i32)> = text
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect();
    let mut found = vec![];
    let mut frontier = vec![root];
    while let Some(p) = frontier.pop() {
        for &(pid, ppid) in &pairs {
            if ppid == p && !found.contains(&pid) {
                found.push(pid);
                frontier.push(pid);
            }
        }
    }
    found
}

pub fn kill_tree(pid: i32) {
    let mut all = descendants(pid);
    all.push(pid);
    unsafe {
        libc::kill(-pid, libc::SIGHUP);
        for &p in &all {
            libc::kill(p, libc::SIGHUP);
        }
    }
    std::thread::spawn(move || {
        std::thread::sleep(KILL_GRACE);
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
            for &p in &all {
                if libc::kill(p, 0) == 0 {
                    libc::kill(p, libc::SIGKILL);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(cmd: &str) -> SpawnSpec {
        SpawnSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), cmd.into()],
            cwd: std::env::temp_dir(),
            env: vec![],
            cols: 80,
            rows: 24,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn output_and_exit() {
        let s = PtySession::spawn(spec("printf hello; exit 3")).unwrap();
        let mut rx = s.subscribe();
        let mut got = Vec::new();
        let code = loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
            {
                Ok(PtyEvent::Output { data, .. }) => got.extend_from_slice(&data),
                Ok(PtyEvent::Exited(c)) => break c,
                Ok(_) => {}
                Err(_) => {}
            }
        };
        let a = s.attach(0);
        assert!(String::from_utf8_lossy(&a.data).contains("hello"));
        assert_eq!(code, Some(3));
        assert!(!s.is_alive());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_cursor_query() {
        // `read` the CPR reply back from the terminal and echo it.
        let s = PtySession::spawn(spec("stty raw -echo; printf 'ab\\033[6n'; dd bs=1 count=6 2>/dev/null | od -c | head -1; exit 0")).unwrap();
        let mut rx = s.subscribe();
        loop {
            if let Ok(PtyEvent::Exited(_)) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
            {
                break;
            }
        }
        let a = s.attach(0);
        let text = String::from_utf8_lossy(&a.data);
        assert!(
            text.contains("[   1   ;   3   R") || text.contains("1   ;   3   R"),
            "{text:?}"
        );
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// `cargo test -p hive-daemon --release -- --ignored --nocapture pty_throughput`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn pty_throughput() {
        let t = Instant::now();
        let s = PtySession::spawn(SpawnSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "yes | head -c 20000000".into()],
            cwd: std::env::temp_dir(),
            env: vec![],
            cols: 120,
            rows: 30,
        })
        .unwrap();
        let mut rx = s.subscribe();
        let mut bytes = 0usize;
        loop {
            match rx.recv().await {
                Ok(PtyEvent::Output { data, .. }) => bytes += data.len(),
                Ok(PtyEvent::Exited(_)) => break,
                _ => {}
            }
        }
        eprintln!("{bytes} bytes in {:?}", t.elapsed());
    }
}
