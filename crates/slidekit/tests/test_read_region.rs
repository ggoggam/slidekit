//! End-to-end tests for read_region through the tile pipeline.
//!
//! These tests build synthetic TIFF files with uncompressed tile data
//! and verify that read_region produces correct pixel output.

use slidekit::color;
use slidekit::decode::tiff::tags;

/// Helper to write a TIFF IFD entry.
fn write_entry(buf: &mut Vec<u8>, tag: u16, typ: u16, count: u32, value: u32) {
    buf.extend_from_slice(&tag.to_le_bytes());
    buf.extend_from_slice(&typ.to_le_bytes());
    buf.extend_from_slice(&count.to_le_bytes());
    buf.extend_from_slice(&value.to_le_bytes());
}

/// Build a TIFF with uncompressed 4x4 RGB tiles.
/// Layout: 8x4 image with two 4x4 tiles side by side.
/// Tile 0 = red (255,0,0), Tile 1 = blue (0,0,255)
fn build_uncompressed_tiled_tiff() -> Vec<u8> {
    let tile_w: u32 = 4;
    let tile_h: u32 = 4;
    let image_w: u32 = 8;
    let image_h: u32 = 4;
    let pixels_per_tile = (tile_w * tile_h) as usize;
    let bytes_per_tile = pixels_per_tile * 3; // RGB

    let mut red_tile = vec![0u8; bytes_per_tile];
    for i in 0..pixels_per_tile {
        red_tile[i * 3] = 255;
    }

    let mut blue_tile = vec![0u8; bytes_per_tile];
    for i in 0..pixels_per_tile {
        blue_tile[i * 3 + 2] = 255;
    }

    let mut buf = Vec::new();

    // TIFF Header
    buf.extend_from_slice(&[0x49, 0x49]); // Little-endian
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&8u32.to_le_bytes()); // IFD at offset 8

    let num_entries: u16 = 8;
    buf.extend_from_slice(&num_entries.to_le_bytes());

    // IFD ends at: 8 (header) + 2 (count) + 8*12 (entries) + 4 (next) = 110
    let ifd_end = 8 + 2 + num_entries as usize * 12 + 4;

    // Out-of-line data layout:
    let tile_offsets_offset = ifd_end;
    let tile_byte_counts_offset = ifd_end + 8;
    let tile0_data_offset = ifd_end + 16;
    let tile1_data_offset = tile0_data_offset + bytes_per_tile;

    // Entries sorted by tag number
    write_entry(&mut buf, tags::IMAGE_WIDTH, 4, 1, image_w);
    write_entry(&mut buf, tags::IMAGE_LENGTH, 4, 1, image_h);
    write_entry(&mut buf, tags::COMPRESSION, 3, 1, 1);
    write_entry(&mut buf, tags::SAMPLES_PER_PIXEL, 3, 1, 3);
    write_entry(&mut buf, tags::TILE_WIDTH, 3, 1, tile_w);
    write_entry(&mut buf, tags::TILE_LENGTH, 3, 1, tile_h);
    write_entry(
        &mut buf,
        tags::TILE_OFFSETS,
        4,
        2,
        tile_offsets_offset as u32,
    );
    write_entry(
        &mut buf,
        tags::TILE_BYTE_COUNTS,
        4,
        2,
        tile_byte_counts_offset as u32,
    );

    // Next IFD = 0
    buf.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(buf.len(), ifd_end);

    // Tile offsets
    buf.extend_from_slice(&(tile0_data_offset as u32).to_le_bytes());
    buf.extend_from_slice(&(tile1_data_offset as u32).to_le_bytes());

    // Tile byte counts
    buf.extend_from_slice(&(bytes_per_tile as u32).to_le_bytes());
    buf.extend_from_slice(&(bytes_per_tile as u32).to_le_bytes());

    // Tile data
    buf.extend_from_slice(&red_tile);
    buf.extend_from_slice(&blue_tile);

    buf
}

/// Helper: write TIFF to temp file and open as OpenSlide.
fn open_test_tiff(name: &str) -> (slidekit::OpenSlide, std::path::PathBuf) {
    let tiff_data = build_uncompressed_tiled_tiff();
    let dir = std::env::temp_dir().join("openslide_test_read_region");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, &tiff_data).unwrap();
    let slide = slidekit::OpenSlide::open(&path).unwrap();
    (slide, path)
}

#[test]
fn read_region_full_image() {
    let (slide, path) = open_test_tiff("test_full.tif");

    assert_eq!(slide.level_count(), 1);
    assert_eq!(slide.level0_dimensions(), (8, 4));

    let pixels = slide.read_region(0, 0, 0, 8, 4).unwrap();
    assert_eq!(pixels.len(), 32);

    let expected_red = color::argb(0xFF, 255, 0, 0);
    let expected_blue = color::argb(0xFF, 0, 0, 255);

    for row in 0..4 {
        for col in 0..4 {
            let idx = row * 8 + col;
            assert_eq!(
                pixels[idx], expected_red,
                "pixel ({col},{row}) should be red"
            );
        }
        for col in 4..8 {
            let idx = row * 8 + col;
            assert_eq!(
                pixels[idx], expected_blue,
                "pixel ({col},{row}) should be blue"
            );
        }
    }

    std::fs::remove_file(&path).ok();
}

#[test]
fn read_region_partial() {
    let (slide, path) = open_test_tiff("test_partial.tif");

    // Read just the red tile
    let pixels = slide.read_region(0, 0, 0, 4, 4).unwrap();
    assert_eq!(pixels.len(), 16);
    let expected_red = color::argb(0xFF, 255, 0, 0);
    for p in &pixels {
        assert_eq!(*p, expected_red);
    }

    // Read just the blue tile
    let pixels = slide.read_region(4, 0, 0, 4, 4).unwrap();
    assert_eq!(pixels.len(), 16);
    let expected_blue = color::argb(0xFF, 0, 0, 255);
    for p in &pixels {
        assert_eq!(*p, expected_blue);
    }

    std::fs::remove_file(&path).ok();
}

#[test]
fn read_region_spanning_tiles() {
    let (slide, path) = open_test_tiff("test_spanning.tif");

    // 4x2 region spanning both tiles (cols 2..6, rows 1..3)
    let pixels = slide.read_region(2, 1, 0, 4, 2).unwrap();
    assert_eq!(pixels.len(), 8);

    let expected_red = color::argb(0xFF, 255, 0, 0);
    let expected_blue = color::argb(0xFF, 0, 0, 255);

    for row in 0..2 {
        for col in 0..2 {
            let idx = row * 4 + col;
            assert_eq!(
                pixels[idx], expected_red,
                "pixel ({col},{row}) should be red"
            );
        }
        for col in 2..4 {
            let idx = row * 4 + col;
            assert_eq!(
                pixels[idx], expected_blue,
                "pixel ({col},{row}) should be blue"
            );
        }
    }

    std::fs::remove_file(&path).ok();
}

#[test]
fn read_region_invalid_level() {
    let (slide, path) = open_test_tiff("test_invalid_level.tif");
    assert!(slide.read_region(0, 0, 5, 1, 1).is_err());
    assert!(slide.read_region(0, 0, -1, 1, 1).is_err());
    std::fs::remove_file(&path).ok();
}

#[test]
fn read_region_zero_size() {
    let (slide, path) = open_test_tiff("test_zero.tif");
    let pixels = slide.read_region(0, 0, 0, 0, 0).unwrap();
    assert!(pixels.is_empty());
    std::fs::remove_file(&path).ok();
}
