//! Terminal-query handling done in the daemon, tmux-style: the child talks to
//! a virtual terminal, so nobody else would answer its queries.
//!
//! Tracks the kitty keyboard flag stack (`CSI > f u` push, `CSI < n u` pop,
//! `CSI = f ; m u` set), and answers `CSI ? u`, DA1 (`CSI c`), DSR (`CSI 5 n`)
//! and cursor position reports (`CSI 6 n`, filled in by the caller from its
//! screen). Adapted from Nebula's `pty/kitty.rs` (MIT).

const ESC: u8 = 0x1b;
const MAX_STACK: usize = 32;
const MAX_PARAMS: usize = 16;

#[derive(Debug, PartialEq)]
pub enum Reply {
    Bytes(Vec<u8>),
    /// Cursor position as of `at` bytes into the fed chunk.
    CursorPosition {
        at: usize,
    },
}

#[derive(Debug, Default, PartialEq)]
pub struct ScanActions {
    pub replies: Vec<Reply>,
    pub flags_changed: Option<u8>,
}

#[derive(Debug, Clone, Copy)]
enum State {
    Ground,
    Esc,
    Csi { poisoned: bool },
}

pub struct KittyScanner {
    state: State,
    params: Vec<u8>,
    stack: Vec<u8>,
}

impl Default for KittyScanner {
    fn default() -> Self {
        Self {
            state: State::Ground,
            params: Vec::new(),
            stack: Vec::new(),
        }
    }
}

impl KittyScanner {
    pub fn flags(&self) -> u8 {
        self.stack.last().copied().unwrap_or(0)
    }

    pub fn feed(&mut self, data: &[u8]) -> ScanActions {
        let mut actions = ScanActions::default();
        let before = self.flags();
        for (i, &b) in data.iter().enumerate() {
            self.step(b, i + 1, &mut actions);
        }
        if self.flags() != before {
            actions.flags_changed = Some(self.flags());
        }
        actions
    }

    fn step(&mut self, b: u8, end: usize, actions: &mut ScanActions) {
        match self.state {
            State::Ground => {
                if b == ESC {
                    self.state = State::Esc;
                }
            }
            State::Esc => {
                if b == b'[' {
                    self.params.clear();
                    self.state = State::Csi { poisoned: false };
                } else {
                    if b == b'c' {
                        // RIS resets keyboard modes too.
                        self.stack.clear();
                    }
                    self.state = if b == ESC { State::Esc } else { State::Ground };
                }
            }
            State::Csi { poisoned } => match b {
                0x30..=0x3F => {
                    if self.params.len() < MAX_PARAMS {
                        self.params.push(b);
                    } else {
                        self.state = State::Csi { poisoned: true };
                    }
                }
                0x20..=0x2F => self.state = State::Csi { poisoned: true },
                0x40..=0x7E => {
                    if !poisoned {
                        self.dispatch(b, end, actions);
                    }
                    self.state = State::Ground;
                }
                _ => self.state = State::Ground,
            },
        }
    }

    fn dispatch(&mut self, fin: u8, end: usize, actions: &mut ScanActions) {
        let params = std::mem::take(&mut self.params);
        let reply = |actions: &mut ScanActions, bytes: &[u8]| match actions.replies.last_mut() {
            Some(Reply::Bytes(p)) => p.extend_from_slice(bytes),
            _ => actions.replies.push(Reply::Bytes(bytes.to_vec())),
        };
        match (fin, params.split_first()) {
            (b'u', Some((b'?', []))) => {
                reply(actions, format!("\x1b[?{}u", self.flags()).as_bytes())
            }
            (b'u', Some((b'>', rest))) => {
                let f = parse_num(rest).unwrap_or(0) as u8;
                if self.stack.len() >= MAX_STACK {
                    self.stack.remove(0);
                }
                self.stack.push(f);
            }
            (b'u', Some((b'<', rest))) => {
                let n = parse_num(rest).unwrap_or(1).max(1) as usize;
                for _ in 0..n {
                    if self.stack.pop().is_none() {
                        break;
                    }
                }
            }
            (b'u', Some((b'=', rest))) => {
                let mut it = rest.split(|&c| c == b';');
                let f = it.next().and_then(parse_num).unwrap_or(0) as u8;
                let mode = it.next().and_then(parse_num).unwrap_or(1);
                let cur = self.flags();
                let new = match mode {
                    2 => cur | f,
                    3 => cur & !f,
                    _ => f,
                };
                match self.stack.last_mut() {
                    Some(top) => *top = new,
                    None => self.stack.push(new),
                }
            }
            // DA1: claim a VT220-ish terminal.
            (b'c', None) | (b'c', Some((b'0', []))) => reply(actions, b"\x1b[?62;22c"),
            (b'n', Some((b'5', []))) => reply(actions, b"\x1b[0n"),
            (b'n', Some((b'6', []))) => actions.replies.push(Reply::CursorPosition { at: end }),
            _ => {}
        }
    }
}

fn parse_num(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_query_pop() {
        let mut k = KittyScanner::default();
        let a = k.feed(b"\x1b[>1u\x1b[?u");
        assert_eq!(a.flags_changed, Some(1));
        assert_eq!(a.replies, vec![Reply::Bytes(b"\x1b[?1u".to_vec())]);
        let a = k.feed(b"\x1b[<u");
        assert_eq!(a.flags_changed, Some(0));
    }

    #[test]
    fn split_sequences_and_da1_order() {
        let mut k = KittyScanner::default();
        assert!(k.feed(b"\x1b[?").replies.is_empty());
        let a = k.feed(b"u\x1b[c");
        assert_eq!(
            a.replies,
            vec![Reply::Bytes(b"\x1b[?0u\x1b[?62;22c".to_vec())]
        );
    }

    #[test]
    fn cursor_report_position() {
        let mut k = KittyScanner::default();
        let a = k.feed(b"ab\x1b[6nxy");
        assert_eq!(a.replies, vec![Reply::CursorPosition { at: 6 }]);
    }

    #[test]
    fn set_mode() {
        let mut k = KittyScanner::default();
        k.feed(b"\x1b[=5;1u");
        assert_eq!(k.flags(), 5);
        k.feed(b"\x1b[=1;3u");
        assert_eq!(k.flags(), 4);
    }
}
