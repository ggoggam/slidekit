//! Tilemap grid: tiles placed at arbitrary pixel offsets.
//!
//! Unlike [`SimpleGrid`](super::simple::SimpleGrid) (regular row-major layout
//! where a tile's position is `col * tile_w`), a tilemap level is an explicit
//! list of placed (sub)tiles. Each one names the physical TIFF tile it comes
//! from, the sub-rectangle within that decoded tile, and its destination in the
//! level's pixel space. Built by the Ventana BIF driver from AOI stitch
//! geometry; consumed by [`OpenSlide::read_region`](crate::OpenSlide::read_region).

/// One placed (sub)tile within a level.
#[derive(Debug, Clone)]
pub struct PlacedTile {
    /// Physical TIFF tile index (row-major) within the level's directory.
    pub tiff_tile_index: usize,
    /// Sub-rectangle within the decoded TIFF tile (pixels).
    pub src_x: i64,
    pub src_y: i64,
    pub src_w: i64,
    pub src_h: i64,
    /// Destination top-left in this level's pixel space (nearest integer of the
    /// float stitch position — we composite by pixel, not subpixel resampling).
    pub dest_x: i64,
    pub dest_y: i64,
}

/// A set of placed tiles forming one pyramid level.
#[derive(Debug, Clone)]
pub struct TileMap {
    pub tiles: Vec<PlacedTile>,
    /// Level dimensions: the ceiling of the placed-tile bounding box.
    pub width: i64,
    pub height: i64,
}

impl TileMap {
    /// Build from placed tiles, deriving `width`/`height` from their extent.
    pub fn new(tiles: Vec<PlacedTile>) -> Self {
        let mut width = 0;
        let mut height = 0;
        for t in &tiles {
            width = width.max(t.dest_x + t.src_w);
            height = height.max(t.dest_y + t.src_h);
        }
        TileMap {
            tiles,
            width,
            height,
        }
    }

    /// Placed tiles whose destination rectangle overlaps the level-pixel region
    /// `(x, y, w, h)`.
    ///
    /// ponytail: linear scan — a level holds at most ~10^5 tiles, so a per-read
    /// O(n) filter is microseconds. Add a spatial index only if a real slide
    /// makes this a hot path.
    pub fn tiles_for_region(
        &self,
        x: i64,
        y: i64,
        w: i64,
        h: i64,
    ) -> impl Iterator<Item = &PlacedTile> {
        let x1 = x + w;
        let y1 = y + h;
        self.tiles.iter().filter(move |t| {
            t.dest_x < x1 && t.dest_x + t.src_w > x && t.dest_y < y1 && t.dest_y + t.src_h > y
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(dest_x: i64, dest_y: i64, w: i64, h: i64) -> PlacedTile {
        PlacedTile {
            tiff_tile_index: 0,
            src_x: 0,
            src_y: 0,
            src_w: w,
            src_h: h,
            dest_x,
            dest_y,
        }
    }

    #[test]
    fn bounds_and_region_filter() {
        let map = TileMap::new(vec![
            tile(0, 0, 10, 10),
            tile(10, 0, 10, 10),
            tile(0, 10, 10, 10),
        ]);
        assert_eq!((map.width, map.height), (20, 20));

        // A region in the top-left touches only the first tile.
        let hit: Vec<_> = map.tiles_for_region(0, 0, 5, 5).collect();
        assert_eq!(hit.len(), 1);
        assert_eq!((hit[0].dest_x, hit[0].dest_y), (0, 0));

        // A region spanning the seam touches both top tiles.
        let hit: Vec<_> = map.tiles_for_region(8, 0, 4, 4).collect();
        assert_eq!(hit.len(), 2);

        // A region past everything touches nothing.
        assert_eq!(map.tiles_for_region(100, 100, 10, 10).count(), 0);
    }
}
