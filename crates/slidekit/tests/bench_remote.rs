//! Remote-storage bench — the same slides read from an S3-compatible bucket
//! instead of local disk, with request and byte counts.
//!
//! This is NOT part of the correctness suite. It is `#[ignore]`d, assumes a warm
//! local cache, and assumes the slides have already been mirrored into the
//! bucket by `upload_testdata.rs`:
//!
//! ```sh
//! mise run bench:upload-slides   # once
//! mise run bench:remote
//! ```
//!
//! It reports, per slide:
//!   - local `open()` / `read_region()` latency, as a baseline,
//!   - remote `open()` / `read_region()` latency over byte-range requests,
//!   - a second remote read of a *different* region, which reuses the already
//!     warm connection pool and tile-offset tables — the steady-state number a
//!     viewer would actually see,
//!   - how many HTTP requests and bytes each phase cost.
//!
//! The request/byte columns are the point of the whole exercise. They separate
//! the two things a remote open can cost — bytes and round trips — and it is the
//! round trips that dominate: the TIFF directory parse reads tag-by-tag, so each
//! few-byte read is its own ranged GET. Bytes stay small (the "no full download"
//! claim holds); request count is what makes a remote open slow.
//!
//! Byte accounting deliberately excludes zero-body HEAD probes; counting the
//! object range such a response reports would charge a metadata call the slide's
//! entire size and make a lazy open look like a full download.
//!
//! Correctness is asserted exactly, not tolerantly: local and remote go through
//! the same decoder over the same bytes, so the ARGB output must be identical
//! pixel-for-pixel. Any difference means the I/O layer (range math, `SlideFs`
//! sibling resolution, size clamping) corrupted or misplaced data — which is
//! precisely the risk in a remote read path. The tolerant diff belongs in
//! `bench_openslide.rs`, where an independent JPEG decoder is involved.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use futures_util::stream::BoxStream;
use object_store::path::Path as StorePath;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result as OsResult,
};
use slidekit::OpenSlide;

/// Side length of the compared square region, in pixels.
const REGION: i64 = 1024;

/// Minimum request size, in KiB, that [`CountingStore`] traces to stderr.
/// `None` unless `SLIDEKIT_BENCH_TRACE` is set.
fn trace_threshold_kib() -> Option<u64> {
    static THRESHOLD: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *THRESHOLD.get_or_init(|| {
        std::env::var("SLIDEKIT_BENCH_TRACE")
            .ok()
            .and_then(|v| v.trim().parse().ok())
    })
}

/// Counts of the store traffic one phase of work cost.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Traffic {
    requests: u64,
    bytes: u64,
    lists: u64,
    /// Metadata probes (zero-body HEADs) among `requests`.
    heads: u64,
}

impl std::ops::Sub for Traffic {
    type Output = Traffic;
    fn sub(self, rhs: Traffic) -> Traffic {
        Traffic {
            requests: self.requests - rhs.requests,
            bytes: self.bytes - rhs.bytes,
            lists: self.lists - rhs.lists,
            heads: self.heads - rhs.heads,
        }
    }
}

/// An [`ObjectStore`] that counts the requests and bytes flowing through it.
///
/// Only the trait's required methods are forwarded; in particular `get_ranges`
/// is deliberately **not** overridden, so its default coalescing implementation
/// runs against this wrapper and every request it issues lands in
/// [`CountingStore::get_opts`]. Forwarding `get_ranges` to the inner store
/// instead would hide those requests inside it and undercount.
#[derive(Debug)]
struct CountingStore {
    inner: Arc<dyn ObjectStore>,
    requests: AtomicU64,
    bytes: AtomicU64,
    lists: AtomicU64,
    heads: AtomicU64,
}

impl CountingStore {
    fn new(inner: Arc<dyn ObjectStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            requests: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            lists: AtomicU64::new(0),
            heads: AtomicU64::new(0),
        })
    }

    fn traffic(&self) -> Traffic {
        Traffic {
            requests: self.requests.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            lists: self.lists.load(Ordering::Relaxed),
            heads: self.heads.load(Ordering::Relaxed),
        }
    }
}

impl std::fmt::Display for CountingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CountingStore({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for CountingStore {
    async fn put_opts(
        &self,
        location: &StorePath,
        payload: PutPayload,
        opts: PutOptions,
    ) -> OsResult<PutResult> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &StorePath,
        opts: PutMultipartOptions,
    ) -> OsResult<Box<dyn MultipartUpload>> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(&self, location: &StorePath, options: GetOptions) -> OsResult<GetResult> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        // A metadata probe carries no body. `ObjectStoreExt::head` is implemented
        // as `get_opts` with this flag, and the result still reports `range` as
        // the whole object — so counting that span would charge a zero-byte HEAD
        // the slide's full size and make a lazy open look like a full download.
        let is_head = options.head;
        let result = self.inner.get_opts(location, options).await?;
        if is_head {
            self.heads.fetch_add(1, Ordering::Relaxed);
            return Ok(result);
        }
        // `range` is the byte span this response actually carries, which for a
        // ranged GET is what crossed the wire (bar headers).
        let span = result.range.end.saturating_sub(result.range.start);
        self.bytes.fetch_add(span, Ordering::Relaxed);
        // SLIDEKIT_BENCH_TRACE=<KiB> logs every request at least that large —
        // how you find out *which* reads are responsible for the byte total.
        if let Some(min_kib) = trace_threshold_kib()
            && span >= min_kib * 1024
        {
            eprintln!(
                "    GET {}..{} ({:.2} MB) {}",
                result.range.start,
                result.range.end,
                span as f64 / (1024.0 * 1024.0),
                location,
            );
            if std::env::var_os("SLIDEKIT_BENCH_BT").is_some() {
                eprintln!("{}", std::backtrace::Backtrace::force_capture());
            }
        }
        Ok(result)
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, OsResult<StorePath>>,
    ) -> BoxStream<'static, OsResult<StorePath>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&StorePath>) -> BoxStream<'static, OsResult<ObjectMeta>> {
        self.lists.fetch_add(1, Ordering::Relaxed);
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&StorePath>) -> OsResult<ListResult> {
        self.lists.fetch_add(1, Ordering::Relaxed);
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &StorePath,
        to: &StorePath,
        options: CopyOptions,
    ) -> OsResult<()> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.copy_opts(from, to, options).await
    }
}

/// Does the *documented* remote entry point work from the environment alone?
///
/// `OpenSlide::open_url` says credentials and region/endpoint configuration are
/// taken from the environment, and it is the API a user reaches for first. It
/// used to route through `object_store::parse_url`, which builds
/// `AmazonS3Builder::new()` — **not** `from_env()` — and passes no options, so
/// against a non-AWS endpoint no `AWS_*` variable was consulted at all: the open
/// fell through to the EC2 instance metadata service and failed after ~2.2 s.
/// `parse_url` now goes through `parse_url_opts` with the environment, so this
/// asserts the same URL the bench opens via `open_store` also resolves here.
///
/// A HEAD is enough — the question is whether credentials and endpoint resolve,
/// not whether the slide parses, and a full remote open costs a minute.
#[test]
#[ignore = "needs the bucket configured + slides mirrored"]
fn open_url_resolves_config_from_environment() {
    let Some(bucket) = common::remote_bucket() else {
        return;
    };
    // Only the standard spellings are what `open_url` reads. A `.env` carrying
    // just the `BUCKET_*` aliases configures the bench store but says nothing
    // about the public entry point, so there is nothing to assert.
    if std::env::var("AWS_ENDPOINT_URL").is_err() {
        eprintln!(
            "SKIP: AWS_ENDPOINT_URL is not set — `open_url` reads the `AWS_*` names, \
             so set AWS_ENDPOINT_URL (and AWS_REGION) in .env to exercise it."
        );
        return;
    }
    let Some(slide) = common::bench_slide("Aperio/CMU-1.svs") else {
        eprintln!("SKIP: Aperio fixture not available");
        return;
    };
    let url = bucket.url(&slide.open_key);
    eprintln!("open_url target: {url}");

    let (store, path) = slidekit::ObjectStoreFs::parse_url(&url)
        .unwrap_or_else(|e| panic!("parse_url could not build a store for {url}: {e}"));
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let meta = rt
        .block_on(async { object_store::ObjectStoreExt::head(&store, &path).await })
        .unwrap_or_else(|e| {
            panic!(
                "HEAD via parse_url failed for {url}: {e}\n\
                 → `open_url` is not honouring the endpoint/credentials in the environment."
            )
        });
    assert!(meta.size > 0, "HEAD reported an empty object for {url}");
    eprintln!("open_url path works: HEAD returned {} bytes", meta.size);
}

struct Row {
    vendor: String,
    dims: String,
    local_open_ms: f64,
    local_read_ms: f64,
    remote_open_ms: f64,
    remote_read_ms: f64,
    remote_warm_ms: f64,
    open_traffic: Traffic,
    read_traffic: Traffic,
    /// Size of the whole slide on disk — what a download-first reader would pay.
    slide_bytes: u64,
    pixels_match: bool,
}

#[test]
#[ignore = "bench: needs a warm local cache + slides mirrored to the bucket"]
fn bench_remote_vs_local() {
    let Some(bucket) = common::remote_bucket() else {
        return; // remote_bucket() explains why
    };
    eprintln!(
        "Reading from s3://{}/{}/ (byte-range requests, no download)\n",
        bucket.bucket, bucket.root
    );

    // A remote open is round-trip-bound and can take minutes per slide, so allow
    // narrowing the run to a substring of the case path while investigating one
    // vendor: SLIDEKIT_BENCH_ONLY=Aperio, or a comma-separated list.
    let only: Vec<String> = std::env::var("SLIDEKIT_BENCH_ONLY")
        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();

    // Total wall-clock budget, checked between slides. Until read coalescing
    // lands, one round-trip-bound vendor can run for many minutes (Hamamatsu's
    // NDPI restart-marker scan walks a 198 MB strip a few bytes at a time), and
    // a bench that never prints a table is not a bench. Remaining slides are
    // reported as not-run rather than silently dropped. 0 disables the budget.
    let budget = std::time::Duration::from_secs(
        std::env::var("SLIDEKIT_BENCH_BUDGET_SECS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1800),
    );
    let started = Instant::now();

    let mut rows = Vec::new();
    let mut mismatches = Vec::new();
    let mut not_run = Vec::new();
    for &(case, level) in common::BENCH_CASES {
        if !only.is_empty() && !only.iter().any(|f| case.contains(f.as_str())) {
            continue;
        }
        if !budget.is_zero() && started.elapsed() > budget {
            not_run.push(case);
            continue;
        }
        let Some(slide) = common::bench_slide(case) else {
            eprintln!("SKIP: {case} not available locally");
            continue;
        };
        // Report per slide as we go: the whole table only prints at the end, and
        // a run this slow is unusable if there is no sign of progress.
        eprintln!("→ {case} ...");

        // ── local baseline, and the region both readers will compare ──
        let t = Instant::now();
        let local = match OpenSlide::open(&slide.local_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("SKIP {case}: local open failed: {e}");
                continue;
            }
        };
        let local_open_ms = ms(t.elapsed());

        let (lw, lh) = local.level_dimensions(level).unwrap_or((0, 0));
        let w = REGION.min(lw);
        let h = REGION.min(lh);
        let ds = local.level_downsample(level).unwrap_or(1.0);
        // Pick the window locally and reuse it remotely, so the remote numbers
        // time the read itself rather than a second tissue search.
        let (x0, y0) = common::tissue_region(&local, level, w, h).unwrap_or_else(|| {
            (
                (((lw - w) / 2) as f64 * ds) as i64,
                (((lh - h) / 2) as f64 * ds) as i64,
            )
        });

        let t = Instant::now();
        let local_px = match local.read_region(x0, y0, level, w, h) {
            Ok(px) => px,
            Err(e) => {
                eprintln!("SKIP {case}: local read failed: {e}");
                continue;
            }
        };
        let local_read_ms = ms(t.elapsed());

        // A second window, offset by one region, for the warm-read measurement.
        let (x1, y1) = (
            (x0 + (w as f64 * ds) as i64)
                .min(((lw - w) as f64 * ds) as i64)
                .max(0),
            y0,
        );

        // ── remote: same slide, same region, over ranged GETs ──
        let store = CountingStore::new(bucket.store.clone());
        let key = bucket.key(&slide.open_key);

        let before_open = store.traffic();
        let t = Instant::now();
        let remote = match OpenSlide::open_store(store.clone(), key.clone()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "SKIP {case}: remote open failed for {key}: {e}\n      \
                     (mirrored? run `mise run bench:upload-slides`)"
                );
                continue;
            }
        };
        let remote_open_ms = ms(t.elapsed());
        let open_traffic = store.traffic() - before_open;

        let before_read = store.traffic();
        let t = Instant::now();
        let remote_px = match remote.read_region(x0, y0, level, w, h) {
            Ok(px) => px,
            Err(e) => {
                eprintln!("SKIP {case}: remote read failed: {e}");
                continue;
            }
        };
        let remote_read_ms = ms(t.elapsed());
        let read_traffic = store.traffic() - before_read;

        // Steady state: warm connection pool, offset tables already fetched.
        let t = Instant::now();
        let warm = remote.read_region(x1, y1, level, w, h);
        let remote_warm_ms = ms(t.elapsed());
        if let Err(e) = warm {
            eprintln!("WARN {case}: warm remote read failed: {e}");
        }

        // Same decoder, same bytes — the pixels must match exactly.
        let pixels_match = local_px == remote_px;
        if !pixels_match {
            let differing = local_px
                .iter()
                .zip(&remote_px)
                .filter(|(a, b)| a != b)
                .count();
            eprintln!(
                "MISMATCH {case}: {differing}/{} pixels differ between local and remote",
                local_px.len()
            );
            mismatches.push(case);
        }

        eprintln!(
            "  {}: open {:.0}ms ({} reqs, {:.1} MB), read {:.0}ms ({} reqs, {:.1} MB), \
             warm read {:.0}ms, pixels {}",
            slide.name,
            remote_open_ms,
            open_traffic.requests,
            mib(open_traffic.bytes),
            remote_read_ms,
            read_traffic.requests,
            mib(read_traffic.bytes),
            remote_warm_ms,
            if pixels_match { "match" } else { "DIFFER" },
        );

        rows.push(Row {
            vendor: slide.name.clone(),
            dims: format!("{lw}x{lh}"),
            local_open_ms,
            local_read_ms,
            remote_open_ms,
            remote_read_ms,
            remote_warm_ms,
            open_traffic,
            read_traffic,
            slide_bytes: slide.total_bytes(),
            pixels_match,
        });
    }

    print_table(&rows);
    if !not_run.is_empty() {
        eprintln!(
            "\n{} slide(s) not run — {:.0}s budget exhausted (raise or disable with \
             SLIDEKIT_BENCH_BUDGET_SECS): {not_run:?}",
            not_run.len(),
            budget.as_secs_f64(),
        );
    }

    assert!(
        !rows.is_empty(),
        "no slide could be read remotely — mirror them first with \
         `mise run bench:upload-slides`"
    );
    // A remote read that disagrees with the local one is a real bug in the I/O
    // layer, not bench noise, so fail rather than warn.
    assert!(
        mismatches.is_empty(),
        "remote pixels differ from local for: {mismatches:?}"
    );
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn print_table(rows: &[Row]) {
    eprintln!();
    eprintln!(
        "{:<20} {:>14} {:>9} {:>9} {:>10} {:>10} {:>10} {:>6}",
        "vendor", "level0", "lc_open", "lc_read", "rm_open", "rm_read", "rm_warm", "px=="
    );
    eprintln!("{}", "-".repeat(96));
    for r in rows {
        eprintln!(
            "{:<20} {:>14} {:>7.1}ms {:>7.1}ms {:>8.1}ms {:>8.1}ms {:>8.1}ms {:>6}",
            r.vendor,
            r.dims,
            r.local_open_ms,
            r.local_read_ms,
            r.remote_open_ms,
            r.remote_read_ms,
            r.remote_warm_ms,
            if r.pixels_match { "yes" } else { "NO" },
        );
    }

    eprintln!();
    eprintln!(
        "{:<20} {:>10} {:>8} {:>6} {:>10} {:>8} {:>6} {:>11} {:>9}",
        "vendor",
        "open_reqs",
        "open_MB",
        "heads",
        "read_reqs",
        "read_MB",
        "lists",
        "slide_MB",
        "fetched%"
    );
    eprintln!("{}", "-".repeat(96));
    for r in rows {
        let fetched = r.open_traffic.bytes + r.read_traffic.bytes;
        let pct = if r.slide_bytes > 0 {
            fetched as f64 / r.slide_bytes as f64 * 100.0
        } else {
            0.0
        };
        eprintln!(
            "{:<20} {:>10} {:>8.2} {:>6} {:>10} {:>8.2} {:>6} {:>11.1} {:>8.3}%",
            r.vendor,
            r.open_traffic.requests,
            mib(r.open_traffic.bytes),
            r.open_traffic.heads + r.read_traffic.heads,
            r.read_traffic.requests,
            mib(r.read_traffic.bytes),
            r.open_traffic.lists + r.read_traffic.lists,
            mib(r.slide_bytes),
            pct,
        );
    }
    eprintln!(
        "\n(lc_* = local disk. rm_open/rm_read = first remote open + read, cold \
         connection pool.\n rm_warm = a second {REGION}x{REGION} region from the \
         already-open remote slide.\n fetched% = bytes pulled for open+read \
         against the slide's full size — what a download-first reader would pay.)"
    );
}
