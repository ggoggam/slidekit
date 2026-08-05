//! Filesystem-like namespace abstraction for multi-file slide formats.
//!
//! Most vendor formats are a single file, and [`SlideReader`] — a positioned
//! byte source — is all they need. MIRAX and DICOM are not: a `.mrxs` file is
//! useless without the `Foo/` directory beside it, and a DICOM slide *is* a
//! directory of instances that share a SeriesInstanceUID. Those drivers need to
//! resolve and enumerate files, not just read one.
//!
//! [`SlideFs`] is that layer, shaped after Python's `fsspec`: a flat namespace
//! addressed by `/`-separated relative paths, with `open`, `read`, `list`, and
//! `exists`. Two backends ship in-tree — [`LocalFs`](crate::io::local::LocalFs)
//! and
//! [`ObjectStoreFs`](crate::io::object_store::ObjectStoreFs) (S3, GCS, Azure,
//! HTTP) — and the trait is public so callers can plug in their own storage.
//!
//! The filesystem is consulted only while *opening* a slide. Once open, tiles
//! are read through the [`SlideReader`]s the driver already holds, so this
//! abstraction costs nothing per region read.

use std::sync::Arc;

use crate::error::{OpenSlideError, Result};
use crate::io::SlideReader;

/// One entry returned by [`SlideFs::list`].
///
/// Carrying the size here is deliberate: a single directory listing gives the
/// size of every file in it, which lets [`SlideFs::open_sized`] skip the
/// per-file metadata lookup that [`SlideFs::open`] would otherwise need. On
/// blob storage that turns N `HEAD` round trips into one `LIST`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Full path within the filesystem, not just the final component.
    pub path: String,
    /// Size of the file in bytes.
    pub size: u64,
}

impl Entry {
    /// The final path component, e.g. `Data0000.dat`.
    pub fn file_name(&self) -> &str {
        file_name(&self.path)
    }
}

/// A filesystem-like namespace that a slide can be read from.
///
/// Paths are `/`-separated and relative to the filesystem's root; they never
/// start with `/` and never contain `.` or `..` components. Implementations
/// must reject anything else rather than resolving it, because multi-file
/// formats take path fragments from *inside slide files* (MIRAX reads its data
/// file names out of `Slidedat.ini`) and those are untrusted input.
///
/// Only [`open`](Self::open) and [`list`](Self::list) are required; the rest
/// have working defaults built on them. Overriding the defaults is worthwhile
/// for network-backed stores — see each method's note.
pub trait SlideFs: Send + Sync {
    /// Open `path` as a positioned byte source.
    fn open(&self, path: &str) -> Result<Arc<dyn SlideReader>>;

    /// Entries for the files directly under `dir`, non-recursive.
    ///
    /// Subdirectories are omitted — every caller here wants files. `dir` is
    /// `""` for the filesystem root. Ordering is unspecified; callers that
    /// need determinism sort the result.
    fn list(&self, dir: &str) -> Result<Vec<Entry>>;

    /// Open `path` when its size is already known, e.g. from [`list`](Self::list).
    ///
    /// The default ignores the hint and calls [`open`](Self::open). Backends
    /// that would otherwise issue a metadata request to learn the size should
    /// override this — it is the difference between one `LIST` and N `HEAD`s
    /// when opening a slide with many data files.
    fn open_sized(&self, path: &str, size: u64) -> Result<Arc<dyn SlideReader>> {
        let _ = size;
        self.open(path)
    }

    /// Read a whole file into memory.
    ///
    /// Intended for the small metadata files multi-file formats are described
    /// by (`Slidedat.ini`, `Index.dat`) — not for image data. The default opens
    /// the file and reads it; backends should override with a single whole
    /// object fetch, which saves a round trip over the size lookup + range read
    /// the default performs.
    fn read(&self, path: &str) -> Result<Vec<u8>> {
        let reader = self.open(path)?;
        let size = reader.size()?;
        let mut buf = vec![0u8; size as usize];
        reader.read_exact_at(0, &mut buf)?;
        Ok(buf)
    }

    /// Whether `path` names a readable file.
    ///
    /// The default probes with [`open`](Self::open), which is wasteful on
    /// stores where opening fetches metadata; override with an existence check.
    fn exists(&self, path: &str) -> Result<bool> {
        Ok(self.open(path).is_ok())
    }
}

/// Where a slide lives: the filesystem it is in, plus the path of its primary
/// file within that filesystem.
///
/// This is what replaces "the local path of the file we opened". Single-file
/// drivers ignore it; MIRAX and DICOM use it to reach the rest of the slide.
#[derive(Clone)]
pub struct SlideLocation {
    fs: Arc<dyn SlideFs>,
    path: String,
}

impl SlideLocation {
    pub fn new(fs: Arc<dyn SlideFs>, path: impl Into<String>) -> Self {
        Self {
            fs,
            path: path.into(),
        }
    }

    /// The filesystem this slide lives in.
    pub fn fs(&self) -> &Arc<dyn SlideFs> {
        &self.fs
    }

    /// Path of the primary file within [`fs`](Self::fs).
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Final component of the path, e.g. `CMU-1.mrxs`.
    pub fn file_name(&self) -> &str {
        file_name(&self.path)
    }

    /// Lowercase extension of the primary file, without the dot.
    pub fn extension(&self) -> Option<String> {
        let name = self.file_name();
        let (_, ext) = name.rsplit_once('.')?;
        if ext.is_empty() {
            return None;
        }
        Some(ext.to_ascii_lowercase())
    }

    /// The directory containing the primary file (`""` at the root).
    pub fn parent(&self) -> &str {
        parent(&self.path)
    }

    /// Path of `relative` resolved against this slide's directory.
    pub fn sibling(&self, relative: &str) -> String {
        join(self.parent(), relative)
    }
}

impl std::fmt::Debug for SlideLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlideLocation")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// A [`SlideReader`] tagged with the [`SlideLocation`] it was opened from.
///
/// Only the slide's *primary* reader carries a location. Readers the drivers
/// open for sibling files deliberately do not, so nothing can re-derive a
/// slide's identity from one of its data files.
pub struct Located {
    inner: Box<dyn SlideReader>,
    location: SlideLocation,
}

impl Located {
    pub fn new(inner: Box<dyn SlideReader>, location: SlideLocation) -> Self {
        Self { inner, location }
    }
}

impl SlideReader for Located {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.inner.read_at(offset, buf)
    }

    fn size(&self) -> Result<u64> {
        self.inner.size()
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_exact_at(offset, buf)
    }

    fn location(&self) -> Option<&SlideLocation> {
        Some(&self.location)
    }
}

/// Adapts a shared [`Arc<dyn SlideReader>`] into an owned one.
///
/// [`SlideFs::open`] hands back an `Arc` because multi-file drivers share a
/// reader across levels, but the open pipeline wants an owned `Box`.
pub struct Shared(Arc<dyn SlideReader>);

impl Shared {
    pub fn new(inner: Arc<dyn SlideReader>) -> Self {
        Self(inner)
    }
}

impl SlideReader for Shared {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.0.read_at(offset, buf)
    }

    fn size(&self) -> Result<u64> {
        self.0.size()
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.0.read_exact_at(offset, buf)
    }
}

// ─────────────────────────── path helpers ──────────────────────────────────

/// The directory part of `path`, or `""` if it has none.
pub fn parent(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// The final component of `path`.
pub fn file_name(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((_, name)) => name,
        None => path,
    }
}

/// Join `dir` and `relative` with a single `/`, tolerating an empty `dir`.
pub fn join(dir: &str, relative: &str) -> String {
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() {
        return relative.trim_start_matches('/').to_string();
    }
    format!("{dir}/{}", relative.trim_start_matches('/'))
}

/// Reject paths that could escape the filesystem root or are otherwise not the
/// plain relative form [`SlideFs`] specifies.
///
/// This is a real check, not a formality: MIRAX takes its data file names from
/// `Slidedat.ini`, so a hostile slide could otherwise name `../../etc/passwd`
/// and have the driver dutifully open it.
pub fn validate(path: &str) -> Result<()> {
    if path.starts_with('/') {
        return Err(OpenSlideError::InvalidArgument(format!(
            "slide filesystem path must be relative: {path:?}"
        )));
    }
    if path.split('/').any(|c| c == ".." || c == ".") {
        return Err(OpenSlideError::InvalidArgument(format!(
            "slide filesystem path must not contain '.' or '..': {path:?}"
        )));
    }
    if path.contains('\\') || path.contains('\0') {
        return Err(OpenSlideError::InvalidArgument(format!(
            "invalid character in slide filesystem path: {path:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_paths() {
        assert_eq!(parent("a/b/c.svs"), "a/b");
        assert_eq!(parent("c.svs"), "");
        assert_eq!(file_name("a/b/c.svs"), "c.svs");
        assert_eq!(file_name("c.svs"), "c.svs");
    }

    #[test]
    fn joins_paths() {
        assert_eq!(join("a/b", "c.dat"), "a/b/c.dat");
        assert_eq!(join("", "c.dat"), "c.dat");
        assert_eq!(join("a/b/", "/c.dat"), "a/b/c.dat");
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        assert!(validate("CMU-1/Data0000.dat").is_ok());
        assert!(validate("/etc/passwd").is_err());
        assert!(validate("../../etc/passwd").is_err());
        assert!(validate("CMU-1/../../etc/passwd").is_err());
        assert!(validate("./x").is_err());
        assert!(validate("a\0b").is_err());
    }

    #[test]
    fn location_derives_siblings() {
        struct Dummy;
        impl SlideFs for Dummy {
            fn open(&self, _: &str) -> Result<Arc<dyn SlideReader>> {
                Err(OpenSlideError::Other("dummy".into()))
            }
            fn list(&self, _: &str) -> Result<Vec<Entry>> {
                Ok(Vec::new())
            }
        }

        let loc = SlideLocation::new(Arc::new(Dummy), "slides/CMU-1.mrxs");
        assert_eq!(loc.file_name(), "CMU-1.mrxs");
        assert_eq!(loc.extension().as_deref(), Some("mrxs"));
        assert_eq!(loc.parent(), "slides");
        assert_eq!(
            loc.sibling("CMU-1/Slidedat.ini"),
            "slides/CMU-1/Slidedat.ini"
        );

        // A file at the root has no parent, and siblings resolve bare.
        let root = SlideLocation::new(Arc::new(Dummy), "CMU-1.mrxs");
        assert_eq!(root.parent(), "");
        assert_eq!(root.sibling("CMU-1/Index.dat"), "CMU-1/Index.dat");
    }
}
