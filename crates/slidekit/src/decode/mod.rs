pub mod deflate;
pub mod dicom;
pub mod jp2k;
pub mod jpeg;
pub mod jxl;
pub mod lzw;
pub mod packbits;
pub mod png;
pub mod tiff;
pub mod xml;

use crate::color::argb;
use crate::error::{OpenSlideError, Result};
use tiff::{TiffLike, tags};

/// Trait for tile/image decoders (JPEG, JPEG 2000, PNG, BMP, etc.).
pub trait TileDecoder: Send + Sync {
    /// Decode compressed tile data into ARGB pixels.
    fn decode(&self, data: &[u8], width: u32, height: u32) -> crate::error::Result<Vec<u32>>;
}

/// Apply the TIFF horizontal-differencing predictor (predictor == 2) in place.
///
/// Each scanline is `row_bytes` long; within a row every sample is the running
/// sum of preceding samples of the same color component.
pub fn apply_horizontal_predictor(buf: &mut [u8], row_bytes: usize, samples_per_pixel: usize) {
    if row_bytes == 0 {
        return;
    }
    for row_start in (0..buf.len()).step_by(row_bytes) {
        let row_end = (row_start + row_bytes).min(buf.len());
        for j in (row_start + samples_per_pixel)..row_end {
            buf[j] = buf[j].wrapping_add(buf[j - samples_per_pixel]);
        }
    }
}

/// Convert raw interleaved samples into ARGB pixels.
///
/// `samples_per_pixel` of 4 is treated as RGBA, 3 as RGB (opaque). Any other
/// value (or a buffer that is too small) is an error.
pub fn samples_to_argb(
    data: &[u8],
    width: u32,
    height: u32,
    samples_per_pixel: usize,
) -> Result<Vec<u32>> {
    let pixel_count = (width as usize) * (height as usize);
    match samples_per_pixel {
        4 if data.len() >= pixel_count * 4 => Ok(data
            .as_chunks::<4>()
            .0
            .iter()
            .take(pixel_count)
            .map(|c| argb(c[3], c[0], c[1], c[2]))
            .collect()),
        3 if data.len() >= pixel_count * 3 => Ok(data
            .as_chunks::<3>()
            .0
            .iter()
            .take(pixel_count)
            .map(|c| argb(0xFF, c[0], c[1], c[2]))
            .collect()),
        _ => Err(OpenSlideError::Decode(format!(
            "decompressed size {} doesn't match {}x{} pixels (spp={})",
            data.len(),
            width,
            height,
            samples_per_pixel
        ))),
    }
}

/// Finish decoding a lossless TIFF tile: apply the predictor (if any) then pack
/// the interleaved samples into ARGB. Shared by the LZW/Deflate/PackBits paths,
/// which differ only in their decompression step.
pub fn decode_predicted(
    mut decompressed: Vec<u8>,
    width: u32,
    height: u32,
    samples_per_pixel: usize,
    predictor: u32,
) -> Result<Vec<u32>> {
    if predictor == 2 {
        let row_bytes = width as usize * samples_per_pixel;
        apply_horizontal_predictor(&mut decompressed, row_bytes, samples_per_pixel);
    }
    samples_to_argb(&decompressed, width, height, samples_per_pixel)
}

/// Reject a directory whose samples are not 8 bits wide.
///
/// Everything downstream of here — the horizontal predictor's row stride,
/// `samples_to_argb`'s channel packing — assumes one byte per sample. A 16-bit
/// tile does not fail those checks, it passes them at the wrong stride: the
/// predictor sums bytes across half a scanline and the packer reads the first
/// `pixel_count * spp` bytes as 8-bit RGB, so the tile renders as noise and the
/// read reports success. Refusing the file says so instead.
pub fn check_bits_per_sample(tl: &mut TiffLike, dir: i64) -> Result<()> {
    if !tl.has_tag(dir, tags::BITS_PER_SAMPLE) {
        return Ok(()); // TIFF's default is 1, but in practice absent means 8.
    }
    let bits = tl.get_uint(dir, tags::BITS_PER_SAMPLE)?;
    if bits != 8 {
        return Err(OpenSlideError::Decode(format!(
            "Unsupported BitsPerSample {bits} in directory {dir}; only 8-bit samples decode"
        )));
    }
    Ok(())
}

/// Read a TIFF directory's SamplesPerPixel and Predictor tags, defaulting to
/// 1 sample and no predictor when absent.
pub fn read_spp_predictor(tl: &mut TiffLike, dir: i64) -> Result<(usize, u32)> {
    check_bits_per_sample(tl, dir)?;
    let samples_per_pixel = if tl.has_tag(dir, tags::SAMPLES_PER_PIXEL) {
        tl.get_uint(dir, tags::SAMPLES_PER_PIXEL)? as usize
    } else {
        1
    };
    let predictor = if tl.has_tag(dir, tags::PREDICTOR) {
        tl.get_uint(dir, tags::PREDICTOR)? as u32
    } else {
        1
    };
    Ok((samples_per_pixel, predictor))
}
