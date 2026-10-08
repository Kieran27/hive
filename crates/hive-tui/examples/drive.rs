//! Headless driver for manual/scripted TUI checks: runs a command in a PTY,
//! feeds it a script from stdin and prints the emulated screen.
//!
//!   cargo run -p hive-tui --example drive -- 140 40 target/debug/hive < script
//!
//! Script lines: `wait <ms>`, `keys <text>` (typed byte by byte, with \r \t \e
//! escapes), `seq <bytes>` (written at once, for escape sequences),
//! `ctrl <char>`, `dump`.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

fn unescape(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('r') => out.push(b'\r'),
                Some('n') => out.push(b'\n'),
                Some('t') => out.push(b'\t'),
                Some('e') => out.push(0x1b),
                Some('s') => out.push(b' '),
                Some(o) => out.extend(o.to_string().bytes()),
                None => {}
            }
        } else {
            out.extend(c.to_string().bytes());
        }
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cols: u16 = args[1].parse().unwrap();
    let rows: u16 = args[2].parse().unwrap();
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(&args[3]);
    cmd.args(&args[4..]);
    cmd.env("TERM", "xterm-256color");
    cmd.cwd(std::env::current_dir().unwrap());
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
    let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
    let raw = Arc::new(Mutex::new(Vec::<u8>::new()));
    let p2 = parser.clone();
    let r2 = raw.clone();
    let answer = writer.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            let chunk = &buf[..n];
            r2.lock().unwrap().extend_from_slice(chunk);
            // Answer the queries a real terminal would (kitty flags, DA1).
            let text = String::from_utf8_lossy(chunk);
            if text.contains("\x1b[?u") {
                let _ = answer.lock().unwrap().write_all(b"\x1b[?0u");
            }
            if text.contains("\x1b[c") {
                let _ = answer.lock().unwrap().write_all(b"\x1b[?62;22c");
            }
            let mut p = p2.lock().unwrap();
            p.process(chunk);
            if text.contains("\x1b[6n") {
                let (r, c) = p.screen().cursor_position();
                let _ = answer
                    .lock()
                    .unwrap()
                    .write_all(format!("\x1b[{};{}R", r + 1, c + 1).as_bytes());
            }
        }
    });
    let mut script = String::new();
    std::io::stdin().read_to_string(&mut script).unwrap();
    for line in script.lines() {
        let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
        match cmd {
            "wait" => std::thread::sleep(Duration::from_millis(rest.parse().unwrap())),
            "keys" => {
                for b in unescape(rest) {
                    let mut w = writer.lock().unwrap();
                    w.write_all(&[b]).unwrap();
                    w.flush().unwrap();
                    drop(w);
                    std::thread::sleep(Duration::from_millis(15));
                }
            }
            "seq" => {
                let mut w = writer.lock().unwrap();
                w.write_all(&unescape(rest)).unwrap();
                w.flush().unwrap();
            }
            "ctrl" => {
                let c = rest.chars().next().unwrap().to_ascii_lowercase();
                writer
                    .lock()
                    .unwrap()
                    .write_all(&[(c as u8) - b'a' + 1])
                    .unwrap();
            }
            "dump" => {
                let p = parser.lock().unwrap();
                println!("┌{}┐", "─".repeat(cols as usize));
                for row in p.screen().rows(0, cols) {
                    println!("│{row:<width$}│", width = cols as usize);
                }
                let (r, c) = p.screen().cursor_position();
                println!("└{}┘ cursor {r},{c}", "─".repeat(cols as usize));
            }
            "raw" => {
                let r = raw.lock().unwrap();
                let tail = &r[r.len().saturating_sub(600)..];
                println!(
                    "{} bytes total; tail: {:?}",
                    r.len(),
                    String::from_utf8_lossy(tail)
                );
                println!("exited: {:?}", child.try_wait());
            }
            "" => {}
            other => eprintln!("unknown script command {other}"),
        }
    }
    let _ = child.kill();
}
