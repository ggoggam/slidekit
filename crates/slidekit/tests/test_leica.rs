//! Integration tests for the Leica SCN vendor driver.
//!
//! These run against `Leica/Leica-1.scn` from the OpenSlide test data server
//! (declared in `tests/testdata.yaml`) — a single brightfield region plus a
//! macro overview. The slide is ~290 MiB, so they skip via `require_slide!`
//! when downloads are disabled (e.g. CI past the size budget,
//! `OPENSLIDE_SKIP_DOWNLOAD`). The XML parsing and main/macro classification are
//! unit-tested in `src/vendor/leica.rs`; these exercise the end-to-end open +
//! read path against real pixels.
//!
//! The slide is opened a single time per test — assertions about the same slide
//! are grouped so we don't re-parse it once per assertion.

mod common;

use slidekit::OpenSlide;

const SLIDE: &str = "Leica/Leica-1.scn";

/// Everything we can assert about `Leica-1.scn` from a single open.
#[test]
fn leica_1() {
    let path = require_slide!(SLIDE);

    // ── Detection ──
    assert_eq!(OpenSlide::detect_vendor(&path).as_deref(), Some("leica"));

    let slide = OpenSlide::open(&path).expect("should open Leica SCN");

    // ── Levels (pyramid) ──
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

    // ── Properties (vendor + metadata) ──
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

    // ── Geometry matches OpenSlide exactly ──
    // A level is the whole collection canvas (the glass slide), with the scanned
    // region placed at `bounds-*`. These are `slidetool prop list` values from
    // OpenSlide 4.0.1 on this fixture: if slidekit sized level 0 to the region
    // instead, the same (x, y) would address different pixels in the two
    // libraries — silently, for every Leica slide.
    assert_eq!(slide.level_count(), 5);
    assert_eq!((w0, h0), (53130, 153470));
    let expected = [
        (53130, 153470),
        (13283, 38368),
        (3321, 9592),
        (831, 2401),
        (208, 601),
    ];
    for (level, dims) in expected.iter().enumerate() {
        assert_eq!(
            slide.level_dimensions(level as i32).unwrap(),
            *dims,
            "level {level} dimensions"
        );
    }
    // OpenSlide averages the per-axis ratios; check one against its own report.
    let ds1 = slide.level_downsample(1).unwrap();
    assert!(
        (ds1 - 3.999_898_652_415_998).abs() < 1e-9,
        "level 1 downsample {ds1}"
    );

    for (name, value) in [
        ("openslide.bounds-x", "10778"),
        ("openslide.bounds-y", "35096"),
        ("openslide.bounds-width", "36832"),
        ("openslide.bounds-height", "38432"),
        ("openslide.region[0].x", "10778"),
        ("openslide.region[0].y", "35096"),
        ("openslide.region[0].width", "36832"),
        ("openslide.region[0].height", "38432"),
    ] {
        assert_eq!(slide.property_value(name), Some(value), "{name}");
    }

    // ── read_region: the canvas origin is empty glass, the bounds origin is not ──
    // This is the pair that fails if the coordinate spaces disagree. OpenSlide
    // returns a uniformly transparent tile at (0, 0) and real image at the bounds
    // origin; so must we.
    let pixels = slide.read_region(0, 0, 0, 512, 512).unwrap();
    assert_eq!(pixels.len(), 512 * 512);
    assert!(
        pixels.iter().all(|&p| p == 0),
        "level 0 (0, 0) is outside the scanned region and must be transparent"
    );

    let bx: i64 = slide
        .property_value("openslide.bounds-x")
        .unwrap()
        .parse()
        .unwrap();
    let by: i64 = slide
        .property_value("openslide.bounds-y")
        .unwrap()
        .parse()
        .unwrap();
    let at_bounds = slide.read_region(bx, by, 0, 512, 512).unwrap();
    assert!(
        at_bounds.iter().any(|&p| p != 0),
        "the bounds origin should hold real image data"
    );

    // ── Smallest level has visible tissue ──
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

    // ── Macro associated image ──
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
