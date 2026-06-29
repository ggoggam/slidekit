//! Generic TIFF vendor driver.
//!
//! This is the simplest vendor driver — it handles any tiled TIFF file.
//! It serves as the fallback at the end of the detection pipeline.
//!
//! Ported from `openslide-vendor-generic-tiff.c` (313 LOC in C).

use crate::color::argb;
use crate::decode::TileDecoder;
use crate::decode::deflate::DeflateDecoder;
use crate::decode::jpeg::JpegDecoder;
use crate::decode::lzw::LzwDecoder;
use crate::decode::packbits::PackBitsDecoder;
use crate::decode::tiff::TiffLike;
use crate::decode::tiff::tags;
use crate::error::{OpenSlideError, Result};
use crate::grid::simple::SimpleGrid;
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
            icc_profile: None,
            tiff: Some(tl),
        })
    }
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
        tags::COMPRESSION_JPEG => {
            let tables = tl.jpeg_tables(dir)?;
            match tables {
                Some(t) => Ok(Box::new(JpegDecoder::with_tables(t))),
                None => Ok(Box::new(JpegDecoder::new())),
            }
        }
        tags::COMPRESSION_LZW => Ok(Box::new(LzwDecoder::for_dir(tl, dir)?)),
        tags::COMPRESSION_DEFLATE | tags::COMPRESSION_DEFLATE_OLD => {
            Ok(Box::new(DeflateDecoder::for_dir(tl, dir)?))
        }
        tags::COMPRESSION_PACKBITS => Ok(Box::new(PackBitsDecoder::for_dir(tl, dir)?)),
        1 => {
            // Uncompressed — return a passthrough decoder
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
        let pixel_count = (width * height) as usize;
        // Assume RGBA (4 bytes per pixel)
        if data.len() >= pixel_count * 4 {
            let mut argb_buf = Vec::with_capacity(pixel_count);
            for chunk in data.chunks_exact(4) {
                argb_buf.push(argb(chunk[3], chunk[0], chunk[1], chunk[2]));
            }
            return Ok(argb_buf);
        }
        // Assume RGB (3 bytes per pixel)
        if data.len() >= pixel_count * 3 {
            let mut argb_buf = Vec::with_capacity(pixel_count);
            for chunk in data.chunks_exact(3) {
                argb_buf.push(argb(0xFF, chunk[0], chunk[1], chunk[2]));
            }
            return Ok(argb_buf);
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
        if tl.has_tag(dir, tag) {
            if let Ok(val) = tl.get_string(dir, tag) {
                if !val.is_empty() {
                    props.insert(prop_name, val);
                }
            }
        }
    }
}
