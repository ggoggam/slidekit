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
    /// Where tile `(0, 0)` sits in the *level's* pixel space.
    ///
    /// Usually `(0, 0)`: the image and the level are the same thing. Leica SCN
    /// is the exception — a level there is the whole glass-slide canvas the
    /// collection describes, with the scanned region placed inside it at an
    /// offset (`openslide.bounds-x`/`-y`), so that slidekit and OpenSlide agree
    /// on what a coordinate means. Region coordinates are translated by this
    /// before tiles are looked up, and everything outside the image stays
    /// transparent.
    pub origin_x: i64,
    pub origin_y: i64,
}

impl SimpleGrid {
    pub fn new(image_w: i64, image_h: i64, tile_w: i64, tile_h: i64) -> Self {
        Self {
            image_w,
            image_h,
            tile_w,
            tile_h,
            origin_x: 0,
            origin_y: 0,
        }
    }

    /// Place the image at `(origin_x, origin_y)` within a larger level canvas.
    pub fn at_origin(mut self, origin_x: i64, origin_y: i64) -> Self {
        self.origin_x = origin_x;
        self.origin_y = origin_y;
        self
    }

    /// Top-left of tile `(col, row)` in level pixel space.
    pub fn tile_position(&self, col: i64, row: i64) -> (i64, i64) {
        (
            self.origin_x + col * self.tile_w,
            self.origin_y + row * self.tile_h,
        )
    }

    /// The part of tile `(col, row)` that is actually image, as
    /// `(x, y, w, h)` in level pixel space.
    ///
    /// `tile_w`/`tile_h` are the decoded tile's real dimensions, which can exceed
    /// the grid advance (overlapping tiles) and, at the right and bottom edges,
    /// run past the image: a 36832-pixel-wide image in 512-pixel tiles ends 32
    /// pixels into its last column, and those 32 pixels are encoder padding.
    /// Painting them shows up as a band of black beyond the image — visible
    /// whenever the level is larger than the image, which is exactly the Leica
    /// canvas case.
    pub fn tile_extent(
        &self,
        col: i64,
        row: i64,
        tile_w: i64,
        tile_h: i64,
    ) -> (i64, i64, i64, i64) {
        let (x, y) = self.tile_position(col, row);
        let w = tile_w.min(self.origin_x + self.image_w - x).max(0);
        let h = tile_h.min(self.origin_y + self.image_h - y).max(0);
        (x, y, w, h)
    }

    /// Number of tile columns.
    pub fn tiles_across(&self) -> i64 {
        (self.image_w + self.tile_w - 1) / self.tile_w
    }

    /// Number of tile rows.
    pub fn tiles_down(&self) -> i64 {
        (self.image_h + self.tile_h - 1) / self.tile_h
    }

    /// Convert a level-pixel region to the range of tile column/row indices that
    /// overlap it. Returns (col_start, col_end, row_start, row_end) where end is
    /// exclusive.
    pub fn tile_range(&self, x: f64, y: f64, w: f64, h: f64) -> (i64, i64, i64, i64) {
        // Into image space: the image may sit at an offset within the level.
        let x = x - self.origin_x as f64;
        let y = y - self.origin_y as f64;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_range_ignores_the_offset_when_there_is_none() {
        let grid = SimpleGrid::new(1000, 800, 256, 256);
        assert_eq!(grid.tile_position(2, 3), (512, 768));
        assert_eq!(grid.tile_range(0.0, 0.0, 300.0, 300.0), (0, 2, 0, 2));
    }

    /// A Leica-shaped level: the image sits inside a larger canvas, so a region
    /// addressed in canvas coordinates has to be translated before tile lookup,
    /// and tiles land back at the offset.
    #[test]
    fn an_offset_image_translates_regions_and_tiles() {
        let grid = SimpleGrid::new(1000, 800, 256, 256).at_origin(10_000, 35_000);

        // The canvas origin is outside the image: no tiles.
        assert_eq!(grid.tiles_for_region(0.0, 0.0, 512.0, 512.0).len(), 0);

        // The image origin is tile (0, 0), and it is placed back at the offset.
        assert_eq!(
            grid.tiles_for_region(10_000.0, 35_000.0, 10.0, 10.0),
            [(0, 0)]
        );
        assert_eq!(grid.tile_position(0, 0), (10_000, 35_000));
        assert_eq!(grid.tile_position(1, 1), (10_256, 35_256));

        // A region straddling the image's left edge picks up only the first column.
        assert_eq!(
            grid.tiles_for_region(9_900.0, 35_000.0, 200.0, 10.0),
            [(0, 0)]
        );
    }

    /// The last tile in a row or column runs past the image; only the image part
    /// of it may be painted, or encoder padding shows as a band beyond the edge.
    #[test]
    fn edge_tiles_are_clipped_to_the_image() {
        // 1000 = 3*256 + 232, so the last column has 232 real pixels of 256.
        let grid = SimpleGrid::new(1000, 800, 256, 256);
        assert_eq!(grid.tile_extent(0, 0, 256, 256), (0, 0, 256, 256));
        assert_eq!(grid.tile_extent(3, 0, 256, 256), (768, 0, 232, 256));
        // 800 = 3*256 + 32 vertically.
        assert_eq!(grid.tile_extent(0, 3, 256, 256), (0, 768, 256, 32));

        // Same, offset onto a canvas: the clip is against the image, not the level.
        let grid = grid.at_origin(10_000, 35_000);
        assert_eq!(grid.tile_extent(3, 3, 256, 256), (10_768, 35_768, 232, 32));

        // Tiles wider than the grid advance (overlapping tilings) still clip at
        // the image edge rather than to the advance.
        let overlap = SimpleGrid::new(1000, 800, 240, 240);
        assert_eq!(overlap.tile_extent(0, 0, 256, 256), (0, 0, 256, 256));
        assert_eq!(overlap.tile_extent(4, 0, 256, 256), (960, 0, 40, 256));
    }
}
