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

/// Verify SHA-256 of a file matches expected hash.
fn verify_sha256(path: &Path, expected: &str) -> bool {
    let Ok(data) = fs::read(path) else {
        return false;
    };
    let hash = Sha256::digest(&data);
    let hex = format!("{:x}", hash);
    hex == expected
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

    // Check if already cached with correct checksum.
    if local.exists() && verify_sha256(&local, &entry.sha256) {
        return Some(local);
    }

    let url = format!("{}{}", manifest().base_url, relative_path);
    match download_file(&url, &local) {
        Ok(()) => {
            if verify_sha256(&local, &entry.sha256) {
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
