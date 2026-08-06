//! End-to-end tests for the async read path (`read_region_async`) backed by an
//! `object_store` source. A synthetic uncompressed tiled TIFF is served from an
//! in-memory store, so these exercise the full open + async tile-read pipeline
//! without touching the network or downloaded test data.
//!
//! The second half covers the level kinds that path can't range-fetch and hands
//! to the synchronous reader instead — the case where "synchronous" and "inside
//! a runtime" collide.

use std::sync::Arc;

use object_store::memory::InMemory;
use object_store::path::Path as StorePath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use slidekit::OpenSlide;
use slidekit::color;
use slidekit::decode::tiff::tags;

fn write_entry(buf: &mut Vec<u8>, tag: u16, typ: u16, count: u32, value: u32) {
    buf.extend_from_slice(&tag.to_le_bytes());
    buf.extend_from_slice(&typ.to_le_bytes());
    buf.extend_from_slice(&count.to_le_bytes());
    buf.extend_from_slice(&value.to_le_bytes());
}

/// 8x4 image of two 4x4 tiles: tile 0 = red, tile 1 = blue. Uncompressed RGB.
fn build_uncompressed_tiled_tiff() -> Vec<u8> {
    let tile_w: u32 = 4;
    let tile_h: u32 = 4;
    let image_w: u32 = 8;
    let image_h: u32 = 4;
    let pixels_per_tile = (tile_w * tile_h) as usize;
    let bytes_per_tile = pixels_per_tile * 3;

    let mut red_tile = vec![0u8; bytes_per_tile];
    for i in 0..pixels_per_tile {
        red_tile[i * 3] = 255;
    }
    let mut blue_tile = vec![0u8; bytes_per_tile];
    for i in 0..pixels_per_tile {
        blue_tile[i * 3 + 2] = 255;
    }

    let mut buf = Vec::new();
    buf.extend_from_slice(&[0x49, 0x49]);
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&8u32.to_le_bytes());

    let num_entries: u16 = 8;
    buf.extend_from_slice(&num_entries.to_le_bytes());

    let ifd_end = 8 + 2 + num_entries as usize * 12 + 4;
    let tile_offsets_offset = ifd_end;
    let tile_byte_counts_offset = ifd_end + 8;
    let tile0_data_offset = ifd_end + 16;
    let tile1_data_offset = tile0_data_offset + bytes_per_tile;

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

    buf.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(buf.len(), ifd_end);

    buf.extend_from_slice(&(tile0_data_offset as u32).to_le_bytes());
    buf.extend_from_slice(&(tile1_data_offset as u32).to_le_bytes());
    buf.extend_from_slice(&(bytes_per_tile as u32).to_le_bytes());
    buf.extend_from_slice(&(bytes_per_tile as u32).to_le_bytes());
    buf.extend_from_slice(&red_tile);
    buf.extend_from_slice(&blue_tile);

    buf
}

async fn store_with_slide() -> (Arc<dyn ObjectStore>, StorePath) {
    let store = InMemory::new();
    let path = StorePath::from("slide.tif");
    let data = build_uncompressed_tiled_tiff();
    store.put(&path, PutPayload::from(data)).await.unwrap();
    (Arc::new(store), path)
}

#[tokio::test(flavor = "multi_thread")]
async fn read_region_async_full_image() {
    let (store, path) = store_with_slide().await;
    let slide = OpenSlide::open_store_async(store, path).await.unwrap();

    assert_eq!(slide.level_count(), 1);
    assert_eq!(slide.level0_dimensions(), (8, 4));

    let pixels = slide.read_region_async(0, 0, 0, 8, 4).await.unwrap();
    assert_eq!(pixels.len(), 32);

    let red = color::argb(0xFF, 255, 0, 0);
    let blue = color::argb(0xFF, 0, 0, 255);
    for row in 0..4 {
        for col in 0..4 {
            assert_eq!(pixels[row * 8 + col], red, "({col},{row}) should be red");
        }
        for col in 4..8 {
            assert_eq!(pixels[row * 8 + col], blue, "({col},{row}) should be blue");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn read_region_async_spanning_tiles() {
    let (store, path) = store_with_slide().await;
    let slide = OpenSlide::open_store_async(store, path).await.unwrap();

    // 4x2 region straddling the red/blue tile boundary.
    let pixels = slide.read_region_async(2, 1, 0, 4, 2).await.unwrap();
    assert_eq!(pixels.len(), 8);

    let red = color::argb(0xFF, 255, 0, 0);
    let blue = color::argb(0xFF, 0, 0, 255);
    for row in 0..2 {
        for col in 0..2 {
            assert_eq!(pixels[row * 4 + col], red);
        }
        for col in 2..4 {
            assert_eq!(pixels[row * 4 + col], blue);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn read_region_async_invalid_args() {
    let (store, path) = store_with_slide().await;
    let slide = OpenSlide::open_store_async(store, path).await.unwrap();

    assert!(slide.read_region_async(0, 0, 5, 1, 1).await.is_err());
    assert!(slide.read_region_async(0, 0, -1, 1, 1).await.is_err());
    assert!(
        slide
            .read_region_async(0, 0, 0, 0, 0)
            .await
            .unwrap()
            .is_empty()
    );
}

// ─────────────── the formats that fall back to the sync read ────────────────
//
// Tilemap (Ventana), NDPI, MIRAX, Zeiss, DICOM and synthetic levels have no
// range-fetch path, so `read_region_async` reads them synchronously. On an
// object-store slide every byte of that read goes through `BlockingReader`,
// whose `Runtime::block_on` panics when it runs on a thread that is already
// inside a runtime — which, inside `read_region_async`, it always is. The
// fallback therefore has to leave the runtime thread before it reads.
//
// Ventana is the cheapest of the six to build from scratch: a tiled TIFF plus
// an XMLPacket describing one scanned area is enough to produce a tilemap
// level. The AOI geometry mirrors the driver's own unit-test fixture.

const VT_TILE: u32 = 32;
const VT_TILES: u32 = 2; // 2x2 physical tiles
/// Tile joint overlaps from the XML below; the stitched level is smaller than
/// the TIFF because neighbouring tiles overlap by this much.
const VT_OVERLAP_X: u32 = 8;
const VT_OVERLAP_Y: u32 = 4;

/// Where the tile pixels start, well past the header the open path reads.
///
/// This padding is what makes the test a test. A remote reader is wrapped in
/// `ReadAhead`, which fetches at least 64 KiB per miss and grows to 4 MiB on a
/// sequential walk; in a compact fixture the open pulls the whole file into
/// that cache, the region read is served from memory, and it never reaches the
/// `BlockingReader` underneath — so it cannot panic, and proves nothing. Put
/// the tiles beyond the reach of any block the open could have cached and the
/// read has to go to the store.
const VT_TILE_DATA_AT: usize = 5 << 20;

/// Solid color per physical tile, so a decoded pixel names the tile it came from.
fn vt_tile_color(tx: u32, ty: u32) -> (u8, u8, u8) {
    (32 + (tx as u8) * 64, 32 + (ty as u8) * 64, 200)
}

fn vt_xml() -> String {
    format!(
        r#"<iScan Magnification="20" ScanRes="0.5">
  <EncodeInfo>
    <SlideStitchInfo>
      <ImageInfo AOIScanned="1" Width="{VT_TILE}" Height="{VT_TILE}" NumCols="{VT_TILES}" NumRows="{VT_TILES}" Pos-X="0" Pos-Y="0">
        <TileJointInfo Tile1="1" Tile2="2" Direction="RIGHT" OverlapX="{VT_OVERLAP_X}" OverlapY="0" Confidence="5"/>
        <TileJointInfo Tile1="2" Tile2="3" Direction="UP" OverlapX="0" OverlapY="{VT_OVERLAP_Y}" Confidence="4"/>
      </ImageInfo>
    </SlideStitchInfo>
    <AoiOrigin>
      <Aoi OriginX="0" OriginY="0"/>
    </AoiOrigin>
  </EncodeInfo>
</iScan>"#
    )
}

/// A minimal Ventana BIF: one uncompressed tiled TIFF directory carrying the
/// `level=0` ImageDescription and the AOI stitch XML the driver turns into a
/// tilemap level.
fn build_ventana_tiff() -> Vec<u8> {
    let desc = b"level=0 mag=20\0";
    let xml = vt_xml().into_bytes();
    let num_tiles = VT_TILES * VT_TILES;
    let image_side = VT_TILES * VT_TILE;
    let bytes_per_tile = (VT_TILE * VT_TILE * 3) as usize;

    let num_entries: u16 = 10;
    let ifd_end = 8 + 2 + num_entries as usize * 12 + 4;
    let desc_at = ifd_end;
    let xml_at = desc_at + desc.len();
    let offsets_at = xml_at + xml.len();
    let counts_at = offsets_at + num_tiles as usize * 4;
    let tile_data_at = VT_TILE_DATA_AT;
    assert!(counts_at < tile_data_at);

    let mut buf = Vec::new();
    buf.extend_from_slice(&[0x49, 0x49]);
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&8u32.to_le_bytes());

    // IFD, tags ascending.
    buf.extend_from_slice(&num_entries.to_le_bytes());
    write_entry(&mut buf, tags::IMAGE_WIDTH, 4, 1, image_side);
    write_entry(&mut buf, tags::IMAGE_LENGTH, 4, 1, image_side);
    write_entry(&mut buf, tags::COMPRESSION, 3, 1, 1);
    write_entry(
        &mut buf,
        tags::IMAGE_DESCRIPTION,
        2,
        desc.len() as u32,
        desc_at as u32,
    );
    write_entry(&mut buf, tags::SAMPLES_PER_PIXEL, 3, 1, 3);
    write_entry(&mut buf, tags::TILE_WIDTH, 3, 1, VT_TILE);
    write_entry(&mut buf, tags::TILE_LENGTH, 3, 1, VT_TILE);
    write_entry(
        &mut buf,
        tags::TILE_OFFSETS,
        4,
        num_tiles,
        offsets_at as u32,
    );
    write_entry(
        &mut buf,
        tags::TILE_BYTE_COUNTS,
        4,
        num_tiles,
        counts_at as u32,
    );
    write_entry(
        &mut buf,
        tags::XML_PACKET,
        1,
        xml.len() as u32,
        xml_at as u32,
    );
    buf.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    assert_eq!(buf.len(), ifd_end);

    buf.extend_from_slice(desc);
    buf.extend_from_slice(&xml);
    for i in 0..num_tiles as usize {
        buf.extend_from_slice(&((tile_data_at + i * bytes_per_tile) as u32).to_le_bytes());
    }
    for _ in 0..num_tiles {
        buf.extend_from_slice(&(bytes_per_tile as u32).to_le_bytes());
    }
    buf.resize(tile_data_at, 0); // see VT_TILE_DATA_AT

    // Tile pixels, row-major (physical tile index = ty * VT_TILES + tx).
    for ty in 0..VT_TILES {
        for tx in 0..VT_TILES {
            let (r, g, b) = vt_tile_color(tx, ty);
            for _ in 0..(VT_TILE * VT_TILE) {
                buf.extend_from_slice(&[r, g, b]);
            }
        }
    }

    buf
}

async fn ventana_slide() -> OpenSlide {
    let store = InMemory::new();
    let path = StorePath::from("slide.bif");
    store
        .put(&path, PutPayload::from(build_ventana_tiff()))
        .await
        .unwrap();
    OpenSlide::open_store_async(Arc::new(store), path)
        .await
        .expect("open synthetic Ventana slide")
}

/// Read the whole stitched level and check every quadrant came from the right
/// physical tile. The assertions are secondary: before the fallback moved off
/// the runtime thread, the call below panicked with "Cannot start a runtime
/// from within a runtime" and never returned a pixel at all.
async fn check_ventana_fallback_reads(slide: &OpenSlide) {
    // Overlapping tiles advance by less than their width, so the stitched
    // level is smaller than the 64x64 TIFF behind it.
    let w = ((VT_TILES - 1) * (VT_TILE - VT_OVERLAP_X) + VT_TILE) as i64;
    let h = ((VT_TILES - 1) * (VT_TILE - VT_OVERLAP_Y) + VT_TILE) as i64;
    assert_eq!(slide.level0_dimensions(), (w, h), "stitched level size");

    let pixels = slide.read_region_async(0, 0, 0, w, h).await.unwrap();
    assert_eq!(pixels.len() as i64, w * h);

    // Sample a point inside each physical tile's exclusive area — past the
    // overlap seam, so exactly one tile paints it.
    for (tx, ty) in [(0u32, 0u32), (1, 0), (0, 1), (1, 1)] {
        let px = if tx == 0 { 4 } else { VT_TILE as i64 + 8 };
        let py = if ty == 0 { 4 } else { VT_TILE as i64 + 8 };
        let (r, g, b) = vt_tile_color(tx, ty);
        assert_eq!(
            pixels[(py * w + px) as usize],
            color::argb(0xFF, r, g, b),
            "tile ({tx}, {ty}) at ({px}, {py})"
        );
    }
}

/// The flavour `#[tokio::main]` builds by default.
#[tokio::test(flavor = "multi_thread")]
async fn fallback_formats_read_from_a_multi_thread_runtime() {
    let slide = ventana_slide().await;
    check_ventana_fallback_reads(&slide).await;
}

/// And the one `Builder::new_current_thread` (and a bare `#[tokio::test]`)
/// builds — the flavour `block_in_place` refuses to run on, which is why the
/// fallback uses a scoped thread instead. Both must work: a library doesn't
/// get to pick its caller's runtime.
#[tokio::test(flavor = "current_thread")]
async fn fallback_formats_read_from_a_current_thread_runtime() {
    let slide = ventana_slide().await;
    check_ventana_fallback_reads(&slide).await;
}
