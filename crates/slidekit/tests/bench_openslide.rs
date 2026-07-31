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

/// (case path, level) — one representative real slide per vendor.
///
/// Most are manifest-relative paths resolved straight from the warm cache. The
/// ZIP-packaged vendors (Trestle, Mirax, DICOM) use a bare `Vendor/Name` key
/// that [`resolve`] maps to the file their test suites extract next to the ZIP;
/// the JP2K Aperio variants are omitted (slidekit has no JPEG-2000 decoder yet).
/// Both DICOM slides are listed because they exercise different frame layouts
/// (sparse vs. TILED_FULL), which is exactly the kind of tile-placement bug a
/// pixel cross-check catches.
const CASES: &[(&str, i32)] = &[
    ("Aperio/CMU-1.svs", 0),
    ("Hamamatsu/CMU-1.ndpi", 0),
    ("Leica/Leica-1.scn", 0),
    ("Philips-TIFF/Philips-4.tiff", 0),
    ("Generic-TIFF/CMU-1.tiff", 0),
    ("Trestle/CMU-1", 0),
    ("Mirax/CMU-1", 0),
    ("Zeiss/Zeiss-5-Uncompressed.czi", 0),
    ("Ventana/OS-2.bif", 0),
    ("DICOM/Leica-4", 0),
    ("DICOM/Hamamatsu-2", 0),
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

    // Optional: dump each reader's region as a PNG for side-by-side inspection.
    //   SLIDEKIT_BENCH_DUMP=/tmp/cmp cargo test --test bench_openslide -- --ignored
    let dump_dir = std::env::var_os("SLIDEKIT_BENCH_DUMP").map(PathBuf::from);
    if let Some(dir) = dump_dir.as_deref() {
        std::fs::create_dir_all(dir).expect("create dump dir");
        eprintln!("Dumping regions to {}", dir.display());
    }

    let mut rows = Vec::new();
    for &(rel, level) in CASES {
        let Some(path) = resolve(rel) else {
            eprintln!("SKIP: {rel} not cached (ZIP vendors need their test run once to extract)");
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
        // Top-left of the compared window, in level-0 coords. Prefer a spot with
        // real tissue (many slides have blank centers); fall back to center.
        let ds = slide.level_downsample(level).unwrap_or(1.0);
        let (x0, y0) = tissue_region(&slide, level, w, h).unwrap_or_else(|| {
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

        // One row per case. DICOM contributes two, so those keep the slide
        // name; every other vendor has a single case and needs only its name.
        let vendor = match rel.split('/').next().unwrap_or(rel) {
            "DICOM" => rel.to_string(),
            other => other.to_string(),
        };
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
        }

        rows.push(Row {
            vendor,
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

/// Find the WxH window (at `level`, returned as level-0 top-left coords) that
/// covers the most tissue, by scanning the coarsest pyramid level for non-white
/// opaque pixels. Returns None if the overview can't be read, so the caller
/// falls back to slide center.
///
/// ponytail: builds a summed-area table over the overview so the best-window
/// search is O(pixels) instead of O(pixels * window). The coarsest level is a
/// small thumbnail on real pyramids, so memory/time are trivial.
fn tissue_region(slide: &OpenSlide, level: i32, w: i64, h: i64) -> Option<(i64, i64)> {
    let ov = slide.level_count() - 1;
    let (ow, oh) = slide.level_dimensions(ov)?;
    if ow <= 0 || oh <= 0 {
        return None;
    }
    let ov_ds = slide.level_downsample(ov)?;
    let px = slide.read_region(0, 0, ov, ow, oh).ok()?;

    // Tissue mask: opaque and not near-white (dark OR saturated → stain).
    let (ow, oh) = (ow as usize, oh as usize);
    let mut sat = vec![0u32; (ow + 1) * (oh + 1)]; // summed-area table, 1px pad
    let stride = ow + 1;
    for y in 0..oh {
        for x in 0..ow {
            let p = px[y * ow + x];
            let (r, g, b, a) = ((p >> 16) & 0xff, (p >> 8) & 0xff, p & 0xff, p >> 24);
            let bright = r.max(g).max(b);
            let saturation = bright - r.min(g).min(b);
            let tissue = (a & 0xff) == 0xff && (bright < 210 || saturation > 25);
            let v = tissue as u32;
            sat[(y + 1) * stride + (x + 1)] =
                v + sat[y * stride + (x + 1)] + sat[(y + 1) * stride + x] - sat[y * stride + x];
        }
    }

    // Window size at overview scale (level → level-0 → overview px).
    let target_ds = slide.level_downsample(level)?;
    let win_w = ((w as f64 * target_ds / ov_ds).round() as usize).clamp(1, ow);
    let win_h = ((h as f64 * target_ds / ov_ds).round() as usize).clamp(1, oh);

    let sum = |x: usize, y: usize, bw: usize, bh: usize| {
        sat[(y + bh) * stride + x + bw] + sat[y * stride + x]
            - sat[y * stride + x + bw]
            - sat[(y + bh) * stride + x]
    };
    let (mut best, mut best_xy) = (0, (0usize, 0usize));
    for y in 0..=(oh - win_h) {
        for x in 0..=(ow - win_w) {
            let s = sum(x, y, win_w, win_h);
            if s > best {
                best = s;
                best_xy = (x, y);
            }
        }
    }
    if best == 0 {
        return None; // no tissue found — let caller use center
    }
    // Overview top-left → level-0 coords (what read_region expects).
    Some((
        (best_xy.0 as f64 * ov_ds) as i64,
        (best_xy.1 as f64 * ov_ds) as i64,
    ))
}

/// Resolve a case to an on-disk slide. Plain single-file slides come straight
/// from the manifest cache; the two ZIP-packaged vendors are resolved to the
/// file their test suites extract next to the cached ZIP. The bench assumes a
/// warm cache, so this only *reads* the extraction — it never creates it (run
/// `test_trestle` / `test_mirax` once to populate `extracted/`).
fn resolve(rel: &str) -> Option<PathBuf> {
    match rel {
        "Trestle/CMU-1" => extracted("Trestle/CMU-1.zip", "CMU-1.tif"),
        "Mirax/CMU-1" => extracted("Mirax/CMU-1.zip", "CMU-1.mrxs"),
        // A DICOM slide is a directory of instances; opening any one of them
        // opens the slide, so hand the driver the first.
        "DICOM/Leica-4" => first_instance(&extracted("DICOM/Leica-4.zip", "Leica-4")?),
        "DICOM/Hamamatsu-2" => first_instance(&extracted("DICOM/Hamamatsu-2.zip", "Hamamatsu-2")?),
        _ => common::test_slide_path(rel),
    }
}

/// Path to `extracted/<name>` beside a cached ZIP, if it's already been unpacked.
fn extracted(zip_rel: &str, name: &str) -> Option<PathBuf> {
    let zip = common::test_slide_path(zip_rel)?;
    let path = zip.parent()?.join("extracted").join(name);
    path.exists().then_some(path)
}

/// The first `.dcm` file in `dir`, in sorted order.
fn first_instance(dir: &Path) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dcm")))
        .collect();
    entries.sort();
    entries.into_iter().next()
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
        "{:<20} {:>15} {:>10} {:>10} {:>12} {:>10} {:>8} {:>10}",
        "vendor", "level0", "sk_open", "sk_read", "osl_proc", "mean_d", "max_d", "opaque%"
    );
    eprintln!("{}", "-".repeat(102));
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
            "{:<20} {:>15} {:>8.1}ms {:>8.1}ms {:>12} {:>10} {:>8} {:>10}",
            r.vendor, r.dims, r.sk_open_ms, r.sk_read_ms, osl, mean, max, frac
        );
    }
    eprintln!(
        "\n(osl_proc = one cold slidetool process: open+read+encode. sk_open/sk_read measured in-process.)"
    );
}
