# OpenSlide Rust — Implementation Progress

## Phase 0: Project Scaffolding

- [x] Cargo workspace root (`Cargo.toml`, resolver = "2")
- [x] Workspace-level dependencies pinned (`thiserror`, `sha2`, `lru`, `parking_lot`, `quick-xml`, `bytes`, `jpeg-decoder`, `weezl`, `object_store`, `tokio`, `url`)
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
- [x] `OpenSlide::icc_profile()`
- [x] `OpenSlide::associated_image_icc_profile(name)`
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
- [ ] JPEG 2000 decoder (`decode/jp2k.rs`) — `openjp2` or `jpeg2k` crate
- [ ] PNG decoder (`decode/png.rs`)
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
- [x] `synthetic` — synthetic test slides (`vendor/synthetic.rs`), no real file needed. Ported from `openslide-vendor-synthetic.c`: embeds tiny (16×16) zlib-compressed per-codec payloads and decodes every one at `open` (the self-test), then lays the rendered images out as one 1-row strip via a `LevelData.synthetic` pre-rendered-image read path. Gated behind an empty path + `OPENSLIDE_DEBUG=synthetic` (matches C). Includes the items whose decoders exist here — JPEG (YCbCr/RGB + corrupt-JPEG negative test), tiled JPEG/LZW classic+BigTIFF (via an in-memory `SlideReader`), XML (non-image), Zstd (raw BE-ARGB); **BMP/PNG/JPEG-2000/DICOM items deferred until those decoders land**. `test_synthetic.rs` (1 test, no download needed).
- [x] `aperio` — Aperio SVS (`vendor/aperio.rs`) — detect + open + properties + associated images + ICC + tile decode
- [x] `trestle` — Trestle TIFF (`vendor/trestle.rs`) — detect + open + properties + tile decode
- [x] `philips` — Philips TIFF (`vendor/philips.rs`) — detect + open + XML-driven level fixup + properties (mpp, objective) + associated images + tile decode
- [x] `ventana` — Roche Ventana BIF (`vendor/ventana.rs`) — detect (XMLPacket + iScan) + open + initial-XML properties (objective power, mpp) + level enumeration (level=/mag= validation) + AOI stitch geometry → `TileMap` + associated images (macro/thumbnail) + ICC. Geometry/parse unit-tested; end-to-end `test_ventana.rs` gated on a Ventana fixture not yet in `testdata.yaml` (multi-GB; needs a real sha256/size)
- [x] `leica` — Leica SCN (`vendor/leica.rs`) — TIFF+XML (not SQLite); detect (`scn` namespace in dir-0 ImageDescription) + open + XML-driven level selection from the main image's `<dimension ifd=…>` children + properties (objective power, mpp, `leica.*` metadata, barcode) + macro associated image (via the new tiled-image decode path) + ICC. Single brightfield main image fully supported; multi-region SCN (multiple main images composited at collection offsets) deferred — the read path keys one IFD per level, so `open` errors clearly. XML parse/classification unit-tested; end-to-end `test_leica.rs` against `Leica/Leica-1.scn` (6 tests passing)
- [ ] `sakura` — Sakura (SQLite-based). **Blocked on test data:** the OpenSlide test-data server has no `Sakura/` directory, so a driver could not meet the per-vendor definition of done (no fixture to integration-test or bench against)
- [x] `hamamatsu` — Hamamatsu NDPI (`vendor/hamamatsu.rs`) — detect (TIFF + tag 65420) + open + directory classification by source lens (>0 pyramid, -1 macro) + z=0 focal-plane pyramid + **restart-marker JPEG tiling** (each level is one giant single-strip JPEG; tiles reconstructed as header + one restart interval's entropy + EOI, SOF resized, decoded by the stock `JpegDecoder`) via a new `LevelData.ndpi` read path + `NDPI_MCU_STARTS` tag fast-path with lazy restart-marker scan fallback + properties (objective, X/Y offsets, reference, `NDPI_PROPERTY_MAP` → `hamamatsu.*`, mpp) + macro associated image + ICC. JPEG-marker parse / MCU geometry / restart-marker scan / property-map unit-tested; end-to-end `test_hamamatsu.rs` against `Hamamatsu/CMU-1.ndpi` (6 tests passing). **VMS/VMU** (multi-file index formats, need a multi-blob reader), **z-stacks**, and **`scale_denom` derived levels** deferred
- [x] `mirax` — 3DHistech MRXS (`vendor/mirax.rs`) — multi-file format (`.mrxs` + sibling `Foo/` dir of `Slidedat.ini`, `Index.dat`, `Data####.dat`). Reaches sibling files through the slide's `SlideFs` (`io/fs.rs`), so it opens from a local directory or blob storage alike; one directory listing sizes every `Data####.dat` so the readers are built without a metadata request each. Parses the INI (levels/datafiles/nonhier groups) + the binary `Index.dat` (hier data pages → `(image_index, offset, length, fileno)` tile entries; nonhier records → associated images + `VIMSLIDE_POSITION_BUFFER`) + the 9-byte camera-position buffer. Builds per-level placed-subtile lists via faithful `mirax.c` geometry (cumulative concat exponents, subtile expansion, `get_tile_position` camera-position math, level-0 active-position filtering), rendered through a new `LevelData.mirax` read path (`MiraxLevel` owns the data-file readers + decodes standalone JPEG blobs). Properties (`mirax.*` dump, objective, mpp, background) + macro/label/thumbnail associated images. INI/nonhier-offset/base-dim/position unit-tested; end-to-end `test_mirax.rs` against `Mirax/CMU-1.zip` (6 tests passing; 10 levels, 109240×220696, verified against the thumbnail). **Fluorescence** (multi-channel), **PNG/BMP tiles**, **`StitchingIntensityLayer` (zlib) position fallback**, and **openslide.bounds-\*** deferred; extreme-zoom levels show faint subtile seams from integer (vs cairo subpixel) compositing
- [x] `zeiss` — Zeiss CZI (`vendor/zeiss.rs`) — ZISRAW segmented container (not TIFF): file-header → subblock-directory parse (`DirectoryEntryDV` + `DimensionEntryDV`), pyramid levels grouped by per-subblock downsample `round(Size/StoredSize)`, origin-normalized placed-subtile placement (CZI mosaic coords are not zero-based — a scene sits at negative X) via a new `LevelData.zeiss` / `ZeissLevel` read path + properties (mpp from Scaling metadata, objective power) + JPEG label/macro/thumbnail attachments (attachment-directory parse). **Compression parity with OpenSlide's own `zeiss.c`:** decodes uncompressed (Gray8/16, BGR24/48, BGRA32 → ARGB), Zstd0, and Zstd1 (header parse + HiLo byte unpack) via the pure-Rust `ruzstd`; **JPEG XR / JPEG / LZW are rejected on open with a clear error** (OpenSlide lacks a JXR decoder too, and there is no pure-Rust one). Binary-parse / BGR-conversion / HiLo-unpack unit-tested; end-to-end `test_zeiss.rs` against real fixtures — `Zeiss-5-Uncompressed.czi` (brightfield pyramidal, 5 levels, 50171×11340, two scenes), `Zeiss-5-SlidePreview-Zstd0.czi`, `Zeiss-5-SlidePreview-Zstd1-HiLo.czi`, and `Zeiss-5-JXR.czi` (asserts the rejection) — 8 tests passing. **Multi-channel fluorescence** (only first C/Z/T plane used), **JPEG XR tiles**, **embedded-CZI associated images**, and **`openslide.bounds-*`** deferred
- [x] `dicom` — DICOM WSI (`vendor/dicom.rs`) — a slide is a *directory* of SOP instances sharing a SeriesInstanceUID, so `open` enumerates the containing directory through the slide's `SlideFs` (`io/fs.rs`) — local or blob storage alike — parsing candidate instances concurrently (each is several round trips) and sorting them by `ImageType[2]`: `VOLUME` → pyramid level, `LABEL`/`OVERVIEW`/`THUMBNAIL` → label/macro/thumbnail. Tiles are the instance's *frames*, indexed either row-major (`TILED_FULL`) or by explicit per-frame plane position (`TILED_SPARSE`, holes left transparent), through a new `LevelData.dicom` / `DicomLevel` read path; levels split across a *concatenation* look the tile up in each member (missing/duplicate members error). Format checks match `dicom.c` (PlanarConfiguration 0, 8-bit, 3 samples, single focal plane) + properties (whole dataset dumped as `dicom.*` with C's `Seq[i].Key[j]` naming, mpp from PixelSpacing×1000, objective power, barcode) + ICC from the first optical path. **Baseline JPEG (YBR_FULL_422 and RGB) and uncompressed RGB decode; JPEG 2000 (`…1.2.4.90`/`.91`) is rejected on open with a clear error** since there is no pure-Rust decoder — the same stance `zeiss` takes on JPEG XR. Parser and frame/tile-index unit-tested; end-to-end `test_dicom.rs` against `DICOM/Leica-4.zip` + `DICOM/Hamamatsu-2.zip` (2 tests passing) and both are cross-checked against `slidetool` in `bench_openslide.rs`. **Multi-focal-plane (z-stack) and multi-optical-path (fluorescence) slides**, and **JPEG 2000 instances**, are deferred

---

## Phase 6: Streaming Optimization

- [ ] Lazy tile loading — only fetch tiles when `read_region` is called
- [ ] **Batched range requests** — combine adjacent reads into one request.
      Measured by `bench_remote.rs` against a real S3-compatible bucket
      (~50 ms RTT, ~390 ms observed per request): opening `Aperio/CMU-1.svs`
      (169 MB) fetches only **0.97 MB (0.93%)** — so remote reads are genuinely
      lazy and nothing is downloaded wholesale — but it takes **272 sequential
      requests** and ~107 s, because `read_directory` reads each IFD entry, and
      each out-of-line tag value, as its own few-byte ranged GET. The region read
      itself is already efficient (25 requests, 0.61 MB). **This is the single
      biggest remote-performance item: a read-ahead/coalescing reader for
      `AsyncSlideReader` (serve small reads from ~64 KB–1 MB blocks) should
      collapse those 272 requests into a handful.** Laziness is not the gap;
      granularity is.
- [ ] Adaptive cache sizing for remote vs. local files
- [ ] Connection pooling for HTTP backend — note the client already reuses
      connections; a keep-alive probe against the bench endpoint measured ~583 ms
      per request on an *established* connection vs ~50 ms network RTT, so
      per-request latency there is the endpoint's, not a missing pool
- [ ] Byte-range efficiency tests (mock HTTP server via `wiremock`)

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

### Known bugs surfaced by the bench

- [ ] **DICOM remote open transfers the whole slide** — 84–94% of it, in one
      64 KiB request per frame. `bench_remote` measured `DICOM/Leica-4` opening in
      796 requests / 100.0 MB of a 107 MB slide (332 s), and
      `DICOM/Hamamatsu-2` in **2665 requests / 165.8 MB of 198 MB (1171 s —
      19.5 minutes)**. Every other vendor fetches 0.15–2.1%.

      Cause: the fragment-enumeration loop in `parse_pixel_data`
      (`decode/dicom.rs`) reads an 8-byte item header per fragment via
      `Cursor::fill`, which always pulls `WINDOW` (64 KiB); `skip` then jumps past
      that window, so the next header always misses it. One 64 KiB GET per frame
      — 2665 × 64 KiB = 166 MB, matching the measurement exactly. Invisible
      locally (open is ~120–200 ms off page cache), so only the remote bench
      exposes it.

      The walk itself cannot be avoided: the Basic Offset Table is **empty** in
      both fixtures (verified), and with no offset table the fragment offsets are
      a linked list — each position depends on the previous length. So the fix is
      the read pattern, not the algorithm. Note the two obvious fixes pull in
      opposite directions and must be weighed against the medium:
      * reading only the 8 bytes needed drops transfer to ~21 KB but leaves 2665
        *sequential* round trips (~19 min at the 400 ms/request measured here) —
        worse remotely;
      * a large sequential read-ahead (4–16 MB blocks) still transfers the whole
        pixel region but in ~20 requests — roughly 13 s at the observed
        throughput, a ~90× improvement.
      A remote-aware read-ahead size is therefore the right shape, shared with the
      TIFF coalescing work above.

- [ ] **`open_url` ignores the environment, so it cannot reach a non-AWS
      S3-compatible endpoint.** Its doc comment promises "Cloud credentials and
      region/endpoint configuration are taken from the environment", but it calls
      `object_store::parse_url`, which builds `AmazonS3Builder::new()` (not
      `from_env()`) and passes no options — so no `AWS_*`/endpoint variable is
      read. Against the bench bucket it falls through to the EC2 instance
      metadata service, retries `http://169.254.169.254/latest/api/token` ten
      times, and fails after ~2.2 s. `open_store` with a hand-built store works
      fine (that is what `bench_remote` uses). Fix: pass `std::env::vars()`
      through `parse_url_opts`, and align the doc comment either way.
      Reproduce: `cargo test --release --test bench_remote -- --ignored
      open_url_resolves`

### Known parity gap surfaced by the bench

- [ ] **Leica coordinates are not interchangeable with OpenSlide's.** OpenSlide
      reports `Leica-1.scn` level 0 as the whole collection canvas
      (53130×153470) with the image placed at `openslide.bounds-x=10778`,
      `bounds-y=35096`; slidekit reports 36832×38432 — exactly
      `bounds-width`×`bounds-height` — with the origin at the imaged region, and
      exposes no `openslide.bounds-*`. Reading `(0, 0)` from OpenSlide returns
      transparent canvas while slidekit returns tissue, so the same coordinates
      address different pixels. Fix by expanding level 0 to the collection canvas
      and placing the region at its offset (matching C), or at minimum by
      exposing `openslide.bounds-*` so callers can translate. Tracked with
      multi-region SCN under Phase 5

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
