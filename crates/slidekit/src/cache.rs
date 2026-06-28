use lru::LruCache;
use parking_lot::Mutex;
use std::num::NonZeroUsize;
use std::sync::Arc;

/// Key for cached tiles.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct TileKey {
    level: i32,
    tile_x: i64,
    tile_y: i64,
}

/// Thread-safe LRU tile cache.
///
/// Can be shared across multiple `OpenSlide` instances.
#[derive(Debug, Clone)]
pub struct Cache {
    inner: Arc<Mutex<LruCache<TileKey, Vec<u32>>>>,
    capacity: usize,
}

const DEFAULT_CACHE_BYTES: usize = 32 * 1024 * 1024; // 32 MB
const BYTES_PER_PIXEL: usize = 4;

impl Cache {
    /// Create a new cache with the given capacity in bytes.
    pub fn new(capacity_bytes: usize) -> Self {
        // Estimate tiles as 256x256 ARGB
        let tile_size = 256 * 256 * BYTES_PER_PIXEL;
        let num_tiles = (capacity_bytes / tile_size).max(1);
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(num_tiles).unwrap(),
            ))),
            capacity: capacity_bytes,
        }
    }

    /// Capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Look up a cached tile, returning a clone of its pixels if present.
    pub fn get(&self, level: i32, tile_x: i64, tile_y: i64) -> Option<Vec<u32>> {
        let key = TileKey {
            level,
            tile_x,
            tile_y,
        };
        self.inner.lock().get(&key).cloned()
    }

    /// Insert a decoded tile into the cache.
    pub fn insert(&self, level: i32, tile_x: i64, tile_y: i64, pixels: Vec<u32>) {
        let key = TileKey {
            level,
            tile_x,
            tile_y,
        };
        self.inner.lock().put(key, pixels);
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_BYTES)
    }
}
