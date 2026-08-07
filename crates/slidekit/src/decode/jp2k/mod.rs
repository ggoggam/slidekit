//! JPEG 2000 tile decoder.
//!
//! The codestream reconstruction itself comes from a swappable backend, chosen
//! at compile time:
//!
//! - **`openjpeg`** (default) — the OpenJPEG C library, vendored and built by
//!   `openjpeg-sys`, which is the same decoder OpenSlide links against. Tiles
//!   decode *bit-identically* to the C library rather than merely close to it.
//! - **`hayro`** — `hayro-jpeg2000`, a memory-safe pure-Rust decoder, for
//!   builds that cannot run a C compiler (wasm) or will not link C at all.
//!
//! **`hayro` is not bit-exact on Aperio slides, and the gap is not rounding.**
//! Aperio ships Kakadu-encoded tiles at `Q=30`, roughly 1:27, which truncates
//! most code-blocks mid-bitplane. Reconstructing a truncated code-block needs
//! the midpoint rule (add half of the undecoded remainder); `hayro-jpeg2000`
//! 0.4 does not reproduce OpenJPEG there. Measured on a real Aperio tile
//! re-encoded at 1:27 and compared against the *original* image, OpenJPEG
//! lands at 42.99 dB PSNR and `hayro` at 34.97 dB — the pure-Rust backend
//! discards about 8 dB of quality the encoder already paid for. Below roughly
//! 1:15 the two agree to ~0.2 per channel, so the defect only bites on
//! aggressively rate-controlled slides, which is exactly what Aperio ships.
//! Decode speed is within 8% either way, so `openjpeg` costs nothing but the
//! C toolchain.
//!
//! **Colorspace is not in the codestream.** Aperio stores JPEG 2000 tiles under
//! two private TIFF compression tags that mean the same codestream syntax but
//! different component semantics: 33005 codes RGB (the codestream's own
//! multi-component transform yields R,G,B directly), while 33003 codes YCbCr
//! with the chroma horizontally subsampled 2:1 and *no* multi-component
//! transform flagged — a decoder that just hands back components produces a
//! near-monochrome image with the chroma planes read as color. Which one
//! applies is knowable only from the container, so the caller passes it in.
//!
//! The YCbCr conversion reproduces OpenSlide's fixed-point tables exactly (see
//! [`YCbCrTables`]) rather than an arbitrary BT.601 matrix, so pixels match the
//! C library instead of drifting a few counts away from it.

use std::sync::LazyLock;

use crate::color::argb;
use crate::error::{OpenSlideError, Result};

use super::TileDecoder;

#[cfg(feature = "openjpeg")]
#[path = "openjpeg.rs"]
mod backend;

#[cfg(all(not(feature = "openjpeg"), feature = "hayro"))]
#[path = "hayro.rs"]
mod backend;

#[cfg(all(not(feature = "openjpeg"), not(feature = "hayro")))]
compile_error!(
    "slidekit needs a JPEG 2000 backend: enable the `openjpeg` feature (default, \
     bit-exact with OpenSlide) or `hayro` (pure Rust, not bit-exact on Aperio)"
);

/// One decoded component, at its own — possibly subsampled — size.
///
/// Backends normalise to this: samples already scaled to 0..=255, laid out
/// `width` per row. `width`/`height` are the component's *own* dimensions, not
/// the image's, which is what makes the subsampled-chroma case fall out of
/// [`Plane`] indexing rather than needing a special case.
pub(crate) struct Component {
    pub samples: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

/// How to read the three components a JPEG 2000 tile decodes to.
///
/// The codestream does not say, so the vendor driver decides from its own
/// metadata — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Jp2kColorSpace {
    /// Components are already R, G, B. Aperio compression 33005, and DICOM
    /// `YBR_ICT`/`YBR_RCT`, whose transform the codestream applies itself.
    Rgb,
    /// Components are Y, Cb, Cr and need converting. Aperio compression 33003.
    YCbCr,
}

/// OpenSlide's YCbCr → RGB lookup tables, reproduced from its `make-tables.c`.
///
/// R and B are a rounded integer chroma offset added to luma. G sums two
/// 16-bit fixed-point precursors and shifts back down; the `0.5` folded into
/// `g_cb` is what rounds the result, so the shift can be a plain arithmetic
/// one. Keeping the same fixed-point shape as the C library is what makes our
/// pixels match it rather than land a count or two off.
struct YCbCrTables {
    r_cr: [i16; 256],
    g_cb: [i32; 256],
    g_cr: [i32; 256],
    b_cb: [i16; 256],
}

static YCBCR: LazyLock<YCbCrTables> = LazyLock::new(|| {
    let mut t = YCbCrTables {
        r_cr: [0; 256],
        g_cb: [0; 256],
        g_cr: [0; 256],
        b_cb: [0; 256],
    };
    for i in 0..256 {
        let c = i as f64 - 128.0;
        t.r_cr[i] = (1.402 * c).round() as i16;
        t.g_cb[i] = ((1 << 16) as f64 * (0.5 - 0.34414 * c)).round() as i32;
        t.g_cr[i] = ((1 << 16) as f64 * (-0.71414 * c)).round() as i32;
        t.b_cb[i] = (1.772 * c).round() as i16;
    }
    t
});

/// Decoder for JPEG 2000 tiles (raw J2K codestreams or JP2-boxed images).
pub struct Jp2kDecoder {
    colorspace: Jp2kColorSpace,
}

impl Jp2kDecoder {
    /// Tiles whose components are R, G, B.
    pub fn rgb() -> Self {
        Self {
            colorspace: Jp2kColorSpace::Rgb,
        }
    }

    /// Tiles whose components are Y, Cb, Cr.
    pub fn ycbcr() -> Self {
        Self {
            colorspace: Jp2kColorSpace::YCbCr,
        }
    }

    /// A decoder for `colorspace`.
    pub fn new(colorspace: Jp2kColorSpace) -> Self {
        Self { colorspace }
    }
}

/// A component addressed in full-image pixel coordinates.
///
/// The subsampling factor is derived from the component's own size against the
/// image's, never from a declared value: reading a half-width chroma plane at
/// the full stride yields a plausible but wrong image rather than an error —
/// the worst possible failure for a slide viewer.
struct Plane<'a> {
    samples: &'a [u8],
    stride: usize,
    sub_x: usize,
    sub_y: usize,
}

impl<'a> Plane<'a> {
    fn new(comp: &'a Component, width: u32, height: u32) -> Self {
        let (w, h) = (width as usize, height as usize);
        Self {
            samples: &comp.samples,
            stride: comp.width.max(1),
            sub_x: w.div_ceil(comp.width.max(1)).max(1),
            sub_y: h.div_ceil(comp.height.max(1)).max(1),
        }
    }

    /// The sample at full-image position (`row`, `col`).
    fn at(&self, row: usize, col: usize) -> u8 {
        let idx = (row / self.sub_y) * self.stride + (col / self.sub_x);
        self.samples.get(idx).copied().unwrap_or(0)
    }
}

impl TileDecoder for Jp2kDecoder {
    fn decode(&self, data: &[u8], width: u32, height: u32) -> Result<Vec<u32>> {
        let (iw, ih, comps) = backend::decode(data)?;

        if iw != width || ih != height {
            return Err(OpenSlideError::Decode(format!(
                "JPEG 2000 size mismatch: expected {width}x{height}, got {iw}x{ih}"
            )));
        }

        let (w, h) = (width as usize, height as usize);
        let mut out = Vec::with_capacity(w * h);

        match comps.len() {
            1 => {
                let y = Plane::new(&comps[0], width, height);
                for row in 0..h {
                    for col in 0..w {
                        let g = y.at(row, col);
                        out.push(argb(0xFF, g, g, g));
                    }
                }
            }
            // A fourth component is read as colour-only and left opaque, which
            // is what OpenSlide's `unpack_argb` does. Without JP2 boxes there
            // is nothing to say whether it is alpha or the K of CMYK, and
            // guessing wrong on alpha would blank the tile; a slide with one
            // is not a case that occurs in practice either way.
            3 | 4 => {
                let p0 = Plane::new(&comps[0], width, height);
                let p1 = Plane::new(&comps[1], width, height);
                let p2 = Plane::new(&comps[2], width, height);
                match self.colorspace {
                    Jp2kColorSpace::Rgb => {
                        for row in 0..h {
                            for col in 0..w {
                                out.push(argb(
                                    0xFF,
                                    p0.at(row, col),
                                    p1.at(row, col),
                                    p2.at(row, col),
                                ));
                            }
                        }
                    }
                    Jp2kColorSpace::YCbCr => {
                        let t = &*YCBCR;
                        for row in 0..h {
                            for col in 0..w {
                                let y = p0.at(row, col) as i32;
                                let cb = p1.at(row, col) as usize;
                                let cr = p2.at(row, col) as usize;
                                let r = y + t.r_cr[cr] as i32;
                                let g = y + ((t.g_cb[cb] + t.g_cr[cr]) >> 16);
                                let b = y + t.b_cb[cb] as i32;
                                out.push(argb(
                                    0xFF,
                                    r.clamp(0, 255) as u8,
                                    g.clamp(0, 255) as u8,
                                    b.clamp(0, 255) as u8,
                                ));
                            }
                        }
                    }
                }
            }
            n => {
                return Err(OpenSlideError::Decode(format!(
                    "JPEG 2000 tile has {n} components; expected 1, 3 or 4"
                )));
            }
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4x2 reversible (5/3) RGB codestream with the multi-component transform
    /// on, from `opj_compress -r 1 -n 1`. Lossless, so the decode is checked
    /// against exact values rather than a tolerance.
    const RGB_4X2: &[u8] = &[
        0xFF, 0x4F, 0xFF, 0x51, 0x00, 0x2F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00,
        0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00,
        0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x07, 0x01, 0x01,
        0x07, 0x01, 0x01, 0x07, 0x01, 0x01, 0xFF, 0x52, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x01, 0x01,
        0x00, 0x04, 0x04, 0x00, 0x01, 0xFF, 0x5C, 0x00, 0x04, 0x40, 0x40, 0xFF, 0x64, 0x00, 0x25,
        0x00, 0x01, 0x43, 0x72, 0x65, 0x61, 0x74, 0x65, 0x64, 0x20, 0x62, 0x79, 0x20, 0x4F, 0x70,
        0x65, 0x6E, 0x4A, 0x50, 0x45, 0x47, 0x20, 0x76, 0x65, 0x72, 0x73, 0x69, 0x6F, 0x6E, 0x20,
        0x32, 0x2E, 0x35, 0x2E, 0x34, 0xFF, 0x90, 0x00, 0x0A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2A,
        0x00, 0x01, 0xFF, 0x93, 0xDF, 0x80, 0x50, 0x0C, 0x38, 0x66, 0x83, 0xDD, 0x3D, 0xEB, 0x1A,
        0x57, 0x89, 0xDF, 0x80, 0x28, 0x0E, 0x12, 0xEE, 0x90, 0x9F, 0xDF, 0x80, 0x20, 0x02, 0xF0,
        0x06, 0x4F, 0xFF, 0xD9,
    ];

    /// The same geometry with the transform *off* (`-mct 0`), so the three
    /// stored components survive the round trip untouched. Encoded from the
    /// literal triples `(128,128,128) (255,128,128) (0,128,128) (100,200,50)`
    /// / `(200,60,220) (30,128,255) (180,255,128) (90,10,10)`, which the YCbCr
    /// decoder must read as Y, Cb, Cr.
    const YCBCR_4X2: &[u8] = &[
        0xFF, 0x4F, 0xFF, 0x51, 0x00, 0x2F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00,
        0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00,
        0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x07, 0x01, 0x01,
        0x07, 0x01, 0x01, 0x07, 0x01, 0x01, 0xFF, 0x52, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x04, 0x04, 0x00, 0x01, 0xFF, 0x5C, 0x00, 0x04, 0x40, 0x40, 0xFF, 0x64, 0x00, 0x25,
        0x00, 0x01, 0x43, 0x72, 0x65, 0x61, 0x74, 0x65, 0x64, 0x20, 0x62, 0x79, 0x20, 0x4F, 0x70,
        0x65, 0x6E, 0x4A, 0x50, 0x45, 0x47, 0x20, 0x76, 0x65, 0x72, 0x73, 0x69, 0x6F, 0x6E, 0x20,
        0x32, 0x2E, 0x35, 0x2E, 0x34, 0xFF, 0x90, 0x00, 0x0A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30,
        0x00, 0x01, 0xFF, 0x93, 0xDF, 0x80, 0x48, 0x12, 0x7E, 0xCA, 0xEF, 0xD1, 0xDB, 0x95, 0x26,
        0x60, 0xCF, 0xB4, 0x20, 0x0C, 0x1D, 0x9A, 0xCC, 0x50, 0x8F, 0xD5, 0x2F, 0xCF, 0xB4, 0x20,
        0x0B, 0x26, 0x05, 0xBC, 0xE2, 0x1E, 0x07, 0x8B, 0xFF, 0xD9,
    ];

    #[test]
    fn decodes_lossless_rgb_exactly() {
        let got = Jp2kDecoder::rgb()
            .decode(RGB_4X2, 4, 2)
            .expect("should decode");
        assert_eq!(
            got,
            vec![
                0xFFFF0000, 0xFF00FF00, 0xFF0000FF, 0xFFFFFFFF, // row 0
                0xFF000000, 0xFF808080, 0xFFFFFF00, 0xFF00FFFF, // row 1
            ]
        );
    }

    /// The expected values were computed independently from OpenSlide's
    /// published table formulas, so this checks the conversion end to end and
    /// not merely that the code agrees with itself.
    #[test]
    fn converts_ycbcr_components_to_rgb() {
        let got = Jp2kDecoder::ycbcr()
            .decode(YCBCR_4X2, 4, 2)
            .expect("should decode");
        assert_eq!(
            got,
            vec![
                0xFF808080, 0xFFFFFFFF, 0xFF000000, 0xFF0083E4, // row 0
                0xFFFF9E50, 0xFFD0001E, 0xFFB488FF, 0xFF00D700, // row 1
            ]
        );
    }

    /// Reading the same stream as RGB must give back the stored components,
    /// which is what makes the YCbCr case above a real conversion rather than
    /// a pass-through.
    #[test]
    fn rgb_mode_passes_components_through() {
        let got = Jp2kDecoder::rgb()
            .decode(YCBCR_4X2, 4, 2)
            .expect("should decode");
        assert_eq!(got[0], 0xFF808080, "(128,128,128) stays as stored");
        assert_eq!(got[3], 0xFF64C832, "(100,200,50) stays as stored");
    }

    #[test]
    fn rejects_size_mismatch() {
        let err = Jp2kDecoder::rgb()
            .decode(RGB_4X2, 64, 64)
            .expect_err("size must be checked");
        assert!(err.to_string().contains("size mismatch"), "{err}");
    }

    #[test]
    fn rejects_garbage() {
        assert!(
            Jp2kDecoder::rgb()
                .decode(b"not a codestream", 1, 1)
                .is_err()
        );
        assert!(Jp2kDecoder::ycbcr().decode(&[], 8, 8).is_err());
    }

    /// The tables must reproduce OpenSlide's generated values exactly; spot-check
    /// the identity point and both extremes.
    #[test]
    fn ycbcr_tables_match_openslide() {
        let t = &*YCBCR;
        assert_eq!(t.r_cr[128], 0);
        assert_eq!(t.b_cb[128], 0);
        assert_eq!(t.r_cr[255], (1.402 * 127.0f64).round() as i16);
        assert_eq!(t.b_cb[0], (1.772 * -128.0f64).round() as i16);
        // Neutral chroma must leave luma untouched after the shift.
        assert_eq!((t.g_cb[128] + t.g_cr[128]) >> 16, 0);
    }

    /// Neutral chroma turns a luma ramp into exact gray.
    #[test]
    fn neutral_chroma_is_gray() {
        let t = &*YCBCR;
        for y in [0i32, 1, 77, 128, 200, 255] {
            let r = y + t.r_cr[128] as i32;
            let g = y + ((t.g_cb[128] + t.g_cr[128]) >> 16);
            let b = y + t.b_cb[128] as i32;
            assert_eq!((r, g, b), (y, y, y), "luma {y} should stay neutral");
        }
    }

    /// A half-width component replicates across the missing columns; a
    /// full-resolution one is read straight through. This is the Aperio 33003
    /// chroma layout, and reading it at the wrong stride is the failure that
    /// looks like a plausible image.
    #[test]
    fn plane_indexing_handles_both_layouts() {
        let half = Component {
            samples: (0..8).collect(), // 2 rows x 4 cols, for an 8x2 image
            width: 4,
            height: 2,
        };
        let p = Plane::new(&half, 8, 2);
        assert_eq!((p.sub_x, p.sub_y), (2, 1));
        assert_eq!((p.at(0, 0), p.at(0, 1)), (0, 0));
        assert_eq!((p.at(0, 2), p.at(0, 3)), (1, 1));
        assert_eq!(p.at(1, 0), 4);

        let full = Component {
            samples: (0..16).collect(), // 2 rows x 8 cols
            width: 8,
            height: 2,
        };
        let p = Plane::new(&full, 8, 2);
        assert_eq!(p.sub_x, 1, "a full-size plane must not be re-upsampled");
        assert_eq!((p.at(0, 0), p.at(0, 1)), (0, 1));
        assert_eq!(p.at(1, 0), 8);
    }
}
