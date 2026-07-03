//! Single reusable frame buffer + full-frame paint. Spec §11: clear, never
//! free; no per-cell/per-row allocation. Row-dirty granularity, render
//! cache, and style runs arrive in Stage 4 — Stage 0 repaints the whole
//! frame into the one buffer.

use std::io::Write;

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

pub struct View<'a> {
    pub cols: u16,
    pub rows: u16,
    pub tabs: &'a [String],
    pub active_tab: usize,
    pub cursor: (u16, u16), // 0-based (x, y) within the editor area
}

pub fn paint(f: &mut FrameBuf, v: &View) {
    let b = &mut f.bytes;
    b.clear();
    b.extend_from_slice(b"\x1b[?25l\x1b[H");

    // tabline (row 1), inverse video
    b.extend_from_slice(b"\x1b[7m");
    let mut col = 0usize;
    for (i, name) in v.tabs.iter().enumerate() {
        // ponytail: byte-width truncation; real width calc is the Stage 4
        // render pipeline's job and tab names here are display-only
        let label_len = name.len() + 4; // " name  " incl. active markers
        if col + label_len > v.cols as usize {
            break;
        }
        if i == v.active_tab {
            let _ = write!(b, "\x1b[1m [{}] \x1b[22m", name);
        } else {
            let _ = write!(b, "  {}  ", name);
        }
        col += label_len;
    }
    pad(b, v.cols as usize - col.min(v.cols as usize));
    b.extend_from_slice(b"\x1b[0m");

    // editor area (rows 2..rows-1): empty buffer in Stage 0
    let editor_rows = v.rows.saturating_sub(2);
    for r in 0..editor_rows {
        let _ = write!(b, "\x1b[{};1H\x1b[2K", r + 2);
    }

    // statusline (last row), inverse video
    let _ = write!(b, "\x1b[{};1H\x1b[7m", v.rows);
    let name = v.tabs.get(v.active_tab).map(String::as_str).unwrap_or("untitled");
    let mut status = String::new(); // ponytail: one small alloc per paint, gone in Stage 4
    let _ = write!(status, " {}  Ln {}, Col {}  Ctrl+Q quit", name, v.cursor.1 + 1, v.cursor.0 + 1);
    status.truncate(v.cols as usize);
    b.extend_from_slice(status.as_bytes());
    pad(b, (v.cols as usize).saturating_sub(status.len()));
    b.extend_from_slice(b"\x1b[0m");

    // native cursor into the editor area
    let _ = write!(b, "\x1b[{};{}H\x1b[?25h", v.cursor.1 + 2, v.cursor.0 + 1);
}

fn pad(b: &mut Vec<u8>, n: usize) {
    for _ in 0..n {
        b.push(b' ');
    }
}

use std::fmt::Write as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_reuses_buffer_and_places_cursor() {
        let mut f = FrameBuf::new();
        let tabs = vec!["a.txt".to_string(), "b.txt".to_string()];
        let v = View { cols: 40, rows: 10, tabs: &tabs, active_tab: 0, cursor: (3, 2) };
        paint(&mut f, &v);
        let cap = f.bytes.capacity();
        let s = String::from_utf8_lossy(&f.bytes).to_string();
        assert!(s.contains("[a.txt]"));
        assert!(s.contains("Ln 3, Col 4"));
        assert!(s.ends_with("\x1b[4;4H\x1b[?25h")); // cursor row 2+2, col 3+1
        paint(&mut f, &v);
        assert_eq!(f.bytes.capacity(), cap, "paint must not grow/free the buffer");
    }
}
