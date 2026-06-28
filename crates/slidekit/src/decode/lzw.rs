//! LZW tile decoder for TIFF.
//!
//! Decompresses TIFF LZW-compressed tile/strip data; the predictor and ARGB
//! packing are handled by the shared helpers in [`super`].
//!
//! TIFF LZW uses MSB-first bit order with a minimum code size of 8 and the
//! "early change" code-size switch (one code earlier than vanilla LZW), which
//! `weezl` exposes via `with_tiff_size_switch`.

use crate::decode::tiff::TiffLike;
use crate::error::{OpenSlideError, Result};

use super::{TileDecoder, decode_predicted, read_spp_predictor};

/// Decompress a single TIFF LZW stream (one tile or one strip).
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8);
    decoder
        .decode(data)
        .map_err(|e| OpenSlideError::Decode(format!("LZW decompression failed: {e}")))
}

/// Decoder for LZW-compressed TIFF tiles.
pub struct LzwDecoder {
    samples_per_pixel: usize,
    predictor: u32,
}

impl LzwDecoder {
    /// Create an LZW decoder.
    ///
    /// `predictor` is the TIFF Predictor tag value (1 = none, 2 = horizontal
    /// differencing).
    pub fn new(samples_per_pixel: usize, predictor: u32) -> Self {
        Self {
            samples_per_pixel,
            predictor,
        }
    }

    /// Build an LZW decoder from a TIFF directory's SamplesPerPixel and
    /// Predictor tags.
    pub fn for_dir(tl: &mut TiffLike, dir: i64) -> Result<Self> {
        let (samples_per_pixel, predictor) = read_spp_predictor(tl, dir)?;
        Ok(Self::new(samples_per_pixel, predictor))
    }
}

impl TileDecoder for LzwDecoder {
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
    use super::super::{apply_horizontal_predictor, samples_to_argb};
    use super::*;
    use crate::color::argb;

    /// TIFF-style LZW encode for round-trip testing.
    fn lzw_encode(data: &[u8]) -> Vec<u8> {
        let mut encoder = weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8);
        encoder.encode(data).expect("encode")
    }

    #[test]
    fn round_trip_decompress() {
        let original: Vec<u8> = (0..=255u8).cycle().take(1024).collect();
        let compressed = lzw_encode(&original);
        assert_eq!(decompress(&compressed).unwrap(), original);
    }

    #[test]
    fn horizontal_predictor_rgb() {
        // Two rows, 2 px wide, RGB. Stored as horizontal differences.
        // Row 0 pixels: (10,20,30), (1,2,3) -> reconstructs to (10,20,30),(11,22,33)
        let mut buf = vec![10, 20, 30, 1, 2, 3, /* row1 */ 5, 5, 5, 2, 2, 2];
        apply_horizontal_predictor(&mut buf, 6, 3);
        assert_eq!(buf, vec![10, 20, 30, 11, 22, 33, 5, 5, 5, 7, 7, 7]);
    }

    #[test]
    fn samples_to_argb_rgb_and_rgba() {
        let rgb = [255, 0, 0, 0, 255, 0];
        let px = samples_to_argb(&rgb, 2, 1, 3).unwrap();
        assert_eq!(px, vec![argb(0xFF, 255, 0, 0), argb(0xFF, 0, 255, 0)]);

        let rgba = [255, 0, 0, 128, 0, 255, 0, 64];
        let px = samples_to_argb(&rgba, 2, 1, 4).unwrap();
        assert_eq!(px, vec![argb(128, 255, 0, 0), argb(64, 0, 255, 0)]);
    }

    #[test]
    fn samples_to_argb_size_mismatch_errors() {
        assert!(samples_to_argb(&[1, 2, 3], 2, 2, 3).is_err());
    }

    #[test]
    fn decoder_full_path_with_predictor() {
        // 2x1 RGB image, real pixels (10,20,30),(40,60,80).
        // Horizontal-difference encoding: (10,20,30),(30,40,50).
        let diffed = [10u8, 20, 30, 30, 40, 50];
        let compressed = lzw_encode(&diffed);
        let decoder = LzwDecoder::new(3, 2);
        let px = decoder.decode(&compressed, 2, 1).unwrap();
        assert_eq!(px, vec![argb(0xFF, 10, 20, 30), argb(0xFF, 40, 60, 80)]);
    }
}
