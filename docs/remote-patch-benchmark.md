# Remote patch extraction — are the patches right, what do they cost, and how many API calls?

Measured 2026-08-06 at [`6eabf4c`](../PROGRESS.md) plus the bench changes described
in [How this is measured](#how-this-is-measured). Three questions, in the order
they matter:

1. **Are the patches slidekit extracts from remote storage correct?** Yes — every
   one of 120 patches across 13 slides is byte-identical to the same patch read
   from local disk, and agrees with OpenSlide 4.0.1 within decoder tolerance.
   Structural disagreements at tile seams on three slides are real, and the
   clearest of them is characterised below.
2. **What do they cost in latency?** A remote *open* costs 3.0–11.6 s and is
   entirely round-trip-bound. A patch from an already-open slide costs 14 ms–6.3 s,
   most often 0.5–1.0 s, against 6–67 ms from local disk.
3. **How many API calls?** 10–27 GET requests to open a slide (Zeiss 151, the one
   outlier), then **~2 requests per 1024×1024 patch** — 2.1 for spatially adjacent
   patches, 2.3 for scattered ones.

Everything below is the real output of `mise run bench:remote` and
`mise run bench:openslide`; reproduce with the commands in
[Reproducing](#reproducing). This report supersedes Part 2 of
[benchmarks.md](benchmarks.md), which compared remote pixels only against
slidekit's own local reads, over one window per slide.

## Environment

| | |
|---|---|
| Machine | Apple M4 Pro, 12 cores, 24 GB RAM, arm64 |
| OS | macOS 26.5.2 (Darwin 25.5.0) |
| Toolchain | rustc 1.96.1, `--release` |
| Reference reader | `slidetool 4.0.1, using OpenSlide 4.0.1`, from `.tools/openslide/bin/slidetool` — printed by the bench, not assumed |
| Slides | 13 from the OpenSlide test-data server, SHA-256 verified (`tests/testdata.yaml`) |
| Remote storage | S3-compatible endpoint (not AWS), over the public internet |
| Patch | 1024×1024 at level 0 |

The endpoint's own per-request cost, measured by the bench itself before it
touches a slide — ten sequential 1-byte ranged GETs on an object already in the
bucket:

| min | median | max |
|---|---|---|
| 355 ms | **360 ms** | 565 ms |

**Every remote figure in this report is a multiple of that 360 ms.** It is the
single most important number for reading the rest: it means the request *counts*
are the portable result and the seconds are not. A same-region S3 endpoint at
~20 ms per request would run the same request counts roughly 18× faster.

## How this is measured

The remote bench already existed. Answering the question as asked needed four
changes to it, each of which closed a way the old measurement could have looked
better than the truth.

**1. The patches are now compared against OpenSlide directly.** Previously remote
pixels were compared only against slidekit's *own* local reads (exactly), and a
separate bench compared local reads against OpenSlide (tolerantly). That chain is
suggestive but it cannot catch a decode or geometry bug that both slidekit paths
share — they run the same decoder. Every patch is now also diffed against
`slidetool` reading the same window.

OpenSlide has no remote backend at all: it treats a URL as a filename
(`slidetool: s3://bucket/key.svs: Not a file that OpenSlide can recognize`), so
the reference necessarily reads the *local* copy of each slide. That is what makes
it a reference for correctness and for local latency, and not a remote-latency
comparison.

Two comparisons are kept, because they answer different questions:

| comparison | tolerance | what it would catch |
|---|---|---|
| remote slidekit vs local slidekit | **exact**, pixel for pixel | the I/O layer misplacing data: range math, `SlideFs` sibling resolution, size clamping |
| remote slidekit vs OpenSlide | mean per-channel diff, ceiling 15 | a wrong tile, a wrong coordinate space, a channel swap, a colour-space error |

The tolerance is not laxity. slidekit returns premultiplied ARGB and `slidetool`
straight RGBA, so only pixels opaque in *both* are comparable; and independent
JPEG/JPEG-2000 decoders legitimately differ by a count or two per channel.

**2. Ten patches per slide, at two localities, instead of one window.** One patch
measures one path through the tile grid. It also cannot show what a patch costs,
because locality decides that: five *adjacent* patches (a viewer panning, where
one patch's read-ahead can cover the next) and five *scattered* patches at least
six window-widths apart on tissue (a tiling job, where nothing cached can serve
them). The pair brackets the answer instead of quoting the flattering half.

This immediately paid for itself. Zeiss's densest-tissue window agrees with
OpenSlide *exactly* — 0.00 mean, 0 max, reproduced in this session's local bench —
and on eight windows it disagrees by up to 109 per channel ([finding A](#a-zeiss-disagrees-with-openslide-in-mosaic-overlap-strips)).
Mirax's worst-case difference went from 36 to 142 the same way.

**3. Requests and bytes are counted per patch**, not just per phase, through an
`ObjectStore` wrapper that every request passes through. HEAD probes are counted
as requests but contribute no bytes: a metadata response reports the whole-object
range, and charging that would make a lazy open look like a full download.

**4. Patches are screened for actual specimen.** The tissue search works on the
coarsest pyramid level — a thumbnail — so a window it likes can be mostly white at
full resolution. **A blank window agrees between any two readers**, because it
holds no tile content to get wrong, so counting it as agreement inflates the
result. Candidates are now read at level 0 first and skipped below 10% specimen,
and the surviving share is reported per slide as `tissue%`.

That screen exposed a real defect in the search itself: its tissue mask counted
*near-black* pixels as stain, and several vendors surround the specimen with black
padding in the coarse levels. Before the fix, Ventana's patches averaged **13%
specimen with one at 1%**, and DICOM/Leica-4 had a patch at **0%** — those rows'
agreement with OpenSlide was substantially a claim about white space. After it,
the weakest slide in the set is Ventana at 23% mean / 12% worst.

## 1. Correctness

| vendor | patches | remote == local | vs OpenSlide | worst mean_d | worst max_d | opaque overlap | tissue (mean/min) |
|---|---|---|---|---|---|---|---|
| Aperio/CMU-1 | 5+5 | **yes** | 10 patches | 0.01 | 1 | 100% | 98% / 96% |
| Aperio/JP2K-33003-1 | 5+3 | **yes** | 8 patches | 4.61 | 48 | 100% | 73% / 21% |
| Aperio/CMU-1-JP2K-33005 | 5+5 | **yes** | 10 patches | 4.70 | 49 | 100% | 99% / 98% |
| Hamamatsu | 5+5 | **yes** | 10 patches | 0.00 | 2 | 100% | 100% / 99% |
| Leica | 5+5 | **yes** | 10 patches | 0.08 | 3 | 100% | 74% / 37% |
| Philips-TIFF | 5+5 | **yes** | 10 patches | 0.07 | 3 | 100% | 96% / 85% |
| Generic-TIFF | 5+5 | **yes** | 10 patches | 0.07 | 3 | 100% | 92% / 60% |
| Trestle | 5+5 | **yes** | 10 patches | 0.07 | 3 | 100% | 97% / 95% |
| **Mirax** | 5+5 | **yes** | 10 patches | 0.56 | **142** | 100% | 87% / 63% |
| **Zeiss** | 5+3 | **yes** | 8 patches | 3.09 | **109** | 99% | 85% / 17% |
| Ventana | 4+5 | **yes** | 9 patches | 2.94 | 70 | 100% | 23% / 12% |
| DICOM/Leica-4 | 5+0 | **yes** | 5 patches | 0.06 | 3 | 100% | 35% / 30% |
| DICOM/Hamamatsu-2 | 5+5 | **yes** | 10 patches | 0.01 | 3 | 100% | 94% / 79% |

`patches` is adjacent+scattered. `mean_d`/`max_d` are the **worst** per-channel RGB
difference over any patch on that slide, so the table cannot average a bad patch
away. Three rows report fewer than ten patches because the slide could not supply
ten windows meeting the 10% specimen bar at that locality — DICOM/Leica-4 offered
no scattered window at all (its five candidates were all under the bar), so its
scattered locality is unmeasured rather than guessed.

**The remote path is exact.** All 120 patches are byte-identical to the local read.
That is the strongest single result here: range arithmetic, sibling resolution for
the multi-file formats (MIRAX, DICOM), read-ahead block boundaries and size
clamping all place bytes correctly, on every vendor.

**Eight of thirteen slides agree with OpenSlide to within 0.08 per channel** with a
max of 3 — decoder rounding, nothing more. The five that do not divide into two
kinds:

*Expected codec noise.* The two lossy JPEG-2000 Aperios (4.61 and 4.70 mean) are
9/7 irreversible codestreams reconstructed by `hayro-jpeg2000` rather than
OpenJPEG; two conforming decoders may differ by a few counts. What rules out a
colour bug is that the *signed* difference is ~0 per channel while the absolute
mean is 3–5: unbiased scatter, not a cast. This matches the figure recorded in
[benchmarks.md](benchmarks.md#addendum-2026-08-06--the-jpeg-2000-aperio-slides).

*Structural, at tile seams.* Mirax (mean 0.56, max 142), Zeiss (3.09, max 109) and
Ventana (2.94, max 70) disagree in bands rather than uniformly. Zeiss is the
clearest case and is worth stating precisely, because it is invisible to any
single-window comparison — see below.

### Looking at the patches

Numbers say how far apart two readers are; they do not say whether the difference
is a rounding wobble or a displaced tile. `SLIDEKIT_BENCH_DUMP=<dir>` writes all
three images for every patch — slidekit's, OpenSlide's, and their difference
amplified 8× — with `patches.csv` and an `index.html` contact sheet sorted
worst-first, so the disagreements are the first thing on screen.

The difference image is the useful one. Decoder noise of ±1–2 stays black even
amplified; a real disagreement is a shape. Pixels that are not comparable (opaque in
one reader only) are painted dark blue rather than black, so "no data" cannot be
misread as "agrees". The thumbnails are box-averaged rather than sampled, because a
nearest-neighbour thumbnail can drop a one-pixel seam between sample points and hide
exactly what you are looking for.

**In every one of the 120 patches, slidekit's image and OpenSlide's are visually
indistinguishable.** No patch shows displaced, missing, duplicated or wrongly
coloured tissue. What the difference images do show is three distinct signatures,
and they separate cleanly along the two statistics — which is why the table reports
both a mean and a max rather than one number:

| signature | looks like | worst example | mean | max |
|---|---|---|---|---|
| clean | solid black even at 8× | Aperio/CMU-1 `adjacent0` | 0.008 | 1 |
| lossy codec | an even wash over the whole patch | Aperio/CMU-1-JP2K-33005 `scattered4` | 4.702 | 36 |
| mosaic overlap | a band, fixed in slide coordinates | Zeiss `scattered2` | 1.354 | 109 |
| subtile seam | one small box, sparse spikes inside it | Mirax `scattered1` | 0.228 | **142** |

**A high mean and a high max mean opposite things.** The JP2K slides have the
highest means in the set and the *lowest* maxima of the four — every pixel is
slightly off, none badly, which is exactly two conforming lossy decoders. Mirax is
the reverse: the highest max in the whole suite (142) with a mean of 0.228, because
only 1848 pixels of 1048576 (0.176%) differ at all, every one of them inside a
355×122 box at window offset (20,189). Profiling that window puts the worst pixel on
a high-contrast edge — slidekit `[220, 228, 241]` against OpenSlide
`[142, 86, 177]` — which is a sampling difference at a subtile boundary, matching
the integer-vs-cairo seam already recorded in `PROGRESS.md`, not a misplaced tile.

Zeiss sits between them and is the one worth pursuing, because its slide is
uncompressed and so has no codec noise to excuse anything — see below.

### A. Zeiss disagrees with OpenSlide in mosaic overlap strips

`Zeiss-5-Uncompressed.czi` is **uncompressed**. There is no codec, so two correct
readers must agree bit-for-bit, and over most of a window they do. Re-reading one of
the disagreeing windows with the new profiler (`SLIDEKIT_BENCH_WINDOW=42496,6784
SLIDEKIT_BENCH_PROFILE=1`, the worst window of an earlier run over this slide set)
shows exactly where they do not — `.` is zero difference, `#` is ≥10:

```
profile: 199061 of 1048576 both-opaque pixels differ by more than 8 (18.984%)
worst pixel at +(241,19): slidekit rgb [56, 45, 61] vs OpenSlide rgb [68, 91, 165]
mean max-channel diff per 24x24 cell:
  ######################87
  ########################
  #######9#99#98888#####96
  .7####7.................
  .7####6.................
  .5####7.................
  .6####5.................
  ...
```

A ~128 px band along the top and a ~260 px band down the left disagree strongly;
the remaining ~80% of the window is *identical*. Reading a window shifted by
(−512, −512) puts the same bands at slide coordinates x ≈ 42538–42795 and
y ≈ 6613–6912 — **fixed in slide space, not window space**. That is a tile seam,
not a placement off-by-one.

The cause is tile priority in overlap regions. CZI mosaic subblocks overlap, and
`ZeissLevel::read_region` composites them in subblock-directory order, so the last
subblock written wins; OpenSlide draws through its own tilemap grid and can pick a
different one.

Side by side the two patches are indistinguishable — the same glomeruli in the same
places — because both subblocks image the *same* area. That is the finding's real
shape: not a displaced tile, but a choice between two overlapping acquisitions that
differ in fine detail, exposure and registration. It is why nothing short of a
pixel comparison against an independent reader finds it, and why the mean stays
inside tolerance while ~19% of a window differs.

It is inside the bench's tolerance and it is not a data-corruption bug — but on a
slide with no codec, any difference is a decision one of the two readers is making
differently, and 19% of a window is a lot of pixels to leave to whichever order the
subblock directory happens to list. Resolving it means deciding which subblock CZI
intends to win and matching that deliberately.

## 2. Latency

| vendor | level 0 | local open | local patch | remote open | cold patch | adjacent patch | scattered patch | OpenSlide process |
|---|---|---|---|---|---|---|---|---|
| Aperio/CMU-1 | 46000×32914 | 48.5 ms | 17.6 ms | 7.6 s | 1015 ms | 1012 ms | 856 ms | 209 ms |
| Aperio/JP2K-33003-1 | 15374×17497 | 21.9 ms | 66.9 ms | 5.7 s | 1163 ms | 786 ms | 1023 ms | 236 ms |
| Aperio/CMU-1-JP2K-33005 | 46000×32893 | 27.2 ms | 28.5 ms | 6.4 s | 792 ms | 1069 ms | 908 ms | 266 ms |
| Hamamatsu | 51200×38144 | 14.0 ms | 23.1 ms | 5.6 s | 7545 ms | 539 ms | 21 ms | 65 ms |
| Leica | 53130×153470 | 71.1 ms | 14.2 ms | 6.0 s | 496 ms | 642 ms | 473 ms | 200 ms |
| Philips-TIFF | 91136×68096 | 14.3 ms | 5.8 ms | 7.1 s | 555 ms | 534 ms | 542 ms | 328 ms |
| Generic-TIFF | 46000×32914 | 5.3 ms | 16.0 ms | 4.6 s | 824 ms | 747 ms | 893 ms | 193 ms |
| Trestle | 40000×27712 | 10.3 ms | 15.1 ms | 3.8 s | 610 ms | 555 ms | 530 ms | 300 ms |
| Mirax | 109240×220696 | 137.2 ms | 13.7 ms | 3.9 s | 3342 ms | 1885 ms | 2872 ms | 318 ms |
| **Zeiss** | 50171×11340 | 28.3 ms | 14.3 ms | **11.6 s** | 2757 ms | **6337 ms** | 4819 ms | 280 ms |
| Ventana | 114942×76349 | 31.0 ms | 33.9 ms | 7.5 s | 1923 ms | 2044 ms | 1977 ms | 372 ms |
| DICOM/Leica-4 | 23374×22079 | 27.7 ms | 15.9 ms | 6.4 s | 4652 ms | **14 ms** | — | 530 ms |
| DICOM/Hamamatsu-2 | 30720×39168 | 16.9 ms | 10.9 ms | 3.0 s | 521 ms | 504 ms | 464 ms | 310 ms |

`local patch` is the median over every patch. `adjacent`/`scattered` are medians
over those sets, from the already-open remote slide. `OpenSlide process` is the
fastest whole `slidetool region read` over the same windows on the local file —
process startup + open + read + PNG encode, the only granularity the CLI offers.

**Remote open is round-trip-bound, and that is the whole story.** Multiply each
slide's open request count (next section) by 360 ms and you land within noise of
its open time: Trestle's 10 requests → 3.6 s against 3.8 s measured; Zeiss's 151 →
54 s, and it comes in at 11.6 s only because those requests are issued as one
concurrent batch. Bytes are not the constraint; requests are.

**Two rows show what caching does rather than what the format costs.**
DICOM/Leica-4's 14 ms adjacent patch is its memoized fragment walk being reused,
after a
4.7 s cold patch paid for it. Hamamatsu's 21 ms scattered patch is the same
effect: its NDPI level 0 is a single JPEG strip, its cold patch spent 7.5 s
scanning restart markers, and its scattered windows all fall in the band that scan
already covered (see [the caveat](#what-these-numbers-do-and-dont-show)).

**Local opens are 5–137 ms and local patches 6–67 ms, against 65–530 ms for
OpenSlide's whole process** — but that gap is not a decode-speed claim.
`slidetool` re-opens the slide and PNG-encodes the region on every invocation, and
slidekit does neither. A like-for-like decode comparison needs in-process FFI,
which does not exist yet.

## 3. API calls — the portable result

| vendor | open reqs | open MB | adjacent req/patch | adjacent MB/patch | scattered req/patch | scattered MB/patch | slide MB | fetched % |
|---|---|---|---|---|---|---|---|---|
| Aperio/CMU-1 | 18 | 1.33 | 2.2 | 2.44 | 2.6 | 2.29 | 169.3 | 14.8% |
| Aperio/JP2K-33003-1 | 14 | 1.05 | 2.8 | 2.00 | 1.0 | 3.99 | 60.9 | 41.5% |
| Aperio/CMU-1-JP2K-33005 | 16 | 1.19 | 1.0 | 3.61 | 1.0 | 3.43 | 126.4 | 28.8% |
| Hamamatsu | 14 | 1.49 | 3.0 | 1.09 | 1.2 | 0.40 | 188.9 | 5.9% |
| Leica | 14 | 2.33 | 2.5 | 0.40 | 3.0 | 0.65 | 278.3 | 2.8% |
| Philips-TIFF | 18 | 1.24 | 1.0 | 0.51 | 1.0 | 0.64 | 277.5 | 2.5% |
| Generic-TIFF | 12 | 0.70 | 1.5 | 3.21 | 2.6 | 2.29 | 194.7 | 15.0% |
| Trestle | **10** | 0.62 | 1.0 | 0.55 | 1.0 | 0.49 | 158.9 | 3.7% |
| Mirax | **10** | 1.24 | 4.2 | 0.44 | 6.4 | 0.89 | 538.9 | 1.6% |
| **Zeiss** | **151** | 9.59 | 2.8 | **39.86** | 1.7 | **24.16** | 1004.9 | 26.9% |
| Ventana | 18 | 2.99 | 2.7 | 0.97 | 3.6 | 1.49 | 2591.9 | **0.6%** |
| DICOM/Leica-4 | 27 | 6.25 | 0.2 | 0.01 | — | — | 107.5 | 28.2% |
| DICOM/Hamamatsu-2 | 19 | 1.32 | 2.0 | 0.29 | 2.2 | 0.36 | 197.8 | 2.4% |

**Totals: 13 slides, 341 requests to open all of them; 2.1 requests per adjacent
patch and 2.3 per scattered patch.**

**Opening a slide costs 10–27 requests and 0.6–6.3 MB**, on files from 61 MB to
2.6 GB. Ventana is the clearest demonstration: 18 requests and 3.0 MB — 0.1% of the
file — to open a **2.6 GB** slide. DICOM/Leica-4 is the most expensive open at
6.25 MB, because a DICOM slide is a directory of instances that must each be
parsed. Zeiss's 151 requests are the one real outlier, and the cause is known and
unchanged ([finding B](#b-zeiss-open-still-costs-151-requests)).

**A patch costs 1–3 requests on eleven of thirteen slides**, and that is the
headline answer to "how many API calls under the hood". It is close to the floor: a
1024×1024 level-0 patch spans 16–25 tiles of 256², so two requests means the tile
ranges are being coalesced rather than fetched one at a time. Ventana reaches 3.6
scattered, and Mirax is the outlier at 4.2 adjacent / 6.4 scattered because its
subtiles are small and scattered through the file.

**Locality barely matters**: 2.1 requests per adjacent patch against 2.3 scattered,
across the set. That is worth stating because it locates where the saving comes
from — range coalescing *within* one region read, not cache hits *between* reads.
A tiling job that never revisits a region pays essentially the same as a viewer
panning across one.

### B. Zeiss open still costs 151 requests

More than the other twelve slides combined (190 for all of them). The CZI directory
says where each subblock is but not where its pixels start, so it takes one 16-byte
header read per subblock, and they sit megabytes apart in a 1 GB file with nothing
to coalesce. They are already batched and issued concurrently, which is why 151
requests take 11.6 s rather than 54 s. Going further means caching the resolved
offsets or finding metadata that removes the second pass. Unchanged from
[benchmarks.md finding B](benchmarks.md#b-zeiss-open-costs-151-requests).

### C. Bytes per patch are read-ahead blocks, not tiles

The byte columns are the uncomfortable part of this table, and they are a
consequence of the trade the read-ahead layer makes. Tracing requests ≥512 KiB on
JP2K-33003-1 (`SLIDEKIT_BENCH_TRACE=512`) shows each patch is served by exactly one
large GET:

```
GET 39814949..44334058 (4.31 MB)
GET 39927237..43263293 (3.18 MB)
GET 32139954..36322269 (3.99 MB)
GET 25008995..28917577 (3.73 MB)
GET  7417643..11866112 (4.24 MB)
```

Those are the read-ahead block at or near its 4 MiB ceiling. That slide's level 0
holds a few hundred KB per tile, so the tiles behind a 1024×1024 patch come to well
under 1 MB: **over 4× more bytes cross the wire than the patch needs**. The block
doubles on sequential access, and a multi-tile region read looks sequential.

At *this* endpoint the trade is about break-even: 4 MB costs ~110–330 ms at the
observed 12–36 MB/s, against ~360 ms for each request saved. On a low-latency
endpoint it is clearly the wrong side — a same-region S3 at ~20 ms per request
would rather issue four small GETs than move 4 MB. **The block growth should be
bounded by what the read actually asked for**, or exposed as a knob, before this
is pointed at low-latency infrastructure.

Zeiss's 24–40 MB per patch is a different and mostly inherent cost: its subblocks
are large and uncompressed, so a 1024×1024 window genuinely straddles tens of MB
of pixel data. `fetched %` for Zeiss, the JP2K Aperios and DICOM/Leica-4 is
correspondingly high — but note that column now covers an open *plus up to ten
patches*, so it is not comparable with the single-patch figures in
[benchmarks.md](benchmarks.md#traffic--the-actual-result).

### D. The reference reader was unverified (now fixed)

The previous report noted that a bare `cargo test` picks up Homebrew's `slidetool`
4.0.0 instead of the 4.0.1 in `.tools`, which has no Zeiss driver and reports Zeiss
as incomparable with no hint that the wrong binary was used. Both benches now print
the version **and the resolved path**, and warn loudly on anything but 4.0.1.

Fixing it needed a second fix, worth recording because it is the kind of bug that
makes a harness lie in the reassuring direction: `slidetool --version` writes to
**stderr**, so the first version probe read an empty stdout and the bench announced
`slidetool not on PATH — reporting slidekit latency only` while its OpenSlide
columns were visibly populated from the same binary.

## What these numbers do and don't show

**Supported by the data:**

- Remote patches are byte-identical to local patches on all 13 slides, 120 of 120
  patches, at two localities. The remote I/O layer does not alter a pixel.
- Remote patches agree with OpenSlide 4.0.1 over windows containing real specimen:
  eight slides within 0.08 per channel, the rest explained by lossy-codec noise or
  localised tile seams.
- Opening a slide from object storage costs 10–27 requests and ≤2.5 MB (Zeiss 151
  excepted), including 18 requests for a 2.6 GB Ventana.
- A 1024×1024 patch costs ~2 requests once the slide is open, and locality changes
  that by ~10%.
- Remote latency is request-count × per-request cost, at this endpoint 360 ms.

**Not supported:**

- **That slidekit decodes faster than OpenSlide.** `osl_proc` carries a process
  start and a PNG encode slidekit never does. Unresolved until in-process FFI.
- **Any conclusion about remote seconds on other infrastructure.** Request counts
  port; wall-clock does not.
- **That the read-ahead block sizes are right.** [Finding C](#c-bytes-per-patch-are-read-ahead-blocks-not-tiles)
  shows they cost ~8× the necessary bytes on tiled formats. They were chosen to
  minimise request counts and validated on that axis only, never swept.
- **Hamamatsu's per-patch cost as a worst case.** Its scattered windows all land in
  one horizontal band (the densest tissue on that slide sits there), and NDPI
  stores level 0 as one JPEG strip, so those patches reuse the restart-marker scan
  the cold patch paid for. Its 0.2–1.2 requests per patch is a lower bound. The
  scattered selection enforces Chebyshev separation, which cannot demand separation
  in both axes — on a wide, short slide like Zeiss (50171×11340) there is only one
  row of windows to choose from at six window-widths.
- **DICOM/Leica-4's scattered locality**, which is unmeasured: no candidate window
  outside the primary neighbourhood held 10% specimen.
- **Single-shot timings throughout.** Medians over 5 patches per locality, but no
  repetitions of the same read, no variance, no confidence intervals. Differences
  under a few hundred ms remotely, or a few tens of ms locally, are noise.
- **Anything about JPEG XR CZI or Sakura**, which slidekit cannot read (matching
  OpenSlide for JXR) or has no public fixture.

## Reproducing

```sh
mise run bench:setup-slidetool   # build OpenSlide 4.0.1 into .tools (once)
mise run bench:warm-cache        # download + verify fixtures (~6.8 GB, once)
mise run bench:upload-slides     # mirror the slide set into the bucket (once)

mise run bench:openslide         # local slidekit vs OpenSlide
mise run bench:remote            # this report (~5 min, 13 slides × 10 patches)
```

Run them **through `mise`**, which puts `.tools/openslide/bin` first on `PATH` and
loads the bucket credentials from the git-ignored `.env`
(`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `BUCKET_NAME`, `AWS_REGION`,
`AWS_ENDPOINT_URL`). Both remote tasks skip with an explanation when no bucket is
configured, and the uploader only ever writes under `SLIDEKIT_BENCH_PREFIX`
(default `slidekit-bench/testdata`) and never deletes, so a shared bucket is safe.

Knobs, all of which were used in producing this report:

| variable | effect |
|---|---|
| `SLIDEKIT_BENCH_ONLY=Zeiss` | restrict to matching cases (comma-separated) |
| `SLIDEKIT_BENCH_PATCHES=5` | patches per locality per slide |
| `SLIDEKIT_BENCH_MIN_TISSUE=10` | minimum specimen share (%) for a window to be used |
| `SLIDEKIT_BENCH_WINDOW=42496,6784` | re-read one specific window instead of searching for tissue (`bench:openslide`) — this is how a `worst_window` from the correctness table gets investigated |
| `SLIDEKIT_BENCH_PROFILE=1` | print *where* inside the window the readers disagree: bounding box, worst pixel, and a 24×24 heat map (`bench:openslide`) |
| `SLIDEKIT_BENCH_TRACE=512` | log every request ≥512 KiB — how finding C was confirmed |
| `SLIDEKIT_BENCH_DUMP=/tmp/patches` | write **every** patch as PNGs — slidekit's output, OpenSlide's, and an amplified difference image — plus `patches.csv` and a browsable `index.html` sorted worst-disagreement first. Roughly 5 MB per patch, so ~600 MB for the full set |
| `SLIDEKIT_BENCH_THUMB=320` | longest side of the contact-sheet thumbnails |
| `SLIDEKIT_BENCH_DIFF_GAIN=8` | multiplier on the difference image, so decoder noise stays black |
| `SLIDEKIT_BENCH_SHEET=1` | rebuild `index.html` from an existing `patches.csv` without re-reading any slide (`cargo test --release --test bench_remote -- --ignored contact_sheet`) |
| `SLIDEKIT_BENCH_BUDGET_SECS=1800` | wall-clock budget; remaining slides reported as not-run |
