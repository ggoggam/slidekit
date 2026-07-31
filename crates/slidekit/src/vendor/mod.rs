pub mod aperio;
pub mod dicom;
pub mod generic_tiff;
pub mod hamamatsu;
pub mod leica;
pub mod mirax;
pub mod philips;
pub mod synthetic;
pub mod trestle;
pub mod ventana;
pub mod zeiss;

use crate::associated::AssociatedImage;
use crate::decode::TileDecoder;
use crate::decode::tiff::TiffLike;
use crate::error::Result;
use crate::grid::simple::SimpleGrid;
use crate::grid::tilemap::TileMap;
use crate::io::SlideReader;
use crate::level::LevelInfo;
use crate::properties::Properties;
use crate::vendor::hamamatsu::NdpiLevel;

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
    /// Explicit tile placement for formats whose tiles don't lie on a regular
    /// row-major grid (Ventana BIF AOIs). When `Some`, the read path uses it
    /// instead of `grid`/the row-major index formula; `grid` is then unused.
    pub tilemap: Option<TileMap>,
    /// Restart-marker tiling state for Hamamatsu NDPI levels, whose whole level
    /// is one giant JPEG in a single strip (no TIFF tiles). When `Some`, the
    /// read path reconstructs a per-tile JPEG from this instead of reading
    /// `tiff_dir`'s tile table; `grid` still maps regions → (col,row).
    pub ndpi: Option<NdpiLevel>,
    /// MIRAX per-level tile index. MIRAX is not TIFF-based: each tile is a
    /// standalone image blob in one of several `Data####.dat` files, located by
    /// (file, offset, length). When `Some`, the read path reads tiles through
    /// this (which owns the data-file readers) instead of a TIFF container;
    /// `grid` maps regions → (col,row).
    pub mirax: Option<crate::vendor::mirax::MiraxLevel>,
    /// Zeiss CZI per-level tile index. CZI is a single-file segmented container,
    /// not TIFF: each tile is a subblock located by absolute file offset and
    /// carries its own compression/pixel type. When `Some`, the read path reads
    /// tiles through this (which owns the reader) and decodes them internally.
    pub zeiss: Option<crate::vendor::zeiss::ZeissLevel>,
    /// Synthetic self-test level: the whole level pre-rendered as one row-major
    /// `width × height` ARGB image. When `Some`, `read_region` crops it directly
    /// (no file/decoder at read time — everything was decoded at `open`).
    pub synthetic: Option<Vec<u32>>,
    /// DICOM per-level frame index. A DICOM slide is a directory of instances,
    /// one (or a concatenation) per level, whose tiles are the instance's
    /// frames. When `Some`, the read path reads tiles through this (which owns
    /// the per-instance readers and decodes them); `grid` maps regions →
    /// (col,row).
    pub dicom: Option<crate::vendor::dicom::DicomLevel>,
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
        // must come before generic-tiff (the fallback). MIRAX matches only a
        // `.mrxs` path, so it is safe to try first.
        Box::new(mirax::MiraxDriver),
        Box::new(zeiss::ZeissDriver),
        Box::new(dicom::DicomDriver),
        Box::new(hamamatsu::HamamatsuDriver),
        Box::new(trestle::TrestleDriver),
        Box::new(aperio::AperioDriver),
        Box::new(leica::LeicaDriver),
        Box::new(philips::PhilipsDriver),
        Box::new(ventana::VentanaDriver),
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
