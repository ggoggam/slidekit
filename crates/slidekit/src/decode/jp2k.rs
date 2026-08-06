//! JPEG 2000 tile decoder.
//!
//! Backed by `hayro-jpeg2000`, a memory-safe pure-Rust JPEG 2000 decoder, so
//! slidekit stays a single `cargo add` with no C toolchain or system library
//! for a caller to install. (The transpiled-OpenJPEG crate `openjp2` was the
//! alternative: it exposes no in-memory stream constructor, and its tile
//! teardown path hits undefined behaviour on real Aperio tiles.)
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

/// One decoded component, addressed in full-image pixel coordinates.
///
/// `hayro-jpeg2000` replicates a subsampled component up to the full image
/// size before handing it back, so `sub_x`/`sub_y` are normally 1. They are
/// carried anyway: the crate does not document that as a guarantee, and
/// reading a half-width chroma plane at the full stride yields a plausible but
/// wrong image rather than an error — the worst possible failure for a slide
/// viewer. Deriving the divisor from the sample count keeps both layouts
/// correct.
struct Plane<'a> {
    samples: &'a [f32],
    stride: usize,
    sub_x: usize,
    sub_y: usize,
    /// Multiplier that scales a sample to 0..=255.
    scale: f32,
}

impl<'a> Plane<'a> {
    fn new(samples: &'a [f32], bit_depth: u8, width: u32, height: u32, sub: (u8, u8)) -> Self {
        let (w, h) = (width as usize, height as usize);
        let (sub_x, sub_y) = if samples.len() >= w * h {
            (1, 1)
        } else {
            (sub.0.max(1) as usize, sub.1.max(1) as usize)
        };
        let max = ((1u32 << bit_depth.clamp(1, 16)) - 1) as f32;
        Self {
            samples,
            stride: w.div_ceil(sub_x),
            sub_x,
            sub_y,
            scale: if bit_depth == 8 { 1.0 } else { 255.0 / max },
        }
    }

    /// The sample at full-image position (`row`, `col`), clamped to 0..=255.
    fn at(&self, row: usize, col: usize) -> u8 {
        let idx = (row / self.sub_y) * self.stride + (col / self.sub_x);
        let v = self.samples.get(idx).copied().unwrap_or(0.0) * self.scale;
        v.round().clamp(0.0, 255.0) as u8
    }
}

impl TileDecoder for Jp2kDecoder {
    fn decode(&self, data: &[u8], width: u32, height: u32) -> Result<Vec<u32>> {
        use hayro_jpeg2000::{DecodeSettings, DecoderContext, Image};

        let settings = DecodeSettings::default();
        let image = Image::new(data, &settings)
            .map_err(|e| OpenSlideError::Decode(format!("JPEG 2000 parse error: {e:?}")))?;

        if image.width() != width || image.height() != height {
            return Err(OpenSlideError::Decode(format!(
                "JPEG 2000 size mismatch: expected {}x{}, got {}x{}",
                width,
                height,
                image.width(),
                image.height()
            )));
        }

        let subsampling = codestream_subsampling(data);
        let mut ctx = DecoderContext::default();
        let decoded = image
            .decode(&mut ctx)
            .map_err(|e| OpenSlideError::Decode(format!("JPEG 2000 decode error: {e:?}")))?;
        let comps = decoded.components();

        let sub_of = |i: usize| -> (u8, u8) {
            subsampling
                .as_ref()
                .and_then(|s| s.get(i).copied())
                .unwrap_or((1, 1))
        };
        let plane = |i: usize| -> Plane<'_> {
            Plane::new(
                comps[i].samples(),
                comps[i].bit_depth(),
                width,
                height,
                sub_of(i),
            )
        };

        let (w, h) = (width as usize, height as usize);
        let mut out = Vec::with_capacity(w * h);

        match comps.len() {
            1 => {
                let y = plane(0);
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
                let (p0, p1, p2) = (plane(0), plane(1), plane(2));
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

/// Per-component `(XRsiz, YRsiz)` subsampling factors from the codestream's SIZ
/// marker, or `None` if the SIZ can't be found or is truncated.
///
/// Handles a JP2-boxed image as well as a bare codestream by scanning a bounded
/// prefix for the SOC+SIZ marker pair rather than walking the box structure.
fn codestream_subsampling(data: &[u8]) -> Option<Vec<(u8, u8)>> {
    /// SOC followed by SIZ — the first four bytes of every codestream.
    const SOC_SIZ: [u8; 4] = [0xFF, 0x4F, 0xFF, 0x51];
    /// A JP2 header preceding the contiguous codestream box is far smaller.
    const SCAN_LIMIT: usize = 4096;

    let limit = data.len().min(SCAN_LIMIT);
    let start = data[..limit]
        .windows(4)
        .position(|w| w == SOC_SIZ)
        .map(|p| p + 2)?; // position of the SIZ marker itself

    // SIZ: marker(2) Lsiz(2) Rsiz(2) Xsiz(4) Ysiz(4) XOsiz(4) YOsiz(4)
    //      XTsiz(4) YTsiz(4) XTOsiz(4) YTOsiz(4) Csiz(2), then 3 bytes/component.
    let csiz_at = start + 38;
    let csiz = u16::from_be_bytes(data.get(csiz_at..csiz_at + 2)?.try_into().ok()?) as usize;
    let comps_at = csiz_at + 2;

    (0..csiz)
        .map(|c| {
            let o = comps_at + 3 * c;
            Some((*data.get(o + 1)?, *data.get(o + 2)?))
        })
        .collect()
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

    #[test]
    fn siz_subsampling_reads_aperio_422_layout() {
        // Minimal SIZ for a 256x256 3-component image with 2:1 horizontal
        // chroma subsampling — the Aperio 33003 layout.
        let mut d = vec![0xFF, 0x4F, 0xFF, 0x51];
        d.extend_from_slice(&47u16.to_be_bytes()); // Lsiz
        d.extend_from_slice(&0u16.to_be_bytes()); // Rsiz
        for v in [256u32, 256, 0, 0, 256, 256, 0, 0] {
            d.extend_from_slice(&v.to_be_bytes());
        }
        d.extend_from_slice(&3u16.to_be_bytes()); // Csiz
        d.extend_from_slice(&[7, 1, 1, 7, 2, 1, 7, 2, 1]); // Ssiz, XRsiz, YRsiz

        assert_eq!(
            codestream_subsampling(&d),
            Some(vec![(1, 1), (2, 1), (2, 1)])
        );
    }

    #[test]
    fn siz_subsampling_tolerates_garbage() {
        assert_eq!(codestream_subsampling(&[]), None);
        assert_eq!(codestream_subsampling(&[0u8; 64]), None);
        // Truncated right after the marker pair.
        assert_eq!(codestream_subsampling(&[0xFF, 0x4F, 0xFF, 0x51]), None);
    }

    /// A subsampled plane replicates across the missing columns; a
    /// full-resolution plane is read straight through.
    #[test]
    fn plane_indexing_handles_both_layouts() {
        let half: Vec<f32> = (0..8).map(|v| v as f32).collect(); // 2 rows x 4 cols
        let p = Plane::new(&half, 8, 8, 2, (2, 1));
        assert_eq!(p.sub_x, 2);
        assert_eq!((p.at(0, 0), p.at(0, 1)), (0, 0));
        assert_eq!((p.at(0, 2), p.at(0, 3)), (1, 1));
        assert_eq!(p.at(1, 0), 4);

        let full: Vec<f32> = (0..16).map(|v| v as f32).collect(); // 2 rows x 8 cols
        let p = Plane::new(&full, 8, 8, 2, (2, 1));
        assert_eq!(p.sub_x, 1, "a full-size plane must not be re-upsampled");
        assert_eq!((p.at(0, 0), p.at(0, 1)), (0, 1));
        assert_eq!(p.at(1, 0), 8);
    }
}
