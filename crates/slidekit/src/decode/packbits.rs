//! PackBits (TIFF RLE) tile decoder.
//!
//! PackBits is the simple byte-oriented run-length scheme defined by the TIFF
//! spec (compression == 32773). The predictor and ARGB packing are handled by
//! the shared helpers in [`super`].

use crate::decode::tiff::TiffLike;
use crate::error::{OpenSlideError, Result};

use super::{TileDecoder, decode_predicted, read_spp_predictor};

/// Decompress a single PackBits stream (one tile or one strip).
///
/// Control byte `n` (interpreted as `i8`):
/// - `0..=127`: copy the next `n + 1` bytes literally.
/// - `-127..=-1`: repeat the next byte `1 - n` times.
/// - `-128`: no-op (skipped).
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 2);
    let mut i = 0;
    while i < data.len() {
        let n = data[i] as i8;
        i += 1;
        if n >= 0 {
            let count = n as usize + 1;
            let end = i + count;
            if end > data.len() {
                return Err(OpenSlideError::Decode(
                    "PackBits literal run exceeds input".into(),
                ));
            }
            out.extend_from_slice(&data[i..end]);
            i = end;
        } else if n != -128 {
            let count = (1 - n as i32) as usize;
            if i >= data.len() {
                return Err(OpenSlideError::Decode(
                    "PackBits replicate run missing data byte".into(),
                ));
            }
            let byte = data[i];
            i += 1;
            out.resize(out.len() + count, byte);
        }
        // n == -128 is a no-op.
    }
    Ok(out)
}

/// Decoder for PackBits-compressed TIFF tiles.
pub struct PackBitsDecoder {
    samples_per_pixel: usize,
    predictor: u32,
}

impl PackBitsDecoder {
    /// Create a PackBits decoder.
    pub fn new(samples_per_pixel: usize, predictor: u32) -> Self {
        Self {
            samples_per_pixel,
            predictor,
        }
    }

    /// Build a PackBits decoder from a TIFF directory's SamplesPerPixel and
    /// Predictor tags.
    pub fn for_dir(tl: &mut TiffLike, dir: i64) -> Result<Self> {
        let (samples_per_pixel, predictor) = read_spp_predictor(tl, dir)?;
        Ok(Self::new(samples_per_pixel, predictor))
    }
}

impl TileDecoder for PackBitsDecoder {
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

    #[test]
    fn literal_run() {
        // n=2 -> copy 3 literal bytes
        assert_eq!(decompress(&[2, 10, 20, 30]).unwrap(), vec![10, 20, 30]);
    }

    #[test]
    fn replicate_run() {
        // n=-2 (0xFE) -> repeat next byte 3 times
        assert_eq!(decompress(&[0xFE, 0xAB]).unwrap(), vec![0xAB, 0xAB, 0xAB]);
    }

    #[test]
    fn noop_byte_skipped() {
        // 0x80 (-128) is a no-op
        assert_eq!(decompress(&[0x80, 0, 0xAB]).unwrap(), vec![0xAB]);
    }

    #[test]
    fn classic_spec_example() {
        // From the TIFF 6.0 PackBits example.
        let input = [
            0xFE, 0xAA, 0x02, 0x80, 0x00, 0x2A, 0xFD, 0xAA, 0x03, 0x80, 0x00, 0x2A, 0x22, 0xF7,
            0xAA,
        ];
        let expected = [
            0xAA, 0xAA, 0xAA, 0x80, 0x00, 0x2A, 0xAA, 0xAA, 0xAA, 0xAA, 0x80, 0x00, 0x2A, 0x22,
            0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA,
        ];
        assert_eq!(decompress(&input).unwrap(), expected);
    }

    #[test]
    fn truncated_literal_errors() {
        assert!(decompress(&[5, 1, 2]).is_err());
    }

    #[test]
    fn decoder_full_path() {
        // 2x1 RGB: literal run of 6 bytes -> (10,20,30),(40,60,80)
        let data = [5u8, 10, 20, 30, 40, 60, 80];
        let decoder = PackBitsDecoder::new(3, 1);
        let px = decoder.decode(&data, 2, 1).unwrap();
        assert_eq!(px, vec![argb(0xFF, 10, 20, 30), argb(0xFF, 40, 60, 80)]);
    }
}
