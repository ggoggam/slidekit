//! Integration tests for the Zeiss CZI vendor driver.
//!
//! These run against CZI files from the OpenSlide test data server (declared in
//! `tests/testdata.yaml`), skipping via `require_slide!` when downloads are
//! disabled. The CZI binary parsing and BGR→ARGB conversions are unit-tested in
//! `src/vendor/zeiss.rs`; these exercise the end-to-end open + placed-subtile
//! read path against real pixels, and cover all three compressions the driver
//! supports (matching OpenSlide: uncompressed, Zstd0, Zstd1). Each slide is
//! opened a single time per test — assertions about the same slide are grouped
//! so we don't re-parse it once per assertion:
//!
//!   - `Zeiss-5-Uncompressed.czi` — brightfield pyramidal main image.
//!   - `Zeiss-5-SlidePreview-Zstd0.czi` — a Zstd0 BGR48 image.
//!   - `Zeiss-5-SlidePreview-Zstd1-HiLo.czi` — a Zstd1 image with HiLo unpack.
//!   - `Zeiss-5-JXR.czi` — JPEG XR, which OpenSlide (and this driver) reject.

mod common;

use slidekit::OpenSlide;

const UNCOMPRESSED: &str = "Zeiss/Zeiss-5-Uncompressed.czi";
const ZSTD0: &str = "Zeiss/Zeiss-5-SlidePreview-Zstd0.czi";
const ZSTD1_HILO: &str = "Zeiss/Zeiss-5-SlidePreview-Zstd1-HiLo.czi";
const JXR: &str = "Zeiss/Zeiss-5-JXR.czi";

/// Everything we can assert about `Zeiss-5-Uncompressed.czi` from a single open.
#[test]
fn zeiss_5_uncompressed() {
    let path = require_slide!(UNCOMPRESSED);

    // ── Detection ──
    assert_eq!(OpenSlide::detect_vendor(&path).as_deref(), Some("zeiss"));

    let slide = OpenSlide::open(&path).expect("should open uncompressed CZI");

    // ── Levels ──
    assert!(slide.level_count() >= 1, "expected at least one level");
    let (w0, h0) = slide.level0_dimensions();
    assert!(w0 > 0 && h0 > 0, "level 0 dimensions should be positive");
    assert_eq!(slide.level_downsample(0).unwrap(), 1.0);

    // Downsamples strictly increase; dimensions shrink.
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
    assert_eq!(slide.property_value("openslide.vendor"), Some("zeiss"));
    // Brightfield 10x scan → objective power present and numeric.
    if let Some(op) = slide.property_value("openslide.objective-power") {
        assert!(
            op.parse::<f64>().is_ok(),
            "objective-power {op:?} not numeric"
        );
    }
    // Scaling metadata → microns-per-pixel.
    if let Some(mpp) = slide.property_value("openslide.mpp-x") {
        assert!(mpp.parse::<f64>().is_ok(), "mpp-x {mpp:?} not numeric");
    }

    // ── read_region: level 0 ──
    let rw = w0.min(512);
    let rh = h0.min(512);
    let pixels = slide.read_region(0, 0, 0, rw, rh).unwrap();
    assert_eq!(pixels.len(), (rw * rh) as usize);
    assert!(
        pixels.iter().any(|&p| p != 0),
        "level 0 region should contain visible content"
    );
    // Uncompressed BGR tiles decode fully opaque; a two-scene mosaic leaves
    // gaps between scenes as transparent black. So every pixel is either a gap
    // (fully transparent) or decoded content (fully opaque) — never partial.
    assert!(
        pixels
            .iter()
            .all(|&p| (p >> 24) == 0xFF || (p >> 24) == 0x00),
        "decoded pixels should be opaque or transparent-gap, never partial alpha"
    );

    // ── read_region: smallest level ──
    let last = slide.level_count() - 1;
    let (w, h) = slide.level_dimensions(last).unwrap();
    let (rw, rh) = (w.min(2048), h.min(2048));
    let pixels = slide
        .read_region(0, 0, last, rw, rh)
        .expect("read smallest level");
    assert_eq!(pixels.len(), (rw * rh) as usize);
    assert!(
        pixels.iter().any(|&p| p != 0),
        "smallest level should contain visible content"
    );
}

/// Zstd0 decode path against a real Zstd0-compressed CZI.
#[test]
fn zeiss_5_slidepreview_zstd0() {
    let path = require_slide!(ZSTD0);
    let slide = OpenSlide::open(&path).expect("should open Zstd0 CZI");
    let (w, h) = slide.level0_dimensions();
    assert!(w > 0 && h > 0);
    let (rw, rh) = (w.min(256), h.min(256));
    let pixels = slide
        .read_region(0, 0, 0, rw, rh)
        .expect("read Zstd0 region");
    assert!(
        pixels.iter().any(|&p| p != 0),
        "Zstd0 region should have content"
    );
}

/// Zstd1 + HiLo unpack path against a real HiLo-encoded CZI.
#[test]
fn zeiss_5_slidepreview_zstd1_hilo() {
    let path = require_slide!(ZSTD1_HILO);
    let slide = OpenSlide::open(&path).expect("should open Zstd1-HiLo CZI");
    let (w, h) = slide.level0_dimensions();
    assert!(w > 0 && h > 0);
    let (rw, rh) = (w.min(256), h.min(256));
    let pixels = slide
        .read_region(0, 0, 0, rw, rh)
        .expect("read Zstd1 region");
    assert!(
        pixels.iter().any(|&p| p != 0),
        "Zstd1 region should have content"
    );
}

/// JPEG XR is detected as Zeiss but must fail to open with a clear error, since
/// neither OpenSlide nor this pure-Rust port can decode JXR tiles.
#[test]
fn zeiss_5_jxr() {
    let path = require_slide!(JXR);

    // ── Detection ──
    assert_eq!(
        OpenSlide::detect_vendor(&path).as_deref(),
        Some("zeiss"),
        "JXR CZI should still be detected as Zeiss"
    );

    let msg = match OpenSlide::open(&path) {
        Ok(_) => panic!("JXR CZI should not open"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("compression") && msg.contains("not supported"),
        "expected an unsupported-compression error, got: {msg}"
    );
}
