//! Tests for the TiffLike parser.

use slidekit::decode::tiff::{TiffLike, tags};
use slidekit::io::SlideReader;

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

/// Build a minimal valid classic TIFF with one IFD containing a few tags.
fn build_minimal_tiff() -> Vec<u8> {
    let mut buf = Vec::new();

    // Header: little-endian, classic TIFF
    buf.extend_from_slice(&[0x49, 0x49]); // II (little-endian)
    buf.extend_from_slice(&42u16.to_le_bytes()); // version 42
    buf.extend_from_slice(&8u32.to_le_bytes()); // offset to first IFD

    // IFD at offset 8
    let num_entries: u16 = 4;
    buf.extend_from_slice(&num_entries.to_le_bytes());

    // Entry 1: ImageWidth (256) = 1024, type LONG (4), count 1
    buf.extend_from_slice(&256u16.to_le_bytes()); // tag
    buf.extend_from_slice(&4u16.to_le_bytes()); // type LONG
    buf.extend_from_slice(&1u32.to_le_bytes()); // count
    buf.extend_from_slice(&1024u32.to_le_bytes()); // value (inline)

    // Entry 2: ImageLength (257) = 768, type LONG
    buf.extend_from_slice(&257u16.to_le_bytes());
    buf.extend_from_slice(&4u16.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&768u32.to_le_bytes());

    // Entry 3: TileWidth (322) = 256, type SHORT
    buf.extend_from_slice(&322u16.to_le_bytes());
    buf.extend_from_slice(&3u16.to_le_bytes()); // SHORT
    buf.extend_from_slice(&1u32.to_le_bytes());
    // SHORT value in 4-byte field (only first 2 bytes used)
    buf.extend_from_slice(&256u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // padding

    // Entry 4: TileLength (323) = 256, type SHORT
    buf.extend_from_slice(&323u16.to_le_bytes());
    buf.extend_from_slice(&3u16.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&256u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());

    // Next IFD offset = 0 (end of chain)
    buf.extend_from_slice(&0u32.to_le_bytes());

    buf
}

#[test]
fn parse_minimal_tiff() {
    let data = build_minimal_tiff();
    let reader = Box::new(MemReader(data));
    let mut tl = TiffLike::try_parse(reader).expect("should parse minimal TIFF");

    assert_eq!(tl.directory_count(), 1);
    assert!(!tl.is_ndpi());
    assert!(tl.is_tiled(0));

    // Check tag values
    assert_eq!(tl.get_uint(0, tags::IMAGE_WIDTH).unwrap(), 1024);
    assert_eq!(tl.get_uint(0, tags::IMAGE_LENGTH).unwrap(), 768);
    assert_eq!(tl.get_uint(0, tags::TILE_WIDTH).unwrap(), 256);
    assert_eq!(tl.get_uint(0, tags::TILE_LENGTH).unwrap(), 256);
}

#[test]
fn not_a_tiff() {
    let data = vec![0x89, 0x50, 0x4E, 0x47]; // PNG magic
    let reader = Box::new(MemReader(data));
    let result = TiffLike::try_parse(reader);
    assert!(result.is_err());
}

/// Build a minimal BigTIFF.
fn build_minimal_bigtiff() -> Vec<u8> {
    let mut buf = Vec::new();

    // Header
    buf.extend_from_slice(&[0x49, 0x49]); // II
    buf.extend_from_slice(&43u16.to_le_bytes()); // version 43 (BigTIFF)
    buf.extend_from_slice(&8u16.to_le_bytes()); // offset size
    buf.extend_from_slice(&0u16.to_le_bytes()); // padding
    buf.extend_from_slice(&16u64.to_le_bytes()); // offset to first IFD

    // IFD at offset 16
    let num_entries: u64 = 1;
    buf.extend_from_slice(&num_entries.to_le_bytes());

    // Entry: ImageWidth = 2048, type LONG (4), count 1
    buf.extend_from_slice(&256u16.to_le_bytes()); // tag
    buf.extend_from_slice(&4u16.to_le_bytes()); // type LONG
    buf.extend_from_slice(&1u64.to_le_bytes()); // count (8 bytes in BigTIFF)
    // value field is 8 bytes in BigTIFF
    buf.extend_from_slice(&2048u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // padding

    // Next IFD offset = 0
    buf.extend_from_slice(&0u64.to_le_bytes());

    buf
}

#[test]
fn parse_bigtiff() {
    let data = build_minimal_bigtiff();
    let reader = Box::new(MemReader(data));
    let mut tl = TiffLike::try_parse(reader).expect("should parse BigTIFF");

    assert_eq!(tl.directory_count(), 1);
    assert_eq!(tl.get_uint(0, tags::IMAGE_WIDTH).unwrap(), 2048);
}

/// Build a TIFF with an out-of-line (deferred) ASCII value.
fn build_tiff_with_deferred_string() -> Vec<u8> {
    let mut buf = Vec::new();

    // Header
    buf.extend_from_slice(&[0x49, 0x49]);
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&8u32.to_le_bytes());

    // IFD at offset 8
    let num_entries: u16 = 1;
    buf.extend_from_slice(&num_entries.to_le_bytes());

    // String "Hello World!" at offset past the IFD
    let string_data = b"Hello World!\0";
    let string_offset: u32 = 8 + 2 + 12 + 4; // header(8) + count(2) + entry(12) + next(4) = 26

    // Entry: ImageDescription (270) = ASCII, count 13
    buf.extend_from_slice(&270u16.to_le_bytes()); // tag
    buf.extend_from_slice(&2u16.to_le_bytes()); // type ASCII
    buf.extend_from_slice(&(string_data.len() as u32).to_le_bytes()); // count
    buf.extend_from_slice(&string_offset.to_le_bytes()); // offset

    // Next IFD = 0
    buf.extend_from_slice(&0u32.to_le_bytes());

    // String data
    buf.extend_from_slice(string_data);

    buf
}

#[test]
fn parse_deferred_ascii() {
    let data = build_tiff_with_deferred_string();
    let reader = Box::new(MemReader(data));
    let mut tl = TiffLike::try_parse(reader).expect("should parse");

    let desc = tl.get_string(0, tags::IMAGE_DESCRIPTION).unwrap();
    assert_eq!(desc, "Hello World!");
}

/// Multiple IFDs (2 directories).
fn build_tiff_two_directories() -> Vec<u8> {
    let mut buf = Vec::new();

    // Header
    buf.extend_from_slice(&[0x49, 0x49]);
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&8u32.to_le_bytes()); // first IFD at 8

    // First IFD at offset 8
    let num_entries: u16 = 1;
    buf.extend_from_slice(&num_entries.to_le_bytes());

    // ImageWidth = 1024
    buf.extend_from_slice(&256u16.to_le_bytes());
    buf.extend_from_slice(&4u16.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&1024u32.to_le_bytes());

    // Offset to second IFD (after this entry)
    let second_ifd_offset: u32 = 8 + 2 + 12 + 4; // = 26
    buf.extend_from_slice(&second_ifd_offset.to_le_bytes());

    // Second IFD at offset 26
    buf.extend_from_slice(&num_entries.to_le_bytes());

    // ImageWidth = 512
    buf.extend_from_slice(&256u16.to_le_bytes());
    buf.extend_from_slice(&4u16.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&512u32.to_le_bytes());

    // Next IFD = 0 (end)
    buf.extend_from_slice(&0u32.to_le_bytes());

    buf
}

#[test]
fn parse_two_directories() {
    let data = build_tiff_two_directories();
    let reader = Box::new(MemReader(data));
    let mut tl = TiffLike::try_parse(reader).expect("should parse");

    assert_eq!(tl.directory_count(), 2);
    assert_eq!(tl.get_uint(0, tags::IMAGE_WIDTH).unwrap(), 1024);
    assert_eq!(tl.get_uint(1, tags::IMAGE_WIDTH).unwrap(), 512);
}

#[test]
fn big_endian_tiff() {
    let mut buf = Vec::new();

    // Header: big-endian
    buf.extend_from_slice(&[0x4D, 0x4D]); // MM
    buf.extend_from_slice(&42u16.to_be_bytes());
    buf.extend_from_slice(&8u32.to_be_bytes());

    // IFD
    buf.extend_from_slice(&1u16.to_be_bytes()); // 1 entry

    // ImageWidth = 4096, LONG
    buf.extend_from_slice(&256u16.to_be_bytes());
    buf.extend_from_slice(&4u16.to_be_bytes());
    buf.extend_from_slice(&1u32.to_be_bytes());
    buf.extend_from_slice(&4096u32.to_be_bytes());

    // Next IFD = 0
    buf.extend_from_slice(&0u32.to_be_bytes());

    let reader = Box::new(MemReader(buf));
    let mut tl = TiffLike::try_parse(reader).expect("should parse big-endian");

    assert_eq!(tl.directory_count(), 1);
    assert_eq!(tl.get_uint(0, tags::IMAGE_WIDTH).unwrap(), 4096);
}

#[test]
fn value_count_for_missing_tag() {
    let data = build_minimal_tiff();
    let reader = Box::new(MemReader(data));
    let tl = TiffLike::try_parse(reader).expect("should parse");

    // Tag that doesn't exist
    assert_eq!(tl.value_count(0, tags::SOFTWARE), 0);
}
