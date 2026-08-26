//! Trestle (TIF) vendor driver.
//!
//! Trestle slides are TIFF files where directory 0's Software tag starts with
//! "MedScan" and an ImageDescription tag is present. All directories must be
//! tiled. Properties are parsed from the ImageDescription as semicolon-
//! delimited `key=value` pairs, stored as `trestle.*`.
//!
//! Tile overlaps are encoded in the `OverlapsXY` property and subtracted from
//! effective image dimensions. The background color may be specified as a hex
//! value in `trestle.Background Color`.
//!
//! Associated images: Trestle stores a macro image as an external JPEG file
//! with the `.Full` extension (e.g. `CMU-1.Full` for `CMU-1.tif`). This is
//! not currently supported in the Rust port (requires filesystem access).
//!
//! Ported from `openslide-vendor-trestle.c` (412 LOC in C).

use crate::decode::TileDecoder;
use crate::decode::check_bits_per_sample;
use crate::decode::deflate::DeflateDecoder;
use crate::decode::lzw::LzwDecoder;
use crate::decode::packbits::PackBitsDecoder;
use crate::decode::tiff::TiffLike;
use crate::decode::tiff::tags;
use crate::error::{OpenSlideError, Result};
use crate::grid::simple::SimpleGrid;
use crate::icc::IccProfile;
use crate::io::SlideReader;
use crate::level::LevelInfo;
use crate::properties::{self, Properties};
use crate::vendor::generic_tiff::{
    check_tile_size, jpeg_decoder_for_dir, set_resolution_props, set_tiff_string_props,
};
use crate::vendor::{LevelData, SlideData, VendorDriver};

const TRESTLE_SOFTWARE: &str = "MedScan";
const PROP_BACKGROUND_COLOR: &str = "trestle.Background Color";
const PROP_OBJECTIVE_POWER: &str = "trestle.Objective Power";
const PROP_OVERLAPS_XY: &str = "trestle.OverlapsXY";

pub struct TrestleDriver;

impl VendorDriver for TrestleDriver {
    fn name(&self) -> &str {
        "trestle"
    }

    fn vendor(&self) -> &str {
        "trestle"
    }

    fn detect(&self, _reader: &dyn SlideReader, tl: Option<&mut TiffLike>) -> Result<bool> {
        let tl = tl.ok_or_else(|| OpenSlideError::Decode("Not a TIFF file".into()))?;

        // Check Software field starts with "MedScan"
        if !tl.has_tag(0, tags::SOFTWARE) {
            return Err(OpenSlideError::Decode("No Software tag".into()));
        }
        let software = tl.get_string(0, tags::SOFTWARE)?;
        if !software.starts_with(TRESTLE_SOFTWARE) {
            return Err(OpenSlideError::Decode("Not a Trestle slide".into()));
        }

        // Must have ImageDescription
        if !tl.has_tag(0, tags::IMAGE_DESCRIPTION) {
            return Err(OpenSlideError::Decode("No ImageDescription".into()));
        }

        // All directories must be tiled
        let dir_count = tl.directory_count();
        for dir in 0..dir_count {
            if !tl.is_tiled(dir) {
                return Err(OpenSlideError::Decode(format!(
                    "TIFF level {dir} is not tiled"
                )));
            }
        }

        Ok(true)
    }

    fn open(&self, _reader: Box<dyn SlideReader>, tl: Option<TiffLike>) -> Result<SlideData> {
        let mut tl = tl.ok_or_else(|| OpenSlideError::Decode("Not a TIFF file".into()))?;

        let dir_count = tl.directory_count();
        let mut props = Properties::new();
        props.insert(properties::names::VENDOR, "trestle");

        // Copy standard TIFF string properties and resolution from dir 0
        set_tiff_string_props(&mut tl, 0, &mut props);
        set_resolution_props(&mut tl, 0, &mut props);

        // Parse Trestle properties from ImageDescription
        if tl.has_tag(0, tags::IMAGE_DESCRIPTION) {
            let desc = tl.get_string(0, tags::IMAGE_DESCRIPTION)?;
            parse_trestle_properties(&desc, &mut props);
        }

        // Duplicate objective power
        duplicate_prop(
            &mut props,
            PROP_OBJECTIVE_POWER,
            properties::names::OBJECTIVE_POWER,
        );

        // Parse background color
        if let Some(bg_str) = props.get(PROP_BACKGROUND_COLOR)
            && let Ok(bg) = u64::from_str_radix(bg_str.trim(), 16)
        {
            let r = (bg >> 16) & 0xFF;
            let g = (bg >> 8) & 0xFF;
            let b = bg & 0xFF;
            props.insert(
                properties::names::BACKGROUND_COLOR,
                format!("{r:02X}{g:02X}{b:02X}"),
            );
        }

        // Parse overlaps from OverlapsXY property
        let overlaps = parse_overlaps(props.get(PROP_OVERLAPS_XY));

        // Trestle uses XResolution/YResolution non-standardly: they already hold
        // microns per pixel, so the generic `dividend / res` conversion
        // `set_resolution_props` ran above is wrong here and has to be replaced
        // with the raw tag value.
        //
        // Reading the tags is the point. This used to copy from a
        // `tiff.XResolution` property, which nothing in the crate produces —
        // `set_tiff_string_props` maps only the ASCII tags — so it never fired.
        // Trestle files carry `ResolutionUnit = None`, which `set_resolution_props`
        // bails on, and the slides ended up with no mpp at all.
        set_mpp_from_raw_resolution(&mut tl, &mut props);

        // Build levels — all directories are tiled levels (linear ordering)
        let mut levels: Vec<LevelData> = Vec::new();
        let mut has_overlaps = false;

        for dir in 0..dir_count {
            // Verify compression is supported
            let compression = if tl.has_tag(dir, tags::COMPRESSION) {
                tl.compression(dir)? as u16
            } else {
                1 // uncompressed
            };
            match compression {
                tags::COMPRESSION_JPEG => {}
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
            // `is_tiled` only proves the tags are present, not that they hold a
            // usable size; every tile count below divides by these.
            check_tile_size(tile_w, tile_h, dir)?;

            let tiles_across = (image_w + tile_w - 1) / tile_w;
            let tiles_down = (image_h + tile_h - 1) / tile_h;

            // Get overlaps for this directory
            let (overlap_x, overlap_y) = if (dir as usize) < overlaps.len() {
                let ox = overlaps[dir as usize].0;
                let oy = overlaps[dir as usize].1;
                if ox != 0 || oy != 0 {
                    has_overlaps = true;
                }
                (ox, oy)
            } else {
                (0, 0)
            };

            // Adjust effective image dimensions by subtracting overlaps
            // There are (tiles-1) overlaps in each dimension
            let effective_w = if image_w >= tile_w {
                image_w - (tiles_across - 1) * overlap_x as i64
            } else {
                image_w
            };
            let effective_h = if image_h >= tile_h {
                image_h - (tiles_down - 1) * overlap_y as i64
            } else {
                image_h
            };

            let decoder = make_decoder_for_dir(&mut tl, dir)?;

            // Use a grid with effective tile advances (tile_size - overlap)
            let advance_w = tile_w - overlap_x as i64;
            let advance_h = tile_h - overlap_y as i64;

            levels.push(LevelData {
                info: LevelInfo {
                    width: effective_w,
                    height: effective_h,
                    downsample: 1.0, // computed after
                    tile_width: tile_w,
                    tile_height: tile_h,
                },
                // The advance is what separates tiles; the real tile is
                // `tile_w` wide and overlaps its neighbour. The grid needs both
                // to count columns correctly.
                grid: SimpleGrid::new(effective_w, effective_h, advance_w, advance_h)
                    .with_tile_span(tile_w, tile_h),
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
        }

        if levels.is_empty() {
            return Err(OpenSlideError::Decode("No tiled directories found".into()));
        }

        // If any level has overlaps, clear tile size hints (they'd mislead applications)
        if has_overlaps {
            for level in &mut levels {
                level.info.tile_width = 0;
                level.info.tile_height = 0;
            }
        }

        // Compute downsample factors relative to level 0
        let base_w = levels[0].info.width as f64;
        for level in &mut levels {
            level.info.downsample = base_w / level.info.width as f64;
        }

        Ok(SlideData {
            levels,
            properties: props,
            associated_images: vec![],
            icc_profile: IccProfile::absent(),
            tiff: Some(tl),
        })
    }
}

/// Parse Trestle ImageDescription into properties.
///
/// Format: "key1=value1;key2=value2;..."
/// Each segment is split on "=" into key/value, stored as "trestle.{key}".
fn parse_trestle_properties(desc: &str, props: &mut Properties) {
    for part in desc.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((key, value)) = part.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            if !key.is_empty() {
                props.insert(format!("trestle.{key}"), value);
            }
        }
    }
}

/// Write `openslide.mpp-x`/`-y` straight from `XResolution`/`YResolution`.
///
/// For Trestle those tags hold microns per pixel directly rather than pixels
/// per unit, so no conversion applies. Leaves whatever is already there when a
/// tag is missing or zero.
fn set_mpp_from_raw_resolution(tl: &mut TiffLike, props: &mut Properties) {
    for (tag, name) in [
        (tags::X_RESOLUTION, properties::names::MPP_X),
        (tags::Y_RESOLUTION, properties::names::MPP_Y),
    ] {
        if tl.has_tag(0, tag)
            && let Ok(res) = tl.get_float(0, tag)
            && res > 0.0
        {
            props.insert(name, format!("{res:.10}"));
        }
    }
}

/// Parse the OverlapsXY property into (x, y) overlap pairs per directory.
///
/// Format: "x0 y0 x1 y1 ..." — space-separated integers, two per directory.
fn parse_overlaps(overlap_str: Option<&str>) -> Vec<(i32, i32)> {
    let Some(s) = overlap_str else {
        return vec![];
    };

    let values: Vec<i32> = s
        .split_whitespace()
        .filter_map(|v| v.parse::<i32>().ok())
        .collect();

    values
        .chunks(2)
        .map(|c| (c[0], *c.get(1).unwrap_or(&0)))
        .collect()
}

/// Duplicate a property value from one key to another.
fn duplicate_prop(props: &mut Properties, src: &str, dst: &str) {
    if let Some(val) = props.get(src) {
        let val = val.to_string();
        props.insert(dst, val);
    }
}

/// Create a tile decoder for a Trestle tiled directory.
fn make_decoder_for_dir(tl: &mut TiffLike, dir: i64) -> Result<Box<dyn TileDecoder>> {
    let compression = if tl.has_tag(dir, tags::COMPRESSION) {
        tl.compression(dir)? as u16
    } else {
        1 // uncompressed
    };

    match compression {
        tags::COMPRESSION_JPEG => Ok(Box::new(jpeg_decoder_for_dir(tl, dir)?)),
        tags::COMPRESSION_LZW => Ok(Box::new(LzwDecoder::for_dir(tl, dir)?)),
        tags::COMPRESSION_DEFLATE | tags::COMPRESSION_DEFLATE_OLD => {
            Ok(Box::new(DeflateDecoder::for_dir(tl, dir)?))
        }
        tags::COMPRESSION_PACKBITS => Ok(Box::new(PackBitsDecoder::for_dir(tl, dir)?)),
        1 => {
            check_bits_per_sample(tl, dir)?;
            Ok(Box::new(UncompressedDecoder))
        }
        other => Err(OpenSlideError::Decode(format!(
            "Unsupported TIFF compression: {other}"
        ))),
    }
}

/// Decoder for uncompressed TIFF tiles.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_trestle_properties_basic() {
        let mut props = Properties::new();
        parse_trestle_properties("Objective Power=20;Background Color=FFFFFF", &mut props);
        assert_eq!(props.get("trestle.Objective Power"), Some("20"));
        assert_eq!(props.get("trestle.Background Color"), Some("FFFFFF"));
    }

    #[test]
    fn parse_trestle_properties_empty_segments() {
        let mut props = Properties::new();
        parse_trestle_properties(";;Key=Value;;", &mut props);
        assert_eq!(props.get("trestle.Key"), Some("Value"));
        assert_eq!(props.len(), 1);
    }

    #[test]
    fn parse_trestle_properties_no_value() {
        let mut props = Properties::new();
        parse_trestle_properties("JustAKey;Another=has value", &mut props);
        // "JustAKey" has no "=", should be skipped
        assert_eq!(props.get("trestle.JustAKey"), None);
        assert_eq!(props.get("trestle.Another"), Some("has value"));
    }

    #[test]
    fn parse_overlaps_basic() {
        let overlaps = parse_overlaps(Some("10 20 5 15"));
        assert_eq!(overlaps, vec![(10, 20), (5, 15)]);
    }

    #[test]
    fn parse_overlaps_empty() {
        assert!(parse_overlaps(None).is_empty());
        assert!(parse_overlaps(Some("")).is_empty());
    }

    #[test]
    fn parse_overlaps_odd_count() {
        let overlaps = parse_overlaps(Some("10 20 5"));
        assert_eq!(overlaps, vec![(10, 20), (5, 0)]);
    }
}
