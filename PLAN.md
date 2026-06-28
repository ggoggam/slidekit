# OpenSlide Rust Migration Plan

## Context

OpenSlide is a C library (~18K LOC) for reading whole slide images (WSI) used in digital pathology. It supports 12 vendor formats (Aperio, Hamamatsu, Mirax, Leica, DICOM, etc.), provides a minimal thread-safe API, and is widely used in medical imaging pipelines. The goal is a ground-up Rust rewrite with:

1. **1:1 API parity** with the C library
2. **Streaming/remote file support** for blob storage (S3, GCS, Azure) without full download
3. **Python bindings** via PyO3
4. **TDD approach** — tests first, then iterate on implementation

The C codebase has 370+ test cases with YAML configs and real slide data from `https://openslide.cs.cmu.edu/download/openslide-testdata/`.

---

## Project Structure

```
slidekit/
  openslide/                    # Vendored C source (reference only)
  Cargo.toml                    # Workspace root
  mise.toml                     # Tooling (rust, prek, uv)
  crates/
    slidekit/                   # Core Rust library
      Cargo.toml
      src/
        lib.rs                  # Public API + re-exports
        error.rs                # Error types (OpenSlideError, Result<T>)
        slide.rs                # OpenSlide struct (main entry point)
        level.rs                # Level info structs
        properties.rs           # Property storage + predefined names
        associated.rs           # Associated images (thumbnail, label, macro)
        cache.rs                # LRU tile cache
        hash.rs                 # Quickhash-1 (SHA-256 based)
        color.rs                # ARGB pixel format, YCbCr tables
        io/
          mod.rs                # ReadSeek trait abstraction
          local.rs              # Local file I/O
          stream.rs             # Streaming/range-request I/O
          s3.rs                 # S3 backend
          gcs.rs                # GCS backend
          azure.rs              # Azure Blob backend
          http.rs               # Generic HTTP range-request backend
        decode/
          mod.rs                # Decoder trait + registry
          tiff.rs               # TIFF parsing (replaces tifflike + libtiff)
          jpeg.rs               # JPEG decoding
          jp2k.rs               # JPEG 2000 decoding
          png.rs                # PNG decoding
          bmp.rs                # BMP decoding
          xml.rs                # XML metadata parsing
          sqlite.rs             # SQLite-based formats
          dicom.rs              # DICOM decoding
        grid/
          mod.rs                # Grid trait
          simple.rs             # Regular rectangular grid
          tilemap.rs            # Arbitrary-positioned tiles
          range.rs              # Range-based (Z-order, quadtree)
        vendor/
          mod.rs                # Format detection pipeline + VendorDriver trait
          aperio.rs             # Aperio SVS
          hamamatsu.rs          # Hamamatsu VMS/VMU/NDPI
          mirax.rs              # 3DHistech MRXS
          leica.rs              # Leica SCN
          ventana.rs            # Roche Ventana BIF
          philips.rs            # Philips TIFF
          trestle.rs            # Trestle
          sakura.rs             # Sakura
          zeiss.rs              # Zeiss CZI
          dicom.rs              # DICOM WSI
          synthetic.rs          # Synthetic test slides
          generic_tiff.rs       # Generic TIFF fallback
    slidekit-sys/               # C-compatible FFI (optional, for drop-in replacement)
      Cargo.toml
      src/lib.rs                # extern "C" functions matching openslide.h
    slidekit-python/            # Python bindings
      Cargo.toml
      pyproject.toml
      src/lib.rs                # PyO3 module
    slidekit-cli/               # CLI tools (slidetool equivalent)
      Cargo.toml
      src/main.rs
  tests/                        # Integration tests
    common/mod.rs               # Test data download, fixture helpers
    test_api.rs                 # Core API tests
    test_properties.rs          # Property tests
    test_associated.rs          # Associated image tests
    test_cache.rs               # Cache tests
    test_streaming.rs           # Remote/streaming I/O tests
    cases/                      # Symlink or copy of openslide/test/cases/
    vendor/
      test_aperio.rs
      test_hamamatsu.rs
      test_mirax.rs
      ... (one per vendor)
```

---

## Phase 0: Project Scaffolding

**Goal:** Set up the Cargo workspace, CI, and test infrastructure.

1. Initialize Cargo workspace with all crates (`slidekit`, `slidekit-python`, `slidekit-cli`, `slidekit-sys`)
2. Configure dependencies in Cargo.toml:
   - `image` — pixel buffer handling
   - `tiff` — TIFF parsing (rust-native, may need custom fork for vendor violations)
   - `jpeg-decoder` — JPEG decoding
   - `openjp2` or `jpeg2k` — JPEG 2000
   - `png` — PNG decoding
   - `sha2` — SHA-256 for quickhash
   - `lru` — LRU cache
   - `xml-rs` or `quick-xml` — XML parsing
   - `rusqlite` — SQLite
   - `object_store` — Unified S3/GCS/Azure/HTTP abstraction (Apache Arrow project)
   - `tokio` — Async runtime (for streaming I/O)
   - `bytes` + `futures` — Streaming primitives
   - `pyo3` + `maturin` — Python bindings
   - `thiserror` — Error types
   - `parking_lot` — Fast mutexes for cache
3. Set up test infrastructure:
   - Port test case YAML configs → Rust test fixtures
   - Build test data downloader (reuse slides.yaml manifest with SHA-256 verification)
   - Create `TestSlide` helper that downloads + caches test data on first run

---

## Phase 1: Test Harness + Core API Skeleton (TDD Foundation)

**Goal:** Write failing tests that define the public API contract, then build minimal stubs.

### 1a. Port Test Cases from C

Translate the 370+ YAML test configs into Rust integration tests. Each test case becomes:

```rust
#[test_case("aperio/CMU-1.svs", vendor = "aperio", success = true)]
fn test_open_and_detect(slide_path: &str, vendor: &str, success: bool) {
    let slide = OpenSlide::open(test_data_path(slide_path));
    if success {
        let osr = slide.unwrap();
        assert_eq!(osr.detect_vendor(), Some(vendor));
    } else {
        assert!(slide.is_err());
    }
}
```

Key test categories to port first:
- **Open/detect** — Can we open each format and detect the vendor?
- **Level info** — Level count, dimensions, downsample factors
- **Properties** — Property names/values match expected
- **Quickhash** — Content hash matches known values
- **Read region** — Pixel data correctness (compare against reference)
- **Associated images** — Thumbnail/label/macro dimensions
- **Error cases** — Truncated files, bad compression, malformed headers
- **Cache** — Eviction behavior, shared cache
- **Parallel** — Thread-safety under concurrent reads

### 1b. Define Public API (Trait + Struct Signatures)

```rust
// Core types
pub struct OpenSlide { ... }
pub struct Cache { ... }
pub struct AssociatedImage { ... }

impl OpenSlide {
    pub fn open(path: impl AsRef<Path>) -> Result<Self>;
    pub fn open_stream(reader: impl ReadSeekStream) -> Result<Self>;
    pub fn detect_vendor(path: impl AsRef<Path>) -> Option<String>;

    pub fn level_count(&self) -> i32;
    pub fn level0_dimensions(&self) -> (i64, i64);
    pub fn level_dimensions(&self, level: i32) -> Option<(i64, i64)>;
    pub fn level_downsample(&self, level: i32) -> Option<f64>;
    pub fn best_level_for_downsample(&self, downsample: f64) -> i32;

    pub fn read_region(&self, x: i64, y: i64, level: i32, w: i64, h: i64) -> Result<Vec<u32>>;
    pub fn read_region_into(&self, dest: &mut [u32], x: i64, y: i64, level: i32, w: i64, h: i64) -> Result<()>;

    pub fn property_names(&self) -> &[String];
    pub fn property_value(&self, name: &str) -> Option<&str>;

    pub fn associated_image_names(&self) -> &[String];
    pub fn associated_image_dimensions(&self, name: &str) -> Option<(i64, i64)>;
    pub fn read_associated_image(&self, name: &str) -> Result<Vec<u32>>;

    pub fn icc_profile(&self) -> Option<Vec<u8>>;
    pub fn associated_image_icc_profile(&self, name: &str) -> Option<Vec<u8>>;

    pub fn set_cache(&mut self, cache: Cache);

    pub fn error(&self) -> Option<&str>;
    pub fn version() -> &'static str;
}
```

### 1c. Streaming I/O Trait

```rust
/// Abstraction over local files and remote blob storage
pub trait SlideReader: Send + Sync {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize>;
    fn size(&self) -> Result<u64>;
}

// Async variant for streaming
#[async_trait]
pub trait AsyncSlideReader: Send + Sync {
    async fn read_at(&self, offset: u64, length: usize) -> Result<Bytes>;
    async fn size(&self) -> Result<u64>;
}
```

This is the key abstraction enabling remote file support. The `object_store` crate already provides this for S3/GCS/Azure/HTTP with range requests.

---

## Phase 2: I/O Layer + Format Detection

**Goal:** Implement the I/O abstraction and vendor detection pipeline.

### 2a. I/O Backends

1. **Local file** — `mmap` or `pread`-based `SlideReader`
2. **HTTP range requests** — Via `object_store::http`
3. **S3** — Via `object_store::aws`
4. **GCS** — Via `object_store::gcp`
5. **Azure** — Via `object_store::azure`

The `object_store` crate (from Apache Arrow) handles all cloud providers with a unified API including range requests, which maps perfectly to our `SlideReader` trait.

### 2b. TIFF-like Parser

The C code has a custom TIFF parser (`openslide-decode-tifflike.c`, 1313 lines) that handles vendor-specific TIFF violations. This must be ported to Rust — the `tiff` crate alone won't handle the quirks (NDPI high-bit offsets, custom tags, etc.).

Key: This parser operates on `SlideReader`, enabling remote TIFF parsing with minimal reads (header → IFD chain → specific tiles on demand).

### 2c. Format Detection Pipeline

Port the detection order from `openslide.c`:
```
synthetic → mirax → zeiss → dicom → hamamatsu → sakura →
trestle → aperio → leica → philips → ventana → generic_tiff
```

Each vendor implements:
```rust
pub trait VendorDriver: Send + Sync {
    fn name(&self) -> &str;
    fn vendor(&self) -> &str;
    fn detect(&self, reader: &dyn SlideReader, tl: Option<&TiffLike>) -> Result<bool>;
    fn open(&self, reader: Box<dyn SlideReader>, tl: Option<TiffLike>) -> Result<SlideData>;
}
```

---

## Phase 3: Decoders

**Goal:** Implement tile decoders needed by vendor drivers.

Order by dependency (most vendors need these):

1. **TIFF decoder** — Wraps tiff-like parser for actual tile data extraction
2. **JPEG decoder** — `jpeg-decoder` crate, handle MCU restart markers (NDPI), YCbCr
3. **JPEG 2000 decoder** — `openjp2` crate (FFI to OpenJPEG) or pure-Rust alternative
4. **PNG decoder** — `png` crate (straightforward)
5. **BMP decoder** — Simple, ~270 lines in C
6. **SQLite decoder** — `rusqlite` for Leica/Sakura
7. **XML parser** — `quick-xml` for metadata
8. **DICOM decoder** — May need FFI to libdicom or port the subset needed

---

## Phase 4: Grid System + Cache

**Goal:** Port the three grid types and LRU cache.

### Grid Types
- **Simple grid** — Regular tile layout (most TIFF-based formats)
- **Tilemap grid** — Float-offset tiles (Mirax)
- **Range grid** — Z-ordered tiles (some vendor formats)

### Cache
- LRU with configurable capacity (default 32MB)
- Key: `(level, x, y)` coordinates
- Shared cache support across multiple slides
- `parking_lot::Mutex` for thread-safe access

---

## Phase 5: Vendor Drivers (Iterative, TDD)

**Goal:** Implement one vendor at a time, guided by failing tests.

### Recommended order (by complexity and test coverage):

1. **Generic TIFF** (313 LOC) — Simplest, exercises TIFF pipeline end-to-end
2. **Synthetic** (989 LOC) — No real files needed, great for unit tests
3. **Aperio** (607 LOC) — Most common format, well-tested, TIFF-based
4. **Trestle** (412 LOC) — Simple TIFF-based
5. **Philips** (710 LOC) — TIFF-based
6. **Ventana** (973 LOC) — TIFF-based
7. **Leica** (836 LOC) — SQLite-based (different path)
8. **Sakura** (927 LOC) — SQLite-based
9. **Hamamatsu** (2297 LOC) — Complex, VMS/VMU + NDPI
10. **Mirax** (1873 LOC) — Complex, custom format
11. **Zeiss** (1441 LOC) — CZI format
12. **DICOM** (1174 LOC) — Medical imaging standard

For each vendor:
1. Enable the vendor's test cases (they should be failing)
2. Implement `detect()` → tests pass for detection
3. Implement `open()` → level/property tests pass
4. Implement `paint_region()` → pixel data tests pass
5. Handle error cases → error test cases pass
6. Move to next vendor

---

## Phase 6: Streaming Optimization

**Goal:** Make remote file access efficient.

Key design decisions:
- **Lazy loading** — Only read headers + IFD chains on open; tiles on demand
- **Prefetch** — When reading a region, batch-request adjacent tiles in one range request
- **Adaptive caching** — Larger cache for remote files (network latency amortization)
- **Connection pooling** — Reuse HTTP connections across tile requests

The TIFF-like parser is the critical path: it must resolve tile offsets from IFD entries, then issue range requests for just those byte ranges. This avoids downloading multi-GB slides.

### Streaming-specific tests:
- Mock HTTP server with range request support
- Verify minimal bytes transferred for common operations
- Test with real S3/GCS in CI (optional, behind feature flag)

---

## Phase 7: Python Bindings

**Goal:** PyO3-based Python module with pip-installable wheels.

```python
import slidekit as openslide

# Local file
slide = openslide.OpenSlide("CMU-1.svs")
print(slide.level_count)
print(slide.dimensions)
region = slide.read_region((0, 0), 0, (256, 256))  # Returns PIL Image

# Remote file (new capability)
slide = openslide.OpenSlide("s3://bucket/CMU-1.svs")
```

### Python API parity:
- Match the existing `openslide-python` package API for drop-in replacement
- Add `open_stream()` for remote files (new)
- Return PIL `Image` objects from `read_region()` and associated image reads
- Properties as dict-like access

### Build:
- `maturin` for wheel building
- `pyproject.toml` with `[tool.maturin]` config
- CI matrix: Linux (manylinux), macOS (arm64 + x86_64), Windows

---

## Phase 8: CLI Tools + FFI

**Goal:** Ship `slidekit-cli` and optional C-compatible `slidekit-sys`.

### CLI (slidetool equivalent):
- `slidekit-cli show-properties <file>`
- `slidekit-cli write-png <file> <x> <y> <level> <w> <h> <output>`
- `slidekit-cli quickhash1sum <file>`

### FFI (`slidekit-sys`):
- `extern "C"` functions matching `openslide.h` signatures exactly
- Enables C/C++ programs to link against the Rust library
- Generates `openslide.h` via `cbindgen`

---

## Implementation Order (TDD Flow)

```
Phase 0  ──→  Phase 1a (tests)  ──→  Phase 1b (API stubs)
                                          │
Phase 2 (I/O + detection)  ←──────────────┘
    │
Phase 3 (decoders)  ──→  Phase 4 (grid + cache)
    │
Phase 5 (vendors, one at a time, each TDD cycle):
    generic_tiff → synthetic → aperio → trestle → philips →
    ventana → leica → sakura → hamamatsu → mirax → zeiss → dicom
    │
Phase 6 (streaming optimization)
    │
Phase 7 (Python bindings)  ──→  Phase 8 (CLI + FFI)
```

---

## Key Rust Crate Dependencies

| Purpose | Crate | Why |
|---------|-------|-----|
| TIFF parsing | custom (port tifflike) | Vendor TIFF violations |
| JPEG | `jpeg-decoder` | Pure Rust, MCU support |
| JPEG 2000 | `openjp2` or `jpeg2k` | FFI to OpenJPEG |
| PNG | `png` | Standard |
| SHA-256 | `sha2` | Quickhash |
| LRU cache | `lru` | Tile caching |
| XML | `quick-xml` | Fast, zero-alloc |
| SQLite | `rusqlite` | Leica/Sakura formats |
| Cloud I/O | `object_store` | S3/GCS/Azure/HTTP ranges |
| Async | `tokio` | Runtime for streaming |
| Python | `pyo3` + `maturin` | Bindings |
| Error | `thiserror` | Ergonomic errors |
| Mutex | `parking_lot` | Fast locks |
| Test | `test-case` + `rstest` | Parameterized tests |
| DICOM | `dicom-rs` or port | DICOM format |

---

## Verification

### Running tests:
```bash
cargo test                          # All unit + integration tests
cargo test --test test_aperio       # Single vendor
cargo test -- --ignored             # Include slow/network tests
```

### Test data:
- Auto-downloaded from `https://openslide.cs.cmu.edu/download/openslide-testdata/`
- Cached in `target/testdata/` with SHA-256 verification
- CI uses pre-cached test data

### Validation against C:
- For each test slide, compare: vendor detection, level count, dimensions, properties, quickhash
- Pixel-level comparison of `read_region()` output against C reference
- Use the existing `frozen.yaml` test results as ground truth

### Streaming tests:
- `wiremock` or `mockito` for mock HTTP server with range requests
- Verify byte-range efficiency (minimal data transfer)
- Optional real cloud tests behind `--features cloud-tests`

### Python tests:
- `pytest` suite that mirrors the Rust integration tests
- Test PIL Image output correctness
- Test remote file opening
