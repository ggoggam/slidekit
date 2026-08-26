//! TIFF-like parser for vendor slide formats.
//!
//! This is a custom TIFF parser (ported from the C library's `openslide-decode-tifflike.c`)
//! that handles vendor-specific TIFF violations such as NDPI's high-bit offsets,
//! non-standard tags, and BigTIFF variants. It operates on `SlideReader` so it
//! works with both local files and remote/streaming sources.

use std::collections::BTreeMap;

use crate::error::{OpenSlideError, Result};
use crate::io::SlideReader;

// TIFF magic numbers
const TIFF_BIGENDIAN: u16 = 0x4D4D;
const TIFF_LITTLEENDIAN: u16 = 0x4949;
const TIFF_VERSION_CLASSIC: u16 = 42;
const TIFF_VERSION_BIG: u16 = 43;

// NDPI detection tag
const NDPI_TAG: u16 = 65420;

// TIFF type constants
const TIFF_BYTE: u16 = 1;
const TIFF_ASCII: u16 = 2;
const TIFF_SHORT: u16 = 3;
const TIFF_LONG: u16 = 4;
const TIFF_RATIONAL: u16 = 5;
const TIFF_SBYTE: u16 = 6;
const TIFF_UNDEFINED: u16 = 7;
const TIFF_SSHORT: u16 = 8;
const TIFF_SLONG: u16 = 9;
const TIFF_SRATIONAL: u16 = 10;
const TIFF_FLOAT: u16 = 11;
const TIFF_DOUBLE: u16 = 12;
const TIFF_IFD: u16 = 13;
const TIFF_LONG8: u16 = 16;
const TIFF_SLONG8: u16 = 17;
const TIFF_IFD8: u16 = 18;

// Well-known TIFF tags
pub mod tags {
    pub const IMAGE_WIDTH: u16 = 256;
    pub const IMAGE_LENGTH: u16 = 257;
    pub const BITS_PER_SAMPLE: u16 = 258;
    pub const COMPRESSION: u16 = 259;
    pub const PHOTOMETRIC: u16 = 262;
    pub const IMAGE_DESCRIPTION: u16 = 270;
    pub const MAKE: u16 = 271;
    pub const MODEL: u16 = 272;
    pub const STRIP_OFFSETS: u16 = 273;
    pub const SAMPLES_PER_PIXEL: u16 = 277;
    pub const ROWS_PER_STRIP: u16 = 278;
    pub const STRIP_BYTE_COUNTS: u16 = 279;
    pub const X_RESOLUTION: u16 = 282;
    pub const Y_RESOLUTION: u16 = 283;
    pub const RESOLUTION_UNIT: u16 = 296;
    pub const SOFTWARE: u16 = 305;
    pub const DATE_TIME: u16 = 306;
    pub const ARTIST: u16 = 315;
    pub const TILE_WIDTH: u16 = 322;
    pub const TILE_LENGTH: u16 = 323;
    pub const TILE_OFFSETS: u16 = 324;
    pub const TILE_BYTE_COUNTS: u16 = 325;
    pub const SAMPLE_FORMAT: u16 = 339;
    pub const JPEG_TABLES: u16 = 347;
    pub const YCBCR_SUBSAMPLING: u16 = 530;
    pub const X_POSITION: u16 = 286;
    pub const Y_POSITION: u16 = 287;
    pub const HOST_COMPUTER: u16 = 316;
    pub const COPYRIGHT: u16 = 33432;
    pub const DOCUMENT_NAME: u16 = 269;
    pub const XML_PACKET: u16 = 700;
    pub const SUBFILE_TYPE: u16 = 254;
    pub const PLANAR_CONFIG: u16 = 284;
    pub const IMAGE_DEPTH: u16 = 32997;
    pub const ICC_PROFILE: u16 = 34675;
    pub const NDPI: u16 = 65420;

    // Aperio JPEG 2000 compression codes
    pub const COMPRESSION_JP2K_YCBCR: u16 = 33003;
    pub const COMPRESSION_JP2K_RGB: u16 = 33005;

    // Predictor tag
    pub const PREDICTOR: u16 = 317;

    // Compression values
    pub const COMPRESSION_LZW: u16 = 5;
    pub const COMPRESSION_JPEG: u16 = 7;
    pub const COMPRESSION_DEFLATE: u16 = 8; // Adobe-style zlib Deflate
    pub const COMPRESSION_DEFLATE_OLD: u16 = 32946; // Deprecated PKZIP/zlib Deflate
    /// JPEG 2000 in a standard TIFF, as written by libtiff's OpenJPEG codec.
    /// Distinct from Aperio's private 33003/33005, which also say what the
    /// components mean; here they are plain RGB.
    pub const COMPRESSION_JP2K: u16 = 34712;
    pub const COMPRESSION_PACKBITS: u16 = 32773;
    /// PNG-compressed tiles (Adobe's registered code).
    pub const COMPRESSION_PNG: u16 = 34933;
    /// JPEG XL. 52546 is the code libjxl's TIFF support shipped first and DNG
    /// 1.7 adopted; 50002 is the later libtiff registration. Both are found.
    pub const COMPRESSION_JXL_DNG: u16 = 52546;
    pub const COMPRESSION_JXL: u16 = 50002;

    // SubfileType flag bits
    pub const FILETYPE_REDUCEDIMAGE: u64 = 0x1;

    // PhotometricInterpretation values
    pub const PHOTOMETRIC_RGB: u16 = 2;
    pub const PHOTOMETRIC_YCBCR: u16 = 6;

    // ResolutionUnit values
    pub const RESUNIT_INCH: u64 = 2;
    pub const RESUNIT_CENTIMETER: u64 = 3;
}

/// Parsed TIFF tag value — lazy-loaded on first access in the C version,
/// but we eagerly load inline values and defer out-of-line ones.
#[derive(Debug, Clone)]
struct TiffItem {
    tag_type: u16,
    count: i64,
    /// File offset for out-of-line values, or `None` if already loaded.
    offset: Option<u64>,
    /// Parsed value data.
    value: TiffValue,
}

#[derive(Debug, Clone)]
enum TiffValue {
    /// Not yet loaded from file.
    Deferred,
    Uints(Vec<u64>),
    Sints(Vec<i64>),
    Floats(Vec<f64>),
    Buffer(Vec<u8>),
    /// BYTE type has both uint and buffer representations.
    UintsAndBuffer(Vec<u64>, Vec<u8>),
}

/// A single TIFF IFD (Image File Directory).
#[derive(Debug, Clone)]
struct TiffDirectory {
    items: BTreeMap<u16, TiffItem>,
    /// Offset of this directory in the file (needed for NDPI fixups).
    offset: u64,
}

/// Parsed TIFF-like container with all IFDs.
///
/// Thread-safe once constructed. Deferred values are loaded via the reader
/// on first access (behind internal locking in future streaming mode).
pub struct TiffLike {
    big_endian: bool,
    ndpi: bool,
    directories: Vec<TiffDirectory>,
    /// Reader kept alive for deferred value loading.
    reader: Box<dyn SlideReader>,
}

// ── Byte-order helpers ──────────────────────────────────────────────

fn read_u16(buf: &[u8], big_endian: bool) -> u16 {
    let arr: [u8; 2] = [buf[0], buf[1]];
    if big_endian {
        u16::from_be_bytes(arr)
    } else {
        u16::from_le_bytes(arr)
    }
}

fn read_u32(buf: &[u8], big_endian: bool) -> u32 {
    let arr: [u8; 4] = [buf[0], buf[1], buf[2], buf[3]];
    if big_endian {
        u32::from_be_bytes(arr)
    } else {
        u32::from_le_bytes(arr)
    }
}

fn read_u64(buf: &[u8], big_endian: bool) -> u64 {
    let arr: [u8; 8] = [
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ];
    if big_endian {
        u64::from_be_bytes(arr)
    } else {
        u64::from_le_bytes(arr)
    }
}

fn read_i32(buf: &[u8], big_endian: bool) -> i32 {
    let arr: [u8; 4] = [buf[0], buf[1], buf[2], buf[3]];
    if big_endian {
        i32::from_be_bytes(arr)
    } else {
        i32::from_le_bytes(arr)
    }
}

fn read_f32(buf: &[u8], big_endian: bool) -> f32 {
    let arr: [u8; 4] = [buf[0], buf[1], buf[2], buf[3]];
    if big_endian {
        f32::from_be_bytes(arr)
    } else {
        f32::from_le_bytes(arr)
    }
}

fn read_f64(buf: &[u8], big_endian: bool) -> f64 {
    let arr: [u8; 8] = [
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ];
    if big_endian {
        f64::from_be_bytes(arr)
    } else {
        f64::from_le_bytes(arr)
    }
}

/// Read an unsigned integer of the given byte size from the reader at `offset`.
fn read_uint_at(
    reader: &dyn SlideReader,
    offset: u64,
    size: usize,
    big_endian: bool,
) -> Result<u64> {
    let mut buf = [0u8; 8];
    reader.read_exact_at(offset, &mut buf[..size])?;
    Ok(match size {
        1 => buf[0] as u64,
        2 => read_u16(&buf, big_endian) as u64,
        4 => read_u32(&buf, big_endian) as u64,
        8 => read_u64(&buf, big_endian),
        _ => unreachable!(),
    })
}

// ── Value size / type helpers ───────────────────────────────────────

/// Returns (element_size, adjusted_count) for a TIFF type.
///
/// Returns None for an unknown type, and for a count the file cannot honestly
/// mean: BigTIFF reads its count as a `u64` that lands in an `i64`, so the high
/// bit makes it negative, and every caller multiplies it into a length.
fn value_size_and_count(tag_type: u16, count: i64) -> Option<(u32, i64)> {
    if count < 0 {
        return None;
    }
    match tag_type {
        TIFF_BYTE | TIFF_ASCII | TIFF_SBYTE | TIFF_UNDEFINED => Some((1, count)),
        TIFF_SHORT | TIFF_SSHORT => Some((2, count)),
        TIFF_LONG | TIFF_SLONG | TIFF_FLOAT | TIFF_IFD => Some((4, count)),
        TIFF_RATIONAL | TIFF_SRATIONAL => Some((4, count.checked_mul(2)?)),
        TIFF_DOUBLE | TIFF_LONG8 | TIFF_SLONG8 | TIFF_IFD8 => Some((8, count)),
        _ => None,
    }
}

/// The first element of a tag's value list.
///
/// A TIFF entry may declare a count of zero, in which case the value parses to
/// an empty list while `has_tag` still reports the tag as present — so every
/// `has_tag(..) { get_uint(..) }` pair in the crate would index element 0 of
/// nothing.
fn first_value<T>(values: &[T], dir: i64, tag: u16) -> Result<&T> {
    values
        .first()
        .ok_or_else(|| OpenSlideError::Decode(format!("Empty value for dir {dir}, tag {tag}")))
}

/// Re-add implied high-order bits to a 32-bit offset (NDPI heuristic).
/// Maximize high-order bits while keeping the result below `diroff`.
fn fix_offset_ndpi(diroff: u64, offset: u64) -> u64 {
    let result = (diroff & !u64::from(u32::MAX)) | (offset & u64::from(u32::MAX));
    if result >= diroff {
        // Subtract one 4GB page, but don't wrap below zero
        result.saturating_sub(u64::from(u32::MAX) + 1).min(result)
    } else {
        result
    }
}

// ── Parsing values from raw bytes ───────────────────────────────────

fn parse_values(tag_type: u16, count: i64, buf: &[u8], big_endian: bool) -> Result<TiffValue> {
    match tag_type {
        // Unsigned integers
        TIFF_BYTE => {
            let uints: Vec<u64> = buf.iter().take(count as usize).map(|&b| b as u64).collect();
            // BYTE also gets a buffer representation (for XMLPACKET etc.)
            let buffer = buf[..count as usize].to_vec();
            Ok(TiffValue::UintsAndBuffer(uints, buffer))
        }
        TIFF_SHORT => {
            let uints = (0..count as usize)
                .map(|i| read_u16(&buf[i * 2..], big_endian) as u64)
                .collect();
            Ok(TiffValue::Uints(uints))
        }
        TIFF_LONG | TIFF_IFD => {
            let uints = (0..count as usize)
                .map(|i| read_u32(&buf[i * 4..], big_endian) as u64)
                .collect();
            Ok(TiffValue::Uints(uints))
        }
        TIFF_LONG8 | TIFF_IFD8 => {
            let uints = (0..count as usize)
                .map(|i| read_u64(&buf[i * 8..], big_endian))
                .collect();
            Ok(TiffValue::Uints(uints))
        }

        // Signed integers
        TIFF_SBYTE => {
            let sints = buf
                .iter()
                .take(count as usize)
                .map(|&b| b as i8 as i64)
                .collect();
            Ok(TiffValue::Sints(sints))
        }
        TIFF_SSHORT => {
            let sints = (0..count as usize)
                .map(|i| {
                    let v = read_u16(&buf[i * 2..], big_endian);
                    v as i16 as i64
                })
                .collect();
            Ok(TiffValue::Sints(sints))
        }
        TIFF_SLONG => {
            let sints = (0..count as usize)
                .map(|i| read_i32(&buf[i * 4..], big_endian) as i64)
                .collect();
            Ok(TiffValue::Sints(sints))
        }
        TIFF_SLONG8 => {
            let sints = (0..count as usize)
                .map(|i| read_u64(&buf[i * 8..], big_endian) as i64)
                .collect();
            Ok(TiffValue::Sints(sints))
        }

        // Floats
        TIFF_FLOAT => {
            let floats = (0..count as usize)
                .map(|i| read_f32(&buf[i * 4..], big_endian) as f64)
                .collect();
            Ok(TiffValue::Floats(floats))
        }
        TIFF_DOUBLE => {
            let floats = (0..count as usize)
                .map(|i| read_f64(&buf[i * 8..], big_endian))
                .collect();
            Ok(TiffValue::Floats(floats))
        }
        TIFF_RATIONAL => {
            let mut floats = Vec::with_capacity(count as usize);
            for i in 0..count as usize {
                let num = read_u32(&buf[i * 8..], big_endian) as f64;
                let den = read_u32(&buf[i * 8 + 4..], big_endian) as f64;
                if den == 0.0 {
                    return Err(OpenSlideError::Decode(
                        "Zero denominator in rational value".into(),
                    ));
                }
                floats.push(num / den);
            }
            Ok(TiffValue::Floats(floats))
        }
        TIFF_SRATIONAL => {
            let mut floats = Vec::with_capacity(count as usize);
            for i in 0..count as usize {
                let num = read_i32(&buf[i * 8..], big_endian) as f64;
                let den = read_i32(&buf[i * 8 + 4..], big_endian) as f64;
                if den == 0.0 {
                    return Err(OpenSlideError::Decode(
                        "Zero denominator in rational value".into(),
                    ));
                }
                floats.push(num / den);
            }
            Ok(TiffValue::Floats(floats))
        }

        // Buffer types
        TIFF_ASCII | TIFF_UNDEFINED => {
            let buffer = buf[..count as usize].to_vec();
            Ok(TiffValue::Buffer(buffer))
        }

        _ => Err(OpenSlideError::Decode(format!(
            "Unknown TIFF type: {tag_type}"
        ))),
    }
}

// ── Directory reading ───────────────────────────────────────────────

struct ReadDirContext<'a> {
    reader: &'a dyn SlideReader,
    bigtiff: bool,
    ndpi: bool,
    big_endian: bool,
}

/// Read one IFD from the file. Returns the directory and the offset of the next
/// IFD (0 means end of chain).
fn read_directory(
    ctx: &ReadDirContext<'_>,
    dir_offset: u64,
    first_dir: Option<&TiffDirectory>,
) -> Result<(TiffDirectory, u64)> {
    if dir_offset == 0 {
        return Err(OpenSlideError::Decode("Bad offset".into()));
    }

    let reader = ctx.reader;
    let big_endian = ctx.big_endian;
    let bigtiff = ctx.bigtiff;
    let ndpi = ctx.ndpi;

    // Read directory entry count
    let entry_count_size = if bigtiff { 8 } else { 2 };
    let dircount = read_uint_at(reader, dir_offset, entry_count_size, big_endian)?;

    let mut items = BTreeMap::new();
    let value_field_size: usize = if bigtiff { 8 } else { 4 };
    let entry_size: usize = 2 + 2 + (if bigtiff { 8 } else { 4 }) + value_field_size; // tag + type + count + value/offset
    let entries_start = dir_offset + entry_count_size as u64;

    for n in 0..dircount {
        let entry_offset = entries_start + n * entry_size as u64;
        let mut entry_buf = vec![0u8; entry_size];
        reader.read_exact_at(entry_offset, &mut entry_buf)?;

        let tag = read_u16(&entry_buf[0..], big_endian);
        let tag_type = read_u16(&entry_buf[2..], big_endian);
        let count = if bigtiff {
            read_u64(&entry_buf[4..], big_endian) as i64
        } else {
            read_u32(&entry_buf[4..], big_endian) as i64
        };

        let count_offset = if bigtiff { 12 } else { 8 };
        let value_bytes = &entry_buf[count_offset..count_offset + value_field_size];

        // Compute value size
        let (elem_size, adjusted_count) = match value_size_and_count(tag_type, count) {
            Some(v) => v,
            None => {
                return Err(OpenSlideError::Decode(format!(
                    "Unknown TIFF type: {tag_type}"
                )));
            }
        };

        // A count comes straight off the file, and in BigTIFF it is 64 bits: the
        // product can wrap, and a wrapped one small enough to look inline sends
        // `parse_values` reading `count` elements out of an 8-byte field.
        let total_value_len = (elem_size as u64)
            .checked_mul(adjusted_count as u64)
            .ok_or_else(|| {
                OpenSlideError::Decode(format!("TIFF tag {tag}: value length overflows"))
            })?;

        let (value, deferred_offset) = if total_value_len <= value_field_size as u64 {
            // Value fits inline
            let parsed = parse_values(tag_type, count, value_bytes, big_endian)?;
            (parsed, None)
        } else {
            // Value is out-of-line; read the offset
            let mut data_offset = if bigtiff {
                read_u64(value_bytes, big_endian)
            } else {
                read_u32(value_bytes, big_endian) as u64
            };

            if ndpi {
                // NDPI offset fixup: if this tag has the same offset in the
                // first IFD, reuse that value; otherwise apply heuristic.
                let reuse = first_dir
                    .and_then(|fd| fd.items.get(&tag))
                    .and_then(|fi| fi.offset)
                    .map(|first_off| first_off == data_offset)
                    .unwrap_or(false);
                if !reuse {
                    data_offset = fix_offset_ndpi(dir_offset, data_offset);
                }
            }

            (TiffValue::Deferred, Some(data_offset))
        };

        items.insert(
            tag,
            TiffItem {
                tag_type,
                count,
                offset: deferred_offset,
                value,
            },
        );
    }

    // Read next directory offset
    let next_off_pos = entries_start + dircount * entry_size as u64;
    let next_off_size = if bigtiff || ndpi { 8 } else { 4 };
    let next_diroff = read_uint_at(reader, next_off_pos, next_off_size, big_endian)?;

    Ok((
        TiffDirectory {
            items,
            offset: dir_offset,
        },
        next_diroff,
    ))
}

// ── TiffLike construction ───────────────────────────────────────────

impl TiffLike {
    /// Try to parse a TIFF-like container from the given reader.
    ///
    /// Returns `Err` if the file is not TIFF at all. Non-TIFF formats (MRXS,
    /// DICOM, etc.) will fail here; vendor detectors that don't need TIFF can
    /// simply ignore the error.
    pub fn try_parse(reader: Box<dyn SlideReader>) -> Result<Self> {
        // Read header (up to 16 bytes covers both classic and BigTIFF headers)
        let mut header = [0u8; 16];
        reader.read_exact_at(0, &mut header[..8])?;

        // Check magic
        let magic = u16::from_ne_bytes([header[0], header[1]]);
        let big_endian = match magic {
            TIFF_BIGENDIAN => true,
            TIFF_LITTLEENDIAN => false,
            _ => {
                return Err(OpenSlideError::Decode("Not a TIFF file (bad magic)".into()));
            }
        };

        // Read version
        let version = read_u16(&header[2..], big_endian);
        let bigtiff = version == TIFF_VERSION_BIG;

        if !bigtiff && version != TIFF_VERSION_CLASSIC {
            return Err(OpenSlideError::Decode(format!(
                "Unrecognized TIFF version: {version}"
            )));
        }

        // For BigTIFF, read extended header
        let diroff = if bigtiff {
            reader.read_exact_at(4, &mut header[4..16])?;
            let offset_size = read_u16(&header[4..], big_endian);
            let pad = read_u16(&header[6..], big_endian);
            if offset_size != 8 || pad != 0 {
                return Err(OpenSlideError::Decode(
                    "Unexpected value in BigTIFF header".into(),
                ));
            }
            read_u64(&header[8..], big_endian)
        } else {
            // Classic TIFF: the diroff is 4 bytes at offset 4.
            // For NDPI detection we also need the 8-byte interpretation
            // (NDPI is classic TIFF pretending to have 64-bit offsets).
            reader.read_exact_at(4, &mut header[4..12])?;

            // The canonical 32-bit diroff
            let diroff32 = read_u32(&header[4..], big_endian) as u64;

            // The speculative 64-bit diroff for NDPI trial (only differs
            // from diroff32 when high bytes are nonzero)
            let diroff64 = read_u64(&header[4..], big_endian);

            if diroff64 != diroff32 {
                diroff64
            } else {
                diroff32
            }
        };

        // NDPI detection: classic TIFF but the 64-bit diroff points to a
        // valid directory containing the NDPI tag.
        let mut ndpi = false;
        let mut directories = Vec::new();
        let mut current_diroff;

        // For classic TIFF, extract the true 32-bit diroff as fallback
        let diroff32 = if bigtiff {
            diroff
        } else {
            read_u32(&header[4..], big_endian) as u64
        };

        if !bigtiff && diroff != diroff32 && diroff != 0 {
            // The 64-bit and 32-bit interpretations differ — try NDPI
            let trial_ctx = ReadDirContext {
                reader: &*reader,
                bigtiff: false,
                ndpi: true,
                big_endian,
            };
            if let Ok((dir, next_off)) = read_directory(&trial_ctx, diroff, None) {
                if dir.items.contains_key(&NDPI_TAG) {
                    ndpi = true;
                    current_diroff = next_off;
                    directories.push(dir);
                } else {
                    current_diroff = diroff32;
                }
            } else {
                current_diroff = diroff32;
            }
        } else {
            current_diroff = if bigtiff { diroff } else { diroff32 };
        }

        // Read all directories
        let ctx = ReadDirContext {
            reader: &*reader,
            bigtiff,
            ndpi,
            big_endian,
        };

        let mut seen_offsets = std::collections::HashSet::new();
        while current_diroff != 0 {
            if !seen_offsets.insert(current_diroff) {
                // Loop detected
                return Err(OpenSlideError::Decode("IFD loop detected".into()));
            }

            let first_dir = directories.first();
            let (dir, next_off) = read_directory(&ctx, current_diroff, first_dir)?;
            directories.push(dir);
            current_diroff = next_off;
        }

        if directories.is_empty() {
            return Err(OpenSlideError::Decode(
                "TIFF contains no directories".into(),
            ));
        }

        Ok(Self {
            big_endian,
            ndpi,
            directories,
            reader,
        })
    }

    // ── Public accessors ────────────────────────────────────────────

    /// Number of IFDs in this TIFF.
    pub fn directory_count(&self) -> i64 {
        self.directories.len() as i64
    }

    /// Whether this was detected as an NDPI file.
    pub fn is_ndpi(&self) -> bool {
        self.ndpi
    }

    /// Number of values for a tag in the given directory. Returns 0 if not present.
    pub fn value_count(&self, dir: i64, tag: u16) -> i64 {
        self.get_item(dir, tag).map(|it| it.count).unwrap_or(0)
    }

    /// Get the first unsigned integer value for a tag.
    pub fn get_uint(&mut self, dir: i64, tag: u16) -> Result<u64> {
        self.ensure_loaded(dir, tag)?;
        let item = self.get_item(dir, tag).ok_or_else(|| {
            OpenSlideError::Decode(format!("No such value: dir {dir}, tag {tag}"))
        })?;
        match &item.value {
            TiffValue::Uints(v) | TiffValue::UintsAndBuffer(v, _) => {
                first_value(v, dir, tag).copied()
            }
            _ => Err(OpenSlideError::Decode(format!(
                "Unexpected value type for dir {dir}, tag {tag}, type {}",
                item.tag_type
            ))),
        }
    }

    /// Borrow a tag's unsigned integer values, without the clone `get_uints`
    /// makes. The value must already be loaded — call `ensure_loaded` first.
    ///
    /// Taking `&self` lets a caller hold two of these at once, which is what
    /// `tile_location` needs.
    fn uints_of(&self, dir: i64, tag: u16) -> Result<&[u64]> {
        let item = self.get_item(dir, tag).ok_or_else(|| {
            OpenSlideError::Decode(format!("No such value: dir {dir}, tag {tag}"))
        })?;
        match &item.value {
            TiffValue::Uints(v) | TiffValue::UintsAndBuffer(v, _) => Ok(v),
            _ => Err(OpenSlideError::Decode(format!(
                "Unexpected value type for dir {dir}, tag {tag}, type {}",
                item.tag_type
            ))),
        }
    }

    /// Get all unsigned integer values for a tag.
    pub fn get_uints(&mut self, dir: i64, tag: u16) -> Result<Vec<u64>> {
        self.ensure_loaded(dir, tag)?;
        let item = self.get_item(dir, tag).ok_or_else(|| {
            OpenSlideError::Decode(format!("No such value: dir {dir}, tag {tag}"))
        })?;
        match &item.value {
            TiffValue::Uints(v) => Ok(v.clone()),
            TiffValue::UintsAndBuffer(v, _) => Ok(v.clone()),
            _ => Err(OpenSlideError::Decode(format!(
                "Unexpected value type for dir {dir}, tag {tag}, type {}",
                item.tag_type
            ))),
        }
    }

    /// Get the first signed integer value for a tag.
    pub fn get_sint(&mut self, dir: i64, tag: u16) -> Result<i64> {
        self.ensure_loaded(dir, tag)?;
        let item = self.get_item(dir, tag).ok_or_else(|| {
            OpenSlideError::Decode(format!("No such value: dir {dir}, tag {tag}"))
        })?;
        match &item.value {
            TiffValue::Sints(v) => first_value(v, dir, tag).copied(),
            _ => Err(OpenSlideError::Decode(format!(
                "Unexpected value type for dir {dir}, tag {tag}, type {}",
                item.tag_type
            ))),
        }
    }

    /// Get the first float value for a tag.
    pub fn get_float(&mut self, dir: i64, tag: u16) -> Result<f64> {
        self.ensure_loaded(dir, tag)?;
        let item = self.get_item(dir, tag).ok_or_else(|| {
            OpenSlideError::Decode(format!("No such value: dir {dir}, tag {tag}"))
        })?;
        match &item.value {
            TiffValue::Floats(v) => first_value(v, dir, tag).copied(),
            _ => Err(OpenSlideError::Decode(format!(
                "Unexpected value type for dir {dir}, tag {tag}, type {}",
                item.tag_type
            ))),
        }
    }

    /// Get all float values for a tag.
    pub fn get_floats(&mut self, dir: i64, tag: u16) -> Result<Vec<f64>> {
        self.ensure_loaded(dir, tag)?;
        let item = self.get_item(dir, tag).ok_or_else(|| {
            OpenSlideError::Decode(format!("No such value: dir {dir}, tag {tag}"))
        })?;
        match &item.value {
            TiffValue::Floats(v) => Ok(v.clone()),
            _ => Err(OpenSlideError::Decode(format!(
                "Unexpected value type for dir {dir}, tag {tag}, type {}",
                item.tag_type
            ))),
        }
    }

    /// Get the buffer (ASCII/UNDEFINED/BYTE) value for a tag as bytes.
    pub fn get_buffer(&mut self, dir: i64, tag: u16) -> Result<Vec<u8>> {
        self.ensure_loaded(dir, tag)?;
        let item = self.get_item(dir, tag).ok_or_else(|| {
            OpenSlideError::Decode(format!("No such value: dir {dir}, tag {tag}"))
        })?;
        match &item.value {
            TiffValue::Buffer(v) => Ok(v.clone()),
            TiffValue::UintsAndBuffer(_, v) => Ok(v.clone()),
            _ => Err(OpenSlideError::Decode(format!(
                "Unexpected value type for dir {dir}, tag {tag}, type {}",
                item.tag_type
            ))),
        }
    }

    /// Get the buffer value as a string (null-terminated or full length).
    pub fn get_string(&mut self, dir: i64, tag: u16) -> Result<String> {
        let buf = self.get_buffer(dir, tag)?;
        // Trim trailing null bytes
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        Ok(String::from_utf8_lossy(&buf[..end]).into_owned())
    }

    /// Check if a directory is tiled (has tile width and tile length tags).
    pub fn is_tiled(&self, dir: i64) -> bool {
        self.value_count(dir, tags::TILE_WIDTH) > 0 && self.value_count(dir, tags::TILE_LENGTH) > 0
    }

    /// Fix a 32-bit offset for NDPI using the directory's file offset.
    pub fn uint_fix_offset_ndpi(&self, dir: i64, offset: u64) -> u64 {
        if !self.ndpi {
            return offset;
        }
        if let Some(d) = self.directories.get(dir as usize) {
            fix_offset_ndpi(d.offset, offset)
        } else {
            offset
        }
    }

    /// Check whether a tag exists in the given directory.
    pub fn has_tag(&self, dir: i64, tag: u16) -> bool {
        self.get_item(dir, tag).is_some()
    }

    /// Access the underlying reader (for raw byte reads during decoding).
    pub fn reader(&self) -> &dyn SlideReader {
        &*self.reader
    }

    /// Read raw compressed tile data for a given tile index in a directory.
    ///
    /// Tiles are indexed linearly: index = row * tiles_across + col.
    /// Returns the compressed bytes as stored in the TIFF file.
    pub fn read_tile_data(&mut self, dir: i64, tile_index: usize) -> Result<Vec<u8>> {
        match self.tile_location(dir, tile_index)? {
            None => Ok(vec![]), // Empty/missing tile
            Some((offset, size)) => {
                let mut buf = vec![0u8; size];
                self.reader.read_exact_at(offset, &mut buf)?;
                Ok(buf)
            }
        }
    }

    /// Eagerly load the tile offset and byte-count tables for a directory.
    ///
    /// These tags are loaded lazily on first use, which involves a read through
    /// the underlying reader. Async tile reading resolves tile locations while
    /// holding a lock and must not perform blocking I/O there, so callers
    /// pre-load the tables (off any async runtime) at open time. A directory
    /// without tile tables (e.g. a stripped image) is a no-op.
    pub fn preload_tile_tables(&mut self, dir: i64) -> Result<()> {
        if self.has_tag(dir, tags::TILE_OFFSETS) {
            self.get_uints(dir, tags::TILE_OFFSETS)?;
        }
        if self.has_tag(dir, tags::TILE_BYTE_COUNTS) {
            self.get_uints(dir, tags::TILE_BYTE_COUNTS)?;
        }
        Ok(())
    }

    /// Resolve the file offset and byte size of a tile without reading it.
    ///
    /// Returns `None` for an empty/missing tile (offset or byte count of 0).
    /// This lets callers (e.g. async tile readers) issue the byte-range read
    /// themselves while the offset tables stay owned by `TiffLike`.
    ///
    /// Call [`preload_tile_tables`](Self::preload_tile_tables) first if this
    /// must not perform blocking I/O (e.g. from within an async runtime).
    pub fn tile_location(&mut self, dir: i64, tile_index: usize) -> Result<Option<(u64, usize)>> {
        // Borrow the tables rather than `get_uints`, which clones both in full.
        // This runs once per cache-missing tile, and a level-0 Aperio directory
        // has tens of thousands of them: copying ~370 KB of offsets to read two
        // integers cost more than the tile fetch it was preparing.
        self.ensure_loaded(dir, tags::TILE_OFFSETS)?;
        self.ensure_loaded(dir, tags::TILE_BYTE_COUNTS)?;
        let offsets = self.uints_of(dir, tags::TILE_OFFSETS)?;
        let byte_counts = self.uints_of(dir, tags::TILE_BYTE_COUNTS)?;

        if tile_index >= offsets.len() || tile_index >= byte_counts.len() {
            return Err(OpenSlideError::Decode(format!(
                "Tile index {tile_index} out of range (max {})",
                offsets.len()
            )));
        }

        let offset = offsets[tile_index];
        let size = byte_counts[tile_index] as usize;

        if offset == 0 || size == 0 {
            return Ok(None);
        }
        Ok(Some((offset, size)))
    }

    /// Get the compression type for a directory.
    pub fn compression(&mut self, dir: i64) -> Result<u64> {
        self.get_uint(dir, tags::COMPRESSION)
    }

    /// Get JPEG tables for a directory (from the JPEGTables tag), if present.
    pub fn jpeg_tables(&mut self, dir: i64) -> Result<Option<Vec<u8>>> {
        if !self.has_tag(dir, tags::JPEG_TABLES) {
            return Ok(None);
        }
        self.get_buffer(dir, tags::JPEG_TABLES).map(Some)
    }

    /// Check if a directory is strip-based (has strip offsets).
    pub fn is_stripped(&self, dir: i64) -> bool {
        self.has_tag(dir, tags::STRIP_OFFSETS)
    }

    /// Read all strip data for a stripped directory, concatenated.
    pub fn read_strip_data(&mut self, dir: i64) -> Result<Vec<u8>> {
        let offsets = self.get_uints(dir, tags::STRIP_OFFSETS)?;
        let byte_counts = self.get_uints(dir, tags::STRIP_BYTE_COUNTS)?;

        let total_size: usize = byte_counts.iter().map(|&c| c as usize).sum();
        let mut buf = Vec::with_capacity(total_size);

        for (i, (&offset, &size)) in offsets.iter().zip(byte_counts.iter()).enumerate() {
            if offset == 0 || size == 0 {
                continue;
            }
            let size = size as usize;
            let start = buf.len();
            buf.resize(start + size, 0);
            self.reader
                .read_exact_at(offset, &mut buf[start..start + size])
                .map_err(|e| OpenSlideError::Decode(format!("Failed to read strip {i}: {e}")))?;
        }

        Ok(buf)
    }

    /// Read individual strip data for a stripped directory.
    ///
    /// Returns each strip as a separate `Vec<u8>`. For JPEG-compressed strips,
    /// each strip is a separate JPEG image that must be decoded independently.
    pub fn read_strips(&mut self, dir: i64) -> Result<Vec<Vec<u8>>> {
        let offsets = self.get_uints(dir, tags::STRIP_OFFSETS)?;
        let byte_counts = self.get_uints(dir, tags::STRIP_BYTE_COUNTS)?;

        let mut strips = Vec::with_capacity(offsets.len());

        for (i, (&offset, &size)) in offsets.iter().zip(byte_counts.iter()).enumerate() {
            if offset == 0 || size == 0 {
                strips.push(vec![]);
                continue;
            }
            let size = size as usize;
            let mut buf = vec![0u8; size];
            self.reader
                .read_exact_at(offset, &mut buf)
                .map_err(|e| OpenSlideError::Decode(format!("Failed to read strip {i}: {e}")))?;
            strips.push(buf);
        }

        Ok(strips)
    }

    /// Get the rows per strip for a directory.
    pub fn rows_per_strip(&mut self, dir: i64) -> Result<u64> {
        if self.has_tag(dir, tags::ROWS_PER_STRIP) {
            self.get_uint(dir, tags::ROWS_PER_STRIP)
        } else {
            // Default: entire image is one strip
            self.get_uint(dir, tags::IMAGE_LENGTH)
        }
    }

    /// Read the ICC profile from a directory, if present.
    pub fn read_icc_profile(&mut self, dir: i64) -> Result<Option<Vec<u8>>> {
        if !self.has_tag(dir, tags::ICC_PROFILE) {
            return Ok(None);
        }
        self.get_buffer(dir, tags::ICC_PROFILE).map(Some)
    }

    /// Get tile byte counts for a directory (useful for detecting missing tiles).
    pub fn tile_byte_counts(&mut self, dir: i64) -> Result<Vec<u64>> {
        self.get_uints(dir, tags::TILE_BYTE_COUNTS)
    }

    // ── Internal helpers ────────────────────────────────────────────

    fn get_item(&self, dir: i64, tag: u16) -> Option<&TiffItem> {
        self.directories
            .get(dir as usize)
            .and_then(|d| d.items.get(&tag))
    }

    /// Ensure a deferred value is loaded from the file.
    fn ensure_loaded(&mut self, dir: i64, tag: u16) -> Result<()> {
        let d = self
            .directories
            .get(dir as usize)
            .ok_or_else(|| OpenSlideError::Decode(format!("No such directory: {dir}")))?;

        let item = d.items.get(&tag).ok_or_else(|| {
            OpenSlideError::Decode(format!("No such value: dir {dir}, tag {tag}"))
        })?;

        if item.offset.is_none() {
            // Already loaded
            return Ok(());
        }

        // Need to load — get the info we need before mutating
        let file_offset = item.offset.unwrap();
        let tag_type = item.tag_type;
        let count = item.count;
        let big_endian = self.big_endian;

        let (elem_size, adjusted_count) = value_size_and_count(tag_type, count)
            .ok_or_else(|| OpenSlideError::Decode(format!("Unknown type: {tag_type}")))?;

        let total_len = (elem_size as u64)
            .checked_mul(adjusted_count as u64)
            .ok_or_else(|| {
                OpenSlideError::Decode(format!("TIFF tag {tag}: value length overflows"))
            })? as usize;
        // The length is the file's own count field, so a 200-byte file can ask
        // for gigabytes. Refusing anything that cannot fit in the file turns an
        // allocation abort into an ordinary parse error; `read_exact_at` below
        // would have rejected it too, but only after the allocation.
        let file_size = self.reader.size()?;
        if total_len as u64 > file_size {
            return Err(OpenSlideError::Decode(format!(
                "TIFF tag {tag}: declares {total_len} bytes, file holds {file_size}"
            )));
        }
        let mut buf = vec![0u8; total_len];
        self.reader.read_exact_at(file_offset, &mut buf)?;

        let value = parse_values(tag_type, count, &buf, big_endian)?;

        // Now mutate
        let item = self
            .directories
            .get_mut(dir as usize)
            .unwrap()
            .items
            .get_mut(&tag)
            .unwrap();
        item.value = value;
        item.offset = None;

        Ok(())
    }
}
