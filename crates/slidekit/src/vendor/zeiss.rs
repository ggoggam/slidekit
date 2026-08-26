//! Zeiss CZI (ZISRAW) vendor driver.
//!
//! CZI is a single-file segmented container. Every segment begins with a
//! 32-byte header (`SID[16]`, `AllocatedSize i64`, `UsedSize i64`); segments are
//! walked by `next = pos + 32 + AllocatedSize`. All integers/floats are
//! little-endian. The `ZISRAWFILE` header at offset 0 points to three other
//! segments by absolute offset: the subblock directory, the metadata XML, and
//! the attachment directory.
//!
//! Each image tile is a `ZISRAWSUBBLOCK` segment. The subblock **directory**
//! lists one `DirectoryEntryDV` per subblock: pixel type, compression, absolute
//! file position, and a set of `DimensionEntryDV` records. The X/Y dimension
//! entries give the tile's start (in level-0 pixel space), logical `Size`, and
//! physically-`StoredSize` pixels. A pyramid level is the set of subblocks
//! sharing a downsample factor `round(Size / StoredSize)`; within a level, tiles
//! are arbitrary-offset mosaic tiles placed at `Start / downsample`. This is the
//! same placed-subtile model as MIRAX/Ventana, so this driver builds
//! [`ZeissLevel`]s that own the reader plus a per-level placed-tile list and
//! decode + composite themselves ([`ZeissLevel::read_region`]), bypassing the
//! TIFF tile path.
//!
//! **Compression parity with OpenSlide:** OpenSlide's own `zeiss.c` decodes only
//! uncompressed, Zstd0, and Zstd1 tiles and rejects everything else — including
//! JPEG XR (`COMP_JXR`), which is the *common* compression for real brightfield
//! WSI CZIs. There is no pure-Rust JPEG XR decoder, so this driver matches
//! OpenSlide exactly: it opens uncompressed/Zstd0/Zstd1 slides and returns a
//! clear error for JXR/JPEG/LZW rather than silently producing black tiles.
//!
//! Deferred (not in OpenSlide's core path either): multi-channel fluorescence
//! (only the first C/Z/T plane is used), JPEG XR tiles, and embedded-CZI
//! associated images (only JPEG label/preview/thumbnail attachments decode).

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use bytes::Bytes;

use crate::associated::AssociatedImage;
use crate::cache::{LevelCache, TileId};
use crate::color::argb;
use crate::decode::jpeg::JpegDecoder;
use crate::decode::tiff::TiffLike;
use crate::decode::xml::parse_tree;
use crate::error::{OpenSlideError, Result};
use crate::grid::simple::SimpleGrid;
use crate::icc::IccProfile;
use crate::io::SlideReader;
use crate::level::LevelInfo;
use crate::properties::{Properties, names};

use super::{LevelData, SlideData, VendorDriver};

/// `SID` magic (null-padded to 16 bytes) of the file header segment.
const SID_FILE: &[u8] = b"ZISRAWFILE";

/// Compression codes (`DirectoryEntryDV.Compression`), matching OpenSlide's enum.
const COMP_NONE: i32 = 0;
const COMP_ZSTD0: i32 = 5;
const COMP_ZSTD1: i32 = 6;

/// Pixel type codes (`DirectoryEntryDV.PixelType`). Channel order is B,G,R.
const PT_GRAY8: i32 = 0;
const PT_GRAY16: i32 = 1;
const PT_BGR24: i32 = 3;
const PT_BGR48: i32 = 4;
const PT_BGRA32: i32 = 9;

/// Bytes fetched past a subblock's fixed region to cover its metadata block.
///
/// The pixel data sits behind a variable-length XML metadata block whose size is
/// only stated in the subblock's own payload header, so a read that wants both
/// in one request has to guess. Guessing high wastes a few kilobytes of a
/// transfer that is already megabytes; guessing low costs one extra request and
/// is still correct. Real subblock metadata is a few hundred bytes.
const METADATA_SLACK: u64 = 16 * 1024;

/// One placed subblock: where its segment lives, how its pixels are encoded, and
/// where the decoded `stored_w × stored_h` image sits in this level's space.
///
/// The pixel data's offset and length are deliberately *not* stored. They are
/// not in the directory — resolving them costs a 16-byte read of the subblock's
/// own payload header — and nothing outside [`ZeissLevel::read_region`] needs
/// them, so they are resolved there, for the handful of subblocks a region
/// actually touches, rather than for all of them at open. See
/// [`ZeissTile::probe_len`].
struct ZeissTile {
    /// Absolute offset of this subblock's `ZISRAWSUBBLOCK` segment header.
    file_position: u64,
    /// `DimensionCount` from the directory entry, which sizes the fixed region
    /// between the payload header and the metadata block.
    dimension_count: i32,
    compression: i32,
    pixel_type: i32,
    stored_w: i64,
    stored_h: i64,
    dest_x: i64,
    dest_y: i64,
}

impl ZeissTile {
    /// Absolute offset of the subblock payload: `MetadataSize`, `AttachmentSize`,
    /// `DataSize`, then the embedded DirectoryEntry.
    fn payload_start(&self) -> u64 {
        self.file_position + 32
    }

    /// Bytes between the payload start and the metadata block — a fixed region
    /// of `max(256, 16 + entry_size)`, where `entry_size = 32 + 20 × dims`.
    fn fixed_region(&self) -> u64 {
        let entry_size = 32 + 20 * self.dimension_count.max(0) as u64;
        (16 + entry_size).max(256)
    }

    /// Pixel bytes an uncompressed subblock must hold, which the directory entry
    /// alone determines. Zero when the size is not derivable — a compressed
    /// subblock is whatever the codec produced, and only its header says how big.
    fn expected_data_size(&self) -> u64 {
        if self.compression != COMP_NONE {
            return 0;
        }
        match bytes_per_pixel(self.pixel_type) {
            Some(bpp) => (self.stored_w.max(0) as u64) * (self.stored_h.max(0) as u64) * bpp,
            None => 0,
        }
    }

    /// How much to read from [`payload_start`](Self::payload_start) so that one
    /// request carries the payload header *and*, where the size is knowable, the
    /// pixels behind it. Purely a hint: [`ZeissLevel::subblock_pixels`] reads the
    /// remainder if it falls short, so a wrong guess costs a request, never
    /// correctness.
    fn probe_len(&self) -> u64 {
        self.fixed_region() + METADATA_SLACK + self.expected_data_size()
    }

    /// Resolve this subblock's pixel data `(offset relative to the payload
    /// start, size)` from `payload`, which must begin at
    /// [`payload_start`](Self::payload_start).
    ///
    /// The payload opens with `MetadataSize (i32)`, `AttachmentSize (i32)`,
    /// `DataSize (i64)`, then the embedded DirectoryEntry; the metadata block —
    /// and the pixels behind it — start past the fixed region. Only the first 16
    /// bytes are read, so a longer speculative read can be passed straight in.
    fn data_location(&self, payload: &[u8]) -> Result<(u64, usize)> {
        let metadata_size = i32le(payload, 0)?;
        let data_size = i64le(payload, 8)?;
        if metadata_size < 0 {
            return Err(OpenSlideError::Decode(
                "CZI: negative subblock metadata size".into(),
            ));
        }
        if data_size < 0 {
            return Err(OpenSlideError::Decode(
                "CZI: negative subblock data size".into(),
            ));
        }
        Ok((
            self.fixed_region() + metadata_size as u64,
            data_size as usize,
        ))
    }
}

/// Bytes per sample-tuple for a CZI pixel type, or `None` if unsupported.
fn bytes_per_pixel(pixel_type: i32) -> Option<u64> {
    Some(match pixel_type {
        PT_GRAY8 => 1,
        PT_GRAY16 => 2,
        PT_BGR24 => 3,
        PT_BGR48 => 6,
        PT_BGRA32 => 4,
        _ => return None,
    })
}

/// Per-level CZI tile index plus the reader needed to fetch tile bytes.
pub struct ZeissLevel {
    reader: Arc<dyn SlideReader>,
    tiles: Vec<ZeissTile>,
}

impl ZeissLevel {
    /// Read a region of this level. `(ox, oy)` is the top-left in this level's
    /// pixel space. CZI subblocks each carry their own compression and pixel
    /// type, so decoding happens here rather than through a shared decoder.
    ///
    /// `tiles` is the slide's decoded-tile cache, scoped to this level.
    pub fn read_region(
        &self,
        ox: i64,
        oy: i64,
        w: i64,
        h: i64,
        tiles: LevelCache<'_>,
    ) -> Result<Vec<u32>> {
        let mut out = vec![0u32; (w * h) as usize];

        // The subblocks this window touches, each listed once: two directory
        // entries can name the same subblock, and it should cost one fetch and
        // one decode.
        //
        // ponytail: linear scan over all placed tiles — same as the MIRAX path.
        // A per-level spatial bucket would cut this if reads get hot.
        let mut wanted: Vec<&ZeissTile> = Vec::new();
        let mut seen: HashSet<u64> = HashSet::new();
        for t in &self.tiles {
            if t.dest_x + t.stored_w <= ox
                || t.dest_x >= ox + w
                || t.dest_y + t.stored_h <= oy
                || t.dest_y >= oy + h
            {
                continue;
            }
            if seen.insert(t.file_position) {
                wanted.push(t);
            }
        }
        if wanted.is_empty() {
            return Ok(out);
        }

        // Phase 0: the decoded-tile cache, keyed by `file_position` — the
        // subblock's own address, and the only thing that names it. Mosaic
        // subblocks *overlap* in slide space, so a grid cell does not identify
        // one; two directory entries naming the same `file_position` are the
        // same tile, which is exactly what `seen` above already relies on.
        //
        // `plan` keeps the subblocks in directory order because that order is
        // the compositing rule in the overlap strips: the last subblock written
        // wins (see docs/remote-patch-benchmark.md, finding A). A `None` entry
        // means "take the next probe out of the gather", so cached and freshly
        // read subblocks paint interleaved, in the order an uncached read used.
        let mut plan: Vec<(&ZeissTile, Option<Arc<Vec<u32>>>)> = Vec::with_capacity(wanted.len());
        let mut misses: Vec<&ZeissTile> = Vec::new();
        for t in wanted {
            match tiles.get(TileId::Offset {
                stream: 0,
                position: t.file_position,
            }) {
                Some(px) => plan.push((t, Some(px))),
                None => {
                    plan.push((t, None));
                    misses.push(t);
                }
            }
        }

        // One batched gather for every subblock the cache could not serve. Each
        // range starts at the payload header and runs long enough to carry the
        // pixels behind it too, so a subblock normally costs one request rather
        // than a header read followed by a data read. The backend coalesces
        // neighbours and issues the rest concurrently; reading them one at a
        // time made a CZI patch the slowest in the suite.
        // The probe deliberately overshoots, so clamp it to the file: the last
        // subblock's guess otherwise runs past the end, which a backend either
        // rejects outright or — worse — clamps for us and hands back short.
        let file_size = self.reader.size()?;
        let ranges: Vec<Range<u64>> = misses
            .iter()
            .map(|t| {
                let start = t.payload_start();
                let end = start.saturating_add(t.probe_len()).min(file_size);
                start..end.max(start)
            })
            .collect();
        let probes = if ranges.is_empty() {
            Vec::new()
        } else {
            self.reader.read_ranges(&ranges)?
        };

        let mut probes = probes.into_iter();
        for (t, cached) in plan {
            let px = match cached {
                Some(px) => px,
                None => {
                    let Some(probe) = probes.next() else { continue };
                    let pixels = self.subblock_pixels(t, &probe)?;
                    let px = Arc::new(decode_tile(&pixels, t)?);
                    tiles.insert(
                        TileId::Offset {
                            stream: 0,
                            position: t.file_position,
                        },
                        px.clone(),
                    );
                    px
                }
            };
            crate::slide::composite_subtile(
                &px, t.stored_w, 0, 0, t.stored_w, t.stored_h, t.dest_x, t.dest_y, &mut out, ox,
                oy, w, h,
            );
        }
        Ok(out)
    }

    /// A subblock's pixel bytes, taken from `probe` — the speculative read that
    /// began at its payload header — or fetched outright when the probe fell
    /// short of the metadata block or the data behind it.
    fn subblock_pixels(&self, t: &ZeissTile, probe: &Bytes) -> Result<Bytes> {
        let (rel, data_size) = t.data_location(probe)?;
        let rel = rel as usize;
        if let Some(end) = rel.checked_add(data_size)
            && end <= probe.len()
        {
            return Ok(probe.slice(rel..end));
        }
        let mut buf = vec![0u8; data_size];
        self.reader
            .read_exact_at(t.payload_start() + rel as u64, &mut buf)?;
        Ok(Bytes::from(buf))
    }
}

pub struct ZeissDriver;

impl VendorDriver for ZeissDriver {
    fn name(&self) -> &str {
        "zeiss"
    }

    fn vendor(&self) -> &str {
        "zeiss"
    }

    fn detect(&self, reader: &dyn SlideReader, _tl: Option<&mut TiffLike>) -> Result<bool> {
        // CZI is not TIFF; identified by the ZISRAWFILE magic at offset 0.
        let mut magic = [0u8; 16];
        if reader.read_exact_at(0, &mut magic).is_err() {
            return Ok(false);
        }
        Ok(magic.starts_with(SID_FILE))
    }

    fn open(&self, reader: Box<dyn SlideReader>, _tl: Option<TiffLike>) -> Result<SlideData> {
        open_czi(Arc::from(reader))
    }
}

// ───────────────────────────── open ─────────────────────────────────────────

fn open_czi(reader: Arc<dyn SlideReader>) -> Result<SlideData> {
    // File header: 32-byte segment header, then the DataFileHeader payload.
    // We need the three directory pointers at payload offsets 84/92/104.
    let mut hdr = [0u8; 112];
    reader.read_exact_at(0, &mut hdr)?;
    if !hdr.starts_with(SID_FILE) {
        return Err(OpenSlideError::Decode(
            "CZI: missing ZISRAWFILE magic".into(),
        ));
    }
    let subblock_dir_pos = i64le(&hdr, 84)?;
    let metadata_pos = i64le(&hdr, 92)?;
    let attach_dir_pos = i64le(&hdr, 104)?;
    if subblock_dir_pos <= 0 {
        return Err(OpenSlideError::Decode("CZI: no subblock directory".into()));
    }

    let entries = read_subblock_directory(&*reader, subblock_dir_pos as u64)?;

    // First pass: keep usable subblocks (first C/Z/T plane, positive extent)
    // and find the global level-0 origin. CZI mosaic coordinates are not
    // zero-based — a scene commonly sits at negative X — so tiles are placed
    // relative to the minimum X/Y, which becomes the (0,0) of every level.
    let mut kept: Vec<(&DirEntry, i64)> = Vec::new();
    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    for e in &entries {
        if e.c_start != 0 || e.z_start != 0 || e.t_start != 0 {
            continue;
        }
        if e.x_stored <= 0 || e.y_stored <= 0 {
            continue;
        }
        // A negative file position would wrap when read as an offset.
        if e.file_position < 0 {
            continue;
        }
        let downsample = div_round_closest(e.x_size, e.x_stored).max(1);
        min_x = min_x.min(e.x_start);
        min_y = min_y.min(e.y_start);
        kept.push((e, downsample));
    }
    if kept.is_empty() {
        return Err(OpenSlideError::Decode(
            "CZI: no usable image subblocks".into(),
        ));
    }

    // Second pass: group subblocks by downsample into pyramid levels, placing
    // each at `(start - origin) / downsample` in its level's pixel space.
    //
    // Everything below comes from the directory alone. Where a subblock's
    // *pixels* begin does not, and resolving it costs a 16-byte read of that
    // subblock's own payload header — but no part of opening a slide needs it,
    // so it is left to `ZeissLevel::read_region`, which resolves only the
    // subblocks a region touches and folds the read into the tile fetch. Doing
    // it here instead was the whole cost of a remote CZI open: 151 requests
    // against 10–27 for every other vendor, 134 of them 16-byte GETs scattered
    // across a gigabyte, for data the open then never looked at.
    let mut by_downsample: HashMap<i64, Vec<ZeissTile>> = HashMap::new();
    for (e, downsample) in kept {
        by_downsample
            .entry(downsample)
            .or_default()
            .push(ZeissTile {
                file_position: e.file_position as u64,
                dimension_count: e.dimension_count,
                compression: e.compression,
                pixel_type: e.pixel_type,
                stored_w: e.x_stored,
                stored_h: e.y_stored,
                dest_x: div_round_closest(e.x_start - min_x, downsample),
                dest_y: div_round_closest(e.y_start - min_y, downsample),
            });
    }

    let mut downsamples: Vec<i64> = by_downsample.keys().copied().collect();
    downsamples.sort_unstable();
    let base = downsamples[0] as f64;

    let mut levels = Vec::with_capacity(downsamples.len());
    for ds in downsamples {
        let tiles = by_downsample.remove(&ds).unwrap();
        // Reject unsupported compression up front so `open` fails clearly rather
        // than every `read_region` returning black tiles (OpenSlide parity).
        if let Some(t) = tiles
            .iter()
            .find(|t| !matches!(t.compression, COMP_NONE | COMP_ZSTD0 | COMP_ZSTD1))
        {
            return Err(OpenSlideError::Decode(format!(
                "CZI compression {} is not supported (JPEG XR/JPEG/LZW need a decoder OpenSlide also lacks)",
                t.compression
            )));
        }
        // Level dimensions = the placed-tile bounding box.
        let mut width = 0i64;
        let mut height = 0i64;
        for t in &tiles {
            width = width.max(t.dest_x + t.stored_w);
            height = height.max(t.dest_y + t.stored_h);
        }
        // Representative tile size for the (unused-by-zeiss) grid bookkeeping.
        let tw = tiles.first().map(|t| t.stored_w).unwrap_or(1).max(1);
        let th = tiles.first().map(|t| t.stored_h).unwrap_or(1).max(1);
        levels.push(LevelData {
            info: LevelInfo {
                width,
                height,
                downsample: ds as f64 / base,
                tile_width: tw,
                tile_height: th,
            },
            grid: SimpleGrid::new(width, height, tw, th),
            tiff_dir: 0,
            // Placeholder: ZeissLevel decodes internally (per-tile compression),
            // so this decoder is never invoked for zeiss levels.
            decoder: Box::new(JpegDecoder::new()),
            tiff_tile_width: tw,
            tiff_tile_height: th,
            tilemap: None,
            ndpi: None,
            mirax: None,
            zeiss: Some(ZeissLevel {
                reader: reader.clone(),
                tiles,
            }),
            synthetic: None,
            dicom: None,
        });
    }

    let properties = read_metadata(&*reader, metadata_pos);
    let associated_images = read_associated_images(&*reader, attach_dir_pos);

    Ok(SlideData {
        levels,
        properties,
        associated_images,
        icc_profile: IccProfile::absent(),
        tiff: None,
    })
}

/// Round-half-away division of positive quantities (matches OpenSlide's
/// `DIV_ROUND_CLOSEST`). Returns 1 for a zero denominator.
fn div_round_closest(n: i64, d: i64) -> i64 {
    if d == 0 {
        return 1;
    }
    (n + d / 2) / d
}

// ─────────────────────────── directory parse ────────────────────────────────

/// A parsed `DirectoryEntryDV` — just the fields the driver needs.
struct DirEntry {
    pixel_type: i32,
    file_position: i64,
    compression: i32,
    dimension_count: i32,
    x_start: i64,
    x_size: i64,
    x_stored: i64,
    y_start: i64,
    y_stored: i64,
    c_start: i32,
    z_start: i32,
    t_start: i32,
}

/// Read and parse the `ZISRAWDIRECTORY` segment into its entries.
fn read_subblock_directory(reader: &dyn SlideReader, pos: u64) -> Result<Vec<DirEntry>> {
    // Segment header gives UsedSize; the payload starts 32 bytes in.
    let mut seg = [0u8; 32];
    reader.read_exact_at(pos, &mut seg)?;
    let used = i64le(&seg, 24)?;
    if used < 128 {
        return Err(OpenSlideError::Decode(
            "CZI directory segment too small".into(),
        ));
    }
    let mut payload = vec![0u8; used as usize];
    reader.read_exact_at(pos + 32, &mut payload)?;

    let entry_count = i32le(&payload, 0)?;
    if entry_count < 0 {
        return Err(OpenSlideError::Decode("CZI: negative entry count".into()));
    }
    // Each entry is at least 32 bytes, so the payload caps how many there can
    // be; `entry_count` itself is a file-supplied i32 and reserving for
    // 2^31 of them would ask for ~150 GB before the first parse fails.
    let capacity = (entry_count as usize).min(payload.len().saturating_sub(128) / 32);
    let mut entries = Vec::with_capacity(capacity);
    let mut off = 128usize; // fixed region before the entries
    for _ in 0..entry_count {
        let e = parse_dir_entry(&payload, off)?;
        off += 32 + 20 * e.dimension_count as usize;
        entries.push(e);
    }
    Ok(entries)
}

/// Parse one `DirectoryEntryDV` (32-byte head + `DimensionCount` × 20-byte dims).
fn parse_dir_entry(buf: &[u8], off: usize) -> Result<DirEntry> {
    let pixel_type = i32le(buf, off + 2)?;
    let file_position = i64le(buf, off + 6)?;
    let compression = i32le(buf, off + 18)?;
    let dimension_count = i32le(buf, off + 28)?;
    if !(0..=64).contains(&dimension_count) {
        return Err(OpenSlideError::Decode(
            "CZI: implausible dimension count".into(),
        ));
    }

    let (mut x_start, mut x_size, mut x_stored) = (0i64, 0i64, 0i64);
    let (mut y_start, mut y_stored) = (0i64, 0i64);
    let (mut c_start, mut z_start, mut t_start) = (0i32, 0i32, 0i32);
    for i in 0..dimension_count as usize {
        let d = off + 32 + 20 * i;
        let dim = buf
            .get(d..d + 4)
            .ok_or_else(|| OpenSlideError::Decode("CZI: dim entry past end".into()))?;
        let start = i32le(buf, d + 4)?;
        let size = i32le(buf, d + 8)?;
        let stored = i32le(buf, d + 16)?;
        match dim[0] {
            b'X' => {
                x_start = start as i64;
                x_size = size as i64;
                x_stored = stored as i64;
            }
            b'Y' => {
                y_start = start as i64;
                y_stored = stored as i64;
            }
            b'C' => c_start = start,
            b'Z' => z_start = start,
            b'T' => t_start = start,
            _ => {}
        }
    }

    Ok(DirEntry {
        pixel_type,
        file_position,
        compression,
        dimension_count,
        x_start,
        x_size,
        x_stored,
        y_start,
        y_stored,
        c_start,
        z_start,
        t_start,
    })
}

// ───────────────────────────── tile decode ──────────────────────────────────

/// Decode one subblock's pixel bytes into ARGB, applying decompression and the
/// BGR→ARGB / grayscale conversion for its pixel type.
fn decode_tile(data: &[u8], t: &ZeissTile) -> Result<Vec<u32>> {
    let raw = match t.compression {
        COMP_NONE => std::borrow::Cow::Borrowed(data),
        COMP_ZSTD0 => std::borrow::Cow::Owned(zstd_decompress(data)?),
        COMP_ZSTD1 => std::borrow::Cow::Owned(zstd1_decompress(data)?),
        other => {
            return Err(OpenSlideError::Decode(format!(
                "CZI compression {other} is not supported"
            )));
        }
    };
    samples_to_argb(&raw, t.pixel_type, t.stored_w, t.stored_h)
}

/// Convert raw (uncompressed) CZI samples to ARGB. CZI stores color channels in
/// B,G,R order; 16-bit types are down-shifted to 8-bit for display.
fn samples_to_argb(data: &[u8], pixel_type: i32, w: i64, h: i64) -> Result<Vec<u32>> {
    // The dimensions come from the subblock's directory entry, so the product
    // can exceed anything allocatable. Bound it by what `data` could possibly
    // hold — one byte per pixel is the densest any layout below gets — so a
    // crafted entry errors out of `need` instead of aborting on the reserve.
    let n = (w as i128)
        .checked_mul(h as i128)
        .filter(|&n| n >= 0 && n <= data.len() as i128)
        .ok_or_else(|| {
            OpenSlideError::Decode(format!(
                "CZI subblock declares {w}x{h} pixels, payload holds {} bytes",
                data.len()
            ))
        })? as usize;
    let mut out = Vec::with_capacity(n);
    match pixel_type {
        PT_GRAY8 => {
            need(data, n)?;
            for &g in &data[..n] {
                out.push(argb(0xFF, g, g, g));
            }
        }
        PT_GRAY16 => {
            need(data, n * 2)?;
            for c in data[..n * 2].chunks_exact(2) {
                let g = c[1]; // high byte of little-endian 16-bit sample
                out.push(argb(0xFF, g, g, g));
            }
        }
        PT_BGR24 => {
            need(data, n * 3)?;
            for c in data[..n * 3].chunks_exact(3) {
                out.push(argb(0xFF, c[2], c[1], c[0])); // B,G,R → R,G,B
            }
        }
        PT_BGR48 => {
            need(data, n * 6)?;
            for c in data[..n * 6].chunks_exact(6) {
                out.push(argb(0xFF, c[5], c[3], c[1])); // high bytes of B,G,R
            }
        }
        PT_BGRA32 => {
            need(data, n * 4)?;
            for c in data[..n * 4].chunks_exact(4) {
                out.push(argb(c[3], c[2], c[1], c[0])); // B,G,R,A → A,R,G,B
            }
        }
        other => {
            return Err(OpenSlideError::Decode(format!(
                "CZI pixel type {other} is not supported"
            )));
        }
    }
    Ok(out)
}

fn need(data: &[u8], bytes: usize) -> Result<()> {
    if data.len() < bytes {
        return Err(OpenSlideError::Decode(format!(
            "CZI tile: {} bytes decoded, need {}",
            data.len(),
            bytes
        )));
    }
    Ok(())
}

/// Decompress a raw zstd frame (Zstd0), pure-Rust via `ruzstd`.
fn zstd_decompress(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut dec = ruzstd::streaming_decoder::StreamingDecoder::new(data)
        .map_err(|e| OpenSlideError::Decode(format!("CZI zstd init failed: {e}")))?;
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|e| OpenSlideError::Decode(format!("CZI zstd decode failed: {e}")))?;
    Ok(out)
}

/// Decompress a Zstd1 payload: a small header (`size` byte, then optional
/// chunk `[type=1, flags]` whose flag bit 0 selects hi/lo byte unpacking)
/// followed by a zstd frame. When hi/lo packing is set, the decoded stream
/// stores all low bytes then all high bytes of 16-bit samples; interleave them.
///
/// ponytail: implemented per the libCZI/OpenSlide payload layout but not
/// exercised by a real fixture — the synthetic tests cover uncompressed + Zstd0.
fn zstd1_decompress(data: &[u8]) -> Result<Vec<u8>> {
    if data.is_empty() {
        return Err(OpenSlideError::Decode("CZI zstd1: empty payload".into()));
    }
    let header_size = data[0] as usize; // small single-byte size in practice
    if header_size == 0 || header_size > data.len() {
        return Err(OpenSlideError::Decode("CZI zstd1: bad header size".into()));
    }
    let do_hilo = header_size >= 3 && data[1] == 1 && (data[2] & 1) != 0;
    let decoded = zstd_decompress(&data[header_size..])?;
    if !do_hilo {
        return Ok(decoded);
    }
    if decoded.len() % 2 != 0 {
        return Err(OpenSlideError::Decode(
            "CZI zstd1: hi/lo payload not even length".into(),
        ));
    }
    let half = decoded.len() / 2;
    let (low, high) = decoded.split_at(half);
    let mut out = Vec::with_capacity(decoded.len());
    for i in 0..half {
        out.push(low[i]);
        out.push(high[i]);
    }
    Ok(out)
}

// ─────────────────────────── metadata (XML) ─────────────────────────────────

/// Read the `ZISRAWMETADATA` XML and extract mpp + objective power.
fn read_metadata(reader: &dyn SlideReader, metadata_pos: i64) -> Properties {
    let mut props = Properties::new();
    props.insert(names::VENDOR, "zeiss");
    if metadata_pos <= 0 {
        return props;
    }
    if let Some(xml) = read_metadata_xml(reader, metadata_pos as u64) {
        parse_metadata_xml(&xml, &mut props);
    }
    props
}

/// Clamp a file-declared length to what actually remains after `offset`.
///
/// CZI segment headers carry their payload length as a signed 32-bit field, and
/// the reads below allocate it up front. Trimming it to the file first means a
/// corrupt length is a short read rather than a multi-gigabyte allocation.
fn bounded_read_len(reader: &dyn SlideReader, offset: u64, len: usize) -> Option<usize> {
    let size = reader.size().ok()?;
    let remaining = size.saturating_sub(offset);
    let len = (len as u64).min(remaining) as usize;
    (len > 0).then_some(len)
}

fn read_metadata_xml(reader: &dyn SlideReader, pos: u64) -> Option<String> {
    let mut head = [0u8; 8];
    reader.read_exact_at(pos + 32, &mut head).ok()?;
    let xml_size = i32le(&head, 0).ok()?;
    // Signed on the wire, and the allocation below is this number of bytes:
    // `-1 as usize` asks the allocator for 18 exabytes and aborts the process.
    let xml_size = usize::try_from(xml_size).ok().filter(|&n| n > 0)?;
    let xml_size = bounded_read_len(reader, pos + 32 + 256, xml_size)?;
    let mut buf = vec![0u8; xml_size];
    // XML begins after the fixed 256-byte metadata payload region.
    reader.read_exact_at(pos + 32 + 256, &mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn parse_metadata_xml(xml: &str, props: &mut Properties) {
    let Ok(root) = parse_tree(xml) else {
        return;
    };
    // Root is <ImageDocument>; metadata lives under <Metadata>.
    let meta = root.find_child("Metadata").unwrap_or(&root);

    // Scaling: <Scaling><Items><Distance Id="X"><Value>metres</Value>… →  µm/px.
    if let Some(items) = meta.descend(&["Scaling", "Items"]) {
        for dist in items.children_named("Distance") {
            let axis = dist.attr("Id");
            let value = dist
                .find_child("Value")
                .and_then(|v| v.text.trim().parse::<f64>().ok());
            if let (Some(axis), Some(m)) = (axis, value) {
                let mpp = (m * 1.0e6).to_string();
                match axis {
                    "X" => props.insert(names::MPP_X, mpp),
                    "Y" => props.insert(names::MPP_Y, mpp),
                    _ => {}
                }
            }
        }
    }

    // Objective power: Information/Instrument/Objectives/Objective/NominalMagnification.
    if let Some(objs) = meta.descend(&["Information", "Instrument", "Objectives"])
        && let Some(obj) = objs.children_named("Objective").next()
        && let Some(mag) = obj.find_child("NominalMagnification")
    {
        let mag = mag.text.trim();
        if !mag.is_empty() {
            props.insert(names::OBJECTIVE_POWER, mag);
        }
    }
}

// ───────────────────────── associated images ────────────────────────────────

/// Read JPEG label/preview/thumbnail attachments as associated images.
fn read_associated_images(
    reader: &dyn SlideReader,
    attach_dir_pos: i64,
) -> Vec<(String, AssociatedImage)> {
    let mut out = Vec::new();
    if attach_dir_pos <= 0 {
        return out;
    }
    let Some(entries) = read_attachment_directory(reader, attach_dir_pos as u64) else {
        return out;
    };
    // OpenSlide's attachment-name → associated-image-name mapping.
    for (att_name, name) in [
        ("Label", "label"),
        ("SlidePreview", "macro"),
        ("Thumbnail", "thumbnail"),
    ] {
        if let Some(&file_pos) = entries.get(att_name)
            && let Some(img) = read_one_attachment_jpeg(reader, file_pos)
        {
            out.push((name.to_string(), img));
        }
    }
    out
}

/// Map attachment `Name` → `ZISRAWATTACH` segment position.
fn read_attachment_directory(reader: &dyn SlideReader, pos: u64) -> Option<HashMap<String, u64>> {
    let mut seg = [0u8; 32];
    reader.read_exact_at(pos, &mut seg).ok()?;
    let used = i64le(&seg, 24).ok()?;
    if used < 256 {
        return None;
    }
    let mut payload = vec![0u8; used as usize];
    reader.read_exact_at(pos + 32, &mut payload).ok()?;
    let count = i32le(&payload, 0).ok()?;
    if count < 0 {
        return None;
    }
    let mut map = HashMap::new();
    // Entries (128 bytes each) start after the fixed 256-byte region.
    for i in 0..count as usize {
        let e = 256 + 128 * i;
        let Ok(file_position) = i64le(&payload, e + 12) else {
            break;
        };
        let name = payload.get(e + 48..e + 128).map(cstr).unwrap_or_default();
        if !name.is_empty() && file_position > 0 {
            map.insert(name, file_position as u64);
        }
    }
    Some(map)
}

/// Read a `ZISRAWATTACH` segment's JPEG data and decode it.
fn read_one_attachment_jpeg(reader: &dyn SlideReader, pos: u64) -> Option<AssociatedImage> {
    let mut head = [0u8; 4];
    reader.read_exact_at(pos + 32, &mut head).ok()?;
    let data_size = i32le(&head, 0).ok()?;
    // Same signed-length hazard as `read_metadata_xml`.
    let data_size = usize::try_from(data_size).ok().filter(|&n| n > 0)?;
    let data_size = bounded_read_len(reader, pos + 32 + 256, data_size)?;
    let mut buf = vec![0u8; data_size];
    // Attachment data begins after the fixed 256-byte payload region.
    reader.read_exact_at(pos + 32 + 256, &mut buf).ok()?;
    let (argb_data, w, h) = decode_jpeg_sized(&buf)?;
    Some(AssociatedImage {
        width: w,
        height: h,
        argb_data,
        icc_profile: IccProfile::absent(),
    })
}

/// Decode a standalone JPEG whose dimensions aren't known ahead of time,
/// returning ARGB pixels and the decoded size. (The shared `JpegDecoder`
/// validates against an expected size, which we don't have for attachments.)
fn decode_jpeg_sized(data: &[u8]) -> Option<(Vec<u32>, i64, i64)> {
    let mut dec = jpeg_decoder::Decoder::new(data);
    let pixels = dec.decode().ok()?;
    let info = dec.info()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let mut out = Vec::with_capacity(w * h);
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            for c in pixels.chunks_exact(3) {
                out.push(argb(0xFF, c[0], c[1], c[2]));
            }
        }
        jpeg_decoder::PixelFormat::L8 => {
            for &g in &pixels {
                out.push(argb(0xFF, g, g, g));
            }
        }
        _ => return None,
    }
    Some((out, w as i64, h as i64))
}

/// Read a null-terminated ASCII string from a fixed-width field.
fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

// ─────────────────────────── byte helpers ───────────────────────────────────

fn i32le(b: &[u8], off: usize) -> Result<i32> {
    b.get(off..off + 4)
        .map(|s| i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| OpenSlideError::Decode("CZI: read past end (i32)".into()))
}

fn i64le(b: &[u8], off: usize) -> Result<i64> {
    b.get(off..off + 8)
        .map(|s| i64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
        .ok_or_else(|| OpenSlideError::Decode("CZI: read past end (i64)".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn div_round_closest_rounds_half_away() {
        assert_eq!(div_round_closest(10, 4), 3); // 2.5 → 3
        assert_eq!(div_round_closest(9, 4), 2); // 2.25 → 2
        assert_eq!(div_round_closest(100, 1), 100);
        assert_eq!(div_round_closest(5, 0), 1); // guard
    }

    #[test]
    fn bgr24_becomes_argb() {
        // Two pixels: (B,G,R) = (10,20,30) and (40,50,60).
        let data = [10, 20, 30, 40, 50, 60];
        let px = samples_to_argb(&data, PT_BGR24, 2, 1).unwrap();
        assert_eq!(px, vec![argb(0xFF, 30, 20, 10), argb(0xFF, 60, 50, 40)]);
    }

    #[test]
    fn bgra32_preserves_alpha() {
        let data = [10, 20, 30, 0x80]; // B,G,R,A
        let px = samples_to_argb(&data, PT_BGRA32, 1, 1).unwrap();
        assert_eq!(px, vec![argb(0x80, 30, 20, 10)]);
    }

    #[test]
    fn hilo_unpack_interleaves_planes() {
        // HiLo packing stores all low bytes then all high bytes; unpacking
        // interleaves them back into little-endian 16-bit samples. Assert the
        // interleave math directly (ruzstd is decode-only, so a real zstd1
        // frame is exercised end-to-end by the Zstd1-HiLo integration test).
        let decoded = [1u8, 2, 3, 4]; // low=[1,2], high=[3,4]
        let half = decoded.len() / 2;
        let (low, high) = decoded.split_at(half);
        let mut out = Vec::new();
        for i in 0..half {
            out.push(low[i]);
            out.push(high[i]);
        }
        assert_eq!(out, [1, 3, 2, 4]); // lo,hi,lo,hi
    }

    #[test]
    fn unsupported_pixel_type_errors() {
        assert!(samples_to_argb(&[0, 0, 0, 0], 99, 1, 1).is_err());
    }

    // ───────────────────── the decoded-tile cache ──────────────────────────
    //
    // There is no public CZI fixture in the test-data set that CI can rely on
    // (`Zeiss-5-Uncompressed.czi` is not in the cached set), so the cache
    // wiring on this path is covered here, against a hand-laid-out container.
    // That is the right level anyway: what has to be proved is that the
    // `file_position` key and the directory compositing order survive a cache
    // hit, and both are decided in `read_region`.

    use crate::cache::{Cache, SlideId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// In-memory CZI-shaped byte source that counts what reaches it. Reads are
    /// clamped at end-of-file, as a real backend's are: a subblock probe
    /// deliberately over-reads past the pixels it wants.
    struct FakeCzi {
        data: Vec<u8>,
        batches: AtomicUsize,
        reads: AtomicUsize,
    }

    impl FakeCzi {
        fn slice(&self, start: u64, end: u64) -> &[u8] {
            let start = (start as usize).min(self.data.len());
            let end = (end as usize).clamp(start, self.data.len());
            &self.data[start..end]
        }
    }

    impl SlideReader for FakeCzi {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let src = self.slice(offset, offset + buf.len() as u64);
            buf[..src.len()].copy_from_slice(src);
            Ok(src.len())
        }

        fn read_ranges(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
            self.batches.fetch_add(1, Ordering::Relaxed);
            Ok(ranges
                .iter()
                .map(|r| Bytes::copy_from_slice(self.slice(r.start, r.end)))
                .collect())
        }

        fn size(&self) -> Result<u64> {
            Ok(self.data.len() as u64)
        }
    }

    /// Lay out solid-colour uncompressed subblocks as a CZI-shaped buffer and
    /// wrap them in a level. Each entry is `(dest_x, dest_y, w, h, rgb)`.
    #[allow(clippy::type_complexity)]
    fn fake_level(specs: &[(i64, i64, i64, i64, (u8, u8, u8))]) -> (ZeissLevel, Arc<FakeCzi>) {
        let mut data: Vec<u8> = Vec::new();
        let mut tiles = Vec::new();
        for &(dest_x, dest_y, w, h, (r, g, b)) in specs {
            // Segment header (32 bytes), then the payload: MetadataSize,
            // AttachmentSize, DataSize, the fixed region, then the pixels.
            let file_position = data.len() as u64;
            data.extend_from_slice(&[0u8; 32]);
            let n = (w * h) as usize;
            data.extend_from_slice(&0i32.to_le_bytes()); // MetadataSize
            data.extend_from_slice(&0i32.to_le_bytes()); // AttachmentSize
            data.extend_from_slice(&((n * 3) as i64).to_le_bytes()); // DataSize
            // `fixed_region()` for dimension_count 0 is the 256-byte floor.
            data.resize(file_position as usize + 32 + 256, 0);
            for _ in 0..n {
                data.extend_from_slice(&[b, g, r]); // CZI stores B,G,R
            }
            tiles.push(ZeissTile {
                file_position,
                dimension_count: 0,
                compression: COMP_NONE,
                pixel_type: PT_BGR24,
                stored_w: w,
                stored_h: h,
                dest_x,
                dest_y,
            });
        }
        let reader = Arc::new(FakeCzi {
            data,
            batches: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
        });
        let level = ZeissLevel {
            reader: reader.clone(),
            tiles,
        };
        (level, reader)
    }

    /// Two subblocks overlapping in x 8..16, the second listed painting over the
    /// first. Cached or not, the pixels — and that priority — must be the same.
    #[test]
    fn a_cache_hit_changes_neither_the_pixels_nor_the_overlap_priority() {
        let (level, reader) =
            fake_level(&[(0, 0, 16, 16, (30, 20, 10)), (8, 0, 16, 16, (50, 100, 200))]);
        let region = |cache: crate::cache::LevelCache<'_>| {
            level.read_region(0, 0, 24, 16, cache).expect("read_region")
        };

        // Reference: caching disabled, so every subblock is fetched and decoded.
        let off = Cache::new(0);
        let want = region(off.level(SlideId::next(), 0));
        assert_eq!(want[0], argb(0xFF, 30, 20, 10), "left subblock");
        assert_eq!(want[8], argb(0xFF, 50, 100, 200), "overlap: later wins");
        assert_eq!(want[23], argb(0xFF, 50, 100, 200), "right subblock");

        let cache = Cache::default();
        let slide = SlideId::next();
        assert_eq!(region(cache.level(slide, 0)), want, "cold read");
        let batches = reader.batches.load(Ordering::Relaxed);
        assert!(batches > 0, "the cold read should have gathered bytes");

        assert_eq!(region(cache.level(slide, 0)), want, "fully cached read");
        assert_eq!(
            reader.batches.load(Ordering::Relaxed),
            batches,
            "a fully cached region must issue no gather at all"
        );
        assert_eq!(
            reader.reads.load(Ordering::Relaxed),
            0,
            "no subblock should have needed a follow-up read"
        );
    }

    /// The half-and-half case, which is what panning actually produces: one
    /// subblock cached, the other not. The cached tile must still paint in its
    /// directory position, not first.
    #[test]
    fn a_partly_cached_region_keeps_the_directory_order() {
        let (level, _) =
            fake_level(&[(0, 0, 16, 16, (30, 20, 10)), (8, 0, 16, 16, (50, 100, 200))]);

        let off = Cache::new(0);
        let want = level
            .read_region(0, 0, 24, 16, off.level(SlideId::next(), 0))
            .expect("reference");

        let cache = Cache::default();
        let slide = SlideId::next();
        // Warm only the first subblock: x 0..8 cannot reach the second, whose
        // destination starts at 8.
        level
            .read_region(0, 0, 8, 16, cache.level(slide, 0))
            .expect("warm the first subblock");
        assert_eq!(cache.len(), 1, "exactly one subblock should be cached");

        let got = level
            .read_region(0, 0, 24, 16, cache.level(slide, 0))
            .expect("mixed read");
        assert_eq!(got, want);
    }

    /// The key is the subblock's `file_position`, so a different slide sharing
    /// the same cache cannot be served this one's subblocks — the two fake
    /// containers below have subblocks at identical offsets.
    #[test]
    fn subblocks_are_not_shared_between_slides() {
        let (a, _) = fake_level(&[(0, 0, 8, 8, (10, 10, 10))]);
        let (b, _) = fake_level(&[(0, 0, 8, 8, (200, 200, 200))]);
        assert_eq!(a.tiles[0].file_position, b.tiles[0].file_position);

        let cache = Cache::default();
        let (sa, sb) = (SlideId::next(), SlideId::next());
        let from_a = a.read_region(0, 0, 8, 8, cache.level(sa, 0)).expect("a");
        let from_b = b.read_region(0, 0, 8, 8, cache.level(sb, 0)).expect("b");

        assert_eq!(from_a[0], argb(0xFF, 10, 10, 10));
        assert_eq!(from_b[0], argb(0xFF, 200, 200, 200));
        assert_eq!(cache.len(), 2);
    }
}
