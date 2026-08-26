//! Decoded-tile cache: an LRU of decoded ARGB tiles, shared by every read path.
//!
//! A tile is expensive twice over. Its bytes have to be fetched — a round trip
//! on object storage, where the benchmark measures ~2 requests per 1024×1024
//! patch at ~400 ms each — and then decoded, which is the expensive half on
//! local disk (a JPEG-2000 Aperio patch costs 64 ms). A viewer panning across a
//! slide asks for the same tiles over and over, so both costs are avoidable.
//!
//! Every read path therefore probes this cache **before** it resolves a tile's
//! byte range, not after: a region whose tiles are all cached must issue zero
//! requests, so the lookup has to precede the batching phase rather than filter
//! its results.
//!
//! # Why the key is what it is
//!
//! A cache that returns the wrong pixels is worse than no cache at all: nothing
//! downstream can tell, and the result is a wrong diagnosis on a real slide. The
//! key is conservative on all three axes it has to separate.
//!
//! **Slide.** The old key was `(level, tile_x, tile_y)` while the type's own doc
//! comment advertised sharing one cache "across multiple `OpenSlide`
//! instances" — level 0 tile (3, 4) of a Ventana and of an Aperio were the same
//! entry. Every key now carries a [`SlideId`], minted per open, so sharing is
//! genuinely safe. Two handles on the *same* file get different ids and so do
//! not share entries; that gives up some reuse deliberately, because nothing
//! about a path proves two handles see the same bytes (a file can be rewritten,
//! and a custom [`SlideFs`](crate::io::fs::SlideFs) may serve anything it likes
//! for a given path) and the failure mode of guessing wrong is another slide's
//! pixels.
//!
//! **Level.** A level index maps 1:1 onto one `LevelData`, hence onto one TIFF
//! directory / instance set / subblock set, so it is enough to scope tile
//! identity to a single pyramid level.
//!
//! **Tile.** `(tile_x, tile_y)` is *not* enough, because most of these formats
//! have no one-tile-per-grid-cell relationship:
//!
//! * Ventana subtiles are sub-rectangles of a shared physical TIFF tile — many
//!   grid positions, one decoded tile, and the thing worth caching is the
//!   physical tile (`tiff_tile_index`).
//! * MIRAX tiles are standalone blobs addressed by `(fileno, offset)` in one of
//!   several `Data####.dat` files, and several placed subtiles crop the same
//!   blob.
//! * CZI subblocks are addressed by `file_position` and *overlap* in slide
//!   space, so a grid cell does not name one of them.
//! * DICOM tiles are frames of a particular concatenation member.
//!
//! [`TileId`] therefore names a tile the way its own format does. Its variants
//! are an enum rather than a packed integer so that two formats' identifiers
//! cannot collide numerically, and each level uses exactly one variant.

use lru::LruCache;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const DEFAULT_CACHE_BYTES: usize = 32 * 1024 * 1024; // 32 MB
const BYTES_PER_PIXEL: usize = 4;

/// Charged per entry on top of its pixels, covering the LRU node, the key, the
/// `Vec` header and the `Arc` control block.
///
/// Without it a cache of pathologically small tiles would report ~0 bytes used
/// while holding an unbounded number of allocations. It is deliberately a rough
/// over-estimate; a real tile is hundreds of kilobytes, so it never matters.
const ENTRY_OVERHEAD: usize = 128;

/// Identity of one opened slide, for the lifetime of the process.
///
/// Minted by [`SlideId::next`] at open. See the module docs for why this is
/// per-*handle* rather than per-path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlideId(u64);

impl SlideId {
    /// Mint an id no other live slide holds.
    ///
    /// A `u64` counter cannot realistically wrap: at one open per nanosecond it
    /// would take 500 years, and an open is milliseconds.
    pub fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        SlideId(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// Identity of one decoded tile *within one level of one slide*.
///
/// Each pyramid level is read by exactly one path, and each path uses exactly
/// one of these variants, so the variants never mix within a level. They are
/// still an enum rather than a flat integer pair so that a stray identifier
/// from one scheme cannot be mistaken for another's — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TileId {
    /// The tile's ordinal in the level's own tile enumeration, within byte
    /// stream `stream` (`0` when the level has a single stream).
    ///
    /// Tiled TIFF and Ventana use the row-major *physical* TIFF tile index;
    /// DICOM uses the frame number within concatenation member `stream`.
    Index { stream: u64, index: u64 },
    /// The position of the tile's bytes within stream `stream`: MIRAX's
    /// `(fileno, offset)`, a CZI subblock's `file_position`.
    Offset { stream: u64, position: u64 },
    /// The tile's grid cell, for levels whose tiles have no identity outside
    /// the grid — NDPI, whose "tiles" are restart-marker spans of one giant
    /// JPEG and are only ever named by `(col, row)`.
    Cell { col: i64, row: i64 },
}

/// Key for cached tiles. Not constructible without a [`SlideId`], which is what
/// makes cross-slide sharing safe.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct TileKey {
    slide: SlideId,
    level: i32,
    tile: TileId,
}

/// Thread-safe LRU of decoded ARGB tiles, bounded by total pixel bytes.
///
/// Can be shared across multiple `OpenSlide` instances: every key carries the
/// slide's own [`SlideId`], so two slides sharing a cache compete for the same
/// byte budget but can never read each other's tiles.
///
/// Clones share one underlying cache, and reads go through
/// [`Cache::level`], which pins the slide and level a caller may address.
#[derive(Debug, Clone)]
pub struct Cache {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// Unbounded by *entry count* on purpose: `lru`'s own capacity counts
    /// entries, and the old code turned a byte budget into an entry count by
    /// assuming every tile is 256×256 ARGB. A 512² tile then overshot the
    /// stated budget 4×, a 1024² one 16×. Eviction here is driven by the real
    /// `Vec<u32>` sizes instead, tracked in `bytes`.
    map: Mutex<Entries>,
    capacity: usize,
    hits: AtomicUsize,
    misses: AtomicUsize,
}

#[derive(Debug)]
struct Entries {
    lru: LruCache<TileKey, Arc<Vec<u32>>>,
    bytes: usize,
}

/// Bytes an entry occupies: the pixel allocation as actually sized (capacity,
/// not length — a decoder may hand back a `Vec` with slack) plus fixed overhead.
fn entry_bytes(pixels: &Arc<Vec<u32>>) -> usize {
    pixels.capacity() * BYTES_PER_PIXEL + ENTRY_OVERHEAD
}

impl Cache {
    /// Create a new cache with the given capacity in bytes.
    ///
    /// A capacity of `0` disables caching: nothing is ever stored, so every read
    /// re-fetches and re-decodes. That is the reference behaviour the cache
    /// correctness tests compare against.
    pub fn new(capacity_bytes: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                map: Mutex::new(Entries {
                    lru: LruCache::unbounded(),
                    bytes: 0,
                }),
                capacity: capacity_bytes,
                hits: AtomicUsize::new(0),
                misses: AtomicUsize::new(0),
            }),
        }
    }

    /// Capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// Bytes currently held, by the same accounting the budget uses.
    pub fn used_bytes(&self) -> usize {
        self.inner.map.lock().bytes
    }

    /// Number of tiles currently held.
    pub fn len(&self) -> usize {
        self.inner.map.lock().lru.len()
    }

    /// Whether the cache holds no tiles.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lookups served from memory, since this cache was created.
    pub fn hits(&self) -> usize {
        self.inner.hits.load(Ordering::Relaxed)
    }

    /// Lookups that had to fetch and decode.
    pub fn misses(&self) -> usize {
        self.inner.misses.load(Ordering::Relaxed)
    }

    /// Drop every entry, keeping the capacity.
    pub fn clear(&self) {
        let mut entries = self.inner.map.lock();
        entries.lru.clear();
        entries.bytes = 0;
    }

    /// A handle scoped to one level of one slide.
    ///
    /// This is the only way to reach [`get`](LevelCache::get) and
    /// [`insert`](LevelCache::insert), so no read path can build a key that
    /// forgets which slide it is reading.
    pub fn level(&self, slide: SlideId, level: i32) -> LevelCache<'_> {
        LevelCache {
            cache: self,
            slide,
            level,
        }
    }

    fn get(&self, key: TileKey) -> Option<Arc<Vec<u32>>> {
        // `capacity == 0` never stores anything, so skip the lock entirely —
        // this is the disabled-cache path the correctness tests run through.
        // The miss still counts: it is the only evidence those tests have that
        // the read path consulted the cache they disabled, rather than serving
        // the tile from somewhere `set_cache` does not reach.
        if self.inner.capacity == 0 {
            self.inner.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let hit = self.inner.map.lock().lru.get(&key).cloned();
        let counter = if hit.is_some() {
            &self.inner.hits
        } else {
            &self.inner.misses
        };
        counter.fetch_add(1, Ordering::Relaxed);
        hit
    }

    fn insert(&self, key: TileKey, pixels: Arc<Vec<u32>>) {
        let cost = entry_bytes(&pixels);
        // A tile larger than the whole budget would evict everything else and
        // still leave the cache over its limit, so it is simply not cached.
        // Uncompressed CZI subblocks can genuinely be this big.
        if cost > self.inner.capacity {
            return;
        }

        let mut entries = self.inner.map.lock();
        if let Some(old) = entries.lru.put(key, pixels) {
            entries.bytes -= entry_bytes(&old);
        }
        entries.bytes += cost;

        // The entry just inserted is the MRU one, and `cost <= capacity`, so
        // this always terminates with it still present.
        while entries.bytes > self.inner.capacity {
            match entries.lru.pop_lru() {
                Some((_, evicted)) => entries.bytes -= entry_bytes(&evicted),
                None => break,
            }
        }
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_BYTES)
    }
}

/// A [`Cache`] narrowed to one level of one slide.
///
/// Cheap to copy; holds no lock between calls.
#[derive(Debug, Clone, Copy)]
pub struct LevelCache<'a> {
    cache: &'a Cache,
    slide: SlideId,
    level: i32,
}

impl LevelCache<'_> {
    /// Look up a decoded tile.
    ///
    /// Returns an `Arc`, so a hit is a refcount bump rather than a
    /// megabyte-scale memcpy. Every compositor here takes `&[u32]`, which an
    /// `Arc<Vec<u32>>` derefs to.
    pub fn get(&self, tile: TileId) -> Option<Arc<Vec<u32>>> {
        self.cache.get(self.key(tile))
    }

    /// Store a decoded tile, evicting least-recently-used tiles to stay inside
    /// the byte budget.
    pub fn insert(&self, tile: TileId, pixels: Arc<Vec<u32>>) {
        self.cache.insert(self.key(tile), pixels);
    }

    fn key(&self, tile: TileId) -> TileKey {
        TileKey {
            slide: self.slide,
            level: self.level,
            tile,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(len: usize, fill: u32) -> Arc<Vec<u32>> {
        Arc::new(vec![fill; len])
    }

    /// The budget is in bytes of pixels, not in tiles. The old implementation
    /// converted the budget to an entry count assuming 256×256, so this cache —
    /// 1 MB of 512×512 tiles — would have held 16 of them, 16× its budget.
    #[test]
    fn the_budget_is_bytes_not_entries() {
        let px = 512 * 512;
        let cache = Cache::new(4 * px * BYTES_PER_PIXEL);
        let slide = SlideId::next();
        let lvl = cache.level(slide, 0);

        for i in 0..16u64 {
            lvl.insert(
                TileId::Index {
                    stream: 0,
                    index: i,
                },
                tile(px, i as u32),
            );
        }

        assert!(
            cache.used_bytes() <= cache.capacity(),
            "{} bytes held against a {} byte budget",
            cache.used_bytes(),
            cache.capacity()
        );
        // Four tiles of pixels, minus whatever the per-entry overhead displaces.
        assert!(cache.len() <= 4, "{} entries held", cache.len());
        assert!(
            cache.len() >= 3,
            "budget under-used: {} entries",
            cache.len()
        );
    }

    /// Eviction is least-recently-*used*, so touching an old tile keeps it.
    #[test]
    fn eviction_is_lru_and_accounting_stays_exact() {
        let px = 1024;
        let cache = Cache::new(2 * (px * BYTES_PER_PIXEL + ENTRY_OVERHEAD));
        let lvl_id = SlideId::next();
        let lvl = cache.level(lvl_id, 0);
        let key = |i| TileId::Index {
            stream: 0,
            index: i,
        };

        lvl.insert(key(0), tile(px, 0));
        lvl.insert(key(1), tile(px, 1));
        assert_eq!(cache.len(), 2);
        assert_eq!(
            cache.used_bytes(),
            2 * (px * BYTES_PER_PIXEL + ENTRY_OVERHEAD)
        );

        // Touch 0 so 1 becomes the least recently used, then overflow.
        assert!(lvl.get(key(0)).is_some());
        lvl.insert(key(2), tile(px, 2));

        assert!(lvl.get(key(0)).is_some(), "recently used tile was evicted");
        assert!(lvl.get(key(1)).is_none(), "LRU tile survived");
        assert!(lvl.get(key(2)).is_some());
        assert_eq!(
            cache.used_bytes(),
            2 * (px * BYTES_PER_PIXEL + ENTRY_OVERHEAD)
        );
    }

    /// Re-inserting a key must not double-count its bytes.
    #[test]
    fn replacing_an_entry_replaces_its_cost() {
        let cache = Cache::new(1 << 20);
        let lvl = cache.level(SlideId::next(), 0);
        let key = TileId::Cell { col: 1, row: 2 };

        lvl.insert(key, tile(64, 7));
        let once = cache.used_bytes();
        lvl.insert(key, tile(64, 9));
        assert_eq!(cache.used_bytes(), once);
        assert_eq!(cache.len(), 1);
        assert_eq!(lvl.get(key).unwrap()[0], 9);
    }

    /// A tile bigger than the whole budget is skipped rather than evicting the
    /// entire cache to not fit anyway.
    #[test]
    fn an_oversized_tile_is_not_cached() {
        let cache = Cache::new(1024);
        let lvl = cache.level(SlideId::next(), 0);
        lvl.insert(
            TileId::Index {
                stream: 0,
                index: 0,
            },
            tile(64, 1),
        );
        lvl.insert(
            TileId::Index {
                stream: 0,
                index: 1,
            },
            tile(4096, 2),
        );

        assert_eq!(cache.len(), 1);
        assert!(
            lvl.get(TileId::Index {
                stream: 0,
                index: 0
            })
            .is_some()
        );
        assert!(
            lvl.get(TileId::Index {
                stream: 0,
                index: 1
            })
            .is_none()
        );
    }

    /// The whole point of the key. Same level, same tile number, different
    /// slides / levels / identifier schemes — four distinct entries.
    #[test]
    fn keys_do_not_collide_across_slides_levels_or_schemes() {
        let cache = Cache::new(1 << 20);
        let (a, b) = (SlideId::next(), SlideId::next());
        assert_ne!(a, b);

        let id = TileId::Index {
            stream: 0,
            index: 5,
        };
        cache.level(a, 0).insert(id, tile(4, 0xA));
        cache.level(b, 0).insert(id, tile(4, 0xB));
        cache.level(a, 1).insert(id, tile(4, 0xC));
        cache.level(a, 0).insert(
            TileId::Offset {
                stream: 0,
                position: 5,
            },
            tile(4, 0xD),
        );
        cache
            .level(a, 0)
            .insert(TileId::Cell { col: 5, row: 0 }, tile(4, 0xE));

        assert_eq!(cache.len(), 5);
        assert_eq!(cache.level(a, 0).get(id).unwrap()[0], 0xA);
        assert_eq!(cache.level(b, 0).get(id).unwrap()[0], 0xB);
        assert_eq!(cache.level(a, 1).get(id).unwrap()[0], 0xC);
        assert_eq!(
            cache
                .level(a, 0)
                .get(TileId::Offset {
                    stream: 0,
                    position: 5
                })
                .unwrap()[0],
            0xD
        );
    }

    /// A MIRAX blob at offset 100 of `Data0000.dat` is not the one at offset 100
    /// of `Data0001.dat`.
    #[test]
    fn the_stream_discriminates_multi_file_formats() {
        let cache = Cache::new(1 << 20);
        let lvl = cache.level(SlideId::next(), 0);
        lvl.insert(
            TileId::Offset {
                stream: 0,
                position: 100,
            },
            tile(4, 1),
        );
        lvl.insert(
            TileId::Offset {
                stream: 1,
                position: 100,
            },
            tile(4, 2),
        );
        assert_eq!(cache.len(), 2);
        assert_eq!(
            lvl.get(TileId::Offset {
                stream: 1,
                position: 100
            })
            .unwrap()[0],
            2
        );
    }

    /// A hit hands back the same allocation, not a copy of it.
    #[test]
    fn a_hit_shares_the_allocation() {
        let cache = Cache::new(1 << 20);
        let lvl = cache.level(SlideId::next(), 0);
        let px = tile(1024, 3);
        lvl.insert(TileId::Cell { col: 0, row: 0 }, px.clone());
        let got = lvl.get(TileId::Cell { col: 0, row: 0 }).unwrap();
        assert!(Arc::ptr_eq(&px, &got));
    }

    /// Capacity zero is a working "no cache", which is how the read-path tests
    /// obtain reference pixels.
    #[test]
    fn a_zero_capacity_cache_stores_nothing() {
        let cache = Cache::new(0);
        let lvl = cache.level(SlideId::next(), 0);
        lvl.insert(TileId::Cell { col: 0, row: 0 }, tile(16, 1));
        assert!(lvl.get(TileId::Cell { col: 0, row: 0 }).is_none());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.used_bytes(), 0);
    }

    /// Clones share one cache; that is what makes `Cache` shareable between
    /// slides at all.
    #[test]
    fn clones_share_one_cache() {
        let cache = Cache::new(1 << 20);
        let clone = cache.clone();
        let slide = SlideId::next();
        cache.level(slide, 0).insert(
            TileId::Index {
                stream: 0,
                index: 0,
            },
            tile(16, 42),
        );
        assert_eq!(
            clone
                .level(slide, 0)
                .get(TileId::Index {
                    stream: 0,
                    index: 0
                })
                .unwrap()[0],
            42
        );
        assert_eq!(clone.len(), 1);
    }
}
