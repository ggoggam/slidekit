use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::associated::AssociatedImage;
use crate::cache::{Cache, LevelCache, SlideId, TileId};
use crate::decode::tiff::TiffLike;
use crate::error::{OpenSlideError, Result};
use crate::grid::TileGrid;
use crate::grid::tilemap::{PlacedTile, TileMap};
use crate::icc::IccProfile;
use crate::io::fs::{Located, Shared, SlideFs, SlideLocation};
use crate::io::local::{LocalFileReader, LocalFs};
use crate::io::{AsyncReadAt, SlideReader};
use crate::properties::Properties;
use crate::vendor::hamamatsu::NdpiLevel;
use crate::vendor::{self, LevelData};

/// One tile a region read is going to composite, in the order the grid listed
/// it: its extent `(x, y, w, h)` in level pixel space, and its decoded pixels
/// when the tile cache already held them.
///
/// `None` means "take the next tile out of the fetched batch". Keeping both
/// kinds in one ordered list is what lets cached and freshly-read tiles paint
/// in exactly the order an uncached read would have used — which matters
/// wherever tiles overlap.
type TilePlan = ((i64, i64, i64, i64), Option<Arc<Vec<u32>>>);

/// Main entry point for reading whole slide images.
pub struct OpenSlide {
    levels: Vec<LevelData>,
    properties: Properties,
    associated_images: Vec<(String, AssociatedImage)>,
    icc_profile: IccProfile,
    cache: Cache,
    /// This handle's identity within the tile cache. Part of every cache key,
    /// so a [`Cache`] shared between slides can never hand one slide's tiles to
    /// another. Minted per open and kept across [`set_cache`](Self::set_cache).
    id: SlideId,
    /// TIFF container for reading tile data. Behind a mutex because
    /// TiffLike's deferred loading needs &mut self.
    tiff: Option<Mutex<TiffLike>>,
    /// Async backend for `read_region_async`. Present only when the slide was
    /// opened from an async source (object_store); `None` for local files,
    /// in which case `read_region_async` falls back to the synchronous path.
    async_reader: Option<Arc<dyn AsyncReadAt>>,
}

impl OpenSlide {
    /// Open a whole slide image from a local file path.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        // Synthetic self-test slide: no backing file. Gated exactly like the C
        // driver (empty filename + `synthetic` debug flag) so it never triggers
        // for real input.
        if path.as_os_str().is_empty() {
            if !vendor::synthetic::debug_enabled() {
                return Err(OpenSlideError::UnsupportedFormat {
                    path: path.to_path_buf(),
                });
            }
            return Ok(Self::from_slide_data(vendor::synthetic::open()?));
        }

        if !path.exists() {
            return Err(OpenSlideError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("file not found: {}", path.display()),
            )));
        }

        // The detection reader is consumed by the matched driver; TIFF parsing
        // needs its own independent reader.
        Self::open_with_readers(
            Box::new(LocalFileReader::open(path)?),
            Box::new(LocalFileReader::open(path)?),
            path.to_path_buf(),
            local_location(path),
        )
    }

    /// Open a whole slide image from any [`SlideFs`] implementation.
    ///
    /// This is the extension point for storage this crate doesn't know about:
    /// implement [`SlideFs`] over your own backend and multi-file formats
    /// (MIRAX, DICOM) resolve their sibling files through it like any other.
    /// `path` is the slide's primary file within `fs`.
    ///
    /// Slides opened this way read synchronously; `read_region_async` falls
    /// back to the blocking path. Use [`open_url`](Self::open_url) or
    /// [`open_store`](Self::open_store) for a genuinely async remote backend.
    pub fn open_fs(fs: Arc<dyn SlideFs>, path: &str) -> Result<Self> {
        let location = SlideLocation::new(fs.clone(), path);
        Self::open_with_readers(
            reader_to_box(fs.open(path)?),
            reader_to_box(fs.open(path)?),
            std::path::PathBuf::from(path),
            Some(location),
        )
    }

    /// Open a whole slide image from a URL backed by the `object_store` crate.
    ///
    /// Supported schemes: `file://`, `s3://`, `gs://`, `az://`, and
    /// `http(s)://`. Remote slides are read with byte-range requests, so the
    /// whole file is never downloaded. Credentials, region, and endpoint come
    /// from the `AWS_*` / `AZURE_*` / `GOOGLE_*` environment variables — the
    /// same ones each `object_store` builder's `from_env()` reads; see
    /// [`env_config`](crate::io::object_store::env_config). Pass
    /// [`open_store`](Self::open_store) a store you built yourself for anything
    /// the environment can't express.
    ///
    /// This is the synchronous entry point; the underlying reads use the async
    /// [`ObjectStoreReader`](crate::io::object_store::ObjectStoreReader) driven
    /// to completion on a shared runtime. Do not call it from within an async
    /// context — build the reader yourself and use the async I/O directly.
    #[cfg(feature = "object-store")]
    pub fn open_url(url: &str) -> Result<Self> {
        use crate::io::object_store::ObjectStoreFs;
        let (store, path) = ObjectStoreFs::parse_url(url)?;
        Self::open_store(store, path)
    }

    /// Open a whole slide image from an [`object_store::ObjectStore`] handle and
    /// an object path. Use this to supply a pre-configured store (custom
    /// credentials, endpoints, retry policy, etc.).
    #[cfg(feature = "object-store")]
    pub fn open_store(
        store: std::sync::Arc<dyn object_store::ObjectStore>,
        path: object_store::path::Path,
    ) -> Result<Self> {
        use crate::io::object_store::{ObjectStoreFs, ObjectStoreReader};
        let source = std::path::PathBuf::from(path.as_ref());
        // The slide's filesystem is the same store it was read from, so a
        // multi-file format resolves its siblings as neighbouring keys.
        let fs: Arc<dyn SlideFs> = Arc::new(ObjectStoreFs::new(store.clone()));
        let location = SlideLocation::new(fs, path.as_ref());
        Self::from_async_reader(ObjectStoreReader::new(store, path)?, source, location)
    }

    /// Async variant of [`open_url`](Self::open_url).
    ///
    /// Safe to call from inside an async runtime: the (synchronous) parse runs
    /// on a dedicated thread so it never blocks or nests the caller's runtime.
    #[cfg(feature = "object-store")]
    pub async fn open_url_async(url: &str) -> Result<Self> {
        let url = url.to_string();
        run_open_off_runtime(move || Self::open_url(&url)).await
    }

    /// Async variant of [`open_store`](Self::open_store).
    ///
    /// Safe to call from inside an async runtime; see
    /// [`open_url_async`](Self::open_url_async).
    #[cfg(feature = "object-store")]
    pub async fn open_store_async(
        store: std::sync::Arc<dyn object_store::ObjectStore>,
        path: object_store::path::Path,
    ) -> Result<Self> {
        run_open_off_runtime(move || Self::open_store(store, path)).await
    }

    /// Build a slide from a cloneable [`AsyncSlideReader`], wrapping it in a
    /// [`BlockingReader`](crate::io::object_store::BlockingReader) so the
    /// synchronous open pipeline can drive its async reads. The same reader is
    /// also retained as the async backend for `read_region_async`. The single
    /// read implementation lives on the async reader; nothing is duplicated.
    ///
    /// The synchronous side additionally goes through
    /// [`ReadAhead`](crate::io::readahead::ReadAhead), which is what keeps a
    /// remote open from costing one ranged GET per parsed field. Detection and
    /// TIFF parsing get *clones* of one wrapper, so they share its cache instead
    /// of each refetching the file header. The async backend stays unwrapped:
    /// `read_region_async` fetches whole tiles, which need no coalescing.
    #[cfg(feature = "object-store")]
    fn from_async_reader<R>(
        reader: R,
        source: std::path::PathBuf,
        location: SlideLocation,
    ) -> Result<Self>
    where
        R: crate::io::AsyncSlideReader + Clone + 'static,
    {
        use crate::io::object_store::BlockingReader;
        use crate::io::readahead::ReadAhead;
        let async_reader: Arc<dyn AsyncReadAt> = Arc::new(reader.clone());
        let buffered = ReadAhead::new(BlockingReader::new(reader))?;
        let mut slide = Self::open_with_readers(
            Box::new(buffered.clone()),
            Box::new(buffered),
            source,
            Some(location),
        )?;

        // Pre-load tile offset tables now (off any async runtime) so that
        // `read_region_async` resolves tile locations from memory without the
        // blocking reads that deferred tag loading would otherwise trigger.
        if let Some(tiff) = slide.tiff.as_ref() {
            let mut tiff = tiff.lock();
            for lvl in &slide.levels {
                tiff.preload_tile_tables(lvl.tiff_dir)?;
            }
        }

        slide.async_reader = Some(async_reader);
        Ok(slide)
    }

    /// Shared open path: run TIFF parsing + vendor detection over the given
    /// readers and build the slide. `source` is used only for error reporting.
    fn open_with_readers(
        detect_reader: Box<dyn SlideReader>,
        tiff_reader: Box<dyn SlideReader>,
        source: std::path::PathBuf,
        location: Option<SlideLocation>,
    ) -> Result<Self> {
        // Only the reader handed to the driver carries the location: it is the
        // slide's identity, not a property of every byte stream we open.
        let detect_reader: Box<dyn SlideReader> = match location {
            Some(loc) => Box::new(Located::new(detect_reader, loc)),
            None => detect_reader,
        };

        // Try to parse as TIFF (most formats are TIFF-based)
        let mut tl = TiffLike::try_parse(tiff_reader).ok();

        // Run vendor detection
        let drivers = vendor::all_drivers();
        let matched = vendor::detect_format(&drivers, &*detect_reader, &mut tl);

        match matched {
            Some(idx) => {
                let driver = &drivers[idx];
                Ok(Self::from_slide_data(driver.open(detect_reader, tl)?))
            }
            None => Err(OpenSlideError::UnsupportedFormat { path: source }),
        }
    }

    /// Build a slide from parsed [`vendor::SlideData`]. Shared by the file-based
    /// open pipeline and the file-less synthetic entry point.
    fn from_slide_data(data: vendor::SlideData) -> Self {
        Self {
            levels: data.levels,
            properties: data.properties,
            associated_images: data.associated_images,
            icc_profile: data.icc_profile,
            cache: Cache::default(),
            id: SlideId::next(),
            tiff: data.tiff.map(Mutex::new),
            async_reader: None,
        }
    }

    /// Detect the vendor for a slide file without fully opening it.
    pub fn detect_vendor(path: impl AsRef<Path>) -> Option<String> {
        let path = path.as_ref();
        // Carries a location so the multi-file drivers can run their sibling
        // checks; without it a `.mrxs` file would report as unrecognised.
        let reader: Box<dyn SlideReader> = {
            let plain: Box<dyn SlideReader> = Box::new(LocalFileReader::open(path).ok()?);
            match local_location(path) {
                Some(loc) => Box::new(Located::new(plain, loc)),
                None => plain,
            }
        };

        // Try to parse as TIFF
        let mut tl = TiffLike::try_parse(Box::new(LocalFileReader::open(path).ok()?)).ok();

        let drivers = vendor::all_drivers();
        let idx = vendor::detect_format(&drivers, &*reader, &mut tl)?;
        Some(drivers[idx].vendor().to_string())
    }

    /// Number of levels in the slide.
    pub fn level_count(&self) -> i32 {
        self.levels.len() as i32
    }

    /// Dimensions of level 0 (the highest resolution level).
    pub fn level0_dimensions(&self) -> (i64, i64) {
        self.levels
            .first()
            .map(|l| (l.info.width, l.info.height))
            .unwrap_or((0, 0))
    }

    /// Dimensions of the given level.
    pub fn level_dimensions(&self, level: i32) -> Option<(i64, i64)> {
        self.levels
            .get(level as usize)
            .map(|l| (l.info.width, l.info.height))
    }

    /// Downsample factor for the given level.
    pub fn level_downsample(&self, level: i32) -> Option<f64> {
        self.levels.get(level as usize).map(|l| l.info.downsample)
    }

    /// Find the best level to use for the given downsample factor.
    pub fn best_level_for_downsample(&self, downsample: f64) -> i32 {
        let mut best = 0i32;
        for (i, level) in self.levels.iter().enumerate() {
            if level.info.downsample > downsample {
                break;
            }
            best = i as i32;
        }
        best
    }

    /// Read a region of the slide at the given level into a new buffer.
    ///
    /// Coordinates (x, y) are in level-0 pixel space. The region is read at
    /// the specified level. Returns ARGB pixel data as `Vec<u32>` of length `w * h`.
    pub fn read_region(&self, x: i64, y: i64, level: i32, w: i64, h: i64) -> Result<Vec<u32>> {
        if level < 0 || level >= self.level_count() {
            return Err(OpenSlideError::InvalidLevel { level });
        }
        if w <= 0 || h <= 0 {
            return Ok(vec![]);
        }

        let lvl = &self.levels[level as usize];
        let ds = lvl.info.downsample;

        // Convert level-0 coordinates to this level's pixel space
        let lx = x as f64 / ds;
        let ly = y as f64 / ds;

        // Every path below reads decoded tiles through this. It is scoped to
        // (this slide, this level) so no path can key a tile by grid position
        // alone and collide with another slide's; see `cache.rs`.
        let cache = self.level_cache(level);

        // Tilemap levels (Ventana BIF AOIs) don't lie on a regular grid.
        if let Some(tilemap) = &lvl.tilemap {
            return self.read_region_tilemap(tilemap, lvl, cache, lx, ly, w, h);
        }

        // NDPI levels have no TIFF tiles — each tile is reconstructed from the
        // level JPEG's restart markers.
        if let Some(ndpi) = &lvl.ndpi {
            return self.read_region_ndpi(ndpi, lvl, cache, lx, ly, w, h);
        }

        // MIRAX levels have no TIFF either — tiles are standalone blobs in
        // sibling `Data####.dat` files, placed by the MiraxLevel's own index.
        if let Some(mirax) = &lvl.mirax {
            return mirax.read_region(
                &*lvl.decoder,
                lx.round() as i64,
                ly.round() as i64,
                w,
                h,
                cache,
            );
        }

        // Zeiss CZI levels are likewise non-TIFF: each tile is a subblock in the
        // single container file, decoded per its own compression/pixel type.
        if let Some(zeiss) = &lvl.zeiss {
            return zeiss.read_region(lx.round() as i64, ly.round() as i64, w, h, cache);
        }

        // DICOM levels are frames of one (or a concatenation of) SOP instances,
        // each located and decoded by the level itself.
        if let Some(dicom) = &lvl.dicom {
            return dicom.read_region(lx.round() as i64, ly.round() as i64, w, h, cache);
        }

        // Synthetic levels are one prebuilt image; crop the requested rect out
        // of it (transparent fill outside), reusing the subtile compositor.
        if let Some(img) = &lvl.synthetic {
            let mut out = vec![0u32; (w * h) as usize];
            composite_subtile(
                img,
                lvl.info.width,
                0,
                0,
                lvl.info.width,
                lvl.info.height,
                0,
                0,
                &mut out,
                lx.round() as i64,
                ly.round() as i64,
                w,
                h,
            );
            return Ok(out);
        }

        // Find overlapping tiles
        let tiles = lvl.grid.tiles_for_region(lx, ly, w as f64, h as f64);

        // Initialize output buffer with transparent black
        let out_len = (w * h) as usize;
        let mut output = vec![0u32; out_len];

        // Need a TiffLike to read tile data
        let tiff_mutex = self.tiff.as_ref().ok_or_else(|| {
            OpenSlideError::Decode("No TIFF data available for tile reading".into())
        })?;

        // Actual TIFF tile dimensions for decoding. The grid's own tile advance
        // (which may differ, due to overlaps) is applied by `tile_position`.
        let tiff_tile_w = lvl.tiff_tile_width;
        let tiff_tile_h = lvl.tiff_tile_height;
        let tiles_across = tiff_tiles_across(tiff_mutex, lvl.tiff_dir, tiff_tile_w)?;

        // Resolve every tile's byte range first, then fetch them in one call, so
        // a remote read costs a handful of coalesced requests instead of one per
        // tile. Reading them one at a time made a 1024x1024 region 10-25
        // sequential round trips — the same mistake, at read time, that the TIFF
        // directory parse made at open. Same two phases as `read_region_async`.
        //
        // Phase 0 comes first: a tile already in the cache never enters the
        // batch. That ordering is the point — filtering *after* the gather would
        // still pay the round trip a fully-cached region is supposed to avoid.
        //
        // `plan` holds the tiles in the grid's original order, which is
        // load-bearing: for overlap formats (Trestle) the grid advance is
        // smaller than a tile, so tiles genuinely overlap and the last one
        // painted wins. A `None` entry means "take the next tile out of the
        // fetched batch", so cached and fetched tiles composite interleaved,
        // in exactly the order an uncached read would have used.
        let (plan, miss_ids, tiles_data): (Vec<TilePlan>, Vec<TileId>, Vec<bytes::Bytes>) = {
            let mut tiff = tiff_mutex.lock();
            let mut plan = Vec::with_capacity(tiles.len());
            let mut miss_ids = Vec::with_capacity(tiles.len());
            let mut ranges = Vec::with_capacity(tiles.len());
            for (col, row) in tiles {
                let tile_index = (row * tiles_across + col) as usize;
                // Where the tile goes, and how much of it is image rather than
                // encoder padding past the right/bottom edge.
                let extent = lvl.grid.tile_extent(col, row, tiff_tile_w, tiff_tile_h);
                // Row-major *physical* TIFF tile index, not the grid position:
                // `tiles_across` is the real TIFF column count, so overlap
                // formats key on the tile they actually decode.
                let id = TileId::Index {
                    stream: 0,
                    index: tile_index as u64,
                };
                if let Some(px) = cache.get(id) {
                    plan.push((extent, Some(px)));
                    continue;
                }
                // A missing tile (offset or byte count of 0) is a hole; leaving
                // it out of the batch leaves it transparent.
                if let Some((offset, size)) = tiff.tile_location(lvl.tiff_dir, tile_index)? {
                    plan.push((extent, None));
                    miss_ids.push(id);
                    ranges.push(offset..offset + size as u64);
                }
            }
            // Skip the gather entirely when the cache served everything; an
            // empty batch is a request some backends would still issue.
            let data = if ranges.is_empty() {
                Vec::new()
            } else {
                tiff.reader().read_ranges(&ranges)?
            };
            (plan, miss_ids, data)
        };

        let mut fetched = tiles_data.into_iter().zip(miss_ids);
        for ((tile_x, tile_y, src_w, src_h), cached) in plan {
            let pixels = match cached {
                Some(px) => px,
                None => {
                    let Some((tile_data, id)) = fetched.next() else {
                        continue;
                    };
                    if tile_data.is_empty() {
                        continue;
                    }
                    // Decode at actual TIFF tile dimensions.
                    let decoded = Arc::new(lvl.decoder.decode(
                        &tile_data,
                        tiff_tile_w as u32,
                        tiff_tile_h as u32,
                    )?);
                    cache.insert(id, decoded.clone());
                    decoded
                }
            };

            // The decoded buffer is a whole TIFF tile, so that is its row stride;
            // only the image part of it is painted.
            composite_subtile(
                &pixels,
                tiff_tile_w,
                0,
                0,
                src_w,
                src_h,
                tile_x,
                tile_y,
                &mut output,
                lx as i64,
                ly as i64,
                w,
                h,
            );
        }

        Ok(output)
    }

    /// Read a region from a tilemap level (Ventana BIF AOIs).
    ///
    /// Each placed (sub)tile names the physical TIFF tile it comes from, the
    /// sub-rectangle within it, and its destination in level pixel space. One
    /// physical tile can back many subtiles, so each is decoded at most once
    /// per call — and, through `cache`, at most once across calls.
    #[allow(clippy::too_many_arguments)]
    fn read_region_tilemap(
        &self,
        tilemap: &TileMap,
        lvl: &LevelData,
        cache: LevelCache<'_>,
        lx: f64,
        ly: f64,
        w: i64,
        h: i64,
    ) -> Result<Vec<u32>> {
        let mut output = vec![0u32; (w * h) as usize];

        let tiff_mutex = self.tiff.as_ref().ok_or_else(|| {
            OpenSlideError::Decode("No TIFF data available for tile reading".into())
        })?;

        let ox = lx.round() as i64;
        let oy = ly.round() as i64;
        let tiff_tile_w = lvl.tiff_tile_width;
        let tiff_tile_h = lvl.tiff_tile_height;

        let placed: Vec<&PlacedTile> = tilemap.tiles_for_region(ox, oy, w, h).collect();

        // Keyed by the *physical* TIFF tile index, which is a Ventana subtile's
        // real identity: the grid position is not, because several placed
        // subtiles are sub-rectangles of one shared physical tile and must all
        // come from a single decode. `decoded` is both the phase-0 cache hits
        // and, below, the freshly decoded tiles, so a tile touched by several
        // subtiles costs one probe and one decode however many name it.
        let mut decoded: HashMap<usize, Arc<Vec<u32>>> = HashMap::new();

        // Resolve every distinct physical tile's byte range first, then fetch
        // them in one call — the same phases as the tiled path in
        // `read_region`. Reading inside the subtile loop took and dropped the
        // TIFF mutex per subtile and cost a round trip each, which is what put
        // a Ventana patch among the most request-hungry in the suite. The cache
        // is probed before the mutex is taken, so a fully cached region neither
        // locks nor reads.
        let mut seen: HashSet<usize> = HashSet::new();
        let mut wanted: Vec<usize> = Vec::new();
        for t in &placed {
            if !seen.insert(t.tiff_tile_index) {
                continue; // another subtile of a tile already accounted for
            }
            match cache.get(TileId::Index {
                stream: 0,
                index: t.tiff_tile_index as u64,
            }) {
                Some(px) => {
                    decoded.insert(t.tiff_tile_index, px);
                }
                None => wanted.push(t.tiff_tile_index),
            }
        }

        let (indices, tiles_data) = if wanted.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            let mut tiff = tiff_mutex.lock();
            let mut indices = Vec::with_capacity(wanted.len());
            let mut ranges = Vec::with_capacity(wanted.len());
            for index in wanted {
                // A missing tile (offset or byte count of 0) is a hole; leaving
                // it out of the batch leaves its subtiles transparent.
                if let Some((offset, size)) = tiff.tile_location(lvl.tiff_dir, index)? {
                    indices.push(index);
                    ranges.push(offset..offset + size as u64);
                }
            }
            let data = tiff.reader().read_ranges(&ranges)?;
            (indices, data)
        };

        // Decode each physical tile once, however many subtiles it backs.
        for (index, data) in indices.into_iter().zip(tiles_data) {
            if data.is_empty() {
                continue;
            }
            let px = Arc::new(
                lvl.decoder
                    .decode(&data, tiff_tile_w as u32, tiff_tile_h as u32)?,
            );
            cache.insert(
                TileId::Index {
                    stream: 0,
                    index: index as u64,
                },
                px.clone(),
            );
            decoded.insert(index, px);
        }

        for t in placed {
            let Some(px) = decoded.get(&t.tiff_tile_index) else {
                continue; // missing tile → leave transparent
            };
            composite_subtile(
                px.as_slice(),
                tiff_tile_w,
                t.src_x,
                t.src_y,
                t.src_w,
                t.src_h,
                t.dest_x,
                t.dest_y,
                &mut output,
                ox,
                oy,
                w,
                h,
            );
        }

        Ok(output)
    }

    /// Read a region from an NDPI level (one giant JPEG, restart-marker tiles).
    ///
    /// The grid maps the region to `(col, row)` tiles as usual; each tile is
    /// reconstructed into a self-contained JPEG and decoded via the level's
    /// `JpegDecoder`. The TIFF mutex is held for the whole read because the
    /// restart-marker offsets are resolved through its reader (see
    /// `hamamatsu.rs` — restart-marker offsets are memoized per level).
    ///
    /// Two phases, like the tiled path: every tile's entropy range is resolved
    /// first, then the region is fetched in one batched call. Resolution can
    /// itself read — a level whose restart offsets are neither memoized nor
    /// carried by the `NDPI_MCU_STARTS` tag has to be scanned for them, and
    /// that scan is serial by construction, since a marker's position is only
    /// known once everything before it has been read. What the split removes is
    /// the round trip per tile spent on the pixels themselves, which is the
    /// part that recurs on every region read rather than once per level.
    ///
    /// A tile `cache` already holds skips both, marker walk included.
    #[allow(clippy::too_many_arguments)]
    fn read_region_ndpi(
        &self,
        ndpi: &NdpiLevel,
        lvl: &LevelData,
        cache: LevelCache<'_>,
        lx: f64,
        ly: f64,
        w: i64,
        h: i64,
    ) -> Result<Vec<u32>> {
        let tiff_mutex = self.tiff.as_ref().ok_or_else(|| {
            OpenSlideError::Decode("No TIFF data available for tile reading".into())
        })?;
        let tiff = tiff_mutex.lock();
        let reader = tiff.reader();

        let tiles = lvl.grid.tiles_for_region(lx, ly, w as f64, h as f64);
        let mut output = vec![0u32; (w * h) as usize];
        let (tw, th) = (lvl.tiff_tile_width, lvl.tiff_tile_height);
        let (ox, oy) = (lx as i64, ly as i64);

        // Phase 0: an NDPI "tile" is a restart-marker span of one giant JPEG.
        // It has no identity outside the grid — no tile table, no byte range
        // known ahead of the marker walk — so `Cell` is the honest key here,
        // and it is exact because this level is a single regular grid over a
        // single stream. Probing first means a cached tile costs neither the
        // marker walk nor a place in the gather.
        let mut misses: Vec<(i64, i64)> = Vec::new();
        let mut pixels: HashMap<(i64, i64), Arc<Vec<u32>>> = HashMap::new();
        for &(col, row) in &tiles {
            match cache.get(TileId::Cell { col, row }) {
                Some(px) => {
                    pixels.insert((col, row), px);
                }
                None => misses.push((col, row)),
            }
        }

        // Phase 1: where each missing tile's entropy bytes are. A tile off the
        // end of the level, or one the markers give no bytes for, is left out
        // of the batch and so stays transparent.
        if !misses.is_empty() {
            let located = ndpi.tile_ranges(reader, &misses)?;
            let mut owners = Vec::with_capacity(misses.len());
            let mut ranges = Vec::with_capacity(misses.len());
            for (&(col, row), range) in misses.iter().zip(located) {
                if let Some(range) = range {
                    owners.push((col, row));
                    ranges.push(range);
                }
            }

            // Phase 2: one batched fetch. Phase 3: reconstruct and decode.
            if !ranges.is_empty() {
                let tiles_data = reader.read_ranges(&ranges)?;
                for ((col, row), entropy) in owners.into_iter().zip(tiles_data) {
                    let px = Arc::new(ndpi.decode_tile_data(&entropy, &*lvl.decoder)?);
                    cache.insert(TileId::Cell { col, row }, px.clone());
                    pixels.insert((col, row), px);
                }
            }
        }

        // Phase 4: composite, in the grid order the region was mapped to.
        for (col, row) in tiles {
            let Some(px) = pixels.get(&(col, row)) else {
                continue; // no bytes for this tile → leave it transparent
            };
            let (tile_x, tile_y, src_w, src_h) = lvl.grid.tile_extent(col, row, tw, th);
            composite_subtile(
                px,
                tw,
                0,
                0,
                src_w,
                src_h,
                tile_x,
                tile_y,
                &mut output,
                ox,
                oy,
                w,
                h,
            );
        }
        Ok(output)
    }

    /// Async counterpart to [`read_region`](Self::read_region).
    ///
    /// For slides opened from an async backend (`open_url`/`open_store` and
    /// their `_async` variants), the per-tile byte reads are issued as awaited
    /// range requests — so reading a region from remote storage never blocks a
    /// thread on network I/O. Tile offset resolution, decoding, and
    /// compositing are CPU work and remain synchronous.
    ///
    /// For locally-opened slides (no async backend) this transparently defers
    /// to the synchronous [`read_region`](Self::read_region); local positioned
    /// reads do not block on a runtime, so this is safe in an async context.
    pub async fn read_region_async(
        &self,
        x: i64,
        y: i64,
        level: i32,
        w: i64,
        h: i64,
    ) -> Result<Vec<u32>> {
        // No async backend (e.g. local file): the sync path uses non-blocking
        // positioned reads, so it is safe to run here directly.
        let Some(reader) = self.async_reader.clone() else {
            return self.read_region(x, y, level, w, h);
        };

        if level < 0 || level >= self.level_count() {
            return Err(OpenSlideError::InvalidLevel { level });
        }
        if w <= 0 || h <= 0 {
            return Ok(vec![]);
        }

        let lvl = &self.levels[level as usize];

        // Tilemap (Ventana) and NDPI levels don't fit the range-fetch path: the
        // former needs sub-rect compositing across shared physical tiles, the
        // latter resolves per-tile byte ranges by walking restart markers
        // through the sync reader. MIRAX, Zeiss, DICOM and synthetic levels
        // read through their own non-TIFF code. Read all of them via the sync
        // path, off this thread — on a remote slide those reads go through
        // `BlockingReader`, which must not run on a runtime thread. ponytail:
        // async subtile/tile fetch if either becomes a hot remote path.
        if lvl.tilemap.is_some()
            || lvl.ndpi.is_some()
            || lvl.mirax.is_some()
            || lvl.zeiss.is_some()
            || lvl.synthetic.is_some()
            || lvl.dicom.is_some()
        {
            return read_off_runtime(|| self.read_region(x, y, level, w, h));
        }

        let ds = lvl.info.downsample;
        let lx = x as f64 / ds;
        let ly = y as f64 / ds;

        let tiles = lvl.grid.tiles_for_region(lx, ly, w as f64, h as f64);

        let out_len = (w * h) as usize;
        let mut output = vec![0u32; out_len];

        let tiff_mutex = self.tiff.as_ref().ok_or_else(|| {
            OpenSlideError::Decode("No TIFF data available for tile reading".into())
        })?;

        let tiff_tile_w = lvl.tiff_tile_width;
        let tiff_tile_h = lvl.tiff_tile_height;
        let tiles_across = tiff_tiles_across(tiff_mutex, lvl.tiff_dir, tiff_tile_w)?;

        // Phase 0 + 1 (sync, under lock): serve what the cache holds and resolve
        // a byte range only for what it doesn't. Same keys and same ordering
        // rules as the sync path — see `read_region`, which explains why `plan`
        // preserves the grid's order. The mutex guard is released before any
        // await — parking_lot guards are not held across suspension points.
        let cache = self.level_cache(level);
        let mut plan: Vec<TilePlan> = Vec::with_capacity(tiles.len());
        let mut miss_ids: Vec<TileId> = Vec::with_capacity(tiles.len());
        let mut ranges: Vec<std::ops::Range<u64>> = Vec::with_capacity(tiles.len());
        {
            let mut tiff = tiff_mutex.lock();
            for (col, row) in tiles {
                let tile_index = (row * tiles_across + col) as usize;
                let extent = lvl.grid.tile_extent(col, row, tiff_tile_w, tiff_tile_h);
                let id = TileId::Index {
                    stream: 0,
                    index: tile_index as u64,
                };
                if let Some(px) = cache.get(id) {
                    plan.push((extent, Some(px)));
                    continue;
                }
                if let Some((offset, size)) = tiff.tile_location(lvl.tiff_dir, tile_index)? {
                    plan.push((extent, None));
                    miss_ids.push(id);
                    ranges.push(offset..offset + size as u64);
                }
            }
        }

        // Phase 2 (async): fetch the tile byte ranges the cache could not serve,
        // all at once. The backend coalesces adjacent ranges and fetches
        // concurrently. A fully-cached region skips the await entirely.
        let tiles_data = if ranges.is_empty() {
            Vec::new()
        } else {
            reader.read_ranges_boxed(&ranges).await?
        };

        // Phase 3 (sync): decode and composite each tile.
        let mut fetched = tiles_data.into_iter().zip(miss_ids);
        for ((tile_x, tile_y, src_w, src_h), cached) in plan {
            let pixels = match cached {
                Some(px) => px,
                None => {
                    let Some((tile_data, id)) = fetched.next() else {
                        continue;
                    };
                    if tile_data.is_empty() {
                        continue;
                    }
                    let decoded = Arc::new(lvl.decoder.decode(
                        tile_data.as_ref(),
                        tiff_tile_w as u32,
                        tiff_tile_h as u32,
                    )?);
                    cache.insert(id, decoded.clone());
                    decoded
                }
            };

            composite_subtile(
                &pixels,
                tiff_tile_w,
                0,
                0,
                src_w,
                src_h,
                tile_x,
                tile_y,
                &mut output,
                lx as i64,
                ly as i64,
                w,
                h,
            );
        }

        Ok(output)
    }

    /// Read a region into an existing buffer.
    pub fn read_region_into(
        &self,
        dest: &mut [u32],
        x: i64,
        y: i64,
        level: i32,
        w: i64,
        h: i64,
    ) -> Result<()> {
        let data = self.read_region(x, y, level, w, h)?;
        let len = data.len().min(dest.len());
        dest[..len].copy_from_slice(&data[..len]);
        Ok(())
    }

    /// List all property names.
    pub fn property_names(&self) -> Vec<String> {
        self.properties.names()
    }

    /// Get the value of a property by name.
    pub fn property_value(&self, name: &str) -> Option<&str> {
        self.properties.get(name)
    }

    /// List names of all associated images.
    pub fn associated_image_names(&self) -> Vec<String> {
        self.associated_images
            .iter()
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Get the dimensions of an associated image.
    pub fn associated_image_dimensions(&self, name: &str) -> Option<(i64, i64)> {
        self.associated_images
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, img)| (img.width, img.height))
    }

    /// Read the pixel data of an associated image.
    pub fn read_associated_image(&self, name: &str) -> Result<Vec<u32>> {
        self.associated_images
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, img)| img.argb_data.clone())
            .ok_or_else(|| {
                OpenSlideError::InvalidArgument(format!("no such associated image: {name}"))
            })
    }

    /// The ICC colour profile for the main slide image, or `None` if it has none.
    ///
    /// **The first call may read from the slide.** Profiles are not loaded at open
    /// unless they were already on the wire, because a DICOM profile can be 13 MB
    /// — see [`IccProfile`]. Use [`icc_profile_size`](Self::icc_profile_size) to
    /// find out how large one is without fetching it. The bytes are cached, so
    /// later calls are free.
    pub fn icc_profile(&self) -> Result<Option<&[u8]>> {
        self.icc_profile.bytes()
    }

    /// Size of the main slide image's ICC profile, without reading it. `None` if
    /// the slide has no profile.
    pub fn icc_profile_size(&self) -> Option<usize> {
        self.icc_profile.len()
    }

    /// The ICC colour profile for an associated image. As with
    /// [`icc_profile`](Self::icc_profile), the first call may read.
    ///
    /// `Ok(None)` covers both "no such image" and "that image has no profile".
    pub fn associated_image_icc_profile(&self, name: &str) -> Result<Option<&[u8]>> {
        match self.associated_image(name) {
            Some(img) => img.icc_profile.bytes(),
            None => Ok(None),
        }
    }

    /// Size of an associated image's ICC profile, without reading it.
    pub fn associated_image_icc_profile_size(&self, name: &str) -> Option<usize> {
        self.associated_image(name)?.icc_profile.len()
    }

    fn associated_image(&self, name: &str) -> Option<&AssociatedImage> {
        self.associated_images
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, img)| img)
    }

    /// Set or replace the decoded-tile cache.
    ///
    /// Pass a [`Cache`] shared with other slides to give them one common byte
    /// budget; entries stay per-slide regardless, because this handle's
    /// [`SlideId`] is part of every key it writes. `Cache::new(0)` disables
    /// caching for this slide.
    ///
    /// Replacing the cache drops nothing but the old cache's entries — the
    /// slide's identity is unchanged, so a cache handed back later still
    /// resolves its tiles.
    pub fn set_cache(&mut self, cache: Cache) {
        self.cache = cache;
    }

    /// The tile cache, if a caller wants to inspect or share it.
    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    /// The cache handle every read path uses: this slide, one level.
    fn level_cache(&self, level: i32) -> LevelCache<'_> {
        self.cache.level(self.id, level)
    }

    /// Library version string.
    pub fn version() -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
}

/// Build the [`SlideLocation`] for a local file: a [`LocalFs`] rooted at the
/// containing directory, with the file's own name as the path.
///
/// Rooting at the parent rather than `/` keeps a slide's sibling lookups inside
/// its own directory. Returns `None` for a name that isn't UTF-8, which the
/// `/`-separated path API can't address; single-file formats still open fine
/// without a location.
fn local_location(path: &Path) -> Option<SlideLocation> {
    let name = path.file_name()?.to_str()?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    Some(SlideLocation::new(Arc::new(LocalFs::new(parent)), name))
}

/// Adapt a shared reader from [`SlideFs::open`] into the owned form the open
/// pipeline takes.
fn reader_to_box(reader: Arc<dyn SlideReader>) -> Box<dyn SlideReader> {
    Box::new(Shared::new(reader))
}

/// Run a synchronous open on a dedicated thread and await its result.
///
/// The sync open path drives `object_store`'s async I/O via the shared
/// internal runtime's `block_on`, which panics if called from a thread that is
/// already inside an async runtime. Running it on a fresh thread (with no
/// ambient runtime) keeps `open_*_async` safe to call from any executor.
#[cfg(feature = "object-store")]
async fn run_open_off_runtime<F>(f: F) -> Result<OpenSlide>
where
    F: FnOnce() -> Result<OpenSlide> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.await
        .map_err(|_| OpenSlideError::Other("slide open thread terminated unexpectedly".into()))?
}

/// Run a synchronous region read on a thread that is not driving a runtime,
/// and hand back its result.
///
/// The level kinds `read_region_async` can't fetch by range fall back to the
/// synchronous read path, and on a remote slide every byte that path touches
/// goes through `BlockingReader` — i.e. `Runtime::block_on`, which panics
/// outright when the calling thread is already inside a runtime. The open path
/// has the same hazard and solves it with [`run_open_off_runtime`], but that
/// takes a `'static` closure: here the work borrows `&self` from a slide the
/// caller still owns, and `OpenSlide` is neither cloneable nor `'static`, so
/// `std::thread::spawn` can't carry it.
///
/// A scoped thread can, because the borrow only has to outlive the scope. The
/// price is that this thread blocks until the read finishes rather than
/// yielding at an await. That price is also why the alternative —
/// `tokio::task::block_in_place`, which asks the multi-threaded scheduler to
/// move this worker's other tasks to a fresh thread before blocking — was not
/// chosen: it panics on a current-thread runtime, and a library can't dictate
/// which flavour its caller built. The scoped thread behaves identically on
/// both: the closure runs somewhere with no ambient runtime, which is all
/// `block_on` needs. On a multi-threaded runtime the other workers keep
/// running; on a current-thread runtime the caller's other tasks stall for the
/// duration of the read, since that runtime has no second thread to run them
/// on — the same stall a direct `read_region` call would cause, and strictly
/// better than the panic it replaces.
///
/// Off a runtime (a plain sync caller, or a local slide reached through some
/// other path) there is nothing to protect against, so the read runs in place
/// and no thread is spawned.
fn read_off_runtime<F>(f: F) -> Result<Vec<u32>>
where
    F: FnOnce() -> Result<Vec<u32>> + Send,
{
    #[cfg(feature = "object-store")]
    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::scope(|scope| match scope.spawn(f).join() {
            Ok(pixels) => pixels,
            // The read panicked on the scratch thread. Re-raise it here rather
            // than reporting a decoder bug as an ordinary read error.
            Err(payload) => std::panic::resume_unwind(payload),
        });
    }
    f()
}

/// Real TIFF tile-column count for row-major tile indexing, i.e.
/// `ceil(image_width / tile_width)` read straight from the TIFF.
///
/// This is deliberately NOT `grid.tiles_across()`: for overlap formats (Trestle)
/// the grid's stride is the overlap-adjusted *advance* (tile − overlap), so its
/// column count exceeds the real TIFF tile grid by one. Using the grid count to
/// index `tile_index = row * across + col` then drifts by one tile per row —
/// correct at row 0, badly wrong deep into the slide. For non-overlap formats
/// the two counts coincide, so this is safe for every vendor.
fn tiff_tiles_across(tiff: &Mutex<TiffLike>, dir: i64, tile_w: i64) -> Result<i64> {
    let iw = tiff
        .lock()
        .get_uint(dir, crate::decode::tiff::tags::IMAGE_WIDTH)? as i64;
    Ok((iw + tile_w - 1) / tile_w)
}

/// Composite a sub-rectangle of a decoded tile into the output buffer.
///
/// Copies the `src_w * src_h` block at `(src_x, src_y)` within `tile_pixels`
/// (row stride `tile_stride`) to `(dest_x, dest_y)` in the output region,
/// clipping to the output bounds.
///
/// Every caller passes a sub-rectangle rather than the whole tile: a tile at the
/// right or bottom edge of an image runs past it, and the encoder padding out
/// there is not image (see [`SimpleGrid::tile_extent`]).
///
/// [`SimpleGrid::tile_extent`]: crate::grid::simple::SimpleGrid::tile_extent
#[allow(clippy::too_many_arguments)]
pub(crate) fn composite_subtile(
    tile_pixels: &[u32],
    tile_stride: i64,
    src_x: i64,
    src_y: i64,
    src_w: i64,
    src_h: i64,
    dest_x: i64,
    dest_y: i64,
    out_pixels: &mut [u32],
    out_x: i64,
    out_y: i64,
    out_w: i64,
    out_h: i64,
) {
    // Sub-rect's top-left relative to the output buffer.
    let off_x = dest_x - out_x;
    let off_y = dest_y - out_y;

    // Clip the sub-rect to the output region.
    let copy_x0 = off_x.max(0); // first output column written
    let skip_x = copy_x0 - off_x; // source columns skipped at the left
    let copy_w = (src_w - skip_x).min(out_w - copy_x0);
    let copy_y0 = off_y.max(0);
    let skip_y = copy_y0 - off_y;
    let copy_h = (src_h - skip_y).min(out_h - copy_y0);

    if copy_w <= 0 || copy_h <= 0 {
        return;
    }

    for row in 0..copy_h {
        let src_off = ((src_y + skip_y + row) * tile_stride + (src_x + skip_x)) as usize;
        let dst_off = ((copy_y0 + row) * out_w + copy_x0) as usize;
        let len = copy_w as usize;

        if src_off + len <= tile_pixels.len() && dst_off + len <= out_pixels.len() {
            out_pixels[dst_off..dst_off + len]
                .copy_from_slice(&tile_pixels[src_off..src_off + len]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::TileDecoder;
    use crate::grid::simple::SimpleGrid;
    use crate::icc::IccProfile;
    use crate::level::LevelInfo;
    use crate::properties::Properties;
    use bytes::Bytes;
    use std::ops::Range;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// What a region read cost the reader beneath it.
    #[derive(Default)]
    struct Counts {
        /// Single positioned reads — one per subtile is the shape the tilemap
        /// path had before it batched.
        reads: AtomicUsize,
        /// Batched gathers, and the ranges they carried.
        batches: AtomicUsize,
        ranges: AtomicUsize,
        decodes: AtomicUsize,
    }

    impl Counts {
        fn reset(&self) {
            for c in [&self.reads, &self.batches, &self.ranges, &self.decodes] {
                c.store(0, Ordering::Relaxed);
            }
        }
    }

    struct Counting {
        data: Vec<u8>,
        counts: Arc<Counts>,
    }

    // Implemented for the `Arc` so the TIFF parser and the test can hold the
    // same reader — the counters would otherwise be moved into the parser.
    impl SlideReader for Arc<Counting> {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
            self.counts.reads.fetch_add(1, Ordering::Relaxed);
            let start = (offset as usize).min(self.data.len());
            let n = buf.len().min(self.data.len() - start);
            buf[..n].copy_from_slice(&self.data[start..start + n]);
            Ok(n)
        }

        fn size(&self) -> Result<u64> {
            Ok(self.data.len() as u64)
        }

        fn read_ranges(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
            self.counts.batches.fetch_add(1, Ordering::Relaxed);
            self.counts
                .ranges
                .fetch_add(ranges.len(), Ordering::Relaxed);
            Ok(ranges
                .iter()
                .map(|r| Bytes::copy_from_slice(&self.data[r.start as usize..r.end as usize]))
                .collect())
        }
    }

    /// Decodes a tile to a solid image of its first byte, so a composited pixel
    /// names the physical tile it came from.
    struct FirstByteDecoder(Arc<Counts>);

    impl TileDecoder for FirstByteDecoder {
        fn decode(&self, data: &[u8], width: u32, height: u32) -> Result<Vec<u32>> {
            self.0.decodes.fetch_add(1, Ordering::Relaxed);
            Ok(vec![data[0] as u32; (width * height) as usize])
        }
    }

    fn ifd_entry(buf: &mut Vec<u8>, tag: u16, typ: u16, count: u32, value: u32) {
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&typ.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        buf.extend_from_slice(&value.to_le_bytes());
    }

    /// A little-endian classic TIFF of `fills.len()` uncompressed `tile × tile`
    /// RGB tiles, tile *i* filled with `fills[i]`. A `None` entry is a hole —
    /// offset and byte count of 0, which is how a TIFF records a tile that was
    /// never written.
    fn build_tiled_tiff(tile: u32, tiles_across: u32, fills: &[Option<u8>]) -> Vec<u8> {
        let num_tiles = fills.len() as u32;
        let bytes_per_tile = (tile * tile * 3) as usize;
        let entries: u16 = 8;
        let ifd_end = 8 + 2 + entries as usize * 12 + 4;
        let offsets_table = ifd_end;
        let counts_table = offsets_table + num_tiles as usize * 4;
        let tile_data = counts_table + num_tiles as usize * 4;

        let mut buf = Vec::new();
        buf.extend_from_slice(&[0x49, 0x49]); // little-endian
        buf.extend_from_slice(&42u16.to_le_bytes());
        buf.extend_from_slice(&8u32.to_le_bytes());

        // Tags in ascending order, as the format requires.
        buf.extend_from_slice(&entries.to_le_bytes());
        ifd_entry(&mut buf, 256, 4, 1, tiles_across * tile); // ImageWidth
        ifd_entry(&mut buf, 257, 4, 1, num_tiles.div_ceil(tiles_across) * tile); // ImageLength
        ifd_entry(&mut buf, 259, 3, 1, 1); // Compression = none
        ifd_entry(&mut buf, 277, 3, 1, 3); // SamplesPerPixel
        ifd_entry(&mut buf, 322, 3, 1, tile); // TileWidth
        ifd_entry(&mut buf, 323, 3, 1, tile); // TileLength
        ifd_entry(&mut buf, 324, 4, num_tiles, offsets_table as u32); // TileOffsets
        ifd_entry(&mut buf, 325, 4, num_tiles, counts_table as u32); // TileByteCounts
        buf.extend_from_slice(&0u32.to_le_bytes()); // no next IFD

        for (i, fill) in fills.iter().enumerate() {
            let off = match fill {
                Some(_) => (tile_data + i * bytes_per_tile) as u32,
                None => 0,
            };
            buf.extend_from_slice(&off.to_le_bytes());
        }
        for fill in fills {
            let count = fill.map_or(0, |_| bytes_per_tile as u32);
            buf.extend_from_slice(&count.to_le_bytes());
        }
        for fill in fills {
            buf.extend(std::iter::repeat_n(fill.unwrap_or(0), bytes_per_tile));
        }
        buf
    }

    /// A Ventana-shaped slide: one tilemap level over the given TIFF.
    fn tilemap_slide(
        tiff_bytes: Vec<u8>,
        tiles: Vec<PlacedTile>,
        counts: Arc<Counts>,
    ) -> OpenSlide {
        let reader = Arc::new(Counting {
            data: tiff_bytes,
            counts: counts.clone(),
        });
        let tiff = TiffLike::try_parse(Box::new(reader)).expect("parse TIFF");
        let tilemap = TileMap::new(tiles);
        let (width, height) = (tilemap.width, tilemap.height);
        OpenSlide::from_slide_data(vendor::SlideData {
            levels: vec![vendor::LevelData {
                info: LevelInfo {
                    width,
                    height,
                    downsample: 1.0,
                    tile_width: 2,
                    tile_height: 2,
                },
                grid: SimpleGrid::new(width, height, 2, 2),
                tiff_dir: 0,
                decoder: Box::new(FirstByteDecoder(counts)),
                tiff_tile_width: 2,
                tiff_tile_height: 2,
                tilemap: Some(tilemap),
                ndpi: None,
                mirax: None,
                zeiss: None,
                synthetic: None,
                dicom: None,
            }],
            properties: Properties::new(),
            associated_images: Vec::new(),
            icc_profile: IccProfile::absent(),
            tiff: Some(tiff),
        })
    }

    fn placed(index: usize, dest_x: i64, dest_y: i64) -> PlacedTile {
        PlacedTile {
            tiff_tile_index: index,
            src_x: 0,
            src_y: 0,
            src_w: 2,
            src_h: 2,
            dest_x,
            dest_y,
        }
    }

    #[test]
    fn tilemap_region_costs_one_batched_fetch() {
        let counts = Arc::new(Counts::default());
        // Three physical tiles, the last of them never written.
        let tiff = build_tiled_tiff(2, 3, &[Some(1), Some(2), None]);
        let slide = tilemap_slide(
            tiff,
            vec![
                // Two subtiles of one physical tile, as an AOI stitch produces.
                placed(0, 0, 0),
                placed(0, 2, 0),
                placed(1, 0, 2),
                placed(2, 2, 2), // the hole
            ],
            counts.clone(),
        );

        // Parsing the TIFF read; only the region read is under test.
        counts.reset();
        let out = slide.read_region(0, 0, 0, 4, 4).expect("read");

        #[rustfmt::skip]
        assert_eq!(
            out,
            vec![
                1, 1, 1, 1,
                1, 1, 1, 1,
                2, 2, 0, 0,
                2, 2, 0, 0,
            ],
            "the hole stays transparent"
        );
        assert_eq!(
            counts.batches.load(Ordering::Relaxed),
            1,
            "four subtiles should cost one gather, not one fetch each"
        );
        assert_eq!(
            counts.ranges.load(Ordering::Relaxed),
            2,
            "the gather carries one range per distinct written tile"
        );
        assert_eq!(
            counts.decodes.load(Ordering::Relaxed),
            2,
            "the tile two subtiles share is decoded once"
        );
    }
}
