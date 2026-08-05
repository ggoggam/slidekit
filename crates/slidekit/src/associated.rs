use crate::icc::IccProfile;

/// An associated image (thumbnail, label, macro, etc.).
#[derive(Debug, Clone)]
pub struct AssociatedImage {
    pub width: i64,
    pub height: i64,
    pub argb_data: Vec<u32>,
    /// This image's own colour profile, which may not have been read yet — see
    /// [`IccProfile`].
    pub icc_profile: IccProfile,
}
