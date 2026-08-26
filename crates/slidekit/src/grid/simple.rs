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
    /// Horizontal distance between the origins of adjacent tiles.
    ///
    /// For a non-overlapping grid this is the tile width. For an overlapping
    /// one (Trestle, MIRAX) it is the *advance*, `tile_width - overlap`, and
    /// [`span_w`](Self::span_w) carries the real tile width.
    pub tile_w: i64,
    /// Vertical distance between the origins of adjacent tiles. See
    /// [`tile_w`](Self::tile_w).
    pub tile_h: i64,
    /// Real decoded tile width, which is `tile_w` unless tiles overlap.
    ///
    /// The tile count depends on both: `n` tiles reach
    /// `(n - 1) * tile_w + span_w`, not `n * tile_w`. Deriving the count from
    /// the advance alone overcounts an overlapping grid by a column, and the
    /// phantom column indexes a tile the file does not have — see
    /// [`tiles_across`](Self::tiles_across).
    pub span_w: i64,
    /// Real decoded tile height. See [`span_w`](Self::span_w).
    pub span_h: i64,
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
            span_w: tile_w,
            span_h: tile_h,
            origin_x: 0,
            origin_y: 0,
        }
    }

    /// Declare that the decoded tiles are larger than the grid advance, i.e.
    /// that adjacent tiles overlap by `span - advance` pixels.
    ///
    /// Without this the grid assumes tiles butt up against each other, and its
    /// tile count comes out one column and one row too large — indices the
    /// underlying file has no tile for.
    pub fn with_tile_span(mut self, span_w: i64, span_h: i64) -> Self {
        self.span_w = span_w.max(self.tile_w);
        self.span_h = span_h.max(self.tile_h);
        self
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
    ///
    /// `n` tiles laid down at `tile_w` intervals reach
    /// `(n - 1) * tile_w + span_w`, so the count is the smallest `n` that
    /// covers the image at that reach — *not* `ceil(image_w / tile_w)`, which
    /// is right only while `span_w == tile_w`. On an overlapping grid the
    /// simpler form returns one column too many, and `read_region` turns that
    /// phantom column into `row * tiles_across + col` for a tile the file does
    /// not have: the wrong tile painted at the right edge, or a hard
    /// "tile index out of range" that fails the whole read.
    pub fn tiles_across(&self) -> i64 {
        tile_count(self.image_w, self.tile_w, self.span_w)
    }

    /// Number of tile rows. See [`tiles_across`](Self::tiles_across).
    pub fn tiles_down(&self) -> i64 {
        tile_count(self.image_h, self.tile_h, self.span_h)
    }

    /// Convert a level-pixel region to the range of tile column/row indices that
    /// overlap it. Returns (col_start, col_end, row_start, row_end) where end is
    /// exclusive.
    pub fn tile_range(&self, x: f64, y: f64, w: f64, h: f64) -> (i64, i64, i64, i64) {
        // Into image space: the image may sit at an offset within the level.
        let x = x - self.origin_x as f64;
        let y = y - self.origin_y as f64;

        // Tile `i` spans `[i * advance, i * advance + span)`, so the first tile
        // reaching `x` is the smallest `i` with `i * advance > x - span`. With
        // `span == advance` that is exactly `floor(x / advance)`; where tiles
        // overlap it is one column further left, and taking `floor(x / advance)`
        // there would step past the last real tile and drop the region's right
        // edge entirely.
        let col_start = first_tile(x, self.tile_w, self.span_w);
        let col_end = ((x + w) / self.tile_w as f64).ceil() as i64;
        let row_start = first_tile(y, self.tile_h, self.span_h);
        let row_end = ((y + h) / self.tile_h as f64).ceil() as i64;

        // Clamp to valid range
        let col_start = col_start.max(0);
        let col_end = col_end.min(self.tiles_across());
        let row_start = row_start.max(0);
        let row_end = row_end.min(self.tiles_down());

        (col_start, col_end, row_start, row_end)
    }
}

/// How many tiles of real width `span`, laid down every `advance` pixels, it
/// takes to cover `image` pixels.
///
/// `advance` comes from the slide file, so zero is reachable on a corrupt or
/// hostile one; dividing by it would abort the process. Reporting no tiles
/// instead leaves the level unreadable but keeps the failure a `Result` — the
/// drivers reject a zero tile size at open, and this is the backstop.
fn tile_count(image: i64, advance: i64, span: i64) -> i64 {
    if image <= 0 || advance <= 0 {
        return 0;
    }
    if image <= span {
        return 1;
    }
    (image - span + advance - 1) / advance + 1
}

/// Index of the first tile whose span reaches `x`, in image space.
///
/// Tile `i` covers `[i * advance, i * advance + span)`, so this is the smallest
/// `i` with `x < i * advance + span`. May be negative; callers clamp.
fn first_tile(x: f64, advance: i64, span: i64) -> i64 {
    if advance <= 0 {
        return 0;
    }
    ((x - span as f64) / advance as f64).floor() as i64 + 1
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

    /// A Trestle-shaped level: 1024x1024 of 256px tiles overlapping by 32, so
    /// the file holds 4x4 tiles and the level measures 928x928.
    ///
    /// Counting columns as `ceil(928 / 224)` says 5, and `read_region` turns
    /// column 4 into `row * 4 + 4` — tile 4 is row 1's first tile, and row 3
    /// asks for index 16, which the file does not have. The count has to come
    /// from the tiles' real reach, not the advance alone.
    #[test]
    fn an_overlapping_grid_has_as_many_tiles_as_the_file() {
        let grid = SimpleGrid::new(928, 928, 224, 224).with_tile_span(256, 256);
        assert_eq!(grid.tiles_across(), 4);
        assert_eq!(grid.tiles_down(), 4);

        // The whole level: 16 tiles, none of them past the file's last one.
        let tiles = grid.tiles_for_region(0.0, 0.0, 928.0, 928.0);
        assert_eq!(tiles.len(), 16);
        assert!(tiles.iter().all(|&(c, r)| c < 4 && r < 4));

        // The right edge lives in the last column, not in a phantom fifth one.
        // Tile 3 spans 672..928, so it is the one that has these pixels.
        assert_eq!(grid.tiles_for_region(900.0, 0.0, 28.0, 10.0), [(3, 0)]);

        // Past the image there is still nothing.
        assert_eq!(grid.tiles_for_region(1200.0, 0.0, 28.0, 10.0).len(), 0);
    }

    /// The span defaults to the advance, so every non-overlapping caller counts
    /// tiles exactly as it did before the span existed.
    #[test]
    fn a_butting_grid_counts_tiles_by_plain_ceiling_division() {
        for (image, tile, expected) in
            [(1024, 256, 4), (1000, 256, 4), (255, 256, 1), (256, 256, 1)]
        {
            let grid = SimpleGrid::new(image, image, tile, tile);
            assert_eq!(grid.tiles_across(), expected, "image {image} tile {tile}");
            assert_eq!(grid.tiles_down(), expected, "image {image} tile {tile}");
        }
    }

    /// A tile size of zero comes straight off the file. Dividing by it would
    /// abort the process, so the grid reports no tiles and the read comes back
    /// empty instead.
    #[test]
    fn a_zero_tile_size_yields_no_tiles_rather_than_a_panic() {
        let grid = SimpleGrid::new(1024, 1024, 0, 0);
        assert_eq!(grid.tiles_across(), 0);
        assert_eq!(grid.tiles_down(), 0);
        assert_eq!(grid.tiles_for_region(0.0, 0.0, 1024.0, 1024.0).len(), 0);
    }
}
