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
    // Aperio's profile comes out of a TIFF tag the driver has already parsed, so
    // it is in hand at open; the size must agree either way.
    let icc = slide.icc_profile().expect("read the ICC profile");
    assert!(icc.is_some(), "CMU-1.svs should have an ICC profile");
    assert_eq!(
        icc.unwrap().len(),
        141992,
        "ICC profile size should be 141992"
    );
    assert_eq!(slide.icc_profile_size(), Some(141992));

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
            .expect("read the thumbnail ICC profile")
            .expect("thumbnail should have an ICC profile")
            .len(),
        141992,
        "thumbnail ICC profile size should be 141992"
    );
    assert_eq!(
        slide.associated_image_icc_profile_size("thumbnail"),
        Some(141992)
    );
    // An image that isn't there has no profile, and that is not an error.
    assert_eq!(
        slide
            .associated_image_icc_profile("nope")
            .expect("absent is not a failure"),
        None
    );
    assert_eq!(slide.associated_image_icc_profile_size("nope"), None);
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

/// Mean per-channel deviation from neutral gray over a region's opaque pixels.
///
/// A JP2K tile decoded in the wrong colour space still looks plausible in the
/// aggregate — the failure is a *colour cast*, not noise — so a plain "did it
/// decode" assertion misses it. Reading a 33003 tile's YCbCr components as RGB
/// leaves luma in red and both chroma planes hovering around 128, which drives
/// this statistic far above what stained tissue produces.
///
/// The ceiling below is measured, not guessed. Over the same tissue window,
/// `slidetool` reports 33.2 on `JP2K-33003-1` and 21.5 on `CMU-1-JP2K-33005`;
/// slidekit reproduces both exactly, since the `openjpeg` backend decodes
/// bit-identically to the C library. These are genuinely saturated H&E slides.
/// A YCbCr-as-RGB misread of the same pixels measures upwards of 55.
///
/// The margin is deliberately wide rather than an equality check: this test
/// must also pass on the pure-Rust `hayro` backend, which reconstructs the same
/// tiles a few counts differently (see `decode/jp2k/mod.rs`).
const MAX_MEAN_CHROMA: f64 = 45.0;

fn mean_chroma(pixels: &[u32]) -> f64 {
    let mut total = 0.0;
    let mut n = 0u64;
    for &p in pixels {
        if (p >> 24) & 0xFF == 0 {
            continue;
        }
        let (r, g, b) = (
            ((p >> 16) & 0xFF) as f64,
            ((p >> 8) & 0xFF) as f64,
            (p & 0xFF) as f64,
        );
        let mean = (r + g + b) / 3.0;
        total += ((r - mean).abs() + (g - mean).abs() + (b - mean).abs()) / 3.0;
        n += 1;
    }
    if n == 0 { 0.0 } else { total / n as f64 }
}

/// Assert a JP2K Aperio slide opens and its level-0 tiles decode to sane pixels.
fn check_jp2k_slide(path: &std::path::Path, label: &str) {
    assert_eq!(
        OpenSlide::detect_vendor(path).as_deref(),
        Some("aperio"),
        "{label} should be detected as aperio"
    );

    let slide = OpenSlide::open(path).unwrap_or_else(|e| panic!("should open {label}: {e}"));
    assert!(slide.level_count() >= 1, "{label} should have a level");

    // A region well inside the slide, so it lands on real tissue tiles rather
    // than the padded edge.
    let (w0, h0) = slide.level0_dimensions();
    let (x, y) = (w0 / 4, h0 / 4);
    let region = slide
        .read_region(x, y, 0, 256, 256)
        .unwrap_or_else(|e| panic!("{label} level 0 read_region should decode: {e}"));
    assert_eq!(region.len(), 256 * 256, "{label} region size");

    assert!(
        region.iter().any(|&p| (p >> 24) & 0xFF != 0),
        "{label} region should have opaque pixels"
    );
    assert!(
        region
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            > 1,
        "{label} region should not be a single flat colour"
    );

    let chroma = mean_chroma(&region);
    assert!(
        chroma < MAX_MEAN_CHROMA,
        "{label} mean chroma {chroma:.1} exceeds {MAX_MEAN_CHROMA} — the \
         components are probably being read in the wrong colour space"
    );

    // Every level must decode too: the pyramid uses the same codec throughout.
    for level in 1..slide.level_count() {
        let (lw, lh) = slide.level_dimensions(level).unwrap();
        let (rw, rh) = (64.min(lw), 64.min(lh));
        slide
            .read_region(0, 0, level, rw, rh)
            .unwrap_or_else(|e| panic!("{label} level {level} should decode: {e}"));
    }
}

/// `JP2K-33003-1.svs`: JPEG 2000 tiles whose components are YCbCr with the
/// chroma subsampled 2:1 horizontally.
#[test]
fn jp2k_33003_ycbcr() {
    let path = require_slide!("Aperio/JP2K-33003-1.svs");
    check_jp2k_slide(&path, "JP2K-33003-1.svs");
}

/// `CMU-1-JP2K-33005.svs`: JPEG 2000 tiles whose components are already RGB.
#[test]
fn jp2k_33005_rgb() {
    let path = require_slide!("Aperio/CMU-1-JP2K-33005.svs");
    check_jp2k_slide(&path, "CMU-1-JP2K-33005.svs");
}

/// The decoded-tile cache must not change a single pixel of a tiled-TIFF read,
/// on the first read or the tenth. The regions overlap, so later reads are
/// served partly from tiles the earlier ones decoded.
#[test]
fn cached_reads_are_pixel_identical() {
    let path = require_slide!("Aperio/CMU-1-Small-Region.svs");
    common::assert_cache_is_transparent(
        || OpenSlide::open(&path).expect("should open CMU-1-Small-Region.svs"),
        0,
        &[(0, 0, 300, 300), (150, 150, 300, 300), (0, 150, 300, 300)],
    );
}

/// Same, on a JPEG-2000 slide, at two levels.
///
/// This is where the cache pays for itself — a JP2K tile costs tens of
/// milliseconds to decode — and where a wrongly-served tile would be easiest to
/// mistake for the codec noise the two decoders legitimately produce. The
/// second level is checked because the key has to separate the levels: tile
/// (1, 1) of level 0 is not tile (1, 1) of level 1.
#[test]
fn cached_reads_are_pixel_identical_on_jp2k() {
    let path = require_slide!("Aperio/JP2K-33003-1.svs");
    let open = || OpenSlide::open(&path).expect("should open JP2K-33003-1.svs");
    common::assert_cache_is_transparent(
        open,
        0,
        &[
            (4000, 4000, 300, 300),
            (4150, 4150, 300, 300),
            (4000, 4150, 300, 300),
        ],
    );
    common::assert_cache_is_transparent(open, 1, &[(4000, 4000, 200, 200), (4100, 4100, 200, 200)]);
}
