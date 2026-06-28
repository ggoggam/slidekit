//! Deflate (zlib) tile decoder for TIFF.
//!
//! Handles both the Adobe "Deflate" (compression == 8) and the deprecated
//! "old" Deflate (compression == 32946) schemes; both wrap a raw DEFLATE
//! stream in a zlib header/trailer. The predictor and ARGB packing are handled
//! by the shared helpers in [`super`].

use crate::decode::tiff::TiffLike;
use crate::error::{OpenSlideError, Result};

use super::{TileDecoder, decode_predicted, read_spp_predictor};

/// Decompress a single zlib-wrapped DEFLATE stream (one tile or one strip).
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    miniz_oxide::inflate::decompress_to_vec_zlib(data)
        .map_err(|e| OpenSlideError::Decode(format!("Deflate decompression failed: {e:?}")))
}

/// Decoder for Deflate-compressed TIFF tiles.
pub struct DeflateDecoder {
    samples_per_pixel: usize,
    predictor: u32,
}

impl DeflateDecoder {
    /// Create a Deflate decoder.
    pub fn new(samples_per_pixel: usize, predictor: u32) -> Self {
        Self {
            samples_per_pixel,
            predictor,
        }
    }

    /// Build a Deflate decoder from a TIFF directory's SamplesPerPixel and
    /// Predictor tags.
    pub fn for_dir(tl: &mut TiffLike, dir: i64) -> Result<Self> {
        let (samples_per_pixel, predictor) = read_spp_predictor(tl, dir)?;
        Ok(Self::new(samples_per_pixel, predictor))
    }
}

impl TileDecoder for DeflateDecoder {
    fn decode(&self, data: &[u8], width: u32, height: u32) -> Result<Vec<u32>> {
        let decompressed = decompress(data)?;
        decode_predicted(
            decompressed,
            width,
            height,
            self.samples_per_pixel,
            self.predictor,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::argb;

    fn zlib_compress(data: &[u8]) -> Vec<u8> {
        miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
    }

    #[test]
    fn round_trip_decompress() {
        let original: Vec<u8> = (0..=255u8).cycle().take(2048).collect();
        let compressed = zlib_compress(&original);
        assert_eq!(decompress(&compressed).unwrap(), original);
    }

    #[test]
    fn invalid_stream_errors() {
        assert!(decompress(&[0, 1, 2, 3]).is_err());
    }

    #[test]
    fn decoder_full_path_with_predictor() {
        // 2x1 RGB image, real pixels (10,20,30),(40,60,80).
        // Horizontal-difference encoding: (10,20,30),(30,40,50).
        let diffed = [10u8, 20, 30, 30, 40, 50];
        let compressed = zlib_compress(&diffed);
        let decoder = DeflateDecoder::new(3, 2);
        let px = decoder.decode(&compressed, 2, 1).unwrap();
        assert_eq!(px, vec![argb(0xFF, 10, 20, 30), argb(0xFF, 40, 60, 80)]);
    }
}
