//! Leica SCN vendor driver.
//!
//! Leica `.scn` files are (Big)TIFFs whose directory 0 carries an
//! ImageDescription XML document rooted at `<scn>` in the Leica namespace
//! (`http://www.leica-microsystems.com/scn/2010/{03/10,10/01}`). The XML's
//! `<collection>` holds one or more `<image>` elements; each image has a
//! `<view>` (position/size in nanometres), a `<scanSettings>` block, and a
//! `<pixels>` block whose `<dimension ifd=… z=…>` children name the TIFF
//! directories that make up that image's pyramid. The macro/overview image is
//! the one whose view exactly covers the collection (offset 0, size ==
//! collection); the high-resolution scan is a non-macro brightfield image.
//!
//! Ported from `openslide-vendor-leica.c`.
//!
//! **Coordinate space.** A level is the whole *collection* — the glass slide the
//! scanner describes — not the imaged region. `Leica-1.scn` scans a
//! 36832×38432 window out of a 53130×153470 canvas, and level 0 is the canvas,
//! with the region placed at `openslide.bounds-x`/`-y` and everything outside it
//! transparent. This matters for interoperability rather than aesthetics: sizing
//! level 0 to the region instead would make the same `(x, y)` name different
//! pixels in slidekit and OpenSlide, silently, for every Leica slide.
//!
//! The canvas is derived from the XML, all of it in nanometres: a level's scale
//! is `view.sizeX / dimension.sizeX` nm per pixel, the level is
//! `ceil(collection.size / that)`, and the region sits at
//! `floor(view.offset / that)`. The x scale is used for both axes, which is what
//! the C driver does — reproduces all five of `Leica-1.scn`'s level sizes and
//! both bounds values exactly.
//!
//! ponytail / deferred vs. the C driver:
//!  - **Single main image only.** A multi-region SCN places several main images
//!    on that shared canvas. The read path keys one TIFF directory per level
//!    (`LevelData.tiff_dir`), so a level can't draw tiles from multiple IFDs yet
//!    — `open` errors clearly in that case. Upgrade: add a per-`PlacedTile`
//!    directory to `TileMap` and thread it through `read_region_tilemap`; the
//!    canvas geometry this driver now computes is what such a level would need.
//!    The common single-region slide is fully supported.
//!  - Namespace-prefixed SCN documents aren't handled (the real slides use the
//!    default namespace, so element names are unprefixed); the base64 `<barcode>`
//!    element of the 2010/10/01 schema is ignored (the `barcode` attribute is
//!    surfaced when present).

use crate::associated::AssociatedImage;
use crate::decode::TileDecoder;
use crate::decode::tiff::TiffLike;
use crate::decode::tiff::tags;
use crate::decode::xml::{Node, parse_tree};
use crate::error::{OpenSlideError, Result};
use crate::grid::simple::SimpleGrid;
use crate::icc::IccProfile;
use crate::io::SlideReader;
use crate::level::LevelInfo;
use crate::properties::{self, Properties};
use crate::vendor::aperio::decode_associated_image;
use crate::vendor::generic_tiff::{
    check_tile_size, make_decoder_for_dir, set_resolution_props, set_tiff_string_props,
};
use crate::vendor::{LevelData, SlideData, VendorDriver};

const NS_2010_03: &str = "http://www.leica-microsystems.com/scn/2010/03/10";
const NS_2010_10: &str = "http://www.leica-microsystems.com/scn/2010/10/01";

pub struct LeicaDriver;

impl VendorDriver for LeicaDriver {
    fn name(&self) -> &str {
        "leica"
    }

    fn vendor(&self) -> &str {
        "leica"
    }

    fn detect(&self, _reader: &dyn SlideReader, tl: Option<&mut TiffLike>) -> Result<bool> {
        let tl = tl.ok_or_else(|| dec("Not a TIFF file"))?;

        if !tl.is_tiled(0) {
            return Err(dec("TIFF is not tiled"));
        }
        if !tl.has_tag(0, tags::IMAGE_DESCRIPTION) {
            return Err(dec("No ImageDescription"));
        }
        // Cheap substring check on the namespace URI — avoids building the tree
        // just to detect (the full parse happens in `open`).
        let desc = tl.get_string(0, tags::IMAGE_DESCRIPTION)?;
        if !(desc.contains(NS_2010_03) || desc.contains(NS_2010_10)) {
            return Err(dec("Not a Leica slide"));
        }
        Ok(true)
    }

    fn open(&self, _reader: Box<dyn SlideReader>, tl: Option<TiffLike>) -> Result<SlideData> {
        let mut tl = tl.ok_or_else(|| dec("Not a TIFF file"))?;

        let xml = tl.get_string(0, tags::IMAGE_DESCRIPTION)?;
        let scn = parse_scn_xml(&xml)?;

        if scn.images.iter().filter(|i| i.is_macro).count() > 1 {
            return Err(dec("Found multiple macro images"));
        }
        let mains: Vec<&ScnImage> = scn
            .images
            .iter()
            .filter(|i| !i.is_macro && i.brightfield)
            .collect();
        if mains.is_empty() {
            return Err(dec("Can't find main image"));
        }
        if mains.len() > 1 {
            // ponytail: multi-region compositing isn't expressible with one
            // tiff_dir per level (see module docs). Reject rather than stitch wrong.
            return Err(dec(
                "Leica slides with multiple main images (multi-region SCN) are not yet supported",
            ));
        }
        let main = mains[0];

        // Build pyramid levels from the main image's z=0 dimensions. Each
        // dimension is a tiled TIFF directory; sort largest-first like the other
        // TIFF drivers.
        struct Lvl {
            dir: i64,
            /// Imaged region size in this level's pixels.
            w: i64,
            h: i64,
            tw: i64,
            th: i64,
            /// Level canvas: the whole collection at this level's scale.
            canvas_w: i64,
            canvas_h: i64,
            /// Where the region sits on that canvas.
            origin_x: i64,
            origin_y: i64,
            decoder: Box<dyn TileDecoder>,
        }
        let mut lvls: Vec<Lvl> = Vec::with_capacity(main.dim_ifds.len());
        for &dir in &main.dim_ifds {
            if !tl.is_tiled(dir) {
                return Err(dec(format!("Leica level directory {dir} is not tiled")));
            }
            let w = tl.get_uint(dir, tags::IMAGE_WIDTH)? as i64;
            let h = tl.get_uint(dir, tags::IMAGE_LENGTH)? as i64;
            let tw = tl.get_uint(dir, tags::TILE_WIDTH)? as i64;
            let th = tl.get_uint(dir, tags::TILE_LENGTH)? as i64;
            check_tile_size(tw, th, dir)?;
            // Building the decoder also verifies the compression is supported.
            let decoder = make_decoder_for_dir(&mut tl, dir)?;
            // Nanometres per pixel at this level, from the region's own physical
            // extent. The x scale drives both axes, matching the C driver.
            if w <= 0 {
                return Err(dec(format!("Leica level directory {dir} has no width")));
            }
            let nm_per_px = main.view.size_x as f64 / w as f64;
            if nm_per_px <= 0.0 || !nm_per_px.is_finite() {
                return Err(dec("Leica image view has no usable physical width"));
            }
            lvls.push(Lvl {
                dir,
                w,
                h,
                tw,
                th,
                canvas_w: div_ceil_nm(scn.collection_x, nm_per_px),
                canvas_h: div_ceil_nm(scn.collection_y, nm_per_px),
                origin_x: (main.view.offset_x as f64 / nm_per_px).floor() as i64,
                origin_y: (main.view.offset_y as f64 / nm_per_px).floor() as i64,
                decoder,
            });
        }
        lvls.sort_by_key(|l| std::cmp::Reverse(l.w));
        let base_w = lvls[0].canvas_w as f64;
        let base_h = lvls[0].canvas_h as f64;
        let level0_dir = lvls[0].dir;
        let bounds = (lvls[0].origin_x, lvls[0].origin_y, lvls[0].w, lvls[0].h);

        let mut levels = Vec::with_capacity(lvls.len());
        for l in lvls {
            levels.push(LevelData {
                info: LevelInfo {
                    width: l.canvas_w,
                    height: l.canvas_h,
                    // Averaged over both axes, as OpenSlide reports it.
                    downsample: (base_w / l.canvas_w as f64 + base_h / l.canvas_h as f64) / 2.0,
                    tile_width: l.tw,
                    tile_height: l.th,
                },
                grid: SimpleGrid::new(l.w, l.h, l.tw, l.th).at_origin(l.origin_x, l.origin_y),
                tiff_dir: l.dir,
                decoder: l.decoder,
                tiff_tile_width: l.tw,
                tiff_tile_height: l.th,
                tilemap: None,
                ndpi: None,
                mirax: None,
                zeiss: None,
                synthetic: None,
                dicom: None,
            });
        }

        // Properties.
        let mut props = Properties::new();
        props.insert(properties::names::VENDOR, "leica");
        let m = &main.meta;
        if let Some(v) = &m.creation_date {
            props.insert("leica.creation-date", v.clone());
        }
        if let Some(v) = &m.device_model {
            props.insert("leica.device-model", v.clone());
        }
        if let Some(v) = &m.device_version {
            props.insert("leica.device-version", v.clone());
        }
        if let Some(v) = &m.aperture {
            props.insert("leica.aperture", v.clone());
        }
        if let Some(v) = &m.illumination_source {
            props.insert("leica.illumination-source", v.clone());
        }
        if let Some(v) = &m.objective {
            props.insert("leica.objective", v.clone());
            props.insert(properties::names::OBJECTIVE_POWER, v.clone());
        }
        if let Some(bc) = &scn.barcode {
            props.insert("leica.barcode", bc.clone());
        }
        // Where the scan actually is on the level-0 canvas. Without these a
        // caller has no way to find the tissue, since (0, 0) is empty glass.
        let (bx, by, bw, bh) = bounds;
        props.insert(properties::names::BOUNDS_X, bx.to_string());
        props.insert(properties::names::BOUNDS_Y, by.to_string());
        props.insert(properties::names::BOUNDS_WIDTH, bw.to_string());
        props.insert(properties::names::BOUNDS_HEIGHT, bh.to_string());
        // The C driver also enumerates the regions it composited. There is
        // exactly one until multi-region support lands, and it is the bounds.
        props.insert("openslide.region[0].x", bx.to_string());
        props.insert("openslide.region[0].y", by.to_string());
        props.insert("openslide.region[0].width", bw.to_string());
        props.insert("openslide.region[0].height", bh.to_string());
        set_resolution_props(&mut tl, level0_dir, &mut props);
        set_tiff_string_props(&mut tl, level0_dir, &mut props);
        // Keep the (large) XML document out of the property list.
        props.remove("tiff.ImageDescription");

        // Macro associated image: the highest-resolution dimension of the macro
        // image. Decode is best-effort (non-fatal), matching the other drivers.
        let mut associated_images: Vec<(String, AssociatedImage)> = Vec::new();
        if let Some(mac) = scn.images.iter().find(|i| i.is_macro) {
            let mut best: Option<(i64, i64, i64)> = None; // (width, dir, height)
            for &dir in &mac.dim_ifds {
                let w = tl.get_uint(dir, tags::IMAGE_WIDTH)? as i64;
                if best.is_none_or(|(bw, _, _)| w > bw) {
                    let h = tl.get_uint(dir, tags::IMAGE_LENGTH)? as i64;
                    best = Some((w, dir, h));
                }
            }
            if let Some((w, dir, h)) = best
                && let Ok(argb_data) = decode_associated_image(&mut tl, dir, w, h)
            {
                associated_images.push((
                    "macro".to_string(),
                    AssociatedImage {
                        width: w,
                        height: h,
                        argb_data,
                        icc_profile: IccProfile::absent(),
                    },
                ));
            }
        }

        let icc_profile = tl.read_icc_profile(level0_dir).ok().flatten();

        Ok(SlideData {
            levels,
            properties: props,
            associated_images,
            icc_profile: icc_profile.into(),
            tiff: Some(tl),
        })
    }
}

// ───────────────────────── XML parsing ──────────────────────────────────────

/// Per-image metadata surfaced as `leica.*` / `openslide.*` properties.
#[derive(Default)]
struct ScnImageMeta {
    creation_date: Option<String>,
    device_model: Option<String>,
    device_version: Option<String>,
    aperture: Option<String>,
    illumination_source: Option<String>,
    objective: Option<String>,
}

/// An image's `<view>`: its position and extent on the collection canvas, in
/// nanometres.
///
/// Only the x extent is kept. It sets the nanometres-per-pixel scale for *both*
/// axes, which is what the C driver does — and has to be, to reproduce its level
/// sizes: `Leica-1.scn`'s y extent implies a different scale at coarse levels
/// and would put level 3 at 2397 pixels tall where OpenSlide reports 2401.
#[derive(Clone, Copy)]
struct ScnView {
    size_x: i64,
    offset_x: i64,
    offset_y: i64,
}

/// One `<image>` from the SCN collection, reduced to what the driver needs.
struct ScnImage {
    is_macro: bool,
    brightfield: bool,
    view: ScnView,
    /// TIFF directory indices of this image's z=0 pyramid dimensions.
    dim_ifds: Vec<i64>,
    meta: ScnImageMeta,
}

/// The parsed `<scn>` document.
struct Scn {
    barcode: Option<String>,
    /// Collection extent in nanometres — the glass slide every level is sized to.
    collection_x: i64,
    collection_y: i64,
    images: Vec<ScnImage>,
}

/// `ceil(nm / nm_per_px)` as a pixel count, never below 1.
///
/// Ceiling, not rounding: a canvas has to contain the collection, and the C
/// driver's sizes only reproduce under ceiling (level 3 of `Leica-1.scn` is 2401
/// pixels tall for 2400.04 pixels of glass).
fn div_ceil_nm(nm: i64, nm_per_px: f64) -> i64 {
    ((nm as f64 / nm_per_px).ceil() as i64).max(1)
}

/// Parse the SCN ImageDescription XML into its collection + images.
fn parse_scn_xml(xml: &str) -> Result<Scn> {
    let root = parse_tree(xml)?;
    if root.name != "scn" {
        return Err(dec("XML root is not <scn>"));
    }
    let collection = root
        .find_child("collection")
        .ok_or_else(|| dec("Can't find collection element"))?;
    let coll_x = attr_i64(collection, "sizeX").ok_or_else(|| dec("Collection missing sizeX"))?;
    let coll_y = attr_i64(collection, "sizeY").ok_or_else(|| dec("Collection missing sizeY"))?;
    let barcode = collection.attr("barcode").map(str::to_string);

    let mut images = Vec::new();
    for image in collection.children_named("image") {
        let view = image
            .find_child("view")
            .ok_or_else(|| dec("Can't find view node"))?;
        let vx = attr_i64(view, "sizeX").ok_or_else(|| dec("Missing view sizeX"))?;
        let vy = attr_i64(view, "sizeY").ok_or_else(|| dec("Missing view sizeY"))?;
        let ox = attr_i64(view, "offsetX").unwrap_or(0);
        let oy = attr_i64(view, "offsetY").unwrap_or(0);
        // The macro/overview image is the one covering the whole collection.
        let is_macro = ox == 0 && oy == 0 && vx == coll_x && vy == coll_y;

        let illumination_source = text(
            image,
            &["scanSettings", "illuminationSettings", "illuminationSource"],
        )
        .map(str::to_string);
        let brightfield = illumination_source.as_deref() == Some("brightfield");

        let pixels = image
            .find_child("pixels")
            .ok_or_else(|| dec("Can't find pixels node"))?;
        let mut dim_ifds = Vec::new();
        for d in pixels.children_named("dimension") {
            // Only the z=0 plane is supported (no z-stacks).
            if attr_i64(d, "z").unwrap_or(0) != 0 {
                continue;
            }
            let ifd = attr_i64(d, "ifd").ok_or_else(|| dec("Dimension missing ifd"))?;
            dim_ifds.push(ifd);
        }
        if dim_ifds.is_empty() {
            return Err(dec("Can't find any dimensions in image"));
        }

        let device = image.find_child("device");
        let meta = ScnImageMeta {
            creation_date: text(image, &["creationDate"]).map(str::to_string),
            device_model: device.and_then(|d| d.attr("model")).map(str::to_string),
            device_version: device.and_then(|d| d.attr("version")).map(str::to_string),
            aperture: text(
                image,
                &["scanSettings", "illuminationSettings", "numericalAperture"],
            )
            .map(str::to_string),
            illumination_source,
            objective: text(image, &["scanSettings", "objectiveSettings", "objective"])
                .map(str::to_string),
        };

        images.push(ScnImage {
            is_macro,
            brightfield,
            view: ScnView {
                size_x: vx,
                offset_x: ox,
                offset_y: oy,
            },
            dim_ifds,
            meta,
        });
    }

    if images.is_empty() {
        return Err(dec("Can't find any images"));
    }

    Ok(Scn {
        barcode,
        collection_x: coll_x,
        collection_y: coll_y,
        images,
    })
}

/// Trimmed, non-empty text of the node reached by descending `path`.
fn text<'a>(node: &'a Node, path: &[&str]) -> Option<&'a str> {
    let t = node.descend(path)?.text.trim();
    (!t.is_empty()).then_some(t)
}

/// Parse an integer attribute, tolerating surrounding whitespace.
fn attr_i64(node: &Node, name: &str) -> Option<i64> {
    node.attr(name).and_then(|v| v.trim().parse::<i64>().ok())
}

fn dec(msg: impl Into<String>) -> OpenSlideError {
    OpenSlideError::Decode(msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    // One brightfield main image (a sub-region of the collection) with three
    // pyramid dimensions plus a z=1 plane that must be ignored, and one macro
    // image (covers the whole collection) with a single dimension.
    const SAMPLE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<scn xmlns="http://www.leica-microsystems.com/scn/2010/10/01">
  <collection sizeX="100000" sizeY="80000" barcode="ABC123">
    <image name="macro">
      <view sizeX="100000" sizeY="80000" offsetX="0" offsetY="0"/>
      <scanSettings>
        <illuminationSettings><illuminationSource>brightfield</illuminationSource></illuminationSettings>
        <objectiveSettings><objective>1</objective></objectiveSettings>
      </scanSettings>
      <pixels sizeX="1600" sizeY="1280">
        <dimension ifd="1" sizeX="1600" sizeY="1280" z="0"/>
      </pixels>
    </image>
    <image name="main">
      <creationDate>2020-01-02</creationDate>
      <device model="SCN400" version="1.5"/>
      <view sizeX="50000" sizeY="40000" offsetX="10000" offsetY="5000"/>
      <scanSettings>
        <illuminationSettings>
          <illuminationSource>brightfield</illuminationSource>
          <numericalAperture>0.75</numericalAperture>
        </illuminationSettings>
        <objectiveSettings><objective>20</objective></objectiveSettings>
      </scanSettings>
      <pixels sizeX="40000" sizeY="32000">
        <dimension ifd="2" sizeX="40000" sizeY="32000" z="0"/>
        <dimension ifd="4" sizeX="10000" sizeY="8000" z="0"/>
        <dimension ifd="6" sizeX="2500" sizeY="2000" z="0"/>
        <dimension ifd="99" sizeX="40000" sizeY="32000" z="1"/>
      </pixels>
    </image>
  </collection>
</scn>"#;

    #[test]
    fn classifies_main_and_macro() {
        let scn = parse_scn_xml(SAMPLE_XML).unwrap();
        assert_eq!(scn.barcode.as_deref(), Some("ABC123"));
        assert_eq!(scn.images.len(), 2);

        let mains: Vec<&ScnImage> = scn
            .images
            .iter()
            .filter(|i| !i.is_macro && i.brightfield)
            .collect();
        let macros: Vec<&ScnImage> = scn.images.iter().filter(|i| i.is_macro).collect();
        assert_eq!(mains.len(), 1);
        assert_eq!(macros.len(), 1);

        let main = mains[0];
        // z=1 dimension dropped; pyramid IFDs kept in document order.
        assert_eq!(main.dim_ifds, vec![2, 4, 6]);
        assert_eq!(main.meta.objective.as_deref(), Some("20"));
        assert_eq!(main.meta.aperture.as_deref(), Some("0.75"));
        assert_eq!(main.meta.device_model.as_deref(), Some("SCN400"));
        assert_eq!(main.meta.device_version.as_deref(), Some("1.5"));
        assert_eq!(main.meta.creation_date.as_deref(), Some("2020-01-02"));

        assert_eq!(macros[0].dim_ifds, vec![1]);
    }

    #[test]
    fn multiple_main_images_are_detectable() {
        // Two non-macro brightfield images → `open` rejects these (multi-region).
        let xml = r#"<scn><collection sizeX="100" sizeY="100">
          <image><view sizeX="50" sizeY="50" offsetX="0" offsetY="0"/>
            <scanSettings><illuminationSettings><illuminationSource>brightfield</illuminationSource></illuminationSettings></scanSettings>
            <pixels><dimension ifd="2" z="0"/></pixels></image>
          <image><view sizeX="50" sizeY="50" offsetX="50" offsetY="50"/>
            <scanSettings><illuminationSettings><illuminationSource>brightfield</illuminationSource></illuminationSettings></scanSettings>
            <pixels><dimension ifd="4" z="0"/></pixels></image>
        </collection></scn>"#;
        let scn = parse_scn_xml(xml).unwrap();
        let mains = scn
            .images
            .iter()
            .filter(|i| !i.is_macro && i.brightfield)
            .count();
        assert_eq!(mains, 2);
    }

    #[test]
    fn rejects_malformed_documents() {
        assert!(parse_scn_xml("not xml at all").is_err());
        assert!(parse_scn_xml(r#"<notscn/>"#).is_err());
        // scn without a collection
        assert!(parse_scn_xml(r#"<scn xmlns="x"><other/></scn>"#).is_err());
        // image without a view
        assert!(
            parse_scn_xml(
                r#"<scn><collection sizeX="1" sizeY="1"><image><pixels><dimension ifd="0"/></pixels></image></collection></scn>"#
            )
            .is_err()
        );
        // image with no usable dimensions
        assert!(
            parse_scn_xml(
                r#"<scn><collection sizeX="1" sizeY="1"><image><view sizeX="1" sizeY="1"/><pixels/></image></collection></scn>"#
            )
            .is_err()
        );
    }

    #[test]
    fn attr_i64_trims_whitespace() {
        let scn = parse_scn_xml(
            r#"<scn><collection sizeX=" 10 " sizeY="20"><image><view sizeX="10" sizeY="20" offsetX="0" offsetY="0"/><pixels><dimension ifd=" 3 " z="0"/></pixels></image></collection></scn>"#,
        )
        .unwrap();
        // collection covers the only image's view → it's the macro.
        assert_eq!(scn.images[0].dim_ifds, vec![3]);
        assert!(scn.images[0].is_macro);
    }
}
