pub mod aperio;
pub mod generic_tiff;
pub mod philips;
pub mod trestle;

use crate::associated::AssociatedImage;
use crate::decode::TileDecoder;
use crate::decode::tiff::TiffLike;
use crate::error::Result;
use crate::grid::simple::SimpleGrid;
use crate::io::SlideReader;
use crate::level::LevelInfo;
use crate::properties::Properties;

/// Per-level rendering info needed to read tiles at runtime.
pub struct LevelData {
    pub info: LevelInfo,
    pub grid: SimpleGrid,
    /// TIFF directory index for this level.
    pub tiff_dir: i64,
    /// Tile decoder for this level's compression format.
    pub decoder: Box<dyn TileDecoder>,
    /// Actual TIFF tile width (may differ from `info.tile_width` when
    /// the reported tile size is zeroed due to overlaps).
    pub tiff_tile_width: i64,
    /// Actual TIFF tile height (may differ from `info.tile_height`).
    pub tiff_tile_height: i64,
}

/// Data produced by a vendor driver after opening a slide.
pub struct SlideData {
    pub levels: Vec<LevelData>,
    pub properties: Properties,
    pub associated_images: Vec<(String, AssociatedImage)>,
    pub icc_profile: Option<Vec<u8>>,
    /// The TIFF container, kept alive for tile reads.
    pub tiff: Option<TiffLike>,
}

/// Trait implemented by each vendor format driver.
pub trait VendorDriver: Send + Sync {
    /// Short name used in detection order (e.g. "aperio").
    fn name(&self) -> &str;

    /// Vendor string stored in `openslide.vendor` property.
    fn vendor(&self) -> &str;

    /// Detect whether this driver can handle the given data source.
    ///
    /// `tl` is `Some` if the file parsed as TIFF, `None` otherwise.
    fn detect(&self, reader: &dyn SlideReader, tl: Option<&mut TiffLike>) -> Result<bool>;

    /// Open and parse the slide, returning level/property/associated data.
    fn open(&self, reader: Box<dyn SlideReader>, tl: Option<TiffLike>) -> Result<SlideData>;
}

/// Format detection order (matches the C library).
pub const DETECTION_ORDER: &[&str] = &[
    "synthetic",
    "mirax",
    "zeiss",
    "dicom",
    "hamamatsu",
    "sakura",
    "trestle",
    "aperio",
    "leica",
    "philips",
    "ventana",
    "generic-tiff",
];

/// Get the list of all registered vendor drivers.
pub fn all_drivers() -> Vec<Box<dyn VendorDriver>> {
    vec![
        // Drivers are tried in DETECTION_ORDER. More specific vendors
        // must come before generic-tiff (the fallback).
        Box::new(trestle::TrestleDriver),
        Box::new(aperio::AperioDriver),
        Box::new(philips::PhilipsDriver),
        Box::new(generic_tiff::GenericTiffDriver),
    ]
}

/// Run the vendor detection pipeline.
///
/// Takes a reader and an optional pre-parsed TiffLike.
/// Returns the index into `drivers` and the vendor name on match.
pub fn detect_format(
    drivers: &[Box<dyn VendorDriver>],
    reader: &dyn SlideReader,
    tl: &mut Option<TiffLike>,
) -> Option<usize> {
    for (i, driver) in drivers.iter().enumerate() {
        match driver.detect(reader, tl.as_mut()) {
            Ok(true) => return Some(i),
            _ => continue,
        }
    }
    None
}
