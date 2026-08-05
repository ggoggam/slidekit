//! DICOM WSI (VL Whole Slide Microscopy Image) vendor driver.
//!
//! A DICOM slide is a *directory*, not a file: each pyramid level and each
//! associated image is its own SOP instance (`.dcm` file), and they belong
//! together because they share a SeriesInstanceUID. Opening any one of them
//! opens the slide — this driver scans the containing directory, keeps the
//! files whose series matches, and sorts them into levels (`ImageType[2]` =
//! `VOLUME`) and associated images (`LABEL` → label, `OVERVIEW` → macro,
//! `THUMBNAIL` → thumbnail), ignoring flavors it doesn't understand. The scan
//! runs through the slide's [`SlideFs`], so a DICOM slide opens the same way on
//! local disk or on blob storage, and it is fanned out across threads because
//! each instance costs several round trips.
//!
//! Within a level, tiles are *frames* of one multi-frame instance. Two layouts
//! exist: `TILED_FULL`, where frame *n* is simply the *n*-th tile in row-major
//! order, and `TILED_SPARSE`, where each frame's position comes from its
//! per-frame functional group and tiles with no frame are blank. A level may
//! also be split across several instances of one *concatenation* (a level too
//! large for one file), in which case the tile is looked up in each member.
//! Frames are located by [`crate::decode::dicom`] and decoded here, so levels
//! are [`DicomLevel`]s with their own read path rather than TIFF tile grids.
//!
//! **Compression parity:** OpenSlide's C driver decodes baseline JPEG,
//! JPEG 2000, and uncompressed RGB. This driver does the first and last; JPEG
//! 2000 instances (transfer syntax `…1.2.4.90`/`.91`) are detected but fail to
//! open with a clear error, because there is no pure-Rust JPEG 2000 decoder yet
//! — the same stance `zeiss` takes toward JPEG XR.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;

use crate::associated::AssociatedImage;
use crate::decode::dicom::{
    self, DataSet, DicomObject, Frame, VL_WHOLE_SLIDE_MICROSCOPY_IMAGE_STORAGE, Value, tags,
};
use crate::decode::jpeg::JpegDecoder;
use crate::decode::tiff::TiffLike;
use crate::decode::{TileDecoder, samples_to_argb};
use crate::error::{OpenSlideError, Result};
use crate::grid::simple::SimpleGrid;
use crate::io::SlideReader;
use crate::io::fs::{Entry, SlideFs, SlideLocation};
use crate::level::LevelInfo;
use crate::properties::{Properties, names};

use super::{LevelData, SlideData, VendorDriver};

/// `ImageType[2]` of a pyramid level.
const FLAVOR_VOLUME: &str = "VOLUME";
/// `ImageType[2]` values that map to an associated image, and their names.
const ASSOCIATED_FLAVORS: &[(&str, &str)] = &[
    ("LABEL", "label"),
    ("OVERVIEW", "macro"),
    ("THUMBNAIL", "thumbnail"),
];

/// Transfer syntaxes whose pixels this driver can decode.
const TS_EXPLICIT_VR_LITTLE_ENDIAN: &str = "1.2.840.10008.1.2.1";
const TS_JPEG_BASELINE: &str = "1.2.840.10008.1.2.4.50";
/// JPEG 2000, lossless-only and lossy: recognized so the error names them.
const TS_JPEG_2000_LOSSLESS: &str = "1.2.840.10008.1.2.4.90";
const TS_JPEG_2000: &str = "1.2.840.10008.1.2.4.91";

/// How a frame's bytes are encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    /// Baseline JPEG. `rgb` distinguishes `RGB` component data (which carries
    /// no Adobe/JFIF marker to say so) from `YBR_FULL_422`.
    Jpeg { rgb: bool },
    /// Uncompressed interleaved 8-bit RGB.
    Rgb,
}

/// `ceil(a / b)` for a positive divisor, correct for negative `a` (a region can
/// start left of or above the level).
fn ceil_div(a: i64, b: i64) -> i64 {
    (a + b - 1).div_euclid(b)
}

/// Maps a tile position to the frame that holds it.
enum TileIndex {
    /// `TILED_FULL`: frame *n* is tile `(n % across, n / across)`.
    Full { across: i64, frames: usize },
    /// `TILED_SPARSE`: only the listed tiles have frames.
    Sparse(HashMap<(i64, i64), usize>),
}

impl TileIndex {
    fn frame_for(&self, col: i64, row: i64) -> Option<usize> {
        match self {
            TileIndex::Full { across, frames } => {
                let index = row.checked_mul(*across)?.checked_add(col)? as usize;
                (index < *frames).then_some(index)
            }
            TileIndex::Sparse(map) => map.get(&(col, row)).copied(),
        }
    }
}

/// One opened SOP instance: where its frames are and how to decode them.
struct DicomInstance {
    reader: Arc<dyn SlideReader>,
    encoding: Encoding,
    frames: Vec<Frame>,
    tiles: TileIndex,
    tile_w: i64,
    tile_h: i64,
}

impl DicomInstance {
    /// Decode the tile at `(col, row)`, or `None` if this instance has no frame
    /// there (a hole in a sparse level, or another concatenation member's tile).
    fn decode_tile(&self, col: i64, row: i64) -> Result<Option<Vec<u32>>> {
        let Some(index) = self.tiles.frame_for(col, row) else {
            return Ok(None);
        };
        let frame = self.frames.get(index).ok_or_else(|| {
            OpenSlideError::Decode(format!("DICOM frame {index} is out of range"))
        })?;
        let mut buf = vec![0u8; frame.length as usize];
        self.reader.read_exact_at(frame.offset, &mut buf)?;
        self.decode_frame(&buf, self.tile_w, self.tile_h).map(Some)
    }

    fn decode_frame(&self, data: &[u8], w: i64, h: i64) -> Result<Vec<u32>> {
        match self.encoding {
            Encoding::Jpeg { rgb } => {
                let decoder = if rgb {
                    JpegDecoder::new().assume_rgb()
                } else {
                    JpegDecoder::new()
                };
                decoder.decode(data, w as u32, h as u32)
            }
            Encoding::Rgb => samples_to_argb(data, w as u32, h as u32, 3),
        }
    }
}

/// A pyramid level: one or more concatenated instances sharing a tile grid.
pub struct DicomLevel {
    files: Vec<Arc<DicomInstance>>,
    width: i64,
    height: i64,
    tile_w: i64,
    tile_h: i64,
}

impl DicomLevel {
    /// Read a region of this level. `(ox, oy)` is its top-left in this level's
    /// pixel space. Tiles with no frame stay transparent, as in the C driver.
    pub fn read_region(&self, ox: i64, oy: i64, w: i64, h: i64) -> Result<Vec<u32>> {
        let mut out = vec![0u32; (w * h) as usize];
        let across = ceil_div(self.width, self.tile_w);
        let down = ceil_div(self.height, self.tile_h);

        let col_start = ox.div_euclid(self.tile_w).max(0);
        let col_end = ceil_div(ox + w, self.tile_w).min(across);
        let row_start = oy.div_euclid(self.tile_h).max(0);
        let row_end = ceil_div(oy + h, self.tile_h).min(down);

        for row in row_start..row_end {
            for col in col_start..col_end {
                let Some(pixels) = self.decode_tile(col, row)? else {
                    continue;
                };
                let tile_x = col * self.tile_w;
                let tile_y = row * self.tile_h;
                // A tile at the right/bottom edge extends past the level; clip
                // it so the padding the encoder put there isn't painted.
                let src_w = self.tile_w.min(self.width - tile_x);
                let src_h = self.tile_h.min(self.height - tile_y);
                crate::slide::composite_subtile(
                    &pixels,
                    self.tile_w,
                    0,
                    0,
                    src_w,
                    src_h,
                    tile_x,
                    tile_y,
                    &mut out,
                    ox,
                    oy,
                    w,
                    h,
                );
            }
        }
        Ok(out)
    }

    /// Decode a tile from whichever concatenation member holds it.
    fn decode_tile(&self, col: i64, row: i64) -> Result<Option<Vec<u32>>> {
        for file in &self.files {
            if let Some(pixels) = file.decode_tile(col, row)? {
                return Ok(Some(pixels));
            }
        }
        Ok(None)
    }
}

pub struct DicomDriver;

impl VendorDriver for DicomDriver {
    fn name(&self) -> &str {
        "dicom"
    }

    fn vendor(&self) -> &str {
        "dicom"
    }

    fn detect(&self, reader: &dyn SlideReader, tl: Option<&mut TiffLike>) -> Result<bool> {
        // Some DICOM WSI files are dual-personality DICOM/TIFF. When the file
        // also parses as TIFF and is *named* like one, defer to generic-tiff —
        // matching the C driver.
        if tl.is_some()
            && reader
                .location()
                .is_some_and(|loc| matches!(loc.extension().as_deref(), Some("tif") | Some("tiff")))
        {
            return Ok(false);
        }
        let Ok((meta, _)) = dicom::parse_file_meta(reader) else {
            return Ok(false);
        };
        Ok(meta.text(tags::MEDIA_STORAGE_SOP_CLASS_UID)
            == Some(VL_WHOLE_SLIDE_MICROSCOPY_IMAGE_STORAGE))
    }

    fn open(&self, reader: Box<dyn SlideReader>, _tl: Option<TiffLike>) -> Result<SlideData> {
        let loc = reader
            .location()
            .ok_or_else(|| {
                OpenSlideError::Other(
                    "DICOM slides span several files and need a filesystem to resolve them".into(),
                )
            })?
            .clone();
        open_dicom(&loc, reader)
    }
}

// ───────────────────────────────── open ────────────────────────────────────

/// A file that parsed as a WSI instance, before its format is validated.
struct ParsedFile {
    path: String,
    reader: Arc<dyn SlideReader>,
    object: DicomObject,
}

impl ParsedFile {
    fn series_uid(&self) -> Option<&str> {
        self.object.dataset.text(tags::SERIES_INSTANCE_UID)
    }

    fn flavor(&self) -> Option<&str> {
        self.object.dataset.text_at(tags::IMAGE_TYPE, 2)
    }
}

/// Upper bound on the threads used to scan a slide's directory.
///
/// The scan is latency-bound rather than CPU-bound, so more threads than cores
/// would still help — but a DICOM instance sitting in a folder of hundreds of
/// files should not spawn a thread storm or a burst of requests that a store
/// will rate-limit.
const MAX_SCAN_THREADS: usize = 8;

/// Parse an already-open file, rejecting anything that isn't a WSI instance.
///
/// The SOP class is checked from the file meta group before the dataset is
/// parsed: a directory can hold a DICOMDIR or unrelated instances, and those
/// should cost a few hundred bytes to dismiss, not a full parse.
fn parse_reader(path: String, reader: Arc<dyn SlideReader>) -> Result<ParsedFile> {
    let (meta, _) = dicom::parse_file_meta(&*reader)?;
    let sop_class = meta.text(tags::MEDIA_STORAGE_SOP_CLASS_UID);
    if sop_class != Some(VL_WHOLE_SLIDE_MICROSCOPY_IMAGE_STORAGE) {
        return Err(OpenSlideError::Decode(format!(
            "not a WSI DICOM: SOP class {}",
            sop_class.unwrap_or("<none>")
        )));
    }
    let object = dicom::parse(&*reader)?;
    Ok(ParsedFile {
        path,
        reader,
        object,
    })
}

/// Open and parse one directory entry. The size from the listing is passed
/// through so the open costs no extra metadata request.
fn parse_entry(fs: &Arc<dyn SlideFs>, entry: &Entry) -> Result<ParsedFile> {
    let reader = fs.open_sized(&entry.path, entry.size)?;
    parse_reader(entry.path.clone(), reader)
}

/// Parse every entry, concurrently when there is more than one.
///
/// Each parse is independent, and on blob storage each is several round trips,
/// so scanning serially makes open time the *sum* over the directory. Fanning
/// out makes it roughly the slowest single instance. Results stay in input
/// order, so a slide always assembles the same way regardless of thread
/// scheduling.
///
/// A file that fails to parse yields `None` rather than an error: anything that
/// isn't a WSI instance of this slide simply isn't part of it — a DICOMDIR, a
/// stray file, another slide in the same folder.
fn parse_entries(fs: &Arc<dyn SlideFs>, entries: &[Entry]) -> Vec<Option<ParsedFile>> {
    if entries.len() < 2 {
        return entries.iter().map(|e| parse_entry(fs, e).ok()).collect();
    }

    let threads = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4)
        .min(MAX_SCAN_THREADS)
        .min(entries.len());

    let slots: Vec<Mutex<Option<ParsedFile>>> = entries.iter().map(|_| Mutex::new(None)).collect();
    let cursor = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some(entry) = entries.get(i) else {
                        break;
                    };
                    if let Ok(parsed) = parse_entry(fs, entry) {
                        *slots[i].lock() = Some(parsed);
                    }
                }
            });
        }
    });

    slots.into_iter().map(Mutex::into_inner).collect()
}

fn open_dicom(loc: &SlideLocation, primary: Box<dyn SlideReader>) -> Result<SlideData> {
    let fs = loc.fs();

    // Reuse the reader detection already opened instead of opening the primary
    // instance a second time.
    let first = parse_reader(loc.path().to_string(), Arc::from(primary))?;
    let series = first
        .series_uid()
        .ok_or_else(|| OpenSlideError::Decode("DICOM has no SeriesInstanceUID".into()))?
        .to_string();

    let mut entries: Vec<Entry> = fs
        .list(loc.parent())?
        .into_iter()
        .filter(|e| e.path != loc.path())
        .collect();
    // Listing order is arbitrary; sort so a slide always opens the same way.
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    let mut files = vec![first];
    files.extend(
        parse_entries(fs, &entries)
            .into_iter()
            .flatten()
            .filter(|f| f.series_uid() == Some(series.as_str())),
    );

    let mut properties = Properties::new();
    properties.insert(names::VENDOR, "dicom");

    let mut level_groups: Vec<LevelGroup> = Vec::new();
    let mut associated_images: Vec<(String, AssociatedImage)> = Vec::new();
    let mut associated_instances: Vec<(String, String)> = Vec::new(); // name → SOP instance

    for file in files {
        let Some(flavor) = file.flavor() else {
            continue;
        };
        if flavor == FLAVOR_VOLUME {
            add_level_file(&mut level_groups, file)?;
        } else if let Some((_, name)) = ASSOCIATED_FLAVORS.iter().find(|(f, _)| *f == flavor) {
            let name = (*name).to_string();
            let instance_uid = file
                .object
                .dataset
                .text(tags::SOP_INSTANCE_UID)
                .unwrap_or_default()
                .to_string();
            if let Some((_, previous)) = associated_instances.iter().find(|(n, _)| *n == name) {
                // A duplicated file is harmless; two *different* instances
                // claiming the same slot means we've misread the slide.
                if *previous == instance_uid {
                    continue;
                }
                return Err(OpenSlideError::Decode(format!(
                    "DICOM slide has two {name} images ({previous} and {instance_uid})"
                )));
            }
            associated_images.push((name.clone(), read_associated(&file)?));
            associated_instances.push((name, instance_uid));
        }
        // Any other flavor is not something we can place; ignore it.
    }

    if level_groups.is_empty() {
        return Err(OpenSlideError::Decode(
            "DICOM slide has no pyramid levels".into(),
        ));
    }

    // Largest level first, which is what level 0 means.
    level_groups.sort_by_key(|g| std::cmp::Reverse(g.width));
    let base_width = level_groups[0].width as f64;
    let base_height = level_groups[0].height as f64;

    // Properties and the slide ICC profile come from level 0's first instance,
    // so they don't depend on directory order.
    let level0_first = &level_groups[0].files[0];
    add_properties(&mut properties, &level0_first.object);
    let icc_profile = icc_profile(&level0_first.object.dataset).map(<[u8]>::to_vec);

    let mut levels = Vec::with_capacity(level_groups.len());
    for group in level_groups {
        let (width, height) = (group.width, group.height);
        let (tile_w, tile_h) = (group.tile_w, group.tile_h);
        let files = group.finish()?;
        levels.push(LevelData {
            info: LevelInfo {
                width,
                height,
                downsample: (base_width / width as f64 + base_height / height as f64) / 2.0,
                tile_width: tile_w,
                tile_height: tile_h,
            },
            grid: SimpleGrid::new(width, height, tile_w, tile_h),
            tiff_dir: 0,
            // Placeholder: DicomLevel decodes internally (per-instance encoding),
            // so this decoder is never invoked for DICOM levels.
            decoder: Box::new(JpegDecoder::new()),
            tiff_tile_width: tile_w,
            tiff_tile_height: tile_h,
            tilemap: None,
            ndpi: None,
            mirax: None,
            zeiss: None,
            synthetic: None,
            dicom: Some(DicomLevel {
                files,
                width,
                height,
                tile_w,
                tile_h,
            }),
        });
    }

    Ok(SlideData {
        levels,
        properties,
        associated_images,
        icc_profile,
        tiff: None,
    })
}

/// Instances that share a level's dimensions: one, or a whole concatenation.
struct LevelGroup {
    width: i64,
    height: i64,
    tile_w: i64,
    tile_h: i64,
    /// Concatenation members, indexed by `InConcatenationNumber - 1`.
    files: Vec<ParsedFile>,
    /// Slot for each member, parallel to `files`; used to order and check them.
    slots: Vec<usize>,
    concatenation_uid: Option<String>,
}

impl LevelGroup {
    /// Order the members and turn them into read-path instances.
    fn finish(self) -> Result<Vec<Arc<DicomInstance>>> {
        let mut ordered: Vec<(usize, ParsedFile)> =
            self.slots.into_iter().zip(self.files).collect();
        ordered.sort_by_key(|(slot, _)| *slot);

        // Every slot from 1..=n must be filled, or we're missing a file and
        // would silently return blank tiles for its share of the level.
        for (index, (slot, file)) in ordered.iter().enumerate() {
            if *slot != index {
                return Err(OpenSlideError::Decode(format!(
                    "DICOM concatenation {} is missing instance {} (found {} at {})",
                    self.concatenation_uid.as_deref().unwrap_or("<none>"),
                    index + 1,
                    slot + 1,
                    file.path
                )));
            }
        }
        if let Some(total) = ordered
            .first()
            .and_then(|(_, f)| f.object.dataset.int(tags::IN_CONCATENATION_TOTAL_NUMBER))
        {
            if total != ordered.len() as i64 {
                return Err(OpenSlideError::Decode(format!(
                    "DICOM concatenation declares {total} instances, found {}",
                    ordered.len()
                )));
            }
        }

        ordered
            .into_iter()
            .map(|(_, file)| build_instance(&file).map(Arc::new))
            .collect()
    }
}

/// Add a `VOLUME` instance to its level, creating the level if it's new.
fn add_level_file(groups: &mut Vec<LevelGroup>, file: ParsedFile) -> Result<()> {
    let ds = &file.object.dataset;
    let (width, height) = total_pixel_matrix(ds)?;
    let (tile_w, tile_h) = tile_size(ds)?;

    let slot = match ds.int(tags::IN_CONCATENATION_NUMBER) {
        // In-concatenation numbers are 1-based.
        Some(n) if n >= 1 => (n - 1) as usize,
        Some(n) => {
            return Err(OpenSlideError::Decode(format!(
                "DICOM in-concatenation number {n} is out of range"
            )));
        }
        None => 0,
    };
    let concatenation_uid = ds.text(tags::CONCATENATION_UID).map(str::to_string);
    let instance_uid = ds
        .text(tags::SOP_INSTANCE_UID)
        .unwrap_or_default()
        .to_string();

    let Some(group) = groups
        .iter_mut()
        .find(|g| g.width == width && g.height == height)
    else {
        groups.push(LevelGroup {
            width,
            height,
            tile_w,
            tile_h,
            files: vec![file],
            slots: vec![slot],
            concatenation_uid,
        });
        return Ok(());
    };

    // A duplicated file is harmless; two distinct instances at the same
    // dimensions that aren't concatenation members are not something we
    // understand, and guessing would mean dropping half the slide.
    if group
        .files
        .iter()
        .any(|f| f.object.dataset.text(tags::SOP_INSTANCE_UID) == Some(instance_uid.as_str()))
    {
        return Ok(());
    }
    if concatenation_uid.is_none() || group.concatenation_uid != concatenation_uid {
        return Err(OpenSlideError::Decode(format!(
            "DICOM slide has two {width}x{height} images that are not part of one \
             concatenation ({} and {instance_uid})",
            group
                .files
                .first()
                .and_then(|f| f.object.dataset.text(tags::SOP_INSTANCE_UID))
                .unwrap_or("<none>")
        )));
    }
    if group.tile_w != tile_w || group.tile_h != tile_h {
        return Err(OpenSlideError::Decode(format!(
            "DICOM concatenation member tile size {tile_w}x{tile_h} doesn't match \
             {}x{}",
            group.tile_w, group.tile_h
        )));
    }
    if group.slots.contains(&slot) {
        return Err(OpenSlideError::Decode(format!(
            "DICOM concatenation has two instances numbered {}",
            slot + 1
        )));
    }
    group.files.push(file);
    group.slots.push(slot);
    Ok(())
}

/// Validate an instance's pixel format and build its frame/tile index.
fn build_instance(file: &ParsedFile) -> Result<DicomInstance> {
    let ds = &file.object.dataset;
    let encoding = encoding(&file.object)?;

    // The C driver requires exactly this pixel layout; anything else would need
    // a different unpacking path than the one below.
    for (tag, name, expected) in [
        (tags::PLANAR_CONFIGURATION, "PlanarConfiguration", 0),
        (tags::BITS_ALLOCATED, "BitsAllocated", 8),
        (tags::BITS_STORED, "BitsStored", 8),
        (tags::HIGH_BIT, "HighBit", 7),
        (tags::SAMPLES_PER_PIXEL, "SamplesPerPixel", 3),
        (tags::PIXEL_REPRESENTATION, "PixelRepresentation", 0),
    ] {
        match ds.int(tag) {
            Some(value) if value == expected => {}
            Some(value) => {
                return Err(OpenSlideError::Decode(format!(
                    "unsupported DICOM {name} {value} (expected {expected})"
                )));
            }
            None => {
                return Err(OpenSlideError::Decode(format!(
                    "DICOM instance has no {name}"
                )));
            }
        }
    }
    // Optional, but a multi-focal-plane image would need a plane to be chosen.
    if let Some(planes) = ds.int(tags::TOTAL_PIXEL_MATRIX_FOCAL_PLANES) {
        if planes != 1 {
            return Err(OpenSlideError::Decode(format!(
                "unsupported DICOM slide with {planes} focal planes"
            )));
        }
    }

    let (width, height) = total_pixel_matrix(ds)?;
    let (tile_w, tile_h) = tile_size(ds)?;
    let across = ceil_div(width, tile_w);
    let down = ceil_div(height, tile_h);
    let frame_count = ds.int(tags::NUMBER_OF_FRAMES).unwrap_or(1);
    if frame_count < 0 {
        return Err(OpenSlideError::Decode(format!(
            "DICOM instance declares {frame_count} frames"
        )));
    }
    let frame_count = frame_count as usize;

    let frames = file
        .object
        .frame_locations(frame_count, (tile_w * tile_h * 3) as u64)?;

    // DimensionOrganizationType is optional and defaults to sparse.
    let sparse = ds.text(tags::DIMENSION_ORGANIZATION_TYPE) != Some("TILED_FULL");
    let tiles = if sparse {
        let positions = &file.object.frame_positions;
        if positions.len() != frame_count {
            return Err(OpenSlideError::Decode(format!(
                "DICOM sparse level has {} frame positions for {frame_count} frames",
                positions.len()
            )));
        }
        let mut map = HashMap::with_capacity(frame_count);
        for (index, (column, row)) in positions.iter().enumerate() {
            // Positions are 1-based pixel coordinates of the tile's top-left.
            let col = (column - 1).div_euclid(tile_w);
            let row = (row - 1).div_euclid(tile_h);
            if col < 0 || col >= across || row < 0 || row >= down {
                return Err(OpenSlideError::Decode(format!(
                    "DICOM frame {index} is positioned at tile ({col},{row}), outside the \
                     {across}x{down} tile grid"
                )));
            }
            map.insert((col, row), index);
        }
        TileIndex::Sparse(map)
    } else {
        TileIndex::Full {
            across,
            frames: frame_count,
        }
    };

    Ok(DicomInstance {
        reader: file.reader.clone(),
        encoding,
        frames,
        tiles,
        tile_w,
        tile_h,
    })
}

/// Pixel encoding from the transfer syntax and photometric interpretation.
fn encoding(object: &DicomObject) -> Result<Encoding> {
    let photometric = object
        .dataset
        .text(tags::PHOTOMETRIC_INTERPRETATION)
        .ok_or_else(|| {
            OpenSlideError::Decode("DICOM instance has no PhotometricInterpretation".into())
        })?;

    match object.transfer_syntax.as_str() {
        TS_JPEG_BASELINE => match photometric {
            "YBR_FULL_422" | "YBR_FULL" => Ok(Encoding::Jpeg { rgb: false }),
            "RGB" => Ok(Encoding::Jpeg { rgb: true }),
            other => Err(OpenSlideError::Decode(format!(
                "unsupported DICOM photometric interpretation {other} for JPEG"
            ))),
        },
        TS_EXPLICIT_VR_LITTLE_ENDIAN => match photometric {
            "RGB" => Ok(Encoding::Rgb),
            other => Err(OpenSlideError::Decode(format!(
                "unsupported DICOM photometric interpretation {other} for uncompressed pixels"
            ))),
        },
        ts @ (TS_JPEG_2000 | TS_JPEG_2000_LOSSLESS) => Err(OpenSlideError::Decode(format!(
            "DICOM transfer syntax {ts} is JPEG 2000, which slidekit cannot decode yet"
        ))),
        other => Err(OpenSlideError::Decode(format!(
            "unsupported DICOM transfer syntax {other}"
        ))),
    }
}

fn total_pixel_matrix(ds: &DataSet) -> Result<(i64, i64)> {
    let width = ds.int(tags::TOTAL_PIXEL_MATRIX_COLUMNS);
    let height = ds.int(tags::TOTAL_PIXEL_MATRIX_ROWS);
    match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Ok((w, h)),
        _ => Err(OpenSlideError::Decode(
            "DICOM instance has no usable total pixel matrix size".into(),
        )),
    }
}

fn tile_size(ds: &DataSet) -> Result<(i64, i64)> {
    match (ds.int(tags::COLUMNS), ds.int(tags::ROWS)) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Ok((w, h)),
        _ => Err(OpenSlideError::Decode(
            "DICOM instance has no usable frame size".into(),
        )),
    }
}

/// The ICC profile of a dataset's first optical path, if it has one.
fn icc_profile(ds: &DataSet) -> Option<&[u8]> {
    ds.item(tags::OPTICAL_PATH_SEQUENCE, 0)?
        .bytes(tags::ICC_PROFILE)
}

/// Decode a single-frame instance (label, macro, thumbnail) in full.
fn read_associated(file: &ParsedFile) -> Result<AssociatedImage> {
    let instance = build_instance(file)?;
    let (width, height) = total_pixel_matrix(&file.object.dataset)?;
    if instance.tile_w != width || instance.tile_h != height {
        return Err(OpenSlideError::Decode(format!(
            "DICOM associated image is tiled ({}x{} frames in a {width}x{height} image)",
            instance.tile_w, instance.tile_h
        )));
    }
    let argb_data = instance
        .decode_tile(0, 0)?
        .ok_or_else(|| OpenSlideError::Decode("DICOM associated image has no frame".into()))?;

    Ok(AssociatedImage {
        width,
        height,
        argb_data,
        icc_profile: icc_profile(&file.object.dataset).map(<[u8]>::to_vec),
    })
}

// ────────────────────────────── properties ─────────────────────────────────

/// Dump every named element as `dicom.<Keyword>`, then derive the standard
/// OpenSlide properties from it — the same shape the C driver produces, so
/// property paths like
/// `dicom.SharedFunctionalGroupsSequence[0].PixelMeasuresSequence[0].PixelSpacing[0]`
/// match.
fn add_properties(props: &mut Properties, object: &DicomObject) {
    dump_dataset(props, "dicom", &object.file_meta);
    dump_dataset(props, "dicom", &object.dataset);

    // Pixel spacing is (row, column) in millimetres; OpenSlide wants microns.
    let spacing = object
        .dataset
        .item(tags::SHARED_FUNCTIONAL_GROUPS_SEQUENCE, 0)
        .and_then(|g| g.item(tags::PIXEL_MEASURES_SEQUENCE, 0));
    if let Some(spacing) = spacing {
        if let Some(y) = spacing.float_at(tags::PIXEL_SPACING, 0) {
            props.insert(names::MPP_Y, (y * 1000.0).to_string());
        }
        if let Some(x) = spacing.float_at(tags::PIXEL_SPACING, 1) {
            props.insert(names::MPP_X, (x * 1000.0).to_string());
        }
    }

    if let Some(power) = object
        .dataset
        .item(tags::OPTICAL_PATH_SEQUENCE, 0)
        .and_then(|p| p.float(tags::OBJECTIVE_LENS_POWER))
    {
        props.insert(names::OBJECTIVE_POWER, power.to_string());
    }
    if let Some(barcode) = object.dataset.text(tags::BARCODE_VALUE) {
        props.insert(names::BARCODE, barcode);
    }
}

fn dump_dataset(props: &mut Properties, prefix: &str, ds: &DataSet) {
    for element in ds.iter() {
        // Tags outside the dictionary have no name to give them.
        let Some(keyword) = dicom::keyword(element.tag) else {
            continue;
        };
        match &element.value {
            Value::Items(items) => {
                for (index, item) in items.iter().enumerate() {
                    dump_dataset(props, &format!("{prefix}.{keyword}[{index}]"), item);
                }
            }
            // Bulk data (pixels, per-frame groups) and binary blobs are not
            // meaningful as strings.
            Value::Elided | Value::Bytes(_) => {}
            _ => {
                let components = element.value_strings();
                if components.is_empty() {
                    continue;
                }
                if components.len() == 1 {
                    props.insert(format!("{prefix}.{keyword}"), components[0].clone());
                } else {
                    for (index, component) in components.iter().enumerate() {
                        props.insert(format!("{prefix}.{keyword}[{index}]"), component.clone());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile_index_sparse(entries: &[((i64, i64), usize)]) -> TileIndex {
        TileIndex::Sparse(entries.iter().copied().collect())
    }

    #[test]
    fn tiled_full_frames_are_row_major() {
        let index = TileIndex::Full {
            across: 3,
            frames: 7,
        };
        assert_eq!(index.frame_for(0, 0), Some(0));
        assert_eq!(index.frame_for(2, 0), Some(2));
        assert_eq!(index.frame_for(0, 1), Some(3));
        assert_eq!(index.frame_for(0, 2), Some(6));
        // Past the last frame: a hole, not a wrapped-around tile.
        assert_eq!(index.frame_for(1, 2), None);
        assert_eq!(index.frame_for(-1, 0), None);
    }

    #[test]
    fn sparse_tiles_without_a_frame_are_holes() {
        let index = tile_index_sparse(&[((0, 0), 0), ((5, 3), 1)]);
        assert_eq!(index.frame_for(0, 0), Some(0));
        assert_eq!(index.frame_for(5, 3), Some(1));
        assert_eq!(index.frame_for(1, 0), None);
    }

    #[test]
    fn encoding_follows_transfer_syntax_and_photometric() {
        let cases = [
            (
                TS_JPEG_BASELINE,
                "YBR_FULL_422",
                Some(Encoding::Jpeg { rgb: false }),
            ),
            (TS_JPEG_BASELINE, "RGB", Some(Encoding::Jpeg { rgb: true })),
            (TS_EXPLICIT_VR_LITTLE_ENDIAN, "RGB", Some(Encoding::Rgb)),
            // Uncompressed YBR would need a color transform we don't do.
            (TS_EXPLICIT_VR_LITTLE_ENDIAN, "YBR_FULL_422", None),
            (TS_JPEG_2000, "YBR_ICT", None),
            ("1.2.840.10008.1.2.4.70", "RGB", None),
        ];
        for (syntax, photometric, expected) in cases {
            let object = object_with(syntax, photometric);
            assert_eq!(encoding(&object).ok(), expected, "{syntax} + {photometric}");
        }
    }

    #[test]
    fn jpeg_2000_is_rejected_by_name() {
        let object = object_with(TS_JPEG_2000_LOSSLESS, "YBR_RCT");
        let err = encoding(&object).expect_err("JPEG 2000 is not decodable yet");
        assert!(err.to_string().contains("JPEG 2000"), "{err}");
    }

    /// A minimal parsed instance carrying just a transfer syntax and photometric
    /// interpretation, for the encoding table above.
    fn object_with(transfer_syntax: &str, photometric: &str) -> DicomObject {
        use crate::decode::dicom::Element;

        let dataset = DataSet::from_iter([Element {
            tag: tags::PHOTOMETRIC_INTERPRETATION,
            vr: *b"CS",
            value: Value::Text(photometric.to_string()),
        }]);
        DicomObject {
            file_meta: DataSet::default(),
            dataset,
            transfer_syntax: transfer_syntax.to_string(),
            pixel_data: None,
            frame_positions: Vec::new(),
        }
    }
}
