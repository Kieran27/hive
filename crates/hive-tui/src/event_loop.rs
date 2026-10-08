//! The TUI's single event loop: terminal input, daemon events and timers in
//! one `select!`, drawing only when something changed, paced by a token
//! bucket so a keypress and its echo paint back to back while sustained
//! output is capped near 60 fps.

use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use hive_core::protocol::ServerEvent;
use ratatui::backend::Backend;
use ratatui::Terminal;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::app::App;
use crate::ui;

const BURST: f64 = 3.0;
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const MIN_GAP: Duration = Duration::from_millis(2);
const TOAST_TTL: Duration = Duration::from_secs(5);

struct Pacer {
    tokens: f64,
    last_refill: Instant,
    last_draw: Instant,
}

impl Pacer {
    fn new() -> Self {
        Self {
            tokens: BURST,
            last_refill: Instant::now(),
            last_draw: Instant::now() - FRAME_INTERVAL,
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let gained =
            now.duration_since(self.last_refill).as_secs_f64() / FRAME_INTERVAL.as_secs_f64();
        self.tokens = (self.tokens + gained).min(BURST);
        self.last_refill = now;
    }

    /// When the next frame may be drawn.
    fn next_allowed(&mut self) -> Instant {
        self.refill();
        let gap_ok = self.last_draw + MIN_GAP;
        if self.tokens >= 1.0 {
            gap_ok.max(Instant::now())
        } else {
            let wait = FRAME_INTERVAL.mul_f64(1.0 - self.tokens);
            (Instant::now() + wait).max(gap_ok)
        }
    }

    fn consume(&mut self) {
        self.refill();
        self.tokens = (self.tokens - 1.0).max(0.0);
        self.last_draw = Instant::now();
    }
}

pub async fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    mut rx: UnboundedReceiver<ServerEvent>,
) -> anyhow::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let mut events = EventStream::new();
    let mut pacer = Pacer::new();
    let mut anim = tokio::time::interval(Duration::from_millis(110));
    anim.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut housekeeping = tokio::time::interval(Duration::from_secs(1));

    // First layout so attaches use the right size.
    let size = terminal.size()?;
    ui::layout(
        app,
        ratatui::layout::Rect::new(0, 0, size.width, size.height),
    );

    loop {
        if app.quit {
            break;
        }
        let draw_at = if app.dirty {
            pacer.next_allowed()
        } else {
            Instant::now() + Duration::from_secs(3600)
        };
        tokio::select! {
            ev = events.next() => {
                match ev {
                    Some(Ok(Event::Key(k))) if k.kind != KeyEventKind::Release => app.on_key(k),
                    Some(Ok(Event::Mouse(m))) => app.on_mouse(m),
                    Some(Ok(Event::Paste(s))) => app.on_paste(s),
                    Some(Ok(Event::Resize(_, _))) => app.dirty = true,
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
                after_change(terminal, app)?;
            }
            msg = rx.recv() => {
                match msg {
                    Some(ev) => {
                        app.on_server(ev);
                        // Drain whatever else is queued before redrawing.
                        for _ in 0..256 {
                            match rx.try_recv() {
                                Ok(ev) => app.on_server(ev),
                                Err(_) => break,
                            }
                        }
                    }
                    None => {
                        app.connected = false;
                        app.dirty = true;
                        // Keep the UI up so the user sees why; wait for quit.
                        let (_tx, new_rx) = tokio::sync::mpsc::unbounded_channel();
                        rx = new_rx;
                    }
                }
                after_change(terminal, app)?;
            }
            _ = tokio::time::sleep_until(draw_at.into()), if app.dirty => {
                terminal.draw(|f| ui::draw(f, app))?;
                pacer.consume();
                app.dirty = false;
                // The frame may have changed which sessions are visible.
                app.sync_attachments();
            }
            _ = anim.tick(), if app.anything_working() => {
                app.tick = app.tick.wrapping_add(1);
                app.dirty = true;
            }
            _ = housekeeping.tick() => {
                if app.toast.as_ref().map(|t| t.2.elapsed() > TOAST_TTL).unwrap_or(false) {
                    app.toast = None;
                    app.dirty = true;
                }
                app.save_ui(false);
                app.git_tick(false);
            }
        }
    }
    app.save_ui(true);
    Ok(())
}

fn after_change<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> anyhow::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let size = terminal.size()?;
    ui::layout(
        app,
        ratatui::layout::Rect::new(0, 0, size.width, size.height),
    );
    app.sync_attachments();
    app.git_tick(false);
    Ok(())
}
