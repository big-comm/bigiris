// SPDX-License-Identifier: GPL-3.0-or-later
//! ICC normalization without retaining identifying profile metadata.
use image::{DynamicImage, ImageError, ImageResult};
use moxcms::{ColorProfile, DataColorSpace, Layout, ParsingOptions, TransformOptions};

fn error(message: impl std::fmt::Display) -> ImageError {
    ImageError::IoError(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("perfil ICC: {message}"),
    ))
}
/// Convert RGB ICC pixels into canonical sRGB, preserving sample precision
/// and alpha. Unsupported or malformed profiles fail explicitly.
pub fn normalize_srgb(image: &mut DynamicImage, icc: &[u8]) -> ImageResult<()> {
    let profile = ColorProfile::new_from_slice_with_options(
        icc,
        ParsingOptions {
            max_profile_size: 4 * 1024 * 1024,
            max_allowed_clut_size: 1024 * 1024,
            max_allowed_trc_size: 16384,
        },
    )
    .map_err(error)?;
    if profile.color_space != DataColorSpace::Rgb {
        return Err(error(
            "perfil não RGB não suportado; conversão interrompida para preservar a fidelidade",
        ));
    }
    let target = ColorProfile::new_srgb();
    // Our canonical profile needs no transform: preserve exact lossless samples.
    if same_canonical_profile(icc, &target.encode().map_err(error)?) {
        image.set_color_space(image::metadata::Cicp::SRGB)?;
        return Ok(());
    }
    let options = TransformOptions::default();
    match image {
        DynamicImage::ImageRgb8(buffer) => profile
            .create_in_place_transform_8bit(Layout::Rgb, &target, options)
            .map_err(error)?
            .transform(buffer.as_mut())
            .map_err(error)?,
        DynamicImage::ImageRgba8(buffer) => profile
            .create_in_place_transform_8bit(Layout::Rgba, &target, options)
            .map_err(error)?
            .transform(buffer.as_mut())
            .map_err(error)?,
        DynamicImage::ImageRgb16(buffer) => profile
            .create_in_place_transform_16bit(Layout::Rgb, &target, options)
            .map_err(error)?
            .transform(buffer.as_mut())
            .map_err(error)?,
        DynamicImage::ImageRgba16(buffer) => profile
            .create_in_place_transform_16bit(Layout::Rgba, &target, options)
            .map_err(error)?
            .transform(buffer.as_mut())
            .map_err(error)?,
        DynamicImage::ImageRgb32F(buffer) => profile
            .create_in_place_transform_f32(Layout::Rgb, &target, options)
            .map_err(error)?
            .transform(buffer.as_mut())
            .map_err(error)?,
        DynamicImage::ImageRgba32F(buffer) => profile
            .create_in_place_transform_f32(Layout::Rgba, &target, options)
            .map_err(error)?
            .transform(buffer.as_mut())
            .map_err(error)?,
        _ => return Err(error("layout de pixels incompatível com o perfil RGB")),
    }
    image.set_color_space(image::metadata::Cicp::SRGB)?;
    Ok(())
}
// ICC creation timestamp and profile ID do not change colorimetry.
fn same_canonical_profile(a: &[u8], b: &[u8]) -> bool {
    a.len() >= 128
        && a.len() == b.len()
        && a[..24] == b[..24]
        && a[36..84] == b[36..84]
        && a[100..] == b[100..]
}

/// A generated sRGB profile contains no source camera, GPS or author fields.
pub(crate) fn srgb_profile() -> ImageResult<Vec<u8>> {
    ColorProfile::new_srgb().encode().map_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn display_p3_changes_pixels_and_preserves_alpha_and_precision() {
        let profile = ColorProfile::new_display_p3().encode().unwrap();
        let mut image = DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
            2,
            1,
            image::Rgba([40000, 20000, 10000, 12345]),
        ));
        normalize_srgb(&mut image, &profile).unwrap();
        let pixel = image.as_rgba16().unwrap().get_pixel(0, 0).0;
        assert_ne!(&pixel[..3], &[40000, 20000, 10000]);
        assert_eq!(pixel[3], 12345);
    }
    #[test]
    fn generated_srgb_preserves_exact_samples_across_creation_dates() {
        let mut icc = srgb_profile().unwrap();
        icc[24..36].copy_from_slice(&[0x07, 0xe8, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0]);
        icc[84..100].fill(42);
        let mut image = DynamicImage::ImageRgb16(image::ImageBuffer::from_pixel(
            2,
            1,
            image::Rgb([12345, 23456, 34567]),
        ));
        normalize_srgb(&mut image, &icc).unwrap();
        assert_eq!(image.as_rgb16().unwrap().get_pixel(0, 0).0, [12345, 23456, 34567]);
    }

    #[test]
    fn malformed_profile_does_not_silently_relabel_pixels() {
        let mut image = DynamicImage::new_rgb8(1, 1);
        assert!(normalize_srgb(&mut image, b"invalid ICC").is_err());
    }
}
