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
use crate::vendor::generic_tiff::{set_resolution_props, set_tiff_string_props};
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
        if let Some(bg_str) = props.get(PROP_BACKGROUND_COLOR) {
            if let Ok(bg) = u64::from_str_radix(bg_str.trim(), 16) {
                let r = (bg >> 16) & 0xFF;
                let g = (bg >> 8) & 0xFF;
                let b = bg & 0xFF;
                props.insert(
                    properties::names::BACKGROUND_COLOR,
                    format!("{r:02X}{g:02X}{b:02X}"),
                );
            }
        }

        // Parse overlaps from OverlapsXY property
        let overlaps = parse_overlaps(props.get(PROP_OVERLAPS_XY));

        // Copy tiff.XResolution/YResolution to mpp-x/mpp-y
        // Trestle uses these TIFF tags in a non-standard way (they already contain MPP)
        duplicate_prop(&mut props, "tiff.XResolution", properties::names::MPP_X);
        duplicate_prop(&mut props, "tiff.YResolution", properties::names::MPP_Y);

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
                grid: SimpleGrid::new(effective_w, effective_h, advance_w, advance_h),
                tiff_dir: dir,
                decoder,
                tiff_tile_width: tile_w,
                tiff_tile_height: tile_h,
                tilemap: None,
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
            icc_profile: None,
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
        1 => Ok(Box::new(UncompressedDecoder)),
        other => Err(OpenSlideError::Decode(format!(
            "Unsupported TIFF compression: {other}"
        ))),
    }
}

/// Decoder for uncompressed TIFF tiles.
struct UncompressedDecoder;

impl TileDecoder for UncompressedDecoder {
    fn decode(&self, data: &[u8], width: u32, height: u32) -> Result<Vec<u32>> {
        let pixel_count = (width * height) as usize;
        if data.len() >= pixel_count * 4 {
            let mut argb_buf = Vec::with_capacity(pixel_count);
            for chunk in data.chunks_exact(4) {
                argb_buf.push(crate::color::argb(chunk[3], chunk[0], chunk[1], chunk[2]));
            }
            return Ok(argb_buf);
        }
        if data.len() >= pixel_count * 3 {
            let mut argb_buf = Vec::with_capacity(pixel_count);
            for chunk in data.chunks_exact(3) {
                argb_buf.push(crate::color::argb(0xFF, chunk[0], chunk[1], chunk[2]));
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
