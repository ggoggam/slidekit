# Contributing to slidekit

Thanks for helping. slidekit is a Rust port of [OpenSlide](https://openslide.org/), and the goal
is to be correct first: the same pixels, levels, properties and associated images as OpenSlide
4.0 on the same file, then faster. Rust comes through mise, and a C compiler is the only other
thing a default build needs (for the vendored OpenJPEG).

slidekit is experimental (see [Status](README.md#status)): pre-1.0, unpublished, and with no API
stability promise yet. That makes now a good time for breaking changes that bring it closer to
OpenSlide, and reports of slides it reads differently from OpenSlide are the most useful
contribution of all.

```sh
mise trust
mise install               # rust, cargo-nextest and prek, pinned in mise.lock
mise exec -- prek install  # run the hooks on every commit
mise run build
mise run test:unit         # unit tests plus everything that needs no download
mise run test              # adds the integration slides (several GiB, cached under target/testdata)
mise run fmt               # cargo fmt
mise run lint              # clippy with -D warnings
mise run pre-commit        # what CI's pre-commit job runs
```

CI runs `pre-commit` and `test` on every pull request, on GitHub-hosted runners. The integration
slides come from the OpenSlide test-data server, which is slow (about 2 MB/s), so the first full
run on a new machine takes a while. Set `OPENSLIDE_TEST_CACHE` to keep them somewhere shared, or
`OPENSLIDE_SKIP_DOWNLOAD=1` to skip them.

## Layout

| Path | What goes there |
|---|---|
| `crates/slidekit/src/slide.rs` | `OpenSlide`, the public API. It mirrors the C API, so a new method should have a C counterpart (see [docs/openslide-api.md](docs/openslide-api.md)). |
| `crates/slidekit/src/vendor/` | One driver per format: detection, levels, properties, associated images. |
| `crates/slidekit/src/decode/` | Codecs. Pure Rust, except JPEG 2000 (see [Why JPEG 2000 links C](README.md#why-jpeg-2000-links-c)). |
| `crates/slidekit/src/io/` | Local reads, `object_store` byte-range reads and the `SlideFs` trait for multi-file formats. |
| `crates/slidekit/src/grid/` | How tiles map to a region read. |
| `crates/slidekit/tests/` | Integration tests per vendor, plus the `#[ignore]`d benches. |

## Matching OpenSlide

- A format change should follow what OpenSlide's C driver does, and the code should say which
  function or behaviour it mirrors. Where slidekit deliberately differs (JPEG XL in DICOM, for
  example), say so in the code and the README's tables.
- Property names and values must match OpenSlide's exactly. They are what callers key on.
- Prefer a clear error to a guess: a file slidekit can't read correctly should fail to open, not
  return black tiles.
- Keep a default build free of new C dependencies and system libraries. A new codec should be pure
  Rust unless there's a measured reason, written down the way the JPEG 2000 one is.

## Tests and test slides

- Test against the public slides in [`crates/slidekit/tests/testdata.yaml`](crates/slidekit/tests/testdata.yaml),
  which come from the [OpenSlide test data](https://openslide.cs.cmu.edu/download/openslide-testdata/).
  Adding one means its path, SHA-256 and exact `size` in bytes. The size counts against CI's
  10 GiB budget.
- **Never commit, attach or link a clinical slide.** Whole-slide images often carry patient
  details in their label and macro images and in their properties (names, dates, accession
  numbers). If a bug only shows up on a private slide, describe the format, scanner and file
  structure, and share `property_names` with the values redacted.
- Cross-check pixels against OpenSlide where you can (`mise run bench:openslide`). Independent JPEG
  decoders differ by ±1–2 per channel, so compare with a tolerance unless both sides decode the
  same bytes the same way.
- Malformed input should come back as an `Err`, never a panic, unbounded allocation or hang.
  slidekit reads files from wherever its caller points it.

## Pull requests

- Keep each pull request to one change, with a [Conventional Commits](https://www.conventionalcommits.org)
  title such as `feat(mirax): read BMP levels` or `fix(ndpi): honour restart markers past 4 GiB`.
- Fill in the template, including the commands you ran.
- Benchmarks are welcome but optional. If you claim a speedup, include the before and after from
  `bench:openslide` or `bench:remote`.
- Never paste cloud credentials, a `.env` or a signed URL into an issue, a pull request, a test or a
  commit. The remote bench reads them from a git-ignored `.env`.

Security problems go through a private report, not an issue: see [SECURITY.md](SECURITY.md).
