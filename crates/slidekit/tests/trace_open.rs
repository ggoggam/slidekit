//! Request-level trace of what a *remote* open actually does, run against
//! local files.
//!
//! `bench_remote` answers "how many requests and how many seconds". It cannot
//! answer "which requests, in what order, and how many of them waited on the
//! one before" — and that last number is the whole of remote open latency,
//! because a round trip costs 100–1000× a local read.
//!
//! This harness opens each bench slide through the *same* stack a remote open
//! uses (`ObjectStoreReader` → `BlockingReader` → `ReadAhead`), but over
//! `LocalFileSystem`, and injects a fixed artificial delay into every GET. That
//! turns a local disk into a synthetic object store with a known per-request
//! cost, so the open's *dependency structure* becomes measurable:
//!
//!   - `requests × delay ≈ wall clock`  → fully serialized; latency is the
//!     round-trip chain and nothing else.
//!   - `wall clock ≪ requests × delay`  → some requests overlap, and the
//!     critical path is shorter than the count suggests.
//!
//! It needs no bucket and no credentials, so it runs anywhere the fixtures are
//! cached.
//!
//! ```sh
//! cargo test --release --test trace_open -- --ignored --nocapture
//! SLIDEKIT_TRACE_DELAY_MS=50 SLIDEKIT_TRACE_ONLY=Zeiss \
//!   cargo test --release --test trace_open -- --ignored --nocapture
//! ```

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_util::stream::BoxStream;
use object_store::local::LocalFileSystem;
use object_store::path::Path as StorePath;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result as OsResult,
};
use parking_lot::Mutex;
use slidekit::OpenSlide;

/// Side length of the square patches read from each open slide.
const PATCH: i64 = 1024;

/// Patches read per slide, in a ring around its densest tissue.
const PATCHES: usize = 5;

/// Artificial per-request delay, in milliseconds. Zero measures the request
/// sequence alone; a non-zero value turns the local disk into a synthetic
/// object store whose round trip cost is known exactly.
fn delay() -> Duration {
    Duration::from_millis(
        std::env::var("SLIDEKIT_TRACE_DELAY_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0),
    )
}

/// One request the open issued.
#[derive(Clone, Debug)]
struct Record {
    /// Microseconds from the start of the open to when the request was issued.
    start_us: u64,
    /// Microseconds from the start of the open to when it returned.
    end_us: u64,
    kind: &'static str,
    offset: u64,
    len: u64,
    path: String,
}

/// An [`ObjectStore`] that records every request, with timing, and optionally
/// delays each one.
///
/// `get_ranges` is deliberately not overridden so its default coalescing runs
/// against this wrapper and every request it issues is recorded here — the same
/// reason `bench_remote::CountingStore` does it.
#[derive(Debug)]
struct TracingStore {
    inner: Arc<dyn ObjectStore>,
    delay: Duration,
    origin: Instant,
    log: Mutex<Vec<Record>>,
    /// Requests currently in flight, and the high-water mark.
    inflight: AtomicU64,
    peak_inflight: AtomicU64,
}

impl TracingStore {
    fn new(inner: Arc<dyn ObjectStore>, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            inner,
            delay,
            origin: Instant::now(),
            log: Mutex::new(Vec::new()),
            inflight: AtomicU64::new(0),
            peak_inflight: AtomicU64::new(0),
        })
    }

    /// Forget everything recorded so far and restart the clock.
    fn reset(&self) {
        self.log.lock().clear();
        self.peak_inflight.store(0, Ordering::Relaxed);
    }

    fn records(&self) -> Vec<Record> {
        self.log.lock().clone()
    }

    fn enter(&self) -> u64 {
        let now = self.inflight.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak_inflight.fetch_max(now, Ordering::Relaxed);
        self.origin.elapsed().as_micros() as u64
    }

    fn leave(&self, start_us: u64, kind: &'static str, offset: u64, len: u64, path: &StorePath) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
        self.log.lock().push(Record {
            start_us,
            end_us: self.origin.elapsed().as_micros() as u64,
            kind,
            offset,
            len,
            path: path.as_ref().to_string(),
        });
    }
}

impl std::fmt::Display for TracingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TracingStore({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for TracingStore {
    async fn put_opts(
        &self,
        location: &StorePath,
        payload: PutPayload,
        opts: PutOptions,
    ) -> OsResult<PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &StorePath,
        opts: PutMultipartOptions,
    ) -> OsResult<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(&self, location: &StorePath, options: GetOptions) -> OsResult<GetResult> {
        let start = self.enter();
        let is_head = options.head;
        tokio::time::sleep(self.delay).await;
        let result = self.inner.get_opts(location, options).await?;
        let (kind, offset, len) = if is_head {
            ("HEAD", 0, 0)
        } else {
            (
                "GET",
                result.range.start,
                result.range.end.saturating_sub(result.range.start),
            )
        };
        self.leave(start, kind, offset, len, location);
        Ok(result)
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, OsResult<StorePath>>,
    ) -> BoxStream<'static, OsResult<StorePath>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&StorePath>) -> BoxStream<'static, OsResult<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&StorePath>) -> OsResult<ListResult> {
        let start = self.enter();
        tokio::time::sleep(self.delay).await;
        let result = self.inner.list_with_delimiter(prefix).await?;
        let path = prefix.cloned().unwrap_or_default();
        self.leave(start, "LIST", 0, 0, &path);
        Ok(result)
    }

    async fn copy_opts(
        &self,
        from: &StorePath,
        to: &StorePath,
        options: CopyOptions,
    ) -> OsResult<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// Summarize a trace: how many requests, how much of the wall clock they
/// account for, and how much of that was spent waiting on a *chain* of them.
fn report(name: &str, records: &[Record], wall: Duration, delay: Duration) {
    let gets = records.iter().filter(|r| r.kind == "GET").count();
    let heads = records.iter().filter(|r| r.kind == "HEAD").count();
    let lists = records.iter().filter(|r| r.kind == "LIST").count();
    let bytes: u64 = records.iter().map(|r| r.len).sum();

    // Serial fraction: total request count × delay against measured wall clock.
    // 1.0 means every request waited for the previous one.
    let serial_budget = delay.as_secs_f64() * records.len() as f64;
    let overlap = if serial_budget > 0.0 {
        serial_budget / wall.as_secs_f64()
    } else {
        f64::NAN
    };

    println!(
        "{name}: {} reqs ({gets} GET, {heads} HEAD, {lists} LIST), {:.2} MB, \
         wall {:.2}s, peak inflight {}, effective concurrency {:.2}x",
        records.len(),
        bytes as f64 / (1024.0 * 1024.0),
        wall.as_secs_f64(),
        records
            .iter()
            .map(|r| concurrent_at(records, r.start_us))
            .max()
            .unwrap_or(0),
        overlap,
    );

    // Per-file breakdown: which object the requests went to.
    let mut by_path: std::collections::BTreeMap<&str, (usize, u64)> = Default::default();
    for r in records {
        let e = by_path.entry(r.path.as_str()).or_default();
        e.0 += 1;
        e.1 += r.len;
    }
    if by_path.len() > 1 {
        for (path, (n, b)) in &by_path {
            println!("    {n:>4} reqs {:>9.2} MB  {path}", *b as f64 / 1048576.0);
        }
    }

    if std::env::var_os("SLIDEKIT_TRACE_FULL").is_some() {
        for (i, r) in records.iter().enumerate() {
            println!(
                "    #{i:<4} {:>8.1}–{:>8.1} ms  {:<4} {:>12}+{:<9} {}",
                r.start_us as f64 / 1000.0,
                r.end_us as f64 / 1000.0,
                r.kind,
                r.offset,
                r.len,
                r.path,
            );
        }
    }
}

/// How many requests were in flight at `t` (microseconds).
fn concurrent_at(records: &[Record], t: u64) -> usize {
    records
        .iter()
        .filter(|r| r.start_us <= t && t < r.end_us)
        .count()
}

#[test]
#[ignore = "requires cached fixtures; run manually"]
fn trace_remote_open() {
    let delay = delay();
    let only = std::env::var("SLIDEKIT_TRACE_ONLY").ok();
    println!(
        "per-request delay: {} ms  (SLIDEKIT_TRACE_DELAY_MS)",
        delay.as_millis()
    );

    for (case, _) in common::BENCH_CASES {
        if let Some(filter) = &only
            && !filter.split(',').any(|f| case.contains(f.trim()))
        {
            continue;
        }
        let Some(slide) = common::bench_slide(case) else {
            continue;
        };

        // `..` segments are illegal in an object key, and the fixture paths are
        // built relative to the crate directory.
        let path = slide
            .local_path
            .canonicalize()
            .expect("fixture path resolves");
        let local = LocalFileSystem::new();
        let store = TracingStore::new(Arc::new(local), delay);
        let key = StorePath::from_absolute_path(&path).expect("absolute key");

        // Warm the OS page cache so the trace measures the request *structure*,
        // not the first read of a cold file from disk.
        let _ = std::fs::metadata(&path);

        store.reset();
        let started = Instant::now();
        let opened = OpenSlide::open_store(store.clone(), key);
        let wall = started.elapsed();

        let opened = match opened {
            Ok(s) => {
                report(
                    &format!("{} open", slide.name),
                    &store.records(),
                    wall,
                    delay,
                );
                s
            }
            Err(e) => {
                println!("{}: FAILED: {e}", slide.name);
                continue;
            }
        };

        // Steady-state reads from the already-open slide, over windows chosen on
        // tissue so the decode work is real. Reported per patch, since that is
        // what a viewer or a tiling job actually pays.
        let Some(primary) = common::tissue_region(&opened, 0, PATCH, PATCH) else {
            println!("    (no tissue window found; patches not measured)");
            continue;
        };
        let windows = common::patch_grid(&opened, 0, PATCH, PATCH, primary, PATCHES);
        if windows.is_empty() {
            continue;
        }

        store.reset();
        let started = Instant::now();
        for (x, y) in &windows {
            opened
                .read_region(*x, *y, 0, PATCH, PATCH)
                .expect("read patch");
        }
        let wall = started.elapsed();
        let records = store.records();
        println!(
            "{} patches: {} windows, {:.1} reqs/patch, {:.2} MB/patch, {:.0} ms/patch",
            slide.name,
            windows.len(),
            records.len() as f64 / windows.len() as f64,
            records.iter().map(|r| r.len).sum::<u64>() as f64 / (1048576.0 * windows.len() as f64),
            wall.as_secs_f64() * 1000.0 / windows.len() as f64,
        );
    }
}
