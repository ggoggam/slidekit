//! Pure-Rust backend — `hayro-jpeg2000`, for builds that cannot link C.
//!
//! **Not bit-exact with OpenSlide on aggressively rate-controlled slides**, and
//! measurably less accurate than OpenJPEG there, not merely different — see the
//! module docs in [`super`] for the measurement. Prefer the `openjpeg` feature
//! wherever a C compiler is available.

use hayro_jpeg2000::{DecodeSettings, DecoderContext, Image};

use super::Component;
use crate::error::{OpenSlideError, Result};

/// Decode a codestream into its components, each scaled to 0..=255.
pub(super) fn decode(data: &[u8]) -> Result<(u32, u32, Vec<Component>)> {
    let settings = DecodeSettings::default();
    let image = Image::new(data, &settings)
        .map_err(|e| OpenSlideError::Decode(format!("JPEG 2000 parse error: {e:?}")))?;
    let (width, height) = (image.width(), image.height());

    // The subsampling factors are read from the codestream rather than assumed:
    // `hayro-jpeg2000` replicates a subsampled component up to the full image
    // size before handing it back, so they are normally 1, but the crate does
    // not document that as a guarantee and the failure mode if it changes is a
    // plausible-looking wrong image rather than an error.
    let subsampling = codestream_subsampling(data);

    let mut ctx = DecoderContext::default();
    let decoded = image
        .decode(&mut ctx)
        .map_err(|e| OpenSlideError::Decode(format!("JPEG 2000 decode error: {e:?}")))?;

    let (w, h) = (width as usize, height as usize);
    let comps = decoded
        .components()
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let samples = c.samples();
            // Trust the sample count first, the declared factors only as a
            // fallback for a component that did come back subsampled.
            let (cw, ch) = if samples.len() >= w * h {
                (w, h)
            } else {
                let (sx, sy) = subsampling
                    .as_ref()
                    .and_then(|s| s.get(i).copied())
                    .unwrap_or((1, 1));
                (
                    w.div_ceil(sx.max(1) as usize),
                    h.div_ceil(sy.max(1) as usize),
                )
            };

            let bit_depth = c.bit_depth();
            let max = ((1u32 << bit_depth.clamp(1, 16)) - 1) as f32;
            let scale = if bit_depth == 8 { 1.0 } else { 255.0 / max };

            Component {
                samples: samples
                    .iter()
                    .map(|&v| (v * scale).round().clamp(0.0, 255.0) as u8)
                    .collect(),
                width: cw,
                height: ch,
            }
        })
        .collect();

    Ok((width, height, comps))
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
}
