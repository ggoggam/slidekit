//! Block read-ahead over a [`SlideReader`], for backends where a round trip
//! costs far more than a byte.
//!
//! Parsing a slide is a long run of tiny positioned reads. A TIFF directory is
//! walked entry by entry (2–12 bytes each) with out-of-line tag values fetched
//! individually; a DICOM dataset is read field by field. On local disk each of
//! those is a `pread` against the page cache and costs nothing. On blob storage
//! each one is its own ranged GET, and the measured result is stark: opening a
//! generic tiled TIFF cost 210 requests to transfer 0.24 MB, and took 82
//! seconds. The bytes were never the problem — the round trips were.
//!
//! This wrapper serves those reads out of cached blocks, so a burst of small
//! reads over one region of the file costs one request instead of hundreds. Two
//! properties make it work on real slide layouts:
//!
//!  - **Growth on sequential access.** A scan that keeps moving forward — a
//!    DICOM fragment walk, an NDPI restart-marker hunt — doubles the fetch size
//!    up to [`MAX_BLOCK`], so a long walk costs tens of large requests rather
//!    than thousands of small ones. Any other access pattern resets to
//!    [`MIN_BLOCK`], which keeps a random tile read from pulling megabytes.
//!  - **Partial hits are not refetched.** A read that starts inside a cached
//!    block returns what the block holds and reports a short read;
//!    [`SlideReader::read_exact_at`] loops, so the remainder is fetched from
//!    where it actually begins.
//!
//! It is deliberately *not* applied to local files: there the kernel's page
//! cache already does this, and interposing a second cache would only add
//! copies. [`LocalFs`](crate::io::local::LocalFs) stays unwrapped;
//! [`ObjectStoreFs`](crate::io::object_store::ObjectStoreFs) wraps every reader
//! it opens.

use std::ops::Range;
use std::sync::Arc;

use bytes::Bytes;
use parking_lot::Mutex;

use crate::error::{OpenSlideError, Result};
use crate::io::SlideReader;
use crate::io::fs::SlideLocation;

/// Smallest block fetched on a cache miss, and the size a non-sequential access
/// pattern resets to. Comfortably larger than any TIFF directory, so one fetch
/// covers a whole IFD and its out-of-line values.
const MIN_BLOCK: usize = 64 * 1024;

/// Largest block a sequential scan grows to. Caps read amplification: the
/// tail of a walk can overshoot the bytes actually needed by at most this much.
const MAX_BLOCK: usize = 4 * 1024 * 1024;

/// Total cached bytes per reader. Two `MAX_BLOCK` blocks plus room for a
/// working set of small ones — enough for a forward scan to keep its window and
/// for a tile read to hit a neighbour, without a multi-instance DICOM slide
/// (one reader per instance) adding up to anything alarming.
const CAPACITY: usize = 16 * 1024 * 1024;

/// Cap on the number of retained blocks, so the (linear) lookup stays cheap
/// even when they are all small.
const MAX_BLOCKS: usize = 64;

/// A cached run of bytes at a known file offset.
struct Block {
    start: u64,
    data: Arc<Vec<u8>>,
}

impl Block {
    fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }
}

/// What a miss should fetch.
struct Fetch {
    start: u64,
    len: usize,
}

#[derive(Default)]
struct State {
    /// Cached blocks, most recently used first.
    blocks: Vec<Block>,
    /// Sum of `blocks[..].data.len()`.
    bytes: usize,
    /// Extent of the most recent fetch, for sequential detection.
    last: Option<(u64, u64)>,
    /// Size the next miss will fetch, before it is widened to fit the request.
    next_size: usize,
    /// Requests issued to the inner reader, and bytes they carried.
    fetches: u64,
    fetched_bytes: u64,
}

impl State {
    /// Copy what the cache already holds at `offset` into `buf`, returning how
    /// many bytes that was. `None` means nothing covers `offset`.
    fn hit(&mut self, offset: u64, buf: &mut [u8]) -> Option<usize> {
        let index = self
            .blocks
            .iter()
            .position(|b| b.start <= offset && offset < b.end())?;
        // Promote to most-recently-used: a forward scan then keeps its current
        // window ahead of the small blocks a tile read left behind.
        let block = self.blocks.remove(index);
        let from = (offset - block.start) as usize;
        let n = (block.data.len() - from).min(buf.len());
        buf[..n].copy_from_slice(&block.data[from..from + n]);
        self.blocks.insert(0, block);
        Some(n)
    }

    /// Decide what a miss at `offset` for `len` bytes should fetch, and record
    /// the decision so the next one can tell whether the access is sequential.
    ///
    /// `limit` is the bytes left in the file at `offset`.
    fn plan(&mut self, offset: u64, len: usize, limit: u64) -> Fetch {
        // Sequential means "moved forward into or just past the last window".
        // The looser upper bound matters: a buffered parser re-reads from a
        // position *inside* the previous block whenever its own window runs
        // out, so requiring `offset == last_end` would call a steady forward
        // walk random and never grow.
        let sequential = self.last.is_some_and(|(start, end)| {
            offset >= start && offset <= end.saturating_add(MIN_BLOCK as u64)
        });
        self.next_size = if sequential {
            (self.next_size * 2).clamp(MIN_BLOCK, MAX_BLOCK)
        } else {
            MIN_BLOCK
        };

        let len = self.next_size.max(len).min(limit as usize);
        self.last = Some((offset, offset + len as u64));
        Fetch { start: offset, len }
    }

    /// Account for a fetch and, if the block is worth keeping, cache it.
    ///
    /// A fetch far larger than the cache itself is not a block — it is one big
    /// value the caller asked for outright, like a 13 MB DICOM ICC profile.
    /// Caching it would evict the entire working set to hold something no
    /// subsequent read will land inside, and the caller is keeping its own copy
    /// anyway.
    fn insert(&mut self, start: u64, data: Vec<u8>) -> Arc<Vec<u8>> {
        self.fetches += 1;
        self.fetched_bytes += data.len() as u64;

        let data = Arc::new(data);
        if data.len() > CAPACITY / 2 {
            return data;
        }
        self.bytes += data.len();
        self.blocks.insert(
            0,
            Block {
                start,
                data: data.clone(),
            },
        );
        while self.bytes > CAPACITY || self.blocks.len() > MAX_BLOCKS {
            // Never evict the block just inserted, even if it alone is over
            // budget — the caller is about to read from it.
            if self.blocks.len() <= 1 {
                break;
            }
            let evicted = self.blocks.pop().expect("non-empty");
            self.bytes -= evicted.data.len();
        }
        data
    }
}

/// Traffic a [`ReadAhead`] passed to its inner reader. Exposed for tests and
/// benches, which assert on request counts rather than wall-clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadAheadStats {
    /// Reads issued to the inner reader.
    pub fetches: u64,
    /// Bytes those reads carried.
    pub fetched_bytes: u64,
}

/// A [`SlideReader`] that serves small reads from cached blocks. See the
/// [module docs](self).
///
/// Cloning shares one cache, so the several readers an open needs over the same
/// object don't each refetch its header.
pub struct ReadAhead<R> {
    inner: R,
    size: u64,
    state: Arc<Mutex<State>>,
}

impl<R> Clone for ReadAhead<R>
where
    R: Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            size: self.size,
            state: self.state.clone(),
        }
    }
}

impl<R: SlideReader> ReadAhead<R> {
    /// Wrap `inner`, taking its size up front — every read is clamped to it, and
    /// asking again per read would defeat the point.
    pub fn new(inner: R) -> Result<Self> {
        let size = inner.size()?;
        Ok(Self {
            inner,
            size,
            state: Arc::new(Mutex::new(State::default())),
        })
    }

    /// Reads and bytes this reader (and its clones) passed through.
    pub fn stats(&self) -> ReadAheadStats {
        let state = self.state.lock();
        ReadAheadStats {
            fetches: state.fetches,
            fetched_bytes: state.fetched_bytes,
        }
    }
}

impl<R: SlideReader> SlideReader for ReadAhead<R> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() || offset >= self.size {
            return Ok(0);
        }
        let want = buf.len().min((self.size - offset) as usize);
        let buf = &mut buf[..want];

        let fetch = {
            let mut state = self.state.lock();
            if let Some(n) = state.hit(offset, buf) {
                return Ok(n);
            }
            state.plan(offset, want, self.size - offset)
        };

        // Fetched without the lock held: a network read must not block another
        // thread's cache hit. Two threads racing the same miss both fetch,
        // which costs a duplicate request and stays correct.
        let mut data = vec![0u8; fetch.len];
        self.inner.read_exact_at(fetch.start, &mut data)?;

        let data = self.state.lock().insert(fetch.start, data);
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    /// Serve whatever the cache already covers, and pass the rest down as one
    /// batch so the backend can coalesce and parallelize it.
    ///
    /// The sequential-growth state is deliberately left alone: a gather is a
    /// statement about a set of ranges, not a scan, and letting it inflate the
    /// block size would make the *next* miss over-fetch. The fetched ranges are
    /// cached, so overlapping regions (a viewer panning by less than a tile) hit.
    fn read_ranges(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
        if ranges.is_empty() {
            return Ok(Vec::new());
        }

        let mut out: Vec<Option<Bytes>> = Vec::with_capacity(ranges.len());
        let mut missing: Vec<Range<u64>> = Vec::new();
        let mut missing_slots: Vec<usize> = Vec::new();
        {
            let mut state = self.state.lock();
            for (slot, range) in ranges.iter().enumerate() {
                let len = (range.end - range.start) as usize;
                let mut buf = vec![0u8; len];
                // Only a *complete* hit is usable here; a partial one would have
                // to be finished with another request, which is the cost this
                // method exists to avoid.
                match state.hit(range.start, &mut buf) {
                    Some(n) if n == len => out.push(Some(Bytes::from(buf))),
                    _ => {
                        out.push(None);
                        missing.push(range.clone());
                        missing_slots.push(slot);
                    }
                }
            }
        }

        if !missing.is_empty() {
            let fetched = self.inner.read_ranges(&missing)?;
            // `SlideReader` is a public extension point, so the one-buffer-per
            // -range contract is a promise a downstream backend can break. The
            // `zip` below would then leave a slot empty, and unwrapping it would
            // turn a recoverable read failure into a process-killing panic.
            if fetched.len() != missing.len() {
                return Err(OpenSlideError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "reader returned {} buffers for {} ranges",
                        fetched.len(),
                        missing.len()
                    ),
                )));
            }
            let mut state = self.state.lock();
            for ((slot, range), bytes) in missing_slots.iter().zip(&missing).zip(fetched) {
                state.insert(range.start, bytes.to_vec());
                out[*slot] = Some(bytes);
            }
        }

        out.into_iter()
            .map(|b| {
                b.ok_or_else(|| {
                    OpenSlideError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "reader left a requested range unfilled",
                    ))
                })
            })
            .collect()
    }

    fn size(&self) -> Result<u64> {
        Ok(self.size)
    }

    fn location(&self) -> Option<&SlideLocation> {
        self.inner.location()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// An in-memory reader that counts the reads reaching it: single positioned
    /// reads and batched gathers separately, since the point of a gather is that
    /// it arrives as *one* call the backend can coalesce.
    struct Counting {
        data: Vec<u8>,
        reads: AtomicU64,
        batches: AtomicU64,
        bytes: AtomicU64,
    }

    impl Counting {
        fn new(len: usize) -> Arc<Self> {
            Arc::new(Self {
                data: (0..len).map(|i| (i % 251) as u8).collect(),
                reads: AtomicU64::new(0),
                batches: AtomicU64::new(0),
                bytes: AtomicU64::new(0),
            })
        }

        fn reads(&self) -> u64 {
            self.reads.load(Ordering::Relaxed)
        }

        fn batches(&self) -> u64 {
            self.batches.load(Ordering::Relaxed)
        }

        fn slice(&self, offset: u64, len: usize) -> &[u8] {
            let start = (offset as usize).min(self.data.len());
            let n = len.min(self.data.len() - start);
            self.bytes.fetch_add(n as u64, Ordering::Relaxed);
            &self.data[start..start + n]
        }
    }

    impl SlideReader for Arc<Counting> {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let src = self.slice(offset, buf.len());
            buf[..src.len()].copy_from_slice(src);
            Ok(src.len())
        }

        fn read_ranges(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
            self.batches.fetch_add(1, Ordering::Relaxed);
            Ok(ranges
                .iter()
                .map(|r| Bytes::copy_from_slice(self.slice(r.start, (r.end - r.start) as usize)))
                .collect())
        }

        fn size(&self) -> Result<u64> {
            Ok(self.data.len() as u64)
        }
    }

    /// The TIFF-directory shape: many tiny reads scattered over one region.
    #[test]
    fn scattered_small_reads_cost_one_fetch() {
        let inner = Counting::new(1 << 20);
        let reader = ReadAhead::new(inner.clone()).expect("wrap");
        // `new` asks for the size, not for data.
        assert_eq!(inner.reads(), 0);

        let mut buf = [0u8; 4];
        for offset in (0..2000).step_by(7) {
            reader.read_exact_at(offset, &mut buf).expect("read");
            assert_eq!(buf[0], (offset as usize % 251) as u8);
        }
        assert_eq!(
            inner.reads(),
            1,
            "286 reads over one 2 KB span should be one fetch"
        );
        assert_eq!(reader.stats().fetched_bytes, MIN_BLOCK as u64);
    }

    /// The DICOM fragment-walk shape: an 8-byte header, then a skip past most of
    /// a window, over and over. Each step re-reads from *inside* the previous
    /// block, which is the case a strict `offset == last_end` test would miss.
    #[test]
    fn forward_walk_grows_the_block() {
        const STRIDE: u64 = 60 * 1024;
        let inner = Counting::new(64 << 20);
        let reader = ReadAhead::new(inner.clone()).expect("wrap");

        let mut buf = [0u8; 8];
        let mut offset = 0;
        while offset + 8 < 32 << 20 {
            reader.read_exact_at(offset, &mut buf).expect("read");
            offset += STRIDE;
        }

        let steps = (32 << 20) / STRIDE;
        let stats = reader.stats();
        // Without read-ahead this is one request per step. With it, the block
        // doubles to MAX_BLOCK and the whole walk is a couple dozen.
        assert!(
            stats.fetches < steps / 10,
            "{} fetches for {steps} steps",
            stats.fetches
        );
        assert!(
            stats.fetched_bytes <= (32 << 20) + MAX_BLOCK as u64,
            "walked 32 MiB but fetched {}",
            stats.fetched_bytes
        );
    }

    /// A random pattern must not inherit a grown block: the tile reads that
    /// follow a walk would otherwise each pull megabytes.
    #[test]
    fn random_access_resets_to_the_small_block() {
        let inner = Counting::new(64 << 20);
        let reader = ReadAhead::new(inner.clone()).expect("wrap");

        // Grow first, with a forward scan.
        let mut probe = [0u8; 8];
        for step in 0..64u64 {
            reader
                .read_exact_at(step * 60 * 1024, &mut probe)
                .expect("read");
        }
        let after_scan = reader.stats().fetched_bytes;

        // Then jump around, far apart and backwards, so nothing is cached.
        let mut tile = vec![0u8; 16 * 1024];
        for step in 0..8u64 {
            let offset = (60 << 20) - step * (7 << 20);
            reader.read_exact_at(offset, &mut tile).expect("read");
        }
        let per_read = (reader.stats().fetched_bytes - after_scan) / 8;
        assert_eq!(
            per_read, MIN_BLOCK as u64,
            "a scattered read should fetch one small block"
        );
    }

    /// A read larger than the block size is fetched whole rather than in pieces.
    #[test]
    fn large_reads_are_not_split() {
        let inner = Counting::new(16 << 20);
        let reader = ReadAhead::new(inner.clone()).expect("wrap");

        let mut big = vec![0u8; 3 << 20];
        reader.read_exact_at(1 << 20, &mut big).expect("read");
        assert_eq!(inner.reads(), 1);
        assert_eq!(big[0], ((1 << 20) % 251) as u8);
    }

    /// One outsized value — a 13 MB DICOM ICC profile, say — must not evict the
    /// working set to cache something nothing will read again.
    #[test]
    fn an_outsized_read_is_not_cached() {
        let inner = Counting::new(64 << 20);
        let reader = ReadAhead::new(inner.clone()).expect("wrap");

        // Warm a small block, then pull something bigger than half the budget.
        let mut probe = [0u8; 16];
        reader.read_exact_at(4096, &mut probe).expect("read");
        let blocks_before = reader.state.lock().blocks.len();

        let mut huge = vec![0u8; CAPACITY / 2 + 1];
        reader.read_exact_at(32 << 20, &mut huge).expect("read");
        assert_eq!(huge[0], ((32 << 20) % 251) as u8);

        let state = reader.state.lock();
        assert_eq!(
            state.blocks.len(),
            blocks_before,
            "the outsized read should not have entered the cache"
        );
        assert!(state.bytes <= CAPACITY / 2);
        drop(state);

        // The warmed block survived, so it is still a hit.
        let before = inner.reads();
        reader.read_exact_at(4096, &mut probe).expect("read");
        assert_eq!(inner.reads(), before, "the working set was evicted");
    }

    /// Bytes must be identical to the unwrapped reader, including at the very
    /// end of the file, where a fetch is clamped and a read must still report
    /// only what exists.
    #[test]
    fn reads_agree_with_the_inner_reader_at_the_end_of_the_file() {
        let inner = Counting::new(MIN_BLOCK + 100);
        let reader = ReadAhead::new(inner.clone()).expect("wrap");

        let mut all = vec![0u8; MIN_BLOCK + 100];
        reader.read_exact_at(0, &mut all).expect("read");
        assert_eq!(all, inner.data);

        // Past the end reads nothing; straddling the end is a short read.
        let mut tail = [0u8; 32];
        assert_eq!(
            reader.read_at(MIN_BLOCK as u64 + 100, &mut tail).unwrap(),
            0
        );
        assert_eq!(
            reader.read_at(MIN_BLOCK as u64 + 90, &mut tail).unwrap(),
            10
        );
        assert!(
            reader
                .read_exact_at(MIN_BLOCK as u64 + 90, &mut tail)
                .is_err()
        );
    }

    /// Clones share the cache, so the several readers an open builds over one
    /// object don't each refetch its header.
    #[test]
    fn clones_share_one_cache() {
        let inner = Counting::new(1 << 20);
        let first = ReadAhead::new(inner.clone()).expect("wrap");
        let second = first.clone();

        let mut buf = [0u8; 16];
        first.read_exact_at(0, &mut buf).expect("read");
        second.read_exact_at(64, &mut buf).expect("read");
        assert_eq!(inner.reads(), 1);
        assert_eq!(second.stats().fetches, 1);
    }

    /// A gather passes the misses down as one batch, serves what it already has,
    /// and returns the ranges in the order asked for.
    #[test]
    fn read_ranges_batches_misses_and_serves_hits() {
        let inner = Counting::new(8 << 20);
        let reader = ReadAhead::new(inner.clone()).expect("wrap");

        // Warm one region so the batch below has a hit in it.
        let mut warm = [0u8; 16];
        reader.read_exact_at(1024, &mut warm).expect("read");
        assert_eq!(inner.reads(), 1);

        let ranges = [
            4 << 20..(4 << 20) + 100, // miss
            1024..1124,               // inside the warmed block
            6 << 20..(6 << 20) + 100, // miss
        ];
        let parts = reader.read_ranges(&ranges).expect("gather");

        assert_eq!(parts.len(), 3);
        for (part, range) in parts.iter().zip(&ranges) {
            assert_eq!(part.len(), (range.end - range.start) as usize);
            assert_eq!(part[0], (range.start as usize % 251) as u8, "{range:?}");
        }
        // One batched call for the two misses; the hit cost nothing, and no
        // single-range read leaked out alongside it.
        assert_eq!(inner.batches(), 1, "expected a single batched fetch");
        assert_eq!(inner.reads(), 1, "the warm-up read, and nothing since");

        // The fetched ranges are cached, so asking again is free.
        reader.read_ranges(&ranges).expect("gather again");
        assert_eq!(inner.batches(), 1);
        assert_eq!(inner.reads(), 1);

        // An empty batch is not a request.
        assert!(reader.read_ranges(&[]).expect("empty").is_empty());
    }

    /// A gather must not train the sequential heuristic: a batch of far-apart
    /// tile ranges says nothing about what comes next, and letting it grow the
    /// block would make the following read over-fetch.
    #[test]
    fn read_ranges_leaves_the_block_size_alone() {
        let inner = Counting::new(64 << 20);
        let reader = ReadAhead::new(inner.clone()).expect("wrap");

        let ranges: Vec<_> = (0..8u64)
            .map(|i| i * (4 << 20)..i * (4 << 20) + 4096)
            .collect();
        reader.read_ranges(&ranges).expect("gather");
        let after_gather = reader.stats().fetched_bytes;

        let mut probe = [0u8; 8];
        reader.read_exact_at(50 << 20, &mut probe).expect("read");
        assert_eq!(
            reader.stats().fetched_bytes - after_gather,
            MIN_BLOCK as u64,
            "the read after a gather should still fetch one small block"
        );
    }

    /// Eviction keeps the cache bounded no matter how much is read through it.
    #[test]
    fn cache_stays_within_budget() {
        let inner = Counting::new(256 << 20);
        let reader = ReadAhead::new(inner.clone()).expect("wrap");

        let mut buf = [0u8; 8];
        // Strides far enough apart that every read misses and resets to
        // MIN_BLOCK, so this inserts many small blocks.
        for step in 0..(MAX_BLOCKS as u64 * 4) {
            reader
                .read_exact_at(step * (MIN_BLOCK as u64 * 8), &mut buf)
                .expect("read");
        }
        let state = reader.state.lock();
        assert!(state.blocks.len() <= MAX_BLOCKS, "{}", state.blocks.len());
        assert!(state.bytes <= CAPACITY, "{}", state.bytes);
    }
}
