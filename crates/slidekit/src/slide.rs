use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::associated::AssociatedImage;
use crate::cache::Cache;
use crate::decode::tiff::TiffLike;
use crate::error::{OpenSlideError, Result};
use crate::grid::TileGrid;
use crate::grid::tilemap::TileMap;
use crate::icc::IccProfile;
use crate::io::fs::{Located, Shared, SlideFs, SlideLocation};
use crate::io::local::{LocalFileReader, LocalFs};
use crate::io::{AsyncReadAt, SlideReader};
use crate::properties::Properties;
use crate::vendor::hamamatsu::NdpiLevel;
use crate::vendor::{self, LevelData};

/// Main entry point for reading whole slide images.
pub struct OpenSlide {
    levels: Vec<LevelData>,
    properties: Properties,
    associated_images: Vec<(String, AssociatedImage)>,
    icc_profile: IccProfile,
    cache: Cache,
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

        // Tilemap levels (Ventana BIF AOIs) don't lie on a regular grid.
        if let Some(tilemap) = &lvl.tilemap {
            return self.read_region_tilemap(tilemap, lvl, lx, ly, w, h);
        }

        // NDPI levels have no TIFF tiles — each tile is reconstructed from the
        // level JPEG's restart markers.
        if let Some(ndpi) = &lvl.ndpi {
            return self.read_region_ndpi(ndpi, lvl, lx, ly, w, h);
        }

        // MIRAX levels have no TIFF either — tiles are standalone blobs in
        // sibling `Data####.dat` files, placed by the MiraxLevel's own index.
        if let Some(mirax) = &lvl.mirax {
            return mirax.read_region(&*lvl.decoder, lx.round() as i64, ly.round() as i64, w, h);
        }

        // Zeiss CZI levels are likewise non-TIFF: each tile is a subblock in the
        // single container file, decoded per its own compression/pixel type.
        if let Some(zeiss) = &lvl.zeiss {
            return zeiss.read_region(lx.round() as i64, ly.round() as i64, w, h);
        }

        // DICOM levels are frames of one (or a concatenation of) SOP instances,
        // each located and decoded by the level itself.
        if let Some(dicom) = &lvl.dicom {
            return dicom.read_region(lx.round() as i64, ly.round() as i64, w, h);
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
        let (placements, tiles_data) = {
            let mut tiff = tiff_mutex.lock();
            let mut placements = Vec::with_capacity(tiles.len());
            let mut ranges = Vec::with_capacity(tiles.len());
            for (col, row) in tiles {
                let tile_index = (row * tiles_across + col) as usize;
                // A missing tile (offset or byte count of 0) is a hole; leaving
                // it out of the batch leaves it transparent.
                if let Some((offset, size)) = tiff.tile_location(lvl.tiff_dir, tile_index)? {
                    // Where the tile goes, and how much of it is image rather
                    // than encoder padding past the right/bottom edge.
                    placements.push(lvl.grid.tile_extent(col, row, tiff_tile_w, tiff_tile_h));
                    ranges.push(offset..offset + size as u64);
                }
            }
            let data = tiff.reader().read_ranges(&ranges)?;
            (placements, data)
        };

        for ((tile_x, tile_y, src_w, src_h), tile_data) in placements.into_iter().zip(tiles_data) {
            if tile_data.is_empty() {
                continue;
            }

            // Decode at actual TIFF tile dimensions.
            let decoded = lvl
                .decoder
                .decode(&tile_data, tiff_tile_w as u32, tiff_tile_h as u32)?;

            // The decoded buffer is a whole TIFF tile, so that is its row stride;
            // only the image part of it is painted.
            composite_subtile(
                &decoded,
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
    /// per call.
    fn read_region_tilemap(
        &self,
        tilemap: &TileMap,
        lvl: &LevelData,
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

        let mut decoded: HashMap<usize, Vec<u32>> = HashMap::new();
        for t in tilemap.tiles_for_region(ox, oy, w, h) {
            use std::collections::hash_map::Entry;
            let px = match decoded.entry(t.tiff_tile_index) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => {
                    let data = {
                        let mut tiff = tiff_mutex.lock();
                        tiff.read_tile_data(lvl.tiff_dir, t.tiff_tile_index)?
                    };
                    if data.is_empty() {
                        continue; // missing tile → leave transparent
                    }
                    let px = lvl
                        .decoder
                        .decode(&data, tiff_tile_w as u32, tiff_tile_h as u32)?;
                    e.insert(px)
                }
            };
            composite_subtile(
                px,
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
    fn read_region_ndpi(
        &self,
        ndpi: &NdpiLevel,
        lvl: &LevelData,
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

        for (col, row) in tiles {
            if let Some(decoded) = ndpi.decode_tile(reader, col, row, &*lvl.decoder)? {
                let (tile_x, tile_y, src_w, src_h) = lvl.grid.tile_extent(col, row, tw, th);
                composite_subtile(
                    &decoded,
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
        // through the sync reader. Read both via the sync path (positioned
        // reads). ponytail: async subtile/tile fetch if either becomes a hot
        // remote path.
        if lvl.tilemap.is_some()
            || lvl.ndpi.is_some()
            || lvl.mirax.is_some()
            || lvl.zeiss.is_some()
            || lvl.synthetic.is_some()
            || lvl.dicom.is_some()
        {
            return self.read_region(x, y, level, w, h);
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

        // Phase 1 (sync, under lock): resolve each tile's byte range. The mutex
        // guard is released before any await — parking_lot guards are not held
        // across suspension points.
        // `(x, y, w, h)`: where the tile goes, and how much of it is image.
        let mut placements: Vec<(i64, i64, i64, i64)> = Vec::with_capacity(tiles.len());
        let mut ranges: Vec<std::ops::Range<u64>> = Vec::with_capacity(tiles.len());
        {
            let mut tiff = tiff_mutex.lock();
            for (col, row) in tiles {
                let tile_index = (row * tiles_across + col) as usize;
                if let Some((offset, size)) = tiff.tile_location(lvl.tiff_dir, tile_index)? {
                    placements.push(lvl.grid.tile_extent(col, row, tiff_tile_w, tiff_tile_h));
                    ranges.push(offset..offset + size as u64);
                }
            }
        }

        if ranges.is_empty() {
            return Ok(output);
        }

        // Phase 2 (async): fetch all tile byte ranges at once. The backend
        // coalesces adjacent ranges and fetches concurrently.
        let tiles_data = reader.read_ranges_boxed(&ranges).await?;

        // Phase 3 (sync): decode and composite each tile.
        for ((tile_x, tile_y, src_w, src_h), tile_data) in placements.into_iter().zip(tiles_data) {
            if tile_data.is_empty() {
                continue;
            }

            let decoded =
                lvl.decoder
                    .decode(tile_data.as_ref(), tiff_tile_w as u32, tiff_tile_h as u32)?;

            composite_subtile(
                &decoded,
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

    /// Set or replace the tile cache.
    pub fn set_cache(&mut self, cache: Cache) {
        self.cache = cache;
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
