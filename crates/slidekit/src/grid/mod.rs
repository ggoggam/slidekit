pub mod simple;
pub mod tilemap;

/// Trait for tile grid layouts.
///
/// Different slide formats arrange tiles differently:
/// - Simple: regular rectangular grid (most TIFF-based)
/// - Tilemap: arbitrary float-offset tiles (Mirax)
/// - Range: Z-ordered or quadtree tiles
pub trait TileGrid: Send + Sync {
    /// Get the tile coordinates that overlap the given pixel region.
    fn tiles_for_region(&self, x: f64, y: f64, w: f64, h: f64) -> Vec<(i64, i64)>;
}
