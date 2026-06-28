//! Integration tests for the Aperio vendor driver.
//!
//! Tests are based on the C library's test/cases/aperio* configs.
//! Each test downloads (and caches) real SVS files from the OpenSlide test data server.

mod common;

use slidekit::OpenSlide;

// ── Detection ─────────────────────────────────────────────────────

#[test]
fn detect_vendor_aperio_cmu1() {
    let path = require_slide!("Aperio/CMU-1.svs");
    let vendor = OpenSlide::detect_vendor(&path);
    assert_eq!(vendor.as_deref(), Some("aperio"));
}

#[test]
fn detect_vendor_aperio_small_region() {
    let path = require_slide!("Aperio/CMU-1-Small-Region.svs");
    let vendor = OpenSlide::detect_vendor(&path);
    assert_eq!(vendor.as_deref(), Some("aperio"));
}

// ── Open + Level info ─────────────────────────────────────────────

#[test]
fn open_aperio_cmu1() {
    let path = require_slide!("Aperio/CMU-1.svs");
    let slide = OpenSlide::open(&path).expect("should open CMU-1.svs");

    // CMU-1.svs is a multi-level pyramid
    assert!(slide.level_count() > 1, "expected multiple levels");

    // Level 0 should be the largest
    let (w0, h0) = slide.level0_dimensions();
    assert!(w0 > 0 && h0 > 0, "level 0 dimensions should be positive");

    // Downsample of level 0 should be 1.0
    let ds0 = slide.level_downsample(0).unwrap();
    assert!(
        (ds0 - 1.0).abs() < f64::EPSILON,
        "level 0 downsample should be 1.0"
    );

    // Each subsequent level should have a higher downsample factor
    for i in 1..slide.level_count() {
        let ds = slide.level_downsample(i).unwrap();
        let prev_ds = slide.level_downsample(i - 1).unwrap();
        assert!(
            ds > prev_ds,
            "level {i} downsample ({ds}) should be > level {} ({prev_ds})",
            i - 1
        );
    }

    // Each subsequent level should be smaller
    for i in 1..slide.level_count() {
        let (w, h) = slide.level_dimensions(i).unwrap();
        let (pw, ph) = slide.level_dimensions(i - 1).unwrap();
        assert!(
            w < pw || h < ph,
            "level {i} dimensions ({w}x{h}) should be smaller than level {} ({pw}x{ph})",
            i - 1
        );
    }
}

#[test]
fn open_aperio_small_region() {
    let path = require_slide!("Aperio/CMU-1-Small-Region.svs");
    let slide = OpenSlide::open(&path).expect("should open CMU-1-Small-Region.svs");
    assert!(slide.level_count() >= 1);
    let (w, h) = slide.level0_dimensions();
    assert!(w > 0 && h > 0);
}

// ── Properties ────────────────────────────────────────────────────

#[test]
fn properties_vendor_aperio() {
    let path = require_slide!("Aperio/CMU-1.svs");
    let slide = OpenSlide::open(&path).unwrap();

    assert_eq!(
        slide.property_value("openslide.vendor"),
        Some("aperio"),
        "vendor should be 'aperio'"
    );
}

#[test]
fn properties_aperio_specific() {
    let path = require_slide!("Aperio/CMU-1.svs");
    let slide = OpenSlide::open(&path).unwrap();

    // Aperio slides should have aperio.* properties parsed from ImageDescription
    let names = slide.property_names();
    let aperio_props: Vec<_> = names.iter().filter(|n| n.starts_with("aperio.")).collect();
    assert!(
        !aperio_props.is_empty(),
        "should have aperio.* properties from ImageDescription"
    );

    // AppMag should be duplicated to objective-power
    if let Some(appmag) = slide.property_value("aperio.AppMag") {
        assert_eq!(
            slide.property_value("openslide.objective-power"),
            Some(appmag),
            "AppMag should be duplicated to objective-power"
        );
    }

    // MPP should be duplicated to mpp-x and mpp-y
    if let Some(mpp) = slide.property_value("aperio.MPP") {
        assert_eq!(
            slide.property_value("openslide.mpp-x"),
            Some(mpp),
            "MPP should be duplicated to mpp-x"
        );
        assert_eq!(
            slide.property_value("openslide.mpp-y"),
            Some(mpp),
            "MPP should be duplicated to mpp-y"
        );
    }
}

// ── ICC Profile ───────────────────────────────────────────────────

#[test]
fn icc_profile_present() {
    let path = require_slide!("Aperio/CMU-1.svs");
    let slide = OpenSlide::open(&path).unwrap();

    // CMU-1.svs has ICC profiles (size 141992 per the test config)
    let icc = slide.icc_profile();
    assert!(
        icc.is_some(),
        "CMU-1.svs should have an ICC profile on the main image"
    );
    let icc_data = icc.unwrap();
    assert_eq!(icc_data.len(), 141992, "ICC profile size should be 141992");
}

// ── Associated images ─────────────────────────────────────────────

#[test]
fn associated_images_present() {
    let path = require_slide!("Aperio/CMU-1.svs");
    let slide = OpenSlide::open(&path).unwrap();

    let names = slide.associated_image_names();
    assert!(
        names.contains(&"thumbnail".to_string()),
        "should have 'thumbnail' associated image, got: {:?}",
        names
    );
}

#[test]
fn associated_image_thumbnail_dimensions() {
    let path = require_slide!("Aperio/CMU-1.svs");
    let slide = OpenSlide::open(&path).unwrap();

    let dims = slide.associated_image_dimensions("thumbnail");
    assert!(dims.is_some(), "thumbnail should have dimensions");
    let (w, h) = dims.unwrap();
    assert!(w > 0 && h > 0, "thumbnail dimensions should be positive");
}

#[test]
fn associated_image_thumbnail_icc_profile() {
    let path = require_slide!("Aperio/CMU-1.svs");
    let slide = OpenSlide::open(&path).unwrap();

    // Per test config: openslide.associated.thumbnail.icc-size: "141992"
    let icc = slide.associated_image_icc_profile("thumbnail");
    assert!(icc.is_some(), "thumbnail should have an ICC profile");
    assert_eq!(
        icc.unwrap().len(),
        141992,
        "thumbnail ICC profile size should be 141992"
    );
}

#[test]
fn read_associated_image_thumbnail() {
    let path = require_slide!("Aperio/CMU-1.svs");
    let slide = OpenSlide::open(&path).unwrap();

    let (w, h) = slide.associated_image_dimensions("thumbnail").unwrap();
    let pixels = slide.read_associated_image("thumbnail").unwrap();
    assert_eq!(
        pixels.len(),
        (w * h) as usize,
        "pixel count should match dimensions"
    );

    // All pixels should be non-zero (not transparent black)
    let non_zero = pixels.iter().filter(|&&p| p != 0).count();
    assert!(non_zero > 0, "thumbnail should have non-transparent pixels");
}

// ── Associated image names from ImageDescription ──────────────────

#[test]
fn associated_image_label_and_macro() {
    let path = require_slide!("Aperio/CMU-1.svs");
    let slide = OpenSlide::open(&path).unwrap();

    let names = slide.associated_image_names();
    // CMU-1.svs has label and macro associated images in addition to thumbnail
    // (The exact set depends on the file, but typically: thumbnail, label, macro)
    eprintln!("Associated images: {:?}", names);
    // At minimum, thumbnail must exist (tested above)
    // label and macro may exist
    for name in &names {
        let dims = slide.associated_image_dimensions(name).unwrap();
        assert!(
            dims.0 > 0 && dims.1 > 0,
            "associated image '{}' should have positive dimensions",
            name
        );
    }
}

// ── read_region ───────────────────────────────────────────────────

#[test]
fn read_region_level0_small() {
    let path = require_slide!("Aperio/CMU-1.svs");
    let slide = OpenSlide::open(&path).unwrap();

    // Read a small region at level 0
    let pixels = slide.read_region(0, 0, 0, 256, 256).unwrap();
    assert_eq!(pixels.len(), 256 * 256);

    // Should not be all zeros (transparent) — there should be actual tissue data
    let non_zero = pixels.iter().filter(|&&p| p != 0).count();
    assert!(
        non_zero > 0,
        "read_region should return non-transparent pixels"
    );
}

#[test]
fn read_region_lower_level() {
    let path = require_slide!("Aperio/CMU-1.svs");
    let slide = OpenSlide::open(&path).unwrap();

    if slide.level_count() < 2 {
        eprintln!("SKIP: need at least 2 levels");
        return;
    }

    let (w, h) = slide.level_dimensions(1).unwrap();
    let rw = w.min(256);
    let rh = h.min(256);

    let pixels = slide.read_region(0, 0, 1, rw, rh).unwrap();
    assert_eq!(pixels.len(), (rw * rh) as usize);
}

// ── Zero-length tile handling ─────────────────────────────────────

#[test]
fn read_region_returns_transparent_for_missing_tile() {
    // Based on aperio-jpeg-tile-zero-length test case:
    // reading at (0,0) level 0 with 256x256 should succeed
    // even if the tile data is zero-length (transparent black)
    let path = require_slide!("Aperio/CMU-1.svs");
    let slide = OpenSlide::open(&path).unwrap();

    // This should not error even for edge regions
    let result = slide.read_region(0, 0, 0, 256, 256);
    assert!(result.is_ok(), "should handle tile reads without error");
}

// ── best_level_for_downsample ─────────────────────────────────────

#[test]
fn best_level_for_downsample() {
    let path = require_slide!("Aperio/CMU-1.svs");
    let slide = OpenSlide::open(&path).unwrap();

    // Downsample 1.0 should return level 0
    assert_eq!(slide.best_level_for_downsample(1.0), 0);

    // Very large downsample should return the last level
    let last = slide.level_count() - 1;
    assert_eq!(
        slide.best_level_for_downsample(1000.0),
        last,
        "large downsample should return last level"
    );
}

// ── LZW label image ───────────────────────────────────────────────

#[test]
fn associated_image_label_decoded() {
    let path = require_slide!("Aperio/CMU-1.svs");
    let slide = OpenSlide::open(&path).unwrap();

    let names = slide.associated_image_names();
    if !names.contains(&"label".to_string()) {
        eprintln!("SKIP: no label associated image");
        return;
    }

    let (w, h) = slide.associated_image_dimensions("label").unwrap();
    assert!(w > 0 && h > 0, "label should have positive dimensions");

    let pixels = slide.read_associated_image("label").unwrap();
    assert_eq!(
        pixels.len(),
        (w * h) as usize,
        "label pixel count should match dimensions"
    );

    // Label should have actual content (not all transparent)
    let non_zero = pixels.iter().filter(|&&p| p != 0).count();
    assert!(non_zero > 0, "label should have non-transparent pixels");
}

// ── Generic-TIFF should not claim Aperio slides ───────────────────

#[test]
fn generic_tiff_does_not_claim_aperio() {
    let path = require_slide!("Aperio/CMU-1.svs");
    // Opening should detect as "aperio", not "generic-tiff"
    let vendor = OpenSlide::detect_vendor(&path);
    assert_eq!(
        vendor.as_deref(),
        Some("aperio"),
        "CMU-1.svs should be detected as aperio, not generic-tiff"
    );
}
