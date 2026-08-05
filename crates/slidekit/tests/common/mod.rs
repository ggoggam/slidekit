//! Test data download and caching utilities.
//!
//! Downloads whole slide images from the OpenSlide test data server,
//! verifies SHA-256 checksums, and caches them locally in `target/testdata/`.
//!
//! The set of slides — their paths, checksums, and sizes — lives in the
//! git-tracked manifest `tests/testdata.yaml`, which is also what CI keys its
//! cache on and sums to decide whether to run integration downloads at all.
//!
//! Also provides [`build_tiled_rgb_tiff`], a synthetic tiled-TIFF generator
//! shared by the network-free streaming/range tests.

#![allow(dead_code)] // not every test binary uses every helper

use fs2::FileExt;
use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// When set (by CI past the 10 GiB budget, see `mise run test`), every slide
/// request short-circuits to `None` so download-backed tests skip gracefully.
const SKIP_ENV: &str = "OPENSLIDE_SKIP_DOWNLOAD";

/// Parsed form of `tests/testdata.yaml`.
#[derive(serde::Deserialize)]
struct Manifest {
    base_url: String,
    slides: Vec<SlideEntry>,
}

#[derive(serde::Deserialize)]
struct SlideEntry {
    path: String,
    sha256: String,
    size: u64,
}

/// Load and cache the parsed manifest for the lifetime of the test binary.
fn manifest() -> &'static Manifest {
    static MANIFEST: Lazy<Manifest> = Lazy::new(|| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/testdata.yaml");
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        serde_yaml::from_str(&text)
            .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()))
    });
    &MANIFEST
}

/// Look up a slide entry by its manifest-relative path.
fn slide_entry(relative_path: &str) -> Option<&'static SlideEntry> {
    manifest().slides.iter().find(|s| s.path == relative_path)
}

/// Every slide path declared in the manifest, in declaration order.
///
/// Used by the manual prefetch utility (`test_download.rs`) to warm the cache;
/// most tests only request the specific slides they need via `require_slide!`.
pub fn all_slide_paths() -> Vec<String> {
    manifest().slides.iter().map(|s| s.path.clone()).collect()
}

/// Return the path to the test data cache directory.
fn cache_dir() -> PathBuf {
    // Use OPENSLIDE_TEST_CACHE env var if set, otherwise target/testdata/
    if let Ok(dir) = std::env::var("OPENSLIDE_TEST_CACHE") {
        PathBuf::from(dir)
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/testdata")
    }
}

/// Local paths whose SHA-256 has already been confirmed in this test binary.
/// Repeated `require_slide!` calls across tests would otherwise re-hash the same
/// (multi-GB) file every time — hashing a 2.5 GB slide once per test dominated
/// wall-clock far more than the actual read work.
static VERIFIED: Lazy<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
    Lazy::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// Verify the SHA-256 of a file matches `expected`, streaming the file in
/// chunks so a multi-GB slide isn't slurped into a single allocation.
fn verify_sha256(path: &Path, expected: &str) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return false,
        }
    }
    format!("{:x}", hasher.finalize()) == expected
}

/// Path of the on-disk "already verified" sentinel for a cached slide.
fn sentinel_path(local: &Path) -> PathBuf {
    local.with_extension("verified")
}

/// Whether `local` has a valid verification sentinel: it records the expected
/// SHA and is at least as new as the slide file. This lets a fresh process skip
/// the multi-GB re-hash — nextest runs every test in its own process, so the
/// in-process [`VERIFIED`] memo alone would re-hash once per test.
fn sentinel_ok(local: &Path, expected: &str) -> bool {
    let sentinel = sentinel_path(local);
    let (Ok(recorded), Ok(sm), Ok(fm)) = (
        fs::read_to_string(&sentinel),
        sentinel.metadata().and_then(|m| m.modified()),
        local.metadata().and_then(|m| m.modified()),
    ) else {
        return false;
    };
    // Stale if the slide was rewritten after the sentinel (e.g. re-downloaded).
    recorded.trim() == expected && sm >= fm
}

/// Record that `local` matches `expected` so other processes skip re-hashing.
fn write_sentinel(local: &Path, expected: &str) {
    fs::write(sentinel_path(local), expected).ok();
}

/// Download a file from the test data server.
fn download_file(url: &str, dest: &Path) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Downloading {} ...", url);

    let response = ureq::get(url).call()?;
    let mut reader = response.into_body().into_reader();

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp = dest.with_extension("tmp");
    let mut file = fs::File::create(&tmp)?;
    let mut buf = [0u8; 65536];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
    }
    file.flush()?;
    drop(file);

    fs::rename(&tmp, dest)?;
    Ok(())
}

/// Get the local path to a test slide, downloading it if needed.
///
/// `relative_path` is like "Aperio/CMU-1.svs" (relative to the test data root).
/// Returns `None` if downloads are disabled via [`SKIP_ENV`], the path is not
/// in the manifest, or the download fails / checksum doesn't match.
pub fn test_slide_path(relative_path: &str) -> Option<PathBuf> {
    // CI past the size budget disables all integration downloads.
    if std::env::var_os(SKIP_ENV).is_some() {
        eprintln!("SKIP: {SKIP_ENV} set, not fetching {relative_path}");
        return None;
    }

    let Some(entry) = slide_entry(relative_path) else {
        eprintln!("WARNING: {relative_path} is not listed in tests/testdata.yaml");
        return None;
    };

    let local = cache_dir().join(relative_path);

    // Warm path (lock-free): already verified in this process — skip the hash.
    if VERIFIED.lock().unwrap().contains(&local) {
        return Some(local);
    }

    // Cross-process warm path: a prior process left a valid sentinel, so skip
    // both the flock and the multi-GB re-hash. Critical under nextest, which
    // runs each test in its own process (the in-process memo above never hits).
    if sentinel_ok(&local, &entry.sha256) {
        VERIFIED.lock().unwrap().insert(local.clone());
        return Some(local);
    }

    // First-time resolution: serialize it per slide so a fixture shared by
    // several tests is verified/downloaded once, not once per test. libtest
    // runs test fns on parallel threads (and nextest across processes), which
    // would otherwise have every test concurrently re-hash — or re-download
    // into the same temp path — the same multi-GB file. `flock` serializes
    // independent file descriptions whether they're in different threads or
    // different processes, and the OS releases it if the holder dies, so a
    // crashed test can't wedge the others.
    if let Some(parent) = local.parent() {
        fs::create_dir_all(parent).ok();
    }
    let lock = fs::File::create(local.with_extension("lock")).ok();
    if let Some(f) = &lock {
        f.lock_exclusive().ok();
    }

    // Re-check the memo under the lock: a peer may have finished while we waited.
    if VERIFIED.lock().unwrap().contains(&local) {
        return Some(local);
    }

    // Cached and intact? Hash it once, record it (memo + on-disk sentinel), done.
    if local.exists() && verify_sha256(&local, &entry.sha256) {
        write_sentinel(&local, &entry.sha256);
        VERIFIED.lock().unwrap().insert(local.clone());
        return Some(local);
    }

    let url = format!("{}{}", manifest().base_url, relative_path);
    match download_file(&url, &local) {
        Ok(()) => {
            if verify_sha256(&local, &entry.sha256) {
                write_sentinel(&local, &entry.sha256);
                VERIFIED.lock().unwrap().insert(local.clone());
                Some(local)
            } else {
                eprintln!("WARNING: SHA-256 mismatch for {relative_path}, removing");
                fs::remove_file(&local).ok();
                None
            }
        }
        Err(e) => {
            eprintln!("WARNING: Failed to download {relative_path}: {e}");
            None
        }
    }
}

/// Require a test slide, skipping the test if unavailable.
///
/// Use this macro in tests to skip gracefully when test data can't be downloaded.
#[macro_export]
macro_rules! require_slide {
    ($path:expr) => {
        match common::test_slide_path($path) {
            Some(p) => p,
            None => {
                eprintln!("SKIP: test slide {} not available", $path);
                return;
            }
        }
    };
}

// ───────────────────────── ZIP-packaged fixtures ───────────────────────────

/// Extract a ZIP-packaged slide from the manifest into a sibling `extracted/`
/// directory, returning that directory.
///
/// Three vendors ship as ZIPs because they are multi-file formats (or, for
/// Trestle, simply packaged that way). `nest_in_stem` unpacks into
/// `extracted/<zip stem>/` rather than `extracted/` — what the DICOM fixtures
/// need, since a DICOM slide *is* a directory of instances and two of them
/// share one vendor folder.
///
/// Extraction is serialized with the same `flock` the downloader uses and
/// lands via a `.partial` rename, so concurrent test binaries can't race and a
/// killed run can't leave a half-unpacked directory that looks complete.
///
/// A non-empty `dir` is what marks the extraction complete, so nothing that is
/// not archive content may ever be written inside it — the lock file therefore
/// lives beside the archive. (Putting it in `dir` made the non-nested vendors
/// mistake their own lock for an unpacked slide and skip extracting entirely.)
pub fn extracted_slide_dir(manifest_rel: &str, nest_in_stem: bool) -> Option<PathBuf> {
    let zip_path = test_slide_path(manifest_rel)?;
    let base = zip_path.parent()?.join("extracted");
    let dir = if nest_in_stem {
        base.join(zip_path.file_stem()?.to_str()?)
    } else {
        base.clone()
    };

    if dir_has_files(&dir) {
        return Some(dir);
    }

    let lock = fs::File::create(zip_path.with_extension("extract.lock")).ok();
    if let Some(f) = &lock {
        f.lock_exclusive().ok();
    }
    // A peer may have finished while we waited on the lock.
    if dir_has_files(&dir) {
        return Some(dir);
    }

    fs::create_dir_all(dir.parent()?).ok()?;
    let tmp = dir.with_extension("partial");
    fs::remove_dir_all(&tmp).ok();
    fs::create_dir_all(&tmp).ok()?;

    let file = fs::File::open(&zip_path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).ok()?;
        let name = entry.name().to_string();
        if name.ends_with('/') {
            continue;
        }
        // A ZIP entry name is attacker-controlled in general; these archives are
        // checksum-verified, but keep the extraction inside `tmp` regardless.
        let out_path = tmp.join(&name);
        if !out_path.starts_with(&tmp) {
            eprintln!("WARNING: skipping suspicious zip entry {name}");
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).ok()?;
        }
        let mut out_file = fs::File::create(&out_path).ok()?;
        std::io::copy(&mut entry, &mut out_file).ok()?;
    }

    // Rename last: a killed run leaves `.partial`, not a "complete" directory.
    // Renaming onto a non-empty directory fails, so clear any empty husk an
    // earlier interrupted run left behind.
    fs::remove_dir_all(&dir).ok();
    fs::rename(&tmp, &dir).ok()?;
    dir_has_files(&dir).then_some(dir)
}

/// Whether `dir` exists and holds at least one entry — an empty directory is
/// treated as "not extracted" so a failed run retries instead of sticking.
fn dir_has_files(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|mut e| e.next().is_some())
}

/// Every file under `root`, depth-first, in sorted order.
fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// The first `.dcm` file under `dir`, in sorted order.
pub fn first_dcm(dir: &Path) -> Option<PathBuf> {
    walk_files(dir)
        .into_iter()
        .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dcm")))
}

// ───────────────────────── bench fixture set ───────────────────────────────

/// One representative real slide per vendor, with the level to read.
///
/// Shared by `bench_openslide`, `bench_remote`, and `upload_testdata` so all
/// three agree on what "the bench set" is. The ZIP-packaged vendors use a bare
/// `Vendor/Name` key that [`bench_slide`] resolves to the extracted files.
///
/// The JP2K Aperio variants and the JXR CZI are omitted: slidekit has no
/// JPEG-2000 or JPEG XR decoder, so there is nothing to time. Both DICOM
/// slides are kept because they exercise different frame layouts
/// (`TILED_SPARSE` vs `TILED_FULL`) — exactly what a pixel cross-check catches.
pub const BENCH_CASES: &[(&str, i32)] = &[
    ("Aperio/CMU-1.svs", 0),
    ("Hamamatsu/CMU-1.ndpi", 0),
    ("Leica/Leica-1.scn", 0),
    ("Philips-TIFF/Philips-4.tiff", 0),
    ("Generic-TIFF/CMU-1.tiff", 0),
    ("Trestle/CMU-1", 0),
    ("Mirax/CMU-1", 0),
    ("Zeiss/Zeiss-5-Uncompressed.czi", 0),
    ("Ventana/OS-2.bif", 0),
    ("DICOM/Leica-4", 0),
    ("DICOM/Hamamatsu-2", 0),
];

/// A bench slide resolved on local disk, together with the object keys that
/// mirror it into a bucket.
///
/// Single-file vendors carry one file. MIRAX and DICOM carry their whole
/// sibling set — that set is precisely what makes them openable remotely, since
/// both formats resolve siblings through `SlideFs` rather than reading one blob.
pub struct BenchSlide {
    /// Label used in bench output (the vendor, or `Vendor/Slide` for DICOM
    /// where two cases share a vendor).
    pub name: String,
    /// Path handed to `OpenSlide::open`.
    pub local_path: PathBuf,
    /// Key handed to `OpenSlide::open_store`, relative to the mirror root.
    pub open_key: String,
    /// `(local file, key relative to the mirror root)` for every file the slide
    /// needs.
    pub files: Vec<(PathBuf, String)>,
}

impl BenchSlide {
    /// Total on-disk size of every file in the slide.
    pub fn total_bytes(&self) -> u64 {
        self.files
            .iter()
            .filter_map(|(p, _)| p.metadata().ok().map(|m| m.len()))
            .sum()
    }
}

/// Resolve a [`BENCH_CASES`] entry to local files and mirror keys, extracting
/// the ZIP-packaged vendors on demand. Returns `None` when the fixture isn't
/// available (downloads disabled, or a fetch failed), so benches skip the case.
pub fn bench_slide(case: &str) -> Option<BenchSlide> {
    // (slide root on disk, key prefix under the mirror root, file to open)
    let (root, prefix, open) = match case {
        "Trestle/CMU-1" => {
            let dir = extracted_slide_dir("Trestle/CMU-1.zip", false)?;
            let open = dir.join("CMU-1.tif");
            (dir, "Trestle", open)
        }
        "Mirax/CMU-1" => {
            let dir = extracted_slide_dir("Mirax/CMU-1.zip", false)?;
            let open = dir.join("CMU-1.mrxs");
            (dir, "Mirax", open)
        }
        // A DICOM slide is a directory of instances; opening any one of them
        // opens the slide, so hand the driver the first.
        "DICOM/Leica-4" => {
            let dir = extracted_slide_dir("DICOM/Leica-4.zip", true)?;
            let open = first_dcm(&dir)?;
            (dir, "DICOM/Leica-4", open)
        }
        "DICOM/Hamamatsu-2" => {
            let dir = extracted_slide_dir("DICOM/Hamamatsu-2.zip", true)?;
            let open = first_dcm(&dir)?;
            (dir, "DICOM/Hamamatsu-2", open)
        }
        // Single-file vendors mirror straight to their manifest path.
        single => {
            let path = test_slide_path(single)?;
            return Some(BenchSlide {
                name: vendor_label(case),
                local_path: path.clone(),
                open_key: single.to_string(),
                files: vec![(path, single.to_string())],
            });
        }
    };

    if !open.is_file() {
        eprintln!(
            "WARNING: {case}: expected {} in the archive",
            open.display()
        );
        return None;
    }

    let key_of = |path: &Path| -> Option<String> {
        let rel = path.strip_prefix(&root).ok()?;
        // Object keys are `/`-separated regardless of host separator.
        let rel: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        Some(format!("{prefix}/{}", rel.join("/")))
    };

    let files: Vec<(PathBuf, String)> = walk_files(&root)
        .into_iter()
        .filter_map(|p| key_of(&p).map(|k| (p, k)))
        .collect();

    Some(BenchSlide {
        name: vendor_label(case),
        open_key: key_of(&open)?,
        local_path: open,
        files,
    })
}

/// Bench label for a case: the vendor alone, except for DICOM where two slides
/// share a vendor and the slide name is what distinguishes them.
fn vendor_label(case: &str) -> String {
    match case.split('/').next().unwrap_or(case) {
        "DICOM" => case.to_string(),
        other => other.to_string(),
    }
}

// ───────────────────────── remote bench bucket ─────────────────────────────

/// An S3-compatible bucket to mirror the bench slides into, configured from the
/// environment. `slidekit/.env` (loaded by mise, git-ignored) supplies:
///
/// * `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` — credentials
/// * `BUCKET_NAME` — bucket to write under
/// * `AWS_REGION` (or `BUCKET_REGION`) — region (`auto` where it isn't used)
/// * `AWS_ENDPOINT_URL` (or `BUCKET_ENDPOINT_URL`) — S3-compatible endpoint
///
/// The `AWS_*` spellings are the ones [`slidekit::OpenSlide::open_url`] reads,
/// so a `.env` using them configures the bench store and the public URL entry
/// point identically — which is what
/// [`open_url_resolves_config_from_environment`](../bench_remote) asserts. The
/// `BUCKET_*` aliases are accepted so an older `.env` keeps working.
///
/// Everything is written under one key prefix ([`RemoteBucket::root`]) so the
/// bench never touches unrelated objects in a shared bucket.
pub struct RemoteBucket {
    pub store: std::sync::Arc<dyn object_store::ObjectStore>,
    pub bucket: String,
    /// Key prefix every bench object lives under.
    pub root: String,
}

impl RemoteBucket {
    /// Absolute object path for a mirror-root-relative key.
    pub fn key(&self, rel: &str) -> object_store::path::Path {
        object_store::path::Path::from(format!("{}/{}", self.root, rel))
    }

    /// `s3://` URL for a mirror-root-relative key — what a user would pass to
    /// `OpenSlide::open_url` with the same endpoint configured in the
    /// environment. Reported by the bench so the keys are copy-pasteable.
    pub fn url(&self, rel: &str) -> String {
        format!("s3://{}/{}/{}", self.bucket, self.root, rel)
    }
}

/// Default key prefix for the mirrored bench slides. Overridable with
/// `SLIDEKIT_BENCH_PREFIX`, which matters when the bucket is shared with
/// something else (a CI cache, say).
pub const DEFAULT_BENCH_PREFIX: &str = "slidekit-bench/testdata";

/// Build the remote bench bucket from the environment, or `None` (with a note)
/// when it isn't configured — so the remote bench and uploader skip rather than
/// fail on a machine with no credentials.
pub fn remote_bucket() -> Option<RemoteBucket> {
    use object_store::aws::AmazonS3Builder;

    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let (Some(key_id), Some(secret), Some(bucket)) = (
        var("AWS_ACCESS_KEY_ID"),
        var("AWS_SECRET_ACCESS_KEY"),
        var("BUCKET_NAME"),
    ) else {
        eprintln!(
            "SKIP: remote bucket not configured — set AWS_ACCESS_KEY_ID, \
             AWS_SECRET_ACCESS_KEY and BUCKET_NAME (see slidekit/.env)"
        );
        return None;
    };

    let mut builder = AmazonS3Builder::new()
        .with_bucket_name(&bucket)
        .with_access_key_id(key_id)
        .with_secret_access_key(secret)
        // Non-AWS S3 implementations generally serve path-style URLs
        // (`endpoint/bucket/key`) and don't provision per-bucket DNS.
        .with_virtual_hosted_style_request(false)
        .with_region(
            var("AWS_REGION")
                .or_else(|| var("BUCKET_REGION"))
                .unwrap_or_else(|| "us-east-1".into()),
        );
    if let Some(endpoint) = var("AWS_ENDPOINT_URL").or_else(|| var("BUCKET_ENDPOINT_URL")) {
        // Allow plain HTTP only for an explicitly non-TLS endpoint (a local
        // MinIO); real endpoints are HTTPS and unaffected.
        let insecure = endpoint.starts_with("http://");
        builder = builder.with_endpoint(endpoint).with_allow_http(insecure);
    }

    match builder.build() {
        Ok(store) => Some(RemoteBucket {
            store: std::sync::Arc::new(store),
            bucket,
            root: std::env::var("SLIDEKIT_BENCH_PREFIX")
                .unwrap_or_else(|_| DEFAULT_BENCH_PREFIX.to_string()),
        }),
        Err(e) => {
            eprintln!("SKIP: cannot build S3 store: {e}");
            None
        }
    }
}

// ───────────────────────── tissue-region search ────────────────────────────

/// Find the `w`×`h` window (at `level`, returned as level-0 top-left coords)
/// that covers the most tissue, by scanning the coarsest pyramid level for
/// non-white opaque pixels. Returns `None` if the overview can't be read, so
/// callers fall back to slide center.
///
/// Both benches pick their compared region this way, and the remote bench
/// reuses the coordinates the local pass computed — so remote timings measure
/// the read, not a second tissue search.
///
/// Builds a summed-area table so the search is O(pixels) rather than
/// O(pixels × window). The coarsest level is a small thumbnail on real
/// pyramids, so memory and time are trivial.
pub fn tissue_region(
    slide: &slidekit::OpenSlide,
    level: i32,
    w: i64,
    h: i64,
) -> Option<(i64, i64)> {
    let ov = slide.level_count() - 1;
    let (ow, oh) = slide.level_dimensions(ov)?;
    if ow <= 0 || oh <= 0 {
        return None;
    }
    let ov_ds = slide.level_downsample(ov)?;
    let px = slide.read_region(0, 0, ov, ow, oh).ok()?;

    // Tissue mask: opaque and not near-white (dark OR saturated → stain).
    let (ow, oh) = (ow as usize, oh as usize);
    let mut sat = vec![0u32; (ow + 1) * (oh + 1)]; // summed-area table, 1px pad
    let stride = ow + 1;
    for y in 0..oh {
        for x in 0..ow {
            let p = px[y * ow + x];
            let (r, g, b, a) = ((p >> 16) & 0xff, (p >> 8) & 0xff, p & 0xff, p >> 24);
            let bright = r.max(g).max(b);
            let saturation = bright - r.min(g).min(b);
            let tissue = (a & 0xff) == 0xff && (bright < 210 || saturation > 25);
            let v = tissue as u32;
            sat[(y + 1) * stride + (x + 1)] =
                v + sat[y * stride + (x + 1)] + sat[(y + 1) * stride + x] - sat[y * stride + x];
        }
    }

    // Window size at overview scale (level → level-0 → overview px).
    let target_ds = slide.level_downsample(level)?;
    let win_w = ((w as f64 * target_ds / ov_ds).round() as usize).clamp(1, ow);
    let win_h = ((h as f64 * target_ds / ov_ds).round() as usize).clamp(1, oh);

    let sum = |x: usize, y: usize, bw: usize, bh: usize| {
        sat[(y + bh) * stride + x + bw] + sat[y * stride + x]
            - sat[y * stride + x + bw]
            - sat[(y + bh) * stride + x]
    };
    let (mut best, mut best_xy) = (0, (0usize, 0usize));
    for y in 0..=(oh - win_h) {
        for x in 0..=(ow - win_w) {
            let s = sum(x, y, win_w, win_h);
            if s > best {
                best = s;
                best_xy = (x, y);
            }
        }
    }
    if best == 0 {
        return None; // no tissue found — let caller use center
    }
    // Overview top-left → level-0 coords (what read_region expects).
    Some((
        (best_xy.0 as f64 * ov_ds) as i64,
        (best_xy.1 as f64 * ov_ds) as i64,
    ))
}

// ───────────────────────── synthetic fixtures ──────────────────────────────

// Minimal TIFF tag numbers needed to encode an uncompressed tiled RGB image.
mod tiff_tag {
    pub const IMAGE_WIDTH: u16 = 256;
    pub const IMAGE_LENGTH: u16 = 257;
    pub const COMPRESSION: u16 = 259;
    pub const SAMPLES_PER_PIXEL: u16 = 277;
    pub const TILE_WIDTH: u16 = 322;
    pub const TILE_LENGTH: u16 = 323;
    pub const TILE_OFFSETS: u16 = 324;
    pub const TILE_BYTE_COUNTS: u16 = 325;
}

const TIFF_TYPE_SHORT: u16 = 3;
const TIFF_TYPE_LONG: u16 = 4;

fn write_ifd_entry(buf: &mut Vec<u8>, tag: u16, typ: u16, count: u32, value: u32) {
    buf.extend_from_slice(&tag.to_le_bytes());
    buf.extend_from_slice(&typ.to_le_bytes());
    buf.extend_from_slice(&count.to_le_bytes());
    buf.extend_from_slice(&value.to_le_bytes());
}

/// Build an uncompressed, tiled, little-endian classic TIFF (8-bit RGB) made of
/// `tiles_x * tiles_y` tiles of `tile * tile` pixels. Tile `(tx, ty)` is filled
/// with the solid color `color(tx, ty)` as `(r, g, b)`, so a reader can verify
/// exactly which tile a pixel came from.
///
/// Tiles are laid out contiguously in row-major order, so reading a single tile
/// touches a small, well-separated byte range — which is what the streaming
/// tests assert against. Requires at least two tiles (the TIFF offset/count
/// arrays are stored out-of-line, which the parser only expects past one tile).
pub fn build_tiled_rgb_tiff(
    tile: u32,
    tiles_x: u32,
    tiles_y: u32,
    color: impl Fn(u32, u32) -> (u8, u8, u8),
) -> Vec<u8> {
    let num_tiles = tiles_x * tiles_y;
    assert!(num_tiles >= 2, "builder requires at least two tiles");

    let image_w = tiles_x * tile;
    let image_h = tiles_y * tile;
    let bytes_per_tile = (tile * tile * 3) as usize;

    let num_entries: u16 = 8;
    // header (8) + entry count (2) + entries (12 each) + next-IFD pointer (4)
    let ifd_end = 8 + 2 + num_entries as usize * 12 + 4;
    let offsets_table = ifd_end;
    let counts_table = offsets_table + num_tiles as usize * 4;
    let tile_data = counts_table + num_tiles as usize * 4;

    let mut buf = Vec::with_capacity(tile_data + num_tiles as usize * bytes_per_tile);

    // Header: little-endian ("II"), magic 42, first IFD at offset 8.
    buf.extend_from_slice(&[0x49, 0x49]);
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&8u32.to_le_bytes());

    // IFD (tags must be in ascending order).
    buf.extend_from_slice(&num_entries.to_le_bytes());
    write_ifd_entry(&mut buf, tiff_tag::IMAGE_WIDTH, TIFF_TYPE_LONG, 1, image_w);
    write_ifd_entry(&mut buf, tiff_tag::IMAGE_LENGTH, TIFF_TYPE_LONG, 1, image_h);
    write_ifd_entry(&mut buf, tiff_tag::COMPRESSION, TIFF_TYPE_SHORT, 1, 1);
    write_ifd_entry(&mut buf, tiff_tag::SAMPLES_PER_PIXEL, TIFF_TYPE_SHORT, 1, 3);
    write_ifd_entry(&mut buf, tiff_tag::TILE_WIDTH, TIFF_TYPE_SHORT, 1, tile);
    write_ifd_entry(&mut buf, tiff_tag::TILE_LENGTH, TIFF_TYPE_SHORT, 1, tile);
    write_ifd_entry(
        &mut buf,
        tiff_tag::TILE_OFFSETS,
        TIFF_TYPE_LONG,
        num_tiles,
        offsets_table as u32,
    );
    write_ifd_entry(
        &mut buf,
        tiff_tag::TILE_BYTE_COUNTS,
        TIFF_TYPE_LONG,
        num_tiles,
        counts_table as u32,
    );
    buf.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    debug_assert_eq!(buf.len(), ifd_end);

    // Tile offset table.
    for i in 0..num_tiles as usize {
        let off = tile_data + i * bytes_per_tile;
        buf.extend_from_slice(&(off as u32).to_le_bytes());
    }
    // Tile byte-count table (every tile is the same uncompressed size).
    for _ in 0..num_tiles {
        buf.extend_from_slice(&(bytes_per_tile as u32).to_le_bytes());
    }
    debug_assert_eq!(buf.len(), tile_data);

    // Tile pixel data, row-major (tile index = ty * tiles_x + tx).
    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let (r, g, b) = color(tx, ty);
            for _ in 0..(tile * tile) {
                buf.extend_from_slice(&[r, g, b]);
            }
        }
    }

    buf
}
