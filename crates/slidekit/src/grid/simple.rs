//! Simple rectangular tile grid.
//!
//! Handles the common case where tiles are laid out in a regular grid with
//! fixed tile dimensions. Used by most TIFF-based formats.

use super::TileGrid;

/// A regular rectangular tile grid.
#[derive(Debug, Clone)]
pub struct SimpleGrid {
    /// Image width in pixels.
    pub image_w: i64,
    /// Image height in pixels.
    pub image_h: i64,
    /// Tile width in pixels.
    pub tile_w: i64,
    /// Tile height in pixels.
    pub tile_h: i64,
}

impl SimpleGrid {
    pub fn new(image_w: i64, image_h: i64, tile_w: i64, tile_h: i64) -> Self {
        Self {
            image_w,
            image_h,
            tile_w,
            tile_h,
        }
    }

    /// Number of tile columns.
    pub fn tiles_across(&self) -> i64 {
        (self.image_w + self.tile_w - 1) / self.tile_w
    }

    /// Number of tile rows.
    pub fn tiles_down(&self) -> i64 {
        (self.image_h + self.tile_h - 1) / self.tile_h
    }

    /// Convert a pixel region to the range of tile column/row indices that overlap it.
    /// Returns (col_start, col_end, row_start, row_end) where end is exclusive.
    pub fn tile_range(&self, x: f64, y: f64, w: f64, h: f64) -> (i64, i64, i64, i64) {
        let col_start = (x / self.tile_w as f64).floor() as i64;
        let col_end = ((x + w) / self.tile_w as f64).ceil() as i64;
        let row_start = (y / self.tile_h as f64).floor() as i64;
        let row_end = ((y + h) / self.tile_h as f64).ceil() as i64;

        // Clamp to valid range
        let col_start = col_start.max(0);
        let col_end = col_end.min(self.tiles_across());
        let row_start = row_start.max(0);
        let row_end = row_end.min(self.tiles_down());

        (col_start, col_end, row_start, row_end)
    }
}

impl TileGrid for SimpleGrid {
    fn tiles_for_region(&self, x: f64, y: f64, w: f64, h: f64) -> Vec<(i64, i64)> {
        let (col_start, col_end, row_start, row_end) = self.tile_range(x, y, w, h);
        let mut tiles = Vec::new();
        for row in row_start..row_end {
            for col in col_start..col_end {
                tiles.push((col, row));
            }
        }
        tiles
    }
}
