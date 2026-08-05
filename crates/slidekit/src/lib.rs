pub mod associated;
pub mod cache;
pub mod color;
pub mod decode;
pub mod error;
pub mod grid;
pub mod io;
pub mod level;
pub mod properties;
pub mod slide;
pub mod vendor;

pub use cache::Cache;
pub use error::{OpenSlideError, Result};
pub use slide::OpenSlide;

// Re-export key modules for use by tests and downstream crates
pub use io::SlideReader;
pub use io::fs::{Entry, SlideFs, SlideLocation};
pub use io::local::LocalFs;
#[cfg(feature = "object-store")]
pub use io::object_store::ObjectStoreFs;
