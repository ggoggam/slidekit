//! PNG tile decoder.
//!
//! MIRAX stores each tile as a standalone blob whose format the slide's INI
//! declares per zoom level, and PNG is one of the three it can name. PNG also
//! turns up as a TIFF tile codec (compression 34933) and as the encoding of
//! some associated images.
//!
//! Backed by the pure-Rust `png` crate, so nothing here needs libpng.

use crate::color::argb;
use crate::error::{OpenSlideError, Result};

use super::TileDecoder;

/// Decoder for PNG-compressed tiles.
#[derive(Debug, Default, Clone, Copy)]
pub struct PngDecoder;

impl PngDecoder {
    pub fn new() -> Self {
        Self
    }
}

impl TileDecoder for PngDecoder {
    fn decode(&self, data: &[u8], width: u32, height: u32) -> Result<Vec<u32>> {
        let decoder = png::Decoder::new(std::io::Cursor::new(data));
        let mut reader = decoder
            .read_info()
            .map_err(|e| OpenSlideError::Decode(format!("PNG header error: {e}")))?;

        let mut buf = vec![
            0;
            reader.output_buffer_size().ok_or_else(|| {
                OpenSlideError::Decode("PNG image is too large to buffer".into())
            })?
        ];
        let info = reader
            .next_frame(&mut buf)
            .map_err(|e| OpenSlideError::Decode(format!("PNG decode error: {e}")))?;

        if info.width != width || info.height != height {
            return Err(OpenSlideError::Decode(format!(
                "PNG size mismatch: expected {}x{}, got {}x{}",
                width, height, info.width, info.height
            )));
        }

        // `next_frame` expands palettes and low bit depths only when asked, so
        // take the buffer as it comes and read whatever channel layout it has.
        let pixels = &buf[..info.buffer_size()];
        let count = (width as usize) * (height as usize);
        let mut out = Vec::with_capacity(count);

        // 16-bit PNG arrives big-endian; keep the high byte, which is what the
        // 8-bit ARGB surface can hold.
        let step = match info.bit_depth {
            png::BitDepth::Sixteen => 2,
            _ => 1,
        };
        let take = |c: &[u8], i: usize| c[i * step];

        match info.color_type {
            png::ColorType::Rgb => {
                for c in pixels.chunks_exact(3 * step).take(count) {
                    out.push(argb(0xFF, take(c, 0), take(c, 1), take(c, 2)));
                }
            }
            png::ColorType::Rgba => {
                for c in pixels.chunks_exact(4 * step).take(count) {
                    out.push(argb(take(c, 3), take(c, 0), take(c, 1), take(c, 2)));
                }
            }
            png::ColorType::Grayscale => {
                for c in pixels.chunks_exact(step).take(count) {
                    let g = take(c, 0);
                    out.push(argb(0xFF, g, g, g));
                }
            }
            png::ColorType::GrayscaleAlpha => {
                for c in pixels.chunks_exact(2 * step).take(count) {
                    let g = take(c, 0);
                    out.push(argb(take(c, 1), g, g, g));
                }
            }
            png::ColorType::Indexed => {
                // `read_info` leaves palette expansion to a transformation we
                // don't request, so an indexed image reaching here is one the
                // crate could not flatten.
                return Err(OpenSlideError::Decode(
                    "PNG tile is palette-indexed after expansion".into(),
                ));
            }
        }

        if out.len() != count {
            return Err(OpenSlideError::Decode(format!(
                "PNG decoded {} pixels, expected {count}",
                out.len()
            )));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode `pixels` (RGBA8) as a PNG so the decoder can be exercised without
    /// a slide on disk.
    fn encode(width: u32, height: u32, color: png::ColorType, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, width, height);
            enc.set_color(color);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(data).unwrap();
        }
        out
    }

    #[test]
    fn decodes_rgb() {
        let data = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let png = encode(2, 2, png::ColorType::Rgb, &data);
        let got = PngDecoder.decode(&png, 2, 2).unwrap();
        assert_eq!(got, vec![0xFFFF0000, 0xFF00FF00, 0xFF0000FF, 0xFFFFFFFF]);
    }

    #[test]
    fn decodes_rgba_preserving_alpha() {
        let data = [255, 0, 0, 128, 0, 255, 0, 255];
        let png = encode(2, 1, png::ColorType::Rgba, &data);
        let got = PngDecoder.decode(&png, 2, 1).unwrap();
        assert_eq!(got, vec![0x80FF0000, 0xFF00FF00]);
    }

    #[test]
    fn decodes_grayscale() {
        let png = encode(2, 1, png::ColorType::Grayscale, &[0, 255]);
        let got = PngDecoder.decode(&png, 2, 1).unwrap();
        assert_eq!(got, vec![0xFF000000, 0xFFFFFFFF]);
    }

    #[test]
    fn rejects_size_mismatch() {
        let png = encode(2, 2, png::ColorType::Rgb, &[0; 12]);
        let err = PngDecoder
            .decode(&png, 4, 4)
            .expect_err("size must be checked");
        assert!(err.to_string().contains("size mismatch"), "{err}");
    }

    #[test]
    fn rejects_garbage() {
        assert!(PngDecoder.decode(b"not a png", 1, 1).is_err());
    }
}
