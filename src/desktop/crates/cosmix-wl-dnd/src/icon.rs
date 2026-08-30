//! Validated raster input for an outgoing Wayland drag icon.
//!
//! Rasterisation belongs above this crate. The transport accepts only the
//! compositor-ready pixels and geometry it needs to build one `wl_shm` buffer.

use std::fmt;

const SHM_SLOT_ALIGNMENT: usize = 64;

/// Premultiplied RGBA8 pixels for one outgoing drag icon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingIcon {
    pub(crate) pixels: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) buffer_scale: i32,
    pub(crate) offset: (i32, i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutgoingIconError {
    ZeroWidth,
    ZeroHeight,
    PixelLengthOverflow,
    InvalidPixelLength { expected: usize, actual: usize },
    WidthTooLarge(u32),
    HeightTooLarge(u32),
    InvalidBufferScale(i32),
    WidthNotMultipleOfBufferScale { width: u32, buffer_scale: i32 },
    HeightNotMultipleOfBufferScale { height: u32, buffer_scale: i32 },
    ShmPoolTooLarge { required: usize },
}

impl OutgoingIcon {
    /// Validates every value later encoded into `wl_shm` and `wl_surface`.
    ///
    /// This boundary is load-bearing: a bad buffer length, stride, dimension,
    /// or scale is a Wayland protocol error and kills the whole connection.
    pub fn new(
        pixels: Vec<u8>,
        width: u32,
        height: u32,
        buffer_scale: i32,
        offset: (i32, i32),
    ) -> Result<Self, OutgoingIconError> {
        if width == 0 {
            return Err(OutgoingIconError::ZeroWidth);
        }
        if height == 0 {
            return Err(OutgoingIconError::ZeroHeight);
        }
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(OutgoingIconError::PixelLengthOverflow)?;
        // wl_shm encodes width, height, and stride as signed 32-bit integers.
        // Constraining the width by its four-byte stride also keeps that
        // request argument representable.
        if width > (i32::MAX as u32) / 4 {
            return Err(OutgoingIconError::WidthTooLarge(width));
        }
        if height > i32::MAX as u32 {
            return Err(OutgoingIconError::HeightTooLarge(height));
        }
        if buffer_scale <= 0 {
            return Err(OutgoingIconError::InvalidBufferScale(buffer_scale));
        }
        let scale = buffer_scale as u32;
        if !width.is_multiple_of(scale) {
            return Err(OutgoingIconError::WidthNotMultipleOfBufferScale {
                width,
                buffer_scale,
            });
        }
        if !height.is_multiple_of(scale) {
            return Err(OutgoingIconError::HeightNotMultipleOfBufferScale {
                height,
                buffer_scale,
            });
        }
        let pool_len = shm_slot_len(expected).ok_or(OutgoingIconError::PixelLengthOverflow)?;
        if pool_len > i32::MAX as usize {
            return Err(OutgoingIconError::ShmPoolTooLarge { required: pool_len });
        }
        if pixels.len() != expected {
            return Err(OutgoingIconError::InvalidPixelLength {
                expected,
                actual: pixels.len(),
            });
        }
        Ok(Self {
            pixels,
            width,
            height,
            buffer_scale,
            offset,
        })
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn buffer_scale(&self) -> i32 {
        self.buffer_scale
    }

    pub fn offset(&self) -> (i32, i32) {
        self.offset
    }
}

impl fmt::Display for OutgoingIconError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for OutgoingIconError {}

/// Rounds a buffer byte length exactly as SCTK does for one SHM slot.
pub(crate) const fn shm_slot_len(byte_len: usize) -> Option<usize> {
    match byte_len.checked_add(SHM_SLOT_ALIGNMENT - 1) {
        Some(len) => Some(len & !(SHM_SLOT_ALIGNMENT - 1)),
        None => None,
    }
}

/// Converts already-premultiplied RGBA8 to the little-endian storage required
/// by Wayland's `0xAARRGGBB` `Argb8888` definition.
///
/// Kept separate from the pool upload because the byte-level rule is fully
/// testable without pretending a compositor exists.
pub(crate) fn write_little_endian_argb8888(rgba: &[u8], argb: &mut [u8]) {
    debug_assert_eq!(rgba.len(), argb.len());
    for (source, destination) in rgba.chunks_exact(4).zip(argb.chunks_exact_mut(4)) {
        let value = u32::from(source[3]) << 24
            | u32::from(source[0]) << 16
            | u32::from(source[1]) << 8
            | u32::from(source[2]);
        destination.copy_from_slice(&value.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_pixel_layout_is_accepted() {
        let pixels = vec![0; 4 * 2 * 4];
        let icon = OutgoingIcon::new(pixels.clone(), 4, 2, 2, (12, 12)).unwrap();
        assert_eq!(icon.pixels(), pixels);
        assert_eq!(icon.width(), 4);
        assert_eq!(icon.height(), 2);
        assert_eq!(icon.buffer_scale(), 2);
        assert_eq!(icon.offset(), (12, 12));
    }

    #[test]
    fn wrong_pixel_layout_is_rejected() {
        assert_eq!(
            OutgoingIcon::new(vec![0; 23], 3, 2, 1, (0, 0)),
            Err(OutgoingIconError::InvalidPixelLength {
                expected: 24,
                actual: 23,
            })
        );
    }

    #[test]
    fn zero_dimensions_are_rejected() {
        assert_eq!(
            OutgoingIcon::new(Vec::new(), 0, 1, 1, (0, 0)),
            Err(OutgoingIconError::ZeroWidth)
        );
        assert_eq!(
            OutgoingIcon::new(Vec::new(), 1, 0, 1, (0, 0)),
            Err(OutgoingIconError::ZeroHeight)
        );
    }

    #[test]
    fn pixel_length_arithmetic_cannot_wrap() {
        assert_eq!(
            OutgoingIcon::new(Vec::new(), u32::MAX, u32::MAX, 1, (0, 0)),
            Err(OutgoingIconError::PixelLengthOverflow)
        );
    }

    #[test]
    fn invalid_buffer_scales_are_rejected() {
        for scale in [i32::MIN, -1, 0] {
            assert_eq!(
                OutgoingIcon::new(vec![0; 4], 1, 1, scale, (0, 0)),
                Err(OutgoingIconError::InvalidBufferScale(scale))
            );
        }
    }

    #[test]
    fn dimensions_must_be_exact_multiples_of_buffer_scale() {
        assert_eq!(
            OutgoingIcon::new(vec![0; 3 * 2 * 4], 3, 2, 2, (0, 0)),
            Err(OutgoingIconError::WidthNotMultipleOfBufferScale {
                width: 3,
                buffer_scale: 2,
            })
        );
        assert_eq!(
            OutgoingIcon::new(vec![0; 2 * 3 * 4], 2, 3, 2, (0, 0)),
            Err(OutgoingIconError::HeightNotMultipleOfBufferScale {
                height: 3,
                buffer_scale: 2,
            })
        );
        assert!(OutgoingIcon::new(vec![0; 4 * 2 * 4], 4, 2, 2, (0, 0)).is_ok());
    }

    #[test]
    fn signed_shm_pool_limit_includes_slot_rounding() {
        assert_eq!(
            OutgoingIcon::new(Vec::new(), 32_768, 16_384, 1, (0, 0)),
            Err(OutgoingIconError::ShmPoolTooLarge {
                required: 2_147_483_648,
            })
        );
        assert_eq!(
            OutgoingIcon::new(Vec::new(), 536_870_911, 1, 1, (0, 0)),
            Err(OutgoingIconError::ShmPoolTooLarge {
                required: 2_147_483_648,
            })
        );
    }

    #[test]
    fn shm_slot_length_is_rounded_to_64_bytes() {
        assert_eq!(shm_slot_len(0), Some(0));
        assert_eq!(shm_slot_len(1), Some(64));
        assert_eq!(shm_slot_len(63), Some(64));
        assert_eq!(shm_slot_len(64), Some(64));
        assert_eq!(shm_slot_len(65), Some(128));
        assert_eq!(shm_slot_len(usize::MAX), None);
    }

    #[test]
    fn rgba_is_written_as_little_endian_argb8888_without_repremultiplying() {
        let rgba = [0x12, 0x34, 0x56, 0x78, 0x01, 0x02, 0x03, 0x04];
        let mut output = [0; 8];
        write_little_endian_argb8888(&rgba, &mut output);
        assert_eq!(output, [0x56, 0x34, 0x12, 0x78, 0x03, 0x02, 0x01, 0x04]);
    }
}
