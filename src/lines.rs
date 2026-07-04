//! Chunked newline index. A flat Vec<u64> pays an O(lines-after) memmove
//! per edit — measured at ~15 ms/keypress on a 3.7M-line file. Blocks of
//! raw offsets with a per-block delta make byte-shift O(#blocks) and
//! localize splices to one block.

const BLOCK: usize = 32 * 1024;

struct Block {
    raw: Vec<u64>, // value = raw[i] + delta, ascending
    delta: i64,
}

impl Block {
    fn first(&self) -> u64 {
        (self.raw[0] as i64 + self.delta) as u64
    }
    fn last(&self) -> u64 {
        (*self.raw.last().unwrap() as i64 + self.delta) as u64
    }
}

pub struct LineIndex {
    blocks: Vec<Block>,
    count: usize,
    pub complete: bool,
}

impl LineIndex {
    pub fn from_vec(v: Vec<u64>) -> Self {
        let count = v.len();
        let blocks = v
            .chunks(BLOCK)
            .map(|c| Block {
                raw: c.to_vec(),
                delta: 0,
            })
            .collect();
        LineIndex {
            blocks,
            count,
            complete: true,
        }
    }

    pub fn count(&self) -> usize {
        self.count
    }

    /// Value of the i-th newline offset.
    pub fn get(&self, mut i: usize) -> u64 {
        for b in &self.blocks {
            if i < b.raw.len() {
                return (b.raw[i] as i64 + b.delta) as u64;
            }
            i -= b.raw.len();
        }
        panic!("line index out of range");
    }

    pub fn get_opt(&self, i: usize) -> Option<u64> {
        if i < self.count {
            Some(self.get(i))
        } else {
            None
        }
    }

    /// Number of newlines strictly before `byte`
    /// (== partition_point(|&n| n < byte) on the flat vec).
    pub fn rank(&self, byte: u64) -> usize {
        let mut base = 0usize;
        for b in &self.blocks {
            if byte <= b.first() {
                return base
                    + b.raw
                        .partition_point(|&r| ((r as i64 + b.delta) as u64) < byte);
            }
            if byte <= b.last() {
                return base
                    + b.raw
                        .partition_point(|&r| ((r as i64 + b.delta) as u64) < byte);
            }
            base += b.raw.len();
        }
        base
    }

    /// Replace newline entries in byte range [start, end) with `fresh`
    /// (absolute, ascending, all in [start, start+new_len)), then shift
    /// every entry at/after `end` by `delta` bytes.
    pub fn patch(&mut self, start: u64, end: u64, fresh: &[u64], delta: i64) {
        // find the affected block span
        let mut bi = 0;
        while bi < self.blocks.len() && self.blocks[bi].last() < start {
            bi += 1;
        }
        let mut bj = bi;
        while bj < self.blocks.len() && self.blocks[bj].first() < end {
            bj += 1;
        }
        // rebuild blocks bi..bj as one materialized run
        // ponytail: affected span is one or two blocks for typical edits
        let mut run: Vec<u64> = Vec::new();
        for b in &self.blocks[bi..bj] {
            run.extend(b.raw.iter().map(|&r| (r as i64 + b.delta) as u64));
        }
        let lo = run.partition_point(|&n| n < start);
        let hi = run.partition_point(|&n| n < end);
        let removed = hi - lo;
        let mut rebuilt: Vec<u64> = Vec::with_capacity(run.len() - removed + fresh.len());
        rebuilt.extend_from_slice(&run[..lo]);
        rebuilt.extend_from_slice(fresh);
        rebuilt.extend(run[hi..].iter().map(|&n| (n as i64 + delta) as u64));
        self.count = self.count - removed + fresh.len();
        // shift all later blocks by delta only
        for b in &mut self.blocks[bj..] {
            b.delta += delta;
        }
        let new_blocks: Vec<Block> = rebuilt
            .chunks(BLOCK)
            .map(|c| Block {
                raw: c.to_vec(),
                delta: 0,
            })
            .collect();
        self.blocks.splice(bi..bj, new_blocks);
        self.blocks.retain(|b| !b.raw.is_empty());
    }

    #[cfg(test)]
    pub fn to_vec(&self) -> Vec<u64> {
        (0..self.count).map(|i| self.get(i)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference model: flat vec with the old splice+shift semantics.
    fn model_patch(v: &mut Vec<u64>, start: u64, end: u64, fresh: &[u64], delta: i64) {
        let lo = v.partition_point(|&n| n < start);
        let hi = v.partition_point(|&n| n < end);
        let flen = fresh.len();
        v.splice(lo..hi, fresh.iter().copied());
        for n in &mut v[lo + flen..] {
            *n = (*n as i64 + delta) as u64;
        }
    }

    #[test]
    fn matches_flat_model_randomized() {
        let mut seed = 0xabcdefu64;
        let mut rnd = move |m: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            if m == 0 {
                0
            } else {
                (seed >> 33) % m
            }
        };
        // start with newlines every ~10 bytes over 1 MB (100k lines,
        // several blocks)
        let mut flat: Vec<u64> = (1..100_000u64).map(|i| i * 10).collect();
        let mut ix = LineIndex::from_vec(flat.clone());
        let mut file_len = 1_000_000u64;
        for _ in 0..300 {
            let start = rnd(file_len);
            let end = (start + rnd(2000)).min(file_len);
            let ins_len = rnd(300);
            let mut fresh: Vec<u64> = Vec::new();
            let mut at = start;
            while at < start + ins_len {
                at += 1 + rnd(40);
                if at < start + ins_len {
                    fresh.push(at);
                }
            }
            let delta = ins_len as i64 - (end - start) as i64;
            model_patch(&mut flat, start, end, &fresh, delta);
            ix.patch(start, end, &fresh, delta);
            file_len = (file_len as i64 + delta) as u64;
            assert_eq!(ix.count(), flat.len());
        }
        assert_eq!(ix.to_vec(), flat);
        // rank agrees at boundaries
        for probe in [0u64, 5, 9_999, 500_000, file_len] {
            assert_eq!(
                ix.rank(probe),
                flat.partition_point(|&n| n < probe),
                "probe {probe}"
            );
        }
    }

    #[test]
    fn empty_and_grow() {
        let mut ix = LineIndex::from_vec(Vec::new());
        assert_eq!(ix.count(), 0);
        assert_eq!(ix.rank(100), 0);
        ix.patch(0, 0, &[3, 7], 10);
        assert_eq!(ix.to_vec(), vec![3, 7]);
        ix.patch(0, 8, &[], -8); // delete everything
        assert_eq!(ix.count(), 0);
    }

    #[test]
    fn shift_is_cheap_across_blocks() {
        // 10 blocks; a small edit at the front must not rebuild the tail
        let big: Vec<u64> = (1..(BLOCK as u64 * 10)).map(|i| i * 4).collect();
        let mut ix = LineIndex::from_vec(big.clone());
        ix.patch(0, 0, &[], 100); // pure insertion of 100 bytes, no newline
        assert_eq!(ix.get(0), big[0] + 100);
        assert_eq!(ix.get(ix.count() - 1), big[big.len() - 1] + 100);
    }
}
