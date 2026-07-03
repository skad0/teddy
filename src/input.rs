//! Byte-stream → key-event parser. Core-owned per spec §12: no terminal
//! input abstraction crates. Mouse/bracketed-paste/kitty probing arrive
//! with later stages; this handles keys.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(u8), // uppercase ASCII letter, e.g. Ctrl(b'Q')
    Enter,
    Tab,
    Backspace,
    Esc,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    // shift-modified movement (CSI 1;2X) drives selection
    SUp,
    SDown,
    SLeft,
    SRight,
    SHome,
    SEnd,
}

enum Step {
    Key(Key, usize),
    Skip(usize),
    Incomplete,
}

/// Longest escape sequence we wait on before discarding as garbage.
const MAX_PENDING: usize = 32;

pub struct Parser {
    buf: Vec<u8>, // reused; bounded by MAX_PENDING once drained
}

impl Parser {
    pub fn new() -> Self {
        Parser { buf: Vec::with_capacity(64) }
    }

    pub fn has_pending(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Feed raw bytes; decoded keys are pushed into `out` (caller reuses it).
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Key>) {
        self.buf.extend_from_slice(bytes);
        let mut i = 0;
        while i < self.buf.len() {
            match parse_one(&self.buf[i..]) {
                Step::Key(k, n) => {
                    out.push(k);
                    i += n;
                }
                Step::Skip(n) => i += n,
                Step::Incomplete => {
                    if self.buf.len() - i > MAX_PENDING {
                        i += 1; // garbage guard: never wedge on an unterminated sequence
                    } else {
                        break;
                    }
                }
            }
        }
        self.buf.drain(..i);
    }

    /// Called on poll timeout: a lone pending ESC is the Esc key.
    pub fn flush_timeout(&mut self, out: &mut Vec<Key>) {
        if self.buf == [0x1b] {
            out.push(Key::Esc);
            self.buf.clear();
        }
        // ponytail: other incomplete sequences keep waiting; MAX_PENDING
        // already bounds how much garbage we can hold.
    }
}

fn parse_one(b: &[u8]) -> Step {
    match b[0] {
        0x1b => parse_escape(b),
        0x0d | 0x0a => Step::Key(Key::Enter, 1),
        0x09 => Step::Key(Key::Tab, 1),
        0x7f | 0x08 => Step::Key(Key::Backspace, 1),
        c @ 0x01..=0x1a => Step::Key(Key::Ctrl(c + 0x40), 1),
        0x00 | 0x1c..=0x1f => Step::Skip(1),
        0x20..=0x7e => Step::Key(Key::Char(b[0] as char), 1),
        _ => parse_utf8(b),
    }
}

fn parse_escape(b: &[u8]) -> Step {
    match b.get(1) {
        None => Step::Incomplete,
        Some(b'[') => parse_csi(b),
        Some(b'O') => match b.get(2) {
            None => Step::Incomplete,
            Some(fin) => match ss3_key(*fin) {
                Some(k) => Step::Key(k, 3),
                None => Step::Skip(3),
            },
        },
        // ESC + other byte: Alt-chord. ponytail: emit Esc, reparse the rest;
        // Alt bindings land with the keymap work in later stages.
        Some(_) => Step::Key(Key::Esc, 1),
    }
}

fn parse_csi(b: &[u8]) -> Step {
    // b = ESC [ params... final, final byte in 0x40..=0x7e
    for (j, &c) in b.iter().enumerate().skip(2) {
        if (0x40..=0x7e).contains(&c) {
            let params = &b[2..j];
            let n = j + 1;
            // xterm modifier encoding: "1;2" = shift. Other modifiers fall
            // back to the unshifted key; chord bindings are plugin-era work.
            let shifted = params == b"1;2";
            let key = match c {
                b'A' if shifted => Some(Key::SUp),
                b'B' if shifted => Some(Key::SDown),
                b'C' if shifted => Some(Key::SRight),
                b'D' if shifted => Some(Key::SLeft),
                b'H' if shifted => Some(Key::SHome),
                b'F' if shifted => Some(Key::SEnd),
                b'A' => Some(Key::Up),
                b'B' => Some(Key::Down),
                b'C' => Some(Key::Right),
                b'D' => Some(Key::Left),
                b'H' => Some(Key::Home),
                b'F' => Some(Key::End),
                b'~' => match params {
                    b"1" | b"7" => Some(Key::Home),
                    b"4" | b"8" => Some(Key::End),
                    b"1;2" | b"7;2" => Some(Key::SHome),
                    b"4;2" | b"8;2" => Some(Key::SEnd),
                    b"3" => Some(Key::Delete),
                    b"5" => Some(Key::PageUp),
                    b"6" => Some(Key::PageDown),
                    _ => None,
                },
                _ => None,
            };
            return match key {
                Some(k) => Step::Key(k, n),
                None => Step::Skip(n),
            };
        }
    }
    Step::Incomplete
}

fn ss3_key(fin: u8) -> Option<Key> {
    match fin {
        b'A' => Some(Key::Up),
        b'B' => Some(Key::Down),
        b'C' => Some(Key::Right),
        b'D' => Some(Key::Left),
        b'H' => Some(Key::Home),
        b'F' => Some(Key::End),
        _ => None,
    }
}

fn parse_utf8(b: &[u8]) -> Step {
    let len = match b[0] {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return Step::Skip(1), // stray continuation/invalid lead
    };
    if b.len() < len {
        return Step::Incomplete;
    }
    match std::str::from_utf8(&b[..len]) {
        Ok(s) => Step::Key(Key::Char(s.chars().next().unwrap()), len),
        Err(_) => Step::Skip(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Vec<Key> {
        let mut p = Parser::new();
        let mut out = Vec::new();
        p.feed(bytes, &mut out);
        out
    }

    #[test]
    fn ascii_and_ctrl() {
        assert_eq!(parse(b"a"), vec![Key::Char('a')]);
        assert_eq!(parse(b"\x11"), vec![Key::Ctrl(b'Q')]);
        assert_eq!(parse(b"\x13"), vec![Key::Ctrl(b'S')]);
        assert_eq!(parse(b"\r"), vec![Key::Enter]);
        assert_eq!(parse(b"\t"), vec![Key::Tab]);
        assert_eq!(parse(b"\x7f"), vec![Key::Backspace]);
    }

    #[test]
    fn csi_keys() {
        assert_eq!(parse(b"\x1b[A"), vec![Key::Up]);
        assert_eq!(parse(b"\x1b[B\x1b[C\x1b[D"), vec![Key::Down, Key::Right, Key::Left]);
        assert_eq!(parse(b"\x1b[H\x1b[F"), vec![Key::Home, Key::End]);
        assert_eq!(parse(b"\x1b[1~\x1b[4~"), vec![Key::Home, Key::End]);
        assert_eq!(parse(b"\x1b[3~"), vec![Key::Delete]);
        assert_eq!(parse(b"\x1b[5~\x1b[6~"), vec![Key::PageUp, Key::PageDown]);
        assert_eq!(parse(b"\x1bOA"), vec![Key::Up]);
    }

    #[test]
    fn unknown_csi_skipped() {
        assert_eq!(parse(b"\x1b[199~a"), vec![Key::Char('a')]);
    }

    #[test]
    fn utf8_char_and_split_feed() {
        assert_eq!(parse("é".as_bytes()), vec![Key::Char('é')]);
        assert_eq!(parse("🦀".as_bytes()), vec![Key::Char('🦀')]);
        let mut p = Parser::new();
        let mut out = Vec::new();
        let bytes = "é".as_bytes();
        p.feed(&bytes[..1], &mut out);
        assert!(out.is_empty() && p.has_pending());
        p.feed(&bytes[1..], &mut out);
        assert_eq!(out, vec![Key::Char('é')]);
    }

    #[test]
    fn invalid_utf8_skipped() {
        assert_eq!(parse(b"\xff\xfea"), vec![Key::Char('a')]);
        // lead byte with invalid continuation: skip lead, reparse the rest
        assert_eq!(parse(b"\xc2a"), vec![Key::Char('a')]);
    }

    #[test]
    fn split_csi_across_feeds() {
        let mut p = Parser::new();
        let mut out = Vec::new();
        p.feed(b"\x1b[", &mut out);
        assert!(out.is_empty());
        p.feed(b"A", &mut out);
        assert_eq!(out, vec![Key::Up]);
    }

    #[test]
    fn lone_esc_on_timeout() {
        let mut p = Parser::new();
        let mut out = Vec::new();
        p.feed(b"\x1b", &mut out);
        assert!(out.is_empty());
        p.flush_timeout(&mut out);
        assert_eq!(out, vec![Key::Esc]);
    }

    #[test]
    fn garbage_never_wedges() {
        let mut p = Parser::new();
        let mut out = Vec::new();
        let mut junk = vec![0x1b, b'['];
        junk.extend(std::iter::repeat(b'0').take(64)); // unterminated CSI
        p.feed(&junk, &mut out);
        p.feed(b"a", &mut out);
        assert!(out.contains(&Key::Char('a')));
    }
}
