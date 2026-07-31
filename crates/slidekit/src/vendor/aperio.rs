//! Aperio (SVS, TIF) vendor driver.
//!
//! Aperio slides are TIFF files where directory 0's ImageDescription starts
//! with "Aperio". Tiled directories are pyramid levels; stripped directories
//! are associated images (thumbnail at dir 1, then label/macro identified
//! from the second line of their ImageDescription).
//!
//! Ported from `openslide-vendor-aperio.c` (607 LOC in C).

use crate::associated::AssociatedImage;
use crate::color::argb;
use crate::decode::TileDecoder;
use crate::decode::deflate::DeflateDecoder;
use crate::decode::lzw::{self, LzwDecoder};
use crate::decode::packbits::PackBitsDecoder;
use crate::decode::tiff::TiffLike;
use crate::decode::tiff::tags;
use crate::error::{OpenSlideError, Result};
use crate::grid::simple::SimpleGrid;
use crate::io::SlideReader;
use crate::level::LevelInfo;
use crate::properties::{self, Properties};
use crate::vendor::generic_tiff::{
    jpeg_decoder_for_dir, set_resolution_props, set_tiff_string_props,
};
use crate::vendor::{LevelData, SlideData, VendorDriver};

const APERIO_DESCRIPTION: &str = "Aperio";

pub struct AperioDriver;

impl VendorDriver for AperioDriver {
    fn name(&self) -> &str {
        "aperio"
    }

    fn vendor(&self) -> &str {
        "aperio"
    }

    fn detect(&self, _reader: &dyn SlideReader, tl: Option<&mut TiffLike>) -> Result<bool> {
        let tl = tl.ok_or_else(|| OpenSlideError::Decode("Not a TIFF file".into()))?;

        // Must be tiled
        if !tl.is_tiled(0) {
            return Err(OpenSlideError::Decode("TIFF is not tiled".into()));
        }

        // ImageDescription must start with "Aperio"
        if !tl.has_tag(0, tags::IMAGE_DESCRIPTION) {
            return Err(OpenSlideError::Decode("No ImageDescription".into()));
        }

        let desc = tl.get_string(0, tags::IMAGE_DESCRIPTION)?;
        if !desc.starts_with(APERIO_DESCRIPTION) {
            return Err(OpenSlideError::Decode("Not an Aperio slide".into()));
        }

        Ok(true)
    }

    fn open(&self, _reader: Box<dyn SlideReader>, tl: Option<TiffLike>) -> Result<SlideData> {
        let mut tl = tl.ok_or_else(|| OpenSlideError::Decode("Not a TIFF file".into()))?;

        let dir_count = tl.directory_count();
        let mut props = Properties::new();
        props.insert(properties::names::VENDOR, "aperio");

        // Parse Aperio properties from directory 0's ImageDescription
        if tl.has_tag(0, tags::IMAGE_DESCRIPTION) {
            let desc = tl.get_string(0, tags::IMAGE_DESCRIPTION)?;
            parse_aperio_properties(&desc, &mut props);
        }

        // Copy standard TIFF string properties and resolution
        set_tiff_string_props(&mut tl, 0, &mut props);
        set_resolution_props(&mut tl, 0, &mut props);

        // Duplicate Aperio-specific properties to standard OpenSlide names
        duplicate_prop(
            &mut props,
            "aperio.AppMag",
            properties::names::OBJECTIVE_POWER,
        );
        duplicate_prop(&mut props, "aperio.MPP", properties::names::MPP_X);
        duplicate_prop(&mut props, "aperio.MPP", properties::names::MPP_Y);

        // Walk directories: tiled → levels, stripped → associated images
        let mut levels: Vec<LevelData> = Vec::new();
        let mut associated_images: Vec<(String, AssociatedImage)> = Vec::new();

        for dir in 0..dir_count {
            // Check depth — we can't handle depth != 1
            if tl.has_tag(dir, tags::IMAGE_DEPTH) {
                let depth = tl.get_uint(dir, tags::IMAGE_DEPTH)?;
                if depth != 1 {
                    return Err(OpenSlideError::Decode(format!(
                        "Cannot handle ImageDepth={depth}"
                    )));
                }
            }

            if tl.is_tiled(dir) {
                // Tiled directory → pyramid level
                let compression = tl.compression(dir)?;

                // Verify compression is supported
                match compression as u16 {
                    tags::COMPRESSION_JPEG
                    | tags::COMPRESSION_JP2K_YCBCR
                    | tags::COMPRESSION_JP2K_RGB => {}
                    1 => {} // uncompressed
                    other => {
                        return Err(OpenSlideError::Decode(format!(
                            "Unsupported TIFF compression: {other}"
                        )));
                    }
                }

                let image_w = tl.get_uint(dir, tags::IMAGE_WIDTH)? as i64;
                let image_h = tl.get_uint(dir, tags::IMAGE_LENGTH)? as i64;
                let tile_w = tl.get_uint(dir, tags::TILE_WIDTH)? as i64;
                let tile_h = tl.get_uint(dir, tags::TILE_LENGTH)? as i64;

                let decoder = make_decoder_for_dir(&mut tl, dir)?;

                levels.push(LevelData {
                    info: LevelInfo {
                        width: image_w,
                        height: image_h,
                        downsample: 1.0, // computed after sorting
                        tile_width: tile_w,
                        tile_height: tile_h,
                    },
                    grid: SimpleGrid::new(image_w, image_h, tile_w, tile_h),
                    tiff_dir: dir,
                    decoder,
                    tiff_tile_width: tile_w,
                    tiff_tile_height: tile_h,
                    tilemap: None,
                    ndpi: None,
                    mirax: None,
                    zeiss: None,
                    synthetic: None,
                    dicom: None,
                });
            } else {
                // Stripped directory → associated image
                let name = get_associated_image_name(&mut tl, dir)?;
                if let Some(name) = name {
                    // Read dimensions
                    let w = tl.get_uint(dir, tags::IMAGE_WIDTH)? as i64;
                    let h = tl.get_uint(dir, tags::IMAGE_LENGTH)? as i64;

                    // Try to decode the associated image
                    match decode_associated_image(&mut tl, dir, w, h) {
                        Ok(argb_data) => {
                            // Determine ICC profile for this associated image.
                            // For the thumbnail (dir 1): if the ICC Profile name
                            // in its ImageDescription matches the main image's
                            // aperio.ICC Profile property, use the main image's
                            // ICC profile (from dir 0).
                            let icc_profile = if dir == 1 {
                                thumbnail_icc_profile(&mut tl, &props)?
                            } else {
                                tl.read_icc_profile(dir)?
                            };

                            associated_images.push((
                                name,
                                AssociatedImage {
                                    width: w,
                                    height: h,
                                    argb_data,
                                    icc_profile,
                                },
                            ));
                        }
                        Err(_) => {
                            // Skip associated images we can't decode
                        }
                    }
                }
            }
        }

        if levels.is_empty() {
            return Err(OpenSlideError::Decode("No tiled directories found".into()));
        }

        // Sort levels by width descending (largest first = level 0)
        levels.sort_by_key(|l| std::cmp::Reverse(l.info.width));

        // Compute downsample factors relative to level 0
        let base_w = levels[0].info.width as f64;
        for level in &mut levels {
            level.info.downsample = base_w / level.info.width as f64;
        }

        // Read ICC profile from level 0
        let icc_profile = tl.read_icc_profile(levels[0].tiff_dir)?;

        Ok(SlideData {
            levels,
            properties: props,
            associated_images,
            icc_profile,
            tiff: Some(tl),
        })
    }
}

/// Parse Aperio ImageDescription into properties.
///
/// Format: "Aperio ...\n...|key1 = value1|key2 = value2|..."
/// The first segment (before the first "|") is ignored.
/// Each subsequent segment is split on "=" into key/value pairs,
/// stored as "aperio.{key}".
fn parse_aperio_properties(desc: &str, props: &mut Properties) {
    let parts: Vec<&str> = desc.split('|').collect();
    // Skip the first part (the "Aperio ..." header)
    for part in parts.iter().skip(1) {
        if let Some((key, value)) = part.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            if !key.is_empty() {
                props.insert(format!("aperio.{key}"), value);
            }
        }
    }
}

/// Duplicate a property value from one key to another (for Aperio → OpenSlide mappings).
fn duplicate_prop(props: &mut Properties, src: &str, dst: &str) {
    if let Some(val) = props.get(src) {
        let val = val.to_string();
        props.insert(dst, val);
    }
}

/// Determine the associated image name for a stripped directory.
///
/// - Directory 1 is always "thumbnail"
/// - Other directories: parse the second line of ImageDescription, take the first word
fn get_associated_image_name(tl: &mut TiffLike, dir: i64) -> Result<Option<String>> {
    if dir == 1 {
        return Ok(Some("thumbnail".to_string()));
    }

    if !tl.has_tag(dir, tags::IMAGE_DESCRIPTION) {
        return Ok(None);
    }

    let desc = tl.get_string(dir, tags::IMAGE_DESCRIPTION)?;
    // Split on newlines, take second line, first word
    let lines: Vec<&str> = desc.split(['\r', '\n']).filter(|l| !l.is_empty()).collect();
    if lines.len() >= 2 {
        let second_line = lines[1];
        if let Some(name) = second_line.split_whitespace().next() {
            return Ok(Some(name.to_lowercase()));
        }
    }

    Ok(None)
}

/// Determine the ICC profile for the thumbnail (dir 1).
///
/// Aperio stores the ICC profile name in the ImageDescription properties.
/// If the thumbnail's "ICC Profile" name matches the main image's
/// "aperio.ICC Profile" property, use the main image's ICC profile (dir 0).
fn thumbnail_icc_profile(tl: &mut TiffLike, props: &Properties) -> Result<Option<Vec<u8>>> {
    // Get the main image's ICC Profile name from already-parsed properties
    let main_icc_name = props.get("aperio.ICC Profile");

    // Parse the thumbnail's ImageDescription to get its ICC Profile name
    let thumbnail_icc_name = if tl.has_tag(1, tags::IMAGE_DESCRIPTION) {
        let desc = tl.get_string(1, tags::IMAGE_DESCRIPTION)?;
        parse_thumbnail_icc_name(&desc)
    } else {
        None
    };

    // If both names exist and match, use dir 0's ICC profile
    match (main_icc_name, thumbnail_icc_name.as_deref()) {
        (Some(main), Some(thumb)) if main == thumb => tl.read_icc_profile(0),
        _ => {
            // No match or missing — check if dir 1 has its own ICC profile
            tl.read_icc_profile(1)
        }
    }
}

/// Parse the "ICC Profile" property value from an associated image's ImageDescription.
///
/// The format is the same pipe-delimited key=value as the main image description.
fn parse_thumbnail_icc_name(desc: &str) -> Option<String> {
    for part in desc.split('|').skip(1) {
        if let Some((key, value)) = part.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            if key == "ICC Profile" {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Create a tile decoder for an Aperio tiled directory.
fn make_decoder_for_dir(tl: &mut TiffLike, dir: i64) -> Result<Box<dyn TileDecoder>> {
    let compression = tl.compression(dir)? as u16;

    match compression {
        tags::COMPRESSION_JPEG => Ok(Box::new(jpeg_decoder_for_dir(tl, dir)?)),
        tags::COMPRESSION_JP2K_YCBCR | tags::COMPRESSION_JP2K_RGB => {
            // TODO: Add JPEG 2000 decoder support
            Err(OpenSlideError::Decode(format!(
                "JPEG 2000 compression ({compression}) not yet supported"
            )))
        }
        tags::COMPRESSION_LZW => Ok(Box::new(LzwDecoder::for_dir(tl, dir)?)),
        tags::COMPRESSION_DEFLATE | tags::COMPRESSION_DEFLATE_OLD => {
            Ok(Box::new(DeflateDecoder::for_dir(tl, dir)?))
        }
        tags::COMPRESSION_PACKBITS => Ok(Box::new(PackBitsDecoder::for_dir(tl, dir)?)),
        1 => {
            // Uncompressed — passthrough
            Ok(Box::new(UncompressedDecoder))
        }
        other => Err(OpenSlideError::Decode(format!(
            "Unsupported TIFF compression: {other}"
        ))),
    }
}

/// Decode a stripped associated image into ARGB pixel data.
///
/// For JPEG-compressed strips, each strip is a separate JPEG image that must
/// be decoded independently, then stitched together row by row. Shared with
/// the Philips driver, whose label/macro images are stored the same way.
pub(crate) fn decode_associated_image(
    tl: &mut TiffLike,
    dir: i64,
    w: i64,
    h: i64,
) -> Result<Vec<u32>> {
    // Tiled associated images (e.g. the Leica macro) need tile-based assembly;
    // the strip paths below only handle stripped directories.
    if tl.is_tiled(dir) {
        return decode_tiled_image(tl, dir, w, h);
    }

    let compression = if tl.has_tag(dir, tags::COMPRESSION) {
        tl.compression(dir)? as u16
    } else {
        1
    };

    match compression {
        tags::COMPRESSION_JPEG => {
            // Each strip is an independent JPEG — decode separately and stitch
            let strips = tl.read_strips(dir)?;
            let rows_per_strip = tl.rows_per_strip(dir)? as i64;
            let decoder = jpeg_decoder_for_dir(tl, dir)?;

            let pixel_count = (w * h) as usize;
            let mut argb_buf = Vec::with_capacity(pixel_count);

            for (i, strip) in strips.iter().enumerate() {
                if strip.is_empty() {
                    // Fill with transparent pixels for this strip
                    let rows = rows_per_strip.min(h - (i as i64 * rows_per_strip));
                    let count = (w * rows) as usize;
                    argb_buf.resize(argb_buf.len() + count, 0);
                    continue;
                }

                let strip_rows = rows_per_strip.min(h - (i as i64 * rows_per_strip));
                let strip_pixels = decoder.decode(strip, w as u32, strip_rows as u32)?;
                argb_buf.extend_from_slice(&strip_pixels);
            }

            // Truncate to exact pixel count (last strip may produce extra)
            argb_buf.truncate(pixel_count);
            Ok(argb_buf)
        }
        1 => {
            // Uncompressed RGB/RGBA
            let strip_data = tl.read_strip_data(dir)?;
            let pixel_count = (w * h) as usize;
            if strip_data.len() >= pixel_count * 4 {
                let mut argb_buf = Vec::with_capacity(pixel_count);
                for chunk in strip_data.chunks_exact(4) {
                    argb_buf.push(argb(chunk[3], chunk[0], chunk[1], chunk[2]));
                }
                Ok(argb_buf)
            } else if strip_data.len() >= pixel_count * 3 {
                let mut argb_buf = Vec::with_capacity(pixel_count);
                for chunk in strip_data.chunks_exact(3) {
                    argb_buf.push(argb(0xFF, chunk[0], chunk[1], chunk[2]));
                }
                Ok(argb_buf)
            } else {
                Err(OpenSlideError::Decode(format!(
                    "Associated image data size {} doesn't match {}x{} pixels",
                    strip_data.len(),
                    w,
                    h
                )))
            }
        }
        tags::COMPRESSION_LZW => {
            // LZW-compressed associated image (e.g. label).
            // Each strip is independently LZW-compressed.
            let strips = tl.read_strips(dir)?;

            let samples_per_pixel = if tl.has_tag(dir, tags::SAMPLES_PER_PIXEL) {
                tl.get_uint(dir, tags::SAMPLES_PER_PIXEL)? as usize
            } else {
                1
            };
            let predictor = if tl.has_tag(dir, tags::PREDICTOR) {
                tl.get_uint(dir, tags::PREDICTOR)?
            } else {
                1 // no predictor
            };

            // Decompress each strip and concatenate
            let pixel_count = (w * h) as usize;
            let row_bytes = w as usize * samples_per_pixel;
            let mut data = Vec::with_capacity(pixel_count * samples_per_pixel);

            for (i, strip) in strips.iter().enumerate() {
                if strip.is_empty() {
                    // Empty strip — fill with zeros
                    let rows_per_strip = tl.rows_per_strip(dir)? as usize;
                    let remaining_rows = h as usize - (i * rows_per_strip);
                    let rows = rows_per_strip.min(remaining_rows);
                    data.resize(data.len() + rows * row_bytes, 0);
                    continue;
                }

                let mut decompressed = lzw::decompress(strip).map_err(|e| {
                    OpenSlideError::Decode(format!("LZW decompression of strip {i} failed: {e}"))
                })?;

                // Apply horizontal differencing predictor within this strip
                if predictor == 2 {
                    crate::decode::apply_horizontal_predictor(
                        &mut decompressed,
                        row_bytes,
                        samples_per_pixel,
                    );
                }

                data.extend_from_slice(&decompressed);
            }

            // Convert decompressed RGB/RGBA to ARGB
            crate::decode::samples_to_argb(&data, w as u32, h as u32, samples_per_pixel)
        }
        other => Err(OpenSlideError::Decode(format!(
            "Unsupported associated image compression: {other}"
        ))),
    }
}

/// Assemble a tiled TIFF directory into a full ARGB image buffer.
///
/// Some associated images (e.g. the Leica SCN macro) are stored tiled rather
/// than stripped. Read each tile, decode it with the directory's codec, and
/// blit it into place, clipping edge tiles to the image bounds. Missing tiles
/// are left transparent.
pub(crate) fn decode_tiled_image(tl: &mut TiffLike, dir: i64, w: i64, h: i64) -> Result<Vec<u32>> {
    let tile_w = tl.get_uint(dir, tags::TILE_WIDTH)? as i64;
    let tile_h = tl.get_uint(dir, tags::TILE_LENGTH)? as i64;
    if tile_w <= 0 || tile_h <= 0 || w <= 0 || h <= 0 {
        return Err(OpenSlideError::Decode(
            "Invalid tiled associated-image dimensions".into(),
        ));
    }
    let decoder = make_decoder_for_dir(tl, dir)?;
    let across = (w + tile_w - 1) / tile_w;
    let down = (h + tile_h - 1) / tile_h;
    let tile_px_count = (tile_w * tile_h) as usize;

    let mut argb = vec![0u32; (w * h) as usize];
    for trow in 0..down {
        for tcol in 0..across {
            let idx = (trow * across + tcol) as usize;
            let data = tl.read_tile_data(dir, idx)?;
            if data.is_empty() {
                continue; // missing tile → leave transparent
            }
            let tile = decoder.decode(&data, tile_w as u32, tile_h as u32)?;
            if tile.len() < tile_px_count {
                continue; // short decode → skip rather than panic
            }
            let copy_w = (tile_w.min(w - tcol * tile_w)) as usize;
            for ty in 0..tile_h {
                let dy = trow * tile_h + ty;
                if dy >= h {
                    break;
                }
                let src = (ty * tile_w) as usize;
                let dst = (dy * w + tcol * tile_w) as usize;
                argb[dst..dst + copy_w].copy_from_slice(&tile[src..src + copy_w]);
            }
        }
    }
    Ok(argb)
}

/// Decoder for uncompressed TIFF tiles.
struct UncompressedDecoder;

impl TileDecoder for UncompressedDecoder {
    fn decode(&self, data: &[u8], width: u32, height: u32) -> Result<Vec<u32>> {
        let pixel_count = (width * height) as usize;
        if data.len() >= pixel_count * 4 {
            let mut argb_buf = Vec::with_capacity(pixel_count);
            for chunk in data.chunks_exact(4) {
                argb_buf.push(argb(chunk[3], chunk[0], chunk[1], chunk[2]));
            }
            return Ok(argb_buf);
        }
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
