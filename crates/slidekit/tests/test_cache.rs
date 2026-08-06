//! Decoded-tile cache tests over the tiled-TIFF read path.
//!
//! These need no test data: a synthetic tiled TIFF is served through a
//! [`SlideFs`] that counts every read reaching it, in the same shape as the
//! `Counting` fake in `src/io/readahead.rs`. That is what lets them assert the
//! property the cache exists for — **a re-read of the same region issues no I/O
//! at all** — which pixel comparisons alone cannot show, since a correct
//! uncached read produces identical pixels too.
//!
//! Per-vendor pixel-identity regressions live beside each vendor's own fixture
//! (`test_aperio.rs`, `test_mirax.rs`, …) via
//! `common::assert_cache_is_transparent`.

mod common;

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use slidekit::{Cache, Entry, OpenSlide, SlideFs, SlideReader};

const TILE: u32 = 16;
const TILES_X: u32 = 8;
const TILES_Y: u32 = 8;
const SLIDE_NAME: &str = "counting.tif";

/// Distinct solid colour per tile, so a pixel identifies the tile it came from
/// and a mis-keyed cache shows up as the wrong colour rather than as noise.
fn tile_color(tx: u32, ty: u32) -> (u8, u8, u8) {
    ((tx as u8) * 24 + 3, (ty as u8) * 24 + 5, 200)
}

/// Counts the reads that reach the bytes: single positioned reads and batched
/// gathers separately, since a region read is meant to arrive as *one* gather.
#[derive(Default)]
struct Counts {
    reads: AtomicU64,
    batches: AtomicU64,
}

impl Counts {
    fn snapshot(&self) -> (u64, u64) {
        (
            self.reads.load(Ordering::Relaxed),
            self.batches.load(Ordering::Relaxed),
        )
    }
}

struct CountingReader {
    data: Arc<Vec<u8>>,
    counts: Arc<Counts>,
}

impl SlideReader for CountingReader {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> slidekit::Result<usize> {
        self.counts.reads.fetch_add(1, Ordering::Relaxed);
        let start = (offset as usize).min(self.data.len());
        let n = buf.len().min(self.data.len() - start);
        buf[..n].copy_from_slice(&self.data[start..start + n]);
        Ok(n)
    }

    fn read_ranges(&self, ranges: &[Range<u64>]) -> slidekit::Result<Vec<Bytes>> {
        self.counts.batches.fetch_add(1, Ordering::Relaxed);
        Ok(ranges
            .iter()
            .map(|r| {
                let start = (r.start as usize).min(self.data.len());
                let end = (r.end as usize).min(self.data.len());
                Bytes::copy_from_slice(&self.data[start..end])
            })
            .collect())
    }

    fn size(&self) -> slidekit::Result<u64> {
        Ok(self.data.len() as u64)
    }
}

struct CountingFs {
    data: Arc<Vec<u8>>,
    counts: Arc<Counts>,
}

impl SlideFs for CountingFs {
    fn open(&self, path: &str) -> slidekit::Result<Arc<dyn SlideReader>> {
        if path != SLIDE_NAME {
            return Err(slidekit::OpenSlideError::Other(format!(
                "no such file: {path}"
            )));
        }
        Ok(Arc::new(CountingReader {
            data: self.data.clone(),
            counts: self.counts.clone(),
        }))
    }

    fn list(&self, _dir: &str) -> slidekit::Result<Vec<Entry>> {
        Ok(vec![Entry {
            path: SLIDE_NAME.to_string(),
            size: self.data.len() as u64,
        }])
    }
}

/// Open the synthetic slide over a counting filesystem, returning the slide and
/// the counters. Reads made while opening are the caller's to snapshot past.
fn open_counting() -> (OpenSlide, Arc<Counts>) {
    let data = Arc::new(common::build_tiled_rgb_tiff(
        TILE, TILES_X, TILES_Y, tile_color,
    ));
    let counts = Arc::new(Counts::default());
    let fs: Arc<dyn SlideFs> = Arc::new(CountingFs {
        data,
        counts: counts.clone(),
    });
    let slide = OpenSlide::open_fs(fs, SLIDE_NAME).expect("open synthetic tiled TIFF");
    (slide, counts)
}

/// The headline property: a region read twice costs I/O once.
#[test]
fn a_second_read_of_the_same_region_issues_no_reads() {
    let (slide, counts) = open_counting();
    let region = (0, 0, 0, 64, 64);

    let first = slide
        .read_region(region.0, region.1, region.2, region.3, region.4)
        .expect("first read");
    let after_first = counts.snapshot();
    assert!(
        after_first.1 > 0,
        "the first read should have gathered tile bytes"
    );

    let second = slide
        .read_region(region.0, region.1, region.2, region.3, region.4)
        .expect("second read");

    assert_eq!(
        counts.snapshot(),
        after_first,
        "a fully cached region must issue no reads at all — not a gather it then discards"
    );
    common::assert_pixels_eq(&second, &first, "second read of the same region");
}

/// Panning: the overlap with the previous window is free, and the pixels are
/// the same as a cold read of the same window would have produced.
#[test]
fn an_overlapping_region_only_fetches_the_tiles_it_adds() {
    let (slide, counts) = open_counting();

    // Reference pixels for the second window, read with no cache at all.
    let (cold, _) = open_counting();
    let mut cold = cold;
    cold.set_cache(Cache::new(0));
    let reference = cold.read_region(32, 32, 0, 64, 64).expect("cold read");

    slide.read_region(0, 0, 0, 64, 64).expect("first window");
    let (reads_before, _) = counts.snapshot();

    let panned = slide.read_region(32, 32, 0, 64, 64).expect("second window");
    common::assert_pixels_eq(&panned, &reference, "panned window against a cold read");

    // The 4x4 tiles of the first window overlap the second in a 2x2 block, so
    // the second window must fetch 12 tiles, not 16.
    assert!(
        slide.cache().hits() >= 4,
        "expected the overlapping tiles to be cache hits, got {} hits",
        slide.cache().hits()
    );
    let (reads_after, _) = counts.snapshot();
    assert_eq!(
        reads_before, reads_after,
        "tile bytes should still arrive as gathers, not positioned reads"
    );
}

/// A disabled cache must still read correctly — it is the reference the other
/// assertions compare against, so it has to be trustworthy.
#[test]
fn a_disabled_cache_reads_the_same_pixels_and_keeps_fetching() {
    let (slide, counts) = open_counting();
    let warm = slide.read_region(16, 16, 0, 48, 48).expect("warm read");

    let (mut slide, counts_off) = open_counting();
    slide.set_cache(Cache::new(0));
    let first = slide.read_region(16, 16, 0, 48, 48).expect("first");
    let after_first = counts_off.snapshot();
    let second = slide.read_region(16, 16, 0, 48, 48).expect("second");

    common::assert_pixels_eq(&first, &warm, "disabled cache against a warm cache");
    common::assert_pixels_eq(&second, &first, "disabled cache, second read");
    assert!(
        counts_off.snapshot().1 > after_first.1,
        "a disabled cache must re-fetch"
    );
    assert_eq!(slide.cache().len(), 0);
    let _ = counts;
}

/// Two slides sharing one `Cache` must not read each other's tiles. Both are
/// the same *shape* of TIFF at the same offsets, differing only in colour, so a
/// key that forgot the slide would return the other slide's pixels and nothing
/// would notice.
#[test]
fn two_slides_sharing_a_cache_do_not_share_tiles() {
    let shared = Cache::new(8 * 1024 * 1024);

    let build = |shift: u8| {
        Arc::new(common::build_tiled_rgb_tiff(
            TILE,
            TILES_X,
            TILES_Y,
            move |tx, ty| {
                let (r, g, b) = tile_color(tx, ty);
                (r.wrapping_add(shift), g, b)
            },
        ))
    };

    let open = |data: Arc<Vec<u8>>| {
        let fs: Arc<dyn SlideFs> = Arc::new(CountingFs {
            data,
            counts: Arc::new(Counts::default()),
        });
        OpenSlide::open_fs(fs, SLIDE_NAME).expect("open")
    };

    let mut a = open(build(0));
    let mut b = open(build(64));
    a.set_cache(shared.clone());
    b.set_cache(shared.clone());

    // Reference pixels for each, taken before either cache is warm.
    let want_a = {
        let mut solo = open(build(0));
        solo.set_cache(Cache::new(0));
        solo.read_region(0, 0, 0, 64, 64).expect("reference a")
    };
    let want_b = {
        let mut solo = open(build(64));
        solo.set_cache(Cache::new(0));
        solo.read_region(0, 0, 0, 64, 64).expect("reference b")
    };
    assert_ne!(want_a, want_b, "the two slides must differ to be a test");

    // Warm A, then read B over the top of it, then A again.
    common::assert_pixels_eq(
        &a.read_region(0, 0, 0, 64, 64).expect("a"),
        &want_a,
        "slide A, cold",
    );
    common::assert_pixels_eq(
        &b.read_region(0, 0, 0, 64, 64).expect("b"),
        &want_b,
        "slide B, over a cache warmed by slide A",
    );
    common::assert_pixels_eq(
        &a.read_region(0, 0, 0, 64, 64).expect("a again"),
        &want_a,
        "slide A, over a cache also holding slide B",
    );

    // Both slides' tiles are resident at once — 16 tiles each.
    assert_eq!(shared.len(), 32, "each slide should hold its own 16 tiles");
}

/// The cache is bounded by pixel bytes. A budget of two tiles must hold two
/// tiles, whatever the tile size, and must not change what is read.
#[test]
fn a_tiny_budget_evicts_rather_than_growing() {
    let tile_bytes = (TILE * TILE) as usize * 4;
    let (mut slide, _) = open_counting();
    slide.set_cache(Cache::new(2 * tile_bytes + 512));

    let (cold, _) = open_counting();
    let mut cold = cold;
    cold.set_cache(Cache::new(0));
    let reference = cold.read_region(0, 0, 0, 64, 64).expect("cold");

    for _ in 0..3 {
        let got = slide.read_region(0, 0, 0, 64, 64).expect("thrashing read");
        common::assert_pixels_eq(&got, &reference, "read under a 2-tile budget");
    }

    assert!(
        slide.cache().used_bytes() <= slide.cache().capacity(),
        "{} bytes held against a {} byte budget",
        slide.cache().used_bytes(),
        slide.cache().capacity()
    );
    assert!(
        slide.cache().len() <= 2,
        "{} tiles held",
        slide.cache().len()
    );
}

/// The same transparency check the vendor fixtures run, on a slide that needs
/// no test data — so the shared helper itself is exercised on every machine.
#[test]
fn repeated_and_overlapping_reads_stay_pixel_identical() {
    common::assert_cache_is_transparent(
        || open_counting().0,
        0,
        &[(0, 0, 64, 64), (32, 32, 64, 64), (16, 0, 96, 48)],
    );
}
