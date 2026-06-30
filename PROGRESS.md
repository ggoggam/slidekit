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
- [ ] Full detection order matching C (synthetic → mirax → zeiss → dicom → hamamatsu → sakura → trestle → aperio → leica → philips → ventana → generic_tiff)

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
- [x] XML metadata parser (`decode/xml.rs`) — minimal element tree over `quick-xml` (`Node` + `parse_tree` + `children_named`/`find_child`/`descend`), shared by Philips and Ventana
- [ ] SQLite reader (`decode/sqlite.rs`) — `rusqlite`, for Leica/Sakura
- [ ] DICOM decoder (`decode/dicom.rs`) — `dicom-rs` or partial port

---

## Phase 4: Grid System + Cache (`grid/`)

- [x] `TileGrid` trait — `tiles_for_region(x, y, w, h)`
- [x] `SimpleGrid` — regular rectangular tile layout (`grid/simple.rs`)
- [x] `TileMap` — arbitrary-offset placed (sub)tiles (`grid/tilemap.rs`); built by Ventana from AOI stitch geometry, consumed by `read_region` via an additive `LevelData.tilemap` path (also serves Mirax)
- [ ] `RangeGrid` — Z-order / quadtree tiles
- [x] `Cache` — LRU by capacity, `parking_lot::Mutex` (`cache.rs`)
- [ ] Shared cache across multiple `OpenSlide` instances

---

## Phase 5: Vendor Drivers (`vendor/`)

- [x] `generic_tiff` — generic TIFF fallback (`vendor/generic_tiff.rs`)
- [ ] `synthetic` — synthetic test slides (no real file needed)
- [x] `aperio` — Aperio SVS (`vendor/aperio.rs`) — detect + open + properties + associated images + ICC + tile decode
- [x] `trestle` — Trestle TIFF (`vendor/trestle.rs`) — detect + open + properties + tile decode
- [x] `philips` — Philips TIFF (`vendor/philips.rs`) — detect + open + XML-driven level fixup + properties (mpp, objective) + associated images + tile decode
- [x] `ventana` — Roche Ventana BIF (`vendor/ventana.rs`) — detect (XMLPacket + iScan) + open + initial-XML properties (objective power, mpp) + level enumeration (level=/mag= validation) + AOI stitch geometry → `TileMap` + associated images (macro/thumbnail) + ICC. Geometry/parse unit-tested; end-to-end `test_ventana.rs` gated on a Ventana fixture not yet in `testdata.yaml` (multi-GB; needs a real sha256/size)
- [x] `leica` — Leica SCN (`vendor/leica.rs`) — TIFF+XML (not SQLite); detect (`scn` namespace in dir-0 ImageDescription) + open + XML-driven level selection from the main image's `<dimension ifd=…>` children + properties (objective power, mpp, `leica.*` metadata, barcode) + macro associated image (via the new tiled-image decode path) + ICC. Single brightfield main image fully supported; multi-region SCN (multiple main images composited at collection offsets) deferred — the read path keys one IFD per level, so `open` errors clearly. XML parse/classification unit-tested; end-to-end `test_leica.rs` against `Leica/Leica-1.scn` (6 tests passing)
- [ ] `sakura` — Sakura (SQLite-based)
- [ ] `hamamatsu` — Hamamatsu VMS/VMU/NDPI (~2300 LOC in C)
- [ ] `mirax` — 3DHistech MRXS (~1900 LOC in C)
- [ ] `zeiss` — Zeiss CZI (~1400 LOC in C)
- [ ] `dicom` — DICOM WSI (~1200 LOC in C)

---

## Phase 6: Streaming Optimization

- [ ] Lazy tile loading — only fetch tiles when `read_region` is called
- [ ] Batched range requests — combine adjacent tile fetches into one request
- [ ] Adaptive cache sizing for remote vs. local files
- [ ] Connection pooling for HTTP backend
- [ ] Byte-range efficiency tests (mock HTTP server via `wiremock`)

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
