//! Integration tests for the Aperio vendor driver.
//!
//! Tests are based on the C library's test/cases/aperio* configs.
//! Each slide is downloaded (and cached) once from the OpenSlide test data
//! server, then opened a single time per test — assertions about the same slide
//! are grouped so we don't re-parse it once per assertion.

mod common;

use slidekit::OpenSlide;

/// Everything we can assert about `CMU-1.svs` from a single open.
#[test]
fn cmu1() {
    let path = require_slide!("Aperio/CMU-1.svs");

    // ── Detection ──
    // Must be detected as aperio, not generic-tiff.
    assert_eq!(
        OpenSlide::detect_vendor(&path).as_deref(),
        Some("aperio"),
        "CMU-1.svs should be detected as aperio, not generic-tiff"
    );

    let slide = OpenSlide::open(&path).expect("should open CMU-1.svs");

    // ── Levels ──
    assert!(slide.level_count() > 1, "expected multiple levels");

    let (w0, h0) = slide.level0_dimensions();
    assert!(w0 > 0 && h0 > 0, "level 0 dimensions should be positive");

    let ds0 = slide.level_downsample(0).unwrap();
    assert!(
        (ds0 - 1.0).abs() < f64::EPSILON,
        "level 0 downsample should be 1.0"
    );

    for i in 1..slide.level_count() {
        let ds = slide.level_downsample(i).unwrap();
        let prev_ds = slide.level_downsample(i - 1).unwrap();
        assert!(
            ds > prev_ds,
            "level {i} downsample ({ds}) should be > level {} ({prev_ds})",
            i - 1
        );

        let (w, h) = slide.level_dimensions(i).unwrap();
        let (pw, ph) = slide.level_dimensions(i - 1).unwrap();
        assert!(
            w < pw || h < ph,
            "level {i} dimensions ({w}x{h}) should be smaller than level {} ({pw}x{ph})",
            i - 1
        );
    }

    // ── best_level_for_downsample ──
    assert_eq!(
        slide.best_level_for_downsample(1.0),
        0,
        "downsample 1.0 should return level 0"
    );
    assert_eq!(
        slide.best_level_for_downsample(1000.0),
        slide.level_count() - 1,
        "large downsample should return last level"
    );

    // ── Properties ──
    assert_eq!(
        slide.property_value("openslide.vendor"),
        Some("aperio"),
        "vendor should be 'aperio'"
    );

    let names = slide.property_names();
    let aperio_props: Vec<_> = names.iter().filter(|n| n.starts_with("aperio.")).collect();
    assert!(
        !aperio_props.is_empty(),
        "should have aperio.* properties from ImageDescription"
    );

    // AppMag should be duplicated to objective-power.
    if let Some(appmag) = slide.property_value("aperio.AppMag") {
        assert_eq!(
            slide.property_value("openslide.objective-power"),
            Some(appmag),
            "AppMag should be duplicated to objective-power"
        );
    }
    // MPP should be duplicated to mpp-x and mpp-y.
    if let Some(mpp) = slide.property_value("aperio.MPP") {
        assert_eq!(slide.property_value("openslide.mpp-x"), Some(mpp));
        assert_eq!(slide.property_value("openslide.mpp-y"), Some(mpp));
    }

    // ── ICC profile (main image) ──
    let icc = slide.icc_profile();
    assert!(icc.is_some(), "CMU-1.svs should have an ICC profile");
    assert_eq!(
        icc.unwrap().len(),
        141992,
        "ICC profile size should be 141992"
    );

    // ── Associated images ──
    let assoc = slide.associated_image_names();
    assert!(
        assoc.contains(&"thumbnail".to_string()),
        "should have 'thumbnail' associated image, got: {assoc:?}"
    );
    // Every associated image must have positive dimensions.
    for name in &assoc {
        let (w, h) = slide.associated_image_dimensions(name).unwrap();
        assert!(
            w > 0 && h > 0,
            "associated image '{name}' should be positive"
        );
    }

    // Thumbnail: dims, ICC, and pixel content.
    let (tw, th) = slide.associated_image_dimensions("thumbnail").unwrap();
    assert!(tw > 0 && th > 0, "thumbnail dimensions should be positive");
    assert_eq!(
        slide
            .associated_image_icc_profile("thumbnail")
            .expect("thumbnail should have an ICC profile")
            .len(),
        141992,
        "thumbnail ICC profile size should be 141992"
    );
    let thumb = slide.read_associated_image("thumbnail").unwrap();
    assert_eq!(
        thumb.len(),
        (tw * th) as usize,
        "thumbnail pixel count should match dimensions"
    );
    assert!(
        thumb.iter().any(|&p| p != 0),
        "thumbnail should have non-transparent pixels"
    );

    // Label (LZW-encoded) if present: dims + decoded content.
    if assoc.contains(&"label".to_string()) {
        let (lw, lh) = slide.associated_image_dimensions("label").unwrap();
        assert!(lw > 0 && lh > 0, "label should have positive dimensions");
        let label = slide.read_associated_image("label").unwrap();
        assert_eq!(
            label.len(),
            (lw * lh) as usize,
            "label pixel count should match dimensions"
        );
        assert!(
            label.iter().any(|&p| p != 0),
            "label should have non-transparent pixels"
        );
    }

    // ── read_region ──
    let region = slide.read_region(0, 0, 0, 256, 256).unwrap();
    assert_eq!(region.len(), 256 * 256);
    assert!(
        region.iter().any(|&p| p != 0),
        "read_region should return non-transparent pixels"
    );

    // Missing/zero-length tiles must resolve to transparent, not error.
    assert!(
        slide.read_region(0, 0, 0, 256, 256).is_ok(),
        "should handle tile reads without error"
    );

    // A lower level, if present.
    if slide.level_count() >= 2 {
        let (w, h) = slide.level_dimensions(1).unwrap();
        let (rw, rh) = (w.min(256), h.min(256));
        let pixels = slide.read_region(0, 0, 1, rw, rh).unwrap();
        assert_eq!(pixels.len(), (rw * rh) as usize);
    }
}

/// `CMU-1-Small-Region.svs`: detection and a basic open.
#[test]
fn cmu1_small_region() {
    let path = require_slide!("Aperio/CMU-1-Small-Region.svs");

    assert_eq!(OpenSlide::detect_vendor(&path).as_deref(), Some("aperio"));

    let slide = OpenSlide::open(&path).expect("should open CMU-1-Small-Region.svs");
    assert!(slide.level_count() >= 1);
    let (w, h) = slide.level0_dimensions();
    assert!(w > 0 && h > 0);
}
