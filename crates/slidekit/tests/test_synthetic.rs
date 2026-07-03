//! Synthetic self-test slide: no fixture needed — the driver embeds tiny
//! per-codec payloads and decodes them at open. Exercises the JPEG, TIFF
//! (JPEG/LZW, classic + Big) and Zstd decode paths plus XML parsing.
//!
//! `OPENSLIDE_DEBUG` is process-global, so the gate-off and gate-on checks run
//! sequentially in one test to avoid racing other threads' env reads.

use slidekit::OpenSlide;
use slidekit::color::{alpha, blue, green, red};

const IMAGE_PIXELS: i64 = 16;
// jpeg, jpeg.rgb, tiff.jpeg, tiff.jpeg.big, tiff.lzw, tiff.lzw.big, zstd.
// jpeg.bad (invalid) and xml (not an image) are decoded but not rendered.
const RENDERED: i64 = 7;

#[test]
fn synthetic_self_test() {
    // Gate off: an empty path without the debug flag is not a slide.
    unsafe { std::env::remove_var("OPENSLIDE_DEBUG") };
    assert!(
        OpenSlide::open("").is_err(),
        "empty path must fail without the flag"
    );

    // Gate on.
    unsafe { std::env::set_var("OPENSLIDE_DEBUG", "synthetic") };
    let slide = OpenSlide::open("").expect("synthetic slide should open with the flag");

    // One level; a 1-row strip of 16×16 tiles, one per rendered image.
    assert_eq!(slide.level_count(), 1);
    assert_eq!(
        slide.level0_dimensions(),
        (RENDERED * IMAGE_PIXELS, IMAGE_PIXELS)
    );

    // Properties: vendor, per-item descriptions, and the image index.
    assert_eq!(slide.property_value("openslide.vendor"), Some("synthetic"));
    assert_eq!(
        slide.property_value("synthetic.item.jpeg"),
        Some("YCbCr JPEG")
    );
    assert_eq!(
        slide.property_value("synthetic.item.jpeg.bad"),
        Some("Corrupt JPEG (not rendered)")
    );
    assert_eq!(
        slide.property_value("synthetic.item.xml"),
        Some("XML document (not rendered)")
    );
    assert_eq!(slide.property_value("synthetic.image[0]"), Some("jpeg"));
    assert_eq!(slide.property_value("synthetic.image[6]"), Some("zstd"));

    // Full read returns the whole strip. Tile 0 (jpeg) carries a red swatch at
    // intra-tile pixel (4, 4) — the read path must surface the same pixels the
    // decoder validated at open.
    let (w, h) = slide.level0_dimensions();
    let region = slide.read_region(0, 0, 0, w, h).unwrap();
    assert_eq!(region.len() as i64, w * h);
    let red_px = region[(4 * w + 4) as usize];
    assert_eq!(alpha(red_px), 0xff);
    assert!(
        red(red_px) > 0xb0 && green(red_px) < 0x50 && blue(red_px) < 0x50,
        "tile 0 red swatch, got {red_px:#010x}"
    );

    // Out-of-bounds fill: shifting 8px left leaves the left half transparent and
    // the right half (first tile) opaque.
    let shifted = slide
        .read_region(-8, 0, 0, IMAGE_PIXELS, IMAGE_PIXELS)
        .unwrap();
    assert_eq!(alpha(shifted[0]), 0x00, "left of image is transparent");
    assert_eq!(alpha(shifted[8]), 0xff, "first tile is opaque");
}
