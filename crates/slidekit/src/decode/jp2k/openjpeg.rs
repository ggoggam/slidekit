//! OpenJPEG backend — the default, and the one that is bit-exact with OpenSlide.
//!
//! `openjpeg-sys` vendors the OpenJPEG C sources and builds them with `cc`, so
//! this needs a C compiler but no cmake, no pkg-config and no system library.
//! `jpeg2k` is the safe wrapper over it; the raw FFI is deliberately not
//! hand-rolled here, since the decoder's teardown path is exactly where the
//! previously-evaluated transpiled crate went wrong.
//!
//! OpenJPEG applies the codestream's own multi-component transform, so a tile
//! flagged `mct=1` (Aperio 33005) arrives as R,G,B and needs no further work.

use jpeg2k::Image;

use super::Component;
use crate::error::{OpenSlideError, Result};

/// Decode a codestream into its components, each scaled to 0..=255.
///
/// Handles a bare J2K codestream and a JP2-boxed image alike — `jpeg2k` picks
/// the codec from the magic bytes.
pub(super) fn decode(data: &[u8]) -> Result<(u32, u32, Vec<Component>)> {
    let image = Image::from_bytes(data)
        .map_err(|e| OpenSlideError::Decode(format!("JPEG 2000 decode error: {e}")))?;

    let comps = image
        .components()
        .iter()
        .map(|c| {
            let width = c.width() as usize;
            let height = c.height() as usize;
            let prec = c.precision().clamp(1, 16);
            // OpenJPEG hands back signed values for a signed component; shift
            // them into the unsigned range the rest of the pipeline assumes.
            let offset = if c.is_signed() { 1i32 << (prec - 1) } else { 0 };
            let max = ((1u32 << prec) - 1) as i32;

            let samples = c
                .data()
                .iter()
                .map(|&v| {
                    let v = (v + offset).clamp(0, max);
                    // 8-bit is the overwhelmingly common case and must not pay
                    // a rescale, which would round some values off by one.
                    if prec == 8 {
                        v as u8
                    } else {
                        ((v as f32) * 255.0 / max as f32).round().clamp(0.0, 255.0) as u8
                    }
                })
                .collect();

            Component {
                samples,
                width,
                height,
            }
        })
        .collect();

    Ok((image.orig_width(), image.orig_height(), comps))
}
