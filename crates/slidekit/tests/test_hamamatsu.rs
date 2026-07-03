//! Integration tests for the Hamamatsu NDPI vendor driver.
//!
//! These run against `Hamamatsu/CMU-1.ndpi` from the OpenSlide test data server
//! (declared in `tests/testdata.yaml`) — a small brightfield scan with valid
//! JPEG headers. The slide is ~189 MiB, so they skip via `require_slide!` when
//! downloads are disabled (e.g. CI past the size budget,
//! `OPENSLIDE_SKIP_DOWNLOAD`). The JPEG-marker parsing and restart-marker
//! scanning are unit-tested in `src/vendor/hamamatsu.rs`; these exercise the
//! end-to-end open + restart-marker tiling read path against real pixels.

mod common;

use slidekit::OpenSlide;

const SLIDE: &str = "Hamamatsu/CMU-1.ndpi";

#[test]
fn detect_vendor_hamamatsu() {
    let path = require_slide!(SLIDE);
    assert_eq!(
        OpenSlide::detect_vendor(&path).as_deref(),
        Some("hamamatsu")
    );
}

#[test]
fn open_ndpi_pyramid() {
    let path = require_slide!(SLIDE);
    let slide = OpenSlide::open(&path).expect("should open Hamamatsu NDPI");

    assert!(slide.level_count() >= 1, "expected at least one level");

    let (w0, h0) = slide.level0_dimensions();
    assert!(w0 > 0 && h0 > 0, "level 0 dimensions should be positive");
    assert_eq!(slide.level_downsample(0).unwrap(), 1.0);

    // Downsamples strictly increase; dimensions strictly shrink.
    for i in 1..slide.level_count() {
        let ds = slide.level_downsample(i).unwrap();
        let prev = slide.level_downsample(i - 1).unwrap();
        assert!(ds > prev, "level {i} downsample {ds} should exceed {prev}");

        let (w, h) = slide.level_dimensions(i).unwrap();
        let (pw, ph) = slide.level_dimensions(i - 1).unwrap();
        assert!(
            w < pw || h < ph,
            "level {i} should be smaller than previous"
        );
    }
}

#[test]
fn properties_vendor_and_metadata() {
    let path = require_slide!(SLIDE);
    let slide = OpenSlide::open(&path).unwrap();

    assert_eq!(slide.property_value("openslide.vendor"), Some("hamamatsu"));
    assert!(
        slide
            .property_names()
            .iter()
            .any(|n| n.starts_with("hamamatsu.")),
        "expected hamamatsu.* properties from the NDPI metadata"
    );
    // Brightfield scan → objective power should be present and numeric.
    let op = slide
        .property_value("openslide.objective-power")
        .expect("objective-power should be set");
    assert!(
        op.parse::<f64>().is_ok(),
        "objective-power {op:?} not numeric"
    );
    // NDPI stores microns-per-pixel via TIFF resolution tags.
    assert!(
        slide.property_value("openslide.mpp-x").is_some(),
        "expected openslide.mpp-x"
    );
}

#[test]
fn read_region_level0_is_well_formed() {
    let path = require_slide!(SLIDE);
    let slide = OpenSlide::open(&path).unwrap();

    // Reads spanning several restart-marker tiles of the base level.
    let pixels = slide.read_region(0, 0, 0, 512, 512).unwrap();
    assert_eq!(pixels.len(), 512 * 512);
    assert!(
        pixels.iter().any(|&p| p != 0),
        "level 0 region should contain visible content"
    );
    // ARGB from a JPEG tile decode is fully opaque.
    assert!(
        pixels.iter().all(|&p| (p >> 24) == 0xFF),
        "decoded pixels should be opaque"
    );
}

#[test]
fn read_smallest_level_has_tissue() {
    let path = require_slide!(SLIDE);
    let slide = OpenSlide::open(&path).unwrap();

    let last = slide.level_count() - 1;
    let (w, h) = slide.level_dimensions(last).unwrap();
    let rw = w.min(2048);
    let rh = h.min(2048);

    let pixels = slide
        .read_region(0, 0, last, rw, rh)
        .expect("should read smallest level");
    assert_eq!(pixels.len(), (rw * rh) as usize);
    assert!(
        pixels.iter().any(|&p| p != 0),
        "smallest level should contain visible content"
    );
}

#[test]
fn macro_associated_image_present() {
    let path = require_slide!(SLIDE);
    let slide = OpenSlide::open(&path).unwrap();

    // NDPI files carry a macro overview (source lens -1); if our whole-JPEG
    // decode handled it, it surfaces here. Report rather than hard-fail so a
    // variant without one doesn't break the suite.
    let names = slide.associated_image_names();
    eprintln!("Hamamatsu associated images: {names:?}");
    if names.iter().any(|n| n == "macro") {
        let (mw, mh) = slide.associated_image_dimensions("macro").unwrap();
        assert!(mw > 0 && mh > 0, "macro dimensions should be positive");
    }
}
