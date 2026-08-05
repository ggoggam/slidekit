use std::ops::Range;
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use object_store::path::Path as StorePath;
use object_store::{ObjectStore, ObjectStoreExt};
use tokio::runtime::Runtime;
use url::Url;

use crate::error::{OpenSlideError, Result};
use crate::io::fs::{Entry, SlideFs, validate};
use crate::io::readahead::ReadAhead;
use crate::io::{AsyncSlideReader, SlideReader};

/// Shared multi-threaded Tokio runtime used to drive async I/O from
/// synchronous call sites.
///
/// A single process-wide runtime is created lazily on first use so that
/// connection pools (HTTP/TLS) are reused across slides and reads.
pub(crate) fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        Runtime::new().expect("failed to initialize Tokio runtime for object_store I/O")
    })
}

/// Environment variables that configure a cloud backend, as `(key, value)`
/// pairs ready for [`object_store::parse_url_opts`].
///
/// `object_store::parse_url` builds each backend with `Builder::new()` and
/// passes no options, so it consults **no** environment variable: an `s3://`
/// URL against a non-AWS endpoint never sees `AWS_ENDPOINT_URL`, finds no
/// credentials, and falls through to the EC2 instance metadata service before
/// failing. Passing the environment through `parse_url_opts` restores the
/// behaviour each builder's own `from_env()` has.
///
/// The filter is deliberate. `parse_url_opts` silently ignores keys it doesn't
/// recognize, but the ones it *does* recognize include bare names like `region`,
/// `bucket`, `endpoint`, and `token` — handing it the whole environment would
/// let an unrelated `TOKEN` or `ENDPOINT` variable redirect a slide read. So
/// only the prefixes that `AmazonS3Builder::from_env`,
/// `MicrosoftAzureBuilder::from_env`, and `GoogleCloudStorageBuilder::from_env`
/// look at are forwarded, plus the two exact names those builders special-case.
pub fn env_config() -> impl Iterator<Item = (String, String)> {
    backend_config(std::env::vars())
}

/// The [`env_config`] filter, over any set of variables — testable without
/// mutating the process environment.
fn backend_config(
    vars: impl Iterator<Item = (String, String)>,
) -> impl Iterator<Item = (String, String)> {
    /// Prefixes each cloud builder's `from_env()` scans for.
    const PREFIXES: &[&str] = &["AWS_", "AZURE_", "GOOGLE_"];
    /// Unprefixed names the GCP and Azure builders read directly.
    const EXACT: &[&str] = &["SERVICE_ACCOUNT", "MSI_ENDPOINT"];

    vars.filter(|(key, _)| {
        let key = key.to_ascii_uppercase();
        PREFIXES.iter().any(|p| key.starts_with(p)) || EXACT.contains(&key.as_str())
    })
}

/// Parse a URL into a store configured from [`env_config`], plus the object's
/// path within that store.
fn parse_url_from_env(url: &str) -> Result<(Arc<dyn ObjectStore>, StorePath)> {
    let parsed = Url::parse(url)
        .map_err(|e| OpenSlideError::InvalidArgument(format!("invalid URL '{url}': {e}")))?;
    let (store, path) = object_store::parse_url_opts(&parsed, env_config())?;
    Ok((Arc::from(store), path))
}

/// Adapts any [`AsyncSlideReader`] into a synchronous [`SlideReader`] by
/// driving its async methods to completion on the shared [`runtime`].
///
/// This is the bridge that lets the (synchronous) vendor/TIFF pipeline consume
/// async backends without each backend implementing both interfaces — the read
/// logic lives in the `AsyncSlideReader` impl alone.
///
/// Note: `block_on` panics if called from within an existing async context, so
/// `BlockingReader` is for synchronous call sites only. From async code, use
/// the underlying [`AsyncSlideReader`] directly.
#[derive(Clone)]
pub struct BlockingReader<R> {
    inner: R,
}

impl<R> BlockingReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Borrow the underlying async reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Consume the adapter, returning the underlying async reader.
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: AsyncSlideReader> SlideReader for BlockingReader<R> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let bytes = runtime().block_on(self.inner.read_at(offset, buf.len()))?;
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(n)
    }

    /// Hands the whole batch to the async backend, which coalesces neighbouring
    /// ranges into single requests and fetches the rest concurrently. This is why
    /// a synchronous region read costs a handful of requests rather than one per
    /// tile: the tiles of a region are usually neighbours in the file.
    fn read_ranges(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
        if ranges.is_empty() {
            return Ok(Vec::new());
        }
        runtime().block_on(self.inner.read_ranges(ranges))
    }

    fn size(&self) -> Result<u64> {
        runtime().block_on(self.inner.size())
    }
}

/// An [`AsyncSlideReader`] backed by an [`object_store::ObjectStore`].
///
/// This is the vendor-agnostic filesystem backend: the same code path serves
/// slides on local disk, Amazon S3, Google Cloud Storage, Azure Blob Storage,
/// or any HTTP server that supports range requests. Reads are issued as byte
/// range requests, so large remote slides are streamed on demand rather than
/// downloaded in full.
///
/// The read logic is implemented once, asynchronously. Synchronous callers
/// obtain a [`SlideReader`] via [`BlockingReader::new`].
///
/// Cloning is cheap (the store handle is reference counted), which lets the
/// open path build the several reader instances it needs without re-issuing a
/// HEAD request.
#[derive(Clone)]
pub struct ObjectStoreReader {
    store: Arc<dyn ObjectStore>,
    path: StorePath,
    size: u64,
}

impl ObjectStoreReader {
    /// Build a reader from a store handle and an object path, fetching the
    /// object's size up front with a HEAD request.
    pub async fn connect(store: Arc<dyn ObjectStore>, path: StorePath) -> Result<Self> {
        let meta = store.head(&path).await?;
        Ok(Self {
            store,
            path,
            size: meta.size,
        })
    }

    /// Build a reader for an object whose size is already known, skipping the
    /// HEAD request [`connect`](Self::connect) would issue.
    ///
    /// A directory listing reports the size of every object in it, so a
    /// multi-file slide can open all of its data files off one `LIST` instead
    /// of one `HEAD` each. The size must be correct: reads are clamped to it,
    /// so an undersized value silently truncates.
    pub fn with_size(store: Arc<dyn ObjectStore>, path: StorePath, size: u64) -> Self {
        Self { store, path, size }
    }

    /// Synchronous wrapper around [`connect`](Self::connect).
    pub fn new(store: Arc<dyn ObjectStore>, path: StorePath) -> Result<Self> {
        runtime().block_on(Self::connect(store, path))
    }

    /// Build a reader from a URL.
    ///
    /// Supported schemes mirror `object_store`:
    /// `file:///path/to/slide.svs`, `s3://bucket/key`, `gs://bucket/key`,
    /// `az://container/key`, and `http(s)://host/path`. Cloud credentials and
    /// region/endpoint configuration are taken from the environment — see
    /// [`env_config`].
    pub async fn open_url_async(url: &str) -> Result<Self> {
        let (store, path) = parse_url_from_env(url)?;
        Self::connect(store, path).await
    }

    /// Synchronous wrapper around [`open_url_async`](Self::open_url_async).
    pub fn open_url(url: &str) -> Result<Self> {
        runtime().block_on(Self::open_url_async(url))
    }

    /// The object path within the store.
    pub fn path(&self) -> &StorePath {
        &self.path
    }
}

impl AsyncSlideReader for ObjectStoreReader {
    async fn read_at(&self, offset: u64, len: usize) -> Result<Bytes> {
        if len == 0 || offset >= self.size {
            return Ok(Bytes::new());
        }
        // Clamp the requested range to the object size; stores reject ranges
        // that run past the end of the object.
        let end = (offset + len as u64).min(self.size);
        let bytes = self.store.get_range(&self.path, offset..end).await?;
        Ok(bytes)
    }

    async fn read_ranges(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
        if ranges.is_empty() {
            return Ok(Vec::new());
        }
        // `get_ranges` coalesces adjacent/nearby ranges into fewer object-store
        // requests and fetches them concurrently, then splits the result back
        // out in the requested order.
        let bytes = self.store.get_ranges(&self.path, ranges).await?;
        Ok(bytes)
    }

    async fn size(&self) -> Result<u64> {
        Ok(self.size)
    }
}

/// A [`SlideFs`] over an [`ObjectStore`] — the remote counterpart to
/// [`LocalFs`](crate::io::local::LocalFs).
///
/// The filesystem is rooted at the store itself, so paths are object keys
/// (`slides/CMU-1.mrxs`). This is what makes multi-file formats work on blob
/// storage: MIRAX resolves its `CMU-1/` data directory as a key prefix, and
/// DICOM enumerates a slide's instances with one `LIST`.
///
/// Every method drives async store calls to completion on the shared
/// [`runtime`], so — like [`BlockingReader`] — it must not be called from
/// inside an async context. The async open entry points on `OpenSlide` already
/// run the whole open off-runtime.
#[derive(Clone)]
pub struct ObjectStoreFs {
    store: Arc<dyn ObjectStore>,
}

impl ObjectStoreFs {
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
    }

    /// Parse a URL into a store handle and the object's path within it.
    ///
    /// Supported schemes mirror `object_store`: `file://`, `s3://`, `gs://`,
    /// `az://`, `http(s)://`. Credentials and endpoint configuration come from
    /// the environment ([`env_config`]). Returned separately so the caller can
    /// build both a reader for the object and a filesystem over its store.
    pub fn parse_url(url: &str) -> Result<(Arc<dyn ObjectStore>, StorePath)> {
        parse_url_from_env(url)
    }

    /// The underlying store handle.
    pub fn store(&self) -> &Arc<dyn ObjectStore> {
        &self.store
    }

    fn resolve(&self, path: &str) -> Result<StorePath> {
        validate(path)?;
        Ok(StorePath::from(path))
    }
}

/// Wrap a store reader in the layers every remote slide file wants: blocking
/// adaptation for the synchronous parse pipeline, then block read-ahead so a
/// parser's few-byte reads don't each become their own ranged GET.
fn buffered(reader: ObjectStoreReader) -> Result<Arc<dyn SlideReader>> {
    Ok(Arc::new(ReadAhead::new(BlockingReader::new(reader))?))
}

impl SlideFs for ObjectStoreFs {
    fn open(&self, path: &str) -> Result<Arc<dyn SlideReader>> {
        let path = self.resolve(path)?;
        let reader = ObjectStoreReader::new(self.store.clone(), path)?;
        buffered(reader)
    }

    fn open_sized(&self, path: &str, size: u64) -> Result<Arc<dyn SlideReader>> {
        let path = self.resolve(path)?;
        let reader = ObjectStoreReader::with_size(self.store.clone(), path, size);
        buffered(reader)
    }

    fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let prefix = self.resolve(dir)?;
        // `list_with_delimiter` keeps this non-recursive: a MIRAX data
        // directory beside 200 other slides costs one request for its own keys,
        // not a walk of the whole bucket.
        let listing = runtime().block_on(async {
            let prefix = (!dir.is_empty()).then_some(&prefix);
            self.store.list_with_delimiter(prefix).await
        })?;

        Ok(listing
            .objects
            .into_iter()
            .filter_map(|meta| {
                let path = meta.location.as_ref().to_string();
                // Some stores expose a zero-length marker object for the
                // prefix itself; it is not a file in the slide.
                if path == dir {
                    return None;
                }
                Some(Entry {
                    path,
                    size: meta.size,
                })
            })
            .collect())
    }

    fn read(&self, path: &str) -> Result<Vec<u8>> {
        let path = self.resolve(path)?;
        // One whole-object GET. The default impl would cost a size lookup plus
        // a range read; these files (Slidedat.ini, Index.dat) are small enough
        // that the extra round trip dominates.
        let bytes = runtime().block_on(async {
            let result = self.store.get(&path).await?;
            result.bytes().await
        })?;
        Ok(bytes.to_vec())
    }

    fn exists(&self, path: &str) -> Result<bool> {
        let path = self.resolve(path)?;
        match runtime().block_on(self.store.head(&path)) {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::PutPayload;
    use object_store::memory::InMemory;

    fn store_with(data: &[u8]) -> ObjectStoreReader {
        let store = InMemory::new();
        let path = StorePath::from("slide.svs");
        runtime()
            .block_on(store.put(&path, PutPayload::from(data.to_vec())))
            .unwrap();
        ObjectStoreReader::new(Arc::new(store), path).unwrap()
    }

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        backend_config(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
        )
        .collect()
    }

    #[test]
    fn env_config_forwards_only_backend_variables() {
        let kept = vars(&[
            ("AWS_ENDPOINT_URL", "https://example.invalid"),
            ("AWS_ACCESS_KEY_ID", "id"),
            ("AZURE_STORAGE_ACCOUNT_NAME", "acct"),
            ("GOOGLE_SERVICE_ACCOUNT", "/key.json"),
            ("SERVICE_ACCOUNT", "/key.json"),
            ("MSI_ENDPOINT", "http://169.254.169.254/metadata"),
        ]);
        assert_eq!(kept.len(), 6, "{kept:?}");
    }

    #[test]
    fn env_config_ignores_lookalike_variables() {
        // `parse_url_opts` accepts bare `region`/`bucket`/`endpoint`/`token`
        // keys, so forwarding the whole environment would let an unrelated
        // variable redirect a slide read. None of these may survive the filter.
        let dropped = vars(&[
            ("REGION", "eu-west-1"),
            ("BUCKET", "someone-elses-bucket"),
            ("BUCKET_ENDPOINT_URL", "https://wrong.invalid"),
            ("ENDPOINT", "https://wrong.invalid"),
            ("TOKEN", "unrelated-secret"),
            ("PATH", "/usr/bin"),
        ]);
        assert!(dropped.is_empty(), "{dropped:?}");
    }

    /// The point of the whole exercise: a non-AWS endpoint in the environment
    /// must reach the builder. Before this, `parse_url` built
    /// `AmazonS3Builder::new()` with no options, so the endpoint was ignored,
    /// no credentials were found, and the open fell through to EC2 instance
    /// metadata. Asserted through the store's `Display`, which reports the
    /// bucket it was configured with — building at all is the signal, since a
    /// credential-less S3 builder errors out.
    #[test]
    fn parse_url_opts_applies_the_environment() {
        let url = Url::parse("s3://slides/CMU-1.svs").expect("url");
        let opts = [
            ("AWS_ENDPOINT_URL", "https://storage.example.invalid"),
            ("AWS_REGION", "auto"),
            ("AWS_ACCESS_KEY_ID", "id"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("PATH", "/usr/bin"),
        ];
        let (store, path) = object_store::parse_url_opts(
            &url,
            backend_config(opts.iter().map(|(k, v)| (k.to_string(), v.to_string()))),
        )
        .expect("configured store");
        assert_eq!(path.as_ref(), "CMU-1.svs");
        assert!(store.to_string().contains("slides"), "{store}");
    }

    #[test]
    fn async_reads_range_at_offset() {
        let data: Vec<u8> = (0..=255u8).collect();
        let reader = store_with(&data);

        let bytes = runtime().block_on(reader.read_at(10, 16)).unwrap();
        assert_eq!(&bytes[..], &data[10..26]);
        assert_eq!(runtime().block_on(reader.size()).unwrap(), 256);
    }

    #[test]
    fn async_read_past_end_is_clamped() {
        let data: Vec<u8> = (0..100u8).collect();
        let reader = store_with(&data);

        let bytes = runtime().block_on(reader.read_at(90, 32)).unwrap();
        assert_eq!(&bytes[..], &data[90..100]);

        assert!(
            runtime()
                .block_on(reader.read_at(100, 8))
                .unwrap()
                .is_empty()
        );
        assert!(
            runtime()
                .block_on(reader.read_at(200, 8))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn async_read_exact_at_errors_past_end() {
        let reader = store_with(&[0u8; 50]);
        // Within bounds: exact length returned.
        let bytes = runtime().block_on(reader.read_exact_at(10, 20)).unwrap();
        assert_eq!(bytes.len(), 20);
        // Past end: short read -> error.
        assert!(runtime().block_on(reader.read_exact_at(40, 20)).is_err());
    }

    #[test]
    fn async_read_ranges_returns_in_order() {
        let data: Vec<u8> = (0..=255u8).collect();
        let reader = store_with(&data);

        // Non-contiguous and adjacent ranges; results must match request order.
        let ranges = [10u64..20, 200..210, 20..30];
        let parts = runtime().block_on(reader.read_ranges(&ranges)).unwrap();

        assert_eq!(parts.len(), 3);
        assert_eq!(&parts[0][..], &data[10..20]);
        assert_eq!(&parts[1][..], &data[200..210]);
        assert_eq!(&parts[2][..], &data[20..30]);

        assert!(
            runtime()
                .block_on(reader.read_ranges(&[]))
                .unwrap()
                .is_empty()
        );
    }

    /// A store holding a MIRAX-shaped layout: `CMU-1.mrxs` beside a `CMU-1/`
    /// prefix of data files.
    fn multi_file_store() -> ObjectStoreFs {
        let store = InMemory::new();
        let put = |path: &str, data: &[u8]| {
            runtime()
                .block_on(store.put(&StorePath::from(path), PutPayload::from(data.to_vec())))
                .unwrap();
        };
        put("CMU-1.mrxs", b"mrxs-stub");
        put("CMU-1/Slidedat.ini", b"[GENERAL]\nSLIDE_ID=x\n");
        put("CMU-1/Index.dat", b"01.02");
        put("CMU-1/Data0000.dat", &[7u8; 64]);
        put("other-slide.svs", b"unrelated");
        ObjectStoreFs::new(Arc::new(store))
    }

    #[test]
    fn lists_one_prefix_without_walking_the_bucket() {
        let fs = multi_file_store();
        let mut entries = fs.list("CMU-1").unwrap();
        entries.sort_by(|a, b| a.path.cmp(&b.path));

        let names: Vec<&str> = entries.iter().map(|e| e.file_name()).collect();
        assert_eq!(names, ["Data0000.dat", "Index.dat", "Slidedat.ini"]);
        // Paths are full keys, ready to hand back to `open`/`open_sized`.
        assert_eq!(entries[0].path, "CMU-1/Data0000.dat");
        // Sizes come free with the listing — this is what removes the HEADs.
        assert_eq!(entries[0].size, 64);

        // The sibling slide at the root is not part of this prefix.
        let root: Vec<String> = fs.list("").unwrap().into_iter().map(|e| e.path).collect();
        assert!(root.contains(&"CMU-1.mrxs".to_string()));
        assert!(!root.iter().any(|p| p.starts_with("CMU-1/")));
    }

    #[test]
    fn open_sized_skips_the_head_and_still_reads() {
        let fs = multi_file_store();
        let reader = fs.open_sized("CMU-1/Data0000.dat", 64).unwrap();
        assert_eq!(reader.size().unwrap(), 64);

        let mut buf = [0u8; 8];
        reader.read_exact_at(16, &mut buf).unwrap();
        assert_eq!(buf, [7u8; 8]);
    }

    #[test]
    fn reads_whole_files_and_probes_existence() {
        let fs = multi_file_store();
        assert_eq!(fs.read("CMU-1/Index.dat").unwrap(), b"01.02");
        assert!(fs.exists("CMU-1/Slidedat.ini").unwrap());
        // A missing object is a clean `false`, not an error — detection relies
        // on being able to ask cheaply.
        assert!(!fs.exists("CMU-1/Nope.dat").unwrap());
        assert!(!fs.exists("Missing/Slidedat.ini").unwrap());
    }

    #[test]
    fn refuses_paths_that_escape_the_store() {
        let fs = multi_file_store();
        assert!(fs.open("../elsewhere/key").is_err());
        assert!(fs.read("/absolute/key").is_err());
        assert!(fs.exists("CMU-1/../../etc/passwd").is_err());
    }

    #[test]
    fn blocking_bridge_reuses_async_impl() {
        // The sync SlideReader path is derived entirely from the async impl.
        let data: Vec<u8> = (0..200u8).collect();
        let reader = BlockingReader::new(store_with(&data));

        assert_eq!(reader.size().unwrap(), 200);

        let mut buf = [0u8; 50];
        let n = reader.read_at(25, &mut buf).unwrap();
        assert_eq!(n, 50);
        assert_eq!(&buf[..], &data[25..75]);

        // Sync read_exact_at default (loops over read_at) spans the buffer.
        let mut buf2 = [0u8; 40];
        reader.read_exact_at(100, &mut buf2).unwrap();
        assert_eq!(&buf2[..], &data[100..140]);

        // Clamping at EOF.
        assert_eq!(reader.read_at(200, &mut buf).unwrap(), 0);
    }
}
