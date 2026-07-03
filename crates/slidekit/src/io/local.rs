use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::io::SlideReader;

/// Local file reader using positioned reads (pread).
///
/// Thread-safe: `File::read_at` does not modify the file offset,
/// so multiple threads can read concurrently without locking.
pub struct LocalFileReader {
    file: File,
    size: u64,
    path: PathBuf,
}

impl LocalFileReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            file,
            size,
            path: path.to_path_buf(),
        })
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

    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }
}
