/// An associated image (thumbnail, label, macro, etc.).
#[derive(Debug, Clone)]
pub struct AssociatedImage {
    pub width: i64,
    pub height: i64,
    pub argb_data: Vec<u32>,
    pub icc_profile: Option<Vec<u8>>,
}
