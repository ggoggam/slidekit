//! 3DHistech MIRAX (.mrxs) vendor driver.
//!
//! MIRAX is not a single-file format. A `Foo.mrxs` file sits next to a `Foo/`
//! directory containing `Slidedat.ini` (an INI describing every zoom level,
//! data file, and metadata section), an index file (`Index.dat`), and a set of
//! `Data####.dat` files holding the actual image blobs. Each tile is a
//! standalone JPEG (or PNG/BMP) blob located by `(datafile, offset, length)`.
//!
//! Because there is no TIFF container, this driver produces [`MiraxLevel`]s
//! that own their own data-file readers and a per-tile placement list; the
//! read path (`slide.rs`) dispatches to [`MiraxLevel::read_region`] instead of
//! the TIFF tile path. Sibling files are resolved through the slide's
//! [`SlideFs`], so a MIRAX slide opens the same way on local disk or on blob
//! storage.
//!
//! The pyramid geometry follows OpenSlide's `mirax.c`: images are downsampled
//! by 2 and concatenated `image_divisions` at a time; once concatenation
//! crosses camera-photo boundaries, images are split back into subtiles placed
//! by an explicit per-camera-position map (`VIMSLIDE_POSITION_BUFFER`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use crate::associated::AssociatedImage;
use crate::cache::{LevelCache, TileId};
use crate::decode::TileDecoder;
use crate::decode::jpeg::JpegDecoder;
use crate::decode::png::PngDecoder;
use crate::decode::tiff::TiffLike;
use crate::error::{OpenSlideError, Result};
use crate::grid::simple::SimpleGrid;
use crate::icc::IccProfile;
use crate::io::SlideReader;
use crate::io::fs::{self, SlideFs, SlideLocation};
use crate::level::LevelInfo;
use crate::properties::{Properties, names};

use super::{LevelData, SlideData, VendorDriver};

/// Names of nonhier groups / values referenced by the driver (from `mirax.c`).
const GROUP_SCAN_DATA: &str = "Scan data layer";
const VAL_SLIDE_THUMBNAIL: &str = "ScanDataLayer_SlideThumbnail"; // → "macro"
const VAL_SLIDE_BARCODE: &str = "ScanDataLayer_SlideBarcode"; // → "label"
const VAL_SLIDE_PREVIEW: &str = "ScanDataLayer_SlidePreview"; // → "thumbnail"
const GROUP_POSITION_BUFFER: &str = "VIMSLIDE_POSITION_BUFFER";

const INDEX_VERSION: &[u8] = b"01.02";
const SLIDE_POSITION_RECORD_SIZE: usize = 9;

/// One placed subtile: a crop of a decoded image blob, positioned in this
/// level's pixel space.
struct MiraxTile {
    fileno: usize,
    offset: u64,
    length: usize,
    /// Crop rectangle within the decoded (`image_w` × `image_h`) blob.
    src_x: i64,
    src_y: i64,
    src_w: i64,
    src_h: i64,
    /// Top-left of the crop in this level's pixel space.
    dest_x: i64,
    dest_y: i64,
}

/// Per-level MIRAX tile index and the readers needed to fetch tile bytes.
pub struct MiraxLevel {
    /// Shared readers, one per `Data####.dat` file, indexed by fileno.
    readers: Arc<Vec<Arc<dyn SlideReader>>>,
    tiles: Vec<MiraxTile>,
    /// Decoded blob dimensions (constant per level; the crop stride).
    image_w: i64,
    image_h: i64,
}

impl MiraxLevel {
    /// Read a region of this level. `(ox, oy)` is the top-left in this level's
    /// pixel space; the decoder is the level's format decoder (JPEG).
    ///
    /// Every blob the region needs is located first and then fetched in one
    /// batch per data file, so neighbouring blobs coalesce into a handful of
    /// requests instead of a round trip each. MIRAX subtiles are small and
    /// scattered over several `Data####.dat` files, which made a patch read
    /// here the most request-hungry in the suite when it fetched them one at a
    /// time. Same three phases as [`DicomLevel::read_region`], the other
    /// multi-file format.
    ///
    /// `tiles` is the slide's decoded-tile cache, scoped to this level; blobs
    /// it already holds are neither fetched nor decoded again, and so never
    /// reach the gather.
    ///
    /// [`DicomLevel::read_region`]: crate::vendor::dicom::DicomLevel::read_region
    pub fn read_region(
        &self,
        decoder: &dyn TileDecoder,
        ox: i64,
        oy: i64,
        w: i64,
        h: i64,
        tiles: LevelCache<'_>,
    ) -> Result<Vec<u32>> {
        let mut out = vec![0u32; (w * h) as usize];
        // Decode each source blob at most once per call — at higher zoom levels
        // several subtiles are cropped from the same decoded image. This stays
        // even with the shared cache behind it: it keeps that guarantee when the
        // shared cache is disabled or too small to hold the region.
        let mut decoded: HashMap<(usize, u64), Arc<Vec<u32>>> = HashMap::new();

        // Phase 1: which subtiles the region touches, and which distinct blobs
        // back them. A blob shared by several subtiles — at higher zoom levels
        // several are crops of one decoded image — is fetched and decoded once.
        // Keyed by fileno because a reader spans one data file, so that is the
        // coarsest a batch can be; `BTreeMap` only to keep the request order
        // fixed from run to run.
        //
        // Phase 0 rides along: a blob the shared cache already holds is put
        // straight into `decoded` and left out of `wanted`, so a revisited
        // region issues no request at all.
        //
        // ponytail: linear scan over all placed tiles. Region reads touch few
        // tiles but we test every one; a per-level spatial bucket keyed on
        // tile_advance would cut this if reads get hot.
        let mut placed: Vec<&MiraxTile> = Vec::new();
        let mut seen: HashSet<(usize, u64)> = HashSet::new();
        let mut wanted: BTreeMap<usize, Vec<Range<u64>>> = BTreeMap::new();
        for t in &self.tiles {
            // Skip tiles whose destination rect doesn't meet the region.
            if t.dest_x + t.src_w <= ox
                || t.dest_x >= ox + w
                || t.dest_y + t.src_h <= oy
                || t.dest_y >= oy + h
            {
                continue;
            }
            if seen.insert((t.fileno, t.offset)) {
                if t.fileno >= self.readers.len() {
                    return Err(OpenSlideError::Decode(
                        "MIRAX tile fileno out of range".into(),
                    ));
                }
                // A MIRAX tile is a standalone blob, so `(fileno, offset)` —
                // not the grid position — is what names its pixels: several
                // placed subtiles crop the same blob, and the same offset in a
                // different `Data####.dat` is a different tile entirely.
                match tiles.get(TileId::Offset {
                    stream: t.fileno as u64,
                    position: t.offset,
                }) {
                    Some(px) => {
                        decoded.insert((t.fileno, t.offset), px);
                    }
                    None => wanted
                        .entry(t.fileno)
                        .or_default()
                        .push(t.offset..t.offset + t.length as u64),
                }
            }
            placed.push(t);
        }

        // Phase 2: one batched fetch per data file, for the blobs the cache
        // could not serve, decoding each as it arrives.
        for (fileno, ranges) in wanted {
            let blobs = self.readers[fileno].read_ranges(&ranges)?;
            for (range, blob) in ranges.iter().zip(blobs) {
                let px =
                    Arc::new(decoder.decode(&blob, self.image_w as u32, self.image_h as u32)?);
                tiles.insert(
                    TileId::Offset {
                        stream: fileno as u64,
                        position: range.start,
                    },
                    px.clone(),
                );
                decoded.insert((fileno, range.start), px);
            }
        }

        // Phase 3: composite in placement order — subtiles overlap, and the one
        // written last wins, so this must stay the order phase 1 walked.
        for t in placed {
            crate::slide::composite_subtile(
                &decoded[&(t.fileno, t.offset)],
                self.image_w,
                t.src_x,
                t.src_y,
                t.src_w,
                t.src_h,
                t.dest_x,
                t.dest_y,
                &mut out,
                ox,
                oy,
                w,
                h,
            );
        }
        Ok(out)
    }
}

pub struct MiraxDriver;

impl VendorDriver for MiraxDriver {
    fn name(&self) -> &str {
        "mirax"
    }

    fn vendor(&self) -> &str {
        "mirax"
    }

    fn detect(&self, reader: &dyn SlideReader, _tl: Option<&mut TiffLike>) -> Result<bool> {
        // MIRAX is defined by a `.mrxs` file with a sibling `Foo/Slidedat.ini`.
        let Some(loc) = reader.location() else {
            return Ok(false);
        };
        let Some(dir) = data_dir(loc) else {
            return Ok(false);
        };
        // One existence probe, and only for a `.mrxs` name — no cost to any
        // other format. `detect_format` reads an `Err` as "not this driver", so
        // a transient failure is reported as `false` here rather than aborting
        // detection for every remaining driver.
        Ok(loc
            .fs()
            .exists(&fs::join(&dir, "Slidedat.ini"))
            .unwrap_or(false))
    }

    fn open(&self, reader: Box<dyn SlideReader>, _tl: Option<TiffLike>) -> Result<SlideData> {
        let loc = reader.location().ok_or_else(|| {
            OpenSlideError::Other(
                "MIRAX slides span several files and need a filesystem to resolve them".into(),
            )
        })?;
        let dir = data_dir(loc).ok_or_else(|| OpenSlideError::Other("not a .mrxs path".into()))?;
        open_mirax(loc.fs(), &dir)
    }
}

/// `Foo.mrxs` → the `Foo/` data directory beside it.
///
/// The suffix is stripped by length rather than rebuilt from the lowercased
/// extension, so `FOO.MRXS` maps to `FOO/` and not `foo/`.
fn data_dir(loc: &SlideLocation) -> Option<String> {
    if loc.extension().as_deref() != Some("mrxs") {
        return None;
    }
    let path = loc.path();
    Some(path[..path.len() - ".mrxs".len()].to_string())
}

// ───────────────────────────── open ────────────────────────────────────────

fn open_mirax(fs: &Arc<dyn SlideFs>, dir: &str) -> Result<SlideData> {
    // One whole-object read rather than open-then-range: over blob storage that
    // is a single GET instead of a HEAD plus a GET.
    let ini_bytes = fs.read(&fs::join(dir, "Slidedat.ini"))?;
    let ini = Ini::parse(&ini_bytes);

    // [GENERAL]
    let images_x = ini.get_int("GENERAL", "IMAGENUMBER_X")?;
    let images_y = ini.get_int("GENERAL", "IMAGENUMBER_Y")?;
    if images_x <= 0 || images_y <= 0 {
        return Err(OpenSlideError::Decode(
            "MIRAX images_x/y not positive".into(),
        ));
    }
    // Defaults to 1 when absent (each camera photo split into divisions² images).
    let image_divisions = ini
        .get_int("GENERAL", "CameraImageDivisionsPerSide")
        .unwrap_or(1)
        .max(1);
    let slide_id = ini
        .get_str("GENERAL", "SLIDE_ID")
        .ok_or_else(|| OpenSlideError::Decode("MIRAX missing SLIDE_ID".into()))?
        .to_string();

    // [HIERARCHICAL] — locate the "Slide zoom level" hierarchy (must be HIER_0).
    if ini.get_str("HIERARCHICAL", "HIER_0_NAME") != Some("Slide zoom level") {
        return Err(OpenSlideError::Decode(
            "MIRAX: HIER_0 is not the slide zoom level".into(),
        ));
    }
    let zoom_levels = ini.get_int("HIERARCHICAL", "HIER_0_COUNT")? as usize;
    if zoom_levels == 0 {
        return Err(OpenSlideError::Decode("MIRAX: zero zoom levels".into()));
    }
    let index_name = ini
        .get_str("HIERARCHICAL", "INDEXFILE")
        .ok_or_else(|| OpenSlideError::Decode("MIRAX missing INDEXFILE".into()))?
        .to_string();

    // [DATAFILE] — index → filename, opened as shared readers.
    //
    // One directory listing gives every data file's size up front, so each
    // reader is built with `open_sized` and skips its own metadata lookup. On
    // blob storage that collapses FILE_COUNT round trips into one; locally it
    // is the same `read_dir` the open would have done anyway.
    let file_count = ini.get_int("DATAFILE", "FILE_COUNT")? as usize;
    let sizes: HashMap<String, u64> = fs
        .list(dir)
        .unwrap_or_default()
        .into_iter()
        .map(|e| (e.file_name().to_string(), e.size))
        .collect();

    let mut readers: Vec<Arc<dyn SlideReader>> = Vec::with_capacity(file_count);
    for i in 0..file_count {
        let name = ini
            .get_str("DATAFILE", &format!("FILE_{i}"))
            .ok_or_else(|| OpenSlideError::Decode(format!("MIRAX missing FILE_{i}")))?;
        let path = fs::join(dir, name);
        // Fall back to a plain open when the listing didn't cover this name —
        // the file may still exist (an unlistable store, an odd layout).
        let reader = match sizes.get(name) {
            Some(&size) => fs.open_sized(&path, size)?,
            None => fs.open(&path)?,
        };
        readers.push(reader);
    }
    let readers = Arc::new(readers);

    // Per-level sections.
    let mut sections = Vec::with_capacity(zoom_levels);
    for j in 0..zoom_levels {
        let section = ini
            .get_str("HIERARCHICAL", &format!("HIER_0_VAL_{j}_SECTION"))
            .ok_or_else(|| {
                OpenSlideError::Decode(format!("MIRAX missing HIER_0_VAL_{j}_SECTION"))
            })?;
        sections.push(ZoomSection::parse(&ini, section)?);
    }

    // Base (level-0) dimensions: overlap is subtracted only at internal
    // division-block boundaries (see mirax.c).
    let (base_w, base_h) = base_dimensions(&sections[0], images_x, images_y, image_divisions);

    // Per-level derived params.
    let params = level_params(&sections, image_divisions, base_w, base_h)?;

    // Camera-position map (level-0 pixel coords), one (x,y) per photo.
    let npositions = (images_x / image_divisions) * (images_y / image_divisions);
    let index_bytes = fs.read(&fs::join(dir, &index_name))?;
    let index = IndexFile::parse(&index_bytes, &slide_id)?;
    let positions = read_positions(
        &ini,
        &index,
        &readers,
        npositions as usize,
        &sections[0],
        images_x,
        image_divisions,
    )?;

    // Walk the hierarchical data pages into per-level tile lists.
    let per_level_tiles = index.build_tiles(
        &params,
        &positions,
        images_x,
        images_y,
        image_divisions,
        sections[0].image_w,
        sections[0].image_h,
    )?;

    // Assemble levels.
    let mut levels = Vec::with_capacity(zoom_levels);
    for (j, (p, tiles)) in params.iter().zip(per_level_tiles).enumerate() {
        // Each zoom level names its own tile codec in the INI. BMP levels are
        // rare and still undecodable; fail clearly rather than return black
        // tiles.
        let format = sections[j].format.as_str();
        let decoder: Box<dyn TileDecoder> = if format.eq_ignore_ascii_case("JPEG") {
            Box::new(JpegDecoder::new())
        } else if format.eq_ignore_ascii_case("PNG") {
            Box::new(PngDecoder::new())
        } else {
            return Err(OpenSlideError::Decode(format!(
                "MIRAX level {j} image format {format:?} unsupported (only JPEG and PNG)"
            )));
        };
        let advance_w = (p.advance_x.round() as i64).max(1);
        let advance_h = (p.advance_y.round() as i64).max(1);
        levels.push(LevelData {
            info: LevelInfo {
                width: p.level_w,
                height: p.level_h,
                downsample: p.downsample,
                tile_width: advance_w,
                tile_height: advance_h,
            },
            grid: SimpleGrid::new(p.level_w, p.level_h, advance_w, advance_h),
            tiff_dir: 0,
            decoder,
            tiff_tile_width: sections[0].image_w,
            tiff_tile_height: sections[0].image_h,
            tilemap: None,
            ndpi: None,
            mirax: Some(MiraxLevel {
                readers: readers.clone(),
                tiles,
                image_w: sections[0].image_w,
                image_h: sections[0].image_h,
            }),
            zeiss: None,
            synthetic: None,
            dicom: None,
        });
    }

    let properties = build_properties(&ini, &sections[0]);
    let associated_images = read_associated_images(&ini, &index, &readers);

    Ok(SlideData {
        levels,
        properties,
        associated_images,
        icc_profile: IccProfile::absent(),
        tiff: None,
    })
}

/// Level-0 dimensions, summed the "stupid but clear" way from `mirax.c`.
fn base_dimensions(
    s0: &ZoomSection,
    images_x: i64,
    images_y: i64,
    image_divisions: i64,
) -> (i64, i64) {
    let mut base_w = 0i64;
    for i in 0..images_x {
        if i % image_divisions != image_divisions - 1 || i == images_x - 1 {
            base_w += s0.image_w;
        } else {
            base_w += s0.image_w - s0.overlap_x as i64;
        }
    }
    let mut base_h = 0i64;
    for i in 0..images_y {
        if i % image_divisions != image_divisions - 1 || i == images_y - 1 {
            base_h += s0.image_h;
        } else {
            base_h += s0.image_h - s0.overlap_y as i64;
        }
    }
    (base_w, base_h)
}

/// Per-zoom-level parameters (concat factor, tile geometry, advances, dims).
struct LevelParams {
    image_concat: i64,
    tiles_per_image: i64,
    positions_per_tile: i64,
    tile_w: f64,
    tile_h: f64,
    advance_x: f64,
    advance_y: f64,
    level_w: i64,
    level_h: i64,
    downsample: f64,
}

fn level_params(
    sections: &[ZoomSection],
    image_divisions: i64,
    base_w: i64,
    base_h: i64,
) -> Result<Vec<LevelParams>> {
    let mut out = Vec::with_capacity(sections.len());
    let mut total_exp = 0i32;
    let mut concat0 = 1i64;
    for (i, s) in sections.iter().enumerate() {
        total_exp += s.concat_exponent;
        if total_exp > 30 {
            return Err(OpenSlideError::Decode(
                "MIRAX image_concat exponent too large".into(),
            ));
        }
        let image_concat = 1i64 << total_exp;
        if i == 0 {
            concat0 = image_concat;
        }
        let positions_per_image = (image_concat / image_divisions).max(1);

        // Subtile path when explicit positions or overlaps exist; otherwise the
        // fast whole-image path. CMU-1-style slides always have overlaps.
        // tile_count_divisor (the C tilemap grid key divisor) is unneeded here:
        // we place tiles by pixel destination, not grid coordinate.
        let (tiles_per_image, positions_per_tile) =
            if sections[0].overlap_x != 0.0 || sections[0].overlap_y != 0.0 {
                (positions_per_image, 1)
            } else {
                (1, positions_per_image)
            };

        let tile_w = s.image_w as f64 / tiles_per_image as f64;
        let tile_h = s.image_h as f64 / tiles_per_image as f64;
        let images_per_position = (image_divisions / image_concat).max(1);
        let advance_x = tile_w - s.overlap_x / images_per_position as f64;
        let advance_y = tile_h - s.overlap_y / images_per_position as f64;

        out.push(LevelParams {
            image_concat,
            tiles_per_image,
            positions_per_tile,
            tile_w,
            tile_h,
            advance_x,
            advance_y,
            level_w: base_w / image_concat,
            level_h: base_h / image_concat,
            // Level 0 is defined as downsample 1.0 regardless of its exponent.
            downsample: image_concat as f64 / concat0 as f64,
        });
    }
    Ok(out)
}

// ─────────────────────────── index file ─────────────────────────────────────

/// Parsed `Index.dat`: the raw bytes plus the two root pointers.
struct IndexFile<'a> {
    data: &'a [u8],
    hier_root: i64,
    nonhier_root: i64,
}

impl<'a> IndexFile<'a> {
    fn parse(data: &'a [u8], slide_id: &str) -> Result<Self> {
        if data.len() < INDEX_VERSION.len() + slide_id.len() + 8 {
            return Err(OpenSlideError::Decode("MIRAX Index.dat too short".into()));
        }
        if &data[..INDEX_VERSION.len()] != INDEX_VERSION {
            return Err(OpenSlideError::Decode("MIRAX Index.dat bad version".into()));
        }
        let uuid = &data[INDEX_VERSION.len()..INDEX_VERSION.len() + slide_id.len()];
        if uuid != slide_id.as_bytes() {
            return Err(OpenSlideError::Decode(
                "MIRAX Index.dat slide id mismatch".into(),
            ));
        }
        let base = INDEX_VERSION.len() + slide_id.len();
        let hier_root = i32le(data, base)? as i64;
        let nonhier_root = i32le(data, base + 4)? as i64;
        Ok(Self {
            data,
            hier_root,
            nonhier_root,
        })
    }

    /// Read a nonhier record's `(offset, size, fileno)` by its flat record index.
    fn nonhier_record(&self, recordno: i64) -> Result<(u64, usize, usize)> {
        // recptr = i32(nonhier_root + 4*recordno); at recptr: [0, datapage_ptr]
        let recptr = i32le(self.data, (self.nonhier_root + 4 * recordno) as usize)? as usize;
        if i32le(self.data, recptr)? != 0 {
            return Err(OpenSlideError::Decode(
                "MIRAX nonhier: expected 0 marker".into(),
            ));
        }
        let dp = i32le(self.data, recptr + 4)? as usize;
        let pagesize = i32le(self.data, dp)?;
        if pagesize < 1 {
            return Err(OpenSlideError::Decode("MIRAX nonhier: empty page".into()));
        }
        // dp: [pagesize, next, 0, 0, offset, size, fileno]
        let offset = i32le(self.data, dp + 16)? as u64;
        let size = i32le(self.data, dp + 20)? as usize;
        let fileno = i32le(self.data, dp + 24)? as usize;
        Ok((offset, size, fileno))
    }

    /// Walk the hierarchical data pages, expanding each stored image into
    /// placed subtiles per level. Levels are processed in order so that
    /// `active_positions` (built at level 0) filters higher levels.
    #[allow(clippy::too_many_arguments)]
    fn build_tiles(
        &self,
        params: &[LevelParams],
        positions: &[i32],
        images_x: i64,
        images_y: i64,
        image_divisions: i64,
        image0_w: i64,
        image0_h: i64,
    ) -> Result<Vec<Vec<MiraxTile>>> {
        let mut per_level = Vec::with_capacity(params.len());
        let mut active: HashSet<i64> = HashSet::new();
        let positions_across = images_x / image_divisions;

        for (zoom, lp) in params.iter().enumerate() {
            let mut tiles: Vec<MiraxTile> = Vec::new();

            // Per-level pointer, then [0, datapage_ptr].
            let level_ptr = i32le(self.data, (self.hier_root + 4 * zoom as i64) as usize)? as usize;
            if i32le(self.data, level_ptr)? != 0 {
                return Err(OpenSlideError::Decode(
                    "MIRAX hier: expected 0 at data page start".into(),
                ));
            }
            let mut page = i32le(self.data, level_ptr + 4)? as usize;

            loop {
                let page_len = i32le(self.data, page)?;
                let next_ptr = i32le(self.data, page + 4)? as usize;
                let mut cur = page + 8;
                for _ in 0..page_len {
                    let image_index = i32le(self.data, cur)? as i64;
                    let offset = i32le(self.data, cur + 4)? as u64;
                    let length = i32le(self.data, cur + 8)? as usize;
                    let fileno = i32le(self.data, cur + 12)? as usize;
                    cur += 16;
                    if image_index < 0 {
                        continue;
                    }
                    let x = image_index % images_x;
                    let y = image_index / images_x;
                    if y >= images_y || x % lp.image_concat != 0 || y % lp.image_concat != 0 {
                        // Power-of-two grid violated; skip defensively.
                        continue;
                    }

                    // One image → tiles_per_image² subtiles.
                    for yi in 0..lp.tiles_per_image {
                        let yy = y + yi * image_divisions;
                        if yy >= images_y {
                            break;
                        }
                        for xi in 0..lp.tiles_per_image {
                            let xx = x + xi * image_divisions;
                            if xx >= images_x {
                                break;
                            }
                            let Some((pos0_x, pos0_y)) = tile_position(
                                positions,
                                &mut active,
                                positions_across,
                                image_divisions,
                                lp.positions_per_tile,
                                zoom,
                                xx,
                                yy,
                                image0_w,
                                image0_h,
                            ) else {
                                continue;
                            };
                            let pos_x = pos0_x as f64 / lp.image_concat as f64;
                            let pos_y = pos0_y as f64 / lp.image_concat as f64;

                            // Integer crop boundaries so subtiles of one blob
                            // tile without gaps despite fractional tile_w/h.
                            let sx0 = (lp.tile_w * xi as f64).round() as i64;
                            let sx1 = ((lp.tile_w * (xi + 1) as f64).round() as i64).min(image0_w);
                            let sy0 = (lp.tile_h * yi as f64).round() as i64;
                            let sy1 = ((lp.tile_h * (yi + 1) as f64).round() as i64).min(image0_h);
                            if sx1 <= sx0 || sy1 <= sy0 {
                                continue;
                            }
                            tiles.push(MiraxTile {
                                fileno,
                                offset,
                                length,
                                src_x: sx0,
                                src_y: sy0,
                                src_w: sx1 - sx0,
                                src_h: sy1 - sy0,
                                dest_x: pos_x.round() as i64,
                                dest_y: pos_y.round() as i64,
                            });
                        }
                    }
                }
                if next_ptr == 0 {
                    break;
                }
                page = next_ptr;
            }

            per_level.push(tiles);
        }
        Ok(per_level)
    }
}

/// Level-0 pixel coords for sub-image `(xx, yy)`, or `None` if its camera
/// position is inactive. Mirrors `get_tile_position` in `mirax.c`.
#[allow(clippy::too_many_arguments)]
fn tile_position(
    positions: &[i32],
    active: &mut HashSet<i64>,
    positions_across: i64,
    image_divisions: i64,
    positions_per_tile: i64,
    zoom: usize,
    xx: i64,
    yy: i64,
    image0_w: i64,
    image0_h: i64,
) -> Option<(i64, i64)> {
    let xp = xx / image_divisions;
    let yp = yy / image_divisions;
    let cp = yp * positions_across + xp;
    let idx = (cp * 2) as usize;
    if idx + 1 >= positions.len() {
        return None;
    }
    let pos0_x = positions[idx] as i64 + image0_w * (xx - xp * image_divisions);
    let pos0_y = positions[idx + 1] as i64 + image0_h * (yy - yp * image_divisions);

    if zoom == 0 {
        // A (0,0) position that isn't the origin is inactive; skip it (and it
        // would break the grid's range search).
        if positions[idx] == 0 && positions[idx + 1] == 0 && (xp != 0 || yp != 0) {
            return None;
        }
        active.insert(cp);
        Some((pos0_x, pos0_y))
    } else {
        // Keep the tile only if one covered camera position was active at L0.
        for ypp in yp..yp + positions_per_tile {
            for xpp in xp..xp + positions_per_tile {
                if active.contains(&(ypp * positions_across + xpp)) {
                    return Some((pos0_x, pos0_y));
                }
            }
        }
        None
    }
}

// ─────────────────────────── positions ──────────────────────────────────────

/// Read the camera-position map (level-0 pixel coords), preferring the
/// uncompressed `VIMSLIDE_POSITION_BUFFER`; synthesize a regular grid if no
/// position record exists.
#[allow(clippy::too_many_arguments)]
fn read_positions(
    ini: &Ini,
    index: &IndexFile,
    readers: &[Arc<dyn SlideReader>],
    npositions: usize,
    s0: &ZoomSection,
    images_x: i64,
    image_divisions: i64,
) -> Result<Vec<i32>> {
    if let Some(record) = ini.nonhier_record_by_name(GROUP_POSITION_BUFFER) {
        let (offset, size, fileno) = index.nonhier_record(record)?;
        if size == npositions * SLIDE_POSITION_RECORD_SIZE {
            let reader = readers.get(fileno).ok_or_else(|| {
                OpenSlideError::Decode("MIRAX position fileno out of range".into())
            })?;
            let mut buf = vec![0u8; size];
            reader.read_exact_at(offset, &mut buf)?;
            return parse_position_buffer(&buf, npositions);
        }
    }
    // Fallback: regular grid (no per-position deviations).
    let positions_x = (images_x / image_divisions) as usize;
    let step_x = s0.image_w * image_divisions - s0.overlap_x as i64;
    let step_y = s0.image_h * image_divisions - s0.overlap_y as i64;
    let mut out = vec![0i32; npositions * 2];
    for i in 0..npositions {
        out[i * 2] = ((i % positions_x) as i64 * step_x) as i32;
        out[i * 2 + 1] = ((i / positions_x) as i64 * step_y) as i32;
    }
    Ok(out)
}

fn parse_position_buffer(buf: &[u8], npositions: usize) -> Result<Vec<i32>> {
    let mut out = vec![0i32; npositions * 2];
    for i in 0..npositions {
        let rec = &buf[i * SLIDE_POSITION_RECORD_SIZE..];
        // Only bit 0 of the flag byte is allowed.
        if rec[0] & 0xFE != 0 {
            return Err(OpenSlideError::Decode(
                "MIRAX position: bad flag byte".into(),
            ));
        }
        let x = i32::from_le_bytes([rec[1], rec[2], rec[3], rec[4]]);
        let y = i32::from_le_bytes([rec[5], rec[6], rec[7], rec[8]]);
        out[i * 2] = x;
        out[i * 2 + 1] = y;
    }
    Ok(out)
}

// ─────────────────────── properties & associated ────────────────────────────

fn build_properties(ini: &Ini, s0: &ZoomSection) -> Properties {
    let mut props = Properties::new();
    props.insert(names::VENDOR, "mirax");

    // Dump every INI group/key as mirax.<GROUP>.<KEY>.
    for (section, entries) in &ini.sections {
        for (k, v) in entries {
            props.insert(format!("mirax.{section}.{k}"), v.clone());
        }
    }

    if let Some(obj) = ini.get_str("GENERAL", "OBJECTIVE_MAGNIFICATION") {
        let obj = obj.trim().trim_end_matches(['x', 'X']);
        if !obj.is_empty() {
            props.insert(names::OBJECTIVE_POWER, obj);
        }
    }
    if s0.mpp_x > 0.0 {
        props.insert(names::MPP_X, s0.mpp_x.to_string());
    }
    if s0.mpp_y > 0.0 {
        props.insert(names::MPP_Y, s0.mpp_y.to_string());
    }
    // IMAGE_FILL_COLOR_BGR → RGB hex.
    if s0.fill_bgr >= 0 {
        let bgr = s0.fill_bgr;
        let r = (bgr >> 16) & 0xFF;
        let g = (bgr >> 8) & 0xFF;
        let b = bgr & 0xFF;
        props.insert(names::BACKGROUND_COLOR, format!("{b:02X}{g:02X}{r:02X}"));
    }
    props
}

/// Decode the macro/label/thumbnail associated images (JPEG only).
fn read_associated_images(
    ini: &Ini,
    index: &IndexFile,
    readers: &[Arc<dyn SlideReader>],
) -> Vec<(String, AssociatedImage)> {
    let mut out = Vec::new();
    // OpenSlide's naming inversion: SlideThumbnail→macro, SlidePreview→thumbnail.
    for (value, name) in [
        (VAL_SLIDE_THUMBNAIL, "macro"),
        (VAL_SLIDE_BARCODE, "label"),
        (VAL_SLIDE_PREVIEW, "thumbnail"),
    ] {
        if let Some(img) = read_one_associated(ini, index, readers, value) {
            out.push((name.to_string(), img));
        }
    }
    out
}

fn read_one_associated(
    ini: &Ini,
    index: &IndexFile,
    readers: &[Arc<dyn SlideReader>],
    value: &str,
) -> Option<AssociatedImage> {
    let (record, section) = ini.nonhier_record_and_section(GROUP_SCAN_DATA, value)?;
    // Dimensions and format live in the value's own section.
    let fmt = ini.get_str_ending(&section, "_IMAGE_TYPE")?;
    if !fmt.eq_ignore_ascii_case("JPEG") {
        return None; // only JPEG associated images decode today
    }
    let w = ini.get_int_ending(&section, "_IMAGE_WIDTH")?;
    let h = ini.get_int_ending(&section, "_IMAGE_HEIGHT")?;
    let (offset, size, fileno) = index.nonhier_record(record).ok()?;
    let reader = readers.get(fileno)?;
    let mut buf = vec![0u8; size];
    reader.read_exact_at(offset, &mut buf).ok()?;
    let argb = JpegDecoder::new().decode(&buf, w as u32, h as u32).ok()?;
    Some(AssociatedImage {
        width: w,
        height: h,
        argb_data: argb,
        icc_profile: IccProfile::absent(),
    })
}

// ───────────────────────────── ZoomSection ──────────────────────────────────

struct ZoomSection {
    concat_exponent: i32,
    overlap_x: f64,
    overlap_y: f64,
    mpp_x: f64,
    mpp_y: f64,
    fill_bgr: i64,
    image_w: i64,
    image_h: i64,
    format: String,
}

impl ZoomSection {
    fn parse(ini: &Ini, section: &str) -> Result<Self> {
        let image_w = ini.get_int(section, "DIGITIZER_WIDTH")?;
        let image_h = ini.get_int(section, "DIGITIZER_HEIGHT")?;
        if image_w <= 0 || image_h <= 0 {
            return Err(OpenSlideError::Decode("MIRAX bad DIGITIZER size".into()));
        }
        Ok(Self {
            concat_exponent: ini.get_int(section, "IMAGE_CONCAT_FACTOR").unwrap_or(0) as i32,
            overlap_x: ini.get_f64(section, "OVERLAP_X").unwrap_or(0.0),
            overlap_y: ini.get_f64(section, "OVERLAP_Y").unwrap_or(0.0),
            mpp_x: ini
                .get_f64(section, "MICROMETER_PER_PIXEL_X")
                .unwrap_or(0.0),
            mpp_y: ini
                .get_f64(section, "MICROMETER_PER_PIXEL_Y")
                .unwrap_or(0.0),
            fill_bgr: ini.get_int(section, "IMAGE_FILL_COLOR_BGR").unwrap_or(-1),
            image_w,
            image_h,
            format: ini
                .get_str(section, "IMAGE_FORMAT")
                .unwrap_or("JPEG")
                .to_string(),
        })
    }
}

// ─────────────────────────────── INI ────────────────────────────────────────

/// Minimal INI reader preserving group/key order. MIRAX `Slidedat.ini` is a
/// flat `[GROUP]` / `KEY = VALUE` file.
struct Ini {
    sections: Vec<(String, Vec<(String, String)>)>,
}

impl Ini {
    fn parse(bytes: &[u8]) -> Self {
        // MIRAX INIs are ASCII in practice; lossy-decode tolerates stray bytes.
        let text = String::from_utf8_lossy(bytes);
        let mut sections: Vec<(String, Vec<(String, String)>)> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                sections.push((name.trim().to_string(), Vec::new()));
            } else if let Some((k, v)) = line.split_once('=')
                && let Some((_, entries)) = sections.last_mut()
            {
                entries.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        Self { sections }
    }

    fn section(&self, name: &str) -> Option<&Vec<(String, String)>> {
        self.sections
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, e)| e)
    }

    fn get_str(&self, section: &str, key: &str) -> Option<&str> {
        self.section(section)?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// First value whose key ends with `suffix` (handles the type-prefixed
    /// `*_IMAGE_WIDTH`/`_TYPE` keys in associated-image sections).
    fn get_str_ending(&self, section: &str, suffix: &str) -> Option<&str> {
        self.section(section)?
            .iter()
            .find(|(k, _)| k.ends_with(suffix))
            .map(|(_, v)| v.as_str())
    }

    fn get_int(&self, section: &str, key: &str) -> Result<i64> {
        self.get_str(section, key)
            .and_then(|v| v.trim().parse::<i64>().ok())
            .ok_or_else(|| {
                OpenSlideError::Decode(format!("MIRAX missing/invalid [{section}] {key}"))
            })
    }

    fn get_int_ending(&self, section: &str, suffix: &str) -> Option<i64> {
        self.get_str_ending(section, suffix)?.trim().parse().ok()
    }

    fn get_f64(&self, section: &str, key: &str) -> Option<f64> {
        self.get_str(section, key)?.trim().parse().ok()
    }

    /// Flat record index of a nonhier group's first value (`VAL_0`), by name.
    fn nonhier_record_by_name(&self, group_name: &str) -> Option<i64> {
        let count = self.get_int("HIERARCHICAL", "NONHIER_COUNT").ok()?;
        let mut offset = 0i64;
        for i in 0..count {
            if self.get_str("HIERARCHICAL", &format!("NONHIER_{i}_NAME")) == Some(group_name) {
                return Some(offset);
            }
            offset += self
                .get_int("HIERARCHICAL", &format!("NONHIER_{i}_COUNT"))
                .unwrap_or(0);
        }
        None
    }

    /// Flat record index and `_SECTION` of a value within a named nonhier group.
    fn nonhier_record_and_section(
        &self,
        group_name: &str,
        value_name: &str,
    ) -> Option<(i64, String)> {
        let count = self.get_int("HIERARCHICAL", "NONHIER_COUNT").ok()?;
        let mut offset = 0i64;
        for i in 0..count {
            let this_count = self
                .get_int("HIERARCHICAL", &format!("NONHIER_{i}_COUNT"))
                .unwrap_or(0);
            if self.get_str("HIERARCHICAL", &format!("NONHIER_{i}_NAME")) == Some(group_name) {
                for j in 0..this_count {
                    if self.get_str("HIERARCHICAL", &format!("NONHIER_{i}_VAL_{j}"))
                        == Some(value_name)
                    {
                        let section = self
                            .get_str("HIERARCHICAL", &format!("NONHIER_{i}_VAL_{j}_SECTION"))?
                            .to_string();
                        return Some((offset + j, section));
                    }
                }
                return None;
            }
            offset += this_count;
        }
        None
    }
}

/// Read a little-endian i32 at `off`, erroring on a short buffer.
fn i32le(buf: &[u8], off: usize) -> Result<i32> {
    buf.get(off..off + 4)
        .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| OpenSlideError::Decode("MIRAX index: read past end".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{Cache, SlideId};
    use bytes::Bytes;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A disabled cache, so the request-count assertions below measure the
    /// gather itself rather than a cache hit standing in for it. The cached
    /// path has its own coverage in `tests/test_cache.rs`.
    fn no_cache() -> Cache {
        Cache::new(0)
    }

    /// What a region read cost, shared by the fake reader and decoder below.
    #[derive(Default)]
    struct Counts {
        /// Single positioned reads — one per tile is the shape this path used
        /// to have, and the one the batched gather replaces.
        reads: AtomicUsize,
        /// Batched gathers: the call a remote backend can coalesce.
        batches: AtomicUsize,
        decodes: AtomicUsize,
    }

    /// One in-memory `Data####.dat`, counting what reaches it.
    struct Counting {
        data: Vec<u8>,
        counts: Arc<Counts>,
    }

    impl SlideReader for Counting {
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
            Ok(ranges
                .iter()
                .map(|r| Bytes::copy_from_slice(&self.data[r.start as usize..r.end as usize]))
                .collect())
        }
    }

    /// Decodes a blob to a solid image of its first byte, so a composited pixel
    /// names the blob it came from, and counts decodes so the test can see a
    /// blob shared by two subtiles being decoded once.
    struct FirstByteDecoder(Arc<Counts>);

    impl TileDecoder for FirstByteDecoder {
        fn decode(&self, data: &[u8], width: u32, height: u32) -> Result<Vec<u32>> {
            self.0.decodes.fetch_add(1, Ordering::Relaxed);
            Ok(vec![data[0] as u32; (width * height) as usize])
        }
    }

    #[test]
    fn region_read_costs_one_gather_per_data_file() {
        let counts = Arc::new(Counts::default());
        let file = |data: Vec<u8>| -> Arc<dyn SlideReader> {
            Arc::new(Counting {
                data,
                counts: counts.clone(),
            })
        };
        // File 0 holds two blobs (at 0 and 8), file 1 holds one.
        let readers = vec![
            file(vec![10, 0, 0, 0, 0, 0, 0, 0, 20, 0, 0, 0]),
            file(vec![30, 0, 0, 0]),
        ];

        let tile = |fileno, offset, dest_x, dest_y| MiraxTile {
            fileno,
            offset,
            length: 4,
            src_x: 0,
            src_y: 0,
            src_w: 2,
            src_h: 2,
            dest_x,
            dest_y,
        };
        let level = MiraxLevel {
            readers: Arc::new(readers),
            // The first two subtiles are crops of one blob, as they are at the
            // zoom levels where several tiles share a decoded image.
            tiles: vec![
                tile(0, 0, 0, 0),
                tile(0, 0, 2, 0),
                tile(0, 8, 0, 2),
                tile(1, 0, 2, 2),
            ],
            image_w: 2,
            image_h: 2,
        };

        let cache = no_cache();
        let out = level
            .read_region(
                &FirstByteDecoder(counts.clone()),
                0,
                0,
                4,
                4,
                cache.level(SlideId::next(), 0),
            )
            .expect("read");

        #[rustfmt::skip]
        assert_eq!(
            out,
            vec![
                10, 10, 10, 10,
                10, 10, 10, 10,
                20, 20, 30, 30,
                20, 20, 30, 30,
            ]
        );
        assert_eq!(
            counts.batches.load(Ordering::Relaxed),
            2,
            "four subtiles over two data files should cost one gather each"
        );
        assert_eq!(
            counts.reads.load(Ordering::Relaxed),
            0,
            "no tile may be fetched with its own positioned read"
        );
        assert_eq!(
            counts.decodes.load(Ordering::Relaxed),
            3,
            "the blob two subtiles share is decoded once"
        );
    }

    #[test]
    fn region_touching_no_tile_reads_nothing() {
        let counts = Arc::new(Counts::default());
        let level = MiraxLevel {
            readers: Arc::new(vec![Arc::new(Counting {
                data: vec![10, 0, 0, 0],
                counts: counts.clone(),
            }) as Arc<dyn SlideReader>]),
            tiles: vec![MiraxTile {
                fileno: 0,
                offset: 0,
                length: 4,
                src_x: 0,
                src_y: 0,
                src_w: 2,
                src_h: 2,
                dest_x: 0,
                dest_y: 0,
            }],
            image_w: 2,
            image_h: 2,
        };

        let cache = no_cache();
        let out = level
            .read_region(
                &FirstByteDecoder(counts.clone()),
                100,
                100,
                4,
                4,
                cache.level(SlideId::next(), 0),
            )
            .expect("read");
        assert!(out.iter().all(|&p| p == 0));
        assert_eq!(counts.batches.load(Ordering::Relaxed), 0);
        assert_eq!(counts.reads.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn ini_parses_sections_and_keys() {
        let ini = Ini::parse(
            b"[GENERAL]\nIMAGENUMBER_X = 352\nSLIDE_ID = ABC\n[DATAFILE]\nFILE_0 = Data0000.dat\n",
        );
        assert_eq!(ini.get_int("GENERAL", "IMAGENUMBER_X").unwrap(), 352);
        assert_eq!(ini.get_str("GENERAL", "SLIDE_ID"), Some("ABC"));
        assert_eq!(ini.get_str("DATAFILE", "FILE_0"), Some("Data0000.dat"));
        assert!(ini.get_str("GENERAL", "MISSING").is_none());
    }

    #[test]
    fn nonhier_offsets_accumulate() {
        // Two groups: A(count 10), B(count 1) → B's VAL_0 is record 10.
        let ini = Ini::parse(
            b"[HIERARCHICAL]\nNONHIER_COUNT = 2\n\
              NONHIER_0_NAME = A\nNONHIER_0_COUNT = 10\n\
              NONHIER_0_VAL_2 = target\nNONHIER_0_VAL_2_SECTION = SEC\n\
              NONHIER_1_NAME = B\nNONHIER_1_COUNT = 1\n",
        );
        assert_eq!(ini.nonhier_record_by_name("B"), Some(10));
        assert_eq!(
            ini.nonhier_record_and_section("A", "target"),
            Some((2, "SEC".to_string()))
        );
    }

    #[test]
    fn base_dimensions_subtract_overlap_at_block_boundaries() {
        // divisions=1 → overlap subtracted at every column except the last.
        let s0 = ZoomSection {
            concat_exponent: 0,
            overlap_x: 10.0,
            overlap_y: 10.0,
            mpp_x: 0.0,
            mpp_y: 0.0,
            fill_bgr: -1,
            image_w: 100,
            image_h: 100,
            format: "JPEG".into(),
        };
        // 3 columns of 100, overlap 10 at cols 0,1 (col 2 is last): 100+90+90=280? no—
        // divisions=1: i%1==0==(1-1) always, so every non-last column subtracts.
        let (w, _h) = base_dimensions(&s0, 3, 1, 1);
        assert_eq!(w, 90 + 90 + 100);
    }

    #[test]
    fn tile_position_marks_active_and_places() {
        // 2x2 camera positions (divisions=1), a single scanned position at (5,5).
        let mut positions = vec![0i32; 4 * 2];
        positions[0] = 5;
        positions[1] = 5;
        let mut active = HashSet::new();
        // level 0: origin position (cp=0) is active, returns its coord.
        let p = tile_position(&positions, &mut active, 2, 1, 1, 0, 0, 0, 100, 100);
        assert_eq!(p, Some((5, 5)));
        assert!(active.contains(&0));
        // an inactive (0,0) non-origin position at level 0 is skipped.
        let p = tile_position(&positions, &mut active, 2, 1, 1, 0, 1, 0, 100, 100);
        assert_eq!(p, None);
    }
}
