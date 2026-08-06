//! JPEG XL tile decoder.
//!
//! DICOM WSI added JPEG XL transfer syntaxes (`…1.2.4.140`/`.141`/`.142`) and
//! TIFF has two registered compression codes for it, so slides encoded this way
//! are starting to appear. OpenSlide 4.0 decodes none of them.
//!
//! Backed by `jxl-oxide`, a pure-Rust JPEG XL decoder, so this costs no C
//! dependency — libjxl would have meant shipping a C++ toolchain build.

use crate::color::argb;
use crate::error::{OpenSlideError, Result};

use super::TileDecoder;

/// Decoder for JPEG XL tiles.
#[derive(Debug, Default, Clone, Copy)]
pub struct JxlDecoder;

impl JxlDecoder {
    pub fn new() -> Self {
        Self
    }
}

impl TileDecoder for JxlDecoder {
    fn decode(&self, data: &[u8], width: u32, height: u32) -> Result<Vec<u32>> {
        let image = jxl_oxide::JxlImage::builder()
            .read(std::io::Cursor::new(data))
            .map_err(|e| OpenSlideError::Decode(format!("JPEG XL parse error: {e}")))?;

        if image.width() != width || image.height() != height {
            return Err(OpenSlideError::Decode(format!(
                "JPEG XL size mismatch: expected {}x{}, got {}x{}",
                width,
                height,
                image.width(),
                image.height()
            )));
        }

        let render = image
            .render_frame(0)
            .map_err(|e| OpenSlideError::Decode(format!("JPEG XL decode error: {e}")))?;

        // `stream()` interleaves the channels the image actually carries and
        // emits normalized floats, so the channel count drives the unpacking
        // rather than any assumption about the colour encoding.
        let mut stream = render.stream();
        let channels = stream.channels() as usize;
        let count = (width as usize) * (height as usize);
        let mut samples = vec![0f32; count * channels];
        stream.write_to_buffer(&mut samples);

        let q = |v: f32| -> u8 { (v * 255.0).round().clamp(0.0, 255.0) as u8 };
        let mut out = Vec::with_capacity(count);
        match channels {
            1 => {
                for c in samples.chunks_exact(1).take(count) {
                    let g = q(c[0]);
                    out.push(argb(0xFF, g, g, g));
                }
            }
            2 => {
                for c in samples.chunks_exact(2).take(count) {
                    let g = q(c[0]);
                    out.push(argb(q(c[1]), g, g, g));
                }
            }
            3 => {
                for c in samples.chunks_exact(3).take(count) {
                    out.push(argb(0xFF, q(c[0]), q(c[1]), q(c[2])));
                }
            }
            4 => {
                for c in samples.chunks_exact(4).take(count) {
                    out.push(argb(q(c[3]), q(c[0]), q(c[1]), q(c[2])));
                }
            }
            n => {
                return Err(OpenSlideError::Decode(format!(
                    "JPEG XL tile has {n} channels; expected 1-4"
                )));
            }
        }

        if out.len() != count {
            return Err(OpenSlideError::Decode(format!(
                "JPEG XL decoded {} pixels, expected {count}",
                out.len()
            )));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4x2 lossless RGB, encoded with `cjxl -d 0`. Lossless means the decode is
    /// checked against exact values rather than a tolerance.
    const RGB_4X2: &[u8] = &[
        0xFF, 0x0A, 0x08, 0x70, 0x10, 0x09, 0x08, 0x04, 0x01, 0x00, 0x6C, 0x00, 0x4B, 0x18, 0x8B,
        0x15, 0x52, 0x5C, 0xD0, 0xED, 0x02, 0x00, 0x01, 0x88, 0x12, 0x80, 0xCD, 0x7C, 0x81, 0x08,
        0x00, 0x1E, 0x00, 0x2B, 0x62, 0x30, 0xB8, 0x5B, 0x36,
    ];

    /// 2x2 lossless RGBA, so the alpha channel is exercised too.
    const RGBA_2X2: &[u8] = &[
        0xFF, 0x0A, 0x08, 0x10, 0xB0, 0x12, 0x08, 0x10, 0x10, 0x00, 0x7C, 0x00, 0x4B, 0x18, 0x8B,
        0x15, 0x52, 0x5C, 0x16, 0xEA, 0x86, 0xC7, 0x36, 0x29, 0xE4, 0x53, 0x5C, 0x40, 0x06, 0x00,
        0xFE, 0xB3, 0xE8, 0xBE, 0xDE, 0x6C, 0x5A, 0xA4, 0x6C, 0x2D, 0x6E, 0x9A, 0x9D,
    ];

    #[test]
    fn decodes_lossless_rgb_exactly() {
        let got = JxlDecoder.decode(RGB_4X2, 4, 2).expect("should decode");
        assert_eq!(
            got,
            vec![
                0xFFFF0000, 0xFF00FF00, 0xFF0000FF, 0xFFFFFFFF, // row 0
                0xFF000000, 0xFF808080, 0xFFFFFF00, 0xFF00FFFF, // row 1
            ]
        );
    }

    #[test]
    fn decodes_lossless_rgba_with_alpha() {
        let got = JxlDecoder.decode(RGBA_2X2, 2, 2).expect("should decode");
        assert_eq!(got, vec![0xFFFF0000, 0x8000FF00, 0x000000FF, 0xFFFFFFFF]);
    }

    #[test]
    fn rejects_size_mismatch() {
        let err = JxlDecoder
            .decode(RGB_4X2, 64, 64)
            .expect_err("size must be checked");
        assert!(err.to_string().contains("size mismatch"), "{err}");
    }

    #[test]
    fn rejects_garbage() {
        assert!(JxlDecoder.decode(b"not a jxl stream", 1, 1).is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(JxlDecoder.decode(&[], 16, 16).is_err());
    }
}
