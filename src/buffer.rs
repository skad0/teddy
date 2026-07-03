//! Buffer: one open file — piece chain + original + add store + newline
//! index + cursor/viewport state. Byte-addressed throughout (spec §5–§7).

use crate::storage::{AddStore, OriginalFile, Piece, PieceChain, Src};
use std::io;
use std::path::{Path, PathBuf};

pub const HUGE_THRESHOLD: u64 = 256 * 1024 * 1024;
const BINARY_SNIFF: usize = 8 * 1024;
/// Byte window used to reconstruct rows in huge/unindexed files.
pub const HUGE_WINDOW: u64 = 64 * 1024;

pub struct LineIndex {
    /// Byte offset of each '\n', ascending.
    pub newlines: Vec<u64>,
    pub complete: bool,
}

pub struct UndoEntry {
    pub start: u64,
    pub new_len: u64,
    pub old_pieces: Vec<Piece>,
    pub old_len: u64,
    pub cursor_before: u64,
    pub group: u64,
    pub byte_cost: usize,
}

pub struct Buffer {
    pub original: Option<OriginalFile>,
    pub adds: AddStore,
    pub chain: PieceChain,
    pub line_index: Option<LineIndex>,
    pub revision: u64,
    pub path: Option<PathBuf>,
    pub name: String,
    pub modified: bool,
    pub readonly: bool,
    pub huge: bool,
    pub binary: bool,
    pub io_error: bool,
    // cursor/viewport
    pub cursor: u64, // byte offset
    pub goal_col: usize,
    pub top_line: u64, // normal scroll position
    pub top_byte: u64, // huge scroll position (line start or 0)
    pub left_col: usize,
    // undo (S2 wires the keys; inverse capture lives here from S1 on)
    pub undo: Vec<UndoEntry>,
    pub redo: Vec<UndoEntry>,
    pub undo_bytes: usize,
    pub group_counter: u64,
}

impl Buffer {
    pub fn untitled() -> Self {
        Buffer::from_parts(None, None, "untitled".into())
    }

    pub fn open(path: &Path) -> io::Result<Self> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        if !path.exists() {
            // new file: empty buffer, created on save
            let mut b = Buffer::from_parts(None, Some(path.to_path_buf()), name);
            b.line_index = Some(LineIndex { newlines: Vec::new(), complete: true });
            return Ok(b);
        }
        let mut orig = OriginalFile::open(path)?;
        let len = orig.len;
        // binary sniff: NUL in the head → open read-only (spec §5.5)
        let mut head = Vec::new();
        orig.read_into(0, BINARY_SNIFF.min(len as usize) as u64, &mut head)?;
        let binary = head.contains(&0);
        let huge = len >= HUGE_THRESHOLD;

        let mut b = Buffer::from_parts(Some(orig), Some(path.to_path_buf()), name);
        b.binary = binary;
        b.readonly = binary; // explicit force-edit mode is a later stage
        b.huge = huge;
        if !huge {
            b.build_line_index()?; // ponytail: synchronous; cooperative build lands in S3
        }
        Ok(b)
    }

    fn from_parts(original: Option<OriginalFile>, path: Option<PathBuf>, name: String) -> Self {
        let len = original.as_ref().map_or(0, |o| o.len);
        Buffer {
            chain: PieceChain::for_original(len),
            original,
            adds: AddStore::new(),
            line_index: if len == 0 {
                Some(LineIndex { newlines: Vec::new(), complete: true })
            } else {
                None
            },
            revision: 0,
            path,
            name,
            modified: false,
            readonly: false,
            huge: false,
            binary: false,
            io_error: false,
            cursor: 0,
            goal_col: 0,
            top_line: 0,
            top_byte: 0,
            left_col: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            undo_bytes: 0,
            group_counter: 0,
        }
    }

    pub fn len(&self) -> u64 {
        self.chain.total_len
    }

    /// Append [start, start+len) of buffer content to `out`. Read failures
    /// (e.g. the file shrank underneath us) set `io_error` instead of being
    /// silently swallowed; the statusline surfaces IOERR and S5's watcher
    /// path owns recovery.
    pub fn read_range(&mut self, start: u64, len: u64, out: &mut Vec<u8>) {
        // collect piece refs first: for_range borrows chain immutably while
        // reads need &mut original for the chunk cache
        let mut parts: Vec<(Src, u64, u64)> = Vec::new();
        self.chain.for_range(start, len, |p, off, take| {
            parts.push((p.src, p.start + off, take));
        });
        for (src, s, l) in parts {
            let r = match src {
                Src::Orig => {
                    self.original.as_mut().expect("orig piece without file").read_into(s, l, out)
                }
                _ => self.adds.read_into(src, s, l, out),
            };
            if r.is_err() {
                self.io_error = true;
                return;
            }
        }
    }

    fn build_line_index(&mut self) -> io::Result<()> {
        let len = self.len();
        let mut newlines = Vec::new();
        let mut buf = Vec::with_capacity(256 * 1024);
        let mut pos = 0u64;
        while pos < len {
            buf.clear();
            let take = (256 * 1024).min(len - pos);
            self.read_range(pos, take, &mut buf);
            for (i, &b) in buf.iter().enumerate() {
                if b == b'\n' {
                    newlines.push(pos + i as u64);
                }
            }
            pos += take;
            if self.io_error {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        self.line_index = Some(LineIndex { newlines, complete: true });
        Ok(())
    }

    // ------------------------------------------------------------ lines

    pub fn line_count(&self) -> u64 {
        match &self.line_index {
            Some(ix) => ix.newlines.len() as u64 + 1,
            None => 1,
        }
    }

    /// Byte offset where `line` starts (0-based). Requires index.
    pub fn line_start(&self, line: u64) -> u64 {
        let ix = self.line_index.as_ref().expect("line_start without index");
        if line == 0 {
            0
        } else {
            ix.newlines[(line - 1) as usize] + 1
        }
    }

    /// Line end (exclusive of the '\n', or buffer end on the last line).
    pub fn line_end(&self, line: u64) -> u64 {
        let ix = self.line_index.as_ref().expect("line_end without index");
        ix.newlines.get(line as usize).copied().unwrap_or(self.len())
    }

    pub fn line_of_byte(&self, byte: u64) -> u64 {
        let ix = self.line_index.as_ref().expect("line_of_byte without index");
        ix.newlines.partition_point(|&n| n < byte) as u64
    }

    // --------------------------------------------------------- transactions

    /// Validated byte-range replacement (spec §7). Single-edit transactions
    /// are the S1/S2 shape; multi-edit arrives with the plugin host.
    pub fn replace(&mut self, start: u64, end: u64, bytes: &[u8], group: u64) -> Result<(), &'static str> {
        if self.readonly {
            return Err("buffer is read-only");
        }
        if start > end || end > self.len() {
            return Err("edit out of bounds");
        }
        let new: Vec<Piece> = if bytes.is_empty() {
            Vec::new()
        } else {
            let (src, s) = self.adds.push(bytes).map_err(|_| "add store write failed")?;
            vec![Piece { src, start: s, len: bytes.len() as u64 }]
        };
        let cursor_before = self.cursor;
        let removed = self.chain.replace(start, end, &new);
        self.patch_line_index(start, end, bytes);
        let old_len = end - start;
        let byte_cost = removed.len() * std::mem::size_of::<Piece>() + 64;
        self.push_undo(UndoEntry {
            start,
            new_len: bytes.len() as u64,
            old_pieces: removed,
            old_len,
            cursor_before,
            group,
            byte_cost,
        });
        self.redo.clear();
        self.revision += 1;
        self.modified = true;
        Ok(())
    }

    fn push_undo(&mut self, e: UndoEntry) {
        self.undo_bytes += e.byte_cost;
        self.undo.push(e);
        const UNDO_CAP: usize = 8 * 1024 * 1024; // spec §8: fixed byte cap
        while self.undo_bytes > UNDO_CAP && self.undo.len() > 1 {
            let dropped = self.undo.remove(0); // ponytail: O(n) shift, undo depth is small
            self.undo_bytes -= dropped.byte_cost;
        }
    }

    fn patch_line_index(&mut self, start: u64, end: u64, inserted: &[u8]) {
        let Some(ix) = self.line_index.as_mut() else { return };
        let delta = inserted.len() as i64 - (end - start) as i64;
        let lo = ix.newlines.partition_point(|&n| n < start);
        let hi = ix.newlines.partition_point(|&n| n < end);
        let fresh: Vec<u64> = inserted
            .iter()
            .enumerate()
            .filter(|(_, &b)| b == b'\n')
            .map(|(i, _)| start + i as u64)
            .collect();
        let fresh_len = fresh.len();
        ix.newlines.splice(lo..hi, fresh);
        // ponytail: O(lines-after) shift per edit; chunked index if S4
        // profiling shows this dominating on million-line files
        for n in &mut ix.newlines[lo + fresh_len..] {
            *n = (*n as i64 + delta) as u64;
        }
    }

    // ------------------------------------------------------------- cursor

    /// Bytes of one small probe window around a byte offset.
    fn probe(&mut self, start: u64, len: u64) -> Vec<u8> {
        let mut v = Vec::with_capacity(len as usize);
        self.read_range(start, len, &mut v);
        v
    }

    /// Next char boundary after cursor (escaped invalid bytes step 1).
    pub fn next_boundary(&mut self, pos: u64) -> u64 {
        if pos >= self.len() {
            return self.len();
        }
        let w = self.probe(pos, 4.min(self.len() - pos));
        let step = match std::str::from_utf8(&w) {
            Ok(s) => s.chars().next().map_or(1, |c| c.len_utf8()),
            Err(e) if e.valid_up_to() > 0 => {
                std::str::from_utf8(&w[..e.valid_up_to()]).unwrap().chars().next().unwrap().len_utf8()
            }
            Err(_) => 1,
        };
        pos + step as u64
    }

    /// Previous char boundary before pos.
    pub fn prev_boundary(&mut self, pos: u64) -> u64 {
        if pos == 0 {
            return 0;
        }
        let back = 4.min(pos);
        let w = self.probe(pos - back, back);
        // walk back to the last valid boundary in the window
        for k in 1..=w.len() {
            let s = &w[w.len() - k..];
            if std::str::from_utf8(s).is_ok() {
                return pos - k as u64;
            }
            if k > 1 && (s[0] & 0xc0) != 0x80 {
                break; // lead byte of an invalid sequence: single-byte step
            }
        }
        pos - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str, data: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!("teddy-buftest-{}-{}", std::process::id(), name));
        std::fs::write(&p, data).unwrap();
        p
    }

    #[test]
    fn open_and_read() {
        let p = temp("basic", b"one\ntwo\nthree");
        let mut b = Buffer::open(&p).unwrap();
        assert_eq!(b.line_count(), 3);
        assert_eq!(b.line_start(1), 4);
        assert_eq!(b.line_end(1), 7);
        assert_eq!(b.line_of_byte(5), 1);
        let mut out = Vec::new();
        b.read_range(4, 3, &mut out);
        assert_eq!(out, b"two");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn binary_sniff_readonly() {
        let p = temp("bin", b"ab\x00cd");
        let b = Buffer::open(&p).unwrap();
        assert!(b.binary && b.readonly);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn replace_patches_line_index() {
        let p = temp("patch", b"aa\nbb\ncc\n");
        let mut b = Buffer::open(&p).unwrap();
        assert_eq!(b.line_count(), 4);
        b.replace(3, 5, b"x\ny\nz", 1).unwrap(); // "aa\nx\ny\nz\ncc\n"
        let mut out = Vec::new();
        let len = b.len();
        b.read_range(0, len, &mut out);
        assert_eq!(out, b"aa\nx\ny\nz\ncc\n");
        // rebuild index from scratch and compare with patched one
        let patched = b.line_index.as_ref().unwrap().newlines.clone();
        b.build_line_index().unwrap();
        assert_eq!(patched, b.line_index.as_ref().unwrap().newlines);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn readonly_rejects_edit() {
        let p = temp("ro", b"x\x00");
        let mut b = Buffer::open(&p).unwrap();
        assert!(b.replace(0, 0, b"y", 1).is_err());
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn boundaries_utf8_and_invalid() {
        let p = temp("bound", "aé🦀\u{0}".replace('\u{0}', "").as_bytes());
        // content: 'a'(1) 'é'(2) '🦀'(4)
        let mut b = Buffer::open(&p).unwrap();
        assert_eq!(b.next_boundary(0), 1);
        assert_eq!(b.next_boundary(1), 3);
        assert_eq!(b.next_boundary(3), 7);
        assert_eq!(b.prev_boundary(7), 3);
        assert_eq!(b.prev_boundary(3), 1);
        assert_eq!(b.prev_boundary(1), 0);
        std::fs::remove_file(&p).unwrap();

        let p2 = temp("bound2", b"a\xff\xfeb");
        let mut b2 = Buffer::open(&p2).unwrap();
        assert_eq!(b2.next_boundary(1), 2); // invalid byte steps 1
        assert_eq!(b2.prev_boundary(3), 2);
        std::fs::remove_file(&p2).unwrap();
    }

    #[test]
    fn new_file_buffer() {
        let p = std::env::temp_dir().join(format!("teddy-nonexistent-{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let b = Buffer::open(&p).unwrap();
        assert_eq!(b.len(), 0);
        assert_eq!(b.line_count(), 1);
    }
}
