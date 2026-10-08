//! crossterm key events → bytes for the PTY child. Two dialects: legacy xterm
//! and kitty CSI-u (when the child pushed keyboard flags — Claude Code does,
//! which is how Shift+Enter reaches it). Adapted from Nebula's `keys.rs` (MIT).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

const DISAMBIGUATE: u8 = 0x1;
const REPORT_ALL: u8 = 0x8;

pub fn encode_key(key: &KeyEvent, kitty_flags: u8, app_cursor: bool) -> Option<Vec<u8>> {
    if kitty_flags & DISAMBIGUATE != 0 {
        encode_kitty(key, kitty_flags)
    } else {
        encode_legacy(key, app_cursor)
    }
}

fn kitty_mods(m: KeyModifiers) -> u8 {
    let mut b = 0;
    if m.contains(KeyModifiers::SHIFT) {
        b |= 1;
    }
    if m.contains(KeyModifiers::ALT) {
        b |= 2;
    }
    if m.contains(KeyModifiers::CONTROL) {
        b |= 4;
    }
    if m.contains(KeyModifiers::SUPER) {
        b |= 8;
    }
    if m.contains(KeyModifiers::HYPER) {
        b |= 16;
    }
    if m.contains(KeyModifiers::META) {
        b |= 32;
    }
    b
}

fn csi_u(cp: u32, bits: u8) -> Vec<u8> {
    if bits == 0 {
        format!("\x1b[{cp}u").into_bytes()
    } else {
        format!("\x1b[{cp};{}u", bits as u32 + 1).into_bytes()
    }
}

fn csi_func(f: char, bits: u8) -> Vec<u8> {
    if bits == 0 {
        format!("\x1b[{f}").into_bytes()
    } else {
        format!("\x1b[1;{}{f}", bits as u32 + 1).into_bytes()
    }
}

fn csi_tilde(n: u8, bits: u8) -> Vec<u8> {
    if bits == 0 {
        format!("\x1b[{n}~").into_bytes()
    } else {
        format!("\x1b[{n};{}~", bits as u32 + 1).into_bytes()
    }
}

fn encode_kitty(key: &KeyEvent, flags: u8) -> Option<Vec<u8>> {
    let mut bits = kitty_mods(key.modifiers);
    let all = flags & REPORT_ALL != 0;
    Some(match key.code {
        KeyCode::Char(c) => {
            let base = if c.is_uppercase() {
                bits |= 1;
                c.to_lowercase().next().unwrap_or(c)
            } else {
                c
            };
            if bits & !1 == 0 && !all {
                c.to_string().into_bytes()
            } else {
                csi_u(base as u32, bits)
            }
        }
        KeyCode::Esc => csi_u(27, bits),
        KeyCode::Enter if bits == 0 && !all => b"\r".to_vec(),
        KeyCode::Enter => csi_u(13, bits),
        KeyCode::Tab if bits == 0 && !all => b"\t".to_vec(),
        KeyCode::Tab => csi_u(9, bits),
        KeyCode::BackTab => csi_u(9, bits | 1),
        KeyCode::Backspace if bits == 0 && !all => vec![0x7f],
        KeyCode::Backspace => csi_u(127, bits),
        KeyCode::Up => csi_func('A', bits),
        KeyCode::Down => csi_func('B', bits),
        KeyCode::Right => csi_func('C', bits),
        KeyCode::Left => csi_func('D', bits),
        KeyCode::Home => csi_func('H', bits),
        KeyCode::End => csi_func('F', bits),
        KeyCode::Insert => csi_tilde(2, bits),
        KeyCode::Delete => csi_tilde(3, bits),
        KeyCode::PageUp => csi_tilde(5, bits),
        KeyCode::PageDown => csi_tilde(6, bits),
        KeyCode::F(n @ 1..=4) if bits == 0 => vec![0x1b, b'O', b'P' + (n - 1)],
        KeyCode::F(n @ 1..=4) => {
            format!("\x1b[1;{}{}", bits as u32 + 1, (b'P' + (n - 1)) as char).into_bytes()
        }
        KeyCode::F(n) => csi_tilde(fkey_tilde(n)?, bits),
        _ => return None,
    })
}

fn encode_legacy(key: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    if key
        .modifiers
        .intersects(KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META)
    {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let mut out = Vec::with_capacity(8);
    if alt {
        out.push(0x1b);
    }
    let arrow = |d: char| -> Vec<u8> {
        let m = 1 + (shift as u8) + 2 * (alt as u8) + 4 * (ctrl as u8);
        if m > 1 {
            format!("\x1b[1;{m}{d}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{d}").into_bytes()
        } else {
            format!("\x1b[{d}").into_bytes()
        }
    };
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let b = match c.to_ascii_lowercase() {
                    ch @ 'a'..='z' => (ch as u8) - b'a' + 1,
                    ' ' | '@' | '2' => 0,
                    '[' | '3' => 0x1b,
                    '\\' | '4' => 0x1c,
                    ']' | '5' => 0x1d,
                    '^' | '6' => 0x1e,
                    '_' | '/' | '7' => 0x1f,
                    _ => return None,
                };
                out.push(b);
            } else {
                out.extend_from_slice(c.to_string().as_bytes());
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => out.push(if ctrl { 0x08 } else { 0x7f }),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Up => out.extend(arrow('A')),
        KeyCode::Down => out.extend(arrow('B')),
        KeyCode::Right => out.extend(arrow('C')),
        KeyCode::Left => out.extend(arrow('D')),
        KeyCode::Home => out.extend(arrow('H')),
        KeyCode::End => out.extend(arrow('F')),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        KeyCode::F(n @ 1..=4) => out.extend_from_slice(&[0x1b, b'O', b'P' + (n - 1)]),
        KeyCode::F(n) => out.extend(format!("\x1b[{}~", fkey_tilde(n)?).into_bytes()),
        _ => return None,
    }
    Some(out)
}

fn fkey_tilde(n: u8) -> Option<u8> {
    Some(match n {
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => return None,
    })
}

/// Mouse event for a child that enabled mouse reporting. `col`/`row` are
/// 0-based, relative to the pane.
pub fn encode_mouse(ev: &MouseEvent, col: u16, row: u16, sgr: bool) -> Option<Vec<u8>> {
    let (button, release) = match ev.kind {
        MouseEventKind::Down(b) => (btn(b), false),
        MouseEventKind::Up(b) => (btn(b), true),
        MouseEventKind::Drag(b) => (btn(b) + 32, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        _ => return None,
    };
    let mut cb = button;
    if ev.modifiers.contains(KeyModifiers::SHIFT) {
        cb += 4;
    }
    if ev.modifiers.contains(KeyModifiers::ALT) {
        cb += 8;
    }
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        cb += 16;
    }
    let (x, y) = (col + 1, row + 1);
    if sgr {
        Some(format!("\x1b[<{cb};{x};{y}{}", if release { 'm' } else { 'M' }).into_bytes())
    } else {
        if x > 222 || y > 222 {
            return None;
        }
        let cb = if release { 3 } else { cb };
        Some(vec![
            0x1b,
            b'[',
            b'M',
            32 + cb as u8,
            32 + x as u8,
            32 + y as u8,
        ])
    }
}

fn btn(b: MouseButton) -> u16 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// Parse a binding like `ctrl+q`, `ctrl+alt+x`, `f12`.
pub fn parse_binding(s: &str) -> Option<(KeyModifiers, KeyCode)> {
    let mut mods = KeyModifiers::NONE;
    let mut code = None;
    for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" | "option" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            "esc" => code = Some(KeyCode::Esc),
            p if p.len() > 1 && p.starts_with('f') => code = Some(KeyCode::F(p[1..].parse().ok()?)),
            p if p.chars().count() == 1 => code = p.chars().next().map(KeyCode::Char),
            _ => return None,
        }
    }
    Some((mods, code?))
}

pub fn matches_binding(key: &KeyEvent, binding: &(KeyModifiers, KeyCode)) -> bool {
    let code = match key.code {
        KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
        c => c,
    };
    let mods = key.modifiers - KeyModifiers::SHIFT;
    code == binding.1 && mods == binding.0 - KeyModifiers::SHIFT
}

/// A user key and the built-in key event it stands for.
pub type Remap = Vec<((KeyModifiers, KeyCode), KeyEvent)>;

/// NAV actions and their built-in keys; `[keys.nav]` adds alternatives.
pub const NAV_ACTIONS: &[(&str, &str)] = &[
    ("claude", "c"),
    ("codex", "x"),
    ("shell", "t"),
    ("run", "r"),
    ("restart_run", "R"),
    ("stop_run", "s"),
    ("new_worktree", "n"),
    ("setup", "S"),
    ("remove_worktree", "D"),
    ("add_project", "a"),
    ("remove_project", "X"),
    ("vscode", "e"),
    ("close_tab", "w"),
    ("resume", "u"),
    ("next_attention", "."),
    ("palette", "/"),
    ("help", "?"),
    ("quit", "q"),
    ("quit_stop_daemon", "Q"),
    ("reload", "ctrl+r"),
    ("source_control", "g"),
];

/// A user binding (`"C"`, `"ctrl+o"`, `"f2"`) → the key event of the action's
/// built-in binding, so the NAV handler only ever sees built-in keys.
pub fn nav_remap(overrides: &std::collections::BTreeMap<String, String>) -> (Remap, Vec<String>) {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (action, binding) in overrides {
        let Some((_, default)) = NAV_ACTIONS.iter().find(|(a, _)| a == action) else {
            errors.push(format!("unknown action {action:?} in [keys.nav]"));
            continue;
        };
        let (Some(from), Some(to)) = (parse_case_binding(binding), parse_case_binding(default))
        else {
            errors.push(format!("bad key {binding:?} for {action}"));
            continue;
        };
        out.push((from, KeyEvent::new(to.1, to.0)));
    }
    (out, errors)
}

/// Like `parse_binding` but keeps the case of a single character (so `"R"`
/// is shift+r as crossterm reports it: `Char('R')`).
fn parse_case_binding(s: &str) -> Option<(KeyModifiers, KeyCode)> {
    if s.chars().count() == 1 {
        let c = s.chars().next()?;
        let mods = if c.is_ascii_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        return Some((mods, KeyCode::Char(c)));
    }
    parse_binding(s)
}

/// Does `key` match a case-sensitive binding from `nav_remap`?
pub fn matches_exact(key: &KeyEvent, b: &(KeyModifiers, KeyCode)) -> bool {
    match (key.code, b.1) {
        (KeyCode::Char(a), KeyCode::Char(c)) if b.0 - KeyModifiers::SHIFT == KeyModifiers::NONE => {
            a == c
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        }
        _ => matches_binding(key, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn legacy() {
        assert_eq!(
            encode_key(&k(KeyCode::Char('a'), KeyModifiers::NONE), 0, false).unwrap(),
            b"a"
        );
        assert_eq!(
            encode_key(&k(KeyCode::Char('c'), KeyModifiers::CONTROL), 0, false).unwrap(),
            [3]
        );
        assert_eq!(
            encode_key(&k(KeyCode::Up, KeyModifiers::NONE), 0, false).unwrap(),
            b"\x1b[A"
        );
        assert_eq!(
            encode_key(&k(KeyCode::Up, KeyModifiers::NONE), 0, true).unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            encode_key(&k(KeyCode::Left, KeyModifiers::CONTROL), 0, true).unwrap(),
            b"\x1b[1;5D"
        );
        assert_eq!(
            encode_key(&k(KeyCode::Char('b'), KeyModifiers::ALT), 0, false).unwrap(),
            b"\x1bb"
        );
    }

    #[test]
    fn kitty_shift_enter() {
        assert_eq!(
            encode_key(&k(KeyCode::Enter, KeyModifiers::SHIFT), 1, false).unwrap(),
            b"\x1b[13;2u"
        );
        assert_eq!(
            encode_key(&k(KeyCode::Enter, KeyModifiers::NONE), 1, false).unwrap(),
            b"\r"
        );
        assert_eq!(
            encode_key(&k(KeyCode::Esc, KeyModifiers::NONE), 1, false).unwrap(),
            b"\x1b[27u"
        );
        assert_eq!(
            encode_key(&k(KeyCode::Char('A'), KeyModifiers::SHIFT), 1, false).unwrap(),
            b"A"
        );
    }

    #[test]
    fn mouse() {
        let ev = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(encode_mouse(&ev, 4, 2, true).unwrap(), b"\x1b[<64;5;3M");
    }

    #[test]
    fn remap() {
        let o: std::collections::BTreeMap<String, String> = [
            ("claude".to_string(), "C".to_string()),
            ("palette".to_string(), "ctrl+p".to_string()),
            ("nope".to_string(), "z".to_string()),
        ]
        .into();
        let (m, errs) = nav_remap(&o);
        assert_eq!(errs.len(), 1);
        let shift_c = k(KeyCode::Char('C'), KeyModifiers::SHIFT);
        let hit = m.iter().find(|(b, _)| matches_exact(&shift_c, b)).unwrap();
        assert_eq!(hit.1.code, KeyCode::Char('c'));
        let ctrl_p = k(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert_eq!(
            m.iter()
                .find(|(b, _)| matches_exact(&ctrl_p, b))
                .unwrap()
                .1
                .code,
            KeyCode::Char('/')
        );
        assert!(!m
            .iter()
            .any(|(b, _)| matches_exact(&k(KeyCode::Char('c'), KeyModifiers::NONE), b)));
    }

    #[test]
    fn bindings() {
        let b = parse_binding("ctrl+q").unwrap();
        assert!(matches_binding(
            &k(KeyCode::Char('q'), KeyModifiers::CONTROL),
            &b
        ));
        assert!(matches_binding(
            &k(
                KeyCode::Char('Q'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            &b
        ));
        assert!(!matches_binding(
            &k(KeyCode::Char('q'), KeyModifiers::NONE),
            &b
        ));
        assert_eq!(parse_binding("f12").unwrap().1, KeyCode::F(12));
    }
}
