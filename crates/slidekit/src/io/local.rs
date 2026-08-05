use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::Result;
use crate::io::SlideReader;
use crate::io::fs::{Entry, SlideFs, validate};

/// Local file reader using positioned reads (pread).
///
/// Thread-safe: `File::read_at` does not modify the file offset,
/// so multiple threads can read concurrently without locking.
pub struct LocalFileReader {
    file: File,
    size: u64,
}

impl LocalFileReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = File::open(path.as_ref())?;
        let size = file.metadata()?.len();
        Ok(Self { file, size })
    }
}

impl SlideReader for LocalFileReader {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let n = self.file.read_at(buf, offset)?;
        Ok(n)
    }

    fn size(&self) -> Result<u64> {
        Ok(self.size)
    }
}

/// A [`SlideFs`] over a directory on the local filesystem.
///
/// Paths are resolved beneath `root` and validated first, so a slide cannot
/// name its way out of its own directory.
#[derive(Debug, Clone)]
pub struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory this filesystem is rooted at.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, path: &str) -> Result<PathBuf> {
        validate(path)?;
        Ok(self.root.join(path))
    }
}

impl SlideFs for LocalFs {
    fn open(&self, path: &str) -> Result<Arc<dyn SlideReader>> {
        Ok(Arc::new(LocalFileReader::open(self.resolve(path)?)?))
    }

    fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let base = self.resolve(dir)?;
        let mut out = Vec::new();
        for entry in std::fs::read_dir(base)? {
            let entry = entry?;
            // Directories are not slide data; every caller wants files.
            if !entry.file_type()?.is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue; // Non-UTF-8 names can't be addressed by this API.
            };
            out.push(Entry {
                path: crate::io::fs::join(dir, &name),
                size: entry.metadata()?.len(),
            });
        }
        Ok(out)
    }

    fn read(&self, path: &str) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.resolve(path)?)?)
    }

    fn exists(&self, path: &str) -> Result<bool> {
        Ok(self.resolve(path)?.is_file())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that removes itself on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "slidekit-localfs-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            Self(base)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lists_files_with_sizes_and_skips_directories() {
        let tmp = TempDir::new("list");
        std::fs::write(tmp.0.join("a.dat"), b"hello").unwrap();
        std::fs::write(tmp.0.join("b.dat"), b"worldly").unwrap();
        std::fs::create_dir(tmp.0.join("sub")).unwrap();

        let fs = LocalFs::new(&tmp.0);
        let mut entries = fs.list("").unwrap();
        entries.sort_by(|a, b| a.path.cmp(&b.path));

        assert_eq!(entries.len(), 2, "the subdirectory must not be listed");
        assert_eq!(entries[0].path, "a.dat");
        assert_eq!(entries[0].size, 5);
        assert_eq!(entries[1].path, "b.dat");
        assert_eq!(entries[1].size, 7);
        assert_eq!(entries[1].file_name(), "b.dat");
    }

    #[test]
    fn lists_a_subdirectory_with_full_paths() {
        let tmp = TempDir::new("subdir");
        std::fs::create_dir(tmp.0.join("CMU-1")).unwrap();
        std::fs::write(tmp.0.join("CMU-1/Slidedat.ini"), b"[GENERAL]").unwrap();

        let fs = LocalFs::new(&tmp.0);
        let entries = fs.list("CMU-1").unwrap();
        assert_eq!(entries.len(), 1);
        // Paths are full, so they can be fed straight back to `open`.
        assert_eq!(entries[0].path, "CMU-1/Slidedat.ini");
        assert_eq!(fs.read(&entries[0].path).unwrap(), b"[GENERAL]");
    }

    #[test]
    fn reads_and_probes_existence() {
        let tmp = TempDir::new("read");
        std::fs::write(tmp.0.join("x.dat"), b"0123456789").unwrap();
        let fs = LocalFs::new(&tmp.0);

        assert_eq!(fs.read("x.dat").unwrap(), b"0123456789");
        assert!(fs.exists("x.dat").unwrap());
        assert!(!fs.exists("nope.dat").unwrap());
        // A directory is not a file.
        assert!(!fs.exists("").unwrap());

        let reader = fs.open("x.dat").unwrap();
        assert_eq!(reader.size().unwrap(), 10);
        let mut buf = [0u8; 4];
        reader.read_exact_at(3, &mut buf).unwrap();
        assert_eq!(&buf, b"3456");
    }

    #[test]
    fn refuses_to_escape_the_root() {
        let tmp = TempDir::new("escape");
        let fs = LocalFs::new(&tmp.0);

        // A hostile Slidedat.ini naming its way out must not be resolved.
        assert!(fs.open("../../etc/passwd").is_err());
        assert!(fs.read("../secret").is_err());
        assert!(fs.exists("/etc/passwd").is_err());
        assert!(fs.list("..").is_err());
    }
}
