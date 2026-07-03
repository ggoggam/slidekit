//! Integration tests for the Philips TIFF vendor driver.
//!
//! Uses Philips-4.tiff from the OpenSlide test data server: a sparse slide
//! whose macro image is stored as a TIFF directory (associated images edited
//! out of the XML), which exercises both the missing-tile fill and the
//! TIFF-directory associated-image path.
//!
//! The slide is opened a single time per test — assertions about the same slide
//! are grouped so we don't re-parse it once per assertion.

mod common;

use slidekit::OpenSlide;

const SLIDE: &str = "Philips-TIFF/Philips-4.tiff";

/// Everything we can assert about `Philips-4.tiff` from a single open.
#[test]
fn philips_4() {
    let path = require_slide!(SLIDE);

    // ── Detection ──
    // A tiled TIFF would otherwise fall through to generic-tiff; the Philips
    // driver must claim it first.
    assert_eq!(OpenSlide::detect_vendor(&path).as_deref(), Some("philips"));

    let slide = OpenSlide::open(&path).expect("should open Philips-4.tiff");

    // ── Open + Level info ──
    assert!(slide.level_count() > 1, "expected a multi-level pyramid");

    let (w0, h0) = slide.level0_dimensions();
    assert!(w0 > 0 && h0 > 0, "level 0 dimensions should be positive");

    // Level 0 downsample is exactly 1.0.
    assert_eq!(slide.level_downsample(0).unwrap(), 1.0);

    // Sub-levels: strictly increasing integer downsamples, strictly smaller dims.
    for i in 1..slide.level_count() {
        let ds = slide.level_downsample(i).unwrap();
        let prev_ds = slide.level_downsample(i - 1).unwrap();
        assert!(
            ds > prev_ds,
            "level {i} downsample {ds} should exceed {prev_ds}"
        );
        assert_eq!(ds, ds.round(), "Philips downsamples should be integers");

        let (w, h) = slide.level_dimensions(i).unwrap();
        let (pw, ph) = slide.level_dimensions(i - 1).unwrap();
        assert!(
            w < pw || h < ph,
            "level {i} ({w}x{h}) should be smaller than {pw}x{ph}"
        );
    }

    // ── Properties ──
    assert_eq!(slide.property_value("openslide.vendor"), Some("philips"));

    // The huge XML must not be dumped into tiff.ImageDescription.
    assert_eq!(slide.property_value("tiff.ImageDescription"), None);

    // mpp comes from the XML DICOM_PIXEL_SPACING (mm → microns).
    let mpp_x: f64 = slide
        .property_value("openslide.mpp-x")
        .expect("should have mpp-x")
        .parse()
        .unwrap();
    let mpp_y: f64 = slide
        .property_value("openslide.mpp-y")
        .expect("should have mpp-y")
        .parse()
        .unwrap();
    // Brightfield WSI: sub-micron pixels, well under 10 µm.
    assert!(mpp_x > 0.0 && mpp_x < 10.0, "mpp-x out of range: {mpp_x}");
    assert!(mpp_y > 0.0 && mpp_y < 10.0, "mpp-y out of range: {mpp_y}");

    // ── Associated images ──
    let names = slide.associated_image_names();
    eprintln!("Associated images: {names:?}");
    assert!(
        names.contains(&"macro".to_string()),
        "Philips-4 stores a macro image as a TIFF directory, got: {names:?}"
    );

    let (w, h) = slide.associated_image_dimensions("macro").unwrap();
    assert!(w > 0 && h > 0, "macro dimensions should be positive");

    let pixels = slide.read_associated_image("macro").unwrap();
    assert_eq!(pixels.len(), (w * h) as usize);
    assert!(
        pixels.iter().any(|&p| p != 0),
        "macro image should have non-transparent pixels"
    );

    // ── read_region ──
    // Sparse slide: (0,0) may land on a missing tile, so only assert the read
    // succeeds and has the right size — not that it is non-transparent.
    let pixels = slide.read_region(0, 0, 0, 256, 256).unwrap();
    assert_eq!(pixels.len(), 256 * 256);

    // The smallest level is small enough to read whole; tissue must appear
    // somewhere in it, proving JPEG tile decode + compositing work.
    let last = slide.level_count() - 1;
    let (w, h) = slide.level_dimensions(last).unwrap();
    let ds = slide.level_downsample(last).unwrap() as i64;
    let rw = w.min(2048);
    let rh = h.min(2048);

    // x,y are in level-0 space.
    let pixels = slide
        .read_region(0, 0, last, rw, rh)
        .expect("should read smallest level");
    assert_eq!(pixels.len(), (rw * rh) as usize);
    assert!(
        pixels.iter().any(|&p| p != 0),
        "smallest level (downsample {ds}) should contain visible tissue"
    );

    // ── best_level_for_downsample ──
    assert_eq!(slide.best_level_for_downsample(1.0), 0);
    let last = slide.level_count() - 1;
    assert_eq!(slide.best_level_for_downsample(1e6), last);
}
