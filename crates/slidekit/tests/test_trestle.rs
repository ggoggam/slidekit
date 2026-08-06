//! Integration tests for the Trestle vendor driver.
//!
//! Unit tests use synthetic TIFF data. Integration tests use real test data
//! from the OpenSlide test data server (Trestle/CMU-1.zip → CMU-1.tif).
//!
//! Each real slide is opened a single time per test — assertions about the same
//! slide are grouped so we don't re-parse it once per assertion.

mod common;

use slidekit::OpenSlide;
use slidekit::decode::tiff::{TiffLike, tags};
use slidekit::io::SlideReader;
use slidekit::vendor::VendorDriver;
use slidekit::vendor::trestle::TrestleDriver;

/// In-memory reader for testing.
struct MemReader(Vec<u8>);

impl SlideReader for MemReader {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> slidekit::Result<usize> {
        let start = offset as usize;
        if start >= self.0.len() {
            return Ok(0);
        }
        let end = (start + buf.len()).min(self.0.len());
        let n = end - start;
        buf[..n].copy_from_slice(&self.0[start..end]);
        Ok(n)
    }

    fn size(&self) -> slidekit::Result<u64> {
        Ok(self.0.len() as u64)
    }
}

// ── TIFF builders ──────────────────────────────────────────────────

/// Helper to write a TIFF IFD entry (classic, little-endian).
fn write_entry(buf: &mut Vec<u8>, tag: u16, typ: u16, count: u32, value: u32) {
    buf.extend_from_slice(&tag.to_le_bytes());
    buf.extend_from_slice(&typ.to_le_bytes());
    buf.extend_from_slice(&count.to_le_bytes());
    buf.extend_from_slice(&value.to_le_bytes());
}

/// Write an IFD entry whose value is an offset to external data (for ASCII strings etc).
fn write_entry_offset(buf: &mut Vec<u8>, tag: u16, typ: u16, count: u32, offset: u32) {
    buf.extend_from_slice(&tag.to_le_bytes());
    buf.extend_from_slice(&typ.to_le_bytes());
    buf.extend_from_slice(&count.to_le_bytes());
    buf.extend_from_slice(&offset.to_le_bytes());
}

/// Build a Trestle-like TIFF with Software="MedScan..." and an ImageDescription.
fn build_trestle_tiff(
    width: u32,
    height: u32,
    tile_w: u16,
    tile_h: u16,
    image_desc: &str,
) -> Vec<u8> {
    let software = "MedScan 1.0\0";
    let image_desc_nul = format!("{image_desc}\0");

    // Lay out external string data after the IFD
    // IFD starts at offset 8
    // 6 entries + next offset: 2 + 6*12 + 4 = 78 bytes, IFD ends at 86
    let software_offset = 86u32;
    let desc_offset = software_offset + software.len() as u32;

    let mut buf = Vec::new();

    // Header
    buf.extend_from_slice(&[0x49, 0x49]); // little-endian
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&8u32.to_le_bytes()); // IFD at offset 8

    // IFD: 6 entries (tags must be in ascending order)
    buf.extend_from_slice(&6u16.to_le_bytes());
    write_entry(&mut buf, tags::IMAGE_WIDTH, 4, 1, width); // 256
    write_entry(&mut buf, tags::IMAGE_LENGTH, 4, 1, height); // 257
    write_entry_offset(
        &mut buf,
        tags::IMAGE_DESCRIPTION,
        2,
        image_desc_nul.len() as u32,
        desc_offset,
    ); // 270
    write_entry_offset(
        &mut buf,
        tags::SOFTWARE,
        2,
        software.len() as u32,
        software_offset,
    ); // 305
    write_entry(&mut buf, tags::TILE_WIDTH, 3, 1, tile_w as u32); // 322
    write_entry(&mut buf, tags::TILE_LENGTH, 3, 1, tile_h as u32); // 323

    // Next IFD = 0
    buf.extend_from_slice(&0u32.to_le_bytes());

    // String data
    buf.extend_from_slice(software.as_bytes());
    buf.extend_from_slice(image_desc_nul.as_bytes());

    buf
}

/// Build a Trestle TIFF with no Software tag.
fn build_trestle_no_software(width: u32, height: u32, tile_w: u16, tile_h: u16) -> Vec<u8> {
    let image_desc = "some description\0";
    let desc_offset = 8 + 2 + 5 * 12 + 4; // 5 entries

    let mut buf = Vec::new();
    buf.extend_from_slice(&[0x49, 0x49]);
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&8u32.to_le_bytes());

    buf.extend_from_slice(&5u16.to_le_bytes());
    write_entry(&mut buf, tags::IMAGE_WIDTH, 4, 1, width);
    write_entry(&mut buf, tags::IMAGE_LENGTH, 4, 1, height);
    write_entry_offset(
        &mut buf,
        tags::IMAGE_DESCRIPTION,
        2,
        image_desc.len() as u32,
        desc_offset as u32,
    );
    write_entry(&mut buf, tags::TILE_WIDTH, 3, 1, tile_w as u32);
    write_entry(&mut buf, tags::TILE_LENGTH, 3, 1, tile_h as u32);
    buf.extend_from_slice(&0u32.to_le_bytes());

    buf.extend_from_slice(image_desc.as_bytes());
    buf
}

/// Build a Trestle TIFF with bad Software tag.
fn build_trestle_bad_software(width: u32, height: u32, tile_w: u16, tile_h: u16) -> Vec<u8> {
    // Same as trestle but Software = "NotMedScan"
    let software = "NotMedScan\0";
    let image_desc = "desc\0";

    let software_offset = 8 + 2 + 6 * 12 + 4; // after IFD
    let desc_offset = software_offset as u32 + software.len() as u32;

    let mut buf = Vec::new();
    buf.extend_from_slice(&[0x49, 0x49]);
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&8u32.to_le_bytes());

    buf.extend_from_slice(&6u16.to_le_bytes());
    write_entry(&mut buf, tags::IMAGE_WIDTH, 4, 1, width);
    write_entry(&mut buf, tags::IMAGE_LENGTH, 4, 1, height);
    write_entry_offset(
        &mut buf,
        tags::IMAGE_DESCRIPTION,
        2,
        image_desc.len() as u32,
        desc_offset,
    );
    write_entry_offset(
        &mut buf,
        tags::SOFTWARE,
        2,
        software.len() as u32,
        software_offset as u32,
    );
    write_entry(&mut buf, tags::TILE_WIDTH, 3, 1, tile_w as u32);
    write_entry(&mut buf, tags::TILE_LENGTH, 3, 1, tile_h as u32);
    buf.extend_from_slice(&0u32.to_le_bytes());

    buf.extend_from_slice(software.as_bytes());
    buf.extend_from_slice(image_desc.as_bytes());
    buf
}

// ── Detection tests (synthetic) ────────────────────────────────────

#[test]
fn detect_accepts_trestle_tiff() {
    let data = build_trestle_tiff(1024, 768, 256, 256, "Objective Power=20");
    let reader = Box::new(MemReader(data.clone()));
    let mut tl = TiffLike::try_parse(reader).unwrap();

    let driver = TrestleDriver;
    assert!(driver.detect(&MemReader(data), Some(&mut tl)).unwrap());
}

#[test]
fn detect_rejects_no_tiff() {
    let driver = TrestleDriver;
    let data = vec![0x89, 0x50, 0x4E, 0x47];
    let reader = MemReader(data);
    let result = driver.detect(&reader, None);
    assert!(result.is_err());
}

#[test]
fn detect_rejects_no_software() {
    let data = build_trestle_no_software(1024, 768, 256, 256);
    let reader = Box::new(MemReader(data.clone()));
    let mut tl = TiffLike::try_parse(reader).unwrap();

    let driver = TrestleDriver;
    let result = driver.detect(&MemReader(data), Some(&mut tl));
    assert!(result.is_err());
}

#[test]
fn detect_rejects_bad_software() {
    let data = build_trestle_bad_software(1024, 768, 256, 256);
    let reader = Box::new(MemReader(data.clone()));
    let mut tl = TiffLike::try_parse(reader).unwrap();

    let driver = TrestleDriver;
    let result = driver.detect(&MemReader(data), Some(&mut tl));
    assert!(result.is_err());
}

// ── Open tests (synthetic) ─────────────────────────────────────────

#[test]
fn open_single_level() {
    let data = build_trestle_tiff(1024, 768, 256, 256, "Objective Power=20");
    let reader = Box::new(MemReader(data.clone()));
    let tl = TiffLike::try_parse(reader).unwrap();

    let driver = TrestleDriver;
    let slide_data = driver.open(Box::new(MemReader(data)), Some(tl)).unwrap();

    assert_eq!(slide_data.levels.len(), 1);
    assert_eq!(slide_data.levels[0].info.width, 1024);
    assert_eq!(slide_data.levels[0].info.height, 768);
    assert_eq!(
        slide_data.properties.get("openslide.vendor"),
        Some("trestle")
    );
}

#[test]
fn open_parses_properties() {
    let data = build_trestle_tiff(
        1024,
        768,
        256,
        256,
        "Objective Power=40;Background Color=FF0000;Quality=90",
    );
    let reader = Box::new(MemReader(data.clone()));
    let tl = TiffLike::try_parse(reader).unwrap();

    let driver = TrestleDriver;
    let slide_data = driver.open(Box::new(MemReader(data)), Some(tl)).unwrap();

    assert_eq!(
        slide_data.properties.get("trestle.Objective Power"),
        Some("40")
    );
    assert_eq!(
        slide_data.properties.get("trestle.Background Color"),
        Some("FF0000")
    );
    assert_eq!(slide_data.properties.get("trestle.Quality"), Some("90"));

    // Objective power should be duplicated
    assert_eq!(
        slide_data.properties.get("openslide.objective-power"),
        Some("40")
    );

    // Background color should be set
    assert_eq!(
        slide_data.properties.get("openslide.background-color"),
        Some("FF0000")
    );
}

#[test]
fn open_handles_empty_imagedescription_segments() {
    let data = build_trestle_tiff(1024, 768, 256, 256, ";;Key=Value;;");
    let reader = Box::new(MemReader(data.clone()));
    let tl = TiffLike::try_parse(reader).unwrap();

    let driver = TrestleDriver;
    let slide_data = driver.open(Box::new(MemReader(data)), Some(tl)).unwrap();

    assert_eq!(slide_data.properties.get("trestle.Key"), Some("Value"));
}

#[test]
fn open_handles_property_without_value() {
    let data = build_trestle_tiff(1024, 768, 256, 256, "JustAKey;Another=val");
    let reader = Box::new(MemReader(data.clone()));
    let tl = TiffLike::try_parse(reader).unwrap();

    let driver = TrestleDriver;
    let slide_data = driver.open(Box::new(MemReader(data)), Some(tl)).unwrap();

    assert_eq!(slide_data.properties.get("trestle.JustAKey"), None);
    assert_eq!(slide_data.properties.get("trestle.Another"), Some("val"));
}

// ── Detection pipeline tests ───────────────────────────────────────

#[test]
fn detect_format_finds_trestle() {
    let data = build_trestle_tiff(1024, 768, 256, 256, "test=1");
    let drivers = slidekit::vendor::all_drivers();
    let reader = MemReader(data.clone());
    let mut tl = Some(TiffLike::try_parse(Box::new(MemReader(data))).unwrap());

    let idx = slidekit::vendor::detect_format(&drivers, &reader, &mut tl);
    assert!(idx.is_some());

    let driver = &drivers[idx.unwrap()];
    assert_eq!(driver.vendor(), "trestle");
}

#[test]
fn trestle_detected_before_generic_tiff() {
    // A Trestle TIFF should be detected as "trestle", not "generic-tiff"
    let data = build_trestle_tiff(1024, 768, 256, 256, "desc");
    let drivers = slidekit::vendor::all_drivers();
    let reader = MemReader(data.clone());
    let mut tl = Some(TiffLike::try_parse(Box::new(MemReader(data))).unwrap());

    let idx = slidekit::vendor::detect_format(&drivers, &reader, &mut tl);
    assert!(idx.is_some());
    assert_eq!(drivers[idx.unwrap()].vendor(), "trestle");
}

#[test]
fn no_software_falls_through_to_generic_tiff() {
    // Without Software tag, Trestle should not match, but generic-tiff should
    let data = build_trestle_no_software(1024, 768, 256, 256);
    let drivers = slidekit::vendor::all_drivers();
    let reader = MemReader(data.clone());
    let mut tl = Some(TiffLike::try_parse(Box::new(MemReader(data))).unwrap());

    let idx = slidekit::vendor::detect_format(&drivers, &reader, &mut tl);
    assert!(idx.is_some());
    assert_eq!(drivers[idx.unwrap()].vendor(), "generic-tiff");
}

#[test]
fn bad_software_falls_through_to_generic_tiff() {
    let data = build_trestle_bad_software(1024, 768, 256, 256);
    let drivers = slidekit::vendor::all_drivers();
    let reader = MemReader(data.clone());
    let mut tl = Some(TiffLike::try_parse(Box::new(MemReader(data))).unwrap());

    let idx = slidekit::vendor::detect_format(&drivers, &reader, &mut tl);
    assert!(idx.is_some());
    assert_eq!(drivers[idx.unwrap()].vendor(), "generic-tiff");
}

// ── Integration tests (real test data) ─────────────────────────────

/// Helper to get the Trestle test slide path (extracts from ZIP if needed).
fn trestle_slide_path() -> Option<std::path::PathBuf> {
    let zip_path = common::test_slide_path("Trestle/CMU-1.zip")?;
    let extract_dir = zip_path.parent()?.join("extracted");
    let tif_path = extract_dir.join("CMU-1.tif");

    if tif_path.exists() {
        return Some(tif_path);
    }

    // Extract the ZIP
    std::fs::create_dir_all(&extract_dir).ok()?;
    let file = std::fs::File::open(&zip_path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).ok()?;
        let name = entry.name().to_string();
        if name.ends_with('/') {
            continue;
        }
        let out_path = extract_dir.join(&name);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        let mut out_file = std::fs::File::create(&out_path).ok()?;
        std::io::copy(&mut entry, &mut out_file).ok()?;
    }

    if tif_path.exists() {
        Some(tif_path)
    } else {
        None
    }
}

macro_rules! require_trestle_slide {
    () => {
        match trestle_slide_path() {
            Some(p) => p,
            None => {
                eprintln!("SKIP: Trestle test slide not available");
                return;
            }
        }
    };
}

/// Everything we can assert about `CMU-1.tif` from a single open.
#[test]
fn cmu1() {
    let path = require_trestle_slide!();

    // ── Detection ──
    let vendor = OpenSlide::detect_vendor(&path);
    assert_eq!(vendor.as_deref(), Some("trestle"));

    let slide = OpenSlide::open(&path).expect("should open Trestle CMU-1.tif");

    // ── Levels ──
    assert!(slide.level_count() >= 1, "expected at least one level");

    let (w0, h0) = slide.level0_dimensions();
    assert!(w0 > 0 && h0 > 0, "level 0 dimensions should be positive");

    let ds0 = slide.level_downsample(0).unwrap();
    assert!(
        (ds0 - 1.0).abs() < f64::EPSILON,
        "level 0 downsample should be 1.0"
    );

    // ── Properties ──
    assert_eq!(
        slide.property_value("openslide.vendor"),
        Some("trestle"),
        "vendor should be 'trestle'"
    );

    let names = slide.property_names();
    let trestle_props: Vec<_> = names.iter().filter(|n| n.starts_with("trestle.")).collect();
    assert!(
        !trestle_props.is_empty(),
        "should have trestle.* properties from ImageDescription"
    );

    // ── read_region ──
    let pixels = slide.read_region(0, 0, 0, 256, 256).unwrap();
    assert_eq!(pixels.len(), 256 * 256);

    let non_zero = pixels.iter().filter(|&&p| p != 0).count();
    assert!(
        non_zero > 0,
        "read_region should return non-transparent pixels"
    );
}

/// The decoded-tile cache must not change a pixel on an **overlapping** tile
/// grid, which is the case here most at risk. Trestle's grid advances by less
/// than a tile, so tiles overlap in the output and the last one composited
/// wins: a cache that painted its hits ahead of the tiles it fetched would move
/// the seams. It also indexes tiles by the real TIFF column count rather than
/// the grid's, so a key taken from the grid position would drift by a tile per
/// row deep into the slide.
#[test]
fn cached_reads_are_pixel_identical() {
    let path = require_trestle_slide!();
    common::assert_cache_is_transparent(
        || OpenSlide::open(&path).expect("should open Trestle CMU-1.tif"),
        0,
        &[
            (10_000, 10_000, 300, 300),
            (10_150, 10_150, 300, 300),
            (10_000, 10_150, 300, 300),
        ],
    );
}
