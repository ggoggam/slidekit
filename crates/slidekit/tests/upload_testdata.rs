//! Mirror the bench slide set into an S3-compatible bucket — NOT run by default.
//!
//! The remote bench (`bench_remote.rs`) reads slides over byte-range requests
//! rather than downloading them, but something has to put them there first.
//! This utility uploads every slide in [`common::BENCH_CASES`] from the warm
//! local cache to the bucket configured in `slidekit/.env`, under one key
//! prefix (`slidekit-bench/testdata/` by default) so it never touches unrelated
//! objects in a shared bucket.
//!
//! It is `#[ignore]`d — it moves several GB — and follows `test_download.rs` in
//! being a manual data-management utility shaped as a test, since the workspace
//! has no binary crate yet.
//!
//! ```sh
//! mise run bench:upload-slides
//! # or directly:
//! cargo test --release --test upload_testdata -- --ignored --nocapture
//! ```
//!
//! Re-runs are cheap: an object already present at the same size is skipped, so
//! this is safe to re-run after adding one vendor. Nothing is ever deleted.
//!
//! The ZIP-packaged vendors (Trestle, MIRAX, DICOM) are uploaded **extracted**,
//! not as archives — MIRAX needs `Slidedat.ini` and its `Data####.dat` blobs as
//! sibling keys, and a DICOM slide is a directory of instances the driver
//! enumerates with one `LIST`. That is exactly what makes them openable
//! remotely.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use object_store::path::Path as StorePath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart};
use tokio::io::AsyncReadExt;

/// Files at or above this size are uploaded multipart; smaller ones as one PUT.
const MULTIPART_THRESHOLD: u64 = 32 * 1024 * 1024;

/// Multipart chunk size. 16 MiB keeps the 2.7 GB Ventana slide to ~170 parts
/// (S3 caps a multipart upload at 10 000) without holding much in memory.
const CHUNK: usize = 16 * 1024 * 1024;

/// In-flight parts per file. Bounds memory at roughly CHUNK × this.
const PART_CONCURRENCY: usize = 8;

/// Files uploaded concurrently within one slide. MIRAX and DICOM are made of
/// many modest files, where per-request latency dominates throughput.
const FILE_CONCURRENCY: usize = 8;

struct Row {
    slide: String,
    uploaded_files: usize,
    skipped_files: usize,
    uploaded_bytes: u64,
    elapsed: Duration,
    url: String,
}

#[test]
#[ignore = "uploads several GB to the configured bucket; run manually"]
fn mirror_bench_slides_to_bucket() {
    let Some(bucket) = common::remote_bucket() else {
        return; // remote_bucket() explains why
    };
    eprintln!(
        "Mirroring bench slides to s3://{}/{}/ ...",
        bucket.bucket, bucket.root
    );

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    let mut rows = Vec::new();
    let mut missing = Vec::new();
    for &(case, _level) in common::BENCH_CASES {
        let Some(slide) = common::bench_slide(case) else {
            eprintln!("SKIP: {case} not available locally");
            missing.push(case);
            continue;
        };

        eprintln!(
            "\n{}: {} file(s), {:.2} GiB on disk",
            slide.name,
            slide.files.len(),
            gib(slide.total_bytes())
        );

        let started = Instant::now();
        let (uploaded_files, skipped_files, uploaded_bytes) =
            rt.block_on(mirror_slide(&bucket, &slide));
        rows.push(Row {
            slide: slide.name.clone(),
            uploaded_files,
            skipped_files,
            uploaded_bytes,
            elapsed: started.elapsed(),
            url: bucket.url(&slide.open_key),
        });
    }

    print_table(&rows);
    if !missing.is_empty() {
        eprintln!(
            "\n{} case(s) had no local fixture and were not uploaded: {missing:?}\n\
             Warm the cache first: cargo test --test test_download -- --ignored",
            missing.len()
        );
    }
    assert!(
        !rows.is_empty(),
        "nothing uploaded — no bench slide was available locally"
    );
}

/// Upload every file of one slide, skipping objects already present at the same
/// size. Returns `(uploaded files, skipped files, uploaded bytes)`.
async fn mirror_slide(
    bucket: &common::RemoteBucket,
    slide: &common::BenchSlide,
) -> (usize, usize, u64) {
    // Decide what actually needs sending before transferring anything, so a
    // re-run costs one HEAD per file rather than a re-upload.
    let mut todo: Vec<(std::path::PathBuf, StorePath, u64)> = Vec::new();
    let mut skipped = 0usize;
    for (local, key) in &slide.files {
        let Ok(size) = local.metadata().map(|m| m.len()) else {
            eprintln!("  WARN: cannot stat {}", local.display());
            continue;
        };
        let path = bucket.key(key);
        match bucket.store.head(&path).await {
            // Same size at the same key: treat as already mirrored. These
            // fixtures are checksum-verified and immutable, so size is enough.
            Ok(meta) if meta.size == size => {
                skipped += 1;
                continue;
            }
            Ok(_) => eprintln!("  {key}: size differs, re-uploading"),
            Err(object_store::Error::NotFound { .. }) => {}
            Err(e) => eprintln!("  WARN: HEAD {key} failed ({e}); will try to upload"),
        }
        todo.push((local.clone(), path, size));
    }

    if todo.is_empty() {
        eprintln!("  already mirrored ({skipped} file(s) unchanged)");
        return (0, skipped, 0);
    }

    let total: u64 = todo.iter().map(|(_, _, s)| s).sum();
    eprintln!(
        "  uploading {} file(s), {:.2} GiB ({skipped} already present)",
        todo.len(),
        gib(total)
    );

    let mut uploaded_files = 0usize;
    let mut uploaded_bytes = 0u64;
    // Bounded fan-out: FILE_CONCURRENCY files in flight, each with its own
    // bounded part concurrency.
    for batch in todo.chunks(FILE_CONCURRENCY) {
        let mut set = tokio::task::JoinSet::new();
        for (local, path, size) in batch {
            let store = bucket.store.clone();
            let (local, path, size) = (local.clone(), path.clone(), *size);
            set.spawn(async move {
                let result = upload_file(&store, &local, &path, size).await;
                (path, size, result)
            });
        }
        while let Some(joined) = set.join_next().await {
            let (path, size, result) = joined.expect("upload task panicked");
            match result {
                Ok(()) => {
                    uploaded_files += 1;
                    uploaded_bytes += size;
                }
                Err(e) => eprintln!("  FAIL {path}: {e}"),
            }
        }
    }

    (uploaded_files, skipped, uploaded_bytes)
}

/// Upload one local file to `path`. Small files go as a single PUT; larger ones
/// stream through a multipart upload so nothing is buffered whole in memory.
async fn upload_file(
    store: &Arc<dyn ObjectStore>,
    local: &Path,
    path: &StorePath,
    size: u64,
) -> Result<(), String> {
    fn io_err(what: &'static str) -> impl Fn(std::io::Error) -> String {
        move |e| format!("{what}: {e}")
    }

    if size < MULTIPART_THRESHOLD {
        let bytes = tokio::fs::read(local).await.map_err(io_err("read"))?;
        store
            .put(path, PutPayload::from(bytes))
            .await
            .map_err(|e| format!("put: {e}"))?;
        return Ok(());
    }

    let mut file = tokio::fs::File::open(local).await.map_err(io_err("open"))?;
    let upload = store
        .put_multipart(path)
        .await
        .map_err(|e| format!("put_multipart: {e}"))?;
    let mut write = WriteMultipart::new_with_chunk_size(upload, CHUNK);

    let mut buf = vec![0u8; CHUNK];
    loop {
        // `read` on a file returns 0 only at EOF, but may return a short read;
        // `write` is happy with any length, so no need to fill the buffer.
        let n = file.read(&mut buf).await.map_err(io_err("read"))?;
        if n == 0 {
            break;
        }
        // Keep memory bounded: block once enough parts are already in flight.
        write
            .wait_for_capacity(PART_CONCURRENCY)
            .await
            .map_err(|e| format!("upload part: {e}"))?;
        write.write(&buf[..n]);
    }
    write
        .finish()
        .await
        .map_err(|e| format!("complete multipart: {e}"))?;
    Ok(())
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

fn print_table(rows: &[Row]) {
    eprintln!();
    eprintln!(
        "{:<20} {:>6} {:>8} {:>11} {:>9} {:>10}",
        "slide", "sent", "skipped", "bytes", "elapsed", "MB/s"
    );
    eprintln!("{}", "-".repeat(70));
    let mut total_bytes = 0u64;
    let mut total_time = Duration::ZERO;
    for r in rows {
        let secs = r.elapsed.as_secs_f64();
        let mbps = if secs > 0.0 {
            r.uploaded_bytes as f64 / secs / 1e6
        } else {
            0.0
        };
        eprintln!(
            "{:<20} {:>6} {:>8} {:>10.2}G {:>8.1}s {:>10.1}",
            r.slide,
            r.uploaded_files,
            r.skipped_files,
            gib(r.uploaded_bytes),
            secs,
            mbps
        );
        total_bytes += r.uploaded_bytes;
        total_time += r.elapsed;
    }
    eprintln!("{}", "-".repeat(70));
    eprintln!(
        "{:<20} {:>6} {:>8} {:>10.2}G {:>8.1}s",
        "total",
        rows.iter().map(|r| r.uploaded_files).sum::<usize>(),
        rows.iter().map(|r| r.skipped_files).sum::<usize>(),
        gib(total_bytes),
        total_time.as_secs_f64()
    );

    eprintln!("\nSlide URLs (open with OpenSlide::open_url, or open_store):");
    for r in rows {
        eprintln!("  {:<20} {}", r.slide, r.url);
    }
}
