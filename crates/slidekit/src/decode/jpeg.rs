//! JPEG tile decoder.
//!
//! Decodes JPEG-compressed tile data into ARGB pixels. Handles both
//! standalone JPEG tiles and TIFF-style tiles that need JPEG tables
//! prepended (from the JPEGTables tag).

use crate::color::argb;
use crate::error::{OpenSlideError, Result};

use super::TileDecoder;

/// JPEG decoder using the `jpeg-decoder` crate.
pub struct JpegDecoder {
    /// Optional JPEG tables (from TIFF JPEGTables tag) to prepend to tile data.
    jpeg_tables: Option<Vec<u8>>,
    /// Treat the 3 components as R,G,B (interleaved) rather than YCbCr.
    /// Set for JPEG-in-TIFF tiles whose TIFF PhotometricInterpretation is RGB:
    /// their components are already R,G,B, but the stream carries no Adobe/JFIF
    /// marker, so jpeg-decoder would otherwise default to YCbCr and shift color
    /// (e.g. Aperio "JPEG/RGB" tiles turning neutral gray into magenta).
    assume_rgb: bool,
}

impl Default for JpegDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl JpegDecoder {
    /// Create a decoder without JPEG tables (standalone JPEG tiles).
    pub fn new() -> Self {
        Self {
            jpeg_tables: None,
            assume_rgb: false,
        }
    }

    /// Create a decoder with JPEG tables from the TIFF JPEGTables tag.
    ///
    /// TIFF JPEG tiles typically lack their own Huffman/quantization tables and
    /// rely on the tables in the JPEGTables tag. We prepend these tables by
    /// combining the tables header (up to the EOI marker) with the tile data
    /// (starting after the SOI marker).
    pub fn with_tables(tables: Vec<u8>) -> Self {
        Self {
            jpeg_tables: Some(tables),
            assume_rgb: false,
        }
    }

    /// Treat the decoded 3-component data as already-RGB (no YCbCr transform).
    /// Use when the TIFF PhotometricInterpretation is RGB. See the field docs.
    pub fn assume_rgb(mut self) -> Self {
        self.assume_rgb = true;
        self
    }

    /// Build complete JPEG data by prepending tables if needed.
    ///
    /// Public for testing. In production code, called internally by `decode()`.
    pub fn prepare_data<'a>(&'a self, data: &'a [u8]) -> Result<std::borrow::Cow<'a, [u8]>> {
        match &self.jpeg_tables {
            None => Ok(std::borrow::Cow::Borrowed(data)),
            Some(tables) => {
                // JPEGTables: everything before the trailing EOI (FF D9)
                // Tile data: everything after the leading SOI (FF D8)
                let tables_end = find_last_marker(tables, 0xFFD9)
                    .ok_or_else(|| OpenSlideError::Decode("JPEGTables missing EOI".into()))?;
                let tile_start = if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xD8 {
                    2 // Skip SOI
                } else {
                    0
                };

                let mut combined = Vec::with_capacity(tables_end + (data.len() - tile_start));
                combined.extend_from_slice(&tables[..tables_end]);
                combined.extend_from_slice(&data[tile_start..]);
                Ok(std::borrow::Cow::Owned(combined))
            }
        }
    }
}

impl TileDecoder for JpegDecoder {
    fn decode(&self, data: &[u8], width: u32, height: u32) -> Result<Vec<u32>> {
        let jpeg_data = self.prepare_data(data)?;

        let mut decoder = jpeg_decoder::Decoder::new(jpeg_data.as_ref());
        if self.assume_rgb {
            // Force interleaved-RGB output. NB: `ColorTransform::None` is *not*
            // right here — it emits planar output that the RGB24 packer below
            // would read as scrambled triples. `RGB` gives interleaved R,G,B.
            decoder.set_color_transform(jpeg_decoder::ColorTransform::RGB);
        }
        let pixels = decoder
            .decode()
            .map_err(|e| OpenSlideError::Decode(format!("JPEG decode error: {e}")))?;

        let info = decoder
            .info()
            .ok_or_else(|| OpenSlideError::Decode("No JPEG image info".into()))?;

        if info.width as u32 != width || info.height as u32 != height {
            return Err(OpenSlideError::Decode(format!(
                "JPEG size mismatch: expected {}x{}, got {}x{}",
                width, height, info.width, info.height
            )));
        }

        let pixel_count = (width * height) as usize;
        let mut argb_buf = Vec::with_capacity(pixel_count);

        match info.pixel_format {
            jpeg_decoder::PixelFormat::RGB24 => {
                for chunk in pixels.as_chunks::<3>().0.iter() {
                    argb_buf.push(argb(0xFF, chunk[0], chunk[1], chunk[2]));
                }
            }
            jpeg_decoder::PixelFormat::L8 => {
                for &gray in &pixels {
                    argb_buf.push(argb(0xFF, gray, gray, gray));
                }
            }
            jpeg_decoder::PixelFormat::L16 => {
                for chunk in pixels.as_chunks::<2>().0.iter() {
                    let gray = chunk[0]; // Use high byte
                    argb_buf.push(argb(0xFF, gray, gray, gray));
                }
            }
            jpeg_decoder::PixelFormat::CMYK32 => {
                // Simple CMYK→RGB: R = 255 - C, G = 255 - M, B = 255 - Y, scaled by K
                for chunk in pixels.as_chunks::<4>().0.iter() {
                    let c = chunk[0] as f32 / 255.0;
                    let m = chunk[1] as f32 / 255.0;
                    let y = chunk[2] as f32 / 255.0;
                    let k = chunk[3] as f32 / 255.0;
                    let r = (255.0 * (1.0 - c) * (1.0 - k)) as u8;
                    let g = (255.0 * (1.0 - m) * (1.0 - k)) as u8;
                    let b = (255.0 * (1.0 - y) * (1.0 - k)) as u8;
                    argb_buf.push(argb(0xFF, r, g, b));
                }
            }
        }

        Ok(argb_buf)
    }
}

/// Find the *last* occurrence of a 2-byte marker. Returns its offset.
///
/// The trailing EOI is the one that ends a JPEGTables blob, and marker-segment
/// payloads are not byte-stuffed: a quantization table holding the adjacent
/// values 255 and 217, or a Huffman table whose counts do, contains `FF D9`
/// too. Taking the first match cuts the tables off mid-payload and every tile
/// in the directory then fails to decode.
fn find_last_marker(data: &[u8], marker: u16) -> Option<usize> {
    let hi = (marker >> 8) as u8;
    let lo = (marker & 0xFF) as u8;
    data.windows(2).rposition(|w| w[0] == hi && w[1] == lo)
}
