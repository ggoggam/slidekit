//! Tests for the simple tile grid.

use slidekit::grid::TileGrid;
use slidekit::grid::simple::SimpleGrid;

#[test]
fn tiles_across_and_down() {
    let grid = SimpleGrid::new(1024, 768, 256, 256);
    assert_eq!(grid.tiles_across(), 4);
    assert_eq!(grid.tiles_down(), 3);
}

#[test]
fn tiles_across_non_aligned() {
    // 1000 / 256 = 3.9, should round up to 4
    let grid = SimpleGrid::new(1000, 500, 256, 256);
    assert_eq!(grid.tiles_across(), 4);
    assert_eq!(grid.tiles_down(), 2);
}

#[test]
fn tiles_for_region_single_tile() {
    let grid = SimpleGrid::new(1024, 768, 256, 256);
    // Region fully within tile (0,0)
    let tiles = grid.tiles_for_region(10.0, 10.0, 100.0, 100.0);
    assert_eq!(tiles, vec![(0, 0)]);
}

#[test]
fn tiles_for_region_spanning_tiles() {
    let grid = SimpleGrid::new(1024, 768, 256, 256);
    // Region spanning tiles (0,0), (1,0), (0,1), (1,1)
    let tiles = grid.tiles_for_region(200.0, 200.0, 200.0, 200.0);
    assert_eq!(tiles, vec![(0, 0), (1, 0), (0, 1), (1, 1)]);
}

#[test]
fn tiles_for_region_entire_image() {
    let grid = SimpleGrid::new(512, 512, 256, 256);
    let tiles = grid.tiles_for_region(0.0, 0.0, 512.0, 512.0);
    assert_eq!(tiles, vec![(0, 0), (1, 0), (0, 1), (1, 1)]);
}

#[test]
fn tiles_for_region_clamped_to_bounds() {
    let grid = SimpleGrid::new(512, 512, 256, 256);
    // Region extending past image bounds
    let tiles = grid.tiles_for_region(400.0, 400.0, 500.0, 500.0);
    assert_eq!(tiles, vec![(1, 1)]);
}

#[test]
fn tile_range_basic() {
    let grid = SimpleGrid::new(1024, 1024, 256, 256);
    let (c0, c1, r0, r1) = grid.tile_range(0.0, 0.0, 256.0, 256.0);
    assert_eq!((c0, c1, r0, r1), (0, 1, 0, 1));
}

#[test]
fn tile_range_offset() {
    let grid = SimpleGrid::new(1024, 1024, 256, 256);
    let (c0, c1, r0, r1) = grid.tile_range(256.0, 256.0, 256.0, 256.0);
    assert_eq!((c0, c1, r0, r1), (1, 2, 1, 2));
}
