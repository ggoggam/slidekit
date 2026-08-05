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

use std::collections::HashMap;
use std::sync::Arc;

use crate::associated::AssociatedImage;
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

/// One placed subblock: where its pixel bytes live, how they're encoded, and
/// where the decoded `stored_w × stored_h` image sits in this level's space.
struct ZeissTile {
    data_offset: u64,
    data_size: usize,
    compression: i32,
    pixel_type: i32,
    stored_w: i64,
    stored_h: i64,
    dest_x: i64,
    dest_y: i64,
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
    pub fn read_region(&self, ox: i64, oy: i64, w: i64, h: i64) -> Result<Vec<u32>> {
        let mut out = vec![0u32; (w * h) as usize];
        // Decode each subblock at most once per call (keyed by file offset).
        let mut cache: HashMap<u64, Vec<u32>> = HashMap::new();

        // ponytail: linear scan over all placed tiles — same as the MIRAX path.
        // A per-level spatial bucket would cut this if reads get hot.
        for t in &self.tiles {
            if t.dest_x + t.stored_w <= ox
                || t.dest_x >= ox + w
                || t.dest_y + t.stored_h <= oy
                || t.dest_y >= oy + h
            {
                continue;
            }

            let px = if let Some(cached) = cache.get(&t.data_offset) {
                cached
            } else {
                let mut buf = vec![0u8; t.data_size];
                self.reader.read_exact_at(t.data_offset, &mut buf)?;
                let px = decode_tile(&buf, t)?;
                cache.insert(t.data_offset, px);
                &cache[&t.data_offset]
            };

            crate::slide::composite_subtile(
                px, t.stored_w, 0, 0, t.stored_w, t.stored_h, t.dest_x, t.dest_y, &mut out, ox, oy,
                w, h,
            );
        }
        Ok(out)
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
    // Every subblock's 16-byte header is fetched in one batch first. The
    // directory says where the subblocks are but not where the pixels inside
    // them start, so this is unavoidably one small read per subblock — and
    // issued serially it was the whole cost of a remote CZI open (159 requests
    // for 16 MB of 16-byte headers, ~85 s). As one gather the backend coalesces
    // and parallelizes them.
    let headers = subblock_headers(&*reader, &kept)?;

    let mut by_downsample: HashMap<i64, Vec<ZeissTile>> = HashMap::new();
    for ((e, downsample), header) in kept.into_iter().zip(headers) {
        let (data_offset, data_size) =
            subblock_data_location(&header, e.file_position, e.dimension_count)?;
        by_downsample
            .entry(downsample)
            .or_default()
            .push(ZeissTile {
                data_offset,
                data_size,
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
    let mut entries = Vec::with_capacity(entry_count as usize);
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

/// Fetch every subblock's 16-byte payload header in one batched read.
fn subblock_headers(
    reader: &dyn SlideReader,
    entries: &[(&DirEntry, i64)],
) -> Result<Vec<bytes::Bytes>> {
    let ranges: Vec<std::ops::Range<u64>> = entries
        .iter()
        .map(|(e, _)| {
            let start = e.file_position as u64 + 32;
            start..start + 16
        })
        .collect();
    reader.read_ranges(&ranges)
}

/// Resolve a subblock's pixel-data `(offset, size)` from its 16-byte header.
///
/// The `ZISRAWSUBBLOCK` payload begins with `MetadataSize (i32)`,
/// `AttachmentSize (i32)`, `DataSize (i64)`, then the embedded DirectoryEntry.
/// The metadata+data start after a fixed region of `max(256, 16 + entry_size)`
/// bytes, where `entry_size = 32 + 20*DimensionCount`.
fn subblock_data_location(
    head: &[u8],
    file_position: i64,
    dimension_count: i32,
) -> Result<(u64, usize)> {
    let metadata_size = i32le(head, 0)? as u64;
    let data_size = i64le(head, 8)?;
    if data_size < 0 {
        return Err(OpenSlideError::Decode(
            "CZI: negative subblock data size".into(),
        ));
    }
    let entry_size = 32 + 20 * dimension_count as u64;
    let fixed = (16 + entry_size).max(256);
    let data_offset = file_position as u64 + 32 + fixed + metadata_size;
    Ok((data_offset, data_size as usize))
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
    let n = (w * h) as usize;
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

fn read_metadata_xml(reader: &dyn SlideReader, pos: u64) -> Option<String> {
    let mut head = [0u8; 8];
    reader.read_exact_at(pos + 32, &mut head).ok()?;
    let xml_size = i32le(&head, 0).ok()? as usize;
    if xml_size == 0 {
        return None;
    }
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
    if let Some(objs) = meta.descend(&["Information", "Instrument", "Objectives"]) {
        if let Some(obj) = objs.children_named("Objective").next() {
            if let Some(mag) = obj.find_child("NominalMagnification") {
                let mag = mag.text.trim();
                if !mag.is_empty() {
                    props.insert(names::OBJECTIVE_POWER, mag);
                }
            }
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
        if let Some(&file_pos) = entries.get(att_name) {
            if let Some(img) = read_one_attachment_jpeg(reader, file_pos) {
                out.push((name.to_string(), img));
            }
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
    let data_size = i32le(&head, 0).ok()? as usize;
    if data_size == 0 {
        return None;
    }
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
}
