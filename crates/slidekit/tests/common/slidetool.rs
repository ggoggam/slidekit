//! Driving OpenSlide's `slidetool` as the reference reader, and comparing its
//! pixels with ours.
//!
//! Shared by `bench_openslide` (local slidekit vs OpenSlide) and `bench_remote`
//! (remote slidekit vs OpenSlide). Both need the same three things: a way to ask
//! `slidetool` for a region, a way to time it that accounts for process startup,
//! and a pixel comparison that is tolerant of independent decoders but still
//! catches a channel swap or a misplaced tile.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

/// Mean per-channel difference above which the readers are considered to
/// disagree about pixels rather than to round JPEG coefficients differently.
pub const MEAN_DIFF_CEILING: f64 = 15.0;

/// Below this both-opaque fraction there is no real overlap to compare, so a
/// diff is reported as `n/a` rather than as a passing 0.00.
pub const MIN_OPAQUE_FRAC: f64 = 0.01;

/// Is `slidetool` on `PATH`?
pub fn available() -> bool {
    Command::new("slidetool")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The `slidetool` version string (first line of `--version`), for the report to
/// record rather than assume, together with the binary it came from.
///
/// Which binary answers matters: Homebrew still ships OpenSlide 4.0.0, which has
/// no Zeiss driver and reports Zeiss as incomparable with no hint that the wrong
/// tool was used. The repo builds 4.0.1 into `.tools/openslide` and mise puts it
/// first on `PATH`, so a bench run outside mise silently compares against the
/// older release.
///
/// `slidetool --version` writes to **stderr**, not stdout, so both streams are
/// consulted — reading only stdout made the bench announce that slidetool was
/// missing while it was in fact using it.
pub fn version() -> Option<String> {
    let out = Command::new("slidetool").arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let first_line = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .lines()
            .next()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
    };
    let version = first_line(&out.stdout).or_else(|| first_line(&out.stderr))?;
    match which() {
        Some(path) => Some(format!("{version} ({path})")),
        None => Some(version),
    }
}

/// Absolute path of the `slidetool` that `PATH` resolves to.
fn which() -> Option<String> {
    let out = Command::new("sh")
        .args(["-c", "command -v slidetool"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The OpenSlide release this bench is written against. Older releases differ in
/// which vendors they can read at all, so a comparison against one is a
/// different experiment, not the same one with noise.
pub const EXPECTED_VERSION: &str = "4.0.1";

/// Print the reference reader, loudly flagging a version other than
/// [`EXPECTED_VERSION`] instead of letting it pass as an equivalent run.
pub fn report_version() {
    match version() {
        Some(v) if v.contains(EXPECTED_VERSION) => eprintln!("reference reader: {v}"),
        Some(v) => eprintln!(
            "WARNING: reference reader is {v}, not OpenSlide {EXPECTED_VERSION}. \
             Vendor coverage differs between releases (4.0.0 has no Zeiss driver), so \
             these results are not comparable with the recorded ones. \
             Run `mise run bench:setup-slidetool` and invoke the bench through mise."
        ),
        None => eprintln!(
            "NOTE: `slidetool` not on PATH — no OpenSlide cross-check will be made. \
             Build it with `mise run bench:setup-slidetool`."
        ),
    }
}

/// Wall time of the cheapest possible `slidetool` invocation — `--version` does
/// no slide work, so this is the fork+exec+dynamic-link cost every other
/// measurement inherits. Returns the minimum of several runs: we want the floor
/// to subtract, not an average inflated by scheduling noise.
pub fn process_floor_ms() -> Option<f64> {
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
pub fn props_ms(path: &Path) -> Option<f64> {
    let t = Instant::now();
    let out = Command::new("slidetool")
        .args(["prop", "list"])
        .arg(path)
        .output()
        .ok()?;
    out.status.success().then(|| ms(t.elapsed()))
}

/// Read a region with `slidetool region read`, returning the whole-process wall
/// time and the decoded 8-bit RGBA pixels.
///
/// `tag` disambiguates the temp file so concurrent callers (nextest runs test
/// binaries in parallel) cannot read each other's PNG.
pub fn region_read(
    path: &Path,
    tag: &str,
    x: i64,
    y: i64,
    level: i32,
    w: i64,
    h: i64,
) -> Option<(f64, Vec<u8>)> {
    let out = std::env::temp_dir().join(format!(
        "slidekit_bench_osl_{}_{}.png",
        tag.replace(['/', ' '], "-"),
        std::process::id()
    ));
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
    let pixels = read_rgba_png(&out)?;
    std::fs::remove_file(&out).ok();
    Some((osl_ms, pixels))
}

/// Decode an 8-bit RGBA PNG to straight R,G,B,A bytes.
fn read_rgba_png(path: &Path) -> Option<Vec<u8>> {
    let file = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let decoder = png::Decoder::new(file);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
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
    Some(buf)
}

/// How far two readers' pixels are apart.
#[derive(Clone, Copy, Debug, Default)]
pub struct Diff {
    /// Mean per-channel RGB difference over pixels opaque in both readers.
    pub mean: f64,
    /// Largest single-channel difference over those pixels.
    pub max: u32,
    /// Fraction of the window opaque in both readers — the diff is measured
    /// only there, and a small fraction means the diff concludes nothing.
    pub opaque_frac: f64,
}

impl Diff {
    /// Is there enough overlap for the numbers to mean anything?
    pub fn meaningful(&self) -> bool {
        self.opaque_frac >= MIN_OPAQUE_FRAC
    }

    /// A real disagreement: enough overlap to judge, and past the ceiling.
    pub fn exceeds_ceiling(&self) -> bool {
        self.opaque_frac > 0.5 && self.mean >= MEAN_DIFF_CEILING
    }
}

/// Compare slidekit's packed ARGB (`0xAARRGGBB`) with straight RGBA bytes.
///
/// Only pixels opaque in *both* readers are counted: slidekit's ARGB is
/// premultiplied and slidetool's is straight, so the two coincide exactly where
/// alpha is 255 and are not comparable where it isn't. Background beyond the
/// image edge is not a decode comparison either way.
pub fn diff(sk: &[u32], rgba: &[u8], n: usize) -> Diff {
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
            continue;
        }
        for (a, b) in [(sr, or), (sg, og), (sb, ob)] {
            let d = a.abs_diff(b);
            sum += d as u64;
            max = max.max(d);
        }
        counted += 1;
    }
    Diff {
        mean: if counted == 0 {
            0.0
        } else {
            sum as f64 / (counted * 3) as f64
        },
        max,
        opaque_frac: if n == 0 {
            0.0
        } else {
            counted as f64 / n as f64
        },
    }
}

/// Fraction of a window that is actually stained specimen: opaque pixels that
/// are dark or saturated, by the same rule the tissue search uses.
///
/// Without this a pixel comparison can look like a pass while proving nothing. A
/// window of white background or black padding agrees perfectly between any two
/// readers — it has no tile content to get wrong — so the diff over it is not
/// evidence that the reader decodes or places tiles correctly.
pub fn tissue_fraction(argb: &[u32]) -> f64 {
    if argb.is_empty() {
        return 0.0;
    }
    let tissue = argb
        .iter()
        .filter(|&&p| {
            let (r, g, b, a) = (
                (p >> 16) & 0xff,
                (p >> 8) & 0xff,
                p & 0xff,
                (p >> 24) & 0xff,
            );
            let bright = r.max(g).max(b);
            let saturation = bright - r.min(g).min(b);
            // Opaque, not near-white, and not near-black: black padding is as
            // uninformative as white background.
            a == 0xff && bright >= 25 && (bright < 210 || saturation > 25)
        })
        .count();
    tissue as f64 / argb.len() as f64
}

/// Print where inside a window two readers disagree, not just how much.
///
/// A mean is not enough to tell a decoder rounding by ±1 everywhere from a
/// misplaced tile that is badly wrong over a band: both can average to the same
/// small number. This prints the bounding box of the disagreement and a coarse
/// heat map of it, which distinguishes the two at a glance — a seam shows up as a
/// line, a wrong tile as a block, and codec noise as an even wash.
pub fn profile(sk: &[u32], rgba: &[u8], w: i64, h: i64, threshold: u32) {
    const GRID: usize = 24;
    let (w, h) = (w as usize, h as usize);
    let n = (w * h).min(sk.len()).min(rgba.len() / 4);

    let mut cell_sum = vec![0u64; GRID * GRID];
    let mut cell_count = vec![0u64; GRID * GRID];
    let (mut differing, mut compared) = (0usize, 0usize);
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0usize, 0usize);
    let mut worst = (0u32, 0usize, 0usize, [0u32; 3], [0u32; 3]);

    for i in 0..n {
        let (x, y) = (i % w, i / w);
        let px = sk[i];
        let sa = (px >> 24) & 0xff;
        let oa = rgba[i * 4 + 3] as u32;
        if sa != 0xff || oa != 0xff {
            continue; // not comparable: premultiplied vs straight alpha
        }
        compared += 1;
        let s = [(px >> 16) & 0xff, (px >> 8) & 0xff, px & 0xff];
        let o = [
            rgba[i * 4] as u32,
            rgba[i * 4 + 1] as u32,
            rgba[i * 4 + 2] as u32,
        ];
        let d: u32 = (0..3).map(|c| s[c].abs_diff(o[c])).max().unwrap_or(0);
        let cell = (y * GRID / h) * GRID + (x * GRID / w);
        cell_sum[cell] += d as u64;
        cell_count[cell] += 1;
        if d > threshold {
            differing += 1;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
            if d > worst.0 {
                worst = (d, x, y, s, o);
            }
        }
    }

    if compared == 0 {
        eprintln!("    profile: no comparable (both-opaque) pixels in this window");
        return;
    }
    eprintln!(
        "    profile: {differing} of {compared} both-opaque pixels differ by more than \
         {threshold} on some channel ({:.3}%)",
        differing as f64 / compared as f64 * 100.0
    );
    if differing == 0 {
        return;
    }
    eprintln!(
        "    disagreement bounding box within the window: x {x0}..={x1}, y {y0}..={y1} \
         ({} x {} px of {w} x {h})",
        x1 - x0 + 1,
        y1 - y0 + 1
    );
    eprintln!(
        "    worst pixel at +({},{}) in-window: slidekit rgb {:?} vs OpenSlide rgb {:?} \
         (max channel diff {})",
        worst.1, worst.2, worst.3, worst.4, worst.0
    );
    eprintln!("    mean max-channel diff per {GRID}x{GRID} cell (. = 0, 1-9 scaled, # = >=10):");
    for gy in 0..GRID {
        let mut line = String::with_capacity(GRID + 6);
        line.push_str("      ");
        for gx in 0..GRID {
            let c = gy * GRID + gx;
            if cell_count[c] == 0 {
                line.push(' ');
                continue;
            }
            let mean = cell_sum[c] as f64 / cell_count[c] as f64;
            line.push(match mean {
                m if m < 0.5 => '.',
                m if m >= 10.0 => '#',
                m => char::from_digit(m.round() as u32, 10).unwrap_or('#'),
            });
        }
        eprintln!("{line}");
    }
}

/// Pack slidekit's ARGB (`0xAARRGGBB`) into straight R,G,B,A bytes for PNG.
pub fn argb_to_rgba(argb: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(argb.len() * 4);
    for &px in argb {
        out.push((px >> 16) as u8); // R
        out.push((px >> 8) as u8); // G
        out.push(px as u8); // B
        out.push((px >> 24) as u8); // A
    }
    out
}

/// Render the disagreement between two readers as an image.
///
/// Grey level is the per-pixel maximum channel difference multiplied by `gain`, so
/// the ±1–2 counts two conforming decoders produce stay near-black while a
/// misplaced tile lights up. Pixels that are not comparable — opaque in one reader
/// and not the other — are painted dark blue rather than black, so "no data" is
/// never mistaken for "agrees".
pub fn diff_image(sk: &[u32], rgba: &[u8], w: i64, h: i64, gain: u32) -> Vec<u8> {
    let n = ((w * h) as usize).min(sk.len()).min(rgba.len() / 4);
    let mut out = Vec::with_capacity(n * 4);
    for i in 0..n {
        let px = sk[i];
        let sa = (px >> 24) & 0xff;
        let oa = rgba[i * 4 + 3] as u32;
        if sa != 0xff || oa != 0xff {
            out.extend_from_slice(&[0, 0, 72, 255]);
            continue;
        }
        let s = [(px >> 16) & 0xff, (px >> 8) & 0xff, px & 0xff];
        let o = [
            rgba[i * 4] as u32,
            rgba[i * 4 + 1] as u32,
            rgba[i * 4 + 2] as u32,
        ];
        let d = (0..3).map(|c| s[c].abs_diff(o[c])).max().unwrap_or(0);
        let v = (d * gain).min(255) as u8;
        out.extend_from_slice(&[v, v, v, 255]);
    }
    out
}

/// Box-average an RGBA image down so that its longest side is at most `target`.
///
/// Averaging rather than sampling matters for this use: a one-pixel seam in a
/// nearest-neighbour thumbnail can vanish between sample points, and a thumbnail
/// that hides the defect defeats the purpose of looking.
pub fn downscale(rgba: &[u8], w: i64, h: i64, target: u32) -> (Vec<u8>, i64, i64) {
    let (w, h) = (w.max(1), h.max(1));
    let factor = (w.max(h) as f64 / target as f64).ceil().max(1.0) as i64;
    if factor <= 1 {
        return (rgba.to_vec(), w, h);
    }
    let (dw, dh) = ((w / factor).max(1), (h / factor).max(1));
    let mut out = Vec::with_capacity((dw * dh * 4) as usize);
    for dy in 0..dh {
        for dx in 0..dw {
            let mut acc = [0u32; 4];
            let mut count = 0u32;
            for sy in dy * factor..((dy + 1) * factor).min(h) {
                for sx in dx * factor..((dx + 1) * factor).min(w) {
                    let i = ((sy * w + sx) * 4) as usize;
                    if i + 3 >= rgba.len() {
                        continue;
                    }
                    for (c, a) in acc.iter_mut().enumerate() {
                        *a += rgba[i + c] as u32;
                    }
                    count += 1;
                }
            }
            let count = count.max(1);
            for a in acc {
                out.push((a / count) as u8);
            }
        }
    }
    (out, dw, dh)
}

/// Write 8-bit RGBA pixels to `path` as a PNG. Best-effort — a failure here only
/// costs a debug artifact, so it warns rather than aborting the bench.
pub fn dump_png(path: &Path, rgba: &[u8], w: i64, h: i64) {
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

pub fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Median of a set of samples. Takes `&mut` because it sorts in place — the
/// caller usually wants the sorted samples afterwards anyway.
pub fn median(samples: &mut [f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = samples.len() / 2;
    if samples.len().is_multiple_of(2) {
        (samples[mid - 1] + samples[mid]) / 2.0
    } else {
        samples[mid]
    }
}
