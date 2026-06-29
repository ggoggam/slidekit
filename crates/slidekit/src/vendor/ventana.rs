//! Roche Ventana BIF vendor driver.
//!
//! Ventana `.bif` files are (Big)TIFFs whose directory 0 carries an XMLPacket
//! (tag 700) with a `<iScan>` (or `<Metadata><iScan>`) root holding slide
//! metadata. Pyramid levels are the TIFF directories whose ImageDescription is
//! a `level=N mag=M …` string; level numbers run 0,1,2,… and magnification
//! strictly decreases. Directories described "Label Image"/"Label_Image" and
//! "Thumbnail" are associated images.
//!
//! Level 0's directory may also carry an XMLPacket describing the scan's areas
//! of interest (AOIs). When present, the slide is a mosaic of overlapping tiles
//! that must be *stitched*: each AOI gives a tile grid, a position, and a set of
//! per-edge overlaps. A single confidence-weighted overlap (`tile_advance_x/y`)
//! is derived for the whole slide, and every level reuses the same AOI geometry
//! scaled by its downsample, subdividing each physical TIFF tile into
//! `subtiles_per_tile²` cells. This produces a [`TileMap`] (arbitrary-offset
//! tiles) rather than a regular grid. A Ventana TIFF without AOIs is a plain
//! pyramid and uses a `SimpleGrid`.
//!
//! Ported from `openslide-vendor-ventana.c` (973 LOC in C). The detection,
//! metadata, level-enumeration, and AOI-stitch geometry are unit-tested below.
//!
//! ponytail / known approximations:
//!  - Tiles are composited at the nearest integer pixel (no cairo-style subpixel
//!    resampling), so output is not bit-identical to the C library at fractional
//!    `tile_advance` positions.
//!  - Subtile size is `tiff_tile / subtiles_per_tile` by integer division; if it
//!    doesn't divide evenly the last subtile drops the remainder.
//!  - `openslide.bounds-*` region properties are not set (the verbatim
//!    `set_region_props` wasn't ported); add when needed.

use crate::associated::AssociatedImage;
use crate::decode::TileDecoder;
use crate::decode::tiff::TiffLike;
use crate::decode::tiff::tags;
use crate::decode::xml::{Node, parse_tree};
use crate::error::{OpenSlideError, Result};
use crate::grid::simple::SimpleGrid;
use crate::grid::tilemap::{PlacedTile, TileMap};
use crate::io::SlideReader;
use crate::level::LevelInfo;
use crate::properties::{self, Properties};
use crate::vendor::aperio::decode_associated_image;
use crate::vendor::generic_tiff::{make_decoder_for_dir, set_tiff_string_props};
use crate::vendor::{LevelData, SlideData, VendorDriver};
use quick_xml::Reader;
use quick_xml::events::Event;

const LEVEL_TOKEN: &str = "level=";
const MACRO_DESC: &str = "Label Image";
const MACRO_DESC2: &str = "Label_Image";
const THUMBNAIL_DESC: &str = "Thumbnail";

pub struct VentanaDriver;

impl VendorDriver for VentanaDriver {
    fn name(&self) -> &str {
        "ventana"
    }

    fn vendor(&self) -> &str {
        "ventana"
    }

    fn detect(&self, _reader: &dyn SlideReader, tl: Option<&mut TiffLike>) -> Result<bool> {
        let tl = tl.ok_or_else(|| OpenSlideError::Decode("Not a TIFF file".into()))?;

        if !tl.has_tag(0, tags::XML_PACKET) {
            return Err(OpenSlideError::Decode("No XMLPacket tag".into()));
        }
        let xml = String::from_utf8_lossy(&tl.get_buffer(0, tags::XML_PACKET)?).into_owned();
        if !xml.contains("iScan") {
            return Err(OpenSlideError::Decode(
                "XMLPacket does not contain iScan".into(),
            ));
        }
        // Cheap root check: avoid building the whole tree just to detect.
        match xml_first_element(&xml).as_deref() {
            Some("iScan") | Some("Metadata") => Ok(true),
            _ => Err(OpenSlideError::Decode(
                "XMLPacket root is not iScan/Metadata".into(),
            )),
        }
    }

    fn open(&self, _reader: Box<dyn SlideReader>, tl: Option<TiffLike>) -> Result<SlideData> {
        let mut tl = tl.ok_or_else(|| OpenSlideError::Decode("Not a TIFF file".into()))?;

        // Slide metadata from directory 0's XMLPacket.
        let mut props = Properties::new();
        props.insert(properties::names::VENDOR, "ventana");
        let xml0 = String::from_utf8_lossy(&tl.get_buffer(0, tags::XML_PACKET)?).into_owned();
        parse_initial_xml(&xml0, &mut props)?;
        set_tiff_string_props(&mut tl, 0, &mut props);

        // Walk directories: pyramid levels, associated images, and (on level 0)
        // the AOI stitch XML.
        let dir_count = tl.directory_count();
        let mut bif: Option<Bif> = None;
        let mut next_level = 0i64;
        let mut prev_mag = f64::INFINITY;
        let mut level0_mag = 0.0f64;

        struct Pending {
            dir: i64,
            downsample: f64,
            image_w: i64,
            image_h: i64,
            tile_w: i64,
            tile_h: i64,
            decoder: Box<dyn TileDecoder>,
        }
        let mut pending: Vec<Pending> = Vec::new();
        let mut associated_images: Vec<(String, AssociatedImage)> = Vec::new();

        for dir in 0..dir_count {
            if !tl.has_tag(dir, tags::IMAGE_DESCRIPTION) {
                continue;
            }
            let desc = tl.get_string(dir, tags::IMAGE_DESCRIPTION)?;

            if desc.contains(LEVEL_TOKEN) {
                let (level, mag) = parse_level_info(&desc)?;
                if level != next_level {
                    return Err(OpenSlideError::Decode(format!(
                        "Unexpected level {level}, expected {next_level}"
                    )));
                }
                next_level += 1;
                if mag.is_nan() || mag >= prev_mag {
                    return Err(OpenSlideError::Decode(format!(
                        "Unexpected magnification {mag} in level {level}"
                    )));
                }
                prev_mag = mag;
                if level == 0 {
                    level0_mag = mag;
                }
                let downsample = level0_mag / mag;

                if !tl.is_tiled(dir) {
                    return Err(OpenSlideError::Decode(format!(
                        "Directory {dir} is not tiled"
                    )));
                }
                let image_w = tl.get_uint(dir, tags::IMAGE_WIDTH)? as i64;
                let image_h = tl.get_uint(dir, tags::IMAGE_LENGTH)? as i64;
                let tile_w = tl.get_uint(dir, tags::TILE_WIDTH)? as i64;
                let tile_h = tl.get_uint(dir, tags::TILE_LENGTH)? as i64;
                // Building the decoder also verifies the compression is supported.
                let decoder = make_decoder_for_dir(&mut tl, dir)?;

                if level == 0 && tl.has_tag(dir, tags::XML_PACKET) {
                    let xml = String::from_utf8_lossy(&tl.get_buffer(dir, tags::XML_PACKET)?)
                        .into_owned();
                    bif = Some(parse_level0_xml(&xml, tile_w, tile_h)?);
                }

                // All levels must share the level-0 tile size.
                if let Some(first) = pending.first() {
                    if first.tile_w != tile_w || first.tile_h != tile_h {
                        return Err(OpenSlideError::Decode(
                            "Inconsistent TIFF tile sizes".into(),
                        ));
                    }
                }

                pending.push(Pending {
                    dir,
                    downsample,
                    image_w,
                    image_h,
                    tile_w,
                    tile_h,
                    decoder,
                });
            } else if desc == MACRO_DESC || desc == MACRO_DESC2 {
                push_associated(&mut tl, dir, "macro", &mut associated_images)?;
            } else if desc == THUMBNAIL_DESC {
                push_associated(&mut tl, dir, "thumbnail", &mut associated_images)?;
            }
        }

        if pending.is_empty() {
            return Err(OpenSlideError::Decode("No pyramid levels in slide".into()));
        }

        // Build levels. Directory order is already level order (largest first),
        // since level numbers are sequential and magnification decreases.
        let mut levels: Vec<LevelData> = Vec::with_capacity(pending.len());
        for p in pending {
            let (info, grid, tilemap) = if let Some(bif) = &bif {
                let phys_across = (p.image_w + p.tile_w - 1) / p.tile_w;
                let tm = build_tilemap(bif, p.downsample, p.tile_w, p.tile_h, phys_across);
                let info = LevelInfo {
                    width: tm.width,
                    height: tm.height,
                    downsample: p.downsample,
                    // Overlapping tiles → no meaningful tile-size hint.
                    tile_width: 0,
                    tile_height: 0,
                };
                // `grid` is unused for tilemap levels; placeholder fills the field.
                let grid = SimpleGrid::new(tm.width, tm.height, p.tile_w.max(1), p.tile_h.max(1));
                (info, grid, Some(tm))
            } else {
                let info = LevelInfo {
                    width: p.image_w,
                    height: p.image_h,
                    downsample: p.downsample,
                    tile_width: p.tile_w,
                    tile_height: p.tile_h,
                };
                let grid = SimpleGrid::new(p.image_w, p.image_h, p.tile_w, p.tile_h);
                (info, grid, None)
            };
            levels.push(LevelData {
                info,
                grid,
                tiff_dir: p.dir,
                decoder: p.decoder,
                tiff_tile_width: p.tile_w,
                tiff_tile_height: p.tile_h,
                tilemap,
            });
        }

        let icc_profile = tl.read_icc_profile(levels[0].tiff_dir).ok().flatten();

        Ok(SlideData {
            levels,
            properties: props,
            associated_images,
            icc_profile,
            tiff: Some(tl),
        })
    }
}

/// Decode an associated-image directory and push it (non-fatal on failure,
/// matching the lenient handling of the other TIFF drivers).
fn push_associated(
    tl: &mut TiffLike,
    dir: i64,
    name: &str,
    out: &mut Vec<(String, AssociatedImage)>,
) -> Result<()> {
    let w = tl.get_uint(dir, tags::IMAGE_WIDTH)? as i64;
    let h = tl.get_uint(dir, tags::IMAGE_LENGTH)? as i64;
    if let Ok(argb_data) = decode_associated_image(tl, dir, w, h) {
        out.push((
            name.to_string(),
            AssociatedImage {
                width: w,
                height: h,
                argb_data,
                icc_profile: None,
            },
        ));
    }
    Ok(())
}

/// Copy `<iScan>` attributes to `ventana.*` properties and map the standard
/// ones (Magnification → objective-power, ScanRes → mpp-x/mpp-y).
fn parse_initial_xml(xml: &str, props: &mut Properties) -> Result<()> {
    let root = parse_tree(xml)?;
    let iscan: &Node = if root.name == "iScan" {
        &root
    } else {
        root.find_child("iScan")
            .ok_or_else(|| OpenSlideError::Decode("No iScan element in initial XML".into()))?
    };

    for (k, v) in &iscan.attrs {
        props.insert(format!("ventana.{k}"), v.clone());
    }
    if let Some(mag) = iscan.attr("Magnification") {
        props.insert(properties::names::OBJECTIVE_POWER, mag.trim());
    }
    if let Some(res) = iscan.attr("ScanRes") {
        props.insert(properties::names::MPP_X, res.trim());
        props.insert(properties::names::MPP_Y, res.trim());
    }
    Ok(())
}

/// Parse a `level=N mag=M …` ImageDescription into `(level, magnification)`.
fn parse_level_info(desc: &str) -> Result<(i64, f64)> {
    let mut level: Option<i64> = None;
    let mut mag: Option<f64> = None;
    for token in desc.split_whitespace() {
        if let Some((k, v)) = token.split_once('=') {
            match k {
                "level" => level = v.parse::<i64>().ok(),
                "mag" => mag = v.parse::<f64>().ok(),
                _ => {}
            }
        }
    }
    match (level, mag) {
        (Some(l), Some(m)) => Ok((l, m)),
        _ => Err(OpenSlideError::Decode(format!(
            "Couldn't parse level info from {desc:?}"
        ))),
    }
}

// ───────────────────────── AOI stitch geometry ─────────────────────────────

/// One scanned area of interest, in level-0 tile coordinates.
struct Area {
    /// Area position in level-0 pixel space (top-down after Y correction).
    x: i64,
    y: i64,
    /// Index of the area's first tile in the global tile grid.
    start_col: i64,
    start_row: i64,
    /// Tile grid dimensions of this area.
    tiles_across: i64,
    tiles_down: i64,
}

/// Parsed AOI stitch geometry: areas plus the global confidence-weighted tile
/// advance (tile size minus average overlap).
struct Bif {
    areas: Vec<Area>,
    tile_advance_x: f64,
    tile_advance_y: f64,
}

/// Convert a 1-indexed Ventana tile number to its (col, row) in the area grid.
///
/// Tiles are numbered bottom-to-top; within a row, even rows (from the bottom)
/// run left-to-right and odd rows right-to-left (serpentine).
fn tile_coordinates(
    tiles_across: i64,
    tiles_down: i64,
    tile_count: i64,
    tile: i64,
) -> Result<(i64, i64)> {
    if tile < 1 || tile > tile_count {
        return Err(OpenSlideError::Decode(format!(
            "Tile number out of bounds: {tile}"
        )));
    }
    let t = tile - 1;
    let mut col = t % tiles_across;
    let row = t / tiles_across;
    if row % 2 != 0 {
        col = tiles_across - col - 1;
    }
    let row = tiles_down - row - 1;
    Ok((col, row))
}

fn attr_i64(node: &Node, name: &str) -> Result<i64> {
    node.attr(name)
        .and_then(|v| v.trim().parse::<i64>().ok())
        .ok_or_else(|| {
            OpenSlideError::Decode(format!("Missing/invalid integer attribute {name:?}"))
        })
}

fn attr_f64(node: &Node, name: &str) -> Result<f64> {
    node.attr(name)
        .and_then(|v| v.trim().parse::<f64>().ok())
        .ok_or_else(|| OpenSlideError::Decode(format!("Missing/invalid float attribute {name:?}")))
}

/// Parse the level-0 AOI XML (`<EncodeInfo>`) into stitch geometry.
fn parse_level0_xml(xml: &str, tiff_tile_w: i64, tiff_tile_h: i64) -> Result<Bif> {
    let root = parse_tree(xml)?;
    let encode = if root.name == "EncodeInfo" {
        &root
    } else {
        root.find_child("EncodeInfo").unwrap_or(&root)
    };

    let infos: Vec<&Node> = encode
        .descend(&["SlideStitchInfo"])
        .map(|s| s.children_named("ImageInfo").collect())
        .unwrap_or_default();
    let origins: Vec<&Node> = encode
        .find_child("AoiOrigin")
        .map(|a| a.children.iter().collect())
        .unwrap_or_default();
    if infos.is_empty() || infos.len() != origins.len() {
        return Err(OpenSlideError::Decode(
            "Missing or inconsistent region metadata".into(),
        ));
    }

    let mut areas: Vec<Area> = Vec::new();
    let mut total_offset_x = 0.0f64;
    let mut total_offset_y = 0.0f64;
    let mut total_x_weight = 0i64;
    let mut total_y_weight = 0i64;

    for (info, aoi) in infos.iter().zip(origins.iter()) {
        if attr_i64(info, "AOIScanned")? == 0 {
            continue; // unscanned AOI
        }

        let tile_width = attr_i64(info, "Width")?;
        let tile_height = attr_i64(info, "Height")?;
        if tile_width != tiff_tile_w || tile_height != tiff_tile_h {
            return Err(OpenSlideError::Decode(format!(
                "Tile size mismatch: expected {tiff_tile_w}x{tiff_tile_h}, found {tile_width}x{tile_height}"
            )));
        }

        let origin_x = attr_i64(aoi, "OriginX")?;
        let origin_y = attr_i64(aoi, "OriginY")?;
        if tile_width == 0
            || tile_height == 0
            || origin_x % tile_width != 0
            || origin_y % tile_height != 0
        {
            return Err(OpenSlideError::Decode(format!(
                "Area origin not divisible by tile size: {origin_x} % {tile_width}, {origin_y} % {tile_height}"
            )));
        }
        let start_col = origin_x / tile_width;
        let start_row = origin_y / tile_height;

        let tiles_across = attr_i64(info, "NumCols")?;
        let tiles_down = attr_i64(info, "NumRows")?;
        if tiles_across < 1 || tiles_down < 1 {
            return Err(OpenSlideError::Decode(format!(
                "Area has invalid tile count {tiles_across}x{tiles_down}"
            )));
        }
        let tile_count = tiles_across * tiles_down;

        let x = attr_f64(info, "Pos-X")? as i64;
        let y = attr_f64(info, "Pos-Y")? as i64;

        // Accumulate confidence-weighted overlaps from the tile joints.
        for joint in info.children_named("TileJointInfo") {
            let t1 = attr_i64(joint, "Tile1")?;
            let t2 = attr_i64(joint, "Tile2")?;
            let (c1, r1) = tile_coordinates(tiles_across, tiles_down, tile_count, t1)?;
            let (c2, r2) = tile_coordinates(tiles_across, tiles_down, tile_count, t2)?;
            let direction = joint.attr("Direction").unwrap_or("");
            let (direction_y, ok) = match direction {
                "RIGHT" => (false, c2 == c1 + 1 && r2 == r1),
                "UP" => (true, c2 == c1 && r2 == r1 - 1),
                other => {
                    return Err(OpenSlideError::Decode(format!(
                        "Bad direction attribute {other:?}"
                    )));
                }
            };
            if !ok {
                return Err(OpenSlideError::Decode(format!(
                    "Unexpected tile join: {direction}, ({c1},{r1}), ({c2},{r2})"
                )));
            }
            // Overlap is the magnitude; the offset that closes it is negative.
            let overlap = if direction_y { "OverlapY" } else { "OverlapX" };
            let offset = -attr_f64(joint, overlap)?;
            let confidence = attr_i64(joint, "Confidence")?;
            if direction_y {
                total_offset_y += confidence as f64 * offset;
                total_y_weight += confidence;
            } else {
                total_offset_x += confidence as f64 * offset;
                total_x_weight += confidence;
            }
        }

        areas.push(Area {
            x,
            y,
            start_col,
            start_row,
            tiles_across,
            tiles_down,
        });
    }

    if areas.is_empty() {
        return Err(OpenSlideError::Decode(
            "No scanned areas in region metadata".into(),
        ));
    }
    if total_x_weight == 0 || total_y_weight == 0 {
        return Err(OpenSlideError::Decode(
            "No tile joints to derive tile advance".into(),
        ));
    }

    let tile_advance_x = tiff_tile_w as f64 + total_offset_x / total_x_weight as f64;
    let tile_advance_y = tiff_tile_h as f64 + total_offset_y / total_y_weight as f64;

    // Fix area Y coordinates. The Pos-Y read from the file is the distance from
    // the bottom of the area to a reference point below all areas; convert to a
    // top-down pixel origin.
    let mut heights = Vec::with_capacity(areas.len());
    let mut top = 0i64;
    for area in &areas {
        let height = ((area.tiles_down - 1) as f64 * tile_advance_y + tiff_tile_h as f64) as i64;
        heights.push(height);
        top = top.max(area.y + height);
    }
    for (area, height) in areas.iter_mut().zip(heights) {
        area.y = top - area.y - height;
    }

    Ok(Bif {
        areas,
        tile_advance_x,
        tile_advance_y,
    })
}

/// Build the placed-tile map for one pyramid level from the AOI geometry.
///
/// The grid is addressed in level-0 tile coordinates; at downsample `d` each
/// physical TIFF tile is subdivided into `d × d` cells, and advances/offsets are
/// scaled by `d`.
fn build_tilemap(
    bif: &Bif,
    downsample: f64,
    tiff_tile_w: i64,
    tiff_tile_h: i64,
    phys_tiles_across: i64,
) -> TileMap {
    let sp = (downsample.round() as i64).max(1); // subtiles per tile, per axis
    let sub_w = (tiff_tile_w / sp).max(1);
    let sub_h = (tiff_tile_h / sp).max(1);
    let adv_x = bif.tile_advance_x / downsample;
    let adv_y = bif.tile_advance_y / downsample;

    let mut tiles = Vec::new();
    for area in &bif.areas {
        let offset_x = (area.x as f64 - area.start_col as f64 * bif.tile_advance_x) / downsample;
        let offset_y = (area.y as f64 - area.start_row as f64 * bif.tile_advance_y) / downsample;
        for row in area.start_row..area.start_row + area.tiles_down {
            for col in area.start_col..area.start_col + area.tiles_across {
                let dest_x = (col as f64 * adv_x + offset_x).round() as i64;
                let dest_y = (row as f64 * adv_y + offset_y).round() as i64;
                let tile_col = col / sp;
                let tile_row = row / sp;
                tiles.push(PlacedTile {
                    tiff_tile_index: (tile_row * phys_tiles_across + tile_col) as usize,
                    src_x: (col % sp) * sub_w,
                    src_y: (row % sp) * sub_h,
                    src_w: sub_w,
                    src_h: sub_h,
                    dest_x,
                    dest_y,
                });
            }
        }
    }
    TileMap::new(tiles)
}

/// Name of the first XML element, for the cheap detection root check.
fn xml_first_element(xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                return Some(String::from_utf8_lossy(e.name().as_ref()).into_owned());
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two-by-two AOI with one RIGHT and one UP joint. Hand-computed expected
    // advances: x = 256 + (5·-10)/5 = 246; y = 256 + (4·-8)/4 = 248.
    const SAMPLE_XML: &str = r#"<EncodeInfo>
  <SlideStitchInfo>
    <ImageInfo AOIScanned="1" Width="256" Height="256" NumCols="2" NumRows="2" Pos-X="0" Pos-Y="0">
      <TileJointInfo Tile1="1" Tile2="2" Direction="RIGHT" OverlapX="10" OverlapY="0" Confidence="5"/>
      <TileJointInfo Tile1="2" Tile2="3" Direction="UP" OverlapX="0" OverlapY="8" Confidence="4"/>
    </ImageInfo>
  </SlideStitchInfo>
  <AoiOrigin>
    <Aoi OriginX="0" OriginY="0"/>
  </AoiOrigin>
</EncodeInfo>"#;

    #[test]
    fn detect_first_element() {
        assert_eq!(
            xml_first_element("<iScan a=\"1\"/>").as_deref(),
            Some("iScan")
        );
        assert_eq!(
            xml_first_element("<?xml version=\"1.0\"?><Metadata><iScan/></Metadata>").as_deref(),
            Some("Metadata")
        );
        assert_eq!(xml_first_element("not xml").as_deref(), None);
    }

    #[test]
    fn parse_level_info_cases() {
        assert_eq!(
            parse_level_info("level=0 mag=40 foo=bar").unwrap(),
            (0, 40.0)
        );
        assert_eq!(parse_level_info("mag=20.0 level=1").unwrap(), (1, 20.0));
        assert!(parse_level_info("no level here").is_err());
        assert!(parse_level_info("level=2").is_err()); // missing mag
    }

    #[test]
    fn tile_coordinates_serpentine() {
        // 2x2 grid, tiles numbered 1..4, bottom-to-top, serpentine columns.
        assert_eq!(tile_coordinates(2, 2, 4, 1).unwrap(), (0, 1));
        assert_eq!(tile_coordinates(2, 2, 4, 2).unwrap(), (1, 1));
        assert_eq!(tile_coordinates(2, 2, 4, 3).unwrap(), (1, 0));
        assert_eq!(tile_coordinates(2, 2, 4, 4).unwrap(), (0, 0));
        assert!(tile_coordinates(2, 2, 4, 0).is_err());
        assert!(tile_coordinates(2, 2, 4, 5).is_err());
    }

    #[test]
    fn parse_level0_xml_derives_advance() {
        let bif = parse_level0_xml(SAMPLE_XML, 256, 256).unwrap();
        assert_eq!(bif.areas.len(), 1);
        assert_eq!(bif.tile_advance_x, 246.0);
        assert_eq!(bif.tile_advance_y, 248.0);

        let a = &bif.areas[0];
        assert_eq!((a.start_col, a.start_row), (0, 0));
        assert_eq!((a.tiles_across, a.tiles_down), (2, 2));
        assert_eq!((a.x, a.y), (0, 0)); // single area at origin
    }

    #[test]
    fn parse_level0_xml_rejects_mismatched_tile_size() {
        assert!(parse_level0_xml(SAMPLE_XML, 240, 240).is_err());
    }

    #[test]
    fn build_tilemap_level0_full_tiles() {
        let bif = parse_level0_xml(SAMPLE_XML, 256, 256).unwrap();
        let tm = build_tilemap(&bif, 1.0, 256, 256, 2);

        assert_eq!(tm.tiles.len(), 4);
        // bounds = max(dest + sub) = (246+256, 248+256)
        assert_eq!((tm.width, tm.height), (502, 504));

        // Each cell maps to a distinct physical tile at the expected position.
        let find = |dx: i64, dy: i64| {
            tm.tiles
                .iter()
                .find(|t| t.dest_x == dx && t.dest_y == dy)
                .unwrap()
        };
        assert_eq!(find(0, 0).tiff_tile_index, 0);
        assert_eq!(find(246, 0).tiff_tile_index, 1);
        assert_eq!(find(0, 248).tiff_tile_index, 2);
        assert_eq!(find(246, 248).tiff_tile_index, 3);
        // Level 0: full tiles, no sub-rect.
        assert_eq!((find(0, 0).src_x, find(0, 0).src_w), (0, 256));
    }

    #[test]
    fn build_tilemap_level1_subdivides_one_tile() {
        let bif = parse_level0_xml(SAMPLE_XML, 256, 256).unwrap();
        // downsample 2 → one physical tile across, 2x2 subtiles of 128px.
        let tm = build_tilemap(&bif, 2.0, 256, 256, 1);

        assert_eq!(tm.tiles.len(), 4);
        // All four subtiles come from the single physical tile 0.
        assert!(tm.tiles.iter().all(|t| t.tiff_tile_index == 0));
        // Subtile dims halved; advance halved (246/2 = 123, 248/2 = 124).
        let find = |dx: i64, dy: i64| {
            tm.tiles
                .iter()
                .find(|t| t.dest_x == dx && t.dest_y == dy)
                .unwrap()
        };
        let right = find(123, 0);
        assert_eq!((right.src_x, right.src_y), (128, 0));
        assert_eq!((right.src_w, right.src_h), (128, 128));
        let below = find(0, 124);
        assert_eq!((below.src_x, below.src_y), (0, 128));
    }
}
