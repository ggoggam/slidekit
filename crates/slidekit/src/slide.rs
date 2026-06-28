use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::associated::AssociatedImage;
use crate::cache::Cache;
use crate::decode::tiff::TiffLike;
use crate::error::{OpenSlideError, Result};
use crate::grid::TileGrid;
use crate::io::local::LocalFileReader;
use crate::io::{AsyncReadAt, SlideReader};
use crate::properties::Properties;
use crate::vendor::{self, LevelData};

/// Main entry point for reading whole slide images.
pub struct OpenSlide {
    levels: Vec<LevelData>,
    properties: Properties,
    associated_images: Vec<(String, AssociatedImage)>,
    icc_profile: Option<Vec<u8>>,
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
        )
    }

    /// Open a whole slide image from a URL backed by the `object_store` crate.
    ///
    /// Supported schemes: `file://`, `s3://`, `gs://`, `az://`, and
    /// `http(s)://`. Remote slides are read with byte-range requests, so the
    /// whole file is never downloaded. Cloud credentials are read from the
    /// environment.
    ///
    /// This is the synchronous entry point; the underlying reads use the async
    /// [`ObjectStoreReader`](crate::io::object_store::ObjectStoreReader) driven
    /// to completion on a shared runtime. Do not call it from within an async
    /// context — build the reader yourself and use the async I/O directly.
    #[cfg(feature = "object-store")]
    pub fn open_url(url: &str) -> Result<Self> {
        use crate::io::object_store::ObjectStoreReader;
        Self::from_async_reader(
            ObjectStoreReader::open_url(url)?,
            std::path::PathBuf::from(url),
        )
    }

    /// Open a whole slide image from an [`object_store::ObjectStore`] handle and
    /// an object path. Use this to supply a pre-configured store (custom
    /// credentials, endpoints, retry policy, etc.).
    #[cfg(feature = "object-store")]
    pub fn open_store(
        store: std::sync::Arc<dyn object_store::ObjectStore>,
        path: object_store::path::Path,
    ) -> Result<Self> {
        use crate::io::object_store::ObjectStoreReader;
        let source = std::path::PathBuf::from(path.as_ref());
        Self::from_async_reader(ObjectStoreReader::new(store, path)?, source)
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
    #[cfg(feature = "object-store")]
    fn from_async_reader<R>(reader: R, source: std::path::PathBuf) -> Result<Self>
    where
        R: crate::io::AsyncSlideReader + Clone + 'static,
    {
        use crate::io::object_store::BlockingReader;
        let async_reader: Arc<dyn AsyncReadAt> = Arc::new(reader.clone());
        let mut slide = Self::open_with_readers(
            Box::new(BlockingReader::new(reader.clone())),
            Box::new(BlockingReader::new(reader)),
            source,
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
    ) -> Result<Self> {
        // Try to parse as TIFF (most formats are TIFF-based)
        let mut tl = TiffLike::try_parse(tiff_reader).ok();

        // Run vendor detection
        let drivers = vendor::all_drivers();
        let matched = vendor::detect_format(&drivers, &*detect_reader, &mut tl);

        match matched {
            Some(idx) => {
                let driver = &drivers[idx];
                let data = driver.open(detect_reader, tl)?;

                Ok(Self {
                    levels: data.levels,
                    properties: data.properties,
                    associated_images: data.associated_images,
                    icc_profile: data.icc_profile,
                    cache: Cache::default(),
                    tiff: data.tiff.map(Mutex::new),
                    async_reader: None,
                })
            }
            None => Err(OpenSlideError::UnsupportedFormat { path: source }),
        }
    }

    /// Detect the vendor for a slide file without fully opening it.
    pub fn detect_vendor(path: impl AsRef<Path>) -> Option<String> {
        let path = path.as_ref();
        let reader: Box<dyn SlideReader> = Box::new(LocalFileReader::open(path).ok()?);

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

        // Find overlapping tiles
        let tiles = lvl.grid.tiles_for_region(lx, ly, w as f64, h as f64);

        // Initialize output buffer with transparent black
        let out_len = (w * h) as usize;
        let mut output = vec![0u32; out_len];

        // Need a TiffLike to read tile data
        let tiff_mutex = self.tiff.as_ref().ok_or_else(|| {
            OpenSlideError::Decode("No TIFF data available for tile reading".into())
        })?;

        // Grid tile advance (may differ from TIFF tile size due to overlaps)
        let grid_tile_w = lvl.grid.tile_w;
        let grid_tile_h = lvl.grid.tile_h;
        // Actual TIFF tile dimensions for decoding
        let tiff_tile_w = lvl.tiff_tile_width;
        let tiff_tile_h = lvl.tiff_tile_height;
        let tiles_across = lvl.grid.tiles_across();

        for (col, row) in tiles {
            // Tile's position in level pixel space (using grid advance)
            let tile_x = col * grid_tile_w;
            let tile_y = row * grid_tile_h;

            // Tile index in TIFF (row-major)
            let tile_index = (row * tiles_across + col) as usize;

            // Read and decode tile
            let tile_data = {
                let mut tiff = tiff_mutex.lock();
                tiff.read_tile_data(lvl.tiff_dir, tile_index)?
            };

            if tile_data.is_empty() {
                continue; // Empty tile, leave as transparent
            }

            // Decode at actual TIFF tile dimensions
            let decoded = lvl
                .decoder
                .decode(&tile_data, tiff_tile_w as u32, tiff_tile_h as u32)?;

            // Composite using TIFF tile dimensions (the decoded buffer is that size)
            composite_tile(
                &decoded,
                tile_x,
                tile_y,
                tiff_tile_w,
                tiff_tile_h,
                &mut output,
                lx as i64,
                ly as i64,
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
        let ds = lvl.info.downsample;
        let lx = x as f64 / ds;
        let ly = y as f64 / ds;

        let tiles = lvl.grid.tiles_for_region(lx, ly, w as f64, h as f64);

        let out_len = (w * h) as usize;
        let mut output = vec![0u32; out_len];

        let tiff_mutex = self.tiff.as_ref().ok_or_else(|| {
            OpenSlideError::Decode("No TIFF data available for tile reading".into())
        })?;

        let grid_tile_w = lvl.grid.tile_w;
        let grid_tile_h = lvl.grid.tile_h;
        let tiff_tile_w = lvl.tiff_tile_width;
        let tiff_tile_h = lvl.tiff_tile_height;
        let tiles_across = lvl.grid.tiles_across();

        // Phase 1 (sync, under lock): resolve each tile's byte range. The mutex
        // guard is released before any await — parking_lot guards are not held
        // across suspension points.
        let mut placements: Vec<(i64, i64)> = Vec::with_capacity(tiles.len());
        let mut ranges: Vec<std::ops::Range<u64>> = Vec::with_capacity(tiles.len());
        {
            let mut tiff = tiff_mutex.lock();
            for (col, row) in tiles {
                let tile_index = (row * tiles_across + col) as usize;
                if let Some((offset, size)) = tiff.tile_location(lvl.tiff_dir, tile_index)? {
                    placements.push((col * grid_tile_w, row * grid_tile_h));
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
        for ((tile_x, tile_y), tile_data) in placements.into_iter().zip(tiles_data) {
            if tile_data.is_empty() {
                continue;
            }

            let decoded =
                lvl.decoder
                    .decode(tile_data.as_ref(), tiff_tile_w as u32, tiff_tile_h as u32)?;

            composite_tile(
                &decoded,
                tile_x,
                tile_y,
                tiff_tile_w,
                tiff_tile_h,
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

    /// Get the ICC color profile for the main slide image.
    pub fn icc_profile(&self) -> Option<&[u8]> {
        self.icc_profile.as_deref()
    }

    /// Get the ICC color profile for an associated image.
    pub fn associated_image_icc_profile(&self, name: &str) -> Option<&[u8]> {
        self.associated_images
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, img)| img.icc_profile.as_deref())
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

/// Composite a decoded tile into the output buffer.
///
/// `tile_pixels`: decoded ARGB pixels (tile_w * tile_h)
/// `tile_x, tile_y`: tile's top-left in level pixel space
/// `out_pixels`: output buffer (out_w * out_h)
/// `out_x, out_y`: output region's top-left in level pixel space
#[allow(clippy::too_many_arguments)]
fn composite_tile(
    tile_pixels: &[u32],
    tile_x: i64,
    tile_y: i64,
    tile_w: i64,
    tile_h: i64,
    out_pixels: &mut [u32],
    out_x: i64,
    out_y: i64,
    out_w: i64,
    out_h: i64,
) {
    // Compute overlap between tile and output region
    let src_x0 = (out_x - tile_x).max(0);
    let src_y0 = (out_y - tile_y).max(0);
    let dst_x0 = (tile_x - out_x).max(0);
    let dst_y0 = (tile_y - out_y).max(0);

    let copy_w = (tile_w - src_x0).min(out_w - dst_x0);
    let copy_h = (tile_h - src_y0).min(out_h - dst_y0);

    if copy_w <= 0 || copy_h <= 0 {
        return;
    }

    for row in 0..copy_h {
        let src_off = ((src_y0 + row) * tile_w + src_x0) as usize;
        let dst_off = ((dst_y0 + row) * out_w + dst_x0) as usize;
        let len = copy_w as usize;

        if src_off + len <= tile_pixels.len() && dst_off + len <= out_pixels.len() {
            out_pixels[dst_off..dst_off + len]
                .copy_from_slice(&tile_pixels[src_off..src_off + len]);
        }
    }
}
