# Benchmark report

Measured 2026-08-05 on the `slidekit/remote-bench` working tree. Two questions:

1. How does slidekit compare to the reference OpenSlide C library, per vendor?
2. Does the remote path actually stream, or does it quietly download the slide?

Both benches are `#[ignore]`d and live in `crates/slidekit/tests/`. Everything
below is their real output, not a summary of a summary — reproduce with the
commands in [Reproducing](#reproducing).

This run supersedes the first pass, taken at
[`9ae4f04`](../PROGRESS.md), which found five defects. Four are fixed here and
the numbers below are the post-fix measurements; the "before" columns are that
earlier run, on the same machine and the same endpoint, so the comparison is
like-for-like. What changed in the code is summarised in
[What was fixed](#what-was-fixed); what is still wrong is in
[Remaining findings](#remaining-findings).

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
to compare against the current release. **Run the bench through `mise`** (or with
`.tools/openslide/bin` first on `PATH`): a bare `cargo test` picks up Homebrew's
4.0.0 instead, which has no Zeiss driver and silently reports Zeiss as
incomparable.

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
so its columns are derived: a process floor (measured at **10.4 ms** from
`slidetool --version`, which does no slide work) is subtracted from a `prop list`
run to get open, and `region read` minus `prop list` gives read.

| vendor | level 0 | sk_open | osl_open | sk_read | osl_read | osl_proc |
|---|---|---|---|---|---|---|
| Aperio | 46000×32914 | 20.8 ms | 6.4 ms | 18.9 ms | 180.5 ms | 197.3 ms |
| Hamamatsu | 51200×38144 | 6.0 ms | 1.3 ms | 25.4 ms | 66.1 ms | 77.8 ms |
| Leica | 53130×153470 | 45.0 ms | 4.0 ms | 14.9 ms | 198.8 ms | 213.2 ms |
| Philips-TIFF | 91136×68096 | 7.8 ms | 2.8 ms | 12.0 ms | 288.9 ms | 302.0 ms |
| Generic-TIFF | 46000×32914 | 7.1 ms | 2.4 ms | 16.7 ms | 241.1 ms | 253.9 ms |
| Trestle | 40000×27712 | 2.1 ms | 1.8 ms | 13.6 ms | 320.1 ms | 332.2 ms |
| Mirax | 109240×220696 | 34.6 ms | 13.4 ms | 14.6 ms | 317.6 ms | 341.5 ms |
| Zeiss | 50171×11340 | 20.0 ms | 4.5 ms | 7.4 ms | 239.7 ms | 254.6 ms |
| Ventana | 114942×76349 | 32.5 ms | 24.0 ms | 34.3 ms | 421.9 ms | 456.3 ms |
| DICOM/Leica-4 | 23374×22079 | 27.5 ms | 41.2 ms | 16.6 ms | 549.5 ms | 601.1 ms |
| DICOM/Hamamatsu-2 | 30720×39168 | 17.9 ms | 2.0 ms | 14.5 ms | 327.3 ms | 339.7 ms |

**Read this table with care. `osl_read` includes a PNG encode that slidekit never
performs, so it flatters slidekit and the 3–20× read gap is not a like-for-like
decode comparison.** A fair one needs an in-process comparison through the
planned `slidekit-sys` FFI crate; until that exists, the honest claim is only
that slidekit's reads are in the same order of magnitude and not pathologically
slower. Opens are cleaner, since neither side encodes anything, and there
slidekit trades wins and losses: level with Trestle, slower on Leica (45.0 vs
4.0 ms, which is macro-image JPEG decode at open), faster than OpenSlide on
DICOM/Leica-4 (27.5 vs 41.2 ms). These are single-shot timings and differences
under a few tens of milliseconds are noise.

DICOM opens improved sharply as a side effect of the remote work: 92.3 → 27.5 ms
(Leica-4) and 163.4 → 17.9 ms (Hamamatsu-2), because neither now walks the pixel
data or touches a 13 MB ICC profile just to open.

### Pixel agreement

Independent JPEG decoders differ by ±1–2 per channel, so exact equality would
flake. Measured over pixels opaque in *both* readers:

| vendor | mean diff | max diff | opaque overlap |
|---|---|---|---|
| Aperio | 0.01 | 1 | 100% |
| Hamamatsu | 0.00 | 1 | 100% |
| **Leica** | **0.08** | **3** | **100%** |
| Philips-TIFF | 0.05 | 3 | 100% |
| Generic-TIFF | 0.07 | 3 | 100% |
| Trestle | 0.07 | 3 | 100% |
| Mirax | 0.17 | 36 | 100% |
| Zeiss | 0.00 | 0 | 100% |
| Ventana | 2.51 | 45 | 100% |
| DICOM/Leica-4 | 0.05 | 3 | 100% |
| DICOM/Hamamatsu-2 | 0.00 | 3 | 100% |

All eleven now agree, and all eleven have a meaningful window to agree over.
Leica was **0% overlap** before — the two libraries put level 0 in different
coordinate spaces, so the compared window held no pixel that was image in both.
It is now 100% at 0.08 mean difference (finding 3, fixed). Mirax's max of 36 is
the known integer-vs-cairo subtile seam noted in `PROGRESS.md`; Ventana's 2.51
mean is the largest and the one still worth a closer look, though inside
tolerance.

### Addendum, 2026-08-06 — the JPEG 2000 Aperio slides

The two JP2K Aperio variants were outside the bench set above, because slidekit
had no JPEG 2000 decoder when it was taken. They are in it now. Same machine,
same region rule, `slidetool` 4.0.1, measured in one run together with the
eleven slides above (whose figures moved only by run-to-run noise):

| vendor | level 0 | sk_open | osl_open | sk_read | osl_read | osl_proc |
|---|---|---|---|---|---|---|
| Aperio/JP2K-33003-1 | 15374×17497 | 19.0 ms | 2.5 ms | 69.4 ms | 239.0 ms | 253.4 ms |
| Aperio/CMU-1-JP2K-33005 | 46000×32893 | 20.1 ms | 3.4 ms | 28.9 ms | 281.4 ms | 296.8 ms |

| vendor | mean diff | max diff | opaque overlap |
|---|---|---|---|
| Aperio/JP2K-33003-1 | 4.46 | 38 | 100% |
| Aperio/CMU-1-JP2K-33005 | 3.41 | 33 | 100% |

These are the largest pixel differences in the suite, and they are expected
rather than a defect. Both slides are lossy 9/7 irreversible codestreams, and
`hayro-jpeg2000` reconstructs them independently of OpenJPEG; two conforming
decoders are entitled to disagree by a few counts. What rules out a colour bug
is the *signed* difference, which is ~0 on every channel (R +0.16, G +0.02,
B −0.15 on 33003) while the absolute mean is 3–5 — unbiased scatter, not a cast.

33003 is the interesting one. Its chroma is subsampled 2:1 horizontally and its
codestream flags no multi-component transform, so nothing inside the file says
the components are YCbCr; only the private TIFF compression tag does. Decoded as
if they were RGB, the region's mean chroma reads about 55 against `slidetool`'s
33.2 — the failure looks like a plausible image, which is why
`test_aperio.rs` asserts on that statistic and the bench compares pixels.

Reads are the slowest in the table (69.4 ms and 28.9 ms against Aperio/CMU-1's
18.8 ms): JPEG 2000 costs more than baseline JPEG, 33003 additionally pays a
per-pixel YCbCr conversion, and `hayro-jpeg2000` is younger than
`jpeg-decoder`. Both still finish inside OpenSlide's whole-process time.

## Part 2 — remote storage

Same slides, same regions, read from the bucket over byte-range requests. Remote
output is asserted **exactly** equal to local output: both paths run the same
decoder over the same bytes, so any difference means the I/O layer misplaced
data. All eleven vendors matched pixel-for-pixel, as did the two JPEG 2000
slides added later (see the addendum below).

### Latency

| vendor | local open | local read | remote open | remote read | warm read | pixels |
|---|---|---|---|---|---|---|
| **DICOM/Hamamatsu-2** | 9.7 ms | 11.3 ms | **2.8 s** | 0.5 s | 0.4 s | match |
| Trestle | 2.8 ms | 18.4 ms | 4.1 s | 0.6 s | 0.5 s | match |
| Mirax | 32.0 ms | 17.4 ms | 4.3 s | 2.4 s | 1.7 s | match |
| Generic-TIFF | 3.0 ms | 19.9 ms | 5.6 s | 0.8 s | 0.8 s | match |
| Leica | 42.8 ms | 13.7 ms | 6.6 s | 0.9 s | 0.6 s | match |
| Hamamatsu | 7.4 ms | 29.3 ms | 7.0 s | 2.8 s | 0.02 s | match |
| DICOM/Leica-4 | 29.8 ms | 57.5 ms | 7.9 s | 7.5 s | 0.02 s | match |
| Ventana | 45.2 ms | 41.9 ms | 8.1 s | 2.3 s | 1.4 s | match |
| Aperio | 27.0 ms | 15.9 ms | 8.6 s | 0.8 s | 0.5 s | match |
| Philips-TIFF | 12.2 ms | 15.1 ms | 8.8 s | 0.6 s | 0.6 s | match |
| Zeiss | 24.9 ms | 10.7 ms | **11.9 s** | 1.5 s | 6.3 s | match |

Hamamatsu NDPI appears for the first time: the previous run had to exclude it
because `NdpiLevel::ensure_start` walks a 198 MB single-strip JPEG hunting
restart markers and had not finished after four minutes. It now opens in 7.0 s.

The two JP2K Aperio slides were added to the bucket and the bench on
2026-08-06, and behave like the rest — the codec is irrelevant to the I/O layer,
which is the point:

| vendor | local open | local read | remote open | remote read | warm read | pixels |
|---|---|---|---|---|---|---|
| Aperio/JP2K-33003-1 | 34.6 ms | 68.8 ms | 6.1 s | 1.0 s | 0.8 s | match |
| Aperio/CMU-1-JP2K-33005 | 24.9 ms | 28.9 ms | 7.0 s | 0.8 s | 0.7 s | match |

| vendor | open reqs | open MB | read reqs | read MB | slide MB | fetched % |
|---|---|---|---|---|---|---|
| Aperio/JP2K-33003-1 | 14 | 1.05 | 1 | 3.99 | 60.9 | 8.268% |
| Aperio/CMU-1-JP2K-33005 | 16 | 1.19 | 1 | 2.87 | 126.4 | 3.211% |

Remote pixels are byte-identical to local pixels on both, so the JPEG 2000
decoder sees exactly the same bytes whether they came off disk or off S3.
JP2K-33003-1's 8.3% is the highest non-DICOM share in the suite only because
the slide is small (60.9 MB): the *bytes* fetched, 5 MB, are ordinary.

### Traffic — the actual result

| vendor | open reqs | open MB | read reqs | read MB | slide MB | **fetched %** |
|---|---|---|---|---|---|---|
| Trestle | **10** | 0.62 | 1 | 0.49 | 158.9 | 0.703% |
| Mirax | **10** | 1.24 | 5 | 1.00 | 538.9 | 0.415% |
| Generic-TIFF | 12 | 0.70 | 1 | 1.79 | 194.7 | 1.280% |
| Hamamatsu | 14 | 1.49 | 5 | 1.94 | 188.9 | 1.815% |
| Leica | 14 | 2.33 | 3 | 0.73 | 278.3 | 1.101% |
| Aperio | 18 | 1.33 | 5 | 0.61 | 169.3 | 1.147% |
| Philips-TIFF | 18 | 1.24 | 3 | 0.81 | 277.5 | 0.739% |
| Ventana | 18 | 2.99 | 4 | 1.55 | 2591.9 | **0.175%** |
| DICOM/Hamamatsu-2 | 19 | 1.32 | 3 | 0.39 | 197.8 | 0.866% |
| DICOM/Leica-4 | 27 | 6.25 | 6 | 24.00 | 107.5 | **28.139%** |
| Zeiss | **151** | 9.59 | 1 | 14.49 | 1004.9 | 2.397% |

### Before and after

The ten vendors the earlier run completed, open phase only:

| vendor | reqs before → after | MB before → after | seconds before → after |
|---|---|---|---|
| Mirax | 10 → **10** | 1.17 → 1.24 | 4.4 → **4.3** |
| Zeiss | 164 → **151** | 6.49 → 9.59 | 65.6 → **11.9** |
| Trestle | 190 → **10** | 0.11 → 0.62 | 75.2 → **4.1** |
| Leica | 201 → **14** | 1.37 → 2.33 | 78.9 → **6.6** |
| Generic-TIFF | 210 → **12** | 0.24 → 0.70 | 82.1 → **5.6** |
| Philips-TIFF | 221 → **18** | 0.58 → 1.24 | 86.3 → **8.8** |
| Ventana | 262 → **18** | 2.45 → 2.99 | 102.9 → **8.1** |
| Aperio | 272 → **18** | 0.97 → 1.33 | 104.9 → **8.6** |
| DICOM/Leica-4 | 796 → **27** | 100.04 → 6.25 | 331.9 → **7.9** |
| DICOM/Hamamatsu-2 | 2665 → **19** | 165.80 → 1.32 | 1171 → **2.8** |
| **total** | **4991 → 297** | **279.2 → 27.6** | **2103 → 68** |

Region reads improved alongside: **144 → 32 requests** across the same ten
slides. The whole remote bench now finishes in 110 s; before, an 1800 s budget
expired with slides left unrun.

**There is a real trade here, and it is visible in the byte column.** Seven of
the eight TIFF-family vendors fetch *more* bytes than before — Trestle 0.11 →
0.62 MB, Generic-TIFF 0.24 → 0.70 MB — because a 64 KiB read-ahead block is
fetched where a few bytes were read. That is the intended bargain at ~583 ms per
request: Trestle pays 0.5 MB extra (~30 ms of bandwidth) to drop 180 requests
(~105 s). It is only a good trade because bytes were never the constraint. On an
endpoint where they are, the block size is the knob.

**The streaming claim now holds for ten of eleven vendors**: opening a slide and
reading a 1024×1024 region fetches 0.18–2.4% of it, including 0.18% of a 2.5 GB
Ventana. DICOM/Leica-4 is the exception at 28%, and all of that is now in its
*read* — its open fetches 5.8% — for a reason that is inherent to the file rather
than to the reader; see [finding A](#a-dicomleica-4-pays-24-mb-for-a-sparse-level-with-no-offset-table).

**Remote open is still round-trip-bound, and Zeiss is what that looks like now.**
At 151 requests it costs more than the other ten vendors combined. Its subblock
headers are megabytes apart in a 1 GB file, so they cannot be coalesced into
neighbouring reads; they are now at least issued as one concurrent batch, which
is where 65.6 → 11.9 s came from.

## What was fixed

Five defects came out of the first run. Four are fixed; the fifth (Hamamatsu NDPI
having no remote number) was a symptom of the third and is resolved with it. Two
more (5 and 6) were found while re-measuring the fixes.

### 1. `open_url` could not reach a non-AWS S3 endpoint — fixed

`OpenSlide::open_url` documented that credentials and region/endpoint
configuration come from the environment. It called `object_store::parse_url`,
which builds `AmazonS3Builder::new()` — not `from_env()` — and passes no options,
so **no `AWS_*` variable was read at all**. Against this bucket it fell through to
the EC2 instance metadata service, retried
`http://169.254.169.254/latest/api/token` ten times, and failed after ~2.2 s.

It now goes through `parse_url_opts` with the environment filtered to the
prefixes each builder's own `from_env()` consults — `AWS_`, `AZURE_`, `GOOGLE_`,
plus `SERVICE_ACCOUNT` and `MSI_ENDPOINT`. The filter is not cosmetic:
`parse_url_opts` also accepts bare keys like `region`, `bucket`, `endpoint`, and
`token`, so forwarding the whole environment would let an unrelated `TOKEN` or
`ENDPOINT` variable redirect a slide read.

`bench_remote::open_url_resolves_config_from_environment` was a diagnostic that
printed what happened; it is now an assertion, and it passes — a HEAD through
`open_url` against this endpoint returns in 0.8 s. It needs the `AWS_*` spellings
in `.env`, and skips with an explanation if only the `BUCKET_*` aliases are set.

### 2. DICOM open transferred 84–94% of the slide — fixed

The fragment-enumeration loop in `parse_pixel_data` read an 8-byte item header per
fragment through a 64 KiB window that `skip` then jumped past, so every header
missed: one 64 KiB GET per frame. For DICOM/Hamamatsu-2, 2665 fragments ×
64 KiB = 166 MB, matching the measured 165.80 MB exactly.

Three things were wrong, and all three are fixed:

- **The walk happened at open, for every instance.** Parsing now stops at an
  encapsulated pixel-data element — nothing after it in tag order is used — and
  `FrameIndex` resolves frames on demand, walking only as far as the frame
  actually asked for and remembering how far it got. Opening a slide costs
  nothing; reading a coarse level costs its own few frames.
- **The walk happened even when it was unnecessary.** Hamamatsu-2 carries an
  Extended Offset Table for every instance (36960 bytes = 4620 frames × 8), which
  the old code parsed *after* walking all 2665 fragments to reach it. A non-empty
  Basic Offset Table is now honoured too, with all but the last frame's length
  derived from the gaps between entries. This is why Hamamatsu-2 dropped to 19
  requests and 1.32 MB: it never walks at all.
- **A 13.1 MB ICC profile was loaded once per instance.** Leica's DICOM
  instances each embed the same 13.1 MB profile inside `OpticalPathSequence`, and
  the parser materialised it for all of them — 49 MB of duplicate colour data per
  open. It is now recorded by location (`Value::Bulk`) rather than parsed, and not
  read at open at all (see [fix 6](#6-icc-profiles-are-read-on-demand)).

Where the walk is genuinely unavoidable — no offset table, as on Leica-4 — it is
now a sequential scan in growing blocks: 1, 2, 4, 4, 4, 4, 4 MB rather than 2665
separate 64 KiB reads. That is the 23.94 MB / 11 requests in Leica-4's read
column, and it is memoized, which is why its warm read is 19 ms.

### 3. Leica coordinates were not interchangeable with OpenSlide's — fixed

OpenSlide reports `Leica-1.scn` level 0 as the whole collection canvas,
53130×153470, with the image placed at `openslide.bounds-x=10778`,
`bounds-y=35096`. slidekit reported 36832×38432 — exactly
`bounds-width`×`bounds-height` — with the origin at the imaged region, and exposed
no `openslide.bounds-*`. Verified by reading both origins: at `(0, 0)` OpenSlide
returned a uniformly transparent tile while slidekit returned tissue. **The same
coordinates addressed different pixels in the two libraries.**

A level is now the collection, with the region placed on it. The geometry comes
from the SCN XML, all of it in nanometres: a level's scale is
`view.sizeX / dimension.sizeX` nm per pixel, the level is
`ceil(collection.size / that)`, and the region sits at `floor(view.offset / that)`.
The x scale drives both axes, which is what the C driver does and the only way its
sizes reproduce — `Leica-1.scn`'s y extent implies a different scale and would put
level 3 at 2397 pixels tall where OpenSlide reports 2401. This reproduces all five
level sizes, both bounds values, and OpenSlide's averaged downsamples exactly;
`test_leica` now asserts them against `slidetool`'s output, along with the pair
that actually matters — transparent at `(0, 0)`, tissue at the bounds origin.
`openslide.bounds-*` and `openslide.region[0].*` are set.

Fixing this exposed a second, older bug. A tile at the right or bottom edge of an
image runs past it — a 36832-pixel-wide image in 512-pixel tiles ends 32 pixels
into its last column — and slidekit painted that encoder padding. Invisible while
a level *was* the image, since the padding fell outside every valid coordinate;
plainly visible as a 32-pixel black band once the level got larger than the image.
Tiles are now clipped to the image extent, which is what took Leica's opaque
overlap from 9% to 100%.

### 4. TIFF open was round-trip-bound — fixed

`read_directory` read each IFD entry, and each out-of-line tag value, as its own
few-byte ranged GET — 164–272 requests to gather well under 1 MB. Bytes were
already minimal; this was a granularity problem.

`io/readahead.rs` serves those reads from cached blocks: one fetch covers a whole
IFD and its out-of-line values, so a burst of small reads over one region costs
one request instead of hundreds. Two properties make it work on real layouts —
the block **doubles on sequential access** up to 4 MiB (which is what makes a
DICOM fragment walk or an NDPI restart-marker scan finish at all) and **resets to
64 KiB on anything else** (so a random tile read never pulls megabytes). A read
that lands inside a cached block returns a short read rather than refetching from
the block's start. Memory is bounded per reader at 16 MiB; the two readers an
open builds over one object share a cache.

It is applied only to `object_store` readers. Local files keep the kernel's page
cache, which already does this — interposing a second one would only add copies.

### 5. Reads went out one tile at a time — fixed

Not from the original list; found while re-measuring. `read_region_async` had
always gathered its tile ranges into one `object_store::get_ranges` call, which
coalesces neighbours and parallelizes the rest, but the synchronous `read_region`
read tiles one at a time — 10–25 sequential round trips for a region whose tiles
are adjacent in the file.

`SlideReader` gained a `read_ranges` batch method (default: a sequential loop, which
is what a local file wants). `BlockingReader` hands the batch to the async
backend; `ReadAhead` serves what its cache covers and passes the misses down as
one call. `read_region` now resolves every tile location first and fetches them
together, as do the DICOM frame path and the Zeiss subblock-header pass. Region
reads dropped from 144 to 32 requests, Zeiss's open from 79.7 s to 11.9 s.

One trap worth recording: the `Located` and `Shared` reader wrappers inherit trait
defaults, so until they forwarded `read_ranges` explicitly the batch was silently
unrolled back into one read per range — the Zeiss numbers did not budge, and the
gather looked like it had no effect.

### 6. ICC profiles are read on demand

Not from the original list; found by tracing what was left of DICOM/Leica-4's
open. Even after the parser stopped loading every instance's copy, the slide still
fetched the 13.1 MB profile **twice** — once for level 0's instance, once for the
thumbnail's `associated_image_icc_profile` — because both accessors returned
borrowed slices captured at open. That was 24.6 MB of a 30.90 MB open, for colour
data most callers never ask for.

A profile is now an [`IccProfile`](../crates/slidekit/src/icc.rs): either bytes
already in hand or a byte range read on first request and cached. The formats
whose profile comes out of a TIFF tag the driver has already parsed keep loading
it at open — those are kilobytes and the bytes were on the wire anyway — so the
deferral exists exactly where it pays.

The size is available without reading, which is the point: a caller can tell a
profile is 13 MB before deciding it wants it. That is the same split the C API
makes between `openslide_get_icc_profile_size` and `openslide_read_icc_profile`,
and it is why the accessors changed shape:

| before | after |
|---|---|
| `icc_profile() -> Option<&[u8]>` | `icc_profile() -> Result<Option<&[u8]>>` |
| — | `icc_profile_size() -> Option<usize>` |
| `associated_image_icc_profile(name) -> Option<&[u8]>` | `associated_image_icc_profile(name) -> Result<Option<&[u8]>>` |
| — | `associated_image_icc_profile_size(name) -> Option<usize>` |

`Result` is not ceremony: the read can fail, and returning `None` for both "no
profile" and "the fetch failed" would hide an I/O error behind a value that looks
like metadata. A failed read is not cached, so a transient failure can be retried.

DICOM/Leica-4's open went **30.90 → 6.25 MB** and 13.5 → 7.9 s;
`test_dicom::open_does_not_read_the_icc_profile` counts bytes through a wrapping
`SlideFs` and asserts an open reads fewer of them than the profile alone contains.

One related tweak in the read-ahead layer: a fetch larger than half the cache
budget is no longer cached. A 13 MB profile is not a block — caching it would
evict the entire working set to hold something no later read lands inside, and the
caller keeps its own copy. This is also why Leica-4's region read dropped from 11
requests to 6: the fragment walk now keeps its window.

## Remaining findings

### A. DICOM/Leica-4 pays 24 MB for a sparse level with no offset table

28% of the slide, down from 93.8%. Its **open** is now 6.25 MB — 5.8% — and all
28% is in the region read, for a reason that belongs to the file rather than to
the reader.

Leica-4 carries no offset table of either kind, so fragment offsets form a linked
list: reaching a frame in the middle of an 8004-frame level means reading
everything before it. That is 24 MB for this region. It now costs 6 large
sequential requests instead of thousands of small ones, and it is memoized per
instance — the warm read is 19 ms — but the bytes are unavoidable without an
offset table the file does not have. An encoder that writes a Basic or Extended
Offset Table (as Hamamatsu-2 does) skips the walk entirely and lands at 0.87%.

The one thing that would help: persisting a derived offset table alongside the
slide, so the walk is paid once per slide rather than once per process. That is a
caching layer above this library, not a fix inside it.

### B. Zeiss open costs 151 requests

More than the other ten vendors put together. The CZI directory says where each
subblock is but not where the pixels inside it start, so it takes one 16-byte
header read per subblock, and they sit megabytes apart in a 1 GB file — nothing to
coalesce. They are batched and concurrent now (65.6 → 11.9 s), which is most of
the available win; going further means either caching the resolved offsets or
finding the metadata that makes the second pass unnecessary.

### C. `bench_openslide` needs `mise` to be correct

Run bare, `cargo test --test bench_openslide` finds Homebrew's `slidetool` 4.0.0
on `PATH` instead of the 4.0.1 in `.tools`, which has no Zeiss driver and reports
Zeiss as `n/a` with no indication that the wrong binary was used. The harness
should assert the version it found rather than trusting `PATH`.

## What these numbers do and don't show

**Supported by the data:**

- slidekit reads all eleven vendors and agrees with OpenSlide 4.0.1 on pixels,
  everywhere, over a meaningful window — including Leica, which previously had no
  comparable pixels at all.
- Remote reads are byte-identical to local reads on all eleven vendors — the
  `SlideFs` sibling resolution, range math, and size clamping are correct.
- Ten of eleven vendors fetch 0.18–2.4% of a slide to open it and read a
  1024×1024 region; the eleventh fetches 5.8% to open and pays the rest in a
  read whose cost the file format dictates.
- Remote open cost is dominated by request count, and reducing request count is
  what produced the 28× improvement in open latency; the byte cost of doing so is
  real but small.

**Not supported:**

- That slidekit decodes faster than OpenSlide. `osl_read` carries a PNG encode
  slidekit never does. Unresolved until in-process FFI exists.
- Any conclusion about remote *seconds* on other infrastructure. This endpoint
  costs ~583 ms per request on an established connection; request counts port,
  wall-clock does not.
- That the read-ahead block sizes are tuned. They were chosen to fit the access
  patterns in these eleven slides and validated by request counts, not swept.
- Anything about JPEG-2000 or JPEG-XR slides, which slidekit cannot decode, or
  about Sakura, which has no public fixture.
- Single-shot measurements throughout: no repetitions, no variance, no confidence
  intervals. Locally, differences under a few tens of milliseconds are noise, and
  `osl_open` values near zero are the process-floor subtraction bottoming out.

## Reproducing

```sh
mise run bench:setup-slidetool   # build OpenSlide 4.0.1 into .tools (once)
mise run bench:warm-cache        # download + verify fixtures (~5.8 GB, once)
mise run bench:openslide         # Part 1 — via mise, so 4.0.1 is on PATH

# Part 2 needs an S3-compatible bucket configured in a git-ignored .env:
#   AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, BUCKET_NAME,
#   AWS_REGION, AWS_ENDPOINT_URL
mise run bench:upload-slides     # mirror the slide set (once)
mise run bench:remote            # Part 2
```

The `AWS_*` spellings are what `OpenSlide::open_url` itself reads, so a `.env`
using them configures the bench store and the public URL entry point identically.
`BUCKET_REGION` / `BUCKET_ENDPOINT_URL` are still accepted as aliases, but with
only those set the `open_url` test skips.

Both remote tasks skip with an explanation when no bucket is configured, and the
uploader writes only under `SLIDEKIT_BENCH_PREFIX` (default
`slidekit-bench/testdata`) and never deletes, so a shared bucket is safe.

Useful knobs while investigating:

| variable | effect |
|---|---|
| `SLIDEKIT_BENCH_ONLY=Aperio` | restrict to matching cases (comma-separated) |
| `SLIDEKIT_BENCH_TRACE=512` | log every request ≥ 512 KiB — how findings 2 and A were located |
| `SLIDEKIT_BENCH_BT=1` | print a backtrace at each traced request |
| `SLIDEKIT_BENCH_BUDGET_SECS=1800` | total wall-clock budget; remaining slides reported as not-run |
| `SLIDEKIT_BENCH_DUMP=/tmp/cmp` | dump both readers' regions as PNGs (`bench:openslide`) — how the Leica padding band was found |

The whole remote bench takes ~2 minutes now, so the budget knob rarely matters.
