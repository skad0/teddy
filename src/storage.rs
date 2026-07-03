//! S1 byte storage (spec §5, §7): pread-backed chunk cache for the
//! original file, spillable add store for inserted bytes, chunked piece
//! chain, and byte-range replace with inverse-piece capture for undo.
//! Original file bytes never live in RAM beyond the bounded cache.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const CHUNK_SIZE: u64 = 64 * 1024;
const CHUNK_CACHE_CAP: usize = 32; // ≤ 2 MiB resident original bytes
const ADD_MEM_CAP: usize = 8 * 1024 * 1024; // spill inserted bytes past this
const PIECES_PER_CHUNK: usize = 256;

// ---------------------------------------------------------------- original

pub struct OriginalFile {
    file: File,
    pub path: PathBuf,
    pub len: u64,
    pub mtime: SystemTime,
    pub size_at_load: u64,
    cache: HashMap<u64, (u64, Vec<u8>)>, // chunk index -> (lru tick, bytes)
    tick: u64,
}

impl OriginalFile {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let meta = file.metadata()?;
        Ok(OriginalFile {
            file,
            path: path.to_path_buf(),
            len: meta.len(),
            mtime: meta.modified()?,
            size_at_load: meta.len(),
            cache: HashMap::new(),
            tick: 0,
        })
    }

    fn chunk(&mut self, idx: u64) -> io::Result<&[u8]> {
        self.tick += 1;
        if !self.cache.contains_key(&idx) {
            if self.cache.len() >= CHUNK_CACHE_CAP {
                // ponytail: O(cap) eviction scan over ≤32 entries; a real
                // LRU list only if the cap ever grows large
                if let Some((&victim, _)) = self.cache.iter().min_by_key(|(_, (t, _))| *t) {
                    self.cache.remove(&victim);
                }
            }
            let start = idx * CHUNK_SIZE;
            let want = (CHUNK_SIZE.min(self.len.saturating_sub(start))) as usize;
            let mut buf = vec![0u8; want];
            read_exact_at(&self.file, &mut buf, start)?;
            self.cache.insert(idx, (self.tick, buf));
        }
        let entry = self.cache.get_mut(&idx).unwrap();
        entry.0 = self.tick;
        Ok(&entry.1)
    }

    /// Append `len` bytes starting at `start` to `out`, crossing chunks.
    pub fn read_into(&mut self, mut start: u64, mut len: u64, out: &mut Vec<u8>) -> io::Result<()> {
        len = len.min(self.len.saturating_sub(start));
        while len > 0 {
            let idx = start / CHUNK_SIZE;
            let off = (start - idx * CHUNK_SIZE) as usize;
            let chunk = self.chunk(idx)?;
            let take = ((chunk.len() - off) as u64).min(len) as usize;
            out.extend_from_slice(&chunk[off..off + take]);
            start += take as u64;
            len -= take as u64;
        }
        Ok(())
    }
}

fn read_exact_at(f: &File, mut buf: &mut [u8], mut off: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match f.read_at(buf, off) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// --------------------------------------------------------------- add store

pub struct AddStore {
    mem: Vec<u8>,
    spill: Option<File>, // unlinked temp file; cleaned up by the OS
    spill_len: u64,
}

impl AddStore {
    pub fn new() -> Self {
        AddStore { mem: Vec::new(), spill: None, spill_len: 0 }
    }

    /// Store bytes, returning the piece source + start for them.
    pub fn push(&mut self, bytes: &[u8]) -> io::Result<(Src, u64)> {
        if self.mem.len() + bytes.len() <= ADD_MEM_CAP {
            let start = self.mem.len() as u64;
            self.mem.extend_from_slice(bytes);
            return Ok((Src::AddMem, start));
        }
        if self.spill.is_none() {
            let path = std::env::temp_dir()
                .join(format!("teddy-spill-{}-{:x}", std::process::id(), self as *const _ as usize));
            let f = File::options().read(true).write(true).create_new(true).open(&path)?;
            let _ = std::fs::remove_file(&path); // unlink-while-open: auto-cleanup
            self.spill = Some(f);
        }
        let spill = self.spill.as_ref().unwrap();
        let start = self.spill_len;
        spill.write_all_at(bytes, start)?;
        self.spill_len += bytes.len() as u64;
        Ok((Src::AddSpill, start))
    }

    pub fn read_into(&self, src: Src, start: u64, len: u64, out: &mut Vec<u8>) -> io::Result<()> {
        match src {
            Src::AddMem => {
                out.extend_from_slice(&self.mem[start as usize..(start + len) as usize]);
                Ok(())
            }
            Src::AddSpill => {
                let at = out.len();
                out.resize(at + len as usize, 0);
                read_exact_at(self.spill.as_ref().expect("spill piece without spill file"), &mut out[at..], start)
            }
            Src::Orig => unreachable!("orig bytes live in OriginalFile"),
        }
    }
}

// -------------------------------------------------------------- piece chain

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Src {
    Orig,
    AddMem,
    AddSpill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece {
    pub src: Src,
    pub start: u64,
    pub len: u64,
}

struct PieceGroup {
    pieces: Vec<Piece>, // ≤ PIECES_PER_CHUNK after repack
    bytes: u64,         // sum of piece lens
}

/// Chunked piece chain (spec §5.2). Two-level: linear scan over groups by
/// byte sums, then linear scan inside one group. Not a rope on purpose.
pub struct PieceChain {
    groups: Vec<PieceGroup>,
    pub total_len: u64,
}

impl PieceChain {
    /// Chain representing the whole original file (or empty).
    pub fn for_original(len: u64) -> Self {
        let groups = if len == 0 {
            Vec::new()
        } else {
            vec![PieceGroup { pieces: vec![Piece { src: Src::Orig, start: 0, len }], bytes: len }]
        };
        PieceChain { groups, total_len: len }
    }

    /// Replace byte range [start, end) with `new`, returning the exact
    /// pieces that were removed (boundary-split) — the undo inverse.
    pub fn replace(&mut self, start: u64, end: u64, new: &[Piece]) -> Vec<Piece> {
        assert!(start <= end && end <= self.total_len, "range out of bounds");

        // locate affected group span [g0, g1]
        let (mut g0, mut off) = (0usize, start);
        while g0 < self.groups.len() && off > self.groups[g0].bytes {
            off -= self.groups[g0].bytes;
            g0 += 1;
        }
        if g0 == self.groups.len() {
            // insertion at very end of empty/exhausted chain
            g0 = self.groups.len().saturating_sub(1);
        }
        let mut g1 = g0;
        let mut remaining = end - start + off; // end offset relative to group g0 start
        while g1 < self.groups.len() && remaining > self.groups[g1].bytes {
            remaining -= self.groups[g1].bytes;
            g1 += 1;
        }
        if g1 == self.groups.len() {
            g1 = self.groups.len().saturating_sub(1);
        }

        // flatten affected groups, splice in flat piece space
        // ponytail: temp Vec sized by affected pieces only; fine for the
        // locked sparse-edit model
        let mut flat: Vec<Piece> = Vec::new();
        if !self.groups.is_empty() {
            for g in &self.groups[g0..=g1] {
                flat.extend_from_slice(&g.pieces);
            }
        }
        let flat_start = off; // start offset within flat
        let flat_end = off + (end - start);

        let mut removed = Vec::new();
        let mut rebuilt: Vec<Piece> = Vec::with_capacity(flat.len() + new.len() + 2);
        let mut pos = 0u64;
        for p in &flat {
            let p_end = pos + p.len;
            // part before the range
            if pos < flat_start {
                let keep = (flat_start - pos).min(p.len);
                rebuilt.push(Piece { src: p.src, start: p.start, len: keep });
            }
            // part inside the range
            let cut_lo = flat_start.max(pos);
            let cut_hi = flat_end.min(p_end);
            if cut_lo < cut_hi {
                removed.push(Piece { src: p.src, start: p.start + (cut_lo - pos), len: cut_hi - cut_lo });
            }
            // part after the range
            if p_end > flat_end {
                let tail = (p_end - flat_end).min(p.len);
                let skip = p.len - tail;
                rebuilt.push(Piece { src: p.src, start: p.start + skip, len: tail });
            }
            pos = p_end;
        }
        // find insert position in rebuilt: sum of kept-before bytes == flat_start
        let mut acc = 0u64;
        let mut ins = rebuilt.len();
        for (i, p) in rebuilt.iter().enumerate() {
            if acc >= flat_start {
                ins = i;
                break;
            }
            acc += p.len;
        }
        if acc < flat_start {
            ins = rebuilt.len();
        }
        for (k, p) in new.iter().filter(|p| p.len > 0).enumerate() {
            rebuilt.insert(ins + k, *p);
        }
        coalesce(&mut rebuilt);

        // repack into groups of ≤ PIECES_PER_CHUNK
        let mut packed: Vec<PieceGroup> = rebuilt
            .chunks(PIECES_PER_CHUNK)
            .map(|c| PieceGroup { pieces: c.to_vec(), bytes: c.iter().map(|p| p.len).sum() })
            .collect();
        if self.groups.is_empty() {
            self.groups = packed;
        } else {
            self.groups.splice(g0..=g1, packed.drain(..));
        }
        self.groups.retain(|g| !g.pieces.is_empty());

        let new_bytes: u64 = new.iter().map(|p| p.len).sum();
        self.total_len = self.total_len - (end - start) + new_bytes;
        removed
    }

    /// Visit (piece, piece-local start, len) covering [start, start+len).
    pub fn for_range(&self, mut start: u64, mut len: u64, mut f: impl FnMut(&Piece, u64, u64)) {
        for g in &self.groups {
            if len == 0 {
                return;
            }
            if start >= g.bytes {
                start -= g.bytes;
                continue;
            }
            for p in &g.pieces {
                if len == 0 {
                    return;
                }
                if start >= p.len {
                    start -= p.len;
                    continue;
                }
                let take = (p.len - start).min(len);
                f(p, start, take);
                start = 0;
                len -= take;
            }
        }
    }

    #[cfg(test)]
    fn piece_count(&self) -> usize {
        self.groups.iter().map(|g| g.pieces.len()).sum()
    }
}

fn coalesce(pieces: &mut Vec<Piece>) {
    pieces.retain(|p| p.len > 0);
    let mut w = 0usize;
    for r in 1..pieces.len() {
        let prev = pieces[w];
        let cur = pieces[r];
        if prev.src == cur.src && prev.start + prev.len == cur.start {
            pieces[w].len += cur.len;
        } else {
            w += 1;
            pieces[w] = cur;
        }
    }
    pieces.truncate(if pieces.is_empty() { 0 } else { w + 1 });
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference model: a plain Vec<u8> mirrors every chain op.
    struct Model {
        chain: PieceChain,
        adds: AddStore,
        mirror: Vec<u8>,
        orig: Vec<u8>,
    }

    impl Model {
        fn new(orig: &[u8]) -> Self {
            Model {
                chain: PieceChain::for_original(orig.len() as u64),
                adds: AddStore::new(),
                mirror: orig.to_vec(),
                orig: orig.to_vec(),
            }
        }

        fn replace(&mut self, start: u64, end: u64, bytes: &[u8]) -> Vec<Piece> {
            let new: Vec<Piece> = if bytes.is_empty() {
                Vec::new()
            } else {
                let (src, s) = self.adds.push(bytes).unwrap();
                vec![Piece { src, start: s, len: bytes.len() as u64 }]
            };
            let removed = self.chain.replace(start, end, &new);
            self.mirror.splice(start as usize..end as usize, bytes.iter().copied());
            removed
        }

        fn contents(&self) -> Vec<u8> {
            let mut out = Vec::new();
            self.chain.for_range(0, self.chain.total_len, |p, off, len| match p.src {
                Src::Orig => out
                    .extend_from_slice(&self.orig[(p.start + off) as usize..(p.start + off + len) as usize]),
                _ => self.adds.read_into(p.src, p.start + off, len, &mut out).unwrap(),
            });
            out
        }

        fn check(&self) {
            assert_eq!(self.contents(), self.mirror);
            assert_eq!(self.chain.total_len, self.mirror.len() as u64);
        }
    }

    #[test]
    fn insert_middle_start_end() {
        let mut m = Model::new(b"hello world");
        m.replace(5, 5, b",");
        m.check();
        m.replace(0, 0, b">> ");
        m.check();
        let end = m.chain.total_len;
        m.replace(end, end, b"!");
        m.check();
        assert_eq!(m.contents(), b">> hello, world!");
    }

    #[test]
    fn delete_and_replace_across_pieces() {
        let mut m = Model::new(b"aaabbbccc");
        m.replace(3, 3, b"XY"); // aaaXYbbbccc
        m.check();
        m.replace(2, 7, b""); // spans orig|add|orig boundary: aabbccc? -> aa + bbccc
        m.check();
        m.replace(1, 5, b"ZZZZZZ");
        m.check();
    }

    #[test]
    fn removed_pieces_are_exact_inverse() {
        let mut m = Model::new(b"0123456789");
        let before = m.mirror.clone();
        let removed = m.replace(2, 8, b"ab");
        m.check();
        // undo by hand: replace the inserted range with removed pieces
        m.chain.replace(2, 4, &removed);
        m.mirror = before;
        m.check();
    }

    #[test]
    fn empty_file_and_full_delete() {
        let mut m = Model::new(b"");
        m.replace(0, 0, b"abc");
        m.check();
        m.replace(0, 3, b"");
        m.check();
        assert_eq!(m.chain.total_len, 0);
        m.replace(0, 0, b"x");
        m.check();
    }

    #[test]
    fn coalesce_adjacent_add_pieces() {
        let mut m = Model::new(b"");
        for i in 0..10u8 {
            let end = m.chain.total_len;
            m.replace(end, end, &[b'a' + i]);
        }
        m.check();
        assert_eq!(m.chain.piece_count(), 1, "sequential appends must coalesce");
    }

    #[test]
    fn randomized_against_mirror() {
        // ponytail: tiny deterministic LCG instead of a rand dep
        let mut seed = 0xdeadbeefu64;
        let mut rnd = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        let mut m = Model::new(b"the quick brown fox jumps over the lazy dog");
        for _ in 0..500 {
            let len = m.mirror.len();
            let a = if len == 0 { 0 } else { rnd() % (len + 1) };
            let b = (a + rnd() % 8).min(len);
            let n = rnd() % 6;
            let ins: Vec<u8> = (0..n).map(|i| b'A' + ((rnd() + i) % 26) as u8).collect();
            m.replace(a as u64, b as u64, &ins);
            m.check();
        }
        assert!(m.chain.piece_count() < 4000);
    }

    #[test]
    fn group_repack_stays_bounded() {
        let mut m = Model::new(b"");
        // alternating non-coalescable inserts to force many pieces
        for i in 0..2000usize {
            let at = (i % (m.mirror.len() + 1)) as u64;
            m.replace(at, at, &[b'0' + (i % 10) as u8]);
        }
        m.check();
        for g in &m.chain.groups {
            assert!(g.pieces.len() <= PIECES_PER_CHUNK);
            assert_eq!(g.bytes, g.pieces.iter().map(|p| p.len).sum::<u64>());
        }
    }

    #[test]
    fn original_chunk_cache_reads() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("teddy-test-orig-{}", std::process::id()));
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let mut of = OriginalFile::open(&path).unwrap();
        let mut out = Vec::new();
        of.read_into(60_000, 80_000, &mut out).unwrap(); // crosses chunk boundaries
        assert_eq!(out, &data[60_000..140_000]);
        out.clear();
        of.read_into(199_990, 100, &mut out).unwrap(); // clamped at EOF
        assert_eq!(out, &data[199_990..]);
        assert!(of.cache.len() <= CHUNK_CACHE_CAP);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn add_store_spill() {
        let mut a = AddStore::new();
        let big = vec![7u8; ADD_MEM_CAP]; // fills mem exactly
        let (s1, _) = a.push(&big).unwrap();
        assert_eq!(s1, Src::AddMem);
        let (s2, start2) = a.push(b"after-spill").unwrap();
        assert_eq!(s2, Src::AddSpill);
        let mut out = Vec::new();
        a.read_into(s2, start2, 11, &mut out).unwrap();
        assert_eq!(out, b"after-spill");
    }
}
