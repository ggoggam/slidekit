use std::collections::BTreeMap;

/// Predefined OpenSlide property names.
pub mod names {
    pub const VENDOR: &str = "openslide.vendor";
    pub const QUICKHASH1: &str = "openslide.quickhash-1";
    pub const BACKGROUND_COLOR: &str = "openslide.background-color";
    pub const OBJECTIVE_POWER: &str = "openslide.objective-power";
    pub const MPP_X: &str = "openslide.mpp-x";
    pub const MPP_Y: &str = "openslide.mpp-y";
    pub const BOUNDS_X: &str = "openslide.bounds-x";
    pub const BOUNDS_Y: &str = "openslide.bounds-y";
    pub const BOUNDS_WIDTH: &str = "openslide.bounds-width";
    pub const BOUNDS_HEIGHT: &str = "openslide.bounds-height";
    pub const ICC_SIZE: &str = "openslide.icc-size";
}

/// Storage for slide properties (string key-value pairs).
#[derive(Debug, Clone, Default)]
pub struct Properties {
    map: BTreeMap<String, String>,
}

impl Properties {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.map.insert(key.into(), value.into());
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(|s| s.as_str())
    }

    pub fn remove(&mut self, key: &str) {
        self.map.remove(key);
    }

    pub fn names(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}
