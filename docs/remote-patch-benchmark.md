# Remote patch extraction — are the patches right, what do they cost, and how many API calls?

Measured 2026-08-07, on a working tree that swaps the JPEG 2000 decoder from
`hayro-jpeg2000` to OpenJPEG and is not yet committed; everything else is
[`33a209e`](../PROGRESS.md). Three questions, in the order they matter:

1. **Are the patches slidekit extracts from remote storage correct?** Yes — every
   one of 119 patches across 13 slides is byte-identical to the same patch read
   from local disk, and agrees with OpenSlide 4.0.1 within decoder tolerance. The
   two JPEG 2000 Aperios are now **bit-identical** to OpenSlide, 0.00 mean and 0
   max over every patch. Structural disagreements at tile seams on three slides
   are real, and the clearest of them is characterised below.
2. **What do they cost in latency?** A remote *open* costs 3.0–9.4 s and is
   entirely round-trip-bound. A patch from an already-open slide costs 11 ms–4.8 s,
   most often 0.5–1.4 s, against 5.8–50 ms from local disk. These seconds are ~14%
   larger than the previous run's for reasons in the endpoint, not the code — see
   [Environment](#environment).
3. **How many API calls?** 10–27 GET requests to open a slide, then **under 2
   requests per 1024×1024 patch**: 1.6 for spatially adjacent patches, 1.8 for
   scattered ones. For the first time in this report no slide exceeds 3 requests
   per patch at either locality.

Since the previous run, one thing changed: **JPEG 2000 now decodes through
OpenJPEG**, the library OpenSlide itself uses, behind a default `openjpeg` feature
(`hayro-jpeg2000` remains available as `--features hayro` for targets that cannot
link C). The previous run's report called the two Aperio JP2K slides' 4.6–4.7 mean
difference expected codec noise. It was a decoder defect, and it is gone: both
slides are now bit-exact against OpenSlide at every patch, at every locality
([finding H](#h-the-jpeg-2000-slides-are-now-bit-exact-fixed)).

**Every other row of the correctness table is identical to the previous run**, to
the last decimal place, and so is every open request count and byte count in the
suite. That is the result worth taking from a run that replaced a decoder: nothing
outside its blast radius moved. Mirax still reports 0.56/142, Zeiss 3.09/109 and
Ventana 2.94/70 — the three structural tile-seam disagreements are untouched,
because they were never about JPEG 2000.

**Absolute latencies in this report are not comparable with the previous run's.**
The endpoint measured a 401 ms median per-request cost this session against 352 ms
last time, so nearly every remote second here is larger for reasons that have
nothing to do with the code. Request counts are the portable result; see
[Environment](#environment).

Two numbers from the previous run stand and were not re-derived: DICOM/Leica-4's
cold patch remains the one regression at 5.8 s
([finding G](#g-dicomleica-4s-cold-patch-got-slower)), and the tile cache still
changes almost nothing
([finding F](#f-the-tile-cache-is-wired-and-barely-changes-request-counts)). Where
a number quoted here was not re-measured this run, it says so.

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
| 348 ms | **401 ms** | 758 ms |

**Every remote figure in this report is a multiple of that 401 ms.** It is the
single most important number for reading the rest: it means the request *counts*
are the portable result and the seconds are not. A same-region S3 endpoint at
~20 ms per request would run the same request counts roughly 20× faster.

This session's endpoint was **slower** than the previous run's (349/**352**/690 ms)
and matches the run before that (360/**401**/875 ms); its min is unchanged, so what
moved is the tail, not the floor. The spread between its own min and max is wider
than most of the differences between slides. **Every remote latency in this report
therefore rose for reasons that have nothing to do with the code** — the previous
run's seconds are not a baseline this one can be diffed against, and the only honest
way to compare is alongside the request count, which is why the findings below quote
both. Remote seconds should be read as "roughly this many round trips", never
compared with each other at finer than a few hundred ms.

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

This immediately paid for itself, and it still does. Three of Zeiss's eight windows
agree with OpenSlide *exactly* — 0.000 mean, 0 max, the densest-tissue window
(3568,5312) among them — while the other five disagree by up to 109 per channel
([finding A](#a-zeiss-disagrees-with-openslide-in-mosaic-overlap-strips)). A
one-window bench reads the densest tissue, so it would have picked one of the three
clean ones and reported a bit-exact match. Mirax's worst-case difference went from
36 to 142 the same way.

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
| **Aperio/JP2K-33003-1** | 5+2 | **yes** | 7 patches | **0.00** | **0** | 100% | 72% / 27% |
| **Aperio/CMU-1-JP2K-33005** | 5+5 | **yes** | 10 patches | **0.00** | **0** | 100% | 98% / 97% |
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
away. The two are maximised *independently*, so they need not describe the same
patch: Zeiss's 3.09 mean is window (3568,6336) and its 109 max is (49152,4160), a
different window entirely. That is deliberate — either statistic alone hides a
failure the other catches — but it means re-reading the `worst_window` the bench
prints reproduces the mean, not necessarily the max. Four rows report fewer than
ten patches because the slide could not supply
ten windows meeting the 10% specimen bar at that locality — DICOM/Leica-4 offered
no scattered window at all (its five candidates were all under the bar), so its
scattered locality is unmeasured rather than guessed. JP2K-33003-1 supplied two
scattered windows here against three last run; the tissue screen re-searches every
run and its marginal windows are not stable, which is also why that row's tissue
share reads 72%/27% against 73%/21%.

**The remote path is exact.** All 119 patches are byte-identical to the local read.
That is the strongest single result here: range arithmetic, sibling resolution for
the multi-file formats (MIRAX, DICOM), read-ahead block boundaries and size
clamping all place bytes correctly, on every vendor.

**Ten of thirteen slides agree with OpenSlide to within 0.08 per channel** with a
max of 3, and two of those ten are exact. The three that do not are all the same
kind of problem:

*Bit-exact.* The two lossy JPEG-2000 Aperios now reproduce OpenSlide's pixels
exactly — 0.00 mean, 0 max, on all 17 patches between them. They were 4.61 and 4.70
before, which the previous run of this report attributed to two conforming decoders
disagreeing. That was wrong; see
[finding H](#h-the-jpeg-2000-slides-are-now-bit-exact-fixed).

*Structural, at tile seams.* Mirax (mean 0.56, max 142), Zeiss (3.09, max 109) and
Ventana (2.94, max 70) disagree in bands rather than uniformly. All three are
unchanged to the decimal from the previous run. Zeiss is the clearest case and is
worth stating precisely, because it is invisible to any single-window comparison —
see below.

Ventana is now the only *uniform* disagreement left in the suite. It is baseline
JPEG through `jpeg-decoder`, not JPEG 2000, so the decoder swap does not touch it,
and it has not been diagnosed.

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

**In every one of the patches, slidekit's image and OpenSlide's are visually
indistinguishable.** No patch shows displaced, missing, duplicated or wrongly
coloured tissue. (That inspection was done over the full dump on an earlier run.
This run wrote no dump: instead the two worst disagreement profiles — Zeiss and
Mirax below — were re-run by hand and reproduce to the pixel, and every per-slide
difference statistic in the suite is identical bar the two JP2K rows, which went to
zero.) What the difference images show is three distinct signatures, and they
separate cleanly along the two statistics — which is why the table reports both a
mean and a max rather than one number:

| signature | looks like | worst example | mean | max |
|---|---|---|---|---|
| clean | solid black even at 8× | Aperio/CMU-1 `adjacent0` | 0.008 | 1 |
| ~~lossy codec~~ | ~~an even wash over the whole patch~~ | ~~Aperio/CMU-1-JP2K-33005 `scattered4`~~ | ~~4.702~~ | ~~36~~ |
| mosaic overlap | a band, fixed in slide coordinates | Zeiss `scattered2` | 1.354 | 109 |
| subtile seam | one small box, sparse spikes inside it | Mirax `scattered1` | 0.228 | **142** |

**The "lossy codec" signature no longer exists in this suite** — it was the
JPEG 2000 decoder defect, not a property of lossy codecs, and those patches are now
solid black at 8× like the clean row
([finding H](#h-the-jpeg-2000-slides-are-now-bit-exact-fixed)). The row is struck
through rather than deleted because the *shape* it describes is still what a genuine
decoder difference would look like, and knowing that is what made it identifiable.

The remaining per-patch means in that table come from a `patches.csv` this run did
not regenerate; the per-slide maxima they roll up to are re-measured and identical,
and the two profiles quoted below reproduce exactly.

**A high mean and a high max mean opposite things.** Mirax is the extreme case: the
highest max in the whole suite (142) with a mean of 0.228, because only
1848 pixels of 1048576 (0.176%) differ at all, every one of them inside a 355×122
box at window offset (20,189). Profiling that window puts the worst pixel on a
high-contrast edge at +(115,309) — slidekit `[220, 228, 241]` against OpenSlide
`[142, 86, 177]` — which is a sampling difference at a subtile boundary, matching
the integer-vs-cairo seam already recorded in `PROGRESS.md`, not a misplaced tile.

Zeiss sits between the two and is the one worth pursuing, because its slide is
uncompressed and so has no codec noise to excuse anything — see below.

### A. Zeiss disagrees with OpenSlide in mosaic overlap strips

`Zeiss-5-Uncompressed.czi` is **uncompressed**. There is no codec, so two correct
readers must agree bit-for-bit, and on three of its eight windows they do, exactly.
On the other five they do not. Re-reading the worst-by-mean window with the
profiler (`SLIDEKIT_BENCH_WINDOW=3568,6336 SLIDEKIT_BENCH_PROFILE=1`) shows where
— `.` is zero difference, `#` is ≥10, one cell per 24×24 block of the window:

```
profile: 241677 of 1048576 both-opaque pixels differ by more than 8 (23.048%)
disagreement bounding box within the window: x 0..=1023, y 321..=1023
worst pixel at +(1001,321): slidekit rgb [78, 77, 111] vs OpenSlide rgb [163, 128, 118]
mean max-channel diff per 24x24 cell:      (abridged: 12 of 24 rows)
  ........................
  ........................   <- the top ~320 px are identical (7 such rows)
  ........................
  777878787777567687678878
  ########################
  ########################   <- a full-width band disagrees (5 such rows)
  ########################
  434#####7434444543444443
  ...####73...............
  ...6####4...............   <- below it, only a vertical strip (10 such rows)
  ...8####4...............
  ...#####3...............
```

The top ~320 px of the window are *identical*; a full-width band below that
disagrees strongly; below the band only a vertical strip at x ≈ 128–384 does. This
window's own statistics are mean 3.09 and max 85 — the slide's max of 109 comes
from a different window, (49152,4160), for the reason given
[above](#1-correctness).

Re-reading a window shifted by (−512, −512) — `SLIDEKIT_BENCH_WINDOW=3056,5824` —
moves the band's position *within the window* from y 321 to y 833, which is the
same slide coordinate **y = 6657** in both cases (6336+321 = 5824+833). It also
drops the disagreeing share from 23.0% to 13.3%, exactly as a fixed horizontal seam
sliding out of a moving window would. **The seam is fixed in slide space, not
window space** — a tile boundary, not a placement off-by-one.

The cause is tile priority in overlap regions. CZI mosaic subblocks overlap, and
`ZeissLevel::read_region` composites them in subblock-directory order, so the last
subblock written wins; OpenSlide draws through its own tilemap grid and can pick a
different one.

Side by side the two patches are indistinguishable — the same glomeruli in the same
places — because both subblocks image the *same* area. That is the finding's real
shape: not a displaced tile, but a choice between two overlapping acquisitions that
differ in fine detail, exposure and registration. It is why nothing short of a
pixel comparison against an independent reader finds it, and why the mean stays
inside tolerance while ~23% of a window differs.

It is inside the bench's tolerance and it is not a data-corruption bug — but on a
slide with no codec, any difference is a decision one of the two readers is making
differently, and 23% of a window is a lot of pixels to leave to whichever order the
subblock directory happens to list. Resolving it means deciding which subblock CZI
intends to win and matching that deliberately.

Two commits have now touched this code without touching that decision.
[`a52ed2f`](../PROGRESS.md) moved offset resolution out of open, and
[`33a209e`](../PROGRESS.md) put a tile cache in front of the compositing loop —
which is the riskier of the two here, because CZI subblocks overlap and the last one
written wins, so a cached subblock has to occupy the slot it would have had. It
does: every Zeiss figure in the correctness table is identical, and re-running the
profile above at both windows reproduces it to the pixel. The disagreement with
OpenSlide is unchanged because it was never a caching or offset question — it is
the compositing order itself, still deliberately untouched.

### H. The JPEG 2000 slides are now bit-exact (fixed)

The previous run of this report recorded 4.61 and 4.70 mean on the two Aperio JP2K
slides and explained them as expected codec noise — two conforming decoders
reconstructing the same lossy 9/7 codestream, with the ~0 *signed* difference
ruling out a colour cast. The signed statistic does rule out a cast. It does not
say anything about accuracy, because random error is unbiased too, and that is the
step the conclusion skipped.

| | before | after |
|---|---|---|
| Aperio/JP2K-33003-1 | 4.61 mean / 48 max | **0.00 / 0** |
| Aperio/CMU-1-JP2K-33005 | 4.70 mean / 49 max | **0.00 / 0** |

It was a decoder defect. `hayro-jpeg2000` 0.4 mis-reconstructs *truncated*
code-blocks — where the standard's midpoint rule adds half the undecoded remainder
— so it is accurate at high quality and wrong under aggressive rate control.
Aperio ships Kakadu `Q=30`, about 1:27, which is squarely in the broken regime.
Re-encoding one tile across a range of rates isolates it: the two decoders agree to
0.157–0.316 between 1:1 and 1:15, then diverge to 2.567 at 1:25 and 3.584 at 1:40.

Scored against the *original* image rather than against each other, on a real tile
re-encoded at 1:27, OpenJPEG reaches 42.99 dB PSNR and `hayro-jpeg2000` 34.97 dB —
the pure-Rust decoder was discarding about 8 dB of quality the encoder had already
paid for. slidekit now decodes JPEG 2000 with OpenJPEG by default, which is the
same library OpenSlide links, so the two agree by construction rather than by
tolerance. The full measurement is in
[benchmarks.md](benchmarks.md#addendum-2026-08-07--the-jp2k-difference-was-a-decoder-defect).

**Nothing else in this report moved**, which is the useful control: a decoder swap
should change decoded pixels and nothing else, and every open request count, every
byte count, and every other correctness row reproduced exactly.

## 2. Latency

| vendor | level 0 | local open | local patch | remote open | cold patch | adjacent patch | scattered patch | OpenSlide process |
|---|---|---|---|---|---|---|---|---|
| Aperio/CMU-1 | 46000×32914 | 53.3 ms | 16.3 ms | 9.3 s | 1229 ms | 945 ms | 951 ms | 205 ms |
| Aperio/JP2K-33003-1 | 15374×17497 | 26.4 ms | 50.2 ms | 7.8 s | 1416 ms | 826 ms | 1347 ms | 239 ms |
| Aperio/CMU-1-JP2K-33005 | 46000×32893 | 22.7 ms | 32.7 ms | 7.4 s | 1319 ms | 1016 ms | 1144 ms | 260 ms |
| Hamamatsu | 51200×38144 | 13.6 ms | 24.9 ms | 6.9 s | 1694 ms | 715 ms | 1445 ms | 64 ms |
| Leica | 53130×153470 | 51.9 ms | 12.3 ms | 7.0 s | 815 ms | 574 ms | 596 ms | 200 ms |
| Philips-TIFF | 91136×68096 | 10.9 ms | 5.8 ms | 9.4 s | 821 ms | 671 ms | 680 ms | 323 ms |
| Generic-TIFF | 46000×32914 | 9.0 ms | 13.8 ms | 6.0 s | 1384 ms | 1014 ms | 1129 ms | 179 ms |
| Trestle | 40000×27712 | 5.9 ms | 12.4 ms | **4.6 s** | 792 ms | 706 ms | 626 ms | 290 ms |
| Mirax | 109240×220696 | 147.6 ms | 13.1 ms | 4.7 s | 865 ms | 766 ms | 663 ms | 292 ms |
| Zeiss | 50171×11340 | 8.6 ms | 12.5 ms | 5.5 s | 4344 ms | **4820 ms** | 3990 ms | 256 ms |
| Ventana | 114942×76349 | 36.5 ms | 33.3 ms | **9.2 s** | 822 ms | 787 ms | 866 ms | 380 ms |
| DICOM/Leica-4 | 23374×22079 | 42.1 ms | 13.1 ms | 7.6 s | **5762 ms** | **11 ms** | — | 540 ms |
| DICOM/Hamamatsu-2 | 30720×39168 | 35.8 ms | 10.8 ms | **3.0 s** | 621 ms | 527 ms | 643 ms | 309 ms |

`local patch` is the median over every patch. `adjacent`/`scattered` are medians
over those sets, from the already-open remote slide. `OpenSlide process` is the
fastest whole `slidetool region read` over the same windows on the local file —
process startup + open + read + PNG encode, the only granularity the CLI offers.

**Remote open is round-trip-bound, and that is the whole story.** Multiply each
slide's open request count (next section) by the endpoint's 401 ms and you land
just under its measured open time: Trestle's 10 requests → 4.0 s against 4.6 s
measured, Philips-TIFF's 18 → 7.2 s against 9.4 s. Across the eleven single-file
slides the measured open is 1.14–1.40× that product — a fully serialized
round-trip chain, plus the time to actually move the bytes. Bytes are not the
constraint; requests are.

That band is wider than the previous run's 1.02–1.22×, and the reason is the
endpoint rather than the code: its per-request tail ran to 758 ms this session, so
a 14-request chain that catches two slow requests gains a second on its own. It is
wide enough that the previous run's neat story — Zeiss as the lone outlier at
1.42×, because it is the one slide whose open moves enough bytes (6.60 MB) for
transfer to be a visible share — no longer separates. Zeiss sits at 1.37× here and
JP2K-33003-1 at 1.40× on a 1.05 MB open, which the byte explanation cannot
account for. **Treat the ratio as 1.1–1.4× with the spread being endpoint noise,
and do not read a slide's position within it.**

**The two DICOM slides are the real exception, and they show what concurrency
buys.** DICOM/Hamamatsu-2 issues 19 requests, which at 401 ms serial would be
7.6 s, and opens in 3.0 s — the fastest open in the suite despite issuing more
requests than eleven of the other twelve slides. DICOM/Leica-4's 27 requests
predict 10.8 s and measure 7.6 s. A DICOM slide is a directory of instances that can
be parsed independently, so its open overlaps requests instead of chaining them.
Every other format's open is a dependency chain: a header says where the next
header is.

**Hamamatsu no longer scans for restart markers**, which is what removed its
18-request, 8 s cold patch two runs ago
([finding E](#e-mirax-ventana-and-ndpi-now-batch-their-tile-fetches-fixed)); at
1694 ms cold and 1445 ms scattered it now behaves like the rest of the suite. Its
scattered figure being *worse* than its adjacent one is the honest number: the
previous 20 ms reading was scattered windows free-riding on a scan the cold patch
had already paid for.

**One row still shows caching rather than format cost.** DICOM/Leica-4's 11 ms
adjacent patch is its memoized fragment walk being reused, after a cold patch paid
5.8 s for it — and that cold patch remains the one standing regression
([finding G](#g-dicomleica-4s-cold-patch-got-slower)).

**Zeiss's patch latency is the noisiest number in the report and should not be read
as a change.** It measured 4820 ms adjacent here against 4595 ms before, but its
request count (2.0) and its bytes per patch (36.27 MB) are unchanged to the
decimal, and an earlier repeat of that slide alone gave 6004 ms adjacent with a
*faster* cold patch. A row that moves ±40% between runs at a fixed request count
and a fixed byte count is measuring the endpoint's throughput, not slidekit.

**Local opens are 5.9–147.6 ms and local patches 5.8–50 ms, against 64–540 ms for
OpenSlide's whole process** — but that gap is not a decode-speed claim.
`slidetool` re-opens the slide and PNG-encodes the region on every invocation, and
slidekit does neither. A like-for-like decode comparison needs in-process FFI,
which does not exist yet. Mirax's 147.6 ms local open is the one row that moved
materially without a code change behind it; it was 33.9 ms last run at the same
request count, and a cold page cache on its multi-file layout is the likely cause.

## 3. API calls — the portable result

| vendor | open reqs | open MB | adjacent req/patch | adjacent MB/patch | scattered req/patch | scattered MB/patch | slide MB | fetched % |
|---|---|---|---|---|---|---|---|---|
| Aperio/CMU-1 | 18 | 1.33 | 2.0 | 2.41 | 2.6 | 2.29 | 169.3 | 14.7% |
| Aperio/JP2K-33003-1 | 14 | 1.05 | 2.8 | 1.75 | 1.0 | 3.86 | 60.9 | 33.0% |
| Aperio/CMU-1-JP2K-33005 | 16 | 1.19 | 1.0 | 3.16 | 1.0 | 3.43 | 126.4 | 27.4% |
| Hamamatsu | 14 | 1.49 | 1.0 | 2.25 | 2.0 | 3.97 | 188.9 | 18.2% |
| Leica | 14 | 2.33 | 2.5 | 0.40 | 3.0 | 0.65 | 278.3 | 2.8% |
| Philips-TIFF | 18 | 1.24 | 1.0 | 0.51 | 1.0 | 0.64 | 277.5 | 2.5% |
| Generic-TIFF | 12 | 0.70 | 1.5 | 3.21 | 2.6 | 2.29 | 194.7 | 15.0% |
| Trestle | **10** | 0.62 | 1.0 | 0.55 | 1.0 | 0.49 | 158.9 | 3.7% |
| Mirax | **10** | 1.24 | 2.0 | 0.72 | 1.8 | 0.74 | 538.9 | 1.7% |
| Zeiss | **10** | **6.60** | 2.0 | **36.27** | 1.3 | **24.18** | 1004.9 | 25.2% |
| Ventana | 18 | 2.99 | 1.3 | 1.23 | 2.2 | 1.96 | 2591.9 | **0.7%** |
| DICOM/Leica-4 | **27** | 6.25 | 0.2 | 0.01 | — | — | 107.5 | 28.2% |
| DICOM/Hamamatsu-2 | 19 | 1.32 | 2.0 | 0.29 | 2.2 | 0.36 | 197.8 | 2.4% |

**Totals: 13 slides, 200 requests to open all of them; 1.6 requests per adjacent
patch and 1.8 per scattered patch.**

**Opening a slide costs 10–27 requests and 0.6–6.6 MB**, on files from 61 MB to
2.6 GB, with no vendor outside that band — every open request and byte count in this
table is unchanged from the previous run, as expected from a change that touched
only tile decoding. Ventana is the clearest demonstration: 18 requests and 3.0 MB
— 0.1% of the file — to open a **2.6 GB** slide.

The only cells that moved at all are the two JP2K slides' patch bytes and their
`fetched %` (33003 39.9→33.0%, 33005 28.8→27.4%). That is not a decoder effect —
decoding cannot change what was fetched — but the tissue screen choosing slightly
different windows, one of which cost 33003 a scattered patch.

The two most expensive opens are expensive in *bytes*, not requests, and for
opposite reasons. Zeiss pulls 6.60 MB in 10 requests because a CZI's subblock
directory is genuinely that large — one entry per subblock for the whole mosaic —
and it is now read as a few big sequential ranges instead of being chased subblock
by subblock ([finding B](#b-zeiss-open-no-longer-costs-151-requests-fixed)).
DICOM/Leica-4 pulls 6.25 MB in 27 requests because a DICOM slide is a directory of
instances that must each be parsed, so the cost scales with instance count rather
than with slide size — it pays that on a 107 MB slide, the second smallest in the
set, while a 2.6 GB Ventana opens for 3.0 MB.

**A patch costs 1–3 requests on all thirteen slides**, at both localities, and that
is the headline answer to "how many API calls under the hood". It is close to the
floor: a 1024×1024 level-0 patch spans 16–25 tiles of 256², so two requests means
the tile ranges are being coalesced rather than fetched one at a time. This is the
first run of this report with no outlier above the band — the widest rows are now
Aperio/JP2K-33003-1 at 2.8 adjacent and Leica at 3.0 scattered, both of which were
already there. MIRAX (4.2/6.4) and Ventana (2.7/3.6) came into the band in
[`33a209e`](#e-mirax-ventana-and-ndpi-now-batch-their-tile-fetches-fixed).

**Locality barely matters**: 1.6 requests per adjacent patch against 1.8 scattered,
across the set. That is worth stating because it locates where the saving comes
from — range coalescing *within* one region read, not reuse *between* reads. A
tiling job that never revisits a region pays essentially the same as a viewer
panning across one. That claim used to rest on the gap being small; it now rests on
something stronger, because this run wired up a tile cache and measured the
difference directly
([finding F](#f-the-tile-cache-is-wired-and-barely-changes-request-counts)).

### B. Zeiss open no longer costs 151 requests (fixed)

This was the one outstanding outlier two runs ago, and [`a52ed2f`](../PROGRESS.md)
closed it. The table below is that before/after, kept as recorded — it is not a
re-measurement:

| | before | after |
|---|---|---|
| open requests | 151 | **10** |
| open bytes | 9.59 MB | **6.60 MB** |
| open time | 11.6 s | **4.9 s** |
| adjacent patch | 6337 ms | **3129 ms** |
| adjacent requests/patch | 2.8 | **2.0** |

Every one of those "after" figures still holds at [`33a209e`](../PROGRESS.md) except
the adjacent patch time, which this run measured at 4595 ms and a repeat run at
6004 ms, at an unchanged 2.0 requests and 36 MB per patch. Zeiss's per-patch seconds
are throughput noise on this endpoint and nothing should be read into them.

The CZI directory says where each subblock *is* but not where its pixels start, so
resolving a subblock's pixel offset takes a 16-byte read of its own payload header.
Open was doing that for all 134 subblocks — 134 tiny GETs scattered megabytes apart
through a 1 GB file with nothing to coalesce, more requests than the other twelve
slides combined. Nothing about opening a slide needs them: the directory alone
gives every level, extent and tile placement.

They are now resolved in `read_region`, for the handful of subblocks a window
actually touches. The obvious risk in that move is trading one open-time cost for a
per-patch one — a header read followed by a data read for every tile. It does not
happen, because each range in the batched gather starts at the payload header and
runs long enough to carry the pixels behind it (fixed region, plus slack for the
variable-length metadata block, plus the data size where the directory determines
it). Falling short costs one extra request and is still correct. So the patch got
*cheaper*, not dearer: 2.0 requests per adjacent patch against 2.8 before.

Two things did not improve, and both are inherent rather than deferred. Open still
moves 6.60 MB — the largest in the suite in bytes — because the subblock directory
of a full mosaic is genuinely that big, and it is now read as a few large
sequential ranges rather than chased entry by entry. And a patch still moves 24–36
MB ([finding C](#c-bytes-per-patch-are-read-ahead-blocks-not-tiles)), because CZI
subblocks are large and uncompressed.

### E. MIRAX, Ventana and NDPI now batch their tile fetches (fixed)

The tiled-TIFF and DICOM read paths already resolved every tile's byte range first
and issued one `read_ranges`, which the object-store backend coalesces into a
handful of concurrent requests. Three paths did not: MIRAX, Ventana's tilemap and
NDPI fetched one tile per positioned read. The previous run of this report measured
them as the three worst rows in the suite, which is what prompted the fix.

| | before | after |
|---|---|---|
| Mirax adjacent req/patch | 4.2 | **2.0** |
| Mirax scattered req/patch | 6.4 | **1.8** |
| Mirax adjacent patch | 2199 ms | **468 ms** |
| Mirax scattered patch | 3194 ms | **517 ms** |
| Ventana adjacent req/patch | 2.7 | **1.3** |
| Ventana scattered req/patch | 3.6 | **2.2** |
| Ventana cold patch | 2168 ms | **651 ms** |
| Hamamatsu adjacent req/patch | 3.0 | **1.0** |
| Hamamatsu cold patch | 7979 ms | **1248 ms** |

The seconds in that table flatter the fix, because this run's endpoint was 12%
faster per request; the request counts do not, and they moved by 2–3.5×. Mirax's
scattered patch is the clearest single result in this run — **6.4 requests to 1.8**,
on the slide the previous report singled out as "the outlier … because its subtiles
are small and scattered through the file". They still are. They are now asked for
together.

Hamamatsu is the one to read carefully, because two of its numbers got *worse* and
the slide got much better. Its NDPI level 0 is a single JPEG strip, and the old cold
patch spent 8.0 s and 18 requests scanning restart markers to find tile boundaries;
the path now confirms every marker it needs in one gather. Bytes per patch rose
(adjacent 1.09 → 2.25 MB, scattered 0.40 → 3.97 MB) and `fetched%` tripled to 18.2%,
because the scattered patches used to ride on data the scan had already pulled
rather than fetching their own. Trading 3.5 MB per patch for 6.7 s of cold latency
is the right trade at 352 ms a request, and would still be right at 20 ms.

### F. The tile cache is wired, and barely changes request counts

`Cache` was dead code with a public API attached — `get`/`insert` had zero call
sites, so every `read_region` re-fetched and re-decoded tiles it had decoded moments
earlier. [`33a209e`](../PROGRESS.md) wired it into all six read paths, with a 32 MB
default budget, which means this run measured it live for the first time.

It is worth reporting that it changed almost nothing, because the expectation going
in was that it would. Isolating the effect is possible because the batching in
[finding E](#e-mirax-ventana-and-ndpi-now-batch-their-tile-fetches-fixed) touched
only three paths: on the other ten slides, the cache is the *only* thing that
changed. Nine of the ten report request counts identical to the previous run at both
localities — Aperio/JP2K-33003-1 2.8/1.0, Aperio/CMU-1-JP2K-33005 1.0/1.0, Leica
2.5/3.0, Philips-TIFF 1.0/1.0, Generic-TIFF 1.5/2.6, Trestle 1.0/1.0, Zeiss 2.0/1.3,
DICOM/Leica-4 0.2/—, DICOM/Hamamatsu-2 2.0/2.2. The tenth, Aperio/CMU-1, moved from
2.2 to 2.0 adjacent. **That 0.2 is the entire measured effect of the tile cache on
request count across the suite.**

The reason is geometry, and it is the same reason locality barely matters at all.
The adjacent windows in this bench *abut* rather than overlap — they are offset by
exactly one patch width — so consecutive patches share only the single row or
column of tiles that straddles their shared edge, about 5 of 25. Coalescing had
already merged those into the same range as their neighbours, so removing them from
the fetch does not remove a request. Bytes tell the same story from the other side:
Aperio/JP2K-33003-1's adjacent MB/patch fell 2.00 → 1.75 and Zeiss's 39.89 → 36.27,
a real saving in transfer with no saving in round trips. Tracing the large GETs on
JP2K-33003-1 shows exactly one range shrinking, from 4.06 MB to 3.07 MB.

None of that makes the cache useless — it eliminates re-*decode*, which the local
columns are where you would look for, and this report does not isolate. But **the
claim it was wired to support, that reuse between reads would cut remote requests,
is not supported by this measurement.** A viewer that revisits a region at the same
level would benefit; the two localities this bench measures never revisit one.

### C. Bytes per patch are read-ahead blocks, not tiles

The byte columns are the uncomfortable part of this table, and they are a
consequence of the trade the read-ahead layer makes. Tracing requests ≥512 KiB on
JP2K-33003-1 (`SLIDEKIT_BENCH_TRACE=512`) shows each patch is served by exactly one
large GET:

```
GET 39814949..44334058 (4.31 MB)
GET 39927237..43263293 (3.18 MB)
GET 35649138..38871302 (3.07 MB)
GET 32139954..36322269 (3.99 MB)
GET 25008995..28917577 (3.73 MB)
GET  7417643..11866112 (4.24 MB)
```

Those are the read-ahead block at or near its 4 MiB ceiling. That slide's level 0
holds a few hundred KB per tile, so the tiles behind a 1024×1024 patch come to well
under 1 MB: **over 4× more bytes cross the wire than the patch needs**. The block
doubles on sequential access, and a multi-tile region read looks sequential.

At *this* endpoint the trade is about break-even: 4 MB costs ~110–330 ms at the
12–36 MB/s observed in an earlier run (throughput was not re-measured this time),
against ~401 ms for each request saved. On a low-latency endpoint it is clearly the
wrong side — a same-region S3 at ~20 ms per request would rather issue four small
GETs than move 4 MB. **The block growth should be bounded by what the read actually
asked for**, or exposed as a knob, before this is pointed at low-latency
infrastructure.

Nothing in `33a209e` touched the read-ahead layer, and five of the six traced GETs
above are byte-identical to the previous run's, so this finding stands. The
per-patch byte columns are no longer unchanged, though: the tile cache trimmed the
third range from 4.06 MB to 3.07 MB, and shaved a few percent off the adjacent
MB/patch on several slides
([finding F](#f-the-tile-cache-is-wired-and-barely-changes-request-counts)). That is
a saving of the wrong magnitude to matter here — the block is still at its 4 MiB
ceiling, and still over 4× what the patch asked for.

Zeiss's 24–36 MB per patch is a different and mostly inherent cost: its subblocks
are large and uncompressed, so a 1024×1024 window genuinely straddles tens of MB
of pixel data. `fetched %` for Zeiss, the JP2K Aperios and DICOM/Leica-4 is
correspondingly high — but note that column now covers an open *plus up to ten
patches*, so it is not comparable with the single-patch figures in
[benchmarks.md](benchmarks.md#traffic--the-actual-result).

### G. DICOM/Leica-4's cold patch got slower

One number went the wrong way two runs ago, and it still has not come back:

| | two runs ago | previous run | repeat | this run |
|---|---|---|---|---|
| cold patch | 3945 ms | **7345 ms** | 7010 ms | **5762 ms** |
| open requests | 27 | 27 | 27 | 27 |
| cold-patch requests | — | 6 | 6 | 6 |
| adjacent patch | 15 ms | 10 ms | 11 ms | 11 ms |

This run reads 5762 ms — lower than the 7345 ms that first flagged it, but on an
endpoint whose median request got *slower* (352 → 401 ms), so the underlying cost
did not improve and may have worsened. The request count has never moved. Six
requests at 401 ms is 2.4 s; the other ~3.4 s is not round trips. That points at
the fragment walk
`locate_tile` performs to find a frame inside a DICOM concatenation — the expensive
half of a cold patch on this format, and the half [`33a209e`](../PROGRESS.md) put a
cache in front of. A cache cannot help a cold patch by definition, so the plausible
readings are that the walk itself got more expensive, or that per-tile cache
insertion costs something on a format whose frames are large enough to churn a 32 MB
budget. This report does not distinguish them: it measures one slide, once per run,
with no profiler on the local path.

It is worth keeping in proportion. The cost is paid once per slide per session, the
adjacent patch that follows it is 11 ms, and DICOM/Leica-4 is the second-smallest
slide in the set. But it is a ~1.5× regression on a path that was measured before and
after, and it should be profiled rather than left in a table.

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

- Remote patches are byte-identical to local patches on all 13 slides, 119 of 119
  patches, at two localities. The remote I/O layer does not alter a pixel.
- Remote patches agree with OpenSlide 4.0.1 over windows containing real specimen:
  ten slides within 0.08 per channel, two of them exactly, and the remaining three
  explained by localised tile seams.
- The two JPEG 2000 slides are bit-identical to OpenSlide on every patch, which is
  what a decoder that *is* OpenSlide's decoder should produce
  ([finding H](#h-the-jpeg-2000-slides-are-now-bit-exact-fixed)).
- Opening a slide from object storage costs 10–27 requests and 0.6–6.6 MB, with no
  vendor outside that band, including 18 requests for a 2.6 GB Ventana.
- A 1024×1024 patch costs 1–3 requests once the slide is open — 1.6 adjacent and 1.8
  scattered across the set — on all thirteen slides at both localities.
- Remote latency is request-count × per-request cost, at this endpoint 401 ms. The
  eleven single-file formats open in a serial chain of round trips (measured open is
  1.14–1.40× count × 401 ms); the two DICOM directories overlap theirs and beat the
  serial prediction.
- Batching the MIRAX, Ventana and NDPI tile fetches cut their per-patch request
  counts by 2–3.5×, measured before and after at a fixed window set.

**Not supported:**

- **That slidekit decodes faster than OpenSlide.** `osl_proc` carries a process
  start and a PNG encode slidekit never does. Unresolved until in-process FFI.
- **Any conclusion about remote seconds on other infrastructure.** Request counts
  port; wall-clock does not.
- **That the read-ahead block sizes are right.** [Finding C](#c-bytes-per-patch-are-read-ahead-blocks-not-tiles)
  shows they cost over 4× the necessary bytes on tiled formats. They were chosen to
  minimise request counts and validated on that axis only, never swept. Transfer
  throughput was not re-measured this run, so the break-even estimate there is
  carried over.
- **That the tile cache helps remote reads.** It measurably does not, on either
  locality this bench uses ([finding F](#f-the-tile-cache-is-wired-and-barely-changes-request-counts)).
  Its value is in avoiding re-decode and in access patterns that revisit a region,
  neither of which this report measures.
- **Why DICOM/Leica-4's cold patch nearly doubled** ([finding G](#g-dicomleica-4s-cold-patch-got-slower)).
  Reproducible, unexplained, and not attributable to request count.
- **Hamamatsu's per-patch cost as a worst case.** Its scattered windows all land in
  one horizontal band, because the densest tissue on that slide sits there. The
  scattered selection enforces Chebyshev separation, which cannot demand separation
  in both axes — on a wide, short slide like Zeiss (50171×11340) there is only one
  row of windows to choose from at six window-widths. (The stronger version of this
  caveat in the previous run — that Hamamatsu's scattered patches free-rode on the
  cold patch's restart-marker scan, making its figures a lower bound — no longer
  applies: that scan is gone, and its scattered patches now fetch their own bytes at
  2.0 requests and 3.97 MB each.)
- **DICOM/Leica-4's scattered locality**, which is unmeasured: no candidate window
  outside the primary neighbourhood held 10% specimen.
- **Single-shot timings throughout.** Medians over 5 patches per locality, but no
  repetitions of the same read, no variance, no confidence intervals. This run's
  endpoint alone varied 348–758 ms per request, so remote differences smaller than
  ~400 ms — and local ones smaller than a few tens of ms — are noise. Zeiss's
  adjacent patch moved 4595 → 6004 ms between two runs of this suite at an identical
  request and byte count, which is the scale of variance to assume on the
  byte-heavy rows. Comparing this run's seconds against the previous run's is
  **not** meaningful at all on the latency table: the endpoint's own median moved
  352 → 401 ms between them, so every row is inflated by ~14% before any code is
  considered. Only the request and byte columns are comparable across runs.
- **That the ratio band in section 2 says anything about a given slide.** At
  1.14–1.40× it is wide enough to be dominated by the endpoint's request-time tail;
  the previous run's tighter 1.02–1.22× band supported a per-slide reading and this
  one does not.
- **Why Mirax's local open went 33.9 → 147.6 ms** at an unchanged request count.
  Most likely a cold page cache on its multi-file layout, not re-measured.
- **Anything about JPEG XR CZI or Sakura**, which slidekit cannot read (matching
  OpenSlide for JXR) or has no public fixture.

## Reproducing

```sh
mise run bench:setup-slidetool   # build OpenSlide 4.0.1 into .tools (once)
mise run bench:warm-cache        # download + verify fixtures (~6.8 GB, once)
mise run bench:upload-slides     # mirror the slide set into the bucket (once)

mise run bench:openslide         # local slidekit vs OpenSlide (~3 s)
mise run bench:remote            # this report (267 s, 13 slides × 10 patches)
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
| `SLIDEKIT_BENCH_ONLY=Zeiss` | restrict to matching cases (comma-separated). `bench:remote` only — `bench:openslide` has no such filter and always runs the whole set |
| `SLIDEKIT_BENCH_PATCHES=5` | patches per locality per slide |
| `SLIDEKIT_BENCH_MIN_TISSUE=10` | minimum specimen share (%) for a window to be used |
| `SLIDEKIT_BENCH_WINDOW=3568,6336` | re-read one specific window instead of searching for tissue (`bench:openslide`) — this is how a `worst_window` from the correctness table gets investigated. It applies to *every* slide, so expect `n/a` rows where those coordinates miss the specimen; reproduces the window's `mean_d`, not necessarily the slide's `max_d` |
| `SLIDEKIT_BENCH_PROFILE=1` | print *where* inside the window the readers disagree: bounding box, worst pixel, and a 24×24 heat map (`bench:openslide`) |
| `SLIDEKIT_BENCH_TRACE=512` | log every request ≥512 KiB — how finding C was confirmed |
| `SLIDEKIT_BENCH_DUMP=/tmp/patches` | write **every** patch as PNGs — slidekit's output, OpenSlide's, and an amplified difference image — plus `patches.csv` and a browsable `index.html` sorted worst-disagreement first. Roughly 5 MB per patch, so ~600 MB for the full set |
| `SLIDEKIT_BENCH_THUMB=320` | longest side of the contact-sheet thumbnails |
| `SLIDEKIT_BENCH_DIFF_GAIN=8` | multiplier on the difference image, so decoder noise stays black |
| `SLIDEKIT_BENCH_SHEET=1` | rebuild `index.html` from an existing `patches.csv` without re-reading any slide (`cargo test --release --test bench_remote -- --ignored contact_sheet`) |
| `SLIDEKIT_BENCH_BUDGET_SECS=1800` | wall-clock budget; remaining slides reported as not-run |
