# slidekit

A Rust implementation of [OpenSlide](https://openslide.org/) for reading
whole-slide images (WSI) in digital pathology. Ground-up rewrite of the C
library with 1:1 API parity, and streaming/remote file support (S3, GCS, Azure,
HTTP) without full download.

Every codec is pure Rust except JPEG 2000, which decodes through OpenJPEG so
that it matches OpenSlide bit-for-bit; a pure-Rust build is still one feature
flag away. See [Compression / decoders](#compression--decoders).

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
| 3DHistech MIRAX (`.mrxs`) | ✅ | Multi-file; JPEG + PNG tiles; fluorescence deferred |
| Zeiss CZI (`.czi`) | ✅ | Uncompressed / Zstd0 / Zstd1; **JPEG XR not supported** ¹ |
| DICOM WSI (`.dcm`) | ✅ | Directory of instances; JPEG, JPEG 2000, JPEG XL, uncompressed ² |
| Synthetic | ✅ | Self-test slides, no file needed (`OPENSLIDE_DEBUG=synthetic`) |
| Sakura | ❌ | Planned (needs a SQLite reader); no public test slide exists |

¹ Matches OpenSlide's own C driver, which also decodes only uncompressed/Zstd
and rejects JPEG XR. JXR is common for brightfield CZIs but has no pure-Rust
decoder; such files are detected as Zeiss but fail to open with a clear error
rather than producing black tiles.

² slidekit also decodes the JPEG XL transfer syntaxes, which OpenSlide 4.0 does
not. Multi-focal-plane (z-stack) and multi-optical-path (fluorescence) DICOM
slides remain deferred.

## API surface

Mirrors the OpenSlide C API. All methods below are implemented on `OpenSlide`.

| Area | Methods |
|------|---------|
| Open | `open`, `open_url`, `open_store`, `open_fs`, `open_url_async`, `open_store_async`, `detect_vendor` |
| Levels | `level_count`, `level0_dimensions`, `level_dimensions`, `level_downsample`, `best_level_for_downsample` |
| Read | `read_region`, `read_region_async`, `read_region_into` |
| Properties | `property_names`, `property_value` |
| Associated images | `associated_image_names`, `associated_image_dimensions`, `read_associated_image` |
| Color profiles | `icc_profile`, `icc_profile_size`, `associated_image_icc_profile`, `associated_image_icc_profile_size` |
| Misc | `set_cache`, `version` |

`error()` (C-API error-state string) is the one API not yet ported.

The colour-profile accessors return `Result<Option<&[u8]>>` rather than a plain
`Option`, because they may read: a DICOM profile can be 13 MB and is not fetched
at open. `*_size` reports how large one is without fetching it — the same split
the C API makes between `openslide_get_icc_profile_size` and
`openslide_read_icc_profile`.

## Compression / decoders

| Codec | Status | Backend |
|-------|:------:|---------|
| JPEG | ✅ | `jpeg-decoder` |
| LZW (TIFF) | ✅ | `weezl` + predictor |
| Deflate / ZIP (TIFF) | ✅ | `miniz_oxide` |
| PackBits (TIFF) | ✅ | in-tree |
| Zstd v0 / v1 (CZI) | ✅ | `ruzstd` (pure Rust) |
| Uncompressed (CZI BGR/gray, DICOM RGB) | ✅ | in-tree |
| JPEG 2000 | ✅ | `openjpeg-sys` via `jpeg2k` (default) — or `hayro-jpeg2000` (pure Rust) with `--features hayro` |
| JPEG XL | ✅ | `jxl-oxide` (pure Rust) — DICOM `…1.2.4.140`/`.141`/`.142`, TIFF 50002/52546 |
| PNG | ✅ | `png` (pure Rust) |
| JPEG XR (CZI) | ❌ | No pure-Rust decoder |
| BMP | ❌ | Planned (rare MIRAX levels) |

`cargo add slidekit` is the whole install: there is no system package for a
user to find and no `cmake`/`pkg-config` step. Every codec but JPEG 2000 is pure
Rust with no build script at all.

### Why JPEG 2000 links C

Because the pure-Rust option is measurably wrong on the slides that matter.
OpenSlide decodes JPEG 2000 with OpenJPEG; `hayro-jpeg2000` 0.4 does not
reproduce it on aggressively rate-controlled tiles, which is exactly what Aperio
ships (Kakadu `Q=30`, ~1:27). Scored against the *original* image rather than
against OpenSlide, OpenJPEG reaches 42.99 dB PSNR where `hayro` reaches
34.97 dB — 8 dB of quality the encoder already paid for, discarded. With
OpenJPEG both Aperio JP2K bench slides are bit-identical to OpenSlide, 0.00 mean
and 0 max, at roughly +18% decode time on one of them and none on the other.
The measurement is in [docs/benchmarks.md](docs/benchmarks.md).

`openjpeg-sys` vendors the OpenJPEG C sources and builds them with `cc`: it
needs a C compiler, but no cmake, no pkg-config and no system library. (The
third option, `openjp2` — OpenJPEG machine-translated to Rust — exposes no
in-memory stream constructor and hits undefined behaviour tearing down a tile on
real Aperio slides.)

For targets that cannot link C at all, `--no-default-features --features hayro`
restores the pure-Rust decoder and its accuracy cost. Building with neither
backend is a compile error rather than a silent fallback.

The only other C a *default* build compiles is `aws-lc-sys`, the TLS backend
that `object_store` brings in for the optional `object-store` feature. It has
nothing to do with image decoding, and turning that feature off removes it —
along with the Apple framework bindings (`core-foundation-sys`,
`security-framework-sys`) the same TLS stack pulls in on macOS, which link
against the OS rather than building anything.

## I/O backends

| Backend | Status | Notes |
|---------|:------:|-------|
| Local file | ✅ | `pread`-based positioned reads |
| S3 / GCS / Azure / HTTP | ✅ | Byte-range requests via `object_store`; no full download |
| Custom (`SlideFs`) | ✅ | Implement the trait for any other storage |
| `mmap` local reader | ❌ | Planned optimization |

Enabled by default via the `object-store` feature (disable for a
local-file-only build with no async/cloud dependencies).

### Multi-file formats

MIRAX (`.mrxs` plus a sibling data directory) and DICOM (a directory of
instances sharing a SeriesInstanceUID) need to resolve and enumerate files, not
just read one. They do that through `SlideFs`, an `fsspec`-style namespace with
`open` / `read` / `list` / `exists` over `/`-separated relative paths, so both
formats open from blob storage on the same `open_url` / `open_store` call as
any single-file slide:

```rust
let slide = OpenSlide::open_url("s3://bucket/slides/CMU-1.mrxs")?;
```

Opening is kept to as few requests as the format allows: one directory listing
supplies the size of every data file, so readers are built without a `HEAD`
each, metadata files are fetched whole in a single `GET`, and DICOM's directory
scan runs concurrently rather than one instance at a time. The filesystem is
consulted only while opening — `read_region` reads through the readers the
driver already holds, so nothing changes on the local path.

To read from storage this crate doesn't know about, implement `SlideFs` and
open with `OpenSlide::open_fs(fs, "slides/CMU-1.mrxs")`.

## Building

```sh
cargo build
cargo test    # unit tests run always; integration tests auto-download
              # fixtures from the OpenSlide server, or skip if unavailable
```

Integration tests fetch real slides from the OpenSlide test-data server and
verify SHA-256 checksums. They skip gracefully (rather than fail) when
downloads are disabled via `OPENSLIDE_SKIP_DOWNLOAD`, which is how CI runs
them (`mise run test:unit`). `mise run test` also skips them if the declared
fixture size exceeds its 10 GiB budget.

## Benchmarks

Two benches, both `#[ignore]`d so they never run in the normal suite, and both
assuming a warm slide cache (`mise run bench:warm-cache`).

```sh
mise run bench:setup-slidetool  # build OpenSlide 4.0.1 into .tools (once)
mise run bench:openslide        # slidekit vs OpenSlide, one slide per vendor

mise run bench:upload-slides    # mirror the bench slides into a bucket (once)
mise run bench:remote           # the same slides read over byte-range requests
```

`bench:openslide` reports open and read latency for both readers and a tolerant
pixel cross-check (independent JPEG decoders differ by ±1–2 per channel, so
exact equality would flake). Note that slidekit is timed in-process while
`slidetool` can only be timed as a whole process, so its open/read columns are
derived by subtracting a measured process floor and a `prop list` run; the read
column still includes a PNG encode slidekit never does. A like-for-like read
comparison needs the in-process FFI crate, which is not built yet.

`bench:remote` reads the same slides from an S3-compatible bucket, configured
from a git-ignored `.env` (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
`BUCKET_NAME`, `AWS_REGION`, `AWS_ENDPOINT_URL`); everything is confined
to one key prefix (`SLIDEKIT_BENCH_PREFIX`, default `slidekit-bench/testdata`)
so a shared bucket is safe. It asserts remote pixels equal local pixels
*exactly* — same decoder over the same bytes — and counts the requests and bytes
each phase costs. `SLIDEKIT_BENCH_ONLY=Aperio` narrows the run to one vendor and
`SLIDEKIT_BENCH_TRACE=<KiB>` logs every request at least that large.

Measured over a ~583 ms-per-request endpoint: opening a slide and reading a
1024×1024 region fetches **0.18–2.4% of it** on ten of eleven vendors —
including 0.18% of a 2.5 GB Ventana — so nothing is downloaded wholesale. The
cost that remains is round trips rather than bytes, so the reads are batched and
block-buffered rather than issued field-by-field: a remote open takes **10–29
requests** where it used to take 164–2665.

Full results for every vendor — both benches, with methodology, caveats, the
defects they surfaced, and what is still outstanding — are in
[`docs/benchmarks.md`](docs/benchmarks.md).

## Status

Core library, I/O layer, and eleven vendor drivers are implemented. Python
bindings (PyO3), the CLI, and the C-compatible FFI crate are planned — see
[`PROGRESS.md`](PROGRESS.md) for the detailed checklist.

## License

LGPL-2.1, matching upstream OpenSlide.
