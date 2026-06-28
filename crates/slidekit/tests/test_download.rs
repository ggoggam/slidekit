//! Manual test-data prefetch utility — NOT run by default.
//!
//! Downloads and SHA-256-verifies every slide listed in `tests/testdata.yaml`
//! into the local cache (`target/testdata/`, or `$OPENSLIDE_TEST_CACHE`), so the
//! integration tests can then run fully offline. It pulls hundreds of MB, so it
//! is `#[ignore]`d and never runs under `cargo test` / CI's `mise run test`.
//!
//! Run it on demand with:
//!
//! ```sh
//! cargo test --test test_download -- --ignored --nocapture
//! ```
//!
//! Honors `OPENSLIDE_SKIP_DOWNLOAD` (the same gate the rest of the suite uses),
//! in which case every fetch is reported as skipped.

mod common;

#[test]
#[ignore = "downloads all manifest slides (hundreds of MB); run manually"]
fn prefetch_all_test_slides() {
    let mut failed = Vec::new();

    for path in common::all_slide_paths() {
        match common::test_slide_path(&path) {
            Some(local) => eprintln!("OK    {path} -> {}", local.display()),
            None => {
                eprintln!("FAIL  {path}");
                failed.push(path);
            }
        }
    }

    assert!(
        failed.is_empty(),
        "failed to fetch {} slide(s): {failed:?}",
        failed.len()
    );
}
