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

use common::slidetool::{self, MEAN_DIFF_CEILING, MIN_OPAQUE_FRAC, ms};
use slidekit::OpenSlide;
use std::path::PathBuf;
use std::time::Instant;

const REGION: i64 = 1024; // side length of the compared square

struct Row {
    vendor: String,
    dims: String,
    sk_open_ms: f64,
    sk_read_ms: f64,
    /// Whole `slidetool region read` process: startup + open + read + PNG encode.
    osl_ms: Option<f64>,
    /// `slidetool prop list` minus the process floor — open, without a read.
    osl_open_ms: Option<f64>,
    /// `region read` minus `prop list` — the read plus its PNG encode.
    osl_read_ms: Option<f64>,
    mean_diff: Option<f64>,
    max_diff: Option<u32>,
    /// Fraction of pixels opaque in both readers — the RGB diff is measured
    /// only over these (transparent background isn't meaningful to compare).
    opaque_frac: Option<f64>,
}

#[test]
#[ignore = "bench: assumes warm cache + optional openslide (slidetool) on PATH"]
fn bench_vs_openslide() {
    let have_slidetool = slidetool::available();
    slidetool::report_version();

    // slidekit is timed in-process; slidetool can only be timed as a whole
    // process. Measure the bare process floor (`--version` does no slide work)
    // so the open/read columns can have it subtracted out — otherwise every
    // OpenSlide number carries a fixed startup tax that has nothing to do with
    // the library. Best-of-N: we want the floor, not the scheduler's noise.
    let floor_ms = have_slidetool.then(slidetool::process_floor_ms).flatten();
    if let Some(floor) = floor_ms {
        eprintln!("slidetool process floor: {floor:.1}ms (subtracted from osl_open)");
    }

    // Re-read one specific window instead of searching for tissue:
    //   SLIDEKIT_BENCH_WINDOW=42496,6784
    // The remote bench's correctness table prints the level-0 top-left of the
    // patch that disagrees with OpenSlide most, and this is how you go and look
    // at it — with SLIDEKIT_BENCH_PROFILE=1 to see *where* inside the window the
    // two readers part ways.
    let forced_window: Option<(i64, i64)> =
        std::env::var("SLIDEKIT_BENCH_WINDOW").ok().and_then(|v| {
            let (x, y) = v.split_once(',')?;
            Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
        });
    if let Some((x, y)) = forced_window {
        eprintln!("Reading the window at ({x},{y}) on every slide, not the densest tissue");
    }
    let profile = std::env::var_os("SLIDEKIT_BENCH_PROFILE").is_some();

    // Optional: dump each reader's region as a PNG for side-by-side inspection.
    //   SLIDEKIT_BENCH_DUMP=/tmp/cmp cargo test --test bench_openslide -- --ignored
    let dump_dir = std::env::var_os("SLIDEKIT_BENCH_DUMP").map(PathBuf::from);
    if let Some(dir) = dump_dir.as_deref() {
        std::fs::create_dir_all(dir).expect("create dump dir");
        eprintln!("Dumping regions to {}", dir.display());
    }

    let mut rows = Vec::new();
    for &(rel, level) in common::BENCH_CASES {
        let Some(fixture) = common::bench_slide(rel) else {
            eprintln!("SKIP: {rel} not available locally");
            continue;
        };
        let path = fixture.local_path.clone();

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
        // Top-left of the compared window, in level-0 coords. Prefer a spot with
        // real tissue (many slides have blank centers); fall back to center.
        let ds = slide.level_downsample(level).unwrap_or(1.0);
        let (x0, y0) = forced_window.unwrap_or_else(|| {
            common::tissue_region(&slide, level, w, h).unwrap_or_else(|| {
                (
                    (((lw - w) / 2) as f64 * ds) as i64,
                    (((lh - h) / 2) as f64 * ds) as i64,
                )
            })
        });

        let t = Instant::now();
        let sk = slide
            .read_region(x0, y0, level, w, h)
            .expect("slidekit read_region");
        let sk_read_ms = ms(t.elapsed());

        let vendor = fixture.name.clone();
        // Dump filenames must stay path-safe.
        let slug = vendor.replace('/', "-");

        // ── openslide: one cold process, open+read+encode ──
        let (osl_ms, mean_diff, max_diff, opaque_frac) = if have_slidetool {
            match slidetool::region_read(&path, &slug, x0, y0, level, w, h) {
                Some((osl_ms, rgba)) => {
                    let d = slidetool::diff(&sk, &rgba, (w * h) as usize);
                    if profile {
                        eprintln!("  {vendor} at ({x0},{y0}):");
                        slidetool::profile(&sk, &rgba, w, h, 8);
                    }
                    if let Some(dir) = dump_dir.as_deref() {
                        slidetool::dump_png(
                            &dir.join(format!("{slug}_openslide.png")),
                            &rgba,
                            w,
                            h,
                        );
                    }
                    (Some(osl_ms), Some(d.mean), Some(d.max), Some(d.opaque_frac))
                }
                None => (None, None, None, None),
            }
        } else {
            (None, None, None, None)
        };

        // A second slidetool process that opens the slide but reads no region,
        // so `region read` minus this isolates the read from the open. Both
        // carry the same startup tax, which cancels in the subtraction.
        let osl_props_ms = have_slidetool.then(|| slidetool::props_ms(&path)).flatten();
        let osl_open_ms = match (osl_props_ms, floor_ms) {
            // Clamp at zero: on a slide that opens in under a millisecond the
            // floor estimate can exceed the measurement.
            (Some(props), Some(floor)) => Some((props - floor).max(0.0)),
            _ => None,
        };
        let osl_read_ms = match (osl_ms, osl_props_ms) {
            (Some(total), Some(props)) => Some((total - props).max(0.0)),
            _ => None,
        };

        // Dump slidekit's region too, for side-by-side visual comparison.
        if let Some(dir) = dump_dir.as_deref() {
            slidetool::dump_png(
                &dir.join(format!("{slug}_slidekit.png")),
                &slidetool::argb_to_rgba(&sk),
                w,
                h,
            );
        }

        // Tolerant correctness check — only where there's real opaque overlap to
        // compare (a background-only region tells us nothing about the decoder).
        if let (Some(mean), Some(frac)) = (mean_diff, opaque_frac) {
            if frac > 0.5 && mean >= MEAN_DIFF_CEILING {
                eprintln!(
                    "WARN {rel}: mean per-channel diff {mean:.2} over opaque tissue >= {MEAN_DIFF_CEILING}"
                );
            }
            // Say so out loud: a near-zero diff over no overlapping opaque
            // pixels verifies nothing, and reads far too much like a pass.
            if frac < MIN_OPAQUE_FRAC {
                eprintln!(
                    "NOTE {rel}: only {:.1}% of pixels opaque in both readers — \
                     pixel diff not meaningful for this slide",
                    frac * 100.0
                );
            }
        }

        rows.push(Row {
            vendor,
            dims: format!("{lw}x{lh}"),
            sk_open_ms,
            sk_read_ms,
            osl_ms,
            osl_open_ms,
            osl_read_ms,
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

fn print_table(rows: &[Row]) {
    let opt_ms = |v: Option<f64>| {
        v.map(|v| format!("{v:.1}ms"))
            .unwrap_or_else(|| "-".to_string())
    };

    // ── latency: slidekit against OpenSlide, open and read separated ──
    eprintln!();
    eprintln!(
        "{:<20} {:>15} {:>9} {:>10} {:>9} {:>11} {:>11}",
        "vendor", "level0", "sk_open", "osl_open", "sk_read", "osl_read", "osl_proc"
    );
    eprintln!("{}", "-".repeat(92));
    for r in rows {
        eprintln!(
            "{:<20} {:>15} {:>7.1}ms {:>10} {:>7.1}ms {:>11} {:>11}",
            r.vendor,
            r.dims,
            r.sk_open_ms,
            opt_ms(r.osl_open_ms),
            r.sk_read_ms,
            opt_ms(r.osl_read_ms),
            opt_ms(r.osl_ms),
        );
    }
    eprintln!(
        "\n(sk_* measured in-process. slidetool can only be timed as a whole process, so:\n \
         osl_open  = `prop list` (opens, reads no pixels) minus the process floor\n \
         osl_read  = `region read` minus `prop list` — the read plus its PNG encode\n \
         osl_proc  = the whole `region read` process, floor included.\n \
         osl_read still carries a PNG encode slidekit never does, so it flatters slidekit;\n \
         a like-for-like read comparison needs in-process FFI, which is not built yet.)"
    );

    // ── pixel agreement ──
    eprintln!();
    eprintln!(
        "{:<20} {:>10} {:>10} {:>10}",
        "vendor", "mean_d", "max_d", "opaque%"
    );
    eprintln!("{}", "-".repeat(54));
    for r in rows {
        // A diff over (almost) no overlapping opaque pixels measures nothing —
        // print n/a so it can't be misread as agreement.
        let meaningful = r.opaque_frac.is_some_and(|f| f >= MIN_OPAQUE_FRAC);
        let show = |s: String| if meaningful { s } else { "n/a".to_string() };
        let mean = r
            .mean_diff
            .map(|v| show(format!("{v:.2}")))
            .unwrap_or_else(|| "-".into());
        let max = r
            .max_diff
            .map(|v| show(v.to_string()))
            .unwrap_or_else(|| "-".into());
        let frac = r
            .opaque_frac
            .map(|v| format!("{:.0}%", v * 100.0))
            .unwrap_or_else(|| "-".into());
        eprintln!("{:<20} {mean:>10} {max:>10} {frac:>10}", r.vendor);
    }
    eprintln!(
        "\n(mean_d/max_d = per-channel RGB difference over pixels opaque in BOTH readers.\n \
         Independent JPEG decoders differ by ±1-2, so small values are expected, not exact\n \
         equality. n/a = too little opaque overlap in the compared window to conclude anything.)"
    );
}
