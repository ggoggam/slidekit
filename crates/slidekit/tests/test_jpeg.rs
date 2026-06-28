//! Tests for the JPEG tile decoder.

use slidekit::decode::TileDecoder;
use slidekit::decode::jpeg::JpegDecoder;

#[test]
fn jpeg_decoder_rejects_invalid_data() {
    let decoder = JpegDecoder::new();
    let result = decoder.decode(&[0x00, 0x01, 0x02], 1, 1);
    assert!(result.is_err());
}

#[test]
fn jpeg_decoder_rejects_empty_data() {
    let decoder = JpegDecoder::new();
    let result = decoder.decode(&[], 1, 1);
    assert!(result.is_err());
}

#[test]
fn jpeg_tables_prepend_logic() {
    // Test the table-prepending logic.
    // Create fake "tables" with SOI + content + EOI
    let tables = vec![
        0xFF, 0xD8, // SOI
        0xAA, 0xBB, // table content
        0xFF, 0xD9, // EOI
    ];

    let tile_data = vec![
        0xFF, 0xD8, // SOI (should be stripped)
        0xCC, 0xDD, // tile content
        0xFF, 0xD9, // EOI
    ];

    let decoder = JpegDecoder::with_tables(tables);
    let prepared = decoder.prepare_data(&tile_data).unwrap();
    // Expected: tables_before_eoi + tile_after_soi
    // = [FF D8 AA BB] + [CC DD FF D9]
    assert_eq!(
        prepared.as_ref(),
        &[0xFF, 0xD8, 0xAA, 0xBB, 0xCC, 0xDD, 0xFF, 0xD9]
    );
}

#[test]
fn jpeg_tables_without_soi_in_tile() {
    let tables = vec![
        0xFF, 0xD8, // SOI
        0xAA, 0xBB, 0xFF, 0xD9, // EOI
    ];

    // Tile data without SOI marker
    let tile_data = vec![0xCC, 0xDD, 0xFF, 0xD9];

    let decoder = JpegDecoder::with_tables(tables);
    let prepared = decoder.prepare_data(&tile_data).unwrap();
    // Tables before EOI + all tile data (no SOI to strip)
    assert_eq!(
        prepared.as_ref(),
        &[0xFF, 0xD8, 0xAA, 0xBB, 0xCC, 0xDD, 0xFF, 0xD9]
    );
}

#[test]
fn jpeg_no_tables_passes_through() {
    let decoder = JpegDecoder::new();
    let data = vec![0xFF, 0xD8, 0x01, 0x02, 0xFF, 0xD9];
    let prepared = decoder.prepare_data(&data).unwrap();
    // Should be the same data (borrowed, not copied)
    assert_eq!(prepared.as_ref(), &data[..]);
}

#[test]
fn jpeg_tables_missing_eoi_is_error() {
    let tables = vec![0xFF, 0xD8, 0xAA, 0xBB]; // No EOI
    let decoder = JpegDecoder::with_tables(tables);
    let result = decoder.prepare_data(&[0xFF, 0xD8, 0x01]);
    assert!(result.is_err());
}
