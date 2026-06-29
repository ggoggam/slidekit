//! Philips TIFF vendor driver.
//!
//! Philips slides are TIFF files where directory 0's Software tag starts with
//! "Philips" and the ImageDescription is an XML document with a
//! `<DataObject ObjectType="DPUfsImport">` root. Tiled directories are pyramid
//! levels (the first one, then any with the reduced-resolution subfiletype);
//! stripped directories whose ImageDescription starts with "Label"/"Macro" are
//! associated images.
//!
//! The TIFF tags report incorrect dimensions for the reduced-resolution levels,
//! so this driver — like the C library — overrides each sub-level's downsample
//! and dimensions from the per-level `DICOM_PIXEL_SPACING` values found in the
//! XML (`fix_level_dimensions`). Parsing the XML is therefore mandatory for
//! correctness, not just for metadata.
//!
//! Ported from `openslide-vendor-philips-tiff.c` (711 LOC in C).
//!
//! Deferred vs. the C driver (ponytail): the `philips.*` generic property dump,
//! the barcode property (needs base64), and the base64-JPEG-in-XML fallback for
//! associated images. Real test slides store label/macro as TIFF directories,
//! which the shared `decode_associated_image` path handles.

use crate::associated::AssociatedImage;
use crate::decode::tiff::TiffLike;
use crate::decode::tiff::tags;
use crate::error::{OpenSlideError, Result};
use crate::grid::simple::SimpleGrid;
use crate::io::SlideReader;
use crate::level::LevelInfo;
use crate::properties::{self, Properties};
use crate::vendor::aperio::decode_associated_image;
use crate::vendor::generic_tiff::{make_decoder_for_dir, set_tiff_string_props};
use crate::vendor::{LevelData, SlideData, VendorDriver};
use quick_xml::Reader;
use quick_xml::events::Event;

const PHILIPS_SOFTWARE: &str = "Philips";
const XML_ROOT: &str = "DataObject";
const XML_ROOT_TYPE_VALUE: &str = "DPUfsImport";
const LABEL_DESCRIPTION: &str = "Label";
const MACRO_DESCRIPTION: &str = "Macro";

pub struct PhilipsDriver;

impl VendorDriver for PhilipsDriver {
    fn name(&self) -> &str {
        "philips-tiff"
    }

    fn vendor(&self) -> &str {
        "philips"
    }

    fn detect(&self, _reader: &dyn SlideReader, tl: Option<&mut TiffLike>) -> Result<bool> {
        let tl = tl.ok_or_else(|| OpenSlideError::Decode("Not a TIFF file".into()))?;

        // Software tag must start with "Philips".
        if !tl.has_tag(0, tags::SOFTWARE) {
            return Err(OpenSlideError::Decode("No Software tag".into()));
        }
        let software = tl.get_string(0, tags::SOFTWARE)?;
        if !software.starts_with(PHILIPS_SOFTWARE) {
            return Err(OpenSlideError::Decode("Not a Philips slide".into()));
        }

        // ImageDescription must be the Philips XML document.
        if !tl.has_tag(0, tags::IMAGE_DESCRIPTION) {
            return Err(OpenSlideError::Decode("No ImageDescription".into()));
        }
        let desc = tl.get_string(0, tags::IMAGE_DESCRIPTION)?;
        if !xml_root_is_philips(&desc) {
            return Err(OpenSlideError::Decode(
                "ImageDescription is not a Philips DPUfsImport document".into(),
            ));
        }

        Ok(true)
    }

    fn open(&self, _reader: Box<dyn SlideReader>, tl: Option<TiffLike>) -> Result<SlideData> {
        let mut tl = tl.ok_or_else(|| OpenSlideError::Decode("Not a TIFF file".into()))?;

        // Parse the XML metadata from directory 0's ImageDescription.
        let xml = tl.get_string(0, tags::IMAGE_DESCRIPTION)?;
        let root = parse_xml_tree(&xml)?;

        // Locate the single WSI scanned image. More (or fewer) than one is an error.
        let scanned = root
            .child_attr_node("PIM_DP_SCANNED_IMAGES")
            .ok_or_else(|| OpenSlideError::Decode("No scanned images in XML".into()))?;
        let wsi_images: Vec<&Node> = array_objects(scanned)
            .filter(|d| d.child_attr_text("PIM_DP_IMAGE_TYPE") == Some("WSI"))
            .collect();
        if wsi_images.len() != 1 {
            return Err(OpenSlideError::Decode(format!(
                "Expected one WSI image, found {}",
                wsi_images.len()
            )));
        }
        let wsi = wsi_images[0];

        // Per-level pixel spacings (one PixelDataRepresentation per level).
        let level_spacings: Vec<(f64, f64)> = wsi
            .child_attr_node("PIIM_PIXEL_DATA_REPRESENTATION_SEQUENCE")
            .map(|seq| {
                array_objects(seq)
                    .filter_map(|repr| repr.child_attr_text("DICOM_PIXEL_SPACING"))
                    .filter_map(parse_pixel_spacing)
                    .collect()
            })
            .unwrap_or_default();

        // Base resolution (for mpp) and objective power.
        let base_spacing = wsi
            .child_attr_text("DICOM_PIXEL_SPACING")
            .and_then(parse_pixel_spacing);
        let objective_power = wsi
            .child_attr_text("DICOM_DERIVATION_DESCRIPTION")
            .and_then(parse_objective_power);

        // Walk directories: tiled → pyramid level, Label/Macro → associated image.
        let dir_count = tl.directory_count();
        let mut levels: Vec<LevelData> = Vec::new();
        let mut associated_images: Vec<(String, AssociatedImage)> = Vec::new();

        for dir in 0..dir_count {
            if tl.is_tiled(dir) {
                // Only the first tiled directory may lack the reduced-resolution flag.
                if !levels.is_empty() {
                    let subfiletype = if tl.has_tag(dir, tags::SUBFILE_TYPE) {
                        tl.get_uint(dir, tags::SUBFILE_TYPE)?
                    } else {
                        0
                    };
                    if subfiletype & tags::FILETYPE_REDUCEDIMAGE == 0 {
                        return Err(OpenSlideError::Decode(format!(
                            "Directory {dir} is not reduced-resolution"
                        )));
                    }
                }

                let image_w = tl.get_uint(dir, tags::IMAGE_WIDTH)? as i64;
                let image_h = tl.get_uint(dir, tags::IMAGE_LENGTH)? as i64;
                let tile_w = tl.get_uint(dir, tags::TILE_WIDTH)? as i64;
                let tile_h = tl.get_uint(dir, tags::TILE_LENGTH)? as i64;

                // Levels must be stored largest-first.
                if let Some(prev) = levels.last() {
                    if image_w > prev.grid.image_w || image_h > prev.grid.image_h {
                        return Err(OpenSlideError::Decode(format!(
                            "Unexpected dimensions for directory {dir}"
                        )));
                    }
                }

                // Verifying the decoder doubles as the "codec configured?" check.
                let decoder = make_decoder_for_dir(&mut tl, dir)?;

                levels.push(LevelData {
                    info: LevelInfo {
                        width: image_w,
                        height: image_h,
                        downsample: 1.0, // sub-levels fixed up below
                        tile_width: tile_w,
                        tile_height: tile_h,
                    },
                    // Grid covers the (possibly padded) TIFF tile layout so tile
                    // indexing stays correct; `info` carries the clipped dims.
                    grid: SimpleGrid::new(image_w, image_h, tile_w, tile_h),
                    tiff_dir: dir,
                    decoder,
                    tiff_tile_width: tile_w,
                    tiff_tile_height: tile_h,
                });
            } else if tl.has_tag(dir, tags::IMAGE_DESCRIPTION) {
                let desc = tl.get_string(dir, tags::IMAGE_DESCRIPTION)?;
                let name = if desc.starts_with(LABEL_DESCRIPTION) {
                    Some("label")
                } else if desc.starts_with(MACRO_DESCRIPTION) {
                    Some("macro")
                } else {
                    None
                };
                if let Some(name) = name {
                    let w = tl.get_uint(dir, tags::IMAGE_WIDTH)? as i64;
                    let h = tl.get_uint(dir, tags::IMAGE_LENGTH)? as i64;
                    // Skip associated images we can't decode (non-fatal).
                    if let Ok(argb_data) = decode_associated_image(&mut tl, dir, w, h) {
                        associated_images.push((
                            name.to_string(),
                            AssociatedImage {
                                width: w,
                                height: h,
                                argb_data,
                                icc_profile: None,
                            },
                        ));
                    }
                }
            }
        }

        if levels.is_empty() {
            return Err(OpenSlideError::Decode("No tiled directories found".into()));
        }

        fix_level_dimensions(&mut levels, &level_spacings)?;

        // Properties.
        let mut props = Properties::new();
        props.insert(properties::names::VENDOR, "philips");
        set_tiff_string_props(&mut tl, 0, &mut props);
        // Keep the (huge) XML document out of the property list.
        props.remove("tiff.ImageDescription");
        if let Some((w, h)) = base_spacing {
            // Pixel spacing is in mm; mpp wants microns.
            props.insert(properties::names::MPP_X, format!("{:.10}", 1e3 * w));
            props.insert(properties::names::MPP_Y, format!("{:.10}", 1e3 * h));
        }
        if let Some(op) = objective_power {
            props.insert(properties::names::OBJECTIVE_POWER, op.to_string());
        }

        Ok(SlideData {
            levels,
            properties: props,
            associated_images,
            icc_profile: None,
            tiff: Some(tl),
        })
    }
}

/// Override sub-level downsamples and dimensions from the XML pixel spacings.
///
/// The TIFF tags carry incorrect dimensions for reduced-resolution levels.
/// Level 0 keeps its TIFF dimensions; each sub-level's downsample is the rounded
/// ratio of its pixel spacing to level 0's, and its dimensions are level 0's
/// divided by that integer downsample.
fn fix_level_dimensions(levels: &mut [LevelData], spacings: &[(f64, f64)]) -> Result<()> {
    if spacings.len() != levels.len() {
        return Err(OpenSlideError::Decode(format!(
            "Couldn't get level downsamples: {} spacings for {} levels",
            spacings.len(),
            levels.len()
        )));
    }

    let (l0_w, l0_h) = spacings[0];
    if l0_w == 0.0 || l0_h == 0.0 {
        return Err(OpenSlideError::Decode("Zero level-0 pixel spacing".into()));
    }
    let base_w = levels[0].info.width as f64;
    let base_h = levels[0].info.height as f64;

    for i in 1..levels.len() {
        let (w, h) = spacings[i];
        // Assume integer downsamples (true for Philips) to dodge float error.
        let downsample = (((w / l0_w) + (h / l0_h)) / 2.0).round().max(1.0);
        levels[i].info.downsample = downsample;
        levels[i].info.width = (base_w / downsample) as i64;
        levels[i].info.height = (base_h / downsample) as i64;
    }

    Ok(())
}

// ───────────────────────── XML parsing ─────────────────────────────────────
//
// ponytail: a minimal in-house XML tree, scoped to this one driver. The Philips
// `DataObject`/`Attribute` shape is vendor-specific, so a generic XML module
// would be speculative — lift this into `decode/xml.rs` when a second vendor
// needs XML.

/// A minimal XML element tree node.
struct Node {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<Node>,
    text: String,
}

impl Node {
    fn empty() -> Self {
        Node {
            name: String::new(),
            attrs: Vec::new(),
            children: Vec::new(),
            text: String::new(),
        }
    }

    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Direct child `<Attribute Name="name">` node, if any.
    fn child_attr_node(&self, name: &str) -> Option<&Node> {
        self.children
            .iter()
            .find(|c| c.name == "Attribute" && c.attr("Name") == Some(name))
    }

    /// Trimmed text of the direct child `<Attribute Name="name">`.
    fn child_attr_text(&self, name: &str) -> Option<&str> {
        self.child_attr_node(name).map(|n| n.text.trim())
    }
}

/// `<DataObject>` elements inside an attribute node's `<Array>`.
fn array_objects(attr_node: &Node) -> impl Iterator<Item = &Node> {
    attr_node
        .children
        .iter()
        .filter(|c| c.name == "Array")
        .flat_map(|arr| arr.children.iter().filter(|c| c.name == "DataObject"))
}

fn start_node(e: &quick_xml::events::BytesStart) -> Node {
    let mut node = Node::empty();
    node.name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    for a in e.attributes().flatten() {
        let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
        let value = String::from_utf8_lossy(a.value.as_ref()).into_owned();
        node.attrs.push((key, value));
    }
    node
}

/// Parse an XML document into a tree, returning the root element.
fn parse_xml_tree(xml: &str) -> Result<Node> {
    let mut reader = Reader::from_str(xml);
    // Sentinel root collects the real document root as its only child.
    let mut stack: Vec<Node> = vec![Node::empty()];
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => stack.push(start_node(&e)),
            Ok(Event::Empty(e)) => {
                let node = start_node(&e);
                stack.last_mut().unwrap().children.push(node);
            }
            Ok(Event::End(_)) => {
                if stack.len() > 1 {
                    let node = stack.pop().unwrap();
                    stack.last_mut().unwrap().children.push(node);
                }
            }
            Ok(Event::Text(t)) => {
                let s = String::from_utf8_lossy(t.as_ref());
                if !s.trim().is_empty() {
                    stack.last_mut().unwrap().text.push_str(&s);
                }
            }
            Ok(Event::CData(c)) => {
                let s = String::from_utf8_lossy(c.as_ref());
                stack.last_mut().unwrap().text.push_str(&s);
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(OpenSlideError::Decode(format!("XML parse error: {e}"))),
            _ => {}
        }
    }
    stack
        .pop()
        .and_then(|sentinel| sentinel.children.into_iter().next())
        .ok_or_else(|| OpenSlideError::Decode("Empty XML document".into()))
}

/// Cheap detection check: the first element must be `<DataObject
/// ObjectType="DPUfsImport">`. Avoids building the whole tree.
fn xml_root_is_philips(xml: &str) -> bool {
    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name_ok = e.name().as_ref() == XML_ROOT.as_bytes();
                let type_ok = e.attributes().flatten().any(|a| {
                    a.key.as_ref() == b"ObjectType"
                        && a.value.as_ref() == XML_ROOT_TYPE_VALUE.as_bytes()
                });
                return name_ok && type_ok;
            }
            Ok(Event::Eof) | Err(_) => return false,
            _ => {}
        }
    }
}

/// Parse a `DICOM_PIXEL_SPACING` value into `(width, height)` in mm.
///
/// The value is two space-separated, optionally quoted floats given as
/// `row_spacing column_spacing`; width is the column spacing.
fn parse_pixel_spacing(spacing: &str) -> Option<(f64, f64)> {
    let parts: Vec<f64> = spacing
        .split_whitespace()
        .map(|p| p.trim_matches('"'))
        .filter_map(|p| p.parse::<f64>().ok())
        .collect();
    match parts.len() {
        2 => Some((parts[1], parts[0])), // (width = column, height = row)
        _ => None,
    }
}

/// Extract the objective power from a `DICOM_DERIVATION_DESCRIPTION` string.
///
/// The description is dash-separated `key=value` pairs; the first comma-part of
/// the `levels=` value is the objective power, accepted only when it looks
/// reasonable (1..=200). Parsing stops at `sourceFilename` (its quoted value may
/// contain dashes).
fn parse_objective_power(derivation: &str) -> Option<u32> {
    for item in derivation.split('-') {
        let mut kv = item.splitn(2, '=');
        let key = kv.next().unwrap_or("");
        if key == "sourceFilename" {
            break;
        }
        if key == "levels" {
            let first = kv.next()?.split(',').next()?;
            let n: u64 = first.parse().ok()?;
            return (1..=200).contains(&n).then_some(n as u32);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_XML: &str = r#"<?xml version="1.0"?>
<DataObject ObjectType="DPUfsImport">
  <Attribute Name="PIM_DP_UFS_BARCODE" PMSVR="IString">QUJD</Attribute>
  <Attribute Name="PIM_DP_SCANNED_IMAGES" PMSVR="IDataObjectArray">
    <Array>
      <DataObject ObjectType="DPScannedImage">
        <Attribute Name="PIM_DP_IMAGE_TYPE" PMSVR="IString">MACROIMAGE</Attribute>
      </DataObject>
      <DataObject ObjectType="DPScannedImage">
        <Attribute Name="PIM_DP_IMAGE_TYPE" PMSVR="IString">WSI</Attribute>
        <Attribute Name="DICOM_PIXEL_SPACING" PMSVR="IDoubleArray">"0.00025" "0.00025"</Attribute>
        <Attribute Name="DICOM_DERIVATION_DESCRIPTION" PMSVR="IString">tiff-useBigTIFF=1-levels=40,20,10-sourceFilename="a-b.tiff"</Attribute>
        <Attribute Name="PIIM_PIXEL_DATA_REPRESENTATION_SEQUENCE" PMSVR="IDataObjectArray">
          <Array>
            <DataObject ObjectType="PixelDataRepresentation">
              <Attribute Name="DICOM_PIXEL_SPACING">"0.00025" "0.00025"</Attribute>
            </DataObject>
            <DataObject ObjectType="PixelDataRepresentation">
              <Attribute Name="DICOM_PIXEL_SPACING">"0.0005" "0.0005"</Attribute>
            </DataObject>
            <DataObject ObjectType="PixelDataRepresentation">
              <Attribute Name="DICOM_PIXEL_SPACING">"0.00099" "0.00101"</Attribute>
            </DataObject>
          </Array>
        </Attribute>
      </DataObject>
    </Array>
  </Attribute>
</DataObject>"#;

    #[test]
    fn root_detection() {
        assert!(xml_root_is_philips(SAMPLE_XML));
        assert!(!xml_root_is_philips(
            r#"<DataObject ObjectType="Something">"#
        ));
        assert!(!xml_root_is_philips("<NotPhilips/>"));
        assert!(!xml_root_is_philips("not xml at all"));
    }

    #[test]
    fn parse_pixel_spacing_quoted_and_bare() {
        assert_eq!(parse_pixel_spacing(r#""0.25" "0.50""#), Some((0.50, 0.25)));
        assert_eq!(parse_pixel_spacing("0.25 0.50"), Some((0.50, 0.25)));
        assert_eq!(parse_pixel_spacing("0.25"), None);
        assert_eq!(parse_pixel_spacing(""), None);
    }

    #[test]
    fn parse_objective_power_cases() {
        assert_eq!(
            parse_objective_power("tiff-levels=40,20,10-sourceFilename=\"x\""),
            Some(40)
        );
        // sourceFilename appears first → stop before any levels= after it
        assert_eq!(
            parse_objective_power("sourceFilename=\"a-levels=99\""),
            None
        );
        // out-of-range value rejected
        assert_eq!(parse_objective_power("levels=99999,20"), None);
        assert_eq!(parse_objective_power("no-levels-here"), None);
    }

    #[test]
    fn tree_extraction_finds_wsi_and_spacings() {
        let root = parse_xml_tree(SAMPLE_XML).unwrap();
        assert_eq!(root.name, "DataObject");

        let scanned = root.child_attr_node("PIM_DP_SCANNED_IMAGES").unwrap();
        let wsi: Vec<&Node> = array_objects(scanned)
            .filter(|d| d.child_attr_text("PIM_DP_IMAGE_TYPE") == Some("WSI"))
            .collect();
        assert_eq!(wsi.len(), 1);

        let wsi = wsi[0];
        assert_eq!(
            wsi.child_attr_text("DICOM_PIXEL_SPACING"),
            Some(r#""0.00025" "0.00025""#)
        );

        let seq = wsi
            .child_attr_node("PIIM_PIXEL_DATA_REPRESENTATION_SEQUENCE")
            .unwrap();
        let spacings: Vec<(f64, f64)> = array_objects(seq)
            .filter_map(|r| r.child_attr_text("DICOM_PIXEL_SPACING"))
            .filter_map(parse_pixel_spacing)
            .collect();
        assert_eq!(spacings.len(), 3);
        // downsamples derived from these spacings round to 1, 2, 4
        assert_eq!(spacings[0], (0.00025, 0.00025));
    }

    #[test]
    fn fix_level_dimensions_rounds_to_integer_downsamples() {
        let spacings = vec![(0.00025, 0.00025), (0.0005, 0.0005), (0.00099, 0.00101)];
        let mk = |w: i64, h: i64| LevelData {
            info: LevelInfo {
                width: w,
                height: h,
                downsample: 1.0,
                tile_width: 512,
                tile_height: 512,
            },
            grid: SimpleGrid::new(w, h, 512, 512),
            tiff_dir: 0,
            decoder: Box::new(crate::decode::jpeg::JpegDecoder::new()),
            tiff_tile_width: 512,
            tiff_tile_height: 512,
        };
        let mut levels = vec![mk(40000, 30000), mk(20001, 15001), mk(10005, 7505)];
        fix_level_dimensions(&mut levels, &spacings).unwrap();

        assert_eq!(levels[0].info.downsample, 1.0);
        assert_eq!(
            (levels[0].info.width, levels[0].info.height),
            (40000, 30000)
        );

        assert_eq!(levels[1].info.downsample, 2.0);
        assert_eq!(
            (levels[1].info.width, levels[1].info.height),
            (20000, 15000)
        );

        assert_eq!(levels[2].info.downsample, 4.0);
        assert_eq!((levels[2].info.width, levels[2].info.height), (10000, 7500));
    }

    #[test]
    fn fix_level_dimensions_count_mismatch_errors() {
        let spacings = vec![(0.00025, 0.00025)];
        let mut levels: Vec<LevelData> = Vec::new();
        // empty levels vs one spacing → mismatch
        assert!(fix_level_dimensions(&mut levels, &spacings).is_err());
    }
}
