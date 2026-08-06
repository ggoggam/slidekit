# OpenSlide Rust — Implementation Progress

## Phase 0: Project Scaffolding

- [x] Cargo workspace root (`Cargo.toml`, resolver = "2")
- [x] Workspace-level dependencies pinned (`thiserror`, `sha2`, `lru`, `parking_lot`, `quick-xml`, `bytes`, `jpeg-decoder`, `png`, `hayro-jpeg2000`, `jxl-oxide`, `weezl`, `object_store`, `tokio`, `url`) — every image codec is pure Rust, so the crate links no C and needs no system library at install time
- [x] `crates/slidekit` crate created
- [x] `mise.toml` tooling config
- [ ] `crates/slidekit-sys` — C-compatible FFI crate
- [ ] `crates/slidekit-python` — PyO3 crate + `pyproject.toml`
- [ ] `crates/slidekit-cli` — CLI crate
- [ ] Test data downloader (auto-fetch from `openslide.cs.cmu.edu`, SHA-256 verify, cache in `target/testdata/`)
- [ ] Vendor C test YAML configs (`test/cases/`) — needed by `test_cases.rs`

---

## Phase 1: Core API + Test Harness

### 1a. Public API (`slide.rs`)

- [x] `OpenSlide::open(path)` — local file
- [x] `OpenSlide::open_url(url)` — synchronous URL open (blocks on async)
- [x] `OpenSlide::open_store(store, path)` — `object_store` backend
- [x] `OpenSlide::open_fs(fs, path)` — any `SlideFs` implementation
- [x] `OpenSlide::open_url_async(url)` — async URL open
- [x] `OpenSlide::open_store_async(store, path)` — async `object_store` open
- [x] `OpenSlide::detect_vendor(path)` — format detection without full open
- [x] `OpenSlide::level_count()`
- [x] `OpenSlide::level0_dimensions()`
- [x] `OpenSlide::level_dimensions(level)`
- [x] `OpenSlide::level_downsample(level)`
- [x] `OpenSlide::best_level_for_downsample(downsample)`
- [x] `OpenSlide::read_region(x, y, level, w, h)` — sync
- [x] `OpenSlide::read_region_async(x, y, level, w, h)` — async
- [x] `OpenSlide::read_region_into(dest, x, y, level, w, h)` — zero-copy variant
- [x] `OpenSlide::property_names()`
- [x] `OpenSlide::property_value(name)`
- [x] `OpenSlide::associated_image_names()`
- [x] `OpenSlide::associated_image_dimensions(name)`
- [x] `OpenSlide::read_associated_image(name)`
- [x] `OpenSlide::icc_profile()` — `Result<Option<&[u8]>>`; read on demand
- [x] `OpenSlide::icc_profile_size()` — size without reading
- [x] `OpenSlide::associated_image_icc_profile(name)`
- [x] `OpenSlide::associated_image_icc_profile_size(name)`
- [x] `OpenSlide::set_cache(cache)`
- [x] `OpenSlide::version()`
- [ ] `OpenSlide::error()` — error-state string (C API compat)

### 1b. Supporting Types

- [x] `OpenSlideError` + `Result<T>` (`error.rs`)
- [x] `LevelInfo` (`level.rs`)
- [x] `Properties` storage (`properties.rs`)
- [x] `AssociatedImage` placeholder (`associated.rs`)
- [x] `Cache` (LRU, configurable capacity) (`cache.rs`)
- [x] ARGB color helpers (`color.rs`)

### 1c. Test Coverage

- [x] `test_cases.rs` — version, open errors, detect-vendor errors (3/5 passing)
- [ ] `test_cases.rs` — `all_vendors_represented` (needs `cases/` dir)
- [ ] `test_cases.rs` — `parse_all_test_case_configs` (needs `cases/` dir)
- [x] `test_aperio.rs` — open, level info, properties, associated images, ICC, read_region (18 tests, all passing)
- [x] `test_trestle.rs` — open, level info, properties, read_region
- [x] `test_generic_tiff.rs` — basic open + level info
- [x] `test_tiff.rs` — TIFF parser unit tests
- [x] `test_jpeg.rs` — JPEG decoder unit tests
- [x] `test_grid.rs` — SimpleGrid unit tests
- [x] `test_read_region.rs` — region reads, transparent-fill for missing tiles
- [x] `test_read_region_async.rs` — async region reads via `object_store`
- [~] `test_ventana.rs` — detect/open/levels/read_region; gated (skips) until a Ventana fixture is added to `testdata.yaml`
- [x] `test_leica.rs` — detect/open/levels/properties/read_region/macro against `Leica/Leica-1.scn` (6 tests, passing; gated on download)
- [x] `test_hamamatsu.rs` — detect/open/levels/properties/read_region (restart-marker tiling)/macro against `Hamamatsu/CMU-1.ndpi` (6 tests, passing; gated on download)
- [x] `test_mirax.rs` — detect/open/levels/properties/associated/read_region against `Mirax/CMU-1.zip` (6 tests, passing; gated on download)
- [x] `test_zeiss.rs` — detect/open/pyramid/properties/read_region against `Zeiss/Zeiss-5-Uncompressed.czi` (brightfield pyramidal), plus Zstd0 and Zstd1-HiLo SlidePreview fixtures and a JXR-rejection assertion (8 tests, passing; gated on download)
- [x] `test_dicom.rs` — detect/open/levels/properties/associated/ICC/read_region against `DICOM/Leica-4.zip` (TILED_SPARSE, JPEG YCbCr + JPEG RGB + uncompressed label) and `DICOM/Hamamatsu-2.zip` (TILED_FULL) (2 tests, passing; gated on download)
- [ ] `test_properties.rs` — property names/values per vendor (not yet created)
- [ ] `test_associated.rs` — associated image dimensions per vendor (not yet created)
- [ ] `test_cache.rs` — LRU eviction, shared cache (not yet created)
- [ ] `test_streaming.rs` — mock HTTP server, byte-range efficiency (not yet created)
- [ ] Vendor-specific test files for remaining 9 formats (not yet created)

---

## Phase 2: I/O Layer + Format Detection

### 2a. I/O Backends (`io/`)

- [x] `SlideReader` trait — `read_at(offset, buf)` + `size()` (`io/mod.rs`)
- [x] `AsyncSlideReader` trait — async `read_at` + `size()` (`io/mod.rs`)
- [x] Local file reader — `pread`-based (`io/local.rs`)
- [x] `object_store` async backend — S3, GCS, Azure Blob, HTTP range requests (`io/object_store.rs`)
- [x] `SlideFs` trait — `fsspec`-style namespace (`open`/`read`/`list`/`exists` over `/`-separated relative paths) that multi-file formats resolve siblings through; `SlideLocation` replaces the old `SlideReader::path()` (`io/fs.rs`). Path validation rejects `..`/absolute paths, since MIRAX takes data-file names from inside `Slidedat.ini`
- [x] `LocalFs` — directory-rooted `SlideFs` (`io/local.rs`)
- [x] `ObjectStoreFs` — store-rooted `SlideFs`, so MIRAX/DICOM open from S3/GCS/Azure/HTTP; `open_sized` builds readers from listing sizes to avoid a `HEAD` per file (`io/object_store.rs`)
- [x] Block read-ahead reader (`io/readahead.rs`) — wraps a `SlideReader` and serves
      small reads from cached blocks, growing on sequential access; applied to
      `object_store` readers only. See Phase 6
- [x] `SlideReader::read_ranges` — batched gather, so a caller that knows every
      range it needs says so once. Overridden by `BlockingReader` (drives
      `object_store::get_ranges`) and `ReadAhead`; **wrappers must forward it**
- [ ] `mmap`-based local reader (performance optimization)
- [ ] Mock reader for unit tests

### 2b. TIFF-like Parser (`decode/tiff.rs`)

- [x] `TiffLike` struct — IFD chain parsing
- [x] Classic TIFF support (32-bit offsets)
- [x] BigTIFF support (64-bit offsets)
- [x] NDPI high-bit offset fixup (`uint_fix_offset_ndpi`)
- [x] `directory_count()`, `is_ndpi()`
- [x] `value_count(dir, tag)`
- [x] `get_uint / get_uints` — unsigned integer tag reads
- [x] `get_sint` — signed integer tag reads
- [x] `get_float / get_floats` — float tag reads
- [x] `get_buffer` — raw bytes tag reads
- [x] `get_string` — ASCII tag reads
- [x] `has_tag(dir, tag)`
- [x] `is_tiled(dir)` / `is_stripped(dir)`
- [x] `read_tile_data(dir, tile_index)` — fetches compressed tile bytes
- [x] `read_strip_data(dir)` — concatenated strip bytes
- [x] `read_strips(dir)` — individual strips
- [x] `rows_per_strip(dir)`
- [x] `tile_location(dir, tile_index)` — returns `(offset, length)`
- [x] `tile_byte_counts(dir)`
- [x] `preload_tile_tables(dir)` — prefetch offset/length tables
- [x] `compression(dir)` — compression scheme tag
- [x] `jpeg_tables(dir)` — shared JPEG tables tag (0x015B)
- [x] `read_icc_profile(dir)` — ICC profile tag (0x8773)
- [x] LZW decompression (via `weezl`, plumbed into tile decode path) — `decode/lzw.rs` (`LzwDecoder` + predictor + ARGB pack), wired into generic_tiff/aperio/trestle
- [x] Deflate/ZIP decompression — `decode/deflate.rs` (`DeflateDecoder` via `miniz_oxide`, compression 8 + 32946)
- [x] PackBits decompression — `decode/packbits.rs` (`PackBitsDecoder`, compression 32773)

### 2c. Format Detection Pipeline (`vendor/mod.rs`)

- [x] `VendorDriver` trait — `name`, `vendor`, `detect`, `open`
- [x] `all_drivers()` — registry of active drivers
- [x] `detect_format()` — iterates drivers in priority order
- [ ] Full detection order matching C (synthetic → mirax → zeiss → dicom → hamamatsu → sakura → trestle → aperio → leica → philips → ventana → generic_tiff) — only `sakura` is still missing from the registry

---

## Phase 3: Decoders (`decode/`)

- [x] `TileDecoder` trait
- [x] JPEG decoder (`decode/jpeg.rs`) — YCbCr→ARGB conversion, JPEG tables prepending
- [x] JPEG 2000 decoder (`decode/jp2k.rs`) — `hayro-jpeg2000` (pure Rust, memory-safe). Handles raw J2K codestreams and JP2-boxed images, 1/3/4 components, and both Aperio flavours: 33005 (components already RGB) and 33003 (YCbCr with chroma subsampled 2:1 horizontally and no transform flagged in the codestream — the container is the only thing that says so). YCbCr→RGB reproduces OpenSlide's fixed-point `make-tables.c` tables exactly. Wired into aperio (33003/33005), generic_tiff (34712), and dicom (`…1.2.4.90`/`.91`). Cross-checked against `slidetool` on both JP2K Aperio slides: mean per-channel diff 4.5 / 3.4 with a *signed* mean of ~0, i.e. unbiased 9/7-wavelet reconstruction scatter, not a colour error. **Rejected alternatives:** `openjp2` (OpenJPEG via c2rust) has no in-memory stream constructor in its public API and hits UB in `opj_tcd_code_block_dec_deallocate` on real Aperio tiles; `openjpeg-sys` would put a vendored C build in every downstream consumer's build graph
- [x] PNG decoder (`decode/png.rs`) — `png` crate; RGB/RGBA/gray/gray-alpha, 8- and 16-bit. Wired into mirax (PNG zoom levels) and generic_tiff (compression 34933)
- [x] JPEG XL decoder (`decode/jxl.rs`) — `jxl-oxide` (pure Rust). Wired into dicom (`…1.2.4.140`/`.141`/`.142`) and generic_tiff (compression 50002 and 52546). OpenSlide 4.0 has no JPEG XL support at all
- [ ] BMP decoder (`decode/bmp.rs`)
- [x] LZW decompression (TIFF strips/tiles) — `decode/lzw.rs`
- [x] Deflate/ZIP decompression (TIFF) — `decode/deflate.rs`
- [x] PackBits decompression (TIFF) — `decode/packbits.rs`
- [x] Zstd v0 / v1 decompression (Zeiss CZI) — pure-Rust `ruzstd`, inline in `vendor/zeiss.rs` (Zstd1 header parse + HiLo byte unpack)
- [x] XML metadata parser (`decode/xml.rs`) — minimal element tree over `quick-xml` (`Node` + `parse_tree` + `children_named`/`find_child`/`descend`), shared by Philips and Ventana
- [ ] SQLite reader (`decode/sqlite.rs`) — for Sakura (Leica turned out to be TIFF+XML, not SQLite)
- [x] DICOM parser (`decode/dicom.rs`) — in-tree, no `dicom-rs` dependency: preamble + file-meta group, explicit-VR little-endian datasets, nested/undefined-length sequences, and pixel data recorded **by location** (native blob, or encapsulated fragments via basic/extended offset table) so multi-GB slides never load their pixels. Per-frame functional groups are reduced to a frame→position list. Curated tag→keyword dictionary for the property dump

---

## Phase 4: Grid System + Cache (`grid/`)

- [x] `TileGrid` trait — `tiles_for_region(x, y, w, h)`
- [x] `SimpleGrid` — regular rectangular tile layout (`grid/simple.rs`)
- [x] `TileMap` — arbitrary-offset placed (sub)tiles (`grid/tilemap.rs`); built by Ventana from AOI stitch geometry, consumed by `read_region` via an additive `LevelData.tilemap` path. (Mirax needs its own non-TIFF byte source, so it uses a separate `LevelData.mirax` / `MiraxLevel` read path rather than this one.)
- [ ] `RangeGrid` — Z-order / quadtree tiles
- [x] `Cache` — LRU by capacity, `parking_lot::Mutex` (`cache.rs`)
- [ ] Shared cache across multiple `OpenSlide` instances

---

## Phase 5: Vendor Drivers (`vendor/`)

> **Per-vendor requirement (definition of done):** every vendor driver must ship
> both of the following before it counts as complete. A `[x]` for a real-file
> format is not done until both exist (or the gap is called out explicitly, e.g.
> a multi-GB fixture not yet in `testdata.yaml`):
> 1. **Integration test against a real test slide** — an end-to-end `test_<vendor>.rs`
>    (detect → open → levels → properties → `read_region` → associated images)
>    run against a real fixture auto-downloaded + SHA-256-verified via `testdata.yaml`.
> 2. **Benchmark vs. the OpenSlide test suite** — a case in `bench_openslide.rs`
>    that cross-checks pixels against the reference OpenSlide `slidetool` output
>    (tolerant diff for independent JPEG decoders) and records open/read latency.
>
> `synthetic` is the sole exception: it has no real file, so its `test_synthetic.rs`
> self-test and embedded payloads stand in for both.

- [x] `generic_tiff` — generic TIFF fallback (`vendor/generic_tiff.rs`)
- [x] `synthetic` — synthetic test slides (`vendor/synthetic.rs`), no real file needed. Ported from `openslide-vendor-synthetic.c`: embeds tiny (16×16) zlib-compressed per-codec payloads and decodes every one at `open` (the self-test), then lays the rendered images out as one 1-row strip via a `LevelData.synthetic` pre-rendered-image read path. Gated behind an empty path + `OPENSLIDE_DEBUG=synthetic` (matches C). Includes the items whose decoders exist here — JPEG (YCbCr/RGB + corrupt-JPEG negative test), tiled JPEG/LZW classic+BigTIFF (via an in-memory `SlideReader`), XML (non-image), Zstd (raw BE-ARGB); **BMP/DICOM items deferred**; the PNG and JPEG-2000 items are now unblocked by `decode/png.rs` and `decode/jp2k.rs`. `test_synthetic.rs` (1 test, no download needed).
- [x] `aperio` — Aperio SVS (`vendor/aperio.rs`) — detect + open + properties + associated images + ICC + tile decode
- [x] `trestle` — Trestle TIFF (`vendor/trestle.rs`) — detect + open + properties + tile decode
- [x] `philips` — Philips TIFF (`vendor/philips.rs`) — detect + open + XML-driven level fixup + properties (mpp, objective) + associated images + tile decode
- [x] `ventana` — Roche Ventana BIF (`vendor/ventana.rs`) — detect (XMLPacket + iScan) + open + initial-XML properties (objective power, mpp) + level enumeration (level=/mag= validation) + AOI stitch geometry → `TileMap` + associated images (macro/thumbnail) + ICC. Geometry/parse unit-tested; end-to-end `test_ventana.rs` gated on a Ventana fixture not yet in `testdata.yaml` (multi-GB; needs a real sha256/size)
- [x] `leica` — Leica SCN (`vendor/leica.rs`) — TIFF+XML (not SQLite); detect (`scn` namespace in dir-0 ImageDescription) + open + XML-driven level selection from the main image's `<dimension ifd=…>` children + properties (objective power, mpp, `leica.*` metadata, barcode) + macro associated image (via the new tiled-image decode path) + ICC. Single brightfield main image fully supported. A level is the whole **collection canvas** with the region placed on it at `openslide.bounds-x`/`-y`, matching the C driver, so slidekit and OpenSlide coordinates address the same pixels (nm-per-pixel = `view.sizeX / dimension.sizeX`, x scale for both axes; level = `ceil(collection.size / that)`; origin = `floor(view.offset / that)`); `openslide.bounds-*` and `openslide.region[0].*` are set. Multi-region SCN (several main images composited on that canvas) is still deferred — the read path keys one IFD per level, so `open` errors clearly. XML parse/classification unit-tested; end-to-end `test_leica.rs` against `Leica/Leica-1.scn` (6 tests passing)
- [ ] `sakura` — Sakura (SQLite-based). **Blocked on test data:** the OpenSlide test-data server has no `Sakura/` directory, so a driver could not meet the per-vendor definition of done (no fixture to integration-test or bench against)
- [x] `hamamatsu` — Hamamatsu NDPI (`vendor/hamamatsu.rs`) — detect (TIFF + tag 65420) + open + directory classification by source lens (>0 pyramid, -1 macro) + z=0 focal-plane pyramid + **restart-marker JPEG tiling** (each level is one giant single-strip JPEG; tiles reconstructed as header + one restart interval's entropy + EOI, SOF resized, decoded by the stock `JpegDecoder`) via a new `LevelData.ndpi` read path + `NDPI_MCU_STARTS` tag fast-path with lazy restart-marker scan fallback + properties (objective, X/Y offsets, reference, `NDPI_PROPERTY_MAP` → `hamamatsu.*`, mpp) + macro associated image + ICC. JPEG-marker parse / MCU geometry / restart-marker scan / property-map unit-tested; end-to-end `test_hamamatsu.rs` against `Hamamatsu/CMU-1.ndpi` (6 tests passing). **VMS/VMU** (multi-file index formats, need a multi-blob reader), **z-stacks**, and **`scale_denom` derived levels** deferred
- [x] `mirax` — 3DHistech MRXS (`vendor/mirax.rs`) — multi-file format (`.mrxs` + sibling `Foo/` dir of `Slidedat.ini`, `Index.dat`, `Data####.dat`). Reaches sibling files through the slide's `SlideFs` (`io/fs.rs`), so it opens from a local directory or blob storage alike; one directory listing sizes every `Data####.dat` so the readers are built without a metadata request each. Parses the INI (levels/datafiles/nonhier groups) + the binary `Index.dat` (hier data pages → `(image_index, offset, length, fileno)` tile entries; nonhier records → associated images + `VIMSLIDE_POSITION_BUFFER`) + the 9-byte camera-position buffer. Builds per-level placed-subtile lists via faithful `mirax.c` geometry (cumulative concat exponents, subtile expansion, `get_tile_position` camera-position math, level-0 active-position filtering), rendered through a new `LevelData.mirax` read path (`MiraxLevel` owns the data-file readers + decodes standalone JPEG blobs). Properties (`mirax.*` dump, objective, mpp, background) + macro/label/thumbnail associated images. INI/nonhier-offset/base-dim/position unit-tested; end-to-end `test_mirax.rs` against `Mirax/CMU-1.zip` (6 tests passing; 10 levels, 109240×220696, verified against the thumbnail). **Fluorescence** (multi-channel), **BMP tiles**, **`StitchingIntensityLayer` (zlib) position fallback**, and **openslide.bounds-\*** deferred; extreme-zoom levels show faint subtile seams from integer (vs cairo subpixel) compositing
- [x] `zeiss` — Zeiss CZI (`vendor/zeiss.rs`) — ZISRAW segmented container (not TIFF): file-header → subblock-directory parse (`DirectoryEntryDV` + `DimensionEntryDV`), pyramid levels grouped by per-subblock downsample `round(Size/StoredSize)`, origin-normalized placed-subtile placement (CZI mosaic coords are not zero-based — a scene sits at negative X) via a new `LevelData.zeiss` / `ZeissLevel` read path + properties (mpp from Scaling metadata, objective power) + JPEG label/macro/thumbnail attachments (attachment-directory parse). **Compression parity with OpenSlide's own `zeiss.c`:** decodes uncompressed (Gray8/16, BGR24/48, BGRA32 → ARGB), Zstd0, and Zstd1 (header parse + HiLo byte unpack) via the pure-Rust `ruzstd`; **JPEG XR / JPEG / LZW are rejected on open with a clear error** (OpenSlide lacks a JXR decoder too, and there is no pure-Rust one). Binary-parse / BGR-conversion / HiLo-unpack unit-tested; end-to-end `test_zeiss.rs` against real fixtures — `Zeiss-5-Uncompressed.czi` (brightfield pyramidal, 5 levels, 50171×11340, two scenes), `Zeiss-5-SlidePreview-Zstd0.czi`, `Zeiss-5-SlidePreview-Zstd1-HiLo.czi`, and `Zeiss-5-JXR.czi` (asserts the rejection) — 8 tests passing. **Multi-channel fluorescence** (only first C/Z/T plane used), **JPEG XR tiles**, **embedded-CZI associated images**, and **`openslide.bounds-*`** deferred
- [x] `dicom` — DICOM WSI (`vendor/dicom.rs`) — a slide is a *directory* of SOP instances sharing a SeriesInstanceUID, so `open` enumerates the containing directory through the slide's `SlideFs` (`io/fs.rs`) — local or blob storage alike — parsing candidate instances concurrently (each is several round trips) and sorting them by `ImageType[2]`: `VOLUME` → pyramid level, `LABEL`/`OVERVIEW`/`THUMBNAIL` → label/macro/thumbnail. Tiles are the instance's *frames*, indexed either row-major (`TILED_FULL`) or by explicit per-frame plane position (`TILED_SPARSE`, holes left transparent), through a new `LevelData.dicom` / `DicomLevel` read path; levels split across a *concatenation* look the tile up in each member (missing/duplicate members error). Format checks match `dicom.c` (PlanarConfiguration 0, 8-bit, 3 samples, single focal plane) + properties (whole dataset dumped as `dicom.*` with C's `Seq[i].Key[j]` naming, mpp from PixelSpacing×1000, objective power, barcode) + ICC from the first optical path. **Baseline JPEG (YBR_FULL_422 and RGB), JPEG 2000 (`…1.2.4.90`/`.91`), JPEG XL (`…1.2.4.140`/`.141`/`.142`) and uncompressed RGB all decode** — JPEG XL is beyond what OpenSlide 4.0 reads at all. Parser and frame/tile-index unit-tested; end-to-end `test_dicom.rs` against `DICOM/Leica-4.zip` + `DICOM/Hamamatsu-2.zip` (2 tests passing) and both are cross-checked against `slidetool` in `bench_openslide.rs`. **Multi-focal-plane (z-stack) and multi-optical-path (fluorescence) slides** are deferred

---

## Phase 6: Streaming Optimization

- [x] **Block read-ahead for remote readers** (`io/readahead.rs`) — small reads
      are served from cached blocks, so a TIFF directory parse costs one fetch per
      IFD region instead of one ranged GET per tag. The block doubles on
      sequential access (up to 4 MiB) and resets to 64 KiB on anything else, which
      is what lets a DICOM fragment walk or an NDPI restart-marker scan finish
      remotely at all without a random tile read pulling megabytes. Bounded at
      16 MiB per reader; clones share one cache; `object_store` readers only —
      local files already have the page cache. Took remote open from 164–2665
      requests to 10–29 across the vendor set (see `docs/benchmarks.md`)
- [x] **Batched range requests** — `SlideReader::read_ranges` gathers a set of
      ranges into one call; `BlockingReader` hands it to `object_store::get_ranges`
      (which coalesces neighbours and parallelizes the rest) and `ReadAhead` serves
      cache hits and batches the misses. Used by `read_region` (TIFF), the DICOM
      frame path, and the Zeiss subblock-header pass. Region reads across the
      bench set: 144 → 32 requests; Zeiss open 79.7 s → 11.9 s.
      **Note for future wrappers:** `Located`/`Shared` must forward it explicitly,
      or the trait default silently unrolls the batch into one read per range
- [x] Lazy tile loading — `read_region` fetches tiles and nothing else; DICOM
      frame offsets are resolved on demand rather than walked at open
      (`decode/dicom::FrameIndex`), and DICOM ICC profiles are recorded by
      location rather than loaded (`Value::Bulk`)
- [ ] Adaptive cache sizing for remote vs. local files — the read-ahead block
      sizes were chosen to fit the access patterns of the eleven bench slides and
      validated by request counts, not swept
- [ ] Connection pooling for HTTP backend — note the client already reuses
      connections; a keep-alive probe against the bench endpoint measured ~583 ms
      per request on an *established* connection vs ~50 ms network RTT, so
      per-request latency there is the endpoint's, not a missing pool
- [x] Byte-range efficiency tests (mock HTTP server via `wiremock`) —
      `tests/test_streaming.rs`, including a request-count assertion that a
      synchronous region read coalesces its tiles

### Benchmark harness (supporting Phase 6 + the per-vendor bench requirement)

- [x] `tests/bench_openslide.rs` — slidekit vs OpenSlide 4.0.1 `slidetool`, one
      slide per vendor, with open/read latency separated on both sides (a
      measured process floor and a `prop list` run are subtracted so slidetool's
      startup tax is not charged to the library) and a tolerant pixel diff. All
      11 cases pass: mean per-channel diff ≤ 0.17 except Ventana (2.51)
- [x] `tests/bench_remote.rs` — the same slides read from an S3-compatible
      bucket, counting requests/bytes per phase and asserting remote pixels
      equal local pixels *exactly*. Byte accounting excludes zero-body HEAD
      probes (`ObjectStoreExt::head` is `get_opts` with `head: true`, whose
      response still reports the whole-object range — counting it made a lazy
      open look like a full download)
- [x] `tests/upload_testdata.rs` — mirrors the bench set into the bucket
      (multipart above 32 MiB, skips objects already present at the same size,
      never deletes). The ZIP-packaged vendors are uploaded **extracted**, which
      is what makes MIRAX and DICOM openable remotely at all
- [x] Shared fixture resolution in `tests/common/mod.rs` (`BENCH_CASES`,
      `bench_slide`, `extracted_slide_dir`, `tissue_region`) so the two benches
      and the uploader agree on one slide set, and the ZIP vendors self-extract
      instead of requiring their test binaries to have been run first

### Defects surfaced by the bench

Fixed, with the measurements that caught them and now guard them:

- [x] **DICOM remote open transferred the whole slide** — 84–94% of it, one
      64 KiB request per frame: `DICOM/Hamamatsu-2` opened in **2665 requests /
      165.8 MB of 198 MB (1171 s)**. Three causes, all fixed. The fragment walk
      ran at open for every instance (now deferred to `FrameIndex`, incremental
      and memoized); it ran even for instances carrying an Extended Offset Table,
      which the old code only consulted *after* walking to it (Hamamatsu-2 has one
      for every instance, so it now never walks — 19 requests / 1.32 MB); and a
      13.1 MB ICC profile embedded in *each* instance was materialised for all of
      them, 49 MB of duplicates per open (now `Value::Bulk`, read from one
      instance on demand). Where a walk is genuinely unavoidable — no offset table
      of either kind, as on `DICOM/Leica-4` — it is now a sequential scan in
      growing blocks, 11 requests rather than thousands
- [x] **`open_url` ignored the environment**, so it could not reach a non-AWS
      S3-compatible endpoint: `object_store::parse_url` builds
      `AmazonS3Builder::new()` (not `from_env()`) and passes no options, so no
      `AWS_*` variable was read and the open fell through to the EC2 instance
      metadata service, failing after ~2.2 s. Now goes through `parse_url_opts`
      with the environment filtered to the prefixes each builder's own `from_env()`
      consults (`AWS_`/`AZURE_`/`GOOGLE_`, plus `SERVICE_ACCOUNT`/`MSI_ENDPOINT`).
      The filter matters: `parse_url_opts` also accepts bare `region`/`bucket`/
      `endpoint`/`token` keys, so forwarding the whole environment would let an
      unrelated variable redirect a slide read.
      `bench_remote::open_url_resolves_config_from_environment` was a diagnostic
      that printed what happened; it is an assertion now
- [x] **TIFF open was round-trip-bound** — 164–272 requests for well under 1 MB.
      Fixed by the read-ahead layer above; bytes rose modestly (0.11 → 0.62 MB on
      Trestle) in exchange for 180 fewer requests, which is the right trade at
      ~583 ms per request but is a trade, not a free win
- [x] **Hamamatsu NDPI had no remote number** — `NdpiLevel::ensure_start` walks a
      198 MB single-strip JPEG hunting restart markers and had not finished after
      four minutes, so it was excluded from the remote run. It opens in 7.8 s / 14
      requests now, on the same read-ahead growth
- [x] **Leica coordinates were not interchangeable with OpenSlide's.** OpenSlide
      reports `Leica-1.scn` level 0 as the whole collection canvas (53130×153470)
      with the image at `openslide.bounds-x=10778`, `bounds-y=35096`; slidekit
      reported the 36832×38432 region with the origin at the region, so `(0, 0)`
      addressed different pixels in the two libraries — and the compared bench
      window had **0%** of its pixels opaque in both readers, which is what a bare
      passing `0.00` pixel diff would have hidden. A level is now the collection
      with the region placed on it: nm-per-pixel is `view.sizeX / dimension.sizeX`
      (x scale for both axes, as in the C driver), the level is
      `ceil(collection.size / that)`, the origin is `floor(view.offset / that)`.
      Reproduces all five level sizes, both bounds values, and OpenSlide's
      averaged downsamples exactly, asserted in `test_leica`.
      `openslide.bounds-*` and `openslide.region[0].*` are set
- [x] **Edge tiles painted their encoder padding** — found while fixing the
      above. A tile at the right or bottom edge of an image runs past it (a
      36832-wide image in 512-pixel tiles ends 32 pixels into its last column) and
      the padding was composited. Harmless while a level *was* its image, since
      the padding fell outside every valid coordinate; a 32-pixel black band once
      the level got larger. `SimpleGrid::tile_extent` clips tiles to the image, and
      Leica's opaque overlap went 9% → 100%

- [x] **ICC profiles were fetched at open, and DICOM fetched one twice.** Even
      after the parser stopped loading every instance's copy, `DICOM/Leica-4` still
      pulled the 13.1 MB profile for level 0's instance *and* for the thumbnail's
      `associated_image_icc_profile`, because both accessors returned borrowed
      slices captured at open — 24.6 MB of a 30.90 MB open. A profile is now an
      `IccProfile` (`src/icc.rs`): either bytes already in hand (TIFF tag values
      the driver has parsed anyway, kilobytes) or a byte range read on first
      request and cached. The **size** is available without reading, mirroring the
      C API's `openslide_get_icc_profile_size` / `openslide_read_icc_profile`
      split, which is why the accessors now return `Result<Option<&[u8]>>` and gain
      `*_size` companions — returning `None` for both "no profile" and "the fetch
      failed" would hide an I/O error behind something that looks like metadata.
      Leica-4's open: 30.90 → 6.25 MB, 13.5 → 7.9 s. Guarded by
      `test_dicom::open_does_not_read_the_icc_profile`, which counts bytes through
      a wrapping `SlideFs`. Related: the read-ahead layer no longer caches a fetch
      larger than half its budget — a 13 MB profile is not a block, and caching it
      evicted the working set

Outstanding (see `docs/benchmarks.md` for the traces):

- [ ] **`DICOM/Leica-4` fetches 28% of the slide**, all of it in the region read
      (its open is 5.8%). The slide carries no offset table of either kind, so
      fragment offsets form a linked list and reaching a frame mid-way through an
      8004-frame level means reading everything before it — 24 MB here. Now 6 large
      sequential requests instead of thousands of small ones, and memoized per
      instance (warm read 19 ms), but the bytes are unavoidable without a table the
      file does not have. Persisting a derived offset table beside the slide would
      make it once-per-slide rather than once-per-process — a caching layer above
      this library, not a fix inside it
- [ ] **Zeiss open costs 151 requests** — more than the other ten vendors
      combined. The CZI directory says where each subblock is but not where its
      pixels start, so it takes one 16-byte header read per subblock, and they sit
      megabytes apart in a 1 GB file with nothing to coalesce. Batched and
      concurrent now (65.6 → 11.9 s); going further means caching the resolved
      offsets or finding metadata that removes the second pass
- [ ] **`bench_openslide` trusts `PATH`** — run outside `mise` it silently picks up
      Homebrew's `slidetool` 4.0.0 instead of the 4.0.1 in `.tools`, which has no
      Zeiss driver and reports Zeiss as `n/a` with no hint that the wrong binary
      was used. It should assert the version it found

---

## Phase 7: Python Bindings (`crates/slidekit-python/`)

- [ ] `pyproject.toml` + `maturin` build config
- [ ] PyO3 module scaffold
- [ ] `OpenSlide` Python class — `open(path)`, `open(url)` (remote)
- [ ] `.level_count`, `.dimensions` properties
- [ ] `.read_region((x,y), level, (w,h))` → PIL `Image`
- [ ] `.properties` dict-like access
- [ ] `.associated_images` dict-like access
- [ ] `pytest` suite mirroring Rust integration tests
- [ ] CI wheel build matrix (Linux manylinux, macOS arm64 + x86_64, Windows)

---

## Phase 8: CLI + FFI

### CLI (`crates/slidekit-cli/`)

- [ ] Crate scaffold
- [ ] `show-properties <file>`
- [ ] `write-png <file> <x> <y> <level> <w> <h> <output>`
- [ ] `quickhash1sum <file>`

### FFI (`crates/slidekit-sys/`)

- [ ] Crate scaffold
- [ ] `extern "C"` functions matching `openslide.h`
- [ ] `cbindgen` header generation
