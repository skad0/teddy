//! Streaming literal search + cooperative replace-all (spec §10).
//! No regex, no persistent index; bounded work slices driven by the
//! event loop so multi-GB scans never block input.

use crate::buffer::Buffer;

pub enum Step {
    Found(u64),
    Running,
    NotFound,
}

pub struct Search {
    pub needle: Vec<u8>,
    pos: u64,
    /// Where the search started; wrap stops here.
    origin: u64,
    wrapped: bool,
    wrap: bool,
}

impl Search {
    pub fn new(needle: Vec<u8>, from: u64) -> Self {
        Search { needle, pos: from, origin: from, wrapped: false, wrap: true }
    }

    /// Forward-only search: replace flows must never wrap back into
    /// already-processed (or out-of-scope) bytes.
    pub fn new_no_wrap(needle: Vec<u8>, from: u64) -> Self {
        Search { wrap: false, ..Search::new(needle, from) }
    }

    pub fn pos(&self) -> u64 {
        self.pos
    }

    /// Percent of the file scanned since origin (for the statusline).
    pub fn progress(&self, len: u64) -> u64 {
        if len == 0 {
            return 100;
        }
        let scanned = if self.wrapped { len - self.origin + self.pos } else { self.pos - self.origin };
        scanned * 100 / len
    }

    /// Scan up to `budget` bytes. Cancel-safe: state lives in `self`.
    pub fn step(&mut self, buf: &mut Buffer, budget: u64, scratch: &mut Vec<u8>) -> Step {
        if self.needle.is_empty() {
            return Step::NotFound;
        }
        let len = buf.len();
        let n = self.needle.len() as u64;
        let mut spent = 0u64;
        while spent < budget {
            let limit = if self.wrapped { self.origin + n - 1 } else { len };
            if self.pos >= limit {
                if self.wrapped || !self.wrap {
                    return Step::NotFound;
                }
                self.wrapped = true;
                self.pos = 0;
                if self.origin == 0 {
                    return Step::NotFound;
                }
                continue;
            }
            let slice = (64 * 1024).min(budget - spent).max(n);
            // overlap by needle-1 so matches straddling slices are seen
            let want = (slice + n - 1).min(limit.saturating_sub(self.pos)).min(len - self.pos);
            if want < n {
                if self.wrapped || !self.wrap {
                    return Step::NotFound;
                }
                self.wrapped = true;
                self.pos = 0;
                if self.origin == 0 {
                    return Step::NotFound;
                }
                continue;
            }
            scratch.clear();
            buf.read_range(self.pos, want, scratch);
            if let Some(i) = find(scratch, &self.needle) {
                let at = self.pos + i as u64;
                self.pos = at + 1; // next step resumes past this match
                return Step::Found(at);
            }
            self.pos += want - (n - 1);
            spent += want;
        }
        Step::Running
    }
}

/// Naive substring find. ponytail: literal-only core search; the window is
/// ≤64 KiB so worst case stays bounded.
pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        match hay[i..].iter().position(|&b| b == first) {
            Some(off) => {
                i += off;
                if i + needle.len() > hay.len() {
                    return None;
                }
                if &hay[i..i + needle.len()] == needle {
                    return Some(i);
                }
                i += 1;
            }
            None => return None,
        }
    }
    None
}

pub struct ReplaceAll {
    search: Search,
    replacement: Vec<u8>,
    /// Exclusive end bound (selection scope), shifted as edits land.
    end: u64,
    pub count: u64,
    group: u64,
}

impl ReplaceAll {
    pub fn new(needle: Vec<u8>, replacement: Vec<u8>, from: u64, end: u64, group: u64) -> Self {
        ReplaceAll { search: Search::new_no_wrap(needle, from), replacement, end, count: 0, group }
    }

    pub fn progress(&self, len: u64) -> u64 {
        self.search.progress(len)
    }

    /// Replace matches within budget. Returns true when finished.
    pub fn step(&mut self, buf: &mut Buffer, budget: u64, scratch: &mut Vec<u8>) -> Result<bool, &'static str> {
        let mut spent = 0u64;
        while spent < budget {
            if self.search.pos() >= self.end {
                return Ok(true); // scope exhausted: don't scan to EOF
            }
            match self.search.step(buf, budget - spent, scratch) {
                Step::Found(at) => {
                    let n = self.search.needle.len() as u64;
                    if at + n > self.end {
                        return Ok(true); // past scope
                    }
                    buf.replace(at, at + n, &self.replacement, self.group)?;
                    let delta = self.replacement.len() as i64 - n as i64;
                    self.end = (self.end as i64 + delta) as u64;
                    // resume after the inserted replacement (no rescan of it)
                    self.search.pos = at + self.replacement.len() as u64;
                    self.count += 1;
                    spent += n;
                }
                Step::Running => return Ok(false),
                Step::NotFound => return Ok(true),
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp(name: &str, data: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!("teddy-searchtest-{}-{}", std::process::id(), name));
        std::fs::write(&p, data).unwrap();
        p
    }

    fn run_search(buf: &mut Buffer, needle: &[u8], from: u64) -> Option<u64> {
        let mut s = Search::new(needle.to_vec(), from);
        let mut scratch = Vec::new();
        loop {
            match s.step(buf, 8 * 1024, &mut scratch) {
                Step::Found(at) => return Some(at),
                Step::NotFound => return None,
                Step::Running => {}
            }
        }
    }

    #[test]
    fn find_basics() {
        assert_eq!(find(b"hello world", b"world"), Some(6));
        assert_eq!(find(b"hello", b"hello"), Some(0));
        assert_eq!(find(b"hello", b"x"), None);
        assert_eq!(find(b"aaab", b"aab"), Some(1));
        assert_eq!(find(b"", b"a"), None);
    }

    #[test]
    fn streaming_search_across_slices_and_wrap() {
        // needle placed to straddle a 64 KiB slice boundary
        let mut data = vec![b'.'; 200_000];
        let at = 64 * 1024 - 2;
        data[at..at + 6].copy_from_slice(b"NEEDLE");
        data[10..16].copy_from_slice(b"NEEDLE");
        let p = temp("stream", &data);
        let mut b = Buffer::open(&p).unwrap();
        assert_eq!(run_search(&mut b, b"NEEDLE", 0), Some(10));
        assert_eq!(run_search(&mut b, b"NEEDLE", 11), Some(at as u64));
        // wrap: searching from past both finds the first again
        assert_eq!(run_search(&mut b, b"NEEDLE", (at + 10) as u64), Some(10));
        assert_eq!(run_search(&mut b, b"MISSING", 0), None);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn search_sees_unsaved_edits() {
        let p = temp("edits", b"aaaa");
        let mut b = Buffer::open(&p).unwrap();
        b.replace(2, 2, b"XYZ", 1).unwrap();
        assert_eq!(run_search(&mut b, b"XYZ", 0), Some(2));
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn replace_all_with_length_change() {
        let p = temp("repl", b"cat dog cat bird cat");
        let mut b = Buffer::open(&p).unwrap();
        let len = b.len();
        let mut job = ReplaceAll::new(b"cat".to_vec(), b"horse".to_vec(), 0, len, 1);
        let mut scratch = Vec::new();
        while !job.step(&mut b, 1024, &mut scratch).unwrap() {}
        assert_eq!(job.count, 3);
        let mut out = Vec::new();
        let len = b.len();
        b.read_range(0, len, &mut out);
        assert_eq!(out, b"horse dog horse bird horse");
        // one undo group restores everything
        assert!(b.undo_group());
        out.clear();
        let len = b.len();
        b.read_range(0, len, &mut out);
        assert_eq!(out, b"cat dog cat bird cat");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn replace_all_replacement_contains_needle() {
        let p = temp("selfref", b"x x x");
        let mut b = Buffer::open(&p).unwrap();
        let len = b.len();
        let mut job = ReplaceAll::new(b"x".to_vec(), b"xx".to_vec(), 0, len, 1);
        let mut scratch = Vec::new();
        while !job.step(&mut b, 1024, &mut scratch).unwrap() {}
        assert_eq!(job.count, 3);
        let mut out = Vec::new();
        let len = b.len();
        b.read_range(0, len, &mut out);
        assert_eq!(out, b"xx xx xx");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn replace_all_never_wraps_before_scope() {
        // match exists only BEFORE the scope start: must not be touched
        let p = temp("nowrap", b"a zzz");
        let mut b = Buffer::open(&p).unwrap();
        let mut job = ReplaceAll::new(b"a".to_vec(), b"B".to_vec(), 2, 5, 1);
        let mut scratch = Vec::new();
        while !job.step(&mut b, 1024, &mut scratch).unwrap() {}
        assert_eq!(job.count, 0);
        let mut out = Vec::new();
        let len = b.len();
        b.read_range(0, len, &mut out);
        assert_eq!(out, b"a zzz");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn replace_all_scoped_to_selection() {
        let p = temp("scoped", b"a a a a");
        let mut b = Buffer::open(&p).unwrap();
        let mut job = ReplaceAll::new(b"a".to_vec(), b"B".to_vec(), 2, 5, 1);
        let mut scratch = Vec::new();
        while !job.step(&mut b, 1024, &mut scratch).unwrap() {}
        let mut out = Vec::new();
        let len = b.len();
        b.read_range(0, len, &mut out);
        assert_eq!(out, b"a B B a");
        std::fs::remove_file(&p).unwrap();
    }
}
