//! Integration tests for the Roche Ventana BIF vendor driver.
//!
//! These run against `Ventana/OS-2.bif` from the OpenSlide test data server
//! (declared in `tests/testdata.yaml`). The slide is ~2.5 GiB, so they skip via
//! `require_slide!` when downloads are disabled — e.g. CI past the size budget
//! (`OPENSLIDE_SKIP_DOWNLOAD`). The driver's detection, metadata, level
//! enumeration, and AOI stitch geometry are unit-tested in
//! `src/vendor/ventana.rs`; these exercise the end-to-end open + tilemap read
//! path against real pixels.
//!
//! The slide is opened a single time per test — assertions about the same slide
//! are grouped so we don't re-parse it once per assertion.

mod common;

use slidekit::OpenSlide;

const SLIDE: &str = "Ventana/OS-2.bif";

/// Everything we can assert about `Ventana/OS-2.bif` from a single open.
#[test]
fn os_2() {
    let path = require_slide!(SLIDE);

    // ── Detection ──
    assert_eq!(OpenSlide::detect_vendor(&path).as_deref(), Some("ventana"));

    let slide = OpenSlide::open(&path).expect("should open Ventana BIF");

    // ── Levels ──
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

    // ── Properties ──
    assert_eq!(slide.property_value("openslide.vendor"), Some("ventana"));
    // iScan attributes are surfaced under the ventana.* namespace.
    assert!(
        slide
            .property_names()
            .iter()
            .any(|n| n.starts_with("ventana.")),
        "expected ventana.* properties from the iScan metadata"
    );

    // ── read_region (level 0) ──
    // The AOI tilemap should composite real tissue somewhere near the origin.
    let pixels = slide.read_region(0, 0, 0, 512, 512).unwrap();
    assert_eq!(pixels.len(), 512 * 512);

    // ── read_region (smallest level) ──
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
        "smallest level should contain visible tissue"
    );
}
