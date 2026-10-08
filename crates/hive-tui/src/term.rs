//! A session's client-side terminal emulator.

pub const SCROLLBACK_LINES: usize = 10_000;

/// Captures OSC 52 clipboard writes from the child.
#[derive(Default)]
pub struct TermCallbacks {
    pub clipboard: Option<Vec<u8>>,
}

impl vt100::Callbacks for TermCallbacks {
    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, _ty: &[u8], data: &[u8]) {
        self.clipboard = Some(data.to_vec());
    }
}

fn parser(rows: u16, cols: u16) -> vt100::Parser<TermCallbacks> {
    vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK_LINES, TermCallbacks::default())
}

pub struct AttachedTerm {
    pub parser: vt100::Parser<TermCallbacks>,
    /// Seq one past the last byte applied.
    pub next_seq: u64,
    pub kitty_flags: u8,
    /// Got at least one replay since (re)attaching.
    pub synced: bool,
}

impl AttachedTerm {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: parser(rows.max(2), cols.max(2)),
            next_seq: 0,
            synced: false,
            kitty_flags: 0,
        }
    }

    pub fn apply_scrollback(&mut self, base_seq: u64, data: &[u8], kitty_flags: u8) {
        if base_seq != self.next_seq || !self.synced && self.next_seq == 0 {
            let (rows, cols) = self.parser.screen().size();
            self.parser = parser(rows, cols);
        }
        self.parser.process(data);
        // A replay must not re-copy whatever the child copied long ago.
        self.parser.callbacks_mut().clipboard = None;
        self.next_seq = base_seq + data.len() as u64;
        self.kitty_flags = kitty_flags;
        self.synced = true;
    }

    pub fn apply_output(&mut self, seq: u64, data: &[u8]) {
        let end = seq + data.len() as u64;
        if end <= self.next_seq {
            return;
        }
        let skip = self.next_seq.saturating_sub(seq) as usize;
        self.parser.process(&data[skip..]);
        self.next_seq = end;
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (r, c) = self.parser.screen().size();
        if (r, c) != (rows, cols) {
            self.parser.screen_mut().set_size(rows.max(2), cols.max(2));
        }
    }

    pub fn scroll(&mut self, delta: isize) {
        let cur = self.parser.screen().scrollback() as isize;
        let next = (cur + delta).max(0) as usize;
        self.parser.screen_mut().set_scrollback(next);
    }

    pub fn scroll_to_bottom(&mut self) {
        if self.parser.screen().scrollback() != 0 {
            self.parser.screen_mut().set_scrollback(0);
        }
    }

    /// Clipboard text the child sent via OSC 52 since the last call.
    pub fn take_clipboard(&mut self) -> Option<Vec<u8>> {
        let b64 = self.parser.callbacks_mut().clipboard.take()?;
        decode_base64(&b64)
    }

    pub fn cells(&self) -> usize {
        let (r, c) = self.parser.screen().size();
        r as usize * c as usize
    }
}

fn decode_base64(input: &[u8]) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let clean: Vec<u8> = input
        .iter()
        .copied()
        .filter(|c| !c.is_ascii_whitespace() && *c != b'=')
        .collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let mut acc = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= val(c)? << (18 - 6 * i);
        }
        let bytes = [(acc >> 16) as u8, (acc >> 8) as u8, acc as u8];
        out.extend_from_slice(&bytes[..chunk.len().saturating_sub(1)]);
    }
    Some(out)
}

/// Put text on the system clipboard.
pub fn set_clipboard(text: &[u8]) {
    use std::io::Write;
    let text = text.to_vec();
    std::thread::spawn(move || {
        let child = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn();
        if let Ok(mut c) = child {
            if let Some(stdin) = c.stdin.as_mut() {
                let _ = stdin.write_all(&text);
            }
            let _ = c.wait();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_capture() {
        let mut t = AttachedTerm::new(5, 20);
        t.apply_scrollback(0, b"\x1b]52;c;aGVsbG8gd29ybGQ=\x07", 0);
        assert_eq!(t.take_clipboard(), None, "replays don't copy");
        t.apply_output(t.next_seq, b"\x1b]52;c;aGk=\x07");
        assert_eq!(t.take_clipboard().as_deref(), Some(&b"hi"[..]));
        assert_eq!(decode_base64(b"aGVsbG8gd29ybGQ=").unwrap(), b"hello world");
    }

    #[test]
    fn dedup_and_reset() {
        let mut t = AttachedTerm::new(5, 20);
        t.apply_scrollback(0, b"hello", 0);
        t.apply_output(3, b"lo world");
        assert!(t.parser.screen().contents().starts_with("hello world"));
        assert_eq!(t.next_seq, 11);
        // Delta re-attach continues.
        t.apply_scrollback(11, b"!", 0);
        assert!(t.parser.screen().contents().starts_with("hello world!"));
        // Non-contiguous replay resets.
        t.apply_scrollback(100, b"fresh", 1);
        assert!(t.parser.screen().contents().starts_with("fresh"));
        assert_eq!(t.kitty_flags, 1);
    }
}
