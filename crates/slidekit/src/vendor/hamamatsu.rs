//! Hamamatsu NDPI vendor driver.
//!
//! Hamamatsu `.ndpi` files (NanoZoomer Digital Pathology Image) are TIFFs whose
//! directory 0 carries the private tag 65420. Unlike the tiled-TIFF vendors,
//! **each pyramid level is one giant baseline JPEG stored in a single strip** —
//! there are no TIFF tiles. Random tile access is achieved with the JPEG's
//! restart markers: the entropy stream is split into restart intervals, each of
//! which is exactly one tile (`restart_interval` MCUs wide × one MCU row tall).
//! To decode a tile we reconstruct a small self-contained JPEG = the level
//! JPEG's own header (SOI..SOS, carrying its quantization/Huffman tables) +
//! that one restart interval's entropy bytes + an EOI, with the frame header's
//! dimensions rewritten to the tile size. The reconstructed buffer is a normal
//! JPEG that [`JpegDecoder`] decodes with no special support.
//!
//! Ported from `openslide-vendor-hamamatsu.c` (NDPI path only).
//!
//! ponytail / deferred vs. the C driver:
//!  - **NDPI only.** VMS/VMU are separate multi-file (`.vms`/`.vmu` index →
//!    multiple JPEG/NGR files) formats; our single-blob `SlideReader` can't
//!    model them. They live behind their own detect function in C and never
//!    overlap NDPI (which requires a real TIFF + tag 65420).
//!  - **z=0 focal plane only.** Directories with a non-zero focal plane
//!    (`NDPI_FOCAL_PLANE`) are skipped, matching the C driver's pyramid.
//!  - **No `scale_denom` derived levels.** The C driver synthesizes extra 2×/4×/
//!    8× levels via libjpeg DCT-domain downscaling; we emit only the real ones.
//!  - **Coarse locking on read.** A region read holds the TIFF mutex for its
//!    duration (restart-marker offsets are memoized per level under their own
//!    lock). ponytail: finer granularity only if NDPI becomes a hot concurrent
//!    path.

use parking_lot::Mutex;

use crate::associated::AssociatedImage;
use crate::decode::TileDecoder;
use crate::decode::jpeg::JpegDecoder;
use crate::decode::tiff::{TiffLike, tags};
use crate::error::{OpenSlideError, Result};
use crate::grid::simple::SimpleGrid;
use crate::icc::IccProfile;
use crate::io::SlideReader;
use crate::level::LevelInfo;
use crate::properties::{self, Properties};
use crate::vendor::generic_tiff::{set_resolution_props, set_tiff_string_props};
use crate::vendor::{LevelData, SlideData, VendorDriver};

// NDPI private TIFF tags (openslide-vendor-hamamatsu.c).
const NDPI_SOURCELENS: u16 = 65421;
const NDPI_XOFFSET: u16 = 65422;
const NDPI_YOFFSET: u16 = 65423;
const NDPI_FOCAL_PLANE: u16 = 65424;
const NDPI_MCU_STARTS_LOW: u16 = 65426;
const NDPI_REFERENCE: u16 = 65427;
const NDPI_MCU_STARTS_HIGH: u16 = 65432;
const NDPI_PROPERTY_MAP: u16 = 65449;

/// libjpeg's maximum image dimension; also the ceiling for a reconstructed tile.
const JPEG_MAX_DIMENSION: i64 = 65500;

/// Bytes read from the start of a level's strip to parse its JPEG header. NDPI
/// headers (APPn + DQT/DHT/SOF/DRI/SOS) are a few KiB; this is generous slack.
const HEADER_SCAN_LEN: u64 = 256 * 1024;

pub struct HamamatsuDriver;

impl VendorDriver for HamamatsuDriver {
    fn name(&self) -> &str {
        "hamamatsu"
    }

    fn vendor(&self) -> &str {
        "hamamatsu"
    }

    fn detect(&self, _reader: &dyn SlideReader, tl: Option<&mut TiffLike>) -> Result<bool> {
        let tl = tl.ok_or_else(|| dec("Not a TIFF file"))?;
        // The C predicate is exactly "directory 0 has tag 65420". (Note: for
        // sub-4 GiB files the TIFF parser doesn't flag `is_ndpi()` because the
        // 64-bit-offset trick never triggers — so we key on the tag, not that.)
        if !tl.has_tag(0, tags::NDPI) {
            return Err(dec("No NDPI tag 65420"));
        }
        Ok(true)
    }

    fn open(&self, _reader: Box<dyn SlideReader>, tl: Option<TiffLike>) -> Result<SlideData> {
        let mut tl = tl.ok_or_else(|| dec("Not a TIFF file"))?;
        let dir_count = tl.directory_count();

        // Classify each directory (one JPEG each) by its source lens.
        let mut levels: Vec<(i64, NdpiLevel)> = Vec::new(); // (level width, level)
        let mut macro_dir: Option<i64> = None;

        for dir in 0..dir_count {
            let lens = tl.get_float(dir, NDPI_SOURCELENS).unwrap_or(0.0);
            if lens == -1.0 {
                macro_dir = Some(dir); // macro overview
                continue;
            }
            if lens <= 0.0 {
                continue; // map (-2) / unknown → not part of the pyramid
            }
            // Pyramid level. Only the z=0 focal plane is supported.
            if tl.get_sint(dir, NDPI_FOCAL_PLANE).unwrap_or(0) != 0 {
                continue;
            }
            let level = build_ndpi_level(&mut tl, dir)?;
            let w = level.level_w;
            levels.push((w, level));
        }

        if levels.is_empty() {
            return Err(dec("Couldn't find any NDPI pyramid levels"));
        }
        // Largest first (matches the other TIFF-pyramid drivers).
        levels.sort_by_key(|(w, _)| std::cmp::Reverse(*w));
        let base_w = levels[0].0 as f64;

        let level_data: Vec<LevelData> = levels
            .into_iter()
            .map(|(_, lvl)| {
                let (lw, lh) = (lvl.level_w, lvl.level_h);
                let (tw, th) = (lvl.tile_w, lvl.tile_h);
                LevelData {
                    info: LevelInfo {
                        width: lw,
                        height: lh,
                        downsample: base_w / lw as f64,
                        tile_width: tw,
                        tile_height: th,
                    },
                    grid: SimpleGrid::new(lw, lh, tw, th),
                    tiff_dir: lvl.dir,
                    decoder: Box::new(JpegDecoder::new()),
                    tiff_tile_width: tw,
                    tiff_tile_height: th,
                    tilemap: None,
                    ndpi: Some(lvl),
                    mirax: None,
                    zeiss: None,
                    synthetic: None,
                    dicom: None,
                }
            })
            .collect();

        // Properties are read from physical directory 0 (the base level),
        // matching `ndpi_set_props(osr, tl, 0)`.
        let mut props = Properties::new();
        props.insert(properties::names::VENDOR, "hamamatsu");
        set_resolution_props(&mut tl, 0, &mut props);
        set_tiff_string_props(&mut tl, 0, &mut props);
        if let Ok(lens) = tl.get_float(0, NDPI_SOURCELENS) {
            let s = fmt_num(lens);
            props.insert("hamamatsu.SourceLens", s.clone());
            props.insert(properties::names::OBJECTIVE_POWER, s);
        }
        if let Ok(v) = tl.get_sint(0, NDPI_XOFFSET) {
            props.insert("hamamatsu.XOffsetFromSlideCentre", v.to_string());
        }
        if let Ok(v) = tl.get_sint(0, NDPI_YOFFSET) {
            props.insert("hamamatsu.YOffsetFromSlideCentre", v.to_string());
        }
        if let Ok(v) = tl.get_string(0, NDPI_REFERENCE) {
            if !v.is_empty() {
                props.insert("hamamatsu.Reference", v);
            }
        }
        set_property_map(&mut tl, &mut props);

        // Macro associated image: decode the whole single-strip JPEG (small,
        // near the file start, so no >4 GiB offset fixup is needed). Non-fatal.
        let mut associated_images: Vec<(String, AssociatedImage)> = Vec::new();
        if let Some(mdir) = macro_dir {
            if let Ok(img) = decode_whole_jpeg(&mut tl, mdir) {
                associated_images.push(("macro".to_string(), img));
            }
        }

        let icc_profile = tl.read_icc_profile(0).ok().flatten();

        Ok(SlideData {
            levels: level_data,
            properties: props,
            associated_images,
            icc_profile: icc_profile.into(),
            tiff: Some(tl),
        })
    }
}

// ───────────────────────── NDPI level / tiling ──────────────────────────────

/// Restart-marker tiling state for one NDPI pyramid level.
///
/// See the module docs for the reconstruction scheme. `mcu_starts` holds the
/// absolute file offset of each tile's first entropy byte, filled lazily by
/// walking restart markers (seeded by the `NDPI_MCU_STARTS` tag when present).
pub struct NdpiLevel {
    dir: i64,
    /// Level dimensions in pixels.
    level_w: i64,
    level_h: i64,
    /// The level JPEG's own header, SOI..SOS inclusive (prepended to each tile).
    header: Vec<u8>,
    /// Index within `header` of the SOF frame-height high byte; the per-tile
    /// output dimensions are written here (4 bytes: Y hi/lo, X hi/lo).
    sof_dims_offset: usize,
    /// Absolute file offset of the JPEG's first byte (NDPI-fixed).
    start_in_file: u64,
    /// Absolute file offset one past the JPEG's last byte.
    end_in_file: u64,
    /// Tile dimensions in pixels (MCU-aligned).
    tile_w: i64,
    tile_h: i64,
    tiles_across: i64,
    tile_count: usize,
    /// Absolute file offset of each tile's first entropy byte; `-1` = unknown.
    mcu_starts: Mutex<Vec<i64>>,
    /// Per-tile offsets from the `NDPI_MCU_STARTS` tag (absolute, un-validated),
    /// or `None` when absent — then offsets are found by scanning.
    unreliable: Option<Vec<i64>>,
}

impl NdpiLevel {
    /// Decode tile `(col, row)` to ARGB, or `None` if the tile is out of range.
    ///
    /// `reader` supplies raw file bytes; `decoder` is this level's `JpegDecoder`.
    pub fn decode_tile(
        &self,
        reader: &dyn SlideReader,
        col: i64,
        row: i64,
        decoder: &dyn TileDecoder,
    ) -> Result<Option<Vec<u32>>> {
        if col < 0 || row < 0 || col >= self.tiles_across {
            return Ok(None);
        }
        let idx = (row * self.tiles_across + col) as usize;
        if idx >= self.tile_count {
            return Ok(None);
        }

        let start = self.ensure_start(reader, idx)? as u64;
        let stop = if idx + 1 < self.tile_count {
            self.ensure_start(reader, idx + 1)? as u64
        } else {
            self.end_in_file
        };
        if stop <= start {
            return Ok(None);
        }

        // header + entropy(one restart interval) + EOI.
        let entropy_len = (stop - start) as usize;
        let mut buf = Vec::with_capacity(self.header.len() + entropy_len);
        buf.extend_from_slice(&self.header);
        let entropy_at = buf.len();
        buf.resize(entropy_at + entropy_len, 0);
        reader.read_exact_at(start, &mut buf[entropy_at..])?;

        // The copied range ends on the next restart marker (or the file's own
        // EOI) — rewrite those trailing 2 bytes to an EOI so the stream is a
        // complete JPEG.
        let n = buf.len();
        if n < 2 || buf[n - 2] != 0xFF {
            return Err(dec("NDPI tile does not end on a marker boundary"));
        }
        buf[n - 1] = 0xD9;

        // Declare the tile's true (MCU-aligned) size in the frame header. The
        // base level's real SOF dimensions are bogus/oversized (> 65500).
        let o = self.sof_dims_offset;
        buf[o] = (self.tile_h >> 8) as u8;
        buf[o + 1] = self.tile_h as u8;
        buf[o + 2] = (self.tile_w >> 8) as u8;
        buf[o + 3] = self.tile_w as u8;

        decoder
            .decode(&buf, self.tile_w as u32, self.tile_h as u32)
            .map(Some)
    }

    /// Ensure `mcu_starts[target]` is known and return it, walking restart
    /// markers as needed. Offsets are memoized, so in-order reads scan once.
    fn ensure_start(&self, reader: &dyn SlideReader, target: usize) -> Result<i64> {
        let mut starts = self.mcu_starts.lock();
        if starts[0] == -1 {
            // Tile 0's entropy begins right after the header.
            starts[0] = (self.start_in_file + self.header.len() as u64) as i64;
        }
        if starts[target] != -1 {
            return Ok(starts[target]);
        }

        // Walk back to the nearest known offset, promoting a valid tag hint.
        let mut first_good = target;
        while starts[first_good] == -1 {
            if let Some(hint) = self.unreliable.as_ref().map(|v| v[first_good]) {
                if hint >= 2 && is_restart_marker(reader, hint as u64 - 2)? {
                    starts[first_good] = hint;
                    break;
                }
                // Bogus hint → keep walking; the forward scan recomputes it.
            }
            first_good -= 1; // starts[0] is always known, so this terminates.
        }

        // Scan forward, counting restart markers, until we reach `target`.
        let mut pos = starts[first_good] as u64;
        let mut idx = first_good;
        while idx < target {
            let after = next_restart(reader, pos, self.end_in_file)?
                .ok_or_else(|| dec("NDPI: ran out of restart markers before tile"))?;
            idx += 1;
            starts[idx] = after as i64;
            pos = after;
        }
        Ok(starts[target])
    }
}

/// Read a directory's single-strip JPEG header and build its [`NdpiLevel`].
fn build_ndpi_level(tl: &mut TiffLike, dir: i64) -> Result<NdpiLevel> {
    let offsets = tl.get_uints(dir, tags::STRIP_OFFSETS)?;
    let counts = tl.get_uints(dir, tags::STRIP_BYTE_COUNTS)?;
    if offsets.len() != 1 || counts.len() != 1 {
        return Err(dec("NDPI level is not a single strip"));
    }
    let start_in_file = tl.uint_fix_offset_ndpi(dir, offsets[0]);
    let strip_len = counts[0];
    let end_in_file = start_in_file + strip_len;

    let tiff_w = tl.get_uint(dir, tags::IMAGE_WIDTH)? as i64;
    let tiff_h = tl.get_uint(dir, tags::IMAGE_LENGTH)? as i64;

    // Parse the JPEG header from the front of the strip.
    let scan_len = strip_len.min(HEADER_SCAN_LEN) as usize;
    let mut head = vec![0u8; scan_len];
    tl.reader().read_exact_at(start_in_file, &mut head)?;
    let hdr = parse_jpeg_header(&head)?;

    // Tile geometry.
    let (level_w, level_h, tile_w, tile_h, tiles_across, tiles_down) = if hdr.restart_interval == 0
    {
        // Untiled: one tile = whole image; the JPEG's own dimensions win.
        (hdr.sof_w, hdr.sof_h, hdr.sof_w, hdr.sof_h, 1, 1)
    } else {
        // Tiled: TIFF dimensions win (the base JPEG's SOF dims are bogus).
        let mcus_per_row = (tiff_w + hdr.mcu_w - 1) / hdr.mcu_w;
        let ri = hdr.restart_interval as i64;
        if ri > mcus_per_row || mcus_per_row % ri != 0 {
            return Err(dec("NDPI: inconsistent restart-marker spacing"));
        }
        let tw = hdr.mcu_w * ri;
        let th = hdr.mcu_h;
        (
            tiff_w,
            tiff_h,
            tw,
            th,
            mcus_per_row / ri,
            (tiff_h + th - 1) / th,
        )
    };

    if tile_w <= 0 || tile_h <= 0 || tile_w > JPEG_MAX_DIMENSION || tile_h > JPEG_MAX_DIMENSION {
        return Err(dec("NDPI: unsupported tile dimensions"));
    }
    let tile_count = (tiles_across * tiles_down) as usize;
    let unreliable = read_unreliable_starts(tl, dir, start_in_file, tile_count)?;

    Ok(NdpiLevel {
        dir,
        level_w,
        level_h,
        header: hdr.header,
        sof_dims_offset: hdr.sof_dims_offset,
        start_in_file,
        end_in_file,
        tile_w,
        tile_h,
        tiles_across,
        tile_count,
        mcu_starts: Mutex::new(vec![-1; tile_count]),
        unreliable,
    })
}

/// Load the `NDPI_MCU_STARTS` tag(s) into absolute file offsets, or `None` when
/// absent/mismatched (the caller then scans for restart markers). Only tiled
/// levels (`tile_count > 1`) carry the tag.
fn read_unreliable_starts(
    tl: &mut TiffLike,
    dir: i64,
    start_in_file: u64,
    tile_count: usize,
) -> Result<Option<Vec<i64>>> {
    if tile_count <= 1 || tl.value_count(dir, NDPI_MCU_STARTS_LOW) as usize != tile_count {
        return Ok(None);
    }
    let low = tl.get_uints(dir, NDPI_MCU_STARTS_LOW)?;
    let high = if tl.value_count(dir, NDPI_MCU_STARTS_HIGH) as usize == tile_count {
        Some(tl.get_uints(dir, NDPI_MCU_STARTS_HIGH)?)
    } else {
        None
    };
    let offsets = (0..tile_count)
        .map(|i| {
            let v = low[i] | high.as_ref().map_or(0, |h| h[i] << 32);
            (start_in_file + v) as i64
        })
        .collect();
    Ok(Some(offsets))
}

// ───────────────────────── JPEG marker parsing ──────────────────────────────

/// Geometry and header bytes extracted from a level JPEG.
struct JpegHeader {
    /// SOI..SOS inclusive.
    header: Vec<u8>,
    /// Index of the SOF frame-height high byte within `header`.
    sof_dims_offset: usize,
    /// Restart interval in MCUs (0 = no DRI marker = untiled).
    restart_interval: usize,
    mcu_w: i64,
    mcu_h: i64,
    sof_w: i64,
    sof_h: i64,
}

/// Walk the JPEG marker stream from SOI to SOS, extracting tiling geometry.
fn parse_jpeg_header(data: &[u8]) -> Result<JpegHeader> {
    if data.len() < 2 || data[0] != 0xFF || data[1] != 0xD8 {
        return Err(dec("NDPI: not a JPEG (no SOI)"));
    }
    let mut pos = 2usize;
    let mut restart_interval = 0usize;
    let mut sof_dims_offset: Option<usize> = None;
    let (mut sof_w, mut sof_h) = (0i64, 0i64);
    let (mut max_h, mut max_v) = (1i64, 1i64);

    let u16_at = |o: usize| -> Result<usize> {
        match (data.get(o), data.get(o + 1)) {
            (Some(&hi), Some(&lo)) => Ok(((hi as usize) << 8) | lo as usize),
            _ => Err(dec("NDPI: truncated JPEG header")),
        }
    };

    loop {
        if data.get(pos) != Some(&0xFF) {
            return Err(dec("NDPI: expected a JPEG marker"));
        }
        // Skip any fill 0xFF bytes preceding the marker code.
        let mut mp = pos + 1;
        while data.get(mp) == Some(&0xFF) {
            mp += 1;
        }
        let &marker = data
            .get(mp)
            .ok_or_else(|| dec("NDPI: truncated JPEG header"))?;
        let seg = mp + 1; // first length byte

        match marker {
            0xD8 | 0xD9 => return Err(dec("NDPI: unexpected SOI/EOI in header")),
            0xDA => {
                // Start of scan — the header ends after this segment.
                let len = u16_at(seg)?;
                let header_end = seg + len;
                if header_end > data.len() {
                    return Err(dec("NDPI: SOS extends past scanned header"));
                }
                let dims = sof_dims_offset.ok_or_else(|| dec("NDPI: no SOF before SOS"))?;
                // Ns (components in scan) is the byte after the length field.
                let comps = *data
                    .get(seg + 2)
                    .ok_or_else(|| dec("NDPI: truncated SOS"))? as i64;
                let (mcu_w, mcu_h) = if comps == 1 {
                    (8, 8)
                } else {
                    (max_h * 8, max_v * 8)
                };
                return Ok(JpegHeader {
                    header: data[..header_end].to_vec(),
                    sof_dims_offset: dims,
                    restart_interval,
                    mcu_w,
                    mcu_h,
                    sof_w,
                    sof_h,
                });
            }
            0xDD => {
                // Define restart interval: len(2)=4, then Ri(2).
                restart_interval = u16_at(seg + 2)?;
                pos = seg + u16_at(seg)?;
            }
            // Start-of-frame markers (baseline/extended/progressive/lossless),
            // excluding DHT (C4), JPG (C8), DAC (CC).
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                let len = u16_at(seg)?;
                sof_h = u16_at(seg + 3)? as i64; // Y (after len + 1-byte precision)
                sof_w = u16_at(seg + 5)? as i64; // X
                sof_dims_offset = Some(seg + 3);
                let nf = *data
                    .get(seg + 7)
                    .ok_or_else(|| dec("NDPI: truncated SOF"))? as usize;
                for c in 0..nf {
                    let hv = *data
                        .get(seg + 8 + c * 3 + 1)
                        .ok_or_else(|| dec("NDPI: truncated SOF component"))?;
                    max_h = max_h.max((hv >> 4) as i64);
                    max_v = max_v.max((hv & 0x0F) as i64);
                }
                pos = seg + len;
            }
            _ => {
                // Any other marker carries a 2-byte length; skip its segment.
                pos = seg + u16_at(seg)?;
            }
        }
    }
}

// ───────────────────────── restart-marker scanning ──────────────────────────

/// Whether the 2 bytes at `off` are a restart marker (`0xFF 0xD0..=0xD7`).
fn is_restart_marker(reader: &dyn SlideReader, off: u64) -> Result<bool> {
    let mut m = [0u8; 2];
    reader.read_exact_at(off, &mut m)?;
    Ok(m[0] == 0xFF && (0xD0..=0xD7).contains(&m[1]))
}

/// Scan `[pos, end)` for the next restart marker, returning the absolute offset
/// of the byte immediately after the 2-byte marker (i.e. the next tile's start).
fn next_restart(reader: &dyn SlideReader, mut pos: u64, end: u64) -> Result<Option<u64>> {
    const CHUNK: usize = 64 * 1024;
    let mut buf = vec![0u8; CHUNK];
    let mut prev_ff = false;
    while pos < end {
        let n = ((end - pos) as usize).min(CHUNK);
        reader.read_exact_at(pos, &mut buf[..n])?;
        for (i, &b) in buf[..n].iter().enumerate() {
            if prev_ff && (0xD0..=0xD7).contains(&b) {
                return Ok(Some(pos + i as u64 + 1));
            }
            prev_ff = b == 0xFF;
        }
        pos += n as u64;
    }
    Ok(None)
}

// ───────────────────────── properties / associated ──────────────────────────

/// Parse the `NDPI_PROPERTY_MAP` (65449) — CRLF-separated `key=value` records —
/// into `hamamatsu.<key>` properties.
fn set_property_map(tl: &mut TiffLike, props: &mut Properties) {
    let Ok(raw) = tl.get_string(0, NDPI_PROPERTY_MAP) else {
        return;
    };
    for record in raw.split("\r\n") {
        if let Some((k, v)) = record.split_once('=') {
            if !k.is_empty() && !v.is_empty() {
                props.insert(format!("hamamatsu.{k}"), v.to_string());
            }
        }
    }
}

/// Decode a single-strip JPEG directory whole (used for the macro image).
fn decode_whole_jpeg(tl: &mut TiffLike, dir: i64) -> Result<AssociatedImage> {
    let width = tl.get_uint(dir, tags::IMAGE_WIDTH)? as i64;
    let height = tl.get_uint(dir, tags::IMAGE_LENGTH)? as i64;
    let data = tl.read_strip_data(dir)?;
    let argb_data = JpegDecoder::new().decode(&data, width as u32, height as u32)?;
    Ok(AssociatedImage {
        width,
        height,
        argb_data,
        icc_profile: IccProfile::absent(),
    })
}

/// Format a lens/objective value without a trailing `.0` for whole numbers.
fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn dec(msg: impl Into<String>) -> OpenSlideError {
    OpenSlideError::Decode(msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal JPEG-ish header: SOI, SOF0 (dims + sampling), DRI, SOS.
    /// `sof_sampling` is the packed H/V nibble of the (single) component.
    fn make_header(sof_w: u16, sof_h: u16, sof_sampling: u8, restart: u16, comps: u8) -> Vec<u8> {
        let mut d = vec![0xFF, 0xD8]; // SOI
        // SOF0: FF C0, len, precision, Y(2), X(2), Nf, [id, HV, Tq]*Nf
        let nf = 1u8;
        let sof_len = 8 + 3 * nf as u16;
        d.extend_from_slice(&[0xFF, 0xC0]);
        d.extend_from_slice(&sof_len.to_be_bytes());
        d.push(8); // precision
        d.extend_from_slice(&sof_h.to_be_bytes());
        d.extend_from_slice(&sof_w.to_be_bytes());
        d.push(nf);
        d.extend_from_slice(&[1, sof_sampling, 0]); // component 1
        // DRI: FF DD, len=4, Ri(2)
        d.extend_from_slice(&[0xFF, 0xDD, 0x00, 0x04]);
        d.extend_from_slice(&restart.to_be_bytes());
        // SOS: FF DA, len, Ns, [Cs, Td/Ta]*Ns, Ss, Se, Ah/Al
        let sos_len = 6 + 2 * comps as u16;
        d.extend_from_slice(&[0xFF, 0xDA]);
        d.extend_from_slice(&sos_len.to_be_bytes());
        d.push(comps);
        for _ in 0..comps {
            d.extend_from_slice(&[1, 0]);
        }
        d.extend_from_slice(&[0, 63, 0]);
        d
    }

    #[test]
    fn parses_sof_dri_and_mcu_geometry() {
        // 4:2:0 (H=2,V=2 → 0x22), interleaved 3-component scan, restart 5.
        let bytes = make_header(40000, 30000, 0x22, 5, 3);
        let h = parse_jpeg_header(&bytes).unwrap();
        assert_eq!(h.restart_interval, 5);
        assert_eq!((h.sof_w, h.sof_h), (40000, 30000));
        assert_eq!((h.mcu_w, h.mcu_h), (16, 16)); // 2*8
        // header is SOI..SOS inclusive == the whole synthetic buffer.
        assert_eq!(h.header.len(), bytes.len());
        // The dims offset points at the SOF Y high byte.
        assert_eq!(
            h.sof_h,
            ((bytes[h.sof_dims_offset] as i64) << 8) | bytes[h.sof_dims_offset + 1] as i64
        );
        assert_eq!(
            h.sof_w,
            ((bytes[h.sof_dims_offset + 2] as i64) << 8) | bytes[h.sof_dims_offset + 3] as i64
        );
    }

    #[test]
    fn single_component_scan_uses_8px_mcu() {
        // comps_in_scan == 1 → MCU is one DCT block regardless of sampling.
        let bytes = make_header(1024, 768, 0x22, 0, 1);
        let h = parse_jpeg_header(&bytes).unwrap();
        assert_eq!((h.mcu_w, h.mcu_h), (8, 8));
        assert_eq!(h.restart_interval, 0); // no DRI → untiled
    }

    #[test]
    fn rejects_non_jpeg_and_headerless() {
        assert!(parse_jpeg_header(&[0, 1, 2, 3]).is_err());
        // SOI with no SOF before SOS.
        let d = [0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x08, 0x01, 1, 0, 0, 63, 0];
        assert!(parse_jpeg_header(&d).is_err());
    }

    // A fake reader over an in-memory buffer, for the restart-marker scanner.
    struct MemReader(Vec<u8>);
    impl SlideReader for MemReader {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
            let start = offset as usize;
            let n = buf.len().min(self.0.len().saturating_sub(start));
            buf[..n].copy_from_slice(&self.0[start..start + n]);
            Ok(n)
        }
        fn size(&self) -> Result<u64> {
            Ok(self.0.len() as u64)
        }
    }

    #[test]
    fn scans_restart_markers_ignoring_stuffing() {
        // entropy ... FF00 (stuffed) ... FFD0 (restart) ... FFFF FFD1 ... FFD9(EOI)
        let data = vec![
            0x12, 0xFF, 0x00, 0x34, // stuffed 0xFF00 → not a marker
            0xFF, 0xD0, // restart 0 at offset 4..6 → after = 6
            0x56, 0xFF, 0xFF, 0xD1, // fill FF then restart 1 → marker at 8..10, after = 10
            0xFF, 0xD9, // EOI (not a restart) at 10..12
        ];
        let r = MemReader(data.clone());
        let first = next_restart(&r, 0, data.len() as u64).unwrap();
        assert_eq!(first, Some(6));
        let second = next_restart(&r, first.unwrap(), data.len() as u64).unwrap();
        assert_eq!(second, Some(10));
        // Past the last restart there are no more (EOI D9 is excluded).
        let none = next_restart(&r, second.unwrap(), data.len() as u64).unwrap();
        assert_eq!(none, None);
        // Restart-marker validation reads the 2 bytes *at* the marker.
        assert!(is_restart_marker(&r, 4).unwrap());
        assert!(!is_restart_marker(&r, 10).unwrap()); // that's the EOI
    }

    #[test]
    fn property_map_parses_key_values() {
        // Directly exercise the record parser used by set_property_map.
        let raw = "Objective.Lens.Magnificant=20\r\nblank\r\n=noKey\r\nNoValue=\r\nk=v";
        let mut got: Vec<(String, String)> = Vec::new();
        for record in raw.split("\r\n") {
            if let Some((k, v)) = record.split_once('=') {
                if !k.is_empty() && !v.is_empty() {
                    got.push((k.to_string(), v.to_string()));
                }
            }
        }
        assert_eq!(
            got,
            vec![
                ("Objective.Lens.Magnificant".into(), "20".into()),
                ("k".into(), "v".into()),
            ]
        );
    }
}
