//! ICC colour profiles, read on demand.
//!
//! Most formats keep a profile in a tag the driver has already parsed, a few
//! kilobytes at most, and there is nothing to gain from deferring it. DICOM is
//! not like that: Leica's instances embed a **13.1 MB** profile inside
//! `OpticalPathSequence`, and every instance of a slide carries its own copy. A
//! slide that loaded the profile at open paid 12.3 MB for the level it reads
//! from and another 12.3 MB for the thumbnail's, which was 24.6 MB of the 30.9 MB
//! a remote open of `DICOM/Leica-4` transferred — for colour data most callers
//! never ask for.
//!
//! So a profile is either bytes already in hand or a byte range to read when
//! first requested. The size is known either way, without reading anything, which
//! is what lets a caller decide whether it wants 13 MB — the same split the C API
//! makes between `openslide_get_icc_profile_size` and
//! `openslide_read_icc_profile`.

use std::sync::{Arc, OnceLock};

use crate::error::Result;
use crate::io::SlideReader;

/// A slide's or associated image's ICC profile.
///
/// [`bytes`](Self::bytes) is the accessor; it may perform I/O the first time it
/// is called, and caches the result. [`len`](Self::len) never reads.
#[derive(Clone, Default)]
pub struct IccProfile(Repr);

#[derive(Clone, Default)]
enum Repr {
    /// The slide has no profile.
    #[default]
    Absent,
    /// Bytes the driver already had — a TIFF tag value, say, parsed at open
    /// because it was cheap and already on the wire.
    Loaded(Vec<u8>),
    /// A byte range, read on first use.
    Deferred {
        reader: Arc<dyn SlideReader>,
        offset: u64,
        length: usize,
        cache: OnceLock<Vec<u8>>,
    },
}

impl IccProfile {
    /// No profile.
    pub fn absent() -> Self {
        Self(Repr::Absent)
    }

    /// A profile whose bytes are already in hand.
    pub fn loaded(bytes: Vec<u8>) -> Self {
        Self(Repr::Loaded(bytes))
    }

    /// A profile of `length` bytes at `offset` in `reader`, to be read on first
    /// use.
    ///
    /// `reader` is kept alive by this profile. For DICOM that is the instance's
    /// own reader, which the level already holds for its frames.
    pub fn deferred(reader: Arc<dyn SlideReader>, offset: u64, length: usize) -> Self {
        Self(Repr::Deferred {
            reader,
            offset,
            length,
            cache: OnceLock::new(),
        })
    }

    /// Whether the slide has a profile at all. Never reads.
    pub fn is_present(&self) -> bool {
        !matches!(self.0, Repr::Absent)
    }

    /// The profile's size in bytes, or `None` if there is no profile.
    ///
    /// Never reads — this is how a caller finds out a profile is 13 MB *before*
    /// deciding to fetch it.
    pub fn len(&self) -> Option<usize> {
        match &self.0 {
            Repr::Absent => None,
            Repr::Loaded(bytes) => Some(bytes.len()),
            Repr::Deferred { length, .. } => Some(*length),
        }
    }

    /// True when there is no profile. (Clippy asks for this next to `len`; a
    /// zero-length profile is not a thing, so it means the same as `!is_present`.)
    pub fn is_empty(&self) -> bool {
        !self.is_present()
    }

    /// The profile bytes, reading them if this is the first request.
    ///
    /// `Ok(None)` means the slide has no profile; `Err` means it has one and the
    /// read failed. A failed read is not cached, so a transient error can be
    /// retried.
    pub fn bytes(&self) -> Result<Option<&[u8]>> {
        match &self.0 {
            Repr::Absent => Ok(None),
            Repr::Loaded(bytes) => Ok(Some(bytes)),
            Repr::Deferred {
                reader,
                offset,
                length,
                cache,
            } => {
                if let Some(bytes) = cache.get() {
                    return Ok(Some(bytes));
                }
                let mut buf = vec![0u8; *length];
                reader.read_exact_at(*offset, &mut buf)?;
                // A concurrent caller may have won the race; either value is the
                // same bytes, so whichever landed first is the one to hand back.
                Ok(Some(cache.get_or_init(|| buf)))
            }
        }
    }
}

impl From<Option<Vec<u8>>> for IccProfile {
    fn from(bytes: Option<Vec<u8>>) -> Self {
        match bytes {
            Some(bytes) => Self::loaded(bytes),
            None => Self::absent(),
        }
    }
}

/// Reports the profile's state and size, never its bytes — a 13 MB hex dump in a
/// `{:?}` of a slide is not useful, and formatting one must not trigger a read.
impl std::fmt::Debug for IccProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Repr::Absent => f.write_str("IccProfile(absent)"),
            Repr::Loaded(bytes) => write!(f, "IccProfile(loaded, {} bytes)", bytes.len()),
            Repr::Deferred { length, cache, .. } => write!(
                f,
                "IccProfile({}, {length} bytes)",
                if cache.get().is_some() {
                    "read"
                } else {
                    "deferred"
                }
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Counting {
        data: Vec<u8>,
        reads: AtomicU64,
    }

    impl SlideReader for Counting {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let start = (offset as usize).min(self.data.len());
            let n = buf.len().min(self.data.len() - start);
            buf[..n].copy_from_slice(&self.data[start..start + n]);
            Ok(n)
        }

        fn size(&self) -> Result<u64> {
            Ok(self.data.len() as u64)
        }
    }

    fn counting(len: usize) -> Arc<Counting> {
        Arc::new(Counting {
            data: (0..len).map(|i| (i % 251) as u8).collect(),
            reads: AtomicU64::new(0),
        })
    }

    #[test]
    fn absent_and_loaded_profiles_need_no_reader() {
        let absent = IccProfile::absent();
        assert!(!absent.is_present());
        assert_eq!(absent.len(), None);
        assert_eq!(absent.bytes().expect("absent"), None);

        let loaded = IccProfile::loaded(vec![1, 2, 3]);
        assert!(loaded.is_present());
        assert_eq!(loaded.len(), Some(3));
        assert_eq!(loaded.bytes().expect("loaded"), Some(&[1u8, 2, 3][..]));

        // `Option<Vec<u8>>` is what the TIFF drivers already produce.
        assert!(!IccProfile::from(None).is_present());
        assert_eq!(IccProfile::from(Some(vec![7])).len(), Some(1));
    }

    /// The point of the type: the size is available without touching the file,
    /// and the bytes are read once, on request.
    #[test]
    fn a_deferred_profile_reads_once_and_only_when_asked() {
        let reader = counting(4096);
        let profile = IccProfile::deferred(reader.clone(), 100, 512);

        assert!(profile.is_present());
        assert_eq!(profile.len(), Some(512));
        assert_eq!(
            reader.reads.load(Ordering::Relaxed),
            0,
            "size and presence must not read"
        );
        assert!(format!("{profile:?}").contains("deferred"));

        let bytes = profile.bytes().expect("read").expect("present");
        assert_eq!(bytes.len(), 512);
        assert_eq!(bytes[0], 100, "the profile starts at offset 100");
        let after = reader.reads.load(Ordering::Relaxed);
        assert!(after > 0);

        // Cached: a second request is free, and reports the same bytes.
        assert_eq!(profile.bytes().expect("cached").expect("present"), bytes);
        assert_eq!(reader.reads.load(Ordering::Relaxed), after);
        assert!(format!("{profile:?}").contains("read"));
    }

    #[test]
    fn a_profile_past_the_end_of_the_file_is_an_error_not_a_short_read() {
        let profile = IccProfile::deferred(counting(64), 32, 512);
        assert_eq!(profile.len(), Some(512));
        assert!(profile.bytes().is_err());
    }
}
