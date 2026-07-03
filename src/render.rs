//! Single reusable frame buffer + paint. Spec §11: clear, never free; no
//! per-cell allocation. Row-dirty granularity and the render cache arrive
//! in S4 — until then every dirty frame repaints fully into the one buffer.
//!
//! Row emission decodes bytes → cells: valid UTF-8 chars (width 1 —
//! ponytail: CJK/wide handling is the S4 width-table work), tabs expand to
//! 8-col stops, and every control/invalid byte renders as \xNN so file
//! content can never inject raw ANSI into the frame.

use std::io::Write as _;

const TAB: usize = 8;

pub struct FrameBuf {
    bytes: Vec<u8>,
}

impl FrameBuf {
    pub fn new() -> Self {
        FrameBuf { bytes: Vec::with_capacity(64 * 1024) }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

pub struct TabInfo<'a> {
    pub name: &'a str,
    pub modified: bool,
}

pub struct View<'a> {
    pub cols: u16,
    pub rows: u16,
    pub tabs: &'a [TabInfo<'a>],
    pub active_tab: usize,
    /// Pre-read bytes of each visible editor row (without trailing \n).
    pub row_bytes: &'a [Vec<u8>],
    /// Selection span per row as byte offsets into that row's bytes.
    pub row_sel: &'a [Option<(usize, usize)>],
    pub left_col: usize,
    /// Screen cursor position, 0-based within the editor area.
    pub cursor_screen: (u16, u16),
    pub status_left: &'a str,
    pub status_right: &'a str,
}

/// One display token decoded from a byte stream.
/// (bytes consumed, cell width, what to emit)
enum Token {
    Char(usize, usize),  // consumed, width (chars/tabs emit source or spaces)
    Escape(u8),          // one invalid/control byte -> "\xNN"
}

fn next_token(bytes: &[u8], cell: usize) -> Token {
    let b = bytes[0];
    match b {
        b'\t' => Token::Char(1, TAB - (cell % TAB)),
        0x20..=0x7e => Token::Char(1, 1),
        0x00..=0x1f | 0x7f => Token::Escape(b),
        _ => {
            let len = match b {
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => return Token::Escape(b),
            };
            match std::str::from_utf8(bytes.get(..len).unwrap_or(&[])) {
                // C1 controls (U+0080..U+009F, e.g. CSI) are as dangerous as
                // C0: escape their bytes, never emit them raw
                Ok(s) if s.chars().next().is_some_and(|c| ('\u{80}'..='\u{9f}').contains(&c)) => {
                    Token::Escape(b)
                }
                Ok(_) => Token::Char(len, 1),
                Err(_) => Token::Escape(b),
            }
        }
    }
}

/// Visual column of byte offset `upto` within a line.
pub fn visual_col(bytes: &[u8], upto: usize) -> usize {
    let mut cell = 0;
    let mut i = 0;
    while i < bytes.len() && i < upto {
        match next_token(&bytes[i..], cell) {
            Token::Char(n, w) => {
                i += n;
                cell += w;
            }
            Token::Escape(_) => {
                i += 1;
                cell += 4;
            }
        }
    }
    cell
}

/// Byte offset within a line whose cell is closest to `goal` (not past it).
pub fn byte_at_col(bytes: &[u8], goal: usize) -> usize {
    let mut cell = 0;
    let mut i = 0;
    while i < bytes.len() {
        let (n, w) = match next_token(&bytes[i..], cell) {
            Token::Char(n, w) => (n, w),
            Token::Escape(_) => (1, 4),
        };
        if cell + w > goal {
            return i;
        }
        i += n;
        cell += w;
    }
    i
}

/// Emit one editor row: skip `left` cells, render at most `width` cells.
/// `sel` is a byte span within `bytes` rendered in reverse video.
fn emit_row(out: &mut Vec<u8>, bytes: &[u8], left: usize, width: usize, sel: Option<(usize, usize)>) {
    let mut cell = 0usize;
    let mut i = 0usize;
    let limit = left + width;
    let mut in_sel = false;
    while i < bytes.len() && cell < limit {
        if let Some((a, b)) = sel {
            if !in_sel && i >= a && i < b {
                out.extend_from_slice(b"\x1b[7m");
                in_sel = true;
            } else if in_sel && i >= b {
                out.extend_from_slice(b"\x1b[27m");
                in_sel = false;
            }
        }
        let (consumed, w, escape) = match next_token(&bytes[i..], cell) {
            Token::Char(n, w) => (n, w, None),
            Token::Escape(b) => (1, 4, Some(b)),
        };
        let vis_from = cell.max(left);
        let vis_to = (cell + w).min(limit);
        if vis_to > vis_from {
            if cell >= left && cell + w <= limit {
                match escape {
                    Some(b) => {
                        let _ = write!(out, "\x1b[2m\\x{:02X}\x1b[22m", b);
                    }
                    None if bytes[i] == b'\t' => {
                        for _ in 0..w {
                            out.push(b' ');
                        }
                    }
                    None => out.extend_from_slice(&bytes[i..i + consumed]),
                }
            } else {
                // token straddles an edge: pad its visible cells
                for _ in vis_from..vis_to {
                    out.push(b' ');
                }
            }
        }
        i += consumed;
        cell += w;
    }
    if in_sel {
        out.extend_from_slice(b"\x1b[27m");
    }
    out.extend_from_slice(b"\x1b[K");
}

pub fn paint(f: &mut FrameBuf, v: &View) {
    let b = &mut f.bytes;
    b.clear();
    b.extend_from_slice(b"\x1b[?25l\x1b[H");

    // tabline (row 1), inverse video
    b.extend_from_slice(b"\x1b[7m");
    let mut col = 0usize;
    for (i, tab) in v.tabs.iter().enumerate() {
        let star = if tab.modified { "*" } else { "" };
        let label_len = tab.name.len() + star.len() + 4;
        if col + label_len > v.cols as usize {
            break;
        }
        if i == v.active_tab {
            let _ = write!(b, "\x1b[1m [{}{}] \x1b[22m", tab.name, star);
        } else {
            let _ = write!(b, "  {}{}  ", tab.name, star);
        }
        col += label_len;
    }
    pad(b, (v.cols as usize).saturating_sub(col));
    b.extend_from_slice(b"\x1b[0m");

    // editor area
    let editor_rows = v.rows.saturating_sub(2) as usize;
    for r in 0..editor_rows {
        let _ = write!(b, "\x1b[{};1H", r + 2);
        match v.row_bytes.get(r) {
            Some(bytes) => {
                emit_row(b, bytes, v.left_col, v.cols as usize, v.row_sel.get(r).copied().flatten())
            }
            None => b.extend_from_slice(b"\x1b[K"),
        }
    }

    // statusline (last row), inverse video
    let _ = write!(b, "\x1b[{};1H\x1b[7m", v.rows);
    let w = v.cols as usize;
    let left = truncated(v.status_left, w);
    b.extend_from_slice(left.as_bytes());
    let right = truncated(v.status_right, w.saturating_sub(left.len()));
    pad(b, w.saturating_sub(left.len() + right.len()));
    b.extend_from_slice(right.as_bytes());
    b.extend_from_slice(b"\x1b[0m");

    // native cursor into the editor area
    let _ = write!(b, "\x1b[{};{}H\x1b[?25h", v.cursor_screen.1 + 2, v.cursor_screen.0 + 1);
}

fn truncated(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

fn pad(b: &mut Vec<u8>, n: usize) {
    for _ in 0..n {
        b.push(b' ');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(bytes: &[u8], left: usize, width: usize) -> String {
        let mut out = Vec::new();
        emit_row(&mut out, bytes, left, width, None);
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn selection_reverse_video() {
        let mut out = Vec::new();
        emit_row(&mut out, b"hello", 0, 80, Some((1, 4)));
        let s = String::from_utf8_lossy(&out).into_owned();
        assert_eq!(s, "h\x1b[7mell\x1b[27mo\x1b[K");
        out.clear();
        emit_row(&mut out, b"ab", 0, 80, Some((1, 2))); // sel to end of row
        let s = String::from_utf8_lossy(&out).into_owned();
        assert_eq!(s, "a\x1b[7mb\x1b[27m\x1b[K");
    }

    #[test]
    fn plain_ascii_row() {
        assert_eq!(row(b"hello", 0, 80), "hello\x1b[K");
    }

    #[test]
    fn invalid_bytes_escaped() {
        let r = row(b"a\xffb", 0, 80);
        assert!(r.contains("\\xFF"), "{r:?}");
        assert!(r.starts_with('a'));
    }

    #[test]
    fn control_bytes_escaped_no_ansi_injection() {
        let r = row(b"x\x1b[31mred", 0, 80);
        assert!(r.contains("\\x1B"), "{r:?}");
        // the ESC from file content must not survive raw (only our own SGR)
        assert!(!r.contains("\x1b[31m"), "{r:?}");
    }

    #[test]
    fn c1_control_via_utf8_escaped() {
        // U+009B (CSI) as valid UTF-8 must not reach the terminal raw
        let r = row(b"x\xc2\x9b31mred", 0, 80);
        assert!(!r.contains('\u{9b}'), "{r:?}");
        assert!(r.contains("\\xC2") && r.contains("\\x9B"), "{r:?}");
    }

    #[test]
    fn tab_expansion() {
        let r = row(b"a\tb", 0, 80);
        assert_eq!(r, format!("a{}b\x1b[K", " ".repeat(7)));
    }

    #[test]
    fn horizontal_clip() {
        assert_eq!(row(b"0123456789", 3, 4), "3456\x1b[K");
    }

    #[test]
    fn wide_token_straddles_left_edge() {
        // tab spans cells 0..8; left=4 shows its tail as spaces
        let r = row(b"\tX", 4, 10);
        assert_eq!(r, "    X\x1b[K");
    }

    #[test]
    fn cols_helpers_roundtrip() {
        let line = "a\tb\u{e9}c".as_bytes(); // a TAB b é c
        assert_eq!(visual_col(line, 0), 0);
        assert_eq!(visual_col(line, 1), 1); // after 'a'
        assert_eq!(visual_col(line, 2), 8); // after tab
        assert_eq!(visual_col(line, 3), 9); // after 'b'
        assert_eq!(visual_col(line, 5), 10); // after 'é' (2 bytes)
        assert_eq!(byte_at_col(line, 8), 2);
        assert_eq!(byte_at_col(line, 9), 3);
        assert_eq!(byte_at_col(line, 4), 1); // middle of tab -> tab start
        assert_eq!(byte_at_col(line, 99), line.len());
    }

    #[test]
    fn paint_smoke() {
        let mut f = FrameBuf::new();
        let tabs = [TabInfo { name: "a.txt", modified: true }];
        let rows = vec![b"line one".to_vec(), b"line two".to_vec()];
        let sels = vec![None; rows.len()];
        let v = View {
            cols: 40,
            rows: 10,
            tabs: &tabs,
            active_tab: 0,
            row_bytes: &rows,
            row_sel: &sels,
            left_col: 0,
            cursor_screen: (2, 1),
            status_left: " a.txt *",
            status_right: "Ln 2, Col 3 ",
        };
        paint(&mut f, &v);
        let s = String::from_utf8_lossy(f.as_bytes()).into_owned();
        assert!(s.contains("[a.txt*]"));
        assert!(s.contains("line one"));
        assert!(s.ends_with("\x1b[3;3H\x1b[?25h"));
    }
}
