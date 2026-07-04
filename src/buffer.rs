//! Buffer: one open file — piece chain + original + add store + newline
//! index + cursor/viewport state. Byte-addressed throughout (spec §5–§7).

use crate::lines::LineIndex;
use crate::storage::{AddStore, OriginalFile, Piece, PieceChain, Src};
use std::io;
use std::path::{Path, PathBuf};

pub const HUGE_THRESHOLD: u64 = 256 * 1024 * 1024;
const BINARY_SNIFF: usize = 8 * 1024;
/// Byte window used to reconstruct rows in huge/unindexed files.
pub const HUGE_WINDOW: u64 = 64 * 1024;

/// Cooperative newline-index construction (spec §5.4 progressive open):
/// the viewport shows immediately via the byte-window path; Ln/Col UX
/// becomes exact once the build finishes.
pub struct IndexBuild {
    pub pos: u64,
    newlines: Vec<u64>,
}

/// Files above this build their newline index cooperatively.
const SYNC_INDEX_MAX: u64 = 8 * 1024 * 1024;

pub struct UndoEntry {
    pub start: u64,
    pub new_len: u64,
    pub old_pieces: Vec<Piece>,
    pub old_len: u64,
    pub cursor_before: u64,
    pub group: u64,
    pub byte_cost: usize,
    // content-state identity: undo/redo walk these so `modified` stays
    // exact across save/undo/redo/divergence (spec §8)
    pub before_id: u64,
    pub after_id: u64,
}

pub struct Buffer {
    pub original: Option<OriginalFile>,
    pub adds: AddStore,
    pub chain: PieceChain,
    pub line_index: Option<LineIndex>,
    pub index_build: Option<IndexBuild>,
    pub revision: u64,
    pub path: Option<PathBuf>,
    pub name: String,
    pub readonly: bool,
    pub huge: bool,
    pub binary: bool,
    pub io_error: bool,
    /// Follow mode (spec §13): external changes hard-reload and pin to EOF.
    pub follow: bool,
    /// External change seen while the buffer had unsaved edits (spec §13:
    /// core records change metadata; merge plugins own anything richer).
    pub external_change: bool,
    // cursor/viewport
    pub cursor: u64, // byte offset
    pub goal_col: usize,
    pub top_line: u64, // normal scroll position
    pub top_byte: u64, // huge scroll position (line start or 0)
    pub left_col: usize,
    pub sel_anchor: Option<u64>,
    // undo/redo
    pub undo: Vec<UndoEntry>,
    pub redo: Vec<UndoEntry>,
    pub undo_bytes: usize,
    pub group_counter: u64,
    state_id: u64,
    saved_state_id: u64,
    next_state_id: u64,
    /// (len, mtime) of the file on disk at open/save, for the save guard.
    disk_state: Option<(u64, std::time::SystemTime)>,
    // reusable scratch (spec §4: no per-keypress allocation in hot paths)
    parts_scratch: Vec<(Src, u64, u64)>,
    small_scratch: Vec<u8>,
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
            b.line_index = Some(LineIndex::from_vec(Vec::new()));
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
        b.disk_state = b.original.as_ref().map(|o| (o.len, o.mtime));
        b.binary = binary;
        b.readonly = binary; // explicit force-edit mode is a later stage
        b.huge = huge;
        if !huge {
            if len <= SYNC_INDEX_MAX {
                b.build_line_index()?;
            } else {
                b.index_build = Some(IndexBuild { pos: 0, newlines: Vec::new() });
            }
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
                Some(LineIndex::from_vec(Vec::new()))
            } else {
                None
            },
            index_build: None,
            revision: 0,
            path,
            name,
            readonly: false,
            huge: false,
            binary: false,
            io_error: false,
            follow: false,
            external_change: false,
            cursor: 0,
            goal_col: 0,
            top_line: 0,
            top_byte: 0,
            left_col: 0,
            sel_anchor: None,
            undo: Vec::new(),
            redo: Vec::new(),
            undo_bytes: 0,
            group_counter: 0,
            state_id: 0,
            saved_state_id: 0,
            next_state_id: 1,
            disk_state: None,
            parts_scratch: Vec::new(),
            small_scratch: Vec::new(),
        }
    }

    pub fn modified(&self) -> bool {
        self.state_id != self.saved_state_id
    }

    /// Normalized non-empty selection range.
    pub fn selection(&self) -> Option<(u64, u64)> {
        let a = self.sel_anchor?;
        if a == self.cursor {
            None
        } else {
            Some((a.min(self.cursor), a.max(self.cursor)))
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
        let mut parts = std::mem::take(&mut self.parts_scratch);
        parts.clear();
        self.chain.for_range(start, len, |p, off, take| {
            parts.push((p.src, p.start + off, take));
        });
        for &(src, s, l) in &parts {
            let r = match src {
                Src::Orig => {
                    self.original.as_mut().expect("orig piece without file").read_into(s, l, out)
                }
                _ => self.adds.read_into(src, s, l, out),
            };
            if r.is_err() {
                self.io_error = true;
                break;
            }
        }
        self.parts_scratch = parts;
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
        self.line_index = Some(LineIndex::from_vec(newlines));
        Ok(())
    }

    /// Stat confirmation (spec §13): true when the file on disk no longer
    /// matches the recorded (len, mtime) basis. A missing file counts as
    /// changed; a buffer without one recorded (untitled/new) never does.
    pub fn disk_changed(&self) -> bool {
        let (Some(path), Some((len, mtime))) = (self.path.as_ref(), self.disk_state) else {
            return false;
        };
        match std::fs::metadata(path) {
            Ok(m) => m.len() != len || m.modified().ok() != Some(mtime),
            Err(_) => true,
        }
    }

    /// Reopen from disk (spec §13 reload): the content basis changed, so
    /// the piece chain, add store, undo/redo, and index are rebuilt from
    /// the new inode. Cursor clamps (EOF in follow mode); the view
    /// rescrolls on the next paint.
    pub fn reload(&mut self) -> io::Result<()> {
        let path = self.path.clone().ok_or_else(|| io::Error::other("buffer has no file"))?;
        let mut nb = Buffer::open(&path)?;
        nb.follow = self.follow;
        nb.readonly = nb.readonly || self.follow;
        nb.cursor = if self.follow { nb.len() } else { self.cursor.min(nb.len()) };
        *self = nb;
        Ok(())
    }

    // ------------------------------------------------------------ lines

    pub fn line_count(&self) -> u64 {
        match &self.line_index {
            Some(ix) => ix.count() as u64 + 1,
            None => 1,
        }
    }

    /// Byte offset where `line` starts (0-based). Requires index.
    pub fn line_start(&self, line: u64) -> u64 {
        let ix = self.line_index.as_ref().expect("line_start without index");
        if line == 0 {
            0
        } else {
            ix.get((line - 1) as usize) + 1
        }
    }

    /// Line end (exclusive of the '\n', or buffer end on the last line).
    pub fn line_end(&self, line: u64) -> u64 {
        let ix = self.line_index.as_ref().expect("line_end without index");
        ix.get_opt(line as usize).unwrap_or(self.len())
    }

    pub fn line_of_byte(&self, byte: u64) -> u64 {
        let ix = self.line_index.as_ref().expect("line_of_byte without index");
        ix.rank(byte) as u64
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
        let before_id = self.state_id;
        self.state_id = self.next_state_id;
        self.next_state_id += 1;
        self.push_undo(UndoEntry {
            start,
            new_len: bytes.len() as u64,
            old_pieces: removed,
            old_len,
            cursor_before,
            group,
            byte_cost,
            before_id,
            after_id: self.state_id,
        });
        self.redo.clear();
        self.revision += 1;
        if self.index_build.is_some() {
            // ponytail: an edit invalidates the partial scan; restart —
            // build slices are fast and edits-during-open are rare
            self.index_build = Some(IndexBuild { pos: 0, newlines: Vec::new() });
        }
        Ok(())
    }

    /// Advance the cooperative index build. Returns true when finished.
    pub fn step_index_build(&mut self, budget: u64, scratch: &mut Vec<u8>) -> bool {
        let Some(mut ib) = self.index_build.take() else { return true };
        let len = self.len();
        let mut spent = 0u64;
        while spent < budget && ib.pos < len {
            scratch.clear();
            let take = (256 * 1024).min(len - ib.pos).min(budget - spent);
            self.read_range(ib.pos, take, scratch);
            if self.io_error {
                // never install an index built from failed reads; the
                // buffer stays NOIDX and the statusline shows IOERR
                return true;
            }
            for (i, &b) in scratch.iter().enumerate() {
                if b == b'\n' {
                    ib.newlines.push(ib.pos + i as u64);
                }
            }
            ib.pos += take;
            spent += take;
        }
        if ib.pos >= len {
            self.line_index = Some(LineIndex::from_vec(ib.newlines));
            true
        } else {
            self.index_build = Some(ib);
            false
        }
    }

    /// Undo the most recent group. Returns false if nothing to undo.
    pub fn undo_group(&mut self) -> bool {
        self.walk_history(true)
    }

    pub fn redo_group(&mut self) -> bool {
        self.walk_history(false)
    }

    fn walk_history(&mut self, undo: bool) -> bool {
        let group = {
            let stack = if undo { &self.undo } else { &self.redo };
            match stack.last() {
                Some(e) => e.group,
                None => return false,
            }
        };
        loop {
            let e = {
                let stack = if undo { &mut self.undo } else { &mut self.redo };
                match stack.last() {
                    Some(e) if e.group == group => stack.pop().unwrap(),
                    _ => break,
                }
            };
            if undo {
                self.undo_bytes -= e.byte_cost;
            }
            let removed = self.chain.replace(e.start, e.start + e.new_len, &e.old_pieces);
            self.reindex_piece_replace(e.start, e.start + e.new_len, e.old_len);
            let inverse = UndoEntry {
                start: e.start,
                new_len: e.old_len,
                old_pieces: removed,
                old_len: e.new_len,
                cursor_before: self.cursor,
                group: e.group,
                byte_cost: e.byte_cost,
                before_id: e.after_id,
                after_id: e.before_id,
            };
            if undo {
                self.redo.push(inverse);
            } else {
                self.undo_bytes += inverse.byte_cost;
                self.undo.push(inverse);
            }
            self.cursor = e.cursor_before.min(self.len());
            self.state_id = e.before_id;
            self.revision += 1;
        }
        if self.index_build.is_some() {
            // undo/redo mutates content like any edit: restart the scan
            self.index_build = Some(IndexBuild { pos: 0, newlines: Vec::new() });
        }
        self.sel_anchor = None;
        true
    }

    /// Patch or rebuild the newline index after a piece-based replacement
    /// of [start, old_end) with `new_len` bytes.
    fn reindex_piece_replace(&mut self, start: u64, old_end: u64, new_len: u64) {
        if self.line_index.is_none() {
            return;
        }
        if new_len > 4 * 1024 * 1024 {
            // ponytail: undoing a multi-MB delete rescans the whole file;
            // both paths read the restored bytes anyway
            let _ = self.build_line_index();
            return;
        }
        let mut restored = Vec::with_capacity(new_len as usize);
        self.read_range(start, new_len, &mut restored);
        self.patch_line_index(start, old_end, &restored);
    }

    // ---------------------------------------------------------------- save

    /// Atomic save: temp file + rename (spec §9.1). The pre-save fd stays
    /// open, pinning the old inode, so Orig pieces and the undo stack keep
    /// reading correct bytes after rename.
    pub fn save(&mut self, force: bool) -> io::Result<()> {
        use std::io::Write as _;
        let path = self.path.clone().ok_or_else(|| io::Error::other("buffer has no filename"))?;
        // spec §9.1: symlinks followed — write through to the target so a
        // rename never replaces the symlink itself
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if !force {
            if let (Some((len, mtime)), Ok(meta)) = (self.disk_state, std::fs::metadata(&path)) {
                if meta.len() != len || meta.modified().ok() != Some(mtime) {
                    return Err(io::Error::other("file changed on disk"));
                }
            }
        }
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let (tmp, mut f) = {
            let mut attempt = 0u32;
            loop {
                let cand = dir.join(format!(".{}.teddy-{}-{}", self.name, std::process::id(), attempt));
                // create_new: never truncate a file someone else placed here
                match std::fs::File::options().write(true).create_new(true).open(&cand) {
                    Ok(f) => break (cand, f),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < 16 => attempt += 1,
                    Err(e) => return Err(e),
                }
            }
        };
        if let Ok(meta) = std::fs::metadata(&path) {
            let _ = f.set_permissions(meta.permissions()); // preserve mode
        }
        let _ = f.try_lock(); // brief advisory lock (spec §9.1)
        let write_all = (|| -> io::Result<()> {
            let len = self.len();
            let mut buf = Vec::with_capacity(256 * 1024);
            let mut pos = 0u64;
            while pos < len {
                buf.clear();
                let take = (256 * 1024).min(len - pos);
                self.read_range(pos, take, &mut buf);
                if self.io_error {
                    return Err(io::Error::other("read failed during save"));
                }
                f.write_all(&buf)?;
                pos += take;
            }
            f.sync_all()
        })();
        if let Err(e) = write_all {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        drop(f);
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        let meta = std::fs::metadata(&path)?;
        self.disk_state = Some((meta.len(), meta.modified()?));
        self.saved_state_id = self.state_id;
        self.external_change = false;
        Ok(())
    }

    /// Point the buffer at a new path and save there (palette save-as).
    pub fn save_as(&mut self, path: PathBuf) -> io::Result<()> {
        self.name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        self.path = Some(path);
        self.disk_state = None; // new target: no stale-guard basis yet
        self.save(true)
    }

    fn push_undo(&mut self, e: UndoEntry) {
        self.undo_bytes += e.byte_cost;
        self.undo.push(e);
        const UNDO_CAP: usize = 8 * 1024 * 1024; // spec §8: fixed byte cap
        // evict whole groups: dropping half a group would leave undo_group
        // restoring a corrupted intermediate state
        while self.undo_bytes > UNDO_CAP && self.undo.len() > 1 {
            let victim_group = self.undo[0].group;
            while self.undo.len() > 1 && self.undo[0].group == victim_group {
                let dropped = self.undo.remove(0); // ponytail: O(n) shift, undo depth is small
                self.undo_bytes -= dropped.byte_cost;
            }
        }
    }

    fn patch_line_index(&mut self, start: u64, end: u64, inserted: &[u8]) {
        let Some(ix) = self.line_index.as_mut() else { return };
        let delta = inserted.len() as i64 - (end - start) as i64;
        let fresh: Vec<u64> = inserted
            .iter()
            .enumerate()
            .filter(|(_, &b)| b == b'\n')
            .map(|(i, _)| start + i as u64)
            .collect();
        ix.patch(start, end, &fresh, delta);
    }

    // ------------------------------------------------------------- cursor

    /// Bytes of one small probe window around a byte offset (reused buffer).
    fn probe(&mut self, start: u64, len: u64) -> Vec<u8> {
        let mut v = std::mem::take(&mut self.small_scratch);
        v.clear();
        self.read_range(start, len, &mut v);
        v
    }

    fn probe_done(&mut self, v: Vec<u8>) {
        self.small_scratch = v;
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
        self.probe_done(w);
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
        let mut result = pos - 1;
        for k in 1..=w.len() {
            let s = &w[w.len() - k..];
            if std::str::from_utf8(s).is_ok() {
                result = pos - k as u64;
                break;
            }
            if k > 1 && (s[0] & 0xc0) != 0x80 {
                break; // lead byte of an invalid sequence: single-byte step
            }
        }
        self.probe_done(w);
        result
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
    fn disk_changed_and_reload() {
        let p = temp("reload", b"old contents\n");
        let mut b = Buffer::open(&p).unwrap();
        b.cursor = 12;
        assert!(!b.disk_changed());
        std::fs::write(&p, b"new\n").unwrap(); // len differs: mtime-proof
        assert!(b.disk_changed());
        b.replace(0, 0, b"x", 1).unwrap(); // dirty edit survives until reload
        b.reload().unwrap();
        assert!(!b.disk_changed());
        assert!(!b.modified());
        assert_eq!(b.cursor, 4, "cursor clamped to new len");
        let mut out = Vec::new();
        b.read_range(0, 4, &mut out);
        assert_eq!(out, b"new\n");
        // follow mode pins to EOF
        b.follow = true;
        b.cursor = 0;
        std::fs::write(&p, b"new\nmore\n").unwrap();
        b.reload().unwrap();
        assert_eq!(b.cursor, 9);
        assert!(b.readonly, "follow implies read-only");
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
        let patched = b.line_index.as_ref().unwrap().to_vec();
        b.build_line_index().unwrap();
        assert_eq!(patched, b.line_index.as_ref().unwrap().to_vec());
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

    fn contents(b: &mut Buffer) -> Vec<u8> {
        let mut out = Vec::new();
        let len = b.len();
        b.read_range(0, len, &mut out);
        out
    }

    #[test]
    fn undo_redo_invertibility_randomized() {
        let p = temp("undo", b"the quick brown fox jumps over the lazy dog\nsecond line\n");
        let mut b = Buffer::open(&p).unwrap();
        let initial = contents(&mut b);
        let mut seed = 0x12345u64;
        let mut rnd = move |m: usize| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize % m
        };
        let mut snapshots = vec![initial.clone()];
        for g in 0..40u64 {
            let len = b.len() as usize;
            let a = rnd(len + 1);
            let e = (a + rnd(6)).min(len);
            let ins: Vec<u8> = (0..rnd(5)).map(|i| b'a' + ((i + g as usize) % 26) as u8).collect();
            b.replace(a as u64, e as u64, &ins, g).unwrap();
            snapshots.push(contents(&mut b));
        }
        // undo all the way down, checking every intermediate state
        for i in (0..40).rev() {
            assert!(b.undo_group());
            assert_eq!(contents(&mut b), snapshots[i], "undo to state {i}");
        }
        assert!(!b.undo_group());
        assert!(!b.modified(), "fully undone == unmodified");
        // redo all the way up
        for i in 1..=40 {
            assert!(b.redo_group());
            assert_eq!(contents(&mut b), snapshots[i], "redo to state {i}");
        }
        assert!(!b.redo_group());
        // line index stays consistent throughout
        let patched = b.line_index.as_ref().unwrap().to_vec();
        b.build_line_index().unwrap();
        assert_eq!(patched, b.line_index.as_ref().unwrap().to_vec());
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn grouped_typing_undo() {
        let p = temp("group", b"");
        let mut b = Buffer::open(&p).unwrap();
        for (i, ch) in [b"h", b"e", b"y"].iter().enumerate() {
            b.replace(i as u64, i as u64, *ch, 7).unwrap(); // one group
        }
        b.replace(3, 3, b"!", 8).unwrap(); // new group
        assert!(b.undo_group());
        assert_eq!(contents(&mut b), b"hey");
        assert!(b.undo_group());
        assert_eq!(contents(&mut b), b"");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn save_round_trip_byte_identical() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 256) as u8).map(|b| if b == b'\0' { b'x' } else { b }).collect();
        let p = temp("roundtrip", &data);
        let mut b = Buffer::open(&p).unwrap();
        b.replace(500, 600, b"REPLACED", 1).unwrap();
        b.replace(0, 0, b"HEAD", 2).unwrap();
        let want = contents(&mut b);
        b.save(false).unwrap();
        assert!(!b.modified());
        let on_disk = std::fs::read(&p).unwrap();
        assert_eq!(on_disk, want, "disk bytes == buffer bytes");
        // bytes outside the edits are untouched: HEAD + data[..500] + REPLACED + data[600..]
        assert_eq!(&on_disk[4 + 500 + 8..], &data[600..]);
        assert_eq!(&on_disk[4..504], &data[..500]);
        // buffer still reads correctly after rename (old fd pinned)
        let mut out = Vec::new();
        b.read_range(0, 10, &mut out);
        assert_eq!(out, &want[..10]);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn save_guard_detects_external_change() {
        let p = temp("guard", b"original\n");
        let mut b = Buffer::open(&p).unwrap();
        b.replace(0, 0, b"mine: ", 1).unwrap();
        // external writer changes the file (different size)
        std::fs::write(&p, b"someone else was here\n").unwrap();
        let e = b.save(false).unwrap_err();
        assert_eq!(e.to_string(), "file changed on disk");
        b.save(true).unwrap(); // force overwrites
        assert_eq!(std::fs::read(&p).unwrap(), b"mine: original\n");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn modified_tracks_divergence_after_save() {
        let p = temp("diverge", b"base");
        let mut b = Buffer::open(&p).unwrap();
        b.replace(0, 0, b"A", 1).unwrap();
        b.save(false).unwrap();
        assert!(!b.modified());
        b.undo_group();
        assert!(b.modified(), "undone past save point");
        b.replace(0, 0, b"B", 2).unwrap(); // diverge from saved state
        assert!(b.modified());
        std::fs::remove_file(&p).unwrap();
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
