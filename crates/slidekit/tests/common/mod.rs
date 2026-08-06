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

pub mod slidetool;

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
/// The JXR CZI is omitted: slidekit has no JPEG XR decoder, so there is nothing
/// to time. Both JP2K Aperio variants are included and are the *point* of the
/// pixel cross-check — 33003 carries subsampled YCbCr that only the container
/// identifies, so a wrong colour space shows up here and nowhere else. Both
/// DICOM slides are kept because they exercise different frame layouts
/// (`TILED_SPARSE` vs `TILED_FULL`) — exactly what a pixel cross-check catches.
pub const BENCH_CASES: &[(&str, i32)] = &[
    ("Aperio/CMU-1.svs", 0),
    ("Aperio/JP2K-33003-1.svs", 0),
    ("Aperio/CMU-1-JP2K-33005.svs", 0),
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
    let vendor = case.split('/').next().unwrap_or(case);
    if vendor == "DICOM" {
        return case.to_string();
    }
    // A vendor with one slide in the set is named by the vendor alone. Where
    // several share it — the three Aperio codecs — the slide stem has to come
    // along, or the results table and the `SLIDEKIT_BENCH_DUMP` filenames both
    // collapse onto one row and silently overwrite each other.
    let siblings = BENCH_CASES
        .iter()
        .filter(|(c, _)| c.split('/').next() == Some(vendor))
        .count();
    if siblings <= 1 {
        return vendor.to_string();
    }
    let stem = Path::new(case)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(case);
    format!("{vendor}/{stem}")
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
/// that covers the most tissue. Returns `None` if the overview can't be read, so
/// callers fall back to slide center.
///
/// Both benches pick their compared region this way, and the remote bench
/// reuses the coordinates the local pass computed — so remote timings measure
/// the read, not a second tissue search.
pub fn tissue_region(
    slide: &slidekit::OpenSlide,
    level: i32,
    w: i64,
    h: i64,
) -> Option<(i64, i64)> {
    tissue_patches(slide, level, w, h, 1, 0.0)
        .into_iter()
        .next()
}

/// The `count` most tissue-dense `w`×`h` windows that are at least
/// `min_separation` window-widths apart, densest first, as level-0 top-left
/// coordinates.
///
/// `min_separation` is what makes this more than a repeat of [`tissue_region`].
/// With 0 it returns the single best window and its immediate overlaps; with a
/// few window-widths it returns windows scattered across the specimen, which is
/// the access pattern that defeats read-ahead and tile caching — and therefore
/// the honest upper bound on what a patch costs remotely.
///
/// Tissue is detected by scanning the coarsest pyramid level for opaque pixels
/// that are dark or saturated (i.e. stained). A summed-area table makes the
/// search O(pixels) rather than O(pixels × window); the coarsest level is a small
/// thumbnail on real pyramids, so memory and time are trivial.
pub fn tissue_patches(
    slide: &slidekit::OpenSlide,
    level: i32,
    w: i64,
    h: i64,
    count: usize,
    min_separation: f64,
) -> Vec<(i64, i64)> {
    let Some(patches) = tissue_patches_inner(slide, level, w, h, count, min_separation) else {
        return Vec::new();
    };
    patches
}

fn tissue_patches_inner(
    slide: &slidekit::OpenSlide,
    level: i32,
    w: i64,
    h: i64,
    count: usize,
    min_separation: f64,
) -> Option<Vec<(i64, i64)>> {
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
            // Opaque, not near-white, and **not near-black**. Excluding black is
            // not cosmetic: several vendors surround the specimen with black
            // padding in the coarse levels (Ventana's BIF overview, DICOM
            // thumbnails), and counting it as stain sent the search to a corner of
            // padding — those slides were then "compared" against OpenSlide over
            // windows holding 1% specimen, which agrees trivially and proves
            // nothing about tile placement.
            let tissue = (a & 0xff) == 0xff && bright >= 25 && (bright < 210 || saturation > 25);
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

    // Score every window position, then keep only the best one per grid cell of
    // `min_separation` windows — cheap non-maximum suppression, so the result is
    // spread over the specimen instead of being one hotspot's overlapping shifts.
    let cell_w = ((win_w as f64 * min_separation) as usize).max(1);
    let cell_h = ((win_h as f64 * min_separation) as usize).max(1);
    let cells_x = ow.div_ceil(cell_w);
    let mut best_per_cell: std::collections::HashMap<usize, (u32, (usize, usize))> =
        std::collections::HashMap::new();
    for y in 0..=(oh - win_h) {
        for x in 0..=(ow - win_w) {
            let s = sum(x, y, win_w, win_h);
            if s == 0 {
                continue;
            }
            let cell = (y / cell_h) * cells_x + (x / cell_w);
            let slot = best_per_cell.entry(cell).or_insert((0, (x, y)));
            if s > slot.0 {
                *slot = (s, (x, y));
            }
        }
    }
    if best_per_cell.is_empty() {
        return None; // no tissue found — let caller fall back
    }

    let mut ranked: Vec<(u32, (usize, usize))> = best_per_cell.into_values().collect();
    // Densest first; ties broken by scan order (row-major, so y before x) both to
    // keep the set deterministic — a HashMap's iteration order is not — and to
    // return the same window the earlier single-window search did, so figures
    // recorded before this became a set stay comparable.
    ranked.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| (a.1.1, a.1.0).cmp(&(b.1.1, b.1.0)))
    });

    // Enforce the separation between the windows actually returned, not merely
    // between cells: the densest cells are frequently neighbours, so taking the N
    // densest can hand back a huddle.
    //
    // This is Chebyshev separation — far apart in *either* axis. It deliberately
    // does not demand separation in both, which would be unsatisfiable on a wide,
    // short slide (Zeiss is 50171×11340: at six window-widths there is only one
    // distinct row of windows to choose from). The consequence is that a set can
    // still land in one horizontal band, and on a format stored in rows rather
    // than tiles — a single-strip NDPI — windows in one band share data. Where
    // that happens the per-patch cost measured here is a lower bound.
    let (sep_x, sep_y) = (
        (win_w as f64 * min_separation) as usize,
        (win_h as f64 * min_separation) as usize,
    );
    let mut chosen: Vec<(usize, usize)> = Vec::with_capacity(count);
    for (_, (x, y)) in ranked {
        if chosen.len() == count {
            break;
        }
        let far_enough = chosen
            .iter()
            .all(|&(cx, cy)| x.abs_diff(cx) >= sep_x || y.abs_diff(cy) >= sep_y);
        if far_enough {
            chosen.push((x, y));
        }
    }

    // Overview top-left → level-0 coords (what read_region expects).
    Some(
        chosen
            .into_iter()
            .map(|(x, y)| ((x as f64 * ov_ds) as i64, (y as f64 * ov_ds) as i64))
            .collect(),
    )
}

/// Top-left of a `w`×`h` window whose *neighbourhood* is tissue, for anchoring an
/// adjacent patch set.
///
/// [`tissue_region`] returns the single densest window, which is frequently at the
/// edge of the specimen — its neighbours are then background, and an adjacent set
/// built around it has nothing to read. This scores a 3×3 block of windows
/// instead and returns the middle one, so stepping one window in any direction
/// still lands on specimen. Falls back to the plain densest window when no block
/// qualifies (a slide with less than three windows of tissue in either axis).
pub fn tissue_neighbourhood(
    slide: &slidekit::OpenSlide,
    level: i32,
    w: i64,
    h: i64,
) -> Option<(i64, i64)> {
    let ds = slide.level_downsample(level).unwrap_or(1.0);
    match tissue_patches(slide, level, w * 3, h * 3, 1, 0.0)
        .into_iter()
        .next()
    {
        // The block's top-left is one window up and left of the window we want.
        Some((bx, by)) => Some((bx + (w as f64 * ds) as i64, by + (h as f64 * ds) as i64)),
        None => tissue_region(slide, level, w, h),
    }
}

/// A set of `count` distinct `w`×`h` windows (level-0 top-left coords) starting
/// at `primary` and stepping outwards by one window at a time.
///
/// One patch measures one code path through the tile grid; a real caller
/// extracts many. Stepping by exactly one window width means consecutive patches
/// touch disjoint tile columns, so a steady-state read cannot be served entirely
/// from the tile cache the previous patch filled — which is what makes the
/// per-patch request count meaningful rather than an artefact of caching.
///
/// Windows are clamped inside the level and de-duplicated, so a slide too small
/// to hold `count` of them simply yields fewer.
pub fn patch_grid(
    slide: &slidekit::OpenSlide,
    level: i32,
    w: i64,
    h: i64,
    primary: (i64, i64),
    count: usize,
) -> Vec<(i64, i64)> {
    let (lw, lh) = slide.level_dimensions(level).unwrap_or((0, 0));
    let ds = slide.level_downsample(level).unwrap_or(1.0);
    // Bounds and step are in level-0 coordinates, which is what read_region takes.
    let max_x = (((lw - w).max(0)) as f64 * ds) as i64;
    let max_y = (((lh - h).max(0)) as f64 * ds) as i64;
    let (step_x, step_y) = ((w as f64 * ds) as i64, (h as f64 * ds) as i64);

    // Ring around the primary window, nearest first: staying next to the densest
    // tissue keeps every patch on something worth decoding.
    const RING: &[(i64, i64)] = &[
        (0, 0),
        (1, 0),
        (0, 1),
        (1, 1),
        (-1, 0),
        (0, -1),
        (-1, -1),
        (2, 0),
        (0, 2),
    ];
    let mut out = Vec::with_capacity(count);
    for &(dx, dy) in RING {
        if out.len() == count {
            break;
        }
        let p = (
            (primary.0 + dx * step_x).clamp(0, max_x),
            (primary.1 + dy * step_y).clamp(0, max_y),
        );
        // Clamping can collapse two ring offsets onto the same window; a repeated
        // window would read from cache and understate the per-patch cost.
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
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

// ─────────────────────── decoded-tile cache assertions ─────────────────────

/// Assert that the decoded-tile cache is invisible in the output of
/// `read_region`, for one level of one slide.
///
/// This is the regression that matters for [`slidekit::Cache`]: a caching bug
/// returns *plausible* pixels, so nothing downstream notices until a diagnosis
/// is made on the wrong tissue. Three properties are checked at once, over
/// `regions` that should overlap each other so successive reads share tiles:
///
/// 1. Every region read through a warm cache equals the same region read
///    through a **disabled** cache (`Cache::new(0)`), which fetches and decodes
///    everything afresh. That is the reference for what the pixels were before
///    any caching existed.
/// 2. Reading the whole sequence a second time — now with every tile cached —
///    returns exactly the same bytes again.
/// 3. The cache actually served something, so a wiring that silently did
///    nothing cannot pass by accident.
///
/// `open` is called twice (once for the reference slide, once for the warm
/// one), because a slide's cache cannot be un-warmed.
pub fn assert_cache_is_transparent(
    open: impl Fn() -> slidekit::OpenSlide,
    level: i32,
    regions: &[(i64, i64, i64, i64)],
) {
    let mut cold = open();
    cold.set_cache(slidekit::Cache::new(0));
    let reference: Vec<Vec<u32>> = regions
        .iter()
        .map(|&(x, y, w, h)| {
            cold.read_region(x, y, level, w, h)
                .expect("reference read_region")
        })
        .collect();
    assert_eq!(cold.cache().hits(), 0, "a disabled cache served a tile");

    let warm = open();
    for pass in 0..2 {
        for (i, &(x, y, w, h)) in regions.iter().enumerate() {
            let got = warm
                .read_region(x, y, level, w, h)
                .expect("cached read_region");
            assert_pixels_eq(
                &got,
                &reference[i],
                &format!("level {level} region {i} ({x},{y},{w},{h}), pass {pass}"),
            );
        }
    }

    assert!(
        warm.cache().hits() > 0,
        "no tile was served from the cache — the read path is not wired to it"
    );
}

/// Compare two ARGB buffers, reporting the first difference rather than dumping
/// a megapixel of `Vec<u32>` into the failure message.
pub fn assert_pixels_eq(got: &[u32], want: &[u32], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: pixel count differs");
    if let Some((i, (&g, &w))) = got.iter().zip(want).enumerate().find(|(_, (g, w))| g != w) {
        let differing = got.iter().zip(want).filter(|(g, w)| g != w).count();
        panic!(
            "{what}: {differing} of {} pixels differ; first at index {i}: {g:#010x} != {w:#010x}",
            got.len()
        );
    }
}

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
