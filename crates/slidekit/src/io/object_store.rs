use std::ops::Range;
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use object_store::path::Path as StorePath;
use object_store::{ObjectStore, ObjectStoreExt};
use tokio::runtime::Runtime;
use url::Url;

use crate::error::{OpenSlideError, Result};
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

    /// Synchronous wrapper around [`connect`](Self::connect).
    pub fn new(store: Arc<dyn ObjectStore>, path: StorePath) -> Result<Self> {
        runtime().block_on(Self::connect(store, path))
    }

    /// Build a reader from a URL.
    ///
    /// Supported schemes mirror `object_store`:
    /// `file:///path/to/slide.svs`, `s3://bucket/key`, `gs://bucket/key`,
    /// `az://container/key`, and `http(s)://host/path`. Cloud credentials and
    /// region/endpoint configuration are taken from the environment.
    pub async fn open_url_async(url: &str) -> Result<Self> {
        let parsed = Url::parse(url)
            .map_err(|e| OpenSlideError::InvalidArgument(format!("invalid URL '{url}': {e}")))?;
        let (store, path) = object_store::parse_url(&parsed)?;
        Self::connect(Arc::from(store), path).await
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
