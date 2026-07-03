//! Integration tests for the 3DHistech MIRAX (MRXS) vendor driver.
//!
//! These run against `Mirax/CMU-1.zip` from the OpenSlide test data server
//! (declared in `tests/testdata.yaml`) — a brightfield, JPEG-tiled slide,
//! SLIDE_VERSION 1.9. MIRAX is a multi-file format: the ZIP holds `CMU-1.mrxs`
//! plus a `CMU-1/` directory of `Slidedat.ini`, `Index.dat`, and `Data####.dat`
//! blobs, so we extract it (like Trestle) and open the extracted `.mrxs`. The
//! INI + binary Index.dat parsing and camera-position math are unit-tested in
//! `src/vendor/mirax.rs`; these exercise the full open + tile-placement read
//! path against real pixels. The slide is ~565 MiB, so they skip via
//! `require_mirax_slide!` when downloads are disabled.
//!
//! Assertions about the same slide are grouped so we open it a single time per
//! test rather than re-parsing it once per assertion.

mod common;

use slidekit::OpenSlide;

/// Extract the MIRAX ZIP (preserving its `CMU-1/` dir + `CMU-1.mrxs`) and
/// return the path to the extracted `.mrxs` file.
fn mirax_slide_path() -> Option<std::path::PathBuf> {
    let zip_path = common::test_slide_path("Mirax/CMU-1.zip")?;
    let extract_dir = zip_path.parent()?.join("extracted");
    let mrxs_path = extract_dir.join("CMU-1.mrxs");

    if mrxs_path.exists() {
        return Some(mrxs_path);
    }

    std::fs::create_dir_all(&extract_dir).ok()?;
    let file = std::fs::File::open(&zip_path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).ok()?;
        let name = entry.name().to_string();
        if name.ends_with('/') {
            continue;
        }
        let out_path = extract_dir.join(&name);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        let mut out_file = std::fs::File::create(&out_path).ok()?;
        std::io::copy(&mut entry, &mut out_file).ok()?;
    }

    mrxs_path.exists().then_some(mrxs_path)
}

macro_rules! require_mirax_slide {
    () => {
        match mirax_slide_path() {
            Some(p) => p,
            None => {
                eprintln!("SKIP: MIRAX test slide not available");
                return;
            }
        }
    };
}

/// Everything we can assert about the extracted `CMU-1.mrxs` from a single open.
#[test]
fn cmu1() {
    let path = require_mirax_slide!();

    // ── Detection ──
    assert_eq!(OpenSlide::detect_vendor(&path).as_deref(), Some("mirax"));

    let slide = OpenSlide::open(&path).expect("should open MIRAX CMU-1");

    // ── Pyramid levels ──
    // CMU-1.mrxs is the canonical 10-level, ~109240x220696 slide.
    assert_eq!(slide.level_count(), 10);
    assert_eq!(slide.level0_dimensions(), (109240, 220696));
    assert_eq!(slide.level_downsample(0).unwrap(), 1.0);

    // Downsamples are strict powers of two; dimensions strictly shrink.
    for i in 1..slide.level_count() {
        let ds = slide.level_downsample(i).unwrap();
        let prev = slide.level_downsample(i - 1).unwrap();
        assert!((ds - prev * 2.0).abs() < 1e-9, "level {i} downsample {ds}");

        let (w, h) = slide.level_dimensions(i).unwrap();
        let (pw, ph) = slide.level_dimensions(i - 1).unwrap();
        assert!(w < pw && h < ph, "level {i} should be smaller");
    }

    // ── Properties: vendor and metadata ──
    assert_eq!(slide.property_value("openslide.vendor"), Some("mirax"));
    assert_eq!(
        slide.property_value("openslide.objective-power"),
        Some("20")
    );
    assert_eq!(slide.property_value("openslide.mpp-x"), Some("0.2325"));
    assert_eq!(slide.property_value("openslide.mpp-y"), Some("0.2325"));
    // Every INI group/key is surfaced as mirax.*
    assert!(
        slide
            .property_names()
            .iter()
            .any(|n| n.starts_with("mirax.GENERAL.")),
        "expected mirax.GENERAL.* properties from Slidedat.ini"
    );

    // ── Associated images ──
    // OpenSlide's naming: SlideThumbnail→macro, SlideBarcode→label,
    // SlidePreview→thumbnail. All three are JPEG in CMU-1.
    let names = slide.associated_image_names();
    for want in ["macro", "label", "thumbnail"] {
        assert!(names.iter().any(|n| n == want), "missing associated {want}");
        let (w, h) = slide.associated_image_dimensions(want).unwrap();
        assert!(w > 0 && h > 0, "{want} dimensions should be positive");
        // Pixel buffer matches the declared dimensions and decodes opaque.
        let px = slide.read_associated_image(want).unwrap();
        assert_eq!(px.len(), (w * h) as usize);
        assert!(px.iter().all(|&p| (p >> 24) == 0xFF), "{want} not opaque");
    }

    // ── read_region over tissue is well-formed ──
    // The scanned tissue sits around x∈[10k,77k], y∈[89k,183k] in level-0
    // pixels; read a region squarely inside it.
    let px = slide.read_region(20000, 100000, 0, 512, 512).unwrap();
    assert_eq!(px.len(), 512 * 512);
    assert!(
        px.iter().any(|&p| p != 0),
        "region over tissue should have visible content"
    );
    // Pixels that came from a decoded JPEG tile are fully opaque.
    assert!(
        px.iter().filter(|&&p| p != 0).all(|&p| (p >> 24) == 0xFF),
        "decoded pixels should be opaque"
    );

    // ── Smallest level shows the tissue ──
    let last = slide.level_count() - 1;
    let (w, h) = slide.level_dimensions(last).unwrap();
    let px = slide.read_region(0, 0, last, w, h).expect("read smallest");
    assert_eq!(px.len(), (w * h) as usize);
    assert!(
        px.iter().filter(|&&p| p != 0).count() > 1000,
        "smallest level should show the tissue"
    );
}
