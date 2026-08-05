# Benchmark report

Measured 2026-08-05 against slidekit at `9ae4f04`. Two questions:

1. How does slidekit compare to the reference OpenSlide C library, per vendor?
2. Does the remote path actually stream, or does it quietly download the slide?

Both benches are `#[ignore]`d and live in `crates/slidekit/tests/`. Everything
below is their real output, not a summary of a summary — reproduce with the
commands in [Reproducing](#reproducing).

## Environment

| | |
|---|---|
| Machine | Apple M4 Pro, 12 cores, 26 GB RAM, arm64 |
| OS | macOS 26.5.2 (Darwin 25.5.0) |
| Toolchain | rustc 1.96.1, `--release` |
| Reference | OpenSlide 4.0.1 / `slidetool` 4.0.1, built from source via `mise run bench:setup-slidetool` |
| Slides | One per vendor from the OpenSlide test-data server, SHA-256 verified (`tests/testdata.yaml`) |
| Region | 1024×1024 at level 0, placed over the densest tissue found in the coarsest pyramid level |

Homebrew still ships OpenSlide 4.0.0, so 4.0.1 is built into `.tools/openslide`
to compare against the current release.

Remote storage is an S3-compatible endpoint (not AWS), reached over the public
internet. Its measured characteristics matter more than its identity, because
they dominate every remote number here:

| | |
|---|---|
| Network RTT | ~41–78 ms (ICMP, mean 55 ms) |
| Fresh connection TTFB | ~185–275 ms (TCP 30 ms + TLS ~110 ms) |
| **Per-request latency, keep-alive connection** | **~583 ms** |
| Bulk throughput (observed on upload) | 12–36 MB/s |

That ~583 ms figure was measured with a hand-rolled HTTPS client issuing ten
sequential requests on a single established connection, specifically to rule out
a missing connection pool in slidekit. It is the endpoint's own per-request cost.
**A faster endpoint would shrink every remote latency below roughly
proportionally — the request *counts* are the portable result, not the seconds.**

## Part 1 — slidekit vs OpenSlide, local disk

### Latency

slidekit is timed in-process. `slidetool` can only be timed as a whole process,
so its columns are derived: a process floor (measured at **12.0 ms** from
`slidetool --version`, which does no slide work) is subtracted from a `prop list`
run to get open, and `region read` minus `prop list` gives read.

| vendor | level 0 | sk_open | osl_open | sk_read | osl_read | osl_proc |
|---|---|---|---|---|---|---|
| Aperio | 46000×32914 | 34.6 ms | 9.2 ms | 22.8 ms | 196.3 ms | 217.5 ms |
| Hamamatsu | 51200×38144 | 8.8 ms | 2.0 ms | 34.0 ms | 62.1 ms | 76.1 ms |
| Leica | 36832×38432 | 122.4 ms | 56.6 ms | 15.3 ms | 0.0 ms ¹ | 67.4 ms |
| Philips-TIFF | 91136×68096 | 9.8 ms | 1.6 ms | 15.6 ms | 313.4 ms | 327.0 ms |
| Generic-TIFF | 46000×32914 | 2.2 ms | 2.4 ms | 20.2 ms | 256.8 ms | 271.2 ms |
| Trestle | 40000×27712 | 7.1 ms | 64.8 ms | 13.6 ms | 297.1 ms | 373.9 ms |
| Mirax | 109240×220696 | 43.9 ms | 39.7 ms | 23.0 ms | 316.0 ms | 367.7 ms |
| Zeiss | 50171×11340 | 23.4 ms | 2.5 ms | 7.5 ms | 254.9 ms | 269.4 ms |
| Ventana | 114942×76349 | 34.1 ms | 25.8 ms | 36.9 ms | 467.7 ms | 505.5 ms |
| DICOM/Leica-4 | 23374×22079 | 92.3 ms | 22.9 ms | 14.9 ms | 610.1 ms | 645.0 ms |
| DICOM/Hamamatsu-2 | 30720×39168 | 163.4 ms | 1.3 ms | 12.1 ms | 360.5 ms | 373.9 ms |

¹ Clamped at zero: Leica's whole `region read` process came in marginally under
its `prop list` run. These are single-shot measurements, so differences smaller
than a few tens of milliseconds are noise.

**Read this table with care. `osl_read` includes a PNG encode that slidekit never
performs, so it flatters slidekit and the 3–40× read gap is not a like-for-like
decode comparison.** A fair one needs an in-process comparison through the
planned `slidekit-sys` FFI crate; until that exists, the honest claim is only
that slidekit's reads are in the same order of magnitude and not pathologically
slower. Opens are cleaner, since neither side encodes anything, and there slidekit
trades wins and losses: much faster on Trestle (7.1 vs 64.8 ms), slower on both
DICOM slides (92–163 ms vs 1.3–22.9 ms).

### Pixel agreement

Independent JPEG decoders differ by ±1–2 per channel, so exact equality would
flake. Measured over pixels opaque in *both* readers:

| vendor | mean diff | max diff | opaque overlap |
|---|---|---|---|
| Aperio | 0.01 | 1 | 100% |
| Hamamatsu | 0.00 | 1 | 100% |
| **Leica** | **n/a** | **n/a** | **0%** |
| Philips-TIFF | 0.05 | 3 | 100% |
| Generic-TIFF | 0.07 | 3 | 100% |
| Trestle | 0.07 | 3 | 100% |
| Mirax | 0.17 | 36 | 100% |
| Zeiss | 0.00 | 0 | 100% |
| Ventana | 2.51 | 45 | 100% |
| DICOM/Leica-4 | 0.05 | 3 | 100% |
| DICOM/Hamamatsu-2 | 0.00 | 3 | 100% |

Ten of eleven agree closely. Mirax's max of 36 is consistent with the known
integer-vs-cairo subtile seam noted in `PROGRESS.md`; Ventana's 2.51 mean is the
largest and the one worth a closer look, though still far inside tolerance.

Leica reports `n/a` rather than a passing `0.00` because the compared window had
**no** pixels opaque in both readers — a diff over nothing verifies nothing. That
is finding 3 below, and it is precisely what a bare `0.00` would have hidden.

## Part 2 — remote storage

Same slides, same regions, read from the bucket over byte-range requests. Remote
output is asserted **exactly** equal to local output: both paths run the same
decoder over the same bytes, so any difference means the I/O layer misplaced
data. All ten vendors matched pixel-for-pixel.

### Latency

| vendor | local open | local read | remote open | remote read | warm read | pixels |
|---|---|---|---|---|---|---|
| **Mirax** | 140.1 ms | 20.4 ms | **4.4 s** | 9.7 s | 9.7 s | match |
| Zeiss | 32.8 ms | 9.5 ms | 65.6 s | **1.4 s** | 3.2 s | match |
| Trestle | 1.2 ms | 24.9 ms | 75.2 s | 7.2 s | 6.7 s | match |
| Leica | 74.4 ms | 14.9 ms | 78.9 s | 4.5 s | 4.4 s | match |
| Generic-TIFF | 9.6 ms | 27.2 ms | 82.1 s | 10.4 s | 10.3 s | match |
| Philips-TIFF | 15.8 ms | 16.5 ms | 86.3 s | 4.0 s | 3.8 s | match |
| Ventana | 51.4 ms | 36.2 ms | 102.9 s | 2.6 s | 2.2 s | match |
| Aperio | 30.4 ms | 19.6 ms | 104.9 s | 11.0 s | 11.0 s | match |
| DICOM/Leica-4 | 117.9 ms | 13.4 ms | 331.9 s | 11.2 s | 11.0 s | match |
| DICOM/Hamamatsu-2 | 198.5 ms | 10.7 ms | **1171 s** | 3.0 s | 2.6 s | match |

"Warm read" is a second 1024×1024 region from the already-open slide — the
steady-state number a viewer would see. It tracks the first read closely, so the
per-slide cost is concentrated entirely in `open`.

### Traffic — the actual result

| vendor | open reqs | open MB | read reqs | read MB | slide MB | **fetched %** |
|---|---|---|---|---|---|---|
| **Mirax** | **10** | 1.17 | 24 | 0.33 | 538.9 | 0.279% |
| Zeiss | 164 | 6.49 | 1 | 14.49 | 1004.9 | 2.088% |
| Trestle | 190 | 0.11 | 16 | 0.39 | 158.9 | 0.314% |
| Leica | 201 | 1.37 | 9 | 0.73 | 278.3 | 0.754% |
| Generic-TIFF | 210 | 0.24 | 25 | 0.49 | 194.7 | 0.376% |
| Philips-TIFF | 221 | 0.58 | 9 | 0.32 | 277.5 | 0.324% |
| Ventana | 262 | 2.45 | 4 | 1.55 | 2591.9 | **0.154%** |
| Aperio | 272 | 0.97 | 25 | 0.61 | 169.3 | 0.934% |
| DICOM/Leica-4 | 796 | 100.04 | 25 | 0.79 | 107.5 | **93.786%** |
| DICOM/Hamamatsu-2 | 2665 | 165.80 | 6 | 0.39 | 197.8 | **84.042%** |

**The streaming claim holds for 8 of 10 vendors**: opening a slide fetches
0.15–2.1% of it, including 0.15% of a 2.5 GB Ventana. Nothing is downloaded
wholesale.

**Remote open is round-trip-bound, not bandwidth-bound.** Generic-TIFF is the
clearest case: 0.24 MB transferred, 82 seconds. The bytes are negligible; the 210
sequential requests are not.

**Mirax is the counterexample that shows the fix.** At 10 requests and 4.4 s it is
~20× faster than the TIFF vendors on the same endpoint — because it is not
TIFF-based and bulk-reads `Slidedat.ini` and `Index.dat` as whole-object GETs plus
one `LIST` that carries every data file's size. Zeiss makes the same point from the
read side: one request for 14.49 MB is the fastest read in the set, beating
vendors that fetch 20× less in 25 requests. Fewer, larger reads win decisively at
this latency.

Byte accounting excludes zero-body HEAD probes. This matters: `ObjectStoreExt::head`
is implemented as `get_opts` with `head: true`, and the response still reports
`range` as the whole object, so counting it charges a metadata call the slide's
full size — enough to make a lazy open look like a full download.

### Upload, for reference

Mirroring the bench set into the bucket (`mise run bench:upload-slides`):
24 objects, 4.89 GB, 178 s total, 12–36 MB/s per slide. Bulk throughput is fine;
it is per-request latency that hurts.

## Findings

### 1. `open_url` cannot reach a non-AWS S3 endpoint

`OpenSlide::open_url` documents that "credentials and region/endpoint
configuration are taken from the environment". It calls
`object_store::parse_url`, which builds `AmazonS3Builder::new()` — not
`from_env()` — and passes no options, so **no `AWS_*` or endpoint variable is
read at all**. Against this bucket it falls through to the EC2 instance metadata
service, retries `http://169.254.169.254/latest/api/token` ten times, and fails
after ~2.2 s.

This is the primary documented remote entry point, so anyone following the README
hits it. `open_store` with a hand-built store works — that is what `bench_remote`
uses. Fix: pass `std::env::vars()` through `parse_url_opts`, and align the doc
comment either way.

### 2. DICOM open transfers 84–94% of the slide

The fragment-enumeration loop in `parse_pixel_data` (`decode/dicom.rs`) reads an
8-byte item header per fragment through `Cursor::fill`, which always pulls
`WINDOW` = 64 KiB. `skip` then jumps past that window, so the next header always
misses it: one 64 KiB GET per frame. For DICOM/Hamamatsu-2, 2665 fragments ×
64 KiB = 166 MB, matching the measured 165.80 MB exactly.

The walk cannot simply be skipped — the Basic Offset Table is **empty** in both
fixtures (verified directly), and without it fragment offsets form a linked list
where each position depends on the previous length. So the fix is the read
pattern, and the two obvious options pull in opposite directions:

- read only the 8 bytes needed → transfer drops to ~21 KB, but 2665 *sequential*
  round trips remain (~19 min at this endpoint's latency): **worse remotely**;
- large sequential read-ahead (4–16 MB) → still transfers the pixel region, but in
  ~20 requests: roughly 13 s at observed throughput, a **~90× improvement**.

A remote-aware read-ahead size is the right shape, shared with finding 4. Note
this is invisible locally — open is 118–199 ms off page cache.

### 3. Leica coordinates are not interchangeable with OpenSlide's

OpenSlide reports `Leica-1.scn` level 0 as the whole collection canvas,
53130×153470, with the image placed at `openslide.bounds-x=10778`,
`bounds-y=35096`. slidekit reports 36832×38432 — exactly
`bounds-width`×`bounds-height` — with the origin at the imaged region, and exposes
no `openslide.bounds-*`.

Verified by reading both origins: at `(0, 0)` OpenSlide returns a 389-byte
uniformly transparent PNG, while at `(10778, 35096)` it returns 54 KB of real
image. slidekit returns tissue at `(0, 0)`. **The same coordinates address
different pixels in the two libraries.** Fix by expanding level 0 to the
collection canvas and placing the region at its offset (matching the C driver), or
at minimum by exposing `openslide.bounds-*` so callers can translate.

### 4. TIFF open is round-trip-bound

`read_directory` reads each IFD entry, and each out-of-line tag value, as its own
few-byte ranged GET — 164–272 requests to gather well under 1 MB. Bytes are
already minimal, so this is not a laziness problem; it is a granularity problem.
A read-ahead/coalescing layer over `AsyncSlideReader` (serving small reads from
64 KiB–1 MB blocks) should collapse those into a handful, as the Mirax and Zeiss
numbers above demonstrate.

### 5. Hamamatsu NDPI has no remote number

`NdpiLevel::ensure_start` walks a 198 MB single-strip JPEG hunting restart
markers. Locally that is 34 ms; remotely it had not finished after 4+ minutes, so
it is excluded from the remote run rather than consuming it. Same root cause as
finding 4.

## What these numbers do and don't show

**Supported by the data:**

- slidekit reads all 11 vendors and agrees with OpenSlide 4.0.1 on pixels
  everywhere the comparison is meaningful.
- Remote reads are byte-identical to local reads on all 10 vendors benchmarked —
  the `SlideFs` sibling resolution, range math, and size clamping are correct.
- Remote opens fetch a fraction of a percent to ~2% of a slide, except DICOM.
- Remote open cost is dominated by request count, and one driver in-tree (Mirax)
  already demonstrates the access pattern that fixes it.

**Not supported:**

- That slidekit decodes faster than OpenSlide. `osl_read` carries a PNG encode
  slidekit never does. Unresolved until in-process FFI exists.
- Any conclusion about remote *seconds* on other infrastructure. This endpoint
  costs ~583 ms per request on an established connection; request counts port,
  wall-clock does not.
- Anything about JPEG-2000 or JPEG-XR slides, which slidekit cannot decode, or
  about Sakura, which has no public fixture.
- Single-shot measurements throughout: no repetitions, no variance, no confidence
  intervals. Differences under a few tens of milliseconds locally are noise.

## Reproducing

```sh
mise run bench:setup-slidetool   # build OpenSlide 4.0.1 into .tools (once)
mise run bench:warm-cache        # download + verify fixtures (~5.8 GB, once)
mise run bench:openslide         # Part 1

# Part 2 needs an S3-compatible bucket configured in a git-ignored .env:
#   AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY,
#   BUCKET_NAME, BUCKET_REGION, BUCKET_ENDPOINT_URL
mise run bench:upload-slides     # mirror the slide set (once)
mise run bench:remote            # Part 2
```

Both remote tasks skip with an explanation when no bucket is configured, and the
uploader writes only under `SLIDEKIT_BENCH_PREFIX` (default
`slidekit-bench/testdata`) and never deletes, so a shared bucket is safe.

Useful knobs while investigating:

| variable | effect |
|---|---|
| `SLIDEKIT_BENCH_ONLY=Aperio` | restrict to matching cases (comma-separated) |
| `SLIDEKIT_BENCH_TRACE=512` | log every request ≥ 512 KiB — how finding 2 was located |
| `SLIDEKIT_BENCH_BT=1` | print a backtrace at each traced request |
| `SLIDEKIT_BENCH_BUDGET_SECS=1800` | total wall-clock budget; remaining slides reported as not-run |
| `SLIDEKIT_BENCH_DUMP=/tmp/cmp` | dump both readers' regions as PNGs (`bench:openslide`) |

The DICOM/Hamamatsu-2 remote case alone takes ~20 minutes until finding 2 is
fixed, which is what the budget knob exists for.
