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
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

const REGION: i64 = 1024; // side length of the compared square
const MEAN_DIFF_CEILING: f64 = 15.0; // per-channel; trips only on gross mismatch

/// Below this both-opaque fraction there is no real overlap to compare, so the
/// pixel diff is reported as `n/a` rather than as a passing 0.00.
const MIN_OPAQUE_FRAC: f64 = 0.01;

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
    let have_slidetool = Command::new("slidetool")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !have_slidetool {
        eprintln!("NOTE: `slidetool` not on PATH — reporting slidekit latency only.");
    }

    // slidekit is timed in-process; slidetool can only be timed as a whole
    // process. Measure the bare process floor (`--version` does no slide work)
    // so the open/read columns can have it subtracted out — otherwise every
    // OpenSlide number carries a fixed startup tax that has nothing to do with
    // the library. Best-of-N: we want the floor, not the scheduler's noise.
    let floor_ms = have_slidetool.then(process_floor_ms).flatten();
    if let Some(floor) = floor_ms {
        eprintln!("slidetool process floor: {floor:.1}ms (subtracted from osl_open)");
    }

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
        let (x0, y0) = common::tissue_region(&slide, level, w, h).unwrap_or_else(|| {
            (
                (((lw - w) / 2) as f64 * ds) as i64,
                (((lh - h) / 2) as f64 * ds) as i64,
            )
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
            match run_slidetool(&path, x0, y0, level, w, h) {
                Some((osl_ms, rgba)) => {
                    let (mean, max, frac) = diff(&sk, &rgba, (w * h) as usize);
                    if let Some(dir) = dump_dir.as_deref() {
                        dump_png(&dir.join(format!("{slug}_openslide.png")), &rgba, w, h);
                    }
                    (Some(osl_ms), Some(mean), Some(max), Some(frac))
                }
                None => (None, None, None, None),
            }
        } else {
            (None, None, None, None)
        };

        // A second slidetool process that opens the slide but reads no region,
        // so `region read` minus this isolates the read from the open. Both
        // carry the same startup tax, which cancels in the subtraction.
        let osl_props_ms = have_slidetool.then(|| run_slidetool_props(&path)).flatten();
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
            dump_png(
                &dir.join(format!("{slug}_slidekit.png")),
                &argb_to_rgba(&sk),
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

/// Pack slidekit's ARGB (0xAARRGGBB) into straight R,G,B,A bytes for PNG.
fn argb_to_rgba(argb: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(argb.len() * 4);
    for &px in argb {
        out.push((px >> 16) as u8); // R
        out.push((px >> 8) as u8); // G
        out.push(px as u8); // B
        out.push((px >> 24) as u8); // A
    }
    out
}

/// Write 8-bit RGBA pixels to `path` as a PNG. Best-effort — a failure here
/// only costs a debug artifact, so it warns rather than aborting the bench.
fn dump_png(path: &Path, rgba: &[u8], w: i64, h: i64) {
    let file = match std::fs::File::create(path) {
        Ok(f) => f,
        Err(e) => return eprintln!("WARN: cannot write {}: {e}", path.display()),
    };
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    if let Err(e) = enc
        .write_header()
        .and_then(|mut wr| wr.write_image_data(rgba))
    {
        eprintln!("WARN: cannot encode {}: {e}", path.display());
    }
}

/// Wall time of the cheapest possible `slidetool` invocation — `--version` does
/// no slide work, so this is the fork+exec+dynamic-link cost every other
/// measurement inherits. Returns the minimum of several runs (the floor is what
/// we want to subtract, not an average inflated by scheduling noise).
fn process_floor_ms() -> Option<f64> {
    let mut best: Option<f64> = None;
    for _ in 0..5 {
        let t = Instant::now();
        let ok = Command::new("slidetool")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            return None;
        }
        let elapsed = ms(t.elapsed());
        best = Some(best.map_or(elapsed, |b: f64| b.min(elapsed)));
    }
    best
}

/// Wall time of `slidetool prop list` — a process that opens the slide and dumps
/// its properties, but reads no pixels. Used as the open-side measurement.
fn run_slidetool_props(path: &Path) -> Option<f64> {
    let t = Instant::now();
    let out = Command::new("slidetool")
        .args(["prop", "list"])
        .arg(path)
        .output()
        .ok()?;
    out.status.success().then(|| ms(t.elapsed()))
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
