# slidekit

A pure-Rust implementation of [OpenSlide](https://openslide.org/) for reading
whole-slide images (WSI) in digital pathology. Ground-up rewrite of the C
library with 1:1 API parity, streaming/remote file support (S3, GCS, Azure,
HTTP) without full download, and no C dependencies.

```rust
use slidekit::OpenSlide;

let slide = OpenSlide::open("CMU-1.svs")?;
let (w, h) = slide.level0_dimensions();
let region = slide.read_region(0, 0, 0, 512, 512)?; // ARGB Vec<u32>
```

## Vendor format support

| Format | Status | Notes |
|--------|:------:|-------|
| Generic tiled TIFF | ✅ | Fallback for any tiled TIFF |
| Aperio (`.svs`) | ✅ | Levels, properties, associated images, ICC, tile decode |
| Trestle | ✅ | Detect, levels, properties, tile decode |
| Philips TIFF | ✅ | XML-driven level fixup, mpp/objective, associated images |
| Ventana BIF | ✅ | AOI stitch geometry → tilemap, associated images, ICC |
| Leica SCN | ✅ | Single brightfield region; multi-region SCN deferred |
| Hamamatsu NDPI | ✅ | Restart-marker JPEG tiling; VMS/VMU & z-stacks deferred |
| 3DHistech MIRAX (`.mrxs`) | ✅ | Multi-file; brightfield JPEG tiles; fluorescence deferred |
| Zeiss CZI (`.czi`) | ✅ | Uncompressed / Zstd0 / Zstd1; **JPEG XR not supported** ¹ |
| DICOM WSI (`.dcm`) | ✅ | Directory of instances; JPEG + uncompressed, **JPEG 2000 not supported** ² |
| Synthetic | ✅ | Self-test slides, no file needed (`OPENSLIDE_DEBUG=synthetic`) |
| Sakura | ❌ | Planned (needs a SQLite reader); no public test slide exists |

¹ Matches OpenSlide's own C driver, which also decodes only uncompressed/Zstd
and rejects JPEG XR. JXR is common for brightfield CZIs but has no pure-Rust
decoder; such files are detected as Zeiss but fail to open with a clear error
rather than producing black tiles.

² OpenSlide's C driver decodes JPEG 2000 DICOM through OpenJPEG. slidekit has no
JPEG 2000 decoder yet, so those instances are detected as DICOM but fail to open
with a clear error. Baseline-JPEG and uncompressed slides — what current scanners
write — are fully supported.

## API surface

Mirrors the OpenSlide C API. All methods below are implemented on `OpenSlide`.

| Area | Methods |
|------|---------|
| Open | `open`, `open_url`, `open_store`, `open_url_async`, `open_store_async`, `detect_vendor` |
| Levels | `level_count`, `level0_dimensions`, `level_dimensions`, `level_downsample`, `best_level_for_downsample` |
| Read | `read_region`, `read_region_async`, `read_region_into` |
| Properties | `property_names`, `property_value` |
| Associated images | `associated_image_names`, `associated_image_dimensions`, `read_associated_image` |
| Color profiles | `icc_profile`, `associated_image_icc_profile` |
| Misc | `set_cache`, `version` |

`error()` (C-API error-state string) is the one API not yet ported.

## Compression / decoders

| Codec | Status | Backend |
|-------|:------:|---------|
| JPEG | ✅ | `jpeg-decoder` |
| LZW (TIFF) | ✅ | `weezl` + predictor |
| Deflate / ZIP (TIFF) | ✅ | `miniz_oxide` |
| PackBits (TIFF) | ✅ | in-tree |
| Zstd v0 / v1 (CZI) | ✅ | `ruzstd` (pure Rust) |
| Uncompressed (CZI BGR/gray, DICOM RGB) | ✅ | in-tree |
| JPEG 2000 | ❌ | Planned (needed by JP2K Aperio + DICOM slides) |
| JPEG XR (CZI) | ❌ | No pure-Rust decoder |
| PNG / BMP | ❌ | Planned |

## I/O backends

| Backend | Status | Notes |
|---------|:------:|-------|
| Local file | ✅ | `pread`-based positioned reads |
| S3 / GCS / Azure / HTTP | ✅ | Byte-range requests via `object_store`; no full download |
| `mmap` local reader | ❌ | Planned optimization |

Enabled by default via the `object-store` feature (disable for a
local-file-only build with no async/cloud dependencies).

## Building

```sh
cargo build
cargo test    # unit tests run always; integration tests auto-download
              # fixtures from the OpenSlide server, or skip if unavailable
```

Integration tests fetch real slides from the OpenSlide test-data server and
verify SHA-256 checksums. They skip gracefully (rather than fail) when
downloads are disabled via `OPENSLIDE_SKIP_DOWNLOAD` — used by CI once the
declared fixture size exceeds its budget.

## Status

Core library, I/O layer, and eleven vendor drivers are implemented. Python
bindings (PyO3), the CLI, and the C-compatible FFI crate are planned — see
[`PROGRESS.md`](PROGRESS.md) for the detailed checklist.

## License

LGPL-2.1, matching upstream OpenSlide.
