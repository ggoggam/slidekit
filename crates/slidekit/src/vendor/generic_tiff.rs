//! Generic TIFF vendor driver.
//!
//! This is the simplest vendor driver — it handles any tiled TIFF file.
//! It serves as the fallback at the end of the detection pipeline.
//!
//! Ported from `openslide-vendor-generic-tiff.c` (313 LOC in C).

use crate::decode::TileDecoder;
use crate::decode::check_bits_per_sample;
use crate::decode::deflate::DeflateDecoder;
use crate::decode::jp2k::Jp2kDecoder;
use crate::decode::jpeg::JpegDecoder;
use crate::decode::jxl::JxlDecoder;
use crate::decode::lzw::LzwDecoder;
use crate::decode::packbits::PackBitsDecoder;
use crate::decode::png::PngDecoder;
use crate::decode::tiff::TiffLike;
use crate::decode::tiff::tags;
use crate::error::{OpenSlideError, Result};
use crate::grid::simple::SimpleGrid;
use crate::icc::IccProfile;
use crate::io::SlideReader;
use crate::level::LevelInfo;
use crate::properties::{self, Properties};
use crate::vendor::{LevelData, SlideData, VendorDriver};

pub struct GenericTiffDriver;

impl VendorDriver for GenericTiffDriver {
    fn name(&self) -> &str {
        "generic-tiff"
    }

    fn vendor(&self) -> &str {
        "generic-tiff"
    }

    fn detect(&self, _reader: &dyn SlideReader, tl: Option<&mut TiffLike>) -> Result<bool> {
        let tl = tl.ok_or_else(|| OpenSlideError::Decode("Not a TIFF file".into()))?;

        // The first directory must be tiled.
        if !tl.is_tiled(0) {
            return Err(OpenSlideError::Decode("TIFF is not tiled".into()));
        }

        Ok(true)
    }

    fn open(&self, _reader: Box<dyn SlideReader>, tl: Option<TiffLike>) -> Result<SlideData> {
        let mut tl = tl.ok_or_else(|| OpenSlideError::Decode("Not a TIFF file".into()))?;

        let dir_count = tl.directory_count();

        // Collect tiled directories with their metadata.
        // For dir 0 we accept unconditionally (matches C behavior);
        // for subsequent dirs we require SUBFILETYPE with REDUCEDIMAGE bit.
        struct DirInfo {
            dir: i64,
            image_w: i64,
            image_h: i64,
            tile_w: i64,
            tile_h: i64,
        }

        let mut tiled_dirs: Vec<DirInfo> = Vec::new();

        for dir in 0..dir_count {
            // Must be tiled
            if !tl.is_tiled(dir) {
                continue;
            }

            // For non-first directories, require reduced-resolution subfiletype
            if dir != 0 {
                if !tl.has_tag(dir, tags::SUBFILE_TYPE) {
                    continue;
                }
                let subfiletype = tl.get_uint(dir, tags::SUBFILE_TYPE)?;
                if subfiletype & tags::FILETYPE_REDUCEDIMAGE == 0 {
                    continue;
                }
            }

            let image_w = tl.get_uint(dir, tags::IMAGE_WIDTH)? as i64;
            let image_h = tl.get_uint(dir, tags::IMAGE_LENGTH)? as i64;
            let tile_w = tl.get_uint(dir, tags::TILE_WIDTH)? as i64;
            let tile_h = tl.get_uint(dir, tags::TILE_LENGTH)? as i64;
            check_tile_size(tile_w, tile_h, dir)?;

            tiled_dirs.push(DirInfo {
                dir,
                image_w,
                image_h,
                tile_w,
                tile_h,
            });
        }

        if tiled_dirs.is_empty() {
            return Err(OpenSlideError::Decode("No tiled directories found".into()));
        }

        // Sort by width descending (largest first = level 0)
        tiled_dirs.sort_by_key(|d| std::cmp::Reverse(d.image_w));

        // Compute downsample factors relative to level 0
        let base_w = tiled_dirs[0].image_w as f64;

        // Build levels with grids and decoders
        let mut levels = Vec::with_capacity(tiled_dirs.len());
        for d in &tiled_dirs {
            let decoder = make_decoder_for_dir(&mut tl, d.dir)?;
            levels.push(LevelData {
                info: LevelInfo {
                    width: d.image_w,
                    height: d.image_h,
                    downsample: base_w / d.image_w as f64,
                    tile_width: d.tile_w,
                    tile_height: d.tile_h,
                },
                grid: SimpleGrid::new(d.image_w, d.image_h, d.tile_w, d.tile_h),
                tiff_dir: d.dir,
                decoder,
                tiff_tile_width: d.tile_w,
                tiff_tile_height: d.tile_h,
                tilemap: None,
                ndpi: None,
                mirax: None,
                zeiss: None,
                synthetic: None,
                dicom: None,
            });
        }

        // Build properties
        let mut props = Properties::new();
        props.insert(properties::names::VENDOR, "generic-tiff");

        // Set resolution properties (mpp-x, mpp-y) from TIFF resolution tags
        set_resolution_props(&mut tl, 0, &mut props);

        // Copy standard TIFF string properties from directory 0
        set_tiff_string_props(&mut tl, 0, &mut props);

        Ok(SlideData {
            levels,
            properties: props,
            associated_images: vec![],
            icc_profile: IccProfile::absent(),
            tiff: Some(tl),
        })
    }
}

/// Build a JPEG tile decoder for `dir`, honoring the TIFF PhotometricInterpretation.
///
/// TIFF-tiled JPEG streams carry no JFIF/Adobe marker (they rely on the shared
/// JPEGTables tag), so jpeg-decoder can't tell RGB tiles from YCbCr and defaults
/// to YCbCr. A `PHOTOMETRIC_RGB` tile already holds R,G,B components; letting the
/// YCbCr→RGB transform run turns neutral gray magenta (e.g. Aperio "JPEG/RGB").
/// Read the tag and force RGB interpretation when it says so.
///
/// NB: Aperio SVS also sets a `YCbCrSubSampling` tag of 2,2 on these RGB tiles,
/// but it's spurious — the JPEG frame headers declare 1×1 sampling and the data
/// really is RGB. So the subsampling tag must NOT be used to second-guess the
/// photometric here; trust `PHOTOMETRIC_RGB`.
pub(crate) fn jpeg_decoder_for_dir(tl: &mut TiffLike, dir: i64) -> Result<JpegDecoder> {
    let is_rgb = tl.has_tag(dir, tags::PHOTOMETRIC)
        && tl.get_uint(dir, tags::PHOTOMETRIC)? == tags::PHOTOMETRIC_RGB as u64;
    let mut dec = match tl.jpeg_tables(dir)? {
        Some(t) => JpegDecoder::with_tables(t),
        None => JpegDecoder::new(),
    };
    if is_rgb {
        dec = dec.assume_rgb();
    }
    Ok(dec)
}

/// Create a tile decoder appropriate for the compression of a TIFF directory.
///
/// Errors on an unsupported compression scheme, which doubles as the
/// "is this codec configured?" check for callers building levels.
pub(crate) fn make_decoder_for_dir(tl: &mut TiffLike, dir: i64) -> Result<Box<dyn TileDecoder>> {
    let compression = if tl.has_tag(dir, tags::COMPRESSION) {
        tl.get_uint(dir, tags::COMPRESSION)?
    } else {
        1 // no compression
    };

    match compression as u16 {
        tags::COMPRESSION_JPEG => Ok(Box::new(jpeg_decoder_for_dir(tl, dir)?)),
        tags::COMPRESSION_LZW => Ok(Box::new(LzwDecoder::for_dir(tl, dir)?)),
        tags::COMPRESSION_DEFLATE | tags::COMPRESSION_DEFLATE_OLD => {
            Ok(Box::new(DeflateDecoder::for_dir(tl, dir)?))
        }
        // Standard TIFF JPEG 2000 keeps its colour in the codestream, unlike
        // Aperio's private tags, so the components come out as RGB.
        tags::COMPRESSION_JP2K => Ok(Box::new(Jp2kDecoder::rgb())),
        tags::COMPRESSION_PNG => Ok(Box::new(PngDecoder::new())),
        tags::COMPRESSION_JXL | tags::COMPRESSION_JXL_DNG => Ok(Box::new(JxlDecoder::new())),
        tags::COMPRESSION_PACKBITS => Ok(Box::new(PackBitsDecoder::for_dir(tl, dir)?)),
        1 => {
            // Uncompressed — return a passthrough decoder
            check_bits_per_sample(tl, dir)?;
            Ok(Box::new(UncompressedDecoder))
        }
        other => Err(OpenSlideError::Decode(format!(
            "Unsupported TIFF compression: {other}"
        ))),
    }
}

/// Decoder for uncompressed TIFF tiles (raw RGBA/RGB pixels).
struct UncompressedDecoder;

impl TileDecoder for UncompressedDecoder {
    fn decode(&self, data: &[u8], width: u32, height: u32) -> Result<Vec<u32>> {
        // An uncompressed tile carries no channel count of its own, so infer it
        // from the size: RGBA first, then RGB. Packing goes through the shared
        // `samples_to_argb`, whose `.take(pixel_count)` is what keeps an
        // over-long buffer from yielding more pixels than the tile has.
        if let Ok(px) = crate::decode::samples_to_argb(data, width, height, 4) {
            return Ok(px);
        }
        if let Ok(px) = crate::decode::samples_to_argb(data, width, height, 3) {
            return Ok(px);
        }
        Err(OpenSlideError::Decode(format!(
            "Uncompressed tile data size {} doesn't match {}x{} pixels",
            data.len(),
            width,
            height
        )))
    }
}

/// Compute microns-per-pixel from TIFF resolution tags and store as properties.
///
/// Ported from `_openslide_tifflike_set_resolution_props` in the C library.
pub fn set_resolution_props(tl: &mut TiffLike, dir: i64, props: &mut Properties) {
    // Read resolution unit (default to inches per TIFF spec)
    let unit = if tl.has_tag(dir, tags::RESOLUTION_UNIT) {
        match tl.get_uint(dir, tags::RESOLUTION_UNIT) {
            Ok(u) => u,
            Err(_) => return,
        }
    } else {
        tags::RESUNIT_INCH
    };

    let dividend: f64 = match unit {
        tags::RESUNIT_INCH => 25400.0,       // microns per inch
        tags::RESUNIT_CENTIMETER => 10000.0, // microns per cm
        _ => return,
    };

    // Read X and Y resolution
    let res_tags = [tags::X_RESOLUTION, tags::Y_RESOLUTION];
    let prop_names = [properties::names::MPP_X, properties::names::MPP_Y];

    let mut values = [0.0f64; 2];
    for (i, &tag) in res_tags.iter().enumerate() {
        if !tl.has_tag(dir, tag) {
            return;
        }
        match tl.get_float(dir, tag) {
            Ok(res) if res != 0.0 => values[i] = dividend / res,
            _ => return,
        }
    }

    for (i, name) in prop_names.iter().enumerate() {
        props.insert(*name, format!("{:.10}", values[i]));
    }
}

/// Reject a tile size the tile-count arithmetic cannot divide by.
///
/// `TiffLike::is_tiled` reports whether the `TileWidth`/`TileLength` tags are
/// *present*, not whether they hold a usable value, so a corrupt or hostile
/// file reaches the level builders with a zero tile size. Every tile count is a
/// division by it, and an integer divide by zero aborts the process — a library
/// reading untrusted pathology files has to return `Err` instead.
pub fn check_tile_size(tile_w: i64, tile_h: i64, dir: i64) -> Result<()> {
    if tile_w <= 0 || tile_h <= 0 {
        return Err(OpenSlideError::Decode(format!(
            "Invalid tile size {tile_w}x{tile_h} in directory {dir}"
        )));
    }
    Ok(())
}

/// Copy well-known TIFF string tags as openslide.tiff.* properties.
pub fn set_tiff_string_props(tl: &mut TiffLike, dir: i64, props: &mut Properties) {
    let mappings: &[(u16, &str)] = &[
        (tags::IMAGE_DESCRIPTION, "tiff.ImageDescription"),
        (tags::MAKE, "tiff.Make"),
        (tags::MODEL, "tiff.Model"),
        (tags::SOFTWARE, "tiff.Software"),
        (tags::DATE_TIME, "tiff.DateTime"),
        (tags::ARTIST, "tiff.Artist"),
        (tags::HOST_COMPUTER, "tiff.HostComputer"),
        (tags::COPYRIGHT, "tiff.Copyright"),
        (tags::DOCUMENT_NAME, "tiff.DocumentName"),
    ];

    for &(tag, prop_name) in mappings {
        if tl.has_tag(dir, tag)
            && let Ok(val) = tl.get_string(dir, tag)
            && !val.is_empty()
        {
            props.insert(prop_name, val);
        }
    }
}
