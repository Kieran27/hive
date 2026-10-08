//! Rendering. `layout` runs before every sync/draw and records the rects
//! that input handling and attachment sizing rely on.

use hive_core::paths::tildify;
use hive_core::protocol::*;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::app::{App, Areas, Mode, Row, Tab};
use crate::overlay::*;

const ACCENT: Color = Color::Rgb(255, 183, 77);
const DIM: Color = Color::Rgb(110, 110, 120);
const GREEN: Color = Color::Rgb(120, 200, 120);
const RED: Color = Color::Rgb(235, 100, 100);
const BLUE: Color = Color::Rgb(110, 170, 255);
const MAGENTA: Color = Color::Rgb(210, 130, 240);
const SEL_BG: Color = Color::Rgb(45, 45, 60);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn status_span(status: Status, alive: bool, is_run: bool, tick: u64) -> Span<'static> {
    match status {
        Status::Working if is_run && alive => Span::styled("▶", Style::default().fg(GREEN)),
        Status::Working => Span::styled(
            SPINNER[(tick as usize) % SPINNER.len()],
            Style::default().fg(BLUE),
        ),
        Status::Waiting => Span::styled(
            "◐",
            Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD),
        ),
        Status::Done => Span::styled("✓", Style::default().fg(GREEN).add_modifier(Modifier::BOLD)),
        Status::Exited => Span::styled("■", Style::default().fg(DIM)),
        Status::Idle if alive => Span::styled("○", Style::default().fg(DIM)),
        Status::Idle => Span::styled("■", Style::default().fg(DIM)),
    }
}

fn session_dot(s: &SessionInfo, tick: u64) -> Span<'static> {
    if !s.alive && s.exit_code.map(|c| c != 0).unwrap_or(false) {
        return Span::styled("✗", Style::default().fg(RED));
    }
    if s.group().is_some() && s.alive {
        return Span::styled("▶", Style::default().fg(GREEN));
    }
    status_span(s.status, s.alive, s.group().is_some(), tick)
}

pub fn tree_offset(app: &App, height: usize) -> usize {
    if height == 0 || app.sel < height {
        0
    } else {
        app.sel + 1 - height
    }
}

/// Compute every rect for this frame (also used before attaching).
pub fn layout(app: &mut App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);
    let sw = app.sidebar_width.min(area.width.saturating_sub(30)).max(16);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(sw), Constraint::Min(10)])
        .split(rows[0]);
    let sidebar = cols[0];
    let mut main = cols[1];
    // Source-control panel on the right.
    if app.git.visible && main.width > 60 {
        let gw = (main.width / 3).clamp(30, 56);
        let rect = Rect {
            x: main.x + main.width - gw,
            width: gw,
            ..main
        };
        main.width -= gw;
        app.git.rect = rect;
        // Border, a branch line on top, two hint lines at the bottom.
        app.git.list = Rect {
            x: rect.x + 1,
            y: rect.y + 2,
            width: rect.width.saturating_sub(2),
            height: rect.height.saturating_sub(5),
        };
    } else {
        app.git.rect = Rect::default();
        app.git.list = Rect::default();
    }
    let footer_h = 4u16.min(sidebar.height.saturating_sub(3));
    let tree = Rect {
        x: sidebar.x + 1,
        y: sidebar.y + 1,
        width: sidebar.width.saturating_sub(2),
        height: sidebar.height.saturating_sub(2 + footer_h),
    };
    let header = Rect { height: 1, ..main };
    let tabs = Rect {
        y: main.y + 1,
        height: 1,
        ..main
    };
    let pane_outer = Rect {
        y: main.y + 2,
        height: main.height.saturating_sub(2),
        ..main
    };
    let pane = Rect {
        x: pane_outer.x + 1,
        y: pane_outer.y + 1,
        width: pane_outer.width.saturating_sub(2),
        height: pane_outer.height.saturating_sub(2),
    };
    app.areas = Areas {
        sidebar,
        tree,
        header,
        tabs,
        pane,
        status: rows[1],
    };

    // Visible sessions: the active tab, split vertically for run groups.
    app.pane_rects.clear();
    if let Some(tab) = app.active_tab() {
        let ids = tab.sessions();
        if ids.len() == 1 {
            app.pane_rects.push((ids[0].clone(), pane));
        } else {
            let n = ids.len() as u16;
            // Each proc gets a title line.
            let each = pane.height / n;
            for (i, id) in ids.iter().enumerate() {
                let y = pane.y + each * i as u16;
                let h = if i as u16 == n - 1 {
                    pane.y + pane.height - y
                } else {
                    each
                };
                let r = Rect {
                    x: pane.x,
                    y: y + 1,
                    width: pane.width,
                    height: h.saturating_sub(1),
                };
                app.pane_rects.push((id.clone(), r));
            }
        }
    }

    // Tab hit boxes.
    let labels: Vec<(String, String)> = app
        .current_tabs()
        .iter()
        .enumerate()
        .map(|(i, t)| (tab_label(app, t, i), t.key()))
        .collect();
    app.tab_hits.clear();
    let mut x = tabs.x + 1;
    for (label, key) in labels {
        // dot + spaces around the label
        let w = label.chars().count() as u16 + 3;
        if x + w > tabs.x + tabs.width {
            break;
        }
        app.tab_hits.push((
            Rect {
                x,
                y: tabs.y,
                width: w,
                height: 1,
            },
            key,
        ));
        x += w + 1;
    }
}

fn tab_label(app: &App, tab: &Tab, i: usize) -> String {
    match tab {
        Tab::Single(id) => match app.session(id) {
            Some(s) if s.title != s.kind.label() => {
                let title: String = s.title.chars().take(24).collect();
                format!("{} {}:{title}", i + 1, s.kind.label())
            }
            Some(s) => format!("{} {}", i + 1, s.kind.label()),
            None => format!("{} ?", i + 1),
        },
        Tab::Group { target, sessions } => {
            let ports: Vec<String> = sessions
                .iter()
                .filter_map(|id| app.session(id))
                .flat_map(|s| s.ports.iter().map(|p| p.to_string()))
                .collect();
            if ports.is_empty() {
                format!("{} run:{target}", i + 1)
            } else {
                format!("{} run:{target} {}", i + 1, ports.join("/"))
            }
        }
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    layout(app, area);
    draw_sidebar(f, app);
    draw_main(f, app);
    if app.git.rect.width > 0 {
        draw_git_panel(f, app);
    }
    draw_status(f, app);
    if app.overlay.is_some() {
        draw_overlay(f, app, area);
    }
}

/// Hexagons after the name in the sidebar title.
const HONEYCOMB: &str = "⬢⬢⬢";

fn sidebar_title(app: &App) -> Line<'static> {
    let amber = Style::default().fg(ACCENT);
    let mut spans = vec![
        Span::raw(" "),
        Span::styled("hive", amber.add_modifier(Modifier::BOLD)),
        Span::raw(" "),
    ];
    if app.global.ui.logo {
        spans.push(Span::styled(HONEYCOMB, amber));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

fn draw_sidebar(f: &mut Frame, app: &App) {
    let a = app.areas;
    let focused = app.mode == Mode::Nav && app.overlay.is_none();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { ACCENT } else { DIM }))
        .title(sidebar_title(app));
    f.render_widget(block, a.sidebar);

    if app.projects.is_empty() {
        let msg = Paragraph::new(vec![
            Line::from("No projects yet."),
            Line::from(""),
            Line::from(vec![
                Span::styled("a", Style::default().fg(ACCENT).bold()),
                Span::raw(" add a git repo"),
            ]),
            Line::from(vec![
                Span::styled("?", Style::default().fg(ACCENT).bold()),
                Span::raw(" help"),
            ]),
        ])
        .style(Style::default().fg(DIM))
        .wrap(Wrap { trim: true });
        f.render_widget(msg, a.tree);
        return;
    }

    let h = a.tree.height as usize;
    let offset = tree_offset(app, h);
    let width = a.tree.width as usize;
    for (i, row) in app.rows.iter().enumerate().skip(offset).take(h) {
        let y = a.tree.y + (i - offset) as u16;
        let selected = i == app.sel;
        let mut spans: Vec<Span> = Vec::new();
        match *row {
            Row::Project(pi) => {
                let p = &app.projects[pi];
                let open = app.expanded.contains(&format!("p:{}", p.id));
                spans.push(Span::styled(
                    if open { "▾ " } else { "▸ " },
                    Style::default().fg(DIM),
                ));
                spans.push(Span::styled(p.name.clone(), Style::default().bold()));
                if p.config_error.is_some() {
                    spans.push(Span::styled(" ⚠", Style::default().fg(RED)));
                }
                if let Some(st) = app.project_status(p) {
                    if st > Status::Exited {
                        spans.push(Span::raw(" "));
                        spans.push(status_span(st, true, false, app.tick));
                    }
                }
            }
            Row::Worktree(pi, wi) => {
                let p = &app.projects[pi];
                let w = &p.worktrees[wi];
                let has_pkgs = p.packages.iter().any(|k| w.path.join(&k.path).exists());
                let open = app.expanded.contains(&format!("w:{}", w.path.display()));
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    if !has_pkgs {
                        "  "
                    } else if open {
                        "▾ "
                    } else {
                        "▸ "
                    },
                    Style::default().fg(DIM),
                ));
                let style = if w.prunable {
                    Style::default().fg(DIM).add_modifier(Modifier::CROSSED_OUT)
                } else if w.is_main {
                    Style::default().fg(BLUE)
                } else {
                    Style::default()
                };
                let label = w.label();
                spans.push(Span::styled(label, style));
                if w.dirty {
                    spans.push(Span::styled("*", Style::default().fg(ACCENT)));
                }
                if app.vscode_opened.contains(&w.path) {
                    spans.push(Span::styled(" ◫", Style::default().fg(BLUE)));
                }
                if let Some((st, running, failed)) = app.worktree_status(&w.path) {
                    if st > Status::Exited {
                        spans.push(Span::raw(" "));
                        spans.push(status_span(st, true, false, app.tick));
                    }
                    if running {
                        spans.push(Span::styled(" ▶", Style::default().fg(GREEN)));
                    }
                    if failed {
                        spans.push(Span::styled(" ✗", Style::default().fg(RED)));
                    }
                }
            }
            Row::Package(pi, _, ki) => {
                let k = &app.projects[pi].packages[ki];
                spans.push(Span::styled("      · ", Style::default().fg(DIM)));
                spans.push(Span::styled(
                    k.name.clone(),
                    Style::default().fg(Color::Gray),
                ));
            }
        }
        let line = Line::from(spans);
        let mut style = Style::default();
        if selected {
            style = style.bg(SEL_BG);
            if focused {
                style = style.add_modifier(Modifier::BOLD);
            }
        }
        let r = Rect {
            x: a.tree.x,
            y,
            width: a.tree.width,
            height: 1,
        };
        f.render_widget(Paragraph::new(line).style(style), r);
        let _ = width;
    }

    // Footer: ports + path of the selected worktree.
    let footer = Rect {
        x: a.sidebar.x + 1,
        y: a.tree.y + a.tree.height,
        width: a.tree.width,
        height: a.sidebar.height.saturating_sub(a.tree.height + 2),
    };
    if footer.height == 0 {
        return;
    }
    if let Some((p, w)) = app.selected_worktree() {
        let mut lines = vec![Line::from(Span::styled(
            "─".repeat(footer.width as usize),
            Style::default().fg(DIM),
        ))];
        let mut port_spans = vec![Span::styled(
            format!("slot {} ", w.slot),
            Style::default().fg(DIM),
        )];
        for r in &p.runs {
            let ports: Vec<String> = r
                .procs
                .iter()
                .filter_map(|pr| {
                    pr.base_port
                        .map(|b| (b + w.slot * p.port_stride).to_string())
                })
                .collect();
            if !ports.is_empty() {
                let running = app
                    .sessions
                    .iter()
                    .any(|s| s.worktree == w.path && s.alive && s.group() == Some(r.name.as_str()));
                port_spans.push(Span::styled(
                    format!("{}:{} ", r.name, ports.join("/")),
                    Style::default().fg(if running { GREEN } else { Color::Gray }),
                ));
            }
        }
        lines.push(Line::from(port_spans));
        lines.push(Line::from(Span::styled(
            tildify(&w.path),
            Style::default().fg(DIM),
        )));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), footer);
    }
}

fn draw_main(f: &mut Frame, app: &mut App) {
    let a = app.areas;
    // Header.
    let header = match app.selected_worktree() {
        Some((p, w)) => {
            let mut spans = vec![
                Span::styled(
                    format!(" {} ", p.name),
                    Style::default().fg(Color::Black).bg(ACCENT).bold(),
                ),
                Span::raw(" "),
                Span::styled(w.label(), Style::default().bold()),
            ];
            if let Some(Row::Package(pi, _, ki)) = app.selected_row() {
                spans.push(Span::styled(
                    format!(" › {}", app.projects[pi].packages[ki].name),
                    Style::default().fg(Color::Gray),
                ));
            }
            if app.vscode_opened.contains(&w.path) {
                spans.push(Span::styled(" [vscode]", Style::default().fg(BLUE)));
            }
            spans.push(Span::styled(
                format!("  {}", tildify(&w.path)),
                Style::default().fg(DIM),
            ));
            if w.prunable {
                spans.push(Span::styled(
                    "  directory missing — D to prune",
                    Style::default().fg(RED),
                ));
            }
            if let Some(e) = &p.config_error {
                spans.push(Span::styled(format!("  ⚠ {e}"), Style::default().fg(RED)));
            }
            Line::from(spans)
        }
        None => Line::from(Span::styled(
            " no project selected",
            Style::default().fg(DIM),
        )),
    };
    f.render_widget(Paragraph::new(header), a.header);

    // Tabs.
    let tabs = app.current_tabs();
    let active = app.active_tab().map(|t| t.key());
    for (i, (rect, key)) in app.tab_hits.iter().enumerate() {
        let tab = &tabs[i];
        let is_active = Some(key) == active.as_ref();
        let mut spans = vec![Span::raw(" ")];
        let st = tab
            .sessions()
            .iter()
            .filter_map(|id| app.session(id))
            .map(|s| (s.status, s.alive, s.exit_code))
            .max_by_key(|x| x.0);
        let dot = match tab {
            Tab::Single(id) => app.session(id).map(|s| session_dot(s, app.tick)),
            Tab::Group { sessions, .. } => {
                let any_alive = sessions
                    .iter()
                    .filter_map(|id| app.session(id))
                    .any(|s| s.alive);
                let failed = sessions
                    .iter()
                    .filter_map(|id| app.session(id))
                    .any(|s| !s.alive && s.exit_code.map(|c| c != 0).unwrap_or(false));
                Some(if failed {
                    Span::styled("✗", Style::default().fg(RED))
                } else if any_alive {
                    Span::styled("▶", Style::default().fg(GREEN))
                } else {
                    Span::styled("■", Style::default().fg(DIM))
                })
            }
        };
        let _ = st;
        let label = tab_label(app, tab, i);
        if let Some(d) = dot {
            spans.push(d);
            spans.push(Span::raw(" "));
        }
        spans.push(Span::raw(label));
        let style = if is_active {
            Style::default().bg(SEL_BG).fg(Color::White).bold()
        } else {
            Style::default().fg(Color::Gray)
        };
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(style),
            Rect {
                width: rect.width + 1,
                ..*rect
            },
        );
    }
    if tabs.is_empty() && app.selected_worktree().is_some() {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" c", Style::default().fg(ACCENT).bold()),
                Span::raw(" claude  "),
                Span::styled("x", Style::default().fg(ACCENT).bold()),
                Span::raw(" codex  "),
                Span::styled("t", Style::default().fg(ACCENT).bold()),
                Span::raw(" shell  "),
                Span::styled("r", Style::default().fg(ACCENT).bold()),
                Span::raw(" run  "),
                Span::styled("e", Style::default().fg(ACCENT).bold()),
                Span::raw(" vscode"),
            ]))
            .style(Style::default().fg(DIM)),
            a.tabs,
        );
    }

    // Pane border.
    let outer = Rect {
        x: a.pane.x.saturating_sub(1),
        y: a.pane.y.saturating_sub(1),
        width: a.pane.width + 2,
        height: a.pane.height + 2,
    };
    let term_focus = app.mode == Mode::Terminal && app.overlay.is_none();
    let focused_id = app.focused_session();
    let mut title = String::new();
    if let Some(id) = &focused_id {
        if let Some(s) = app.session(id) {
            title = format!(" {} · {} ", s.kind.label(), tildify(&s.cwd));
            if let Some(t) = app.terms.get(id) {
                let sb = t.parser.screen().scrollback();
                if sb > 0 {
                    title.push_str(&format!("[scrolled {sb}] "));
                }
            }
        }
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if term_focus { GREEN } else { DIM }))
        .title(Span::styled(
            title,
            Style::default().fg(if term_focus { GREEN } else { Color::Gray }),
        ));
    f.render_widget(block, outer);

    let rects = app.pane_rects.clone();
    if rects.is_empty() {
        let hint = if app.selected_worktree().is_some() {
            "No sessions here yet — c: claude  x: codex  t: shell  r: run"
        } else {
            "Press a to add a project."
        };
        f.render_widget(
            Paragraph::new(hint)
                .style(Style::default().fg(DIM))
                .wrap(Wrap { trim: true }),
            a.pane,
        );
        return;
    }
    let multi = rects.len() > 1;
    for (id, rect) in &rects {
        let is_focus = focused_id.as_deref() == Some(id.as_str());
        if multi {
            let s = app.session(id);
            let label = s.map(|s| s.kind.label()).unwrap_or_default();
            let ports = s
                .map(|s| {
                    s.ports
                        .iter()
                        .map(|p| format!(":{p}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            let dot = s
                .map(|s| session_dot(s, app.tick))
                .unwrap_or(Span::raw(" "));
            let style = if is_focus {
                Style::default().fg(Color::White).bg(SEL_BG).bold()
            } else {
                Style::default().fg(DIM)
            };
            let line = Line::from(vec![
                dot,
                Span::raw(format!(" {label} {ports} ")),
                Span::styled("─".repeat(rect.width as usize), Style::default().fg(DIM)),
            ]);
            f.render_widget(
                Paragraph::new(line).style(style),
                Rect {
                    y: rect.y - 1,
                    height: 1,
                    ..*rect
                },
            );
        }
        let info = app.session(id).cloned();
        match app.terms.get(id) {
            Some(t) if t.synced => {
                let screen = t.parser.screen();
                f.render_widget(
                    PseudoTerminal::new(screen).cursor(Cursor::default().visibility(false)),
                    *rect,
                );
                if term_focus
                    && is_focus
                    && screen.scrollback() == 0
                    && !screen.hide_cursor()
                    && info.as_ref().map(|s| s.alive).unwrap_or(false)
                {
                    let (row, col) = screen.cursor_position();
                    if row < rect.height && col < rect.width {
                        f.set_cursor_position(Position {
                            x: rect.x + col,
                            y: rect.y + row,
                        });
                    }
                }
            }
            _ => {
                let msg = match &info {
                    Some(s) if s.resumable() => "Session ended with the daemon. Press u (or Enter) to resume the conversation.".to_string(),
                    Some(s) if !s.alive => "Session ended.".to_string(),
                    _ => "connecting…".to_string(),
                };
                f.render_widget(
                    Paragraph::new(msg)
                        .style(Style::default().fg(DIM))
                        .wrap(Wrap { trim: true }),
                    *rect,
                );
            }
        }
        // Exit banner.
        if let Some(s) = &info {
            if !s.alive && rect.height > 0 {
                let code = s
                    .exit_code
                    .map(|c| format!("exit {c}"))
                    .unwrap_or_else(|| "ended".into());
                let hint = if s.resumable() {
                    " — u: resume  w: close"
                } else if s.kind == SessionKind::Setup {
                    " — S: re-run setup  w: close"
                } else if s.group().is_some() {
                    " — R: restart  w: close"
                } else {
                    " — u: restart  w: close"
                };
                let color = if s.exit_code.map(|c| c != 0).unwrap_or(false) {
                    RED
                } else {
                    DIM
                };
                let banner = Line::from(vec![Span::styled(
                    format!(" ■ {code}{hint} "),
                    Style::default().fg(Color::Black).bg(color),
                )]);
                let r = Rect {
                    y: rect.y + rect.height - 1,
                    height: 1,
                    ..*rect
                };
                f.render_widget(
                    Paragraph::new(banner).alignment(ratatui::layout::Alignment::Right),
                    r,
                );
            }
        }
    }
}

fn draw_status(f: &mut Frame, app: &App) {
    let a = app.areas.status;
    let (badge, color) = match (app.mode, app.overlay.is_some()) {
        (_, true) => (" MENU ", MAGENTA),
        (Mode::Nav, _) => (" NAV ", ACCENT),
        (Mode::Terminal, _) => (" TERM ", GREEN),
    };
    let mut spans = vec![
        Span::styled(badge, Style::default().fg(Color::Black).bg(color).bold()),
        Span::raw(" "),
    ];
    if let Some((level, msg, _)) = &app.toast {
        let c = match level {
            ToastLevel::Info => Color::Gray,
            ToastLevel::Warn => ACCENT,
            ToastLevel::Error => RED,
        };
        spans.push(Span::styled(msg.clone(), Style::default().fg(c)));
    } else {
        let used: usize = spans.iter().map(|sp| sp.content.chars().count()).sum();
        let hints = crate::help::fit(
            crate::help::context_hints(app),
            (a.width as usize).saturating_sub(used),
        );
        for (k, v) in hints {
            spans.push(Span::styled(k, Style::default().fg(ACCENT).bold()));
            spans.push(Span::styled(format!(" {v}  "), Style::default().fg(DIM)));
        }
    }
    if !app.connected {
        spans.push(Span::styled(
            "  daemon disconnected — restart hive",
            Style::default().fg(RED).bold(),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), a);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(4));
    let h = h.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 3,
        width: w,
        height: h,
    }
}

fn modal(f: &mut Frame, r: Rect, title: &str) -> Rect {
    f.render_widget(Clear, r);
    let b = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(ACCENT).bold(),
        ));
    let inner = b.inner(r);
    f.render_widget(b, r);
    inner
}

fn input_line(f: &mut Frame, r: Rect, input: &TextInput, focused: bool) {
    let style = if focused {
        Style::default().bg(SEL_BG)
    } else {
        Style::default()
    };
    // Scroll horizontally so the cursor stays visible.
    let width = r.width.max(1) as usize;
    let start = input.cursor.saturating_sub(width.saturating_sub(1));
    let visible: String = input.value.chars().skip(start).take(width).collect();
    f.render_widget(Paragraph::new(visible).style(style), r);
    if focused {
        f.set_cursor_position(Position {
            x: r.x + (input.cursor - start) as u16,
            y: r.y,
        });
    }
}

fn draw_overlay(f: &mut Frame, app: &App, area: Rect) {
    let Some(o) = &app.overlay else { return };
    match o {
        Overlay::Help { filter, scroll } => draw_help(f, app, filter, *scroll, area),
        Overlay::Loading { pending } => {
            let r = centered(area, 50, 3);
            let inner = modal(f, r, &format!("run {}", pending.target));
            f.render_widget(
                Paragraph::new("loading choices…").style(Style::default().fg(DIM)),
                inner,
            );
        }
        Overlay::Confirm { msg, .. } => {
            let lines = msg.lines().count() as u16;
            let r = centered(area, 70, lines + 4);
            let inner = modal(f, r, "confirm");
            let mut text: Vec<Line> = msg.lines().map(|l| Line::from(l.to_string())).collect();
            text.push(Line::from(""));
            text.push(Line::from(vec![
                Span::styled("y", Style::default().fg(ACCENT).bold()),
                Span::raw(" yes   "),
                Span::styled("n/esc", Style::default().fg(ACCENT).bold()),
                Span::raw(" no"),
            ]));
            f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
        }
        Overlay::Input { title, input, .. } => {
            let r = centered(area, 70, 4);
            let inner = modal(f, r, title);
            input_line(f, Rect { height: 1, ..inner }, input, true);
            f.render_widget(
                Paragraph::new("enter: ok · esc: cancel").style(Style::default().fg(DIM)),
                Rect {
                    y: inner.y + 1,
                    height: 1,
                    ..inner
                },
            );
        }
        Overlay::Picker(p) => draw_picker(f, p, area),
        Overlay::Wizard(w) => draw_wizard(f, w, area),
        Overlay::Diff {
            title,
            lines,
            scroll,
        } => draw_diff(f, title, lines, *scroll, area),
    }
}

fn draw_picker(f: &mut Frame, p: &Picker, area: Rect) {
    let filtered = p.filtered();
    let h = (filtered.len() as u16 + 3).clamp(5, 20);
    let r = centered(area, 80, h);
    let inner = modal(f, r, &p.title);
    input_line(f, Rect { height: 1, ..inner }, &p.filter, true);
    let list_h = inner.height.saturating_sub(1) as usize;
    let offset = if p.sel >= list_h {
        p.sel + 1 - list_h
    } else {
        0
    };
    for (row, &i) in filtered.iter().enumerate().skip(offset).take(list_h) {
        let it = &p.items[i];
        let selected = row == p.sel;
        let line = Line::from(vec![
            Span::styled(
                if selected { "› " } else { "  " },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                it.label.clone(),
                if selected {
                    Style::default().bold()
                } else {
                    Style::default()
                },
            ),
            Span::styled(format!("  {}", it.detail), Style::default().fg(DIM)),
        ]);
        let y = inner.y + 1 + (row - offset) as u16;
        f.render_widget(
            Paragraph::new(line).style(if selected {
                Style::default().bg(SEL_BG)
            } else {
                Style::default()
            }),
            Rect {
                y,
                height: 1,
                ..inner
            },
        );
    }
    if filtered.is_empty() {
        f.render_widget(
            Paragraph::new("  no matches").style(Style::default().fg(DIM)),
            Rect {
                y: inner.y + 1,
                height: 1,
                ..inner
            },
        );
    }
}

fn draw_wizard(f: &mut Frame, w: &Wizard, area: Rect) {
    let fields = w.fields();
    let sugg = w.suggestions();
    let h = fields.len() as u16 + sugg.len() as u16 + 7;
    let title = match &w.setup_only {
        Some((_, label)) => format!("setup · {} › {label}", w.project_name),
        None => format!("new worktree · {}", w.project_name),
    };
    let r = centered(area, 76, h.max(10));
    let inner = modal(f, r, &title);
    let label_w = 10u16;
    let mut y = inner.y;
    let focused = w.focused();
    let put_label = |f: &mut Frame, y: u16, text: &str, is_focus: bool| {
        let style = if is_focus {
            Style::default().fg(ACCENT).bold()
        } else {
            Style::default().fg(Color::Gray)
        };
        f.render_widget(
            Paragraph::new(text.to_string()).style(style),
            Rect {
                x: inner.x,
                y,
                width: label_w,
                height: 1,
            },
        );
    };
    let val = Rect {
        x: inner.x + label_w,
        y: 0,
        width: inner.width.saturating_sub(label_w),
        height: 1,
    };
    for field in &fields {
        let is_focus = *field == focused;
        match field {
            Field::Mode => {
                put_label(f, y, "mode", is_focus);
                let spans: Vec<Span> = MODES
                    .iter()
                    .flat_map(|m| {
                        let on = *m == w.mode;
                        [
                            Span::styled(
                                format!(" {} ", mode_label(*m)),
                                if on {
                                    Style::default().fg(Color::Black).bg(if is_focus {
                                        ACCENT
                                    } else {
                                        Color::Gray
                                    })
                                } else {
                                    Style::default().fg(DIM)
                                },
                            ),
                            Span::raw(" "),
                        ]
                    })
                    .collect();
                f.render_widget(Paragraph::new(Line::from(spans)), Rect { y, ..val });
            }
            Field::Branch => {
                let lbl = match w.mode {
                    WorktreeMode::Detached => "ref",
                    _ => "branch",
                };
                put_label(f, y, lbl, is_focus);
                input_line(f, Rect { y, ..val }, &w.branch, is_focus);
            }
            Field::Base => {
                put_label(f, y, "from", is_focus);
                input_line(f, Rect { y, ..val }, &w.base, is_focus);
            }
            Field::Env => {
                put_label(f, y, "env", is_focus);
                let env = w.env().unwrap_or_default();
                let style = if is_focus {
                    Style::default().bg(SEL_BG)
                } else {
                    Style::default()
                };
                f.render_widget(
                    Paragraph::new(format!("‹ {env} ›")).style(style),
                    Rect { y, ..val },
                );
            }
            Field::Step(i) => {
                if *i == 0 {
                    put_label(f, y, "setup", false);
                }
                let (s, on) = &w.steps[*i];
                let style = if is_focus {
                    Style::default().bg(SEL_BG).bold()
                } else {
                    Style::default()
                };
                let label = s
                    .label
                    .replace("{env}", &w.env().unwrap_or_else(|| "{env}".into()));
                f.render_widget(
                    Paragraph::new(format!("[{}] {label}", if *on { "x" } else { " " }))
                        .style(style),
                    Rect { y, ..val },
                );
            }
        }
        y += 1;
        // Suggestions under the focused branch/base field.
        if is_focus && matches!(field, Field::Branch | Field::Base) {
            if w.loading_branches
                && matches!(
                    w.mode,
                    WorktreeMode::Local | WorktreeMode::Remote | WorktreeMode::Detached
                )
            {
                f.render_widget(
                    Paragraph::new("  fetching branches…").style(Style::default().fg(DIM)),
                    Rect { y, ..val },
                );
                y += 1;
            }
            for (i, s) in sugg.iter().enumerate() {
                let sel = w.sugg_sel == Some(i);
                let style = if sel {
                    Style::default().fg(ACCENT).bg(SEL_BG)
                } else {
                    Style::default().fg(DIM)
                };
                f.render_widget(
                    Paragraph::new(format!("  {s}")).style(style),
                    Rect { y, ..val },
                );
                y += 1;
            }
        }
    }
    y += 1;
    let mut info = Vec::new();
    if w.setup_only.is_none() {
        info.push(Span::styled(
            format!("→ {}", w.dest_preview()),
            Style::default().fg(DIM),
        ));
    }
    if let Some(p) = &w.profile {
        info.push(Span::styled(
            format!("   profile: {p}"),
            Style::default().fg(DIM),
        ));
    }
    if y < inner.y + inner.height {
        f.render_widget(
            Paragraph::new(Line::from(info)),
            Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            },
        );
        y += 1;
    }
    if let Some(e) = &w.error {
        if y < inner.y + inner.height {
            f.render_widget(
                Paragraph::new(e.clone()).style(Style::default().fg(RED)),
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
            );
        }
    }
    let help = "tab/↑↓ move · ←→ change · space toggle · enter create · esc cancel";
    f.render_widget(
        Paragraph::new(help).style(Style::default().fg(DIM)),
        Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        },
    );
}

fn draw_help(f: &mut Frame, app: &App, filter: &TextInput, scroll: usize, area: Rect) {
    let results = crate::help::search(&filter.value, &app.unlock_label());
    // Group under section headers.
    let mut lines: Vec<Line> = Vec::new();
    let mut last = "";
    for (section, keys, desc) in &results {
        if *section != last {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                *section,
                Style::default().fg(ACCENT).bold(),
            )));
            last = section;
        }
        lines.push(Line::from(vec![
            Span::styled(format!("  {keys:<26}"), Style::default().bold()),
            Span::styled(*desc, Style::default().fg(Color::Gray)),
        ]));
    }
    if results.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matching keys",
            Style::default().fg(DIM),
        )));
    }
    let r = centered(area, 90, area.height.saturating_sub(4));
    let inner = modal(f, r, "keys — type to search");
    let search = Rect { height: 1, ..inner };
    let prompt = Rect { width: 2, ..search };
    f.render_widget(
        Paragraph::new("/ ").style(Style::default().fg(ACCENT)),
        prompt,
    );
    input_line(
        f,
        Rect {
            x: search.x + 2,
            width: search.width.saturating_sub(2),
            ..search
        },
        filter,
        true,
    );
    let body = Rect {
        y: inner.y + 2,
        height: inner.height.saturating_sub(3),
        ..inner
    };
    let max_scroll = lines.len().saturating_sub(body.height as usize);
    let scroll = scroll.min(max_scroll);
    let visible: Vec<Line> = lines
        .into_iter()
        .skip(scroll)
        .take(body.height as usize)
        .collect();
    f.render_widget(Paragraph::new(visible), body);
    let more = if max_scroll > scroll {
        "  ↓ more (↑↓ / pgdn)"
    } else {
        ""
    };
    f.render_widget(
        Paragraph::new(format!(
            "{} of {} keys{more} · esc close",
            results.len(),
            crate::help::GLOSSARY.len()
        ))
        .style(Style::default().fg(DIM)),
        Rect {
            y: inner.y + inner.height.saturating_sub(1),
            height: 1,
            ..inner
        },
    );
}
fn status_letter(c: char) -> Span<'static> {
    let color = match c {
        'M' => ACCENT,
        'A' | '?' => GREEN,
        'D' => RED,
        'R' | 'C' => BLUE,
        'U' => MAGENTA,
        _ => Color::Gray,
    };
    let shown = if c == '?' { 'U' } else { c };
    Span::styled(
        shown.to_string(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn draw_git_panel(f: &mut Frame, app: &mut App) {
    use crate::git_panel::{GitRow, Section};
    let rect = app.git.rect;
    let focused = app.git.focused && app.mode == Mode::Nav && app.overlay.is_none();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { ACCENT } else { DIM }))
        .title(Span::styled(
            " source control ",
            Style::default()
                .fg(if focused { ACCENT } else { Color::Gray })
                .add_modifier(Modifier::BOLD),
        ));
    f.render_widget(block, rect);
    let inner = Rect {
        x: rect.x + 1,
        y: rect.y + 1,
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(2),
    };
    if inner.height < 4 {
        return;
    }

    // Branch line.
    let head = match &app.git.status {
        None => Line::from(Span::styled("loading…", Style::default().fg(DIM))),
        Some(st) => {
            let mut spans = vec![Span::styled(
                format!(
                    " {}",
                    st.branch.clone().unwrap_or_else(|| "detached HEAD".into())
                ),
                Style::default().add_modifier(Modifier::BOLD),
            )];
            if st.ahead > 0 {
                spans.push(Span::styled(
                    format!(" ↑{}", st.ahead),
                    Style::default().fg(GREEN),
                ));
            }
            if st.behind > 0 {
                spans.push(Span::styled(
                    format!(" ↓{}", st.behind),
                    Style::default().fg(ACCENT),
                ));
            }
            if st.upstream.is_none() && st.branch.is_some() {
                spans.push(Span::styled("  (no upstream)", Style::default().fg(DIM)));
            }
            Line::from(spans)
        }
    };
    f.render_widget(Paragraph::new(head), Rect { height: 1, ..inner });

    // File list.
    let list = app.git.list;
    let rows = app.git.rows();
    let h = list.height as usize;
    if app.git.sel < app.git.offset {
        app.git.offset = app.git.sel;
    } else if h > 0 && app.git.sel >= app.git.offset + h {
        app.git.offset = app.git.sel + 1 - h;
    }
    let offset = app.git.offset;
    for (i, row) in rows.iter().enumerate().skip(offset).take(h) {
        let y = list.y + (i - offset) as u16;
        let selected = i == app.git.sel;
        let line = match *row {
            GitRow::Header(sec) => {
                let n = app.git.files(sec).len();
                let label = match sec {
                    Section::Staged => "STAGED",
                    Section::Changes => "CHANGES",
                };
                let mut spans = vec![Span::styled(
                    format!("{label} ({n})"),
                    Style::default()
                        .fg(Color::Gray)
                        .add_modifier(Modifier::BOLD),
                )];
                if sec == Section::Changes
                    && n == 0
                    && app.git.status.is_some()
                    && app.git.files(Section::Staged).is_empty()
                {
                    spans.push(Span::styled(
                        "  nothing to commit",
                        Style::default().fg(DIM),
                    ));
                }
                Line::from(spans)
            }
            GitRow::File(sec, idx) => {
                let Some(file) = app.git.file(idx) else {
                    continue;
                };
                let letter = match sec {
                    Section::Staged => file.staged,
                    Section::Changes => file.unstaged,
                }
                .unwrap_or(' ');
                let (dir, name) = match file.path.rsplit_once('/') {
                    Some((d, n)) => (format!(" {d}"), n.to_string()),
                    None => (String::new(), file.path.clone()),
                };
                let mut spans = vec![Span::raw("  "), status_letter(letter), Span::raw(" ")];
                let name_style = if file.conflicted() {
                    Style::default().fg(MAGENTA)
                } else {
                    Style::default()
                };
                spans.push(Span::styled(name, name_style));
                if let Some(orig) = &file.orig_path {
                    spans.push(Span::styled(format!(" ← {orig}"), Style::default().fg(DIM)));
                }
                spans.push(Span::styled(dir, Style::default().fg(DIM)));
                Line::from(spans)
            }
        };
        let style = if selected && (focused || app.git.visible) {
            let base = Style::default().bg(SEL_BG);
            if focused {
                base.add_modifier(Modifier::BOLD)
            } else {
                base
            }
        } else {
            Style::default()
        };
        f.render_widget(
            Paragraph::new(line).style(style),
            Rect {
                y,
                height: 1,
                ..list
            },
        );
    }

    // Hints.
    let hints_y = inner.y + inner.height - 2;
    let key =
        |k: &'static str| Span::styled(k, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let dim = |t: &'static str| Span::styled(t, Style::default().fg(DIM));
    let l1 = Line::from(vec![
        key("space"),
        dim(" stage  "),
        key("⏎"),
        dim(" diff  "),
        key("c"),
        dim(" commit"),
    ]);
    let l2 = if focused {
        Line::from(vec![
            key("a"),
            dim("/"),
            key("u"),
            dim(" all  "),
            key("x"),
            dim(" discard  "),
            key("esc"),
            dim(" back"),
        ])
    } else {
        Line::from(vec![key("g"), dim(" focus panel")])
    };
    f.render_widget(
        Paragraph::new(vec![l1, l2]),
        Rect {
            y: hints_y,
            height: 2,
            ..inner
        },
    );
}

fn draw_diff(f: &mut Frame, title: &str, lines: &[String], scroll: usize, area: Rect) {
    let r = Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(3),
    };
    let inner = modal(f, r, title);
    let h = inner.height.saturating_sub(1) as usize;
    let body: Vec<Line> = lines
        .iter()
        .skip(scroll)
        .take(h)
        .map(|l| {
            let style = if l.starts_with("+++")
                || l.starts_with("---")
                || l.starts_with("diff ")
                || l.starts_with("index ")
            {
                Style::default().fg(DIM)
            } else if l.starts_with('+') {
                Style::default().fg(GREEN)
            } else if l.starts_with('-') {
                Style::default().fg(RED)
            } else if l.starts_with("@@") {
                Style::default().fg(BLUE)
            } else {
                Style::default()
            };
            Line::from(Span::styled(l.replace('\t', "    "), style))
        })
        .collect();
    f.render_widget(
        Paragraph::new(body),
        Rect {
            height: h as u16,
            ..inner
        },
    );
    let pos = format!(
        "j/k scroll · space/u page · g/G top/bottom · esc close   {}/{}",
        (scroll + 1).min(lines.len().max(1)),
        lines.len()
    );
    f.render_widget(
        Paragraph::new(pos).style(Style::default().fg(DIM)),
        Rect {
            y: inner.y + inner.height.saturating_sub(1),
            height: 1,
            ..inner
        },
    );
}
