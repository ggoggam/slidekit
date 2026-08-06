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
//!   - remote `open()` latency over byte-range requests,
//!   - a set of distinct 1024×1024 patches read from the open remote slide: the
//!     first cold, the rest reusing the warm connection pool and the resolved
//!     tile-offset tables — the steady state a viewer or a tile-extraction job
//!     actually sees,
//!   - how many HTTP requests and bytes each phase cost, including the mean per
//!     steady-state patch,
//!   - the endpoint's own per-request cost, so the seconds can be read as
//!     round trips rather than taken at face value.
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
//! Correctness is checked twice over, because the two comparisons answer
//! different questions:
//!
//!   - **Remote against local slidekit, exactly.** Both paths run the same
//!     decoder over the same bytes, so the ARGB output must be identical
//!     pixel-for-pixel. Any difference means the I/O layer (range math, `SlideFs`
//!     sibling resolution, size clamping) corrupted or misplaced data — which is
//!     precisely the risk a remote read path introduces.
//!   - **Remote against OpenSlide, tolerantly.** Exact agreement with our own
//!     local reader only proves the two are self-consistent; it cannot catch a
//!     decode or geometry bug both share. So every patch is also compared with
//!     `slidetool` reading the same slide from disk (OpenSlide has no remote
//!     backend), with the ±1–2 per-channel tolerance independent JPEG decoders
//!     require.

mod common;

use common::slidetool;
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

/// How far apart the scattered patches must be, in window widths. Six is enough
/// that neither the 64 KiB–4 MiB read-ahead block nor the tile cache from one
/// patch can serve any part of the next, on every slide in this set.
const SCATTER_SEP: f64 = 6.0;

/// Least share of a window that must be stained specimen for it to be worth
/// comparing.
///
/// The tissue search works from the coarsest pyramid level, which is a thumbnail:
/// a window it likes can still turn out to be mostly background at full
/// resolution. Screening the candidates at level 0 and skipping the empty ones is
/// what keeps "the patches agree with OpenSlide" from being a claim about white
/// space — a blank window agrees between any two readers, because there is no
/// tile content in it to get wrong.
///
/// Ten percent is about a hundred thousand stained pixels in a 1024×1024 window —
/// far more than enough for a misplaced or mis-decoded tile to show up — and
/// deliberately not stricter: at 20% two slides in this set could offer no
/// qualifying windows at one of the two localities, and losing a measurement is a
/// worse outcome than admitting a window is only a tenth specimen. Override with
/// `SLIDEKIT_BENCH_MIN_TISSUE` (a percentage) when investigating.
fn min_tissue() -> f64 {
    std::env::var("SLIDEKIT_BENCH_MIN_TISSUE")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|pct| pct / 100.0)
        .unwrap_or(0.10)
}

/// Read a positive integer from the environment, falling back to `default`.
fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}

/// Where to write per-patch images, when `SLIDEKIT_BENCH_DUMP` asks for them.
///
/// Dumping is off by default and deliberately so: it writes three PNGs per patch,
/// which for the full set is several hundred megabytes, and the file I/O sits
/// between the timed reads.
struct DumpCtx {
    dir: std::path::PathBuf,
    /// Longest side of the contact-sheet thumbnails, in pixels.
    thumb: u32,
    /// Multiplier applied to the difference image, so decoder noise stays dark.
    gain: u32,
}

/// Everything reading one patch needs. A struct rather than eleven parameters,
/// and shared by both patch sets so they are measured identically.
struct PatchCtx<'a> {
    remote: &'a OpenSlide,
    store: &'a CountingStore,
    local_path: &'a std::path::Path,
    /// Distinguishes this patch's temp PNG from every other one.
    tag: &'a str,
    level: i32,
    w: i64,
    h: i64,
    have_slidetool: bool,
    /// Vendor label, used for the dump directory.
    vendor: &'a str,
    /// `adjacent0`, `scattered3`, … — identifies the patch in the dump and CSV.
    label: &'a str,
    dump: Option<&'a DumpCtx>,
}

/// Write one patch's images: what slidekit returned, what OpenSlide returned, and
/// where they differ.
///
/// Full resolution for looking closely, plus a thumbnail of each for the contact
/// sheet — a browser asked to lay out several hundred 1024×1024 PNGs at once is
/// not a usable way to compare them.
///
/// Only the remote pixels are written as "slidekit", because the bench asserts
/// they are identical to the local ones; the local copy is written **only** if that
/// assertion failed, in which case the pair is the whole point.
fn write_patch_images(
    dump: &DumpCtx,
    ctx: &PatchCtx,
    remote_px: &[u32],
    osl_rgba: Option<&[u8]>,
    local_px: &[u32],
) {
    let (w, h) = (ctx.w, ctx.h);
    let dir = dump.dir.join(ctx.vendor.replace('/', "-"));
    let thumbs = dir.join("thumbs");
    if std::fs::create_dir_all(&thumbs).is_err() {
        return;
    }

    let write = |name: &str, rgba: &[u8]| {
        slidetool::dump_png(&dir.join(format!("{}_{name}.png", ctx.label)), rgba, w, h);
        let (small, tw, th) = slidetool::downscale(rgba, w, h, dump.thumb);
        slidetool::dump_png(
            &thumbs.join(format!("{}_{name}.png", ctx.label)),
            &small,
            tw,
            th,
        );
    };

    write("slidekit", &slidetool::argb_to_rgba(remote_px));
    if let Some(rgba) = osl_rgba {
        write("openslide", rgba);
        write(
            "diff",
            &slidetool::diff_image(remote_px, rgba, w, h, dump.gain),
        );
    }
    if remote_px != local_px {
        write("local-MISMATCH", &slidetool::argb_to_rgba(local_px));
    }
}

/// A window read from local disk, kept so the remote comparison does not have to
/// read it a second time (which would measure a warm cache).
struct LocalPatch {
    at: (i64, i64),
    px: Vec<u32>,
    ms: f64,
    tissue_frac: f64,
}

/// Read `candidates` from local disk in order and keep the first `want` that hold
/// at least [`MIN_TISSUE`] specimen.
///
/// Returns what it accepted plus how many it rejected, so the bench can say out
/// loud that it looked at more windows than it reports.
fn select_patches(
    slide: &OpenSlide,
    level: i32,
    w: i64,
    h: i64,
    candidates: &[(i64, i64)],
    want: usize,
) -> (Vec<LocalPatch>, usize) {
    let mut accepted = Vec::with_capacity(want);
    let mut rejected = 0usize;
    for &(x, y) in candidates {
        if accepted.len() == want {
            break;
        }
        let t = Instant::now();
        let Ok(px) = slide.read_region(x, y, level, w, h) else {
            continue;
        };
        let elapsed = ms(t.elapsed());
        let tissue_frac = slidetool::tissue_fraction(&px);
        if tissue_frac < min_tissue() {
            rejected += 1;
            continue;
        }
        accepted.push(LocalPatch {
            at: (x, y),
            px,
            ms: elapsed,
            tissue_frac,
        });
    }
    (accepted, rejected)
}

/// Read one window three ways — local slidekit, remote slidekit, and OpenSlide —
/// and record what each cost and how far they agree.
fn read_patch(ctx: &PatchCtx, local: &LocalPatch) -> Result<Patch, String> {
    let (x, y) = local.at;
    let at = local.at;
    let (level, w, h) = (ctx.level, ctx.w, ctx.h);
    let local_px = &local.px;
    let local_ms = local.ms;

    let before = ctx.store.traffic();
    let t = Instant::now();
    let remote_px = ctx
        .remote
        .read_region(x, y, level, w, h)
        .map_err(|e| format!("remote read failed: {e}"))?;
    let remote_ms = ms(t.elapsed());
    let traffic = ctx.store.traffic() - before;

    // Same decoder, same bytes — local and remote must match exactly.
    let matches_local = *local_px == remote_px;
    if !matches_local {
        let differing = local_px
            .iter()
            .zip(&remote_px)
            .filter(|(a, b)| a != b)
            .count();
        eprintln!(
            "  MISMATCH {} at ({x},{y}): {differing}/{} pixels differ between local and remote",
            ctx.tag,
            remote_px.len()
        );
    }

    // Independent reference: OpenSlide reading the same slide from disk over the
    // same window. This is what makes the extracted patch *correct* rather than
    // merely self-consistent with our own local reader.
    let mut osl_rgba: Option<Vec<u8>> = None;
    let (osl_ms, osl_diff) = if ctx.have_slidetool {
        match slidetool::region_read(ctx.local_path, ctx.tag, x, y, level, w, h) {
            Some((osl_ms, rgba)) => {
                let d = slidetool::diff(&remote_px, &rgba, (w * h) as usize);
                osl_rgba = Some(rgba);
                if d.exceeds_ceiling() {
                    eprintln!(
                        "  WARN {} at ({x},{y}): remote pixels differ from OpenSlide by \
                         {:.2} per channel over {:.0}% opaque overlap",
                        ctx.tag,
                        d.mean,
                        d.opaque_frac * 100.0
                    );
                }
                (Some(osl_ms), Some(d))
            }
            None => (None, None),
        }
    } else {
        (None, None)
    };

    if let Some(dump) = ctx.dump {
        write_patch_images(dump, ctx, &remote_px, osl_rgba.as_deref(), local_px);
    }

    Ok(Patch {
        at,
        local_ms,
        remote_ms,
        traffic,
        osl_ms,
        osl_diff,
        matches_local,
        tissue_frac: local.tissue_frac,
        label: ctx.label.to_string(),
    })
}

/// One patch's cost, on both sides of the comparison.
struct Patch {
    /// Top-left in level-0 coordinates.
    at: (i64, i64),
    local_ms: f64,
    remote_ms: f64,
    /// Store traffic the remote read of this patch cost.
    traffic: Traffic,
    /// Whole `slidetool region read` process over the same window, local file.
    osl_ms: Option<f64>,
    /// Remote pixels against OpenSlide's, tolerantly.
    osl_diff: Option<slidetool::Diff>,
    /// Remote pixels against local slidekit's, exactly.
    matches_local: bool,
    /// How much of the window is actually stained specimen. A pixel comparison
    /// over white background or black padding proves nothing, so this says how
    /// much the agreement is worth.
    tissue_frac: f64,
    /// `adjacent0`, `scattered3`, … — matches the dumped filenames.
    label: String,
}

struct Row {
    vendor: String,
    dims: String,
    local_open_ms: f64,
    remote_open_ms: f64,
    open_traffic: Traffic,
    /// Patches adjacent to one another, `[0]` cold: a viewer panning.
    patches: Vec<Patch>,
    /// Patches spread across the specimen: a tiling job, and the case that
    /// defeats read-ahead.
    scattered: Vec<Patch>,
    /// Size of the whole slide on disk — what a download-first reader would pay.
    slide_bytes: u64,
}

impl Row {
    /// Every patch read from this slide, in both sets.
    fn all_patches(&self) -> impl Iterator<Item = &Patch> {
        self.patches.iter().chain(self.scattered.iter())
    }

    /// Traffic for every patch read.
    fn patch_traffic(&self) -> Traffic {
        self.all_patches()
            .fold(Traffic::default(), |acc, p| Traffic {
                requests: acc.requests + p.traffic.requests,
                bytes: acc.bytes + p.traffic.bytes,
                lists: acc.lists + p.traffic.lists,
                heads: acc.heads + p.traffic.heads,
            })
    }

    /// Patches after the first: the slide is open, offset tables are resolved and
    /// the connection pool is warm, so this is the steady state a caller
    /// extracting many patches actually sees.
    fn steady(&self) -> &[Patch] {
        self.patches.get(1..).unwrap_or(&[])
    }

    /// Median steady-state patch latency.
    fn steady_median_ms(&self) -> Option<f64> {
        let mut v: Vec<f64> = self.steady().iter().map(|p| p.remote_ms).collect();
        (!v.is_empty()).then(|| slidetool::median(&mut v))
    }

    /// Mean requests per steady-state patch.
    fn steady_reqs_per_patch(&self) -> Option<f64> {
        let s = self.steady();
        (!s.is_empty())
            .then(|| s.iter().map(|p| p.traffic.requests).sum::<u64>() as f64 / s.len() as f64)
    }

    /// Mean bytes per steady-state patch.
    fn steady_bytes_per_patch(&self) -> Option<f64> {
        let s = self.steady();
        (!s.is_empty())
            .then(|| s.iter().map(|p| p.traffic.bytes).sum::<u64>() as f64 / s.len() as f64)
    }

    /// Median scattered-patch latency, requests and bytes: the same steady state
    /// with locality removed.
    fn scattered_median_ms(&self) -> Option<f64> {
        let mut v: Vec<f64> = self.scattered.iter().map(|p| p.remote_ms).collect();
        (!v.is_empty()).then(|| slidetool::median(&mut v))
    }

    fn scattered_reqs_per_patch(&self) -> Option<f64> {
        (!self.scattered.is_empty()).then(|| {
            self.scattered
                .iter()
                .map(|p| p.traffic.requests)
                .sum::<u64>() as f64
                / self.scattered.len() as f64
        })
    }

    fn scattered_bytes_per_patch(&self) -> Option<f64> {
        (!self.scattered.is_empty()).then(|| {
            self.scattered.iter().map(|p| p.traffic.bytes).sum::<u64>() as f64
                / self.scattered.len() as f64
        })
    }

    /// Median local patch latency, as the disk-side baseline.
    fn local_median_ms(&self) -> f64 {
        let mut v: Vec<f64> = self.all_patches().map(|p| p.local_ms).collect();
        slidetool::median(&mut v)
    }

    /// Best (lowest) whole-process `slidetool` time over the patch set. The floor
    /// is the fair reference: every one of those processes re-opens the slide.
    fn osl_best_ms(&self) -> Option<f64> {
        self.all_patches()
            .filter_map(|p| p.osl_ms)
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.min(v)))
            })
    }

    /// The worst pixel disagreement with OpenSlide across the patch set, and how
    /// many patches could be compared at all.
    fn worst_osl_diff(&self) -> (Option<slidetool::Diff>, usize) {
        let diffs: Vec<slidetool::Diff> = self.all_patches().filter_map(|p| p.osl_diff).collect();
        let n = diffs.len();
        let worst = diffs
            .into_iter()
            .fold(None, |acc: Option<slidetool::Diff>, d| {
                Some(match acc {
                    None => d,
                    Some(a) => slidetool::Diff {
                        mean: a.mean.max(d.mean),
                        max: a.max.max(d.max),
                        // The least overlap is the honest headline: it bounds how
                        // much of the set the diff actually judged.
                        opaque_frac: a.opaque_frac.min(d.opaque_frac),
                    },
                })
            });
        (worst, n)
    }

    /// Mean and minimum specimen content over the patch set.
    fn tissue(&self) -> (f64, f64) {
        let v: Vec<f64> = self.all_patches().map(|p| p.tissue_frac).collect();
        if v.is_empty() {
            return (0.0, 0.0);
        }
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        let min = v.iter().copied().fold(f64::INFINITY, f64::min);
        (mean, min)
    }

    fn all_match_local(&self) -> bool {
        self.all_patches().all(|p| p.matches_local)
    }

    /// The patch that disagrees with OpenSlide most, for the report to name.
    fn worst_patch(&self) -> Option<&Patch> {
        self.all_patches()
            .filter(|p| p.osl_diff.is_some_and(|d| d.meaningful()))
            .max_by(|a, b| {
                let (a, b) = (a.osl_diff.unwrap().mean, b.osl_diff.unwrap().mean);
                a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
            })
    }
}

/// Per-request cost of the endpoint itself, measured with tiny ranged GETs on an
/// object already in the bucket.
///
/// Every remote latency in this bench is a multiple of this number, so quoting
/// it is what makes the seconds interpretable — and what separates "slidekit
/// issues too many requests" from "this endpoint is slow". Requests are issued
/// sequentially on the store's own (pooled) connections, at increasing offsets so
/// a server-side cache of one range cannot flatter the result.
fn probe_request_latency(store: &Arc<dyn ObjectStore>, key: &StorePath, n: usize) -> Option<f64> {
    let rt = tokio::runtime::Runtime::new().ok()?;
    let mut samples = Vec::with_capacity(n);
    for i in 0..n {
        let offset = (i as u64) * 4096;
        let t = Instant::now();
        let ok = rt.block_on(async {
            let opts = GetOptions {
                range: Some((offset..offset + 1).into()),
                ..Default::default()
            };
            match store.get_opts(key, opts).await {
                Ok(r) => r.bytes().await.is_ok(),
                Err(_) => false,
            }
        });
        if !ok {
            return None;
        }
        samples.push(ms(t.elapsed()));
    }
    let median = slidetool::median(&mut samples);
    eprintln!(
        "endpoint per-request cost over {n} sequential 1-byte ranged GETs: \
         min {:.0}ms, median {:.0}ms, max {:.0}ms",
        samples.first().copied().unwrap_or(0.0),
        median,
        samples.last().copied().unwrap_or(0.0),
    );
    Some(median)
}

#[test]
#[ignore = "bench: needs a warm local cache + slides mirrored to the bucket"]
fn bench_remote_vs_local() {
    let Some(bucket) = common::remote_bucket() else {
        return; // remote_bucket() explains why
    };
    eprintln!(
        "Reading from s3://{}/{}/ (byte-range requests, no download)",
        bucket.bucket, bucket.root
    );

    // OpenSlide is the reference for "is the patch right". It has no remote
    // backend, so it reads the same slide from local disk — which is the only way
    // to check a remote read against an independent implementation at all.
    let have_slidetool = slidetool::available();
    slidetool::report_version();
    if have_slidetool {
        eprintln!("(OpenSlide has no remote backend, so it reads the local copy of each slide)");
    } else {
        eprintln!(
            "NOTE: remote pixels will only be checked against local slidekit, not against \
             OpenSlide."
        );
    }

    // A remote open is round-trip-bound and can take minutes per slide, so allow
    // narrowing the run to a substring of the case path while investigating one
    // vendor: SLIDEKIT_BENCH_ONLY=Aperio, or a comma-separated list.
    let only: Vec<String> = std::env::var("SLIDEKIT_BENCH_ONLY")
        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();

    // How many patches to extract per slide. One patch measures one path through
    // the tile grid and cannot show the steady state; the default reads five.
    let patch_count: usize = std::env::var("SLIDEKIT_BENCH_PATCHES")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(5);
    assert!(patch_count > 0, "SLIDEKIT_BENCH_PATCHES must be at least 1");

    // Total wall-clock budget, checked between slides. A bench that never prints
    // a table is not a bench, so remaining slides are reported as not-run rather
    // than silently dropped. 0 disables the budget.
    let budget = std::time::Duration::from_secs(
        std::env::var("SLIDEKIT_BENCH_BUDGET_SECS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1800),
    );
    let started = Instant::now();

    // SLIDEKIT_BENCH_DUMP=<dir> writes every patch as PNGs for visual comparison:
    // slidekit's output, OpenSlide's, and an amplified difference image.
    let dump = std::env::var_os("SLIDEKIT_BENCH_DUMP").map(|d| DumpCtx {
        dir: std::path::PathBuf::from(d),
        thumb: env_u32("SLIDEKIT_BENCH_THUMB", 320),
        gain: env_u32("SLIDEKIT_BENCH_DIFF_GAIN", 8),
    });
    if let Some(d) = &dump {
        if let Err(e) = std::fs::create_dir_all(&d.dir) {
            panic!("cannot create dump directory {}: {e}", d.dir.display());
        }
        eprintln!(
            "Dumping every patch to {} (thumbnails {} px, difference gain x{})",
            d.dir.display(),
            d.thumb,
            d.gain,
        );
    }

    // Endpoint baseline, before any slide work.
    let probe_key = common::bench_slide("Aperio/CMU-1.svs")
        .map(|s| bucket.key(&s.open_key))
        .unwrap_or_else(|| bucket.key("Aperio/CMU-1.svs"));
    let request_ms = probe_request_latency(&bucket.store, &probe_key, 10);
    eprintln!();

    let mut rows = Vec::new();
    let mut mismatches = Vec::new();
    let mut osl_disagreements = Vec::new();
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
        let slug = slide.name.replace('/', "-");

        // ── local baseline, and the windows both readers will compare ──
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
        // Pick the windows locally and reuse them remotely, so the remote numbers
        // time the reads themselves rather than a second tissue search.
        let primary = common::tissue_neighbourhood(&local, level, w, h).unwrap_or_else(|| {
            (
                (((lw - w) / 2) as f64 * ds) as i64,
                (((lh - h) / 2) as f64 * ds) as i64,
            )
        });
        // Two patch sets, because locality is what decides the request count and
        // one set cannot show both ends of it:
        //   adjacent — a viewer panning, where read-ahead helps;
        //   scattered — a tiling job, where it cannot.
        //
        // Both are drawn from more candidates than are needed and screened at full
        // resolution for actual specimen, so a window the thumbnail-scale search
        // liked but which is blank up close is replaced rather than compared.
        let adjacent_candidates = common::patch_grid(&local, level, w, h, primary, patch_count * 2);
        let scattered_candidates = {
            let mut c = common::tissue_patches(&local, level, w, h, patch_count * 8, SCATTER_SEP);
            // Drop candidates near an adjacent-set window: a read-ahead block or a
            // cached tile from that read would serve part of this one, which is
            // exactly what the scattered set exists to avoid.
            let gap = (w as f64 * ds * 2.0) as i64;
            c.retain(|&(x, y)| {
                adjacent_candidates
                    .iter()
                    .all(|&(ax, ay)| (x - ax).abs() > gap || (y - ay).abs() > gap)
            });
            c
        };

        let (adjacent_local, adj_rejected) =
            select_patches(&local, level, w, h, &adjacent_candidates, patch_count);
        let (scattered_local, scat_rejected) =
            select_patches(&local, level, w, h, &scattered_candidates, patch_count);
        if adj_rejected + scat_rejected > 0 {
            eprintln!(
                "  ({adj_rejected} adjacent + {scat_rejected} scattered candidate windows \
                 skipped: under {:.0}% specimen)",
                min_tissue() * 100.0
            );
        }
        if adjacent_local.is_empty() {
            eprintln!(
                "SKIP {case}: no candidate window held {:.0}% specimen — the tissue search \
                 found nothing to compare on this slide",
                min_tissue() * 100.0
            );
            continue;
        }

        // ── remote: same slide, same windows, over ranged GETs ──
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

        let mut sets: Vec<Vec<Patch>> = Vec::with_capacity(2);
        let mut slide_failed = false;
        for (set_name, set_windows) in [
            ("adjacent", &adjacent_local),
            ("scattered", &scattered_local),
        ] {
            let mut out = Vec::with_capacity(set_windows.len());
            for (i, lp) in set_windows.iter().enumerate() {
                let tag = format!("{slug}-{set_name}{i}");
                let ctx = PatchCtx {
                    remote: &remote,
                    store: &store,
                    local_path: &slide.local_path,
                    tag: &tag,
                    level,
                    w,
                    h,
                    have_slidetool,
                    vendor: &slide.name,
                    label: &format!("{set_name}{i}"),
                    dump: dump.as_ref(),
                };
                match read_patch(&ctx, lp) {
                    Ok(patch) => {
                        if !patch.matches_local {
                            mismatches.push(format!("{case} {set_name}[{i}]"));
                        }
                        if patch.osl_diff.is_some_and(|d| d.exceeds_ceiling()) {
                            osl_disagreements.push(format!("{case} {set_name}[{i}]"));
                        }
                        out.push(patch);
                    }
                    Err(e) => {
                        eprintln!("SKIP {case}: {set_name} patch {i} at {:?}: {e}", lp.at);
                        slide_failed = true;
                        break;
                    }
                }
            }
            if slide_failed {
                break;
            }
            sets.push(out);
        }
        if slide_failed || sets.first().is_none_or(|s| s.is_empty()) {
            continue;
        }
        let scattered = sets.pop().filter(|_| sets.len() == 1).unwrap_or_default();
        let patches = sets.pop().unwrap_or_default();

        let row = Row {
            vendor: slide.name.clone(),
            dims: format!("{lw}x{lh}"),
            local_open_ms,
            remote_open_ms,
            open_traffic,
            patches,
            scattered,
            slide_bytes: slide.total_bytes(),
        };

        let (tissue_mean, _) = row.tissue();
        if tissue_mean < 0.05 {
            eprintln!(
                "  NOTE {}: patches average only {:.1}% stained specimen — the tissue search \
                 did not find real content on this slide, so its pixel agreement is weak \
                 evidence.",
                row.vendor,
                tissue_mean * 100.0
            );
        }

        let (worst, compared) = row.worst_osl_diff();
        let show = |v: Option<f64>, unit: &str| {
            v.map(|v| format!("{v:.1}{unit}"))
                .unwrap_or_else(|| "-".to_string())
        };
        eprintln!(
            "  {}: open {:.0}ms ({} reqs, {:.1} MB) | cold patch {:.0}ms | adjacent \
             {} at {}/patch | scattered {} at {}/patch | px==local {} | vs OpenSlide {}",
            row.vendor,
            row.remote_open_ms,
            row.open_traffic.requests,
            mib(row.open_traffic.bytes),
            row.patches[0].remote_ms,
            show(row.steady_median_ms(), "ms"),
            show(row.steady_reqs_per_patch(), " req"),
            show(row.scattered_median_ms(), "ms"),
            show(row.scattered_reqs_per_patch(), " req"),
            if row.all_match_local() { "yes" } else { "NO" },
            match (worst, compared) {
                (Some(d), n) if d.meaningful() =>
                    format!("mean<={:.2} max<={} over {n} patch(es)", d.mean, d.max),
                (Some(_), n) => format!("n/a (too little opaque overlap, {n} patch(es))"),
                (None, _) => "not compared".to_string(),
            },
        );

        rows.push(row);
    }

    print_tables(&rows, request_ms);
    if let Some(d) = &dump {
        write_manifest(&d.dir, &rows);
    }
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
    // Disagreeing with OpenSlide past the tolerance is a decode/geometry bug.
    assert!(
        osl_disagreements.is_empty(),
        "remote pixels differ from OpenSlide beyond tolerance for: {osl_disagreements:?}"
    );
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Write one CSV row per dumped patch, so the images can be sorted and filtered by
/// the numbers that go with them instead of by eye alone.
fn write_manifest(dir: &std::path::Path, rows: &[Row]) {
    let mut out = String::from(
        "vendor,dir,patch,locality,x,y,tissue_pct,matches_local,mean_diff,max_diff,\
         opaque_pct,remote_ms,local_ms,openslide_ms,requests,bytes\n",
    );
    for r in rows {
        for p in r.all_patches() {
            let d = p.osl_diff;
            out.push_str(&format!(
                "{},{},{},{},{},{},{:.1},{},{},{},{},{:.1},{:.1},{},{},{}\n",
                r.vendor,
                r.vendor.replace('/', "-"),
                p.label,
                p.label.trim_end_matches(|c: char| c.is_ascii_digit()),
                p.at.0,
                p.at.1,
                p.tissue_frac * 100.0,
                p.matches_local,
                d.map(|d| format!("{:.3}", d.mean)).unwrap_or_default(),
                d.map(|d| d.max.to_string()).unwrap_or_default(),
                d.map(|d| format!("{:.1}", d.opaque_frac * 100.0))
                    .unwrap_or_default(),
                p.remote_ms,
                p.local_ms,
                p.osl_ms.map(|v| format!("{v:.1}")).unwrap_or_default(),
                p.traffic.requests,
                p.traffic.bytes,
            ));
        }
    }
    let path = dir.join("patches.csv");
    match std::fs::write(&path, out) {
        Ok(()) => eprintln!("\nWrote {}", path.display()),
        Err(e) => {
            eprintln!("WARN: cannot write {}: {e}", path.display());
            return;
        }
    }
    write_contact_sheet(dir);
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn print_tables(rows: &[Row], request_ms: Option<f64>) {
    let opt = |v: Option<f64>, unit: &str| {
        v.map(|v| format!("{v:.1}{unit}"))
            .unwrap_or_else(|| "-".to_string())
    };

    // ── latency ──
    eprintln!();
    eprintln!(
        "{:<22} {:>14} {:>9} {:>9} {:>9} {:>9} {:>10} {:>10} {:>10}",
        "vendor",
        "level0",
        "lc_open",
        "lc_read",
        "rm_open",
        "rm_cold",
        "rm_adjac",
        "rm_scatt",
        "osl_proc"
    );
    eprintln!("{}", "-".repeat(112));
    for r in rows {
        eprintln!(
            "{:<22} {:>14} {:>7.1}ms {:>7.1}ms {:>7.1}ms {:>7.1}ms {:>10} {:>10} {:>10}",
            r.vendor,
            r.dims,
            r.local_open_ms,
            r.local_median_ms(),
            r.remote_open_ms,
            r.patches[0].remote_ms,
            opt(r.steady_median_ms(), "ms"),
            opt(r.scattered_median_ms(), "ms"),
            opt(r.osl_best_ms(), "ms"),
        );
    }
    eprintln!(
        "\n(lc_* = local disk, in-process; lc_read is the median over every patch.\n \
         rm_open/rm_cold = first remote open + its first patch, cold connection pool.\n \
         rm_adjac = median of the remaining ADJACENT patches (a viewer panning: read-ahead\n \
         from one patch can cover the next).\n \
         rm_scatt = median of the SCATTERED patches, {SCATTER_SEP:.0} window-widths apart on\n \
         tissue (a tiling job: nothing cached can serve them).\n \
         osl_proc = best whole `slidetool region read` process over the same windows on the\n \
         local file: startup + open + read + PNG encode. OpenSlide has no remote backend, so\n \
         it is a correctness reference and a local-latency reference, not a remote one.)"
    );

    // ── traffic: the number that ports to other infrastructure ──
    eprintln!();
    eprintln!(
        "{:<22} {:>9} {:>8} {:>6} {:>9} {:>9} {:>8} {:>9} {:>8} {:>10} {:>9}",
        "vendor",
        "open_req",
        "open_MB",
        "heads",
        "cold_req",
        "adjac_req",
        "adjac_MB",
        "scatt_req",
        "scatt_MB",
        "slide_MB",
        "fetched%"
    );
    eprintln!("{}", "-".repeat(124));
    for r in rows {
        let patch = r.patch_traffic();
        let fetched = r.open_traffic.bytes + patch.bytes;
        let pct = if r.slide_bytes > 0 {
            fetched as f64 / r.slide_bytes as f64 * 100.0
        } else {
            0.0
        };
        eprintln!(
            "{:<22} {:>9} {:>8.2} {:>6} {:>9} {:>9} {:>8} {:>9} {:>8} {:>10.1} {:>8.3}%",
            r.vendor,
            r.open_traffic.requests,
            mib(r.open_traffic.bytes),
            r.open_traffic.heads + patch.heads,
            r.patches[0].traffic.requests,
            r.steady_reqs_per_patch()
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "-".into()),
            r.steady_bytes_per_patch()
                .map(|v| format!("{:.2}", v / (1024.0 * 1024.0)))
                .unwrap_or_else(|| "-".into()),
            r.scattered_reqs_per_patch()
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "-".into()),
            r.scattered_bytes_per_patch()
                .map(|v| format!("{:.2}", v / (1024.0 * 1024.0)))
                .unwrap_or_else(|| "-".into()),
            mib(r.slide_bytes),
            pct,
        );
    }
    eprintln!(
        "\n(adjac_* / scatt_* = mean requests and MB per patch, after the cold one, for the two\n \
         localities. The pair brackets what a patch costs: adjacent is the best case a viewer\n \
         sees, scattered the worst case a tiling job pays. fetched% = every byte the open and\n \
         both patch sets pulled, against the slide's full size — what a download-first reader\n \
         would pay to do the same work.)"
    );

    // ── correctness ──
    eprintln!();
    eprintln!(
        "{:<22} {:>8} {:>11} {:>8} {:>9} {:>8} {:>9} {:>13} {:>20}",
        "vendor",
        "patches",
        "px==local",
        "osl_cmp",
        "mean_d",
        "max_d",
        "opaque%",
        "tissue%",
        "worst_window"
    );
    eprintln!("{}", "-".repeat(112));
    for r in rows {
        let (worst, compared) = r.worst_osl_diff();
        let meaningful = worst.is_some_and(|d| d.meaningful());
        let cell = |s: String| if meaningful { s } else { "n/a".to_string() };
        eprintln!(
            "{:<22} {:>8} {:>11} {:>8} {:>9} {:>8} {:>9} {:>13} {:>20}",
            r.vendor,
            format!("{}+{}", r.patches.len(), r.scattered.len()),
            if r.all_match_local() { "yes" } else { "NO" },
            compared,
            worst
                .map(|d| cell(format!("{:.2}", d.mean)))
                .unwrap_or_else(|| "-".into()),
            worst
                .map(|d| cell(d.max.to_string()))
                .unwrap_or_else(|| "-".into()),
            worst
                .map(|d| format!("{:.0}%", d.opaque_frac * 100.0))
                .unwrap_or_else(|| "-".into()),
            {
                let (mean, min) = r.tissue();
                format!("{:.0}/{:.0} min", mean * 100.0, min * 100.0)
            },
            r.worst_patch()
                .map(|p| format!("({},{})", p.at.0, p.at.1))
                .unwrap_or_else(|| "-".into()),
        );
    }
    eprintln!(
        "\n(px==local: remote ARGB identical to local ARGB, every patch, exactly — the I/O layer\n \
         must not alter a pixel. osl_cmp = patches compared against OpenSlide; mean_d/max_d are\n \
         the WORST per-channel RGB difference over any patch, measured only where both readers\n \
         are opaque, and opaque% is the smallest such overlap. Independent JPEG/JP2K decoders\n \
         differ by a few counts, so small values are expected rather than exact equality;\n \
         the bench fails past a mean of {:.0}. patches = adjacent+scattered; worst_window is the\n \
         level-0 top-left of the patch that disagrees most, so it can be re-read by hand.\n \
         tissue% = mean / worst-case share of the window that is stained specimen rather than\n \
         white background or black padding: agreement over an empty window is not evidence,\n \
         so a low figure here weakens that row's claim no matter how small mean_d is.)",
        slidetool::MEAN_DIFF_CEILING
    );

    // ── portable summary ──
    let total_open: u64 = rows.iter().map(|r| r.open_traffic.requests).sum();
    let fmt_mean = |v: f64| {
        if v.is_nan() {
            "no".to_string()
        } else {
            format!("{v:.1}")
        }
    };
    let mean = |f: fn(&Row) -> Option<f64>| {
        let vals: Vec<f64> = rows.iter().filter_map(f).collect();
        if vals.is_empty() {
            f64::NAN
        } else {
            vals.iter().sum::<f64>() / vals.len() as f64
        }
    };
    eprintln!();
    eprintln!(
        "Totals: {} slides, {} requests to open them all. Per {REGION}x{REGION} patch, \
         once open: {} requests adjacent, {} requests scattered.",
        rows.len(),
        total_open,
        fmt_mean(mean(Row::steady_reqs_per_patch)),
        fmt_mean(mean(Row::scattered_reqs_per_patch)),
    );
    if let Some(rtt) = request_ms {
        eprintln!(
            "At this endpoint's {rtt:.0}ms per request, request COUNT is what the remote seconds \
             above are made of; the counts port to other infrastructure, the seconds do not."
        );
    }
    // Patch coordinates, so a suspicious row can be re-read by hand.
    eprintln!("\nWindows read (level-0 top-left, {REGION}x{REGION}; adjacent | scattered):");
    for r in rows {
        let fmt = |set: &[Patch]| {
            set.iter()
                .map(|p| format!("({},{})", p.at.0, p.at.1))
                .collect::<Vec<_>>()
                .join(" ")
        };
        eprintln!(
            "  {:<22} {} | {}",
            r.vendor,
            fmt(&r.patches),
            fmt(&r.scattered)
        );
    }
}

/// Build a browsable contact sheet from a dump that `bench_remote_vs_local`
/// already wrote.
///
/// Separate from the bench, and network-free, so the sheet can be rebuilt or
/// restyled in a second without re-reading 13 slides over the internet:
///
/// ```sh
/// SLIDEKIT_BENCH_DUMP=/tmp/patches \
///   cargo test --release --test bench_remote -- --ignored contact_sheet --nocapture
/// ```
///
/// It reads `patches.csv` rather than scanning the directory, because the numbers
/// are the point: a sheet sorted by how much the readers disagree puts the patches
/// worth looking at on the first screen, and 120 patches in vendor order does not.
/// The images stay as files the page links to; inlining several hundred megabytes
/// of PNG would produce a page no browser will open.
#[test]
#[ignore = "post-processes a SLIDEKIT_BENCH_DUMP directory"]
fn contact_sheet() {
    // Gated on its own variable, not just on SLIDEKIT_BENCH_DUMP: `cargo test --
    // --ignored` runs every ignored test in this binary on parallel threads, so
    // without this it would race the bench and build a sheet from the *previous*
    // run's manifest. A dumping bench run writes the sheet itself, so this exists
    // for rebuilding one without re-reading 13 slides over the network.
    if std::env::var_os("SLIDEKIT_BENCH_SHEET").is_none() {
        return;
    }
    let Some(dir) = std::env::var_os("SLIDEKIT_BENCH_DUMP").map(std::path::PathBuf::from) else {
        eprintln!(
            "SKIP: set SLIDEKIT_BENCH_DUMP to the directory a dumping bench run wrote \
             (`SLIDEKIT_BENCH_DUMP=/tmp/patches mise run bench:remote`)."
        );
        return;
    };
    write_contact_sheet(&dir);
}

/// Read `patches.csv` from a dump directory and write `index.html` beside it.
fn write_contact_sheet(dir: &std::path::Path) {
    let csv_path = dir.join("patches.csv");
    let Ok(csv) = std::fs::read_to_string(&csv_path) else {
        eprintln!(
            "WARN: no {} — cannot build the contact sheet.",
            csv_path.display()
        );
        return;
    };

    let mut lines = csv.lines();
    let header: Vec<&str> = lines.next().unwrap_or_default().split(',').collect();
    let col = |name: &str| header.iter().position(|h| *h == name);
    let (Some(c_vendor), Some(c_dir), Some(c_patch)) = (col("vendor"), col("dir"), col("patch"))
    else {
        panic!(
            "{} has an unexpected header: {header:?}",
            csv_path.display()
        );
    };
    let get = |row: &Vec<String>, name: &str| -> String {
        col(name)
            .and_then(|i| row.get(i).cloned())
            .unwrap_or_default()
    };

    let mut rows: Vec<Vec<String>> = lines
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split(',').map(|f| f.trim().to_string()).collect())
        .collect();
    // Worst disagreement first: an empty mean (no OpenSlide comparison) sorts last.
    rows.sort_by(|a, b| {
        let key = |r: &Vec<String>| get(r, "mean_diff").parse::<f64>().unwrap_or(-1.0);
        key(b)
            .partial_cmp(&key(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut html = String::new();
    let thumb = env_u32("SLIDEKIT_BENCH_THUMB", 320);
    html.push_str(
        "<!doctype html><meta charset=utf-8><title>slidekit vs OpenSlide — patches</title>\n\
         <style>\n\
         :root{color-scheme:light dark}\n\
         body{font:14px/1.5 system-ui,sans-serif;margin:0;padding:24px;max-width:1200px}\n\
         h1{font-size:20px;margin:0 0 4px}\n\
         p.lede{margin:0 0 24px;opacity:.75;max-width:70ch}\n\
         .patch{border-top:1px solid color-mix(in srgb,currentColor 20%,transparent);\
         padding:16px 0;display:grid;\
         grid-template-columns:repeat(3,minmax(0,var(--thumb)));gap:12px}\n\
         .patch figure{margin:0}\n\
         .patch img{width:100%;height:auto;display:block;background:#111;\
         border-radius:4px}\n\
         .patch figcaption{font-size:12px;opacity:.7;padding-top:4px}\n\
         .meta{grid-column:1/-1;display:flex;flex-wrap:wrap;gap:6px 16px;\
         align-items:baseline}\n\
         .meta b{font-size:15px}\n\
         .num{font-variant-numeric:tabular-nums}\n\
         .bad{color:#c0392b;font-weight:600}\n\
         .ok{opacity:.6}\n\
         a{color:inherit}\n\
         </style>\n",
    );
    // Render each thumbnail at the size it was written: upscaling a 320 px box-averaged
    // thumbnail into a 500 px slot makes every patch look soft, and softness reads as
    // a difference between the readers when there is none.
    html.push_str(&format!("<style>:root{{--thumb:{thumb}px}}</style>\n"));
    html.push_str(
        "<h1>slidekit (remote) vs OpenSlide — every extracted patch</h1>\n\
         <p class=lede>Left: what slidekit returned reading the slide from object storage. \
         Middle: what OpenSlide 4.0.1 returned reading the same window from the local copy. \
         Right: their difference, amplified so that decoder noise stays black and a \
         real disagreement is visible — dark blue there means the pixel was not \
         comparable (opaque in one reader only), never that they agree. Sorted \
         worst-disagreement first. Click any image for the full-resolution PNG.</p>\n",
    );

    for row in &rows {
        let vendor = row.get(c_vendor).cloned().unwrap_or_default();
        let vdir = row.get(c_dir).cloned().unwrap_or_default();
        let patch = row.get(c_patch).cloned().unwrap_or_default();
        let mean = get(row, "mean_diff");
        let max = get(row, "max_diff");
        let class = if mean.parse::<f64>().unwrap_or(0.0) >= 1.0 {
            "bad"
        } else {
            "ok"
        };
        html.push_str("<div class=patch>\n<div class=meta>");
        html.push_str(&format!(
            "<b>{vendor}</b> <span class=num>{patch} at ({}, {})</span> \
             <span class=num>specimen {}%</span> \
             <span class=num>remote {} ms, {} requests, {} bytes</span> \
             <span class=num>local {} ms · OpenSlide {} ms</span> \
             <span class=num>remote==local: {}</span> \
             <span class='num {class}'>mean diff {mean} · max {max} \
             (over {}% both-opaque)</span>",
            get(row, "x"),
            get(row, "y"),
            get(row, "tissue_pct"),
            get(row, "remote_ms"),
            get(row, "requests"),
            get(row, "bytes"),
            get(row, "local_ms"),
            get(row, "openslide_ms"),
            get(row, "matches_local"),
            get(row, "opaque_pct"),
        ));
        html.push_str("</div>\n");
        for (kind, label) in [
            ("slidekit", "slidekit, from object storage"),
            ("openslide", "OpenSlide 4.0.1, local file"),
            ("diff", "difference, amplified"),
        ] {
            html.push_str(&format!(
                "<figure><a href=\"{vdir}/{patch}_{kind}.png\">\
                 <img loading=lazy src=\"{vdir}/thumbs/{patch}_{kind}.png\" alt=\"{label}\">\
                 </a><figcaption>{label}</figcaption></figure>\n"
            ));
        }
        html.push_str("</div>\n");
    }

    let out = dir.join("index.html");
    match std::fs::write(&out, html) {
        Ok(()) => eprintln!("Wrote {} ({} patches)", out.display(), rows.len()),
        Err(e) => eprintln!("WARN: cannot write {}: {e}", out.display()),
    }
}
