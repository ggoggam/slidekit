//! Tests for the generic-tiff vendor driver.

use slidekit::decode::tiff::{TiffLike, tags};
use slidekit::io::SlideReader;
use slidekit::vendor::VendorDriver;
use slidekit::vendor::generic_tiff::GenericTiffDriver;

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

/// Build a single-directory tiled TIFF.
fn build_tiled_tiff(width: u32, height: u32, tile_w: u16, tile_h: u16) -> Vec<u8> {
    let mut buf = Vec::new();

    // Header
    buf.extend_from_slice(&[0x49, 0x49]); // II
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&8u32.to_le_bytes()); // IFD at offset 8

    // IFD: 4 entries
    buf.extend_from_slice(&4u16.to_le_bytes());
    write_entry(&mut buf, tags::IMAGE_WIDTH, 4, 1, width); // LONG
    write_entry(&mut buf, tags::IMAGE_LENGTH, 4, 1, height); // LONG
    write_entry(&mut buf, tags::TILE_WIDTH, 3, 1, tile_w as u32); // SHORT
    write_entry(&mut buf, tags::TILE_LENGTH, 3, 1, tile_h as u32); // SHORT

    // Next IFD = 0
    buf.extend_from_slice(&0u32.to_le_bytes());

    buf
}

/// Build a multi-directory tiled TIFF (pyramid: level 0 + reduced levels).
fn build_pyramid_tiff(levels: &[(u32, u32)], tile_w: u16, tile_h: u16) -> Vec<u8> {
    let mut buf = Vec::new();

    // Header
    buf.extend_from_slice(&[0x49, 0x49]);
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&8u32.to_le_bytes());

    // Pre-compute IFD offsets.
    // Each IFD: 2 (count) + entries*12 + 4 (next offset)
    // Dir 0: 5 entries (w, h, tw, th, + we skip subfiletype for dir 0)
    // Dir N>0: 5 entries (w, h, tw, th, subfiletype)
    let dir0_entries = 4u16;
    let dirn_entries = 5u16;

    let dir0_size = 2 + dir0_entries as usize * 12 + 4;
    let dirn_size = 2 + dirn_entries as usize * 12 + 4;

    let mut offsets = Vec::new();
    let mut pos = 8usize; // first IFD offset
    for i in 0..levels.len() {
        offsets.push(pos);
        pos += if i == 0 { dir0_size } else { dirn_size };
    }

    for (i, &(w, h)) in levels.iter().enumerate() {
        if i == 0 {
            // Dir 0: no SUBFILETYPE
            buf.extend_from_slice(&dir0_entries.to_le_bytes());
            write_entry(&mut buf, tags::IMAGE_WIDTH, 4, 1, w);
            write_entry(&mut buf, tags::IMAGE_LENGTH, 4, 1, h);
            write_entry(&mut buf, tags::TILE_WIDTH, 3, 1, tile_w as u32);
            write_entry(&mut buf, tags::TILE_LENGTH, 3, 1, tile_h as u32);
        } else {
            // Dir N: include SUBFILETYPE = FILETYPE_REDUCEDIMAGE
            buf.extend_from_slice(&dirn_entries.to_le_bytes());
            write_entry(&mut buf, tags::SUBFILE_TYPE, 4, 1, 1); // REDUCEDIMAGE
            write_entry(&mut buf, tags::IMAGE_WIDTH, 4, 1, w);
            write_entry(&mut buf, tags::IMAGE_LENGTH, 4, 1, h);
            write_entry(&mut buf, tags::TILE_WIDTH, 3, 1, tile_w as u32);
            write_entry(&mut buf, tags::TILE_LENGTH, 3, 1, tile_h as u32);
        }

        // Next IFD offset
        if i + 1 < levels.len() {
            buf.extend_from_slice(&(offsets[i + 1] as u32).to_le_bytes());
        } else {
            buf.extend_from_slice(&0u32.to_le_bytes());
        }
    }

    buf
}

/// Build a non-tiled TIFF (strip-based).
fn build_strip_tiff() -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&[0x49, 0x49]);
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&8u32.to_le_bytes());

    // IFD: 2 entries (width, height — no tile tags)
    buf.extend_from_slice(&2u16.to_le_bytes());
    write_entry(&mut buf, tags::IMAGE_WIDTH, 4, 1, 1024);
    write_entry(&mut buf, tags::IMAGE_LENGTH, 4, 1, 768);

    buf.extend_from_slice(&0u32.to_le_bytes());
    buf
}

// ── Detection tests ────────────────────────────────────────────────

#[test]
fn detect_accepts_tiled_tiff() {
    let data = build_tiled_tiff(1024, 768, 256, 256);
    let reader = Box::new(MemReader(data.clone()));
    let mut tl = TiffLike::try_parse(reader).unwrap();

    let driver = GenericTiffDriver;
    assert!(driver.detect(&MemReader(data), Some(&mut tl)).unwrap());
}

#[test]
fn detect_rejects_non_tiff() {
    let driver = GenericTiffDriver;
    let data = vec![0x89, 0x50, 0x4E, 0x47]; // PNG magic
    let reader = MemReader(data);

    // No TiffLike available
    let result = driver.detect(&reader, None);
    assert!(result.is_err());
}

#[test]
fn detect_rejects_strip_tiff() {
    let data = build_strip_tiff();
    let reader = Box::new(MemReader(data.clone()));
    let mut tl = TiffLike::try_parse(reader).unwrap();

    let driver = GenericTiffDriver;
    let result = driver.detect(&MemReader(data), Some(&mut tl));
    assert!(result.is_err());
}

// ── Open tests ─────────────────────────────────────────────────────

#[test]
fn open_single_level() {
    let data = build_tiled_tiff(1024, 768, 256, 256);
    let reader = Box::new(MemReader(data.clone()));
    let tl = TiffLike::try_parse(reader).unwrap();

    let driver = GenericTiffDriver;
    let slide_data = driver.open(Box::new(MemReader(data)), Some(tl)).unwrap();

    assert_eq!(slide_data.levels.len(), 1);
    assert_eq!(slide_data.levels[0].info.width, 1024);
    assert_eq!(slide_data.levels[0].info.height, 768);
    assert_eq!(slide_data.levels[0].info.tile_width, 256);
    assert_eq!(slide_data.levels[0].info.tile_height, 256);
    assert!((slide_data.levels[0].info.downsample - 1.0).abs() < f64::EPSILON);

    assert_eq!(
        slide_data.properties.get("openslide.vendor"),
        Some("generic-tiff")
    );
}

#[test]
fn open_pyramid_multiple_levels() {
    let levels = [(4096, 4096), (2048, 2048), (1024, 1024), (512, 512)];
    let data = build_pyramid_tiff(&levels, 256, 256);
    let reader = Box::new(MemReader(data.clone()));
    let tl = TiffLike::try_parse(reader).unwrap();

    let driver = GenericTiffDriver;
    let slide_data = driver.open(Box::new(MemReader(data)), Some(tl)).unwrap();

    assert_eq!(slide_data.levels.len(), 4);

    // Sorted by width descending
    assert_eq!(slide_data.levels[0].info.width, 4096);
    assert_eq!(slide_data.levels[1].info.width, 2048);
    assert_eq!(slide_data.levels[2].info.width, 1024);
    assert_eq!(slide_data.levels[3].info.width, 512);

    // Downsample factors
    assert!((slide_data.levels[0].info.downsample - 1.0).abs() < f64::EPSILON);
    assert!((slide_data.levels[1].info.downsample - 2.0).abs() < f64::EPSILON);
    assert!((slide_data.levels[2].info.downsample - 4.0).abs() < f64::EPSILON);
    assert!((slide_data.levels[3].info.downsample - 8.0).abs() < f64::EPSILON);
}

#[test]
fn open_skips_non_reduced_directories() {
    // Build a 2-dir TIFF where dir 1 is tiled but NOT marked as reduced
    let mut buf = Vec::new();

    buf.extend_from_slice(&[0x49, 0x49]);
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&8u32.to_le_bytes());

    // Dir 0: tiled, 4096x4096
    let dir0_entries = 4u16;
    buf.extend_from_slice(&dir0_entries.to_le_bytes());
    write_entry(&mut buf, tags::IMAGE_WIDTH, 4, 1, 4096);
    write_entry(&mut buf, tags::IMAGE_LENGTH, 4, 1, 4096);
    write_entry(&mut buf, tags::TILE_WIDTH, 3, 1, 256);
    write_entry(&mut buf, tags::TILE_LENGTH, 3, 1, 256);

    // Next IFD offset
    let dir1_offset = buf.len() + 4; // current pos + next_offset(4)
    buf.extend_from_slice(&(dir1_offset as u32).to_le_bytes());

    // Dir 1: tiled, 2048x2048, but NO SUBFILETYPE tag
    buf.extend_from_slice(&dir0_entries.to_le_bytes());
    write_entry(&mut buf, tags::IMAGE_WIDTH, 4, 1, 2048);
    write_entry(&mut buf, tags::IMAGE_LENGTH, 4, 1, 2048);
    write_entry(&mut buf, tags::TILE_WIDTH, 3, 1, 256);
    write_entry(&mut buf, tags::TILE_LENGTH, 3, 1, 256);

    buf.extend_from_slice(&0u32.to_le_bytes());

    let reader = Box::new(MemReader(buf.clone()));
    let tl = TiffLike::try_parse(reader).unwrap();

    let driver = GenericTiffDriver;
    let slide_data = driver.open(Box::new(MemReader(buf)), Some(tl)).unwrap();

    // Dir 1 should be skipped (no SUBFILETYPE)
    assert_eq!(slide_data.levels.len(), 1);
    assert_eq!(slide_data.levels[0].info.width, 4096);
}

// ── Integration with detect_vendor pipeline ────────────────────────

#[test]
fn detect_format_finds_generic_tiff() {
    let data = build_tiled_tiff(1024, 768, 256, 256);

    // Use the vendor detection pipeline directly
    let drivers = slidekit::vendor::all_drivers();
    let reader = MemReader(data.clone());
    let mut tl = Some(TiffLike::try_parse(Box::new(MemReader(data))).unwrap());

    let idx = slidekit::vendor::detect_format(&drivers, &reader, &mut tl);
    assert!(idx.is_some());

    let driver = &drivers[idx.unwrap()];
    assert_eq!(driver.vendor(), "generic-tiff");
}

#[test]
fn detect_format_returns_none_for_non_tiff() {
    let data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]; // PNG
    let drivers = slidekit::vendor::all_drivers();
    let reader = MemReader(data);
    let mut tl: Option<TiffLike> = None; // Not a TIFF

    let idx = slidekit::vendor::detect_format(&drivers, &reader, &mut tl);
    assert!(idx.is_none());
}
