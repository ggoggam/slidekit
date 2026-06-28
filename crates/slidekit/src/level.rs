/// Information about a single level of a whole slide image.
#[derive(Debug, Clone)]
pub struct LevelInfo {
    pub width: i64,
    pub height: i64,
    pub downsample: f64,
    pub tile_width: i64,
    pub tile_height: i64,
}
