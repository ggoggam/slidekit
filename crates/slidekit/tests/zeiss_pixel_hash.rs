//! Pixel fingerprint of the CZI read path.
//!
//! `test_zeiss.rs` checks that a CZI opens with sensible levels and that a
//! region comes back non-blank. Neither would notice a read path that returned
//! *different* pixels — and `ZeissLevel::read_region` resolves subblock offsets
//! by hand from byte fields, which is exactly the code where an off-by-one puts
//! plausible-looking tissue in the wrong place.
//!
//! So this hashes the ARGB output over a fixed set of windows on every level and
//! compares against digests recorded from a known-good build. The fixtures are
//! SHA-256 pinned by `testdata.yaml`, so the digests are stable; if one changes,
//! either the decode changed or the read path moved bytes.
//!
//! A digest mismatch prints the new value. Update the constant **only** after
//! confirming the change was intended — that is the whole point of the guard.

mod common;

use sha2::{Digest, Sha256};
use slidekit::OpenSlide;

/// `(fixture, per-level digests from level 0 up)`.
///
/// Recorded 2026-08-06 against the pre-existing read path, immediately before
/// subblock location was deferred out of `open` — the refactor these digests
/// exist to police.
const EXPECTED: &[(&str, &[&str])] = &[
    (
        "Zeiss/Zeiss-5-Uncompressed.czi",
        &[
            "6f8f7eb14a7cbc1726429b2129cad462d2907fa6b745cd3660b6965bd9ea57af",
            "7e89e496498892b5b89f505736fc5c698674a0ae47011be75c824e93f62979c7",
            "102d98e32d00c4bcf9e7b3da20c2d2342a03ef7e3f87504c096b662620f37be2",
            "1b744d84586f01fb972f0f6dd5b9605862d382d61a8655a997062a0a9ced07b0",
            "37c2bf4b3dcff07e33c58ed13bf0cc25ac96cbd9037563841631ee5a31dd09b9",
        ],
    ),
    (
        "Zeiss/Zeiss-5-SlidePreview-Zstd0.czi",
        &["e36a9d01112f4939e25afb82fe59c674ac6a597b02c7e90678491ba4d81316c2"],
    ),
    (
        "Zeiss/Zeiss-5-SlidePreview-Zstd1-HiLo.czi",
        &["e36a9d01112f4939e25afb82fe59c674ac6a597b02c7e90678491ba4d81316c2"],
    ),
];

/// Side length of each probed window.
const WINDOW: i64 = 512;

/// Windows probed per level, as fractions of the level's extent. Spread along X
/// because these slides are wide and short, and including the origin and a
/// window clamped to the far edge so tile seams and clamping are covered as well
/// as tile interiors.
const SPOTS: &[(f64, f64)] = &[
    (0.0, 0.0),
    (0.10, 0.30),
    (0.20, 0.55),
    (0.30, 0.40),
    (0.40, 0.60),
    (0.50, 0.50),
    (0.60, 0.45),
    (0.70, 0.60),
    (0.80, 0.55),
    (0.85, 0.60),
    (0.90, 0.35),
    (0.95, 0.90),
];

#[test]
fn zeiss_pixels_are_unchanged() {
    let mut checked = 0usize;

    for (fixture, expected) in EXPECTED {
        let Some(path) = common::test_slide_path(fixture) else {
            eprintln!("SKIP: test slide {fixture} not available");
            continue;
        };
        let slide = OpenSlide::open(&path).expect("open CZI");
        assert_eq!(
            slide.level_count() as usize,
            expected.len(),
            "{fixture}: level count changed; the recorded digests no longer describe this slide"
        );

        for (level, want) in expected.iter().enumerate() {
            let level = level as i32;
            let (w, h) = slide.level_dimensions(level).expect("level dims");
            let mut hasher = Sha256::new();
            let mut with_content = 0usize;

            for (fx, fy) in SPOTS {
                let (rw, rh) = (w.min(WINDOW), h.min(WINDOW));
                let x = ((w as f64 * fx) as i64).min((w - rw).max(0));
                let y = ((h as f64 * fy) as i64).min((h - rh).max(0));
                let px = slide.read_region(x, y, level, rw, rh).expect("read region");
                if px.iter().any(|&p| p != 0) {
                    with_content += 1;
                }
                for p in &px {
                    hasher.update(p.to_le_bytes());
                }
            }

            // A digest over nothing but transparent gaps would match whatever
            // the read path did, so it is not evidence of anything.
            assert!(
                with_content > 0,
                "{fixture} L{level}: no probed window held content; \
                 the digest would not be testing the read path"
            );
            assert_eq!(
                format!("{:x}", hasher.finalize()),
                *want,
                "{fixture} L{level}: pixels changed"
            );
            checked += 1;
        }
    }

    if checked == 0 {
        eprintln!("SKIP: no CZI fixtures available");
    }
}
