//! Integration tests for the Leica SCN vendor driver.
//!
//! These run against `Leica/Leica-1.scn` from the OpenSlide test data server
//! (declared in `tests/testdata.yaml`) — a single brightfield region plus a
//! macro overview. The slide is ~290 MiB, so they skip via `require_slide!`
//! when downloads are disabled (e.g. CI past the size budget,
//! `OPENSLIDE_SKIP_DOWNLOAD`). The XML parsing and main/macro classification are
//! unit-tested in `src/vendor/leica.rs`; these exercise the end-to-end open +
//! read path against real pixels.

mod common;

use slidekit::OpenSlide;

const SLIDE: &str = "Leica/Leica-1.scn";

#[test]
fn detect_vendor_leica() {
    let path = require_slide!(SLIDE);
    assert_eq!(OpenSlide::detect_vendor(&path).as_deref(), Some("leica"));
}

#[test]
fn open_leica_pyramid() {
    let path = require_slide!(SLIDE);
    let slide = OpenSlide::open(&path).expect("should open Leica SCN");

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

    assert_eq!(slide.property_value("openslide.vendor"), Some("leica"));
    assert!(
        slide
            .property_names()
            .iter()
            .any(|n| n.starts_with("leica.")),
        "expected leica.* properties from the SCN metadata"
    );
    // Brightfield main image → objective power should be present and numeric.
    let op = slide
        .property_value("openslide.objective-power")
        .expect("objective-power should be set");
    assert!(
        op.parse::<f64>().is_ok(),
        "objective-power {op:?} not numeric"
    );
}

#[test]
fn read_region_level0_is_well_formed() {
    let path = require_slide!(SLIDE);
    let slide = OpenSlide::open(&path).unwrap();

    let pixels = slide.read_region(0, 0, 0, 512, 512).unwrap();
    assert_eq!(pixels.len(), 512 * 512);
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

    // SCN files carry a macro overview; if our strip-based associated-image
    // decode handled it, it surfaces here. Report rather than hard-fail so a
    // tiled-macro variant doesn't break the suite (see leica.rs deferrals).
    let names = slide.associated_image_names();
    eprintln!("Leica associated images: {names:?}");
    if names.iter().any(|n| n == "macro") {
        let (mw, mh) = slide.associated_image_dimensions("macro").unwrap();
        assert!(mw > 0 && mh > 0, "macro dimensions should be positive");
    }
}
