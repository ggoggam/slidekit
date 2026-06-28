/// Pack ARGB components into a single u32 pixel value.
#[inline]
pub fn argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
    (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | (b as u32)
}

/// Extract alpha from an ARGB pixel.
#[inline]
pub fn alpha(pixel: u32) -> u8 {
    (pixel >> 24) as u8
}

/// Extract red from an ARGB pixel.
#[inline]
pub fn red(pixel: u32) -> u8 {
    (pixel >> 16) as u8
}

/// Extract green from an ARGB pixel.
#[inline]
pub fn green(pixel: u32) -> u8 {
    (pixel >> 8) as u8
}

/// Extract blue from an ARGB pixel.
#[inline]
pub fn blue(pixel: u32) -> u8 {
    pixel as u8
}
