//! End-to-end tests for the async read path (`read_region_async`) backed by an
//! `object_store` source. A synthetic uncompressed tiled TIFF is served from an
//! in-memory store, so these exercise the full open + async tile-read pipeline
//! without touching the network or downloaded test data.

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
