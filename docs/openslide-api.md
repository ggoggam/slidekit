# OpenSlide C API ↔ slidekit mapping

Reference: OpenSlide **4.0.0** (`openslide.h`). This table is the parity
contract: every public C function maps to exactly one idiomatic Rust item on
[`OpenSlide`](../crates/slidekit/src/slide.rs) (or `Cache`), and the future
`slidekit-sys` FFI crate must implement each C symbol on top of that item.

The Rust surface is conceptually 1-1, not byte-for-byte. C uses out-params +
a sticky per-object error state; Rust uses return values + `Result`/`Option`.
The deliberate shape differences are listed under each section.

## Basic usage

| OpenSlide C | slidekit Rust | Notes |
|---|---|---|
| `openslide_detect_vendor(filename)` | `OpenSlide::detect_vendor(path) -> Option<String>` | `None` ≡ NULL (unrecognized). |
| `openslide_open(filename)` | `OpenSlide::open(path) -> Result<OpenSlide>` | Unrecognized → `Err(UnsupportedFormat)`. C returns NULL (unrecognized) vs an object in error state (recognized-but-broken); Rust folds both into `Err`. |
| `openslide_get_level_count(osr)` | `level_count(&self) -> i32` | |
| `openslide_get_level0_dimensions(osr, *w, *h)` | `level0_dimensions(&self) -> (i64, i64)` | Out-params → tuple. |
| `openslide_get_level_dimensions(osr, level, *w, *h)` | `level_dimensions(&self, level) -> Option<(i64, i64)>` | Out-of-range → `None` (C writes -1,-1). |
| `openslide_get_level_downsample(osr, level)` | `level_downsample(&self, level) -> Option<f64>` | Out-of-range → `None` (C returns -1.0). |
| `openslide_get_best_level_for_downsample(osr, ds)` | `best_level_for_downsample(&self, ds) -> i32` | |
| `openslide_read_region(osr, dest, x, y, level, w, h)` | `read_region_into(&self, dest, x, y, level, w, h) -> Result<()>` | **This is the 1-1 form** (same arg order, caller buffer). `read_region(x, y, level, w, h) -> Result<Vec<u32>>` is the allocating convenience variant. |
| `openslide_get_icc_profile_size(osr)` | `icc_profile(&self) -> Option<&[u8]>` | Size + read collapsed into one. Size ≡ `icc_profile().map_or(0, <[u8]>::len)`. |
| `openslide_read_icc_profile(osr, dest)` | `icc_profile(&self) -> Option<&[u8]>` | Same method; copy the slice into `dest` in FFI. |
| `openslide_close(osr)` | `Drop for OpenSlide` | Automatic; no explicit method. |

## Error handling

| OpenSlide C | slidekit Rust | Notes |
|---|---|---|
| `openslide_get_error(osr)` | `Result` / [`OpenSlideError`](../crates/slidekit/src/error.rs) | Rust has no sticky error state; each fallible call returns `Result`. FFI must hold the last error string on the handle to emulate the C contract. |

Behavioral note: C `read_region` with an invalid level or negative `w`/`h`
clears `dest` and sets the error state. slidekit returns `Err(InvalidLevel)`
for a bad level and `Ok(vec![])` for `w <= 0 || h <= 0`. The FFI shim is
responsible for translating these back to the buffer-clear + error-state
behavior.

## Predefined property names

C `OPENSLIDE_PROPERTY_NAME_*` macros ↔ [`properties::names`](../crates/slidekit/src/properties.rs) constants. All present and value-identical:

`COMMENT`, `VENDOR`, `QUICKHASH1`, `BACKGROUND_COLOR`, `OBJECTIVE_POWER`,
`MPP_X`, `MPP_Y`, `BOUNDS_X`, `BOUNDS_Y`, `BOUNDS_WIDTH`, `BOUNDS_HEIGHT`,
`ICC_SIZE`.

## Properties

| OpenSlide C | slidekit Rust |
|---|---|
| `openslide_get_property_names(osr)` | `property_names(&self) -> Vec<String>` |
| `openslide_get_property_value(osr, name)` | `property_value(&self, name) -> Option<&str>` |

## Associated images

| OpenSlide C | slidekit Rust | Notes |
|---|---|---|
| `openslide_get_associated_image_names(osr)` | `associated_image_names(&self) -> Vec<String>` | |
| `openslide_get_associated_image_dimensions(osr, name, *w, *h)` | `associated_image_dimensions(&self, name) -> Option<(i64, i64)>` | |
| `openslide_read_associated_image(osr, name, dest)` | `read_associated_image(&self, name) -> Result<Vec<u32>>` | |
| `openslide_get_associated_image_icc_profile_size(osr, name)` | `associated_image_icc_profile(&self, name) -> Option<&[u8]>` | Size ≡ slice length, as with the main ICC profile. |
| `openslide_read_associated_image_icc_profile(osr, name, dest)` | `associated_image_icc_profile(&self, name) -> Option<&[u8]>` | Same method. |

## Caching

| OpenSlide C | slidekit Rust | Notes |
|---|---|---|
| `openslide_cache_create(capacity)` | `Cache::new(capacity_bytes) -> Cache` | |
| `openslide_set_cache(osr, cache)` | `OpenSlide::set_cache(&mut self, cache)` | |
| `openslide_cache_release(cache)` | `Drop for Cache` | `Cache` is `Arc`-backed; freed when the last holder drops. |

## Miscellaneous

| OpenSlide C | slidekit Rust |
|---|---|
| `openslide_get_version()` | `OpenSlide::version() -> &'static str` |

## slidekit-only extensions (no OpenSlide equivalent)

Remote/streaming support, gated behind the `object-store` feature; not part of
the parity contract:

- `OpenSlide::open_url` / `open_store` and their `_async` variants
- `OpenSlide::read_region_async`
