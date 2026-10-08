//! The hive TUI: a thin client over the daemon.

pub mod app;
pub mod client;
pub mod event_loop;
pub mod git_panel;
pub mod help;
pub mod keys;
pub mod overlay;
pub mod term;
pub mod ui;

use std::io::stdout;
use std::path::Path;

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, terminal::supports_keyboard_enhancement};
use hive_core::config::GlobalConfig;
use hive_core::protocol::ClientRequest;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

fn restore_terminal(kitty: bool, mouse: bool) {
    let mut out = stdout();
    if kitty {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    if mouse {
        let _ = execute!(out, DisableMouseCapture);
    }
    let _ = execute!(
        out,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let _ = disable_raw_mode();
}

pub async fn run(hive_bin: &Path) -> Result<()> {
    let global = GlobalConfig::load().unwrap_or_else(|e| {
        eprintln!("hive: {e:#} — using defaults");
        GlobalConfig::default()
    });
    let stream = client::connect_or_spawn(hive_bin).await?;
    let (tx, rx) = client::split(stream);
    tx.send(ClientRequest::Hello {
        version: hive_core::PROTOCOL_VERSION,
    })?;
    tx.send(ClientRequest::Subscribe)?;

    let mouse = global.ui.mouse;
    let kitty = supports_keyboard_enhancement().unwrap_or(false);
    // Restores the terminal on every exit path, including errors.
    struct Guard {
        kitty: bool,
        mouse: bool,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            restore_terminal(self.kitty, self.mouse);
        }
    }
    enable_raw_mode()?;
    let _guard = Guard { kitty, mouse };
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
    if mouse {
        execute!(out, EnableMouseCapture)?;
    }
    if kitty {
        execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal(kitty, mouse);
        prev_hook(info);
    }));

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    terminal.clear()?;
    let mut app = app::App::new(tx, global);
    let res = event_loop::run(&mut terminal, &mut app, rx).await;
    // Let the writer flush the last requests (ui state, detach).
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    res
}
