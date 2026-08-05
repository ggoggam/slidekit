pub mod fs;
pub mod local;
#[cfg(feature = "object-store")]
pub mod object_store;

use std::future::Future;
use std::ops::Range;
use std::pin::Pin;

use bytes::Bytes;

use crate::error::Result;
use crate::io::fs::SlideLocation;

/// Abstraction over local files and remote blob storage.
///
/// All vendor drivers read slide data through this trait, enabling
/// transparent local and remote (S3, GCS, Azure, HTTP) access.
pub trait SlideReader: Send + Sync {
    /// Read bytes at the given offset into the buffer.
    /// Returns the number of bytes actually read.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize>;

    /// Total size of the underlying data source in bytes.
    fn size(&self) -> Result<u64>;

    /// Where this slide lives — the filesystem and the path within it.
    ///
    /// Only a slide's *primary* reader carries a location, and only when it was
    /// opened through an entry point that knows one. Multi-file formats (MIRAX,
    /// DICOM) use it to reach the rest of the slide; single-file drivers ignore
    /// it. See [`fs::SlideFs`].
    fn location(&self) -> Option<&SlideLocation> {
        None
    }

    /// Read exactly `buf.len()` bytes at `offset`, or return an error.
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut pos = offset;
        let mut remaining = &mut buf[..];
        while !remaining.is_empty() {
            let n = self.read_at(pos, remaining)?;
            if n == 0 {
                return Err(crate::error::OpenSlideError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "unexpected end of file",
                )));
            }
            pos += n as u64;
            remaining = &mut remaining[n..];
        }
        Ok(())
    }
}

/// Async counterpart to [`SlideReader`] for streaming/remote backends.
///
/// This is the canonical interface for blob storage I/O — a backend
/// implements its read logic here, once. Synchronous callers reuse it without
/// duplicating anything by wrapping the reader in
/// [`BlockingReader`](crate::io::object_store::BlockingReader), which drives
/// these async methods to completion on a shared runtime and exposes a
/// [`SlideReader`].
///
/// Not object-safe (uses `-> impl Future`); use it through generics or a
/// concrete type rather than `dyn AsyncSlideReader`.
pub trait AsyncSlideReader: Send + Sync {
    /// Read up to `len` bytes starting at `offset`. Fewer bytes are returned
    /// near end-of-file; an empty buffer means `offset` is at or past the end.
    fn read_at(&self, offset: u64, len: usize) -> impl Future<Output = Result<Bytes>> + Send;

    /// Total size of the underlying data source in bytes.
    fn size(&self) -> impl Future<Output = Result<u64>> + Send;

    /// Read exactly `len` bytes at `offset`, or return an error.
    fn read_exact_at(&self, offset: u64, len: usize) -> impl Future<Output = Result<Bytes>> + Send {
        async move {
            let bytes = self.read_at(offset, len).await?;
            if bytes.len() < len {
                return Err(crate::error::OpenSlideError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "unexpected end of file",
                )));
            }
            Ok(bytes)
        }
    }

    /// Read several byte ranges, returning one [`Bytes`] per range in order.
    ///
    /// Backends should override this to fetch the ranges concurrently and
    /// coalesce adjacent ones into fewer requests (e.g. via
    /// `object_store::get_ranges`). The default reads sequentially.
    fn read_ranges(
        &self,
        ranges: &[Range<u64>],
    ) -> impl Future<Output = Result<Vec<Bytes>>> + Send {
        async move {
            let mut out = Vec::with_capacity(ranges.len());
            for r in ranges {
                out.push(self.read_at(r.start, (r.end - r.start) as usize).await?);
            }
            Ok(out)
        }
    }
}

/// Object-safe, type-erased async positioned read.
///
/// [`AsyncSlideReader`] returns `impl Future` and so cannot be made into a
/// trait object. This trait boxes the returned futures so an async backend can
/// be stored behind `Arc<dyn AsyncReadAt>` — e.g. on [`OpenSlide`] for async
/// tile reads in `read_region_async`. It is blanket-implemented for every
/// [`AsyncSlideReader`], so callers never implement it directly.
///
/// [`OpenSlide`]: crate::OpenSlide
pub trait AsyncReadAt: Send + Sync {
    /// Read up to `len` bytes at `offset`; see [`AsyncSlideReader::read_at`].
    fn read_at_boxed<'a>(
        &'a self,
        offset: u64,
        len: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Bytes>> + Send + 'a>>;

    /// Read several byte ranges at once; see [`AsyncSlideReader::read_ranges`].
    fn read_ranges_boxed<'a>(
        &'a self,
        ranges: &'a [Range<u64>],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Bytes>>> + Send + 'a>>;

    /// Total size of the underlying data source in bytes.
    fn size_boxed<'a>(&'a self) -> Pin<Box<dyn Future<Output = Result<u64>> + Send + 'a>>;
}

impl<R: AsyncSlideReader> AsyncReadAt for R {
    fn read_at_boxed<'a>(
        &'a self,
        offset: u64,
        len: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Bytes>> + Send + 'a>> {
        Box::pin(self.read_at(offset, len))
    }

    fn read_ranges_boxed<'a>(
        &'a self,
        ranges: &'a [Range<u64>],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Bytes>>> + Send + 'a>> {
        Box::pin(self.read_ranges(ranges))
    }

    fn size_boxed<'a>(&'a self) -> Pin<Box<dyn Future<Output = Result<u64>> + Send + 'a>> {
        Box::pin(self.size())
    }
}
