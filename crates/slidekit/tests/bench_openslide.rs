//! OpenSlide comparison bench — latency + tolerant pixel cross-check.
//!
//! This is NOT part of the correctness suite. It is `#[ignore]`d so it never
//! runs in the normal `cargo nextest run`. It assumes a **warm cache** (slides
//! already downloaded + verified) — timing a cold download or a SHA pass would
//! measure the network / hashing, not the decoder.
//!
//! Run it explicitly:
//!   cargo test --test bench_openslide -- --ignored --nocapture
//!   # or under nextest:
//!   cargo nextest run --test bench_openslide --run-ignored ignored-only --no-capture
//!
//! It reports, per slide:
//!   - slidekit `open()` and `read_region()` latency (measured separately),
//!   - OpenSlide `slidetool region read` wall time (one cold process =
//!     open + read + PNG encode; the CLI gives no finer breakdown),
//!   - a tolerant pixel diff of the SAME region from both readers.
//!
//! Pixel note: independent JPEG decoders differ by ±1–2 per channel, so exact
//! equality would flake on JPEG-tiled vendors (Aperio/Hamamatsu/Ventana/...).
//! We assert dimensions match and mean absolute RGB diff stays under a generous
//! ceiling — enough to catch a channel swap or gross decode bug, not decoder
//! noise. Regions are picked at slide center where tissue is opaque, so
//! slidekit's premultiplied ARGB and slidetool's straight RGBA coincide.

mod common;

use slidekit::OpenSlide;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

/// (relative slide path, level) — one representative real slide per vendor.
const CASES: &[(&str, i32)] = &[
    ("Aperio/CMU-1.svs", 0),
    ("Hamamatsu/CMU-1.ndpi", 0),
    ("Leica/Leica-1.scn", 0),
    ("Philips-TIFF/Philips-4.tiff", 0),
    ("Generic-TIFF/CMU-1.tiff", 0),
    // Mirax omitted: multi-file .mrxs needs zip extraction the simple path
    // resolver here doesn't do; its read path is covered by test_mirax.
    ("Zeiss/Zeiss-5-Uncompressed.czi", 0), // OpenSlide 4.0 can't read this CZI — slidekit-only row
    ("Ventana/OS-2.bif", 0),
];

const REGION: i64 = 1024; // side length of the compared square
const MEAN_DIFF_CEILING: f64 = 15.0; // per-channel; trips only on gross mismatch

struct Row {
    vendor: String,
    dims: String,
    sk_open_ms: f64,
    sk_read_ms: f64,
    osl_ms: Option<f64>,
    mean_diff: Option<f64>,
    max_diff: Option<u32>,
    /// Fraction of pixels opaque in both readers — the RGB diff is measured
    /// only over these (transparent background isn't meaningful to compare).
    opaque_frac: Option<f64>,
}

#[test]
#[ignore = "bench: assumes warm cache + optional openslide (slidetool) on PATH"]
fn bench_vs_openslide() {
    let have_slidetool = Command::new("slidetool")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !have_slidetool {
        eprintln!("NOTE: `slidetool` not on PATH — reporting slidekit latency only.");
    }

    let mut rows = Vec::new();
    for &(rel, level) in CASES {
        let Some(path) = common::test_slide_path(rel) else {
            eprintln!("SKIP: {rel} not cached");
            continue;
        };

        // ── slidekit: open and read timed separately ──
        let t = Instant::now();
        let slide = match OpenSlide::open(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("SKIP {rel}: open failed: {e}");
                continue;
            }
        };
        let sk_open_ms = ms(t.elapsed());

        let (lw, lh) = slide.level_dimensions(level).unwrap_or((0, 0));
        let w = REGION.min(lw);
        let h = REGION.min(lh);
        // Center of the level, in level-0 coordinates.
        let ds = slide.level_downsample(level).unwrap_or(1.0);
        let x0 = (((lw - w) / 2) as f64 * ds) as i64;
        let y0 = (((lh - h) / 2) as f64 * ds) as i64;

        let t = Instant::now();
        let sk = slide
            .read_region(x0, y0, level, w, h)
            .expect("slidekit read_region");
        let sk_read_ms = ms(t.elapsed());

        // ── openslide: one cold process, open+read+encode ──
        let (osl_ms, mean_diff, max_diff, opaque_frac) = if have_slidetool {
            match run_slidetool(&path, x0, y0, level, w, h) {
                Some((osl_ms, rgba)) => {
                    let (mean, max, frac) = diff(&sk, &rgba, (w * h) as usize);
                    (Some(osl_ms), Some(mean), Some(max), Some(frac))
                }
                None => (None, None, None, None),
            }
        } else {
            (None, None, None, None)
        };

        // Tolerant correctness check — only where there's real opaque overlap to
        // compare (a background-only region tells us nothing about the decoder).
        if let (Some(mean), Some(frac)) = (mean_diff, opaque_frac) {
            if frac > 0.5 && mean >= MEAN_DIFF_CEILING {
                eprintln!(
                    "WARN {rel}: mean per-channel diff {mean:.2} over opaque tissue >= {MEAN_DIFF_CEILING}"
                );
            }
        }

        rows.push(Row {
            vendor: rel.split('/').next().unwrap_or(rel).to_string(),
            dims: format!("{lw}x{lh}"),
            sk_open_ms,
            sk_read_ms,
            osl_ms,
            mean_diff,
            max_diff,
            opaque_frac,
        });
    }

    print_table(&rows);
    assert!(
        !rows.is_empty(),
        "no slides available — warm the cache first"
    );
}

fn run_slidetool(
    path: &Path,
    x: i64,
    y: i64,
    level: i32,
    w: i64,
    h: i64,
) -> Option<(f64, Vec<u8>)> {
    let out = std::env::temp_dir().join("slidekit_bench_osl.png");
    let t = Instant::now();
    let status = Command::new("slidetool")
        .args(["region", "read"])
        .arg(path)
        .args([
            x.to_string(),
            y.to_string(),
            level.to_string(),
            w.to_string(),
            h.to_string(),
        ])
        .arg(&out)
        .status()
        .ok()?;
    let osl_ms = ms(t.elapsed());
    if !status.success() {
        eprintln!("slidetool failed on {path:?}");
        return None;
    }
    let file = std::fs::File::open(&out).ok()?;
    let decoder = png::Decoder::new(file);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    // slidetool writes 8-bit RGBA; anything else we don't compare.
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        eprintln!(
            "unexpected PNG format from slidetool: {:?}/{:?}",
            info.color_type, info.bit_depth
        );
        return None;
    }
    Some((osl_ms, buf))
}

/// Mean per-channel RGB diff and max single-channel diff over pixels that are
/// opaque in BOTH readers, plus the fraction of pixels that qualified.
/// `sk` is packed ARGB (0xAARRGGBB); `rgba` is straight bytes R,G,B,A.
fn diff(sk: &[u32], rgba: &[u8], n: usize) -> (f64, u32, f64) {
    let n = n.min(sk.len()).min(rgba.len() / 4);
    let (mut sum, mut max, mut counted) = (0u64, 0u32, 0usize);
    for i in 0..n {
        let px = sk[i];
        let (sr, sg, sb, sa) = (
            (px >> 16) & 0xff,
            (px >> 8) & 0xff,
            px & 0xff,
            (px >> 24) & 0xff,
        );
        let (or, og, ob, oa) = (
            rgba[i * 4] as u32,
            rgba[i * 4 + 1] as u32,
            rgba[i * 4 + 2] as u32,
            rgba[i * 4 + 3] as u32,
        );
        if sa != 0xff || oa != 0xff {
            continue; // background / partial-cover pixel — not a decode comparison
        }
        for (a, b) in [(sr, or), (sg, og), (sb, ob)] {
            let d = a.abs_diff(b);
            sum += d as u64;
            max = max.max(d);
        }
        counted += 1;
    }
    let mean = if counted == 0 {
        0.0
    } else {
        sum as f64 / (counted * 3) as f64
    };
    let frac = if n == 0 {
        0.0
    } else {
        counted as f64 / n as f64
    };
    (mean, max, frac)
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn print_table(rows: &[Row]) {
    eprintln!();
    eprintln!(
        "{:<10} {:>15} {:>10} {:>10} {:>12} {:>10} {:>8} {:>10}",
        "vendor", "level0", "sk_open", "sk_read", "osl_proc", "mean_d", "max_d", "opaque%"
    );
    eprintln!("{}", "-".repeat(92));
    for r in rows {
        let osl = r
            .osl_ms
            .map(|v| format!("{v:.1}ms"))
            .unwrap_or_else(|| "-".into());
        let mean = r
            .mean_diff
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "-".into());
        let max = r
            .max_diff
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into());
        let frac = r
            .opaque_frac
            .map(|v| format!("{:.0}%", v * 100.0))
            .unwrap_or_else(|| "-".into());
        eprintln!(
            "{:<10} {:>15} {:>8.1}ms {:>8.1}ms {:>12} {:>10} {:>8} {:>10}",
            r.vendor, r.dims, r.sk_open_ms, r.sk_read_ms, osl, mean, max, frac
        );
    }
    eprintln!(
        "\n(osl_proc = one cold slidetool process: open+read+encode. sk_open/sk_read measured in-process.)"
    );
}
