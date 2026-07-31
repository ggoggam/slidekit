//! Integration tests for the DICOM WSI vendor driver.
//!
//! These run against two slides from the OpenSlide test data server (declared
//! in `tests/testdata.yaml`), chosen to cover both frame layouts:
//!
//! * `DICOM/Leica-4.zip` — `TILED_SPARSE`, so every frame's tile position comes
//!   from its per-frame functional group. It also carries all three associated
//!   images and stores its label *uncompressed*, so one slide exercises the
//!   JPEG YCbCr, JPEG RGB, and native-RGB decode paths.
//! * `DICOM/Hamamatsu-2.zip` — `TILED_FULL`, where frames are implicitly
//!   row-major and there are no per-frame groups at all.
//!
//! A DICOM slide is a directory of `.dcm` instances rather than one file, so
//! (like Trestle and MIRAX) the ZIP is extracted and one instance inside it is
//! opened; the driver finds the rest. The parser itself is unit-tested in
//! `src/decode/dicom.rs` and the frame/tile indexing in `src/vendor/dicom.rs`;
//! these exercise the whole open + read path against real pixels. The slides
//! are ~85 MiB and ~190 MiB, so they skip via `require_dicom_slide!` when
//! downloads are disabled.
//!
//! Assertions about the same slide are grouped so we open it a single time per
//! test rather than re-parsing it once per assertion.

mod common;

use slidekit::OpenSlide;
use std::path::PathBuf;

/// Extract a DICOM slide ZIP into its own directory under the test cache and
/// return the path of one `.dcm` instance inside it.
fn dicom_slide_path(manifest_path: &str) -> Option<PathBuf> {
    let zip_path = common::test_slide_path(manifest_path)?;
    let stem = zip_path.file_stem()?.to_str()?.to_string();
    let extract_dir = zip_path.parent()?.join("extracted").join(&stem);

    if !extract_dir.exists() {
        let tmp_dir = extract_dir.with_extension("partial");
        std::fs::remove_dir_all(&tmp_dir).ok();
        std::fs::create_dir_all(&tmp_dir).ok()?;
        let file = std::fs::File::open(&zip_path).ok()?;
        let mut archive = zip::ZipArchive::new(file).ok()?;
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).ok()?;
            let name = entry.name().to_string();
            if name.ends_with('/') {
                continue;
            }
            let out_path = tmp_dir.join(&name);
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).ok()?;
            }
            let mut out_file = std::fs::File::create(&out_path).ok()?;
            std::io::copy(&mut entry, &mut out_file).ok()?;
        }
        // Rename last, so a killed run can't leave a half-extracted directory
        // that later runs would treat as complete.
        std::fs::rename(&tmp_dir, &extract_dir).ok()?;
    }

    first_instance(&extract_dir)
}

/// The first `.dcm` file in `dir`, searched depth-first in sorted order.
fn first_instance(dir: &std::path::Path) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in &entries {
        if path.is_file()
            && path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("dcm"))
        {
            return Some(path.clone());
        }
    }
    entries
        .iter()
        .filter(|p| p.is_dir())
        .find_map(|p| first_instance(p))
}

macro_rules! require_dicom_slide {
    ($path:expr) => {
        match dicom_slide_path($path) {
            Some(p) => p,
            None => {
                eprintln!("SKIP: DICOM test slide {} not available", $path);
                return;
            }
        }
    };
}

/// Everything we can assert about the sparse-tiled Leica GT450 slide from a
/// single open.
#[test]
fn leica_4_sparse() {
    let path = require_dicom_slide!("DICOM/Leica-4.zip");

    // ── Detection ──
    assert_eq!(OpenSlide::detect_vendor(&path).as_deref(), Some("dicom"));

    let slide = OpenSlide::open(&path).expect("should open DICOM Leica-4");

    // ── Pyramid levels ──
    // Three VOLUME instances at 1x, 4x, and 16x.
    assert_eq!(slide.level_count(), 3);
    assert_eq!(slide.level0_dimensions(), (23374, 22079));
    assert_eq!(slide.level_dimensions(1).unwrap(), (5843, 5519));
    assert_eq!(slide.level_dimensions(2).unwrap(), (1460, 1379));
    assert_eq!(slide.level_downsample(0).unwrap(), 1.0);
    // Downsamples are the average of the width and height ratios (what the C
    // library reports), so they land near but not exactly on a power of two.
    for (level, expected) in [(1, 4.0), (2, 16.0)] {
        let ds = slide.level_downsample(level).unwrap();
        assert!(
            (ds - expected).abs() < 0.05,
            "level {level} downsample {ds} should be about {expected}"
        );
    }

    // ── Properties ──
    assert_eq!(slide.property_value("openslide.vendor"), Some("dicom"));
    assert_eq!(
        slide.property_value("openslide.objective-power"),
        Some("40")
    );
    // PixelSpacing is in millimetres; OpenSlide reports microns.
    assert_eq!(
        slide.property_value("openslide.mpp-x"),
        Some("1.05105387047")
    );
    assert_eq!(
        slide.property_value("openslide.mpp-y"),
        Some("1.05105387047")
    );
    // Every named DICOM attribute is surfaced as dicom.*, including nested
    // sequence items, indexed the way the C driver indexes them.
    assert_eq!(
        slide.property_value("dicom.Manufacturer"),
        Some("Leica Biosystems")
    );
    assert_eq!(slide.property_value("dicom.ImageType[2]"), Some("VOLUME"));
    assert_eq!(
        slide.property_value("dicom.DimensionOrganizationType"),
        Some("TILED_SPARSE")
    );
    assert_eq!(
        slide.property_value(
            "dicom.SharedFunctionalGroupsSequence[0].PixelMeasuresSequence[0].PixelSpacing[0]"
        ),
        Some("0.00105105387047")
    );
    assert_eq!(
        slide.property_value("dicom.OpticalPathSequence[0].ObjectiveLensPower"),
        Some("40")
    );
    // BarcodeValue is an LT, where a backslash would be content rather than a
    // value separator; this one is a single `;`-separated string.
    assert_eq!(
        slide.property_value("openslide.barcode"),
        Some("PV22;LEI;Case-D;Slide-D-5")
    );
    // The 8004 per-frame functional groups are not dumped as properties.
    assert!(
        !slide
            .property_names()
            .iter()
            .any(|n| n.starts_with("dicom.PerFrameFunctionalGroupsSequence")),
        "per-frame groups should not be surfaced as properties"
    );

    // ── ICC profile ──
    // Level 0's optical path carries one; it is the slide's profile.
    let icc = slide.icc_profile().expect("slide ICC profile");
    assert!(icc.len() > 128, "ICC profile looks truncated");
    assert_eq!(&icc[36..40], b"acsp", "not an ICC profile header");

    // ── Associated images ──
    // LABEL → label (uncompressed RGB), OVERVIEW → macro, THUMBNAIL → thumbnail.
    for (name, dimensions) in [
        ("label", (608, 547)),
        ("macro", (1491, 605)),
        ("thumbnail", (1920, 1813)),
    ] {
        assert_eq!(
            slide.associated_image_dimensions(name),
            Some(dimensions),
            "{name} dimensions"
        );
        let (w, h) = dimensions;
        let px = slide.read_associated_image(name).unwrap();
        assert_eq!(px.len(), (w * h) as usize);
        assert!(px.iter().all(|&p| (p >> 24) == 0xFF), "{name} not opaque");
        assert!(
            px.iter().any(|&p| p & 0x00FF_FFFF != 0),
            "{name} decoded to pure black"
        );
    }

    // ── read_region over tissue ──
    // The frame count exactly covers the 92x87 tile grid, so any in-bounds
    // region has pixels.
    let px = slide.read_region(8000, 8000, 0, 512, 512).unwrap();
    assert_eq!(px.len(), 512 * 512);
    assert!(
        px.iter().all(|&p| (p >> 24) == 0xFF),
        "a fully covered region should decode opaque everywhere"
    );

    // A region straddling the right edge is half filled and half transparent —
    // the edge tiles are clipped to the level, not painted with their padding.
    let (w0, _) = slide.level0_dimensions();
    let px = slide.read_region(w0 - 100, 8000, 0, 200, 16).unwrap();
    assert!(px[..100].iter().all(|&p| (p >> 24) == 0xFF), "left of edge");
    assert!(px[100..200].iter().all(|&p| p == 0), "right of edge");

    // ── Smallest level ──
    let (w, h) = slide.level_dimensions(2).unwrap();
    let px = slide.read_region(0, 0, 2, w, h).expect("read smallest");
    assert_eq!(px.len(), (w * h) as usize);
    assert!(
        px.iter().filter(|&&p| p != 0).count() > (w * h / 2) as usize,
        "smallest level should be mostly covered"
    );
}

/// The TILED_FULL half of the matrix: frames indexed row-major, no per-frame
/// positions.
#[test]
fn hamamatsu_2_tiled_full() {
    let path = require_dicom_slide!("DICOM/Hamamatsu-2.zip");

    assert_eq!(OpenSlide::detect_vendor(&path).as_deref(), Some("dicom"));
    let slide = OpenSlide::open(&path).expect("should open DICOM Hamamatsu-2");

    assert_eq!(
        slide.property_value("dicom.DimensionOrganizationType"),
        Some("TILED_FULL")
    );
    assert_eq!(slide.property_value("openslide.vendor"), Some("dicom"));

    // Levels shrink monotonically and downsamples grow.
    assert!(slide.level_count() >= 2);
    let (w0, h0) = slide.level0_dimensions();
    assert!(w0 > 0 && h0 > 0);
    for level in 1..slide.level_count() {
        let (w, h) = slide.level_dimensions(level).unwrap();
        let (pw, ph) = slide.level_dimensions(level - 1).unwrap();
        assert!(w < pw && h < ph, "level {level} should be smaller");
        assert!(
            slide.level_downsample(level).unwrap() > slide.level_downsample(level - 1).unwrap()
        );
    }

    // The whole smallest level decodes, and it is mostly real pixels.
    let last = slide.level_count() - 1;
    let (w, h) = slide.level_dimensions(last).unwrap();
    let px = slide.read_region(0, 0, last, w, h).expect("read smallest");
    assert_eq!(px.len(), (w * h) as usize);
    assert!(
        px.iter().filter(|&&p| (p >> 24) == 0xFF).count() > (w * h / 2) as usize,
        "smallest level should be mostly covered"
    );

    // A region in the middle of level 0 decodes opaque.
    let px = slide
        .read_region(w0 / 2, h0 / 2, 0, 256, 256)
        .expect("read level 0");
    assert_eq!(px.len(), 256 * 256);
    assert!(px.iter().any(|&p| (p >> 24) == 0xFF));
}
