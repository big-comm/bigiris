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
/// Convert ICC-tagged pixels into canonical sRGB, preserving sample
/// precision and alpha.
///
/// * RGB profile on RGB pixels: colorimetric transform into sRGB.
/// * Gray profile on gray pixels: transform into a gray profile with the
///   sRGB tone curve, so the samples display like untagged gray.
/// * Any other combination (CMYK profile on pixels the decoder already
///   converted to RGB, RGB profile on gray pixels, Lab/XYZ…): the profile is
///   ignored with a warning — refusing to open the image would be worse.
///
/// Only a malformed profile fails: relabeling those pixels silently would
/// hide a corrupt file.
pub fn normalize_srgb(image: &mut DynamicImage, icc: &[u8]) -> ImageResult<()> {
    let profile = parse(icc)?;
    match profile.color_space {
        DataColorSpace::Rgb if is_rgb(image) => normalize_rgb(image, icc, &profile),
        DataColorSpace::Gray if is_gray(image) => normalize_gray(image, &profile),
        space => {
            tracing::warn!(
                ?space,
                color = ?image.color(),
                "perfil ICC incompatível com os pixels decodificados; perfil ignorado"
            );
            Ok(())
        }
    }
}

fn parse(icc: &[u8]) -> ImageResult<ColorProfile> {
    ColorProfile::new_from_slice_with_options(
        icc,
        ParsingOptions {
            max_profile_size: 4 * 1024 * 1024,
            max_allowed_clut_size: 1024 * 1024,
            max_allowed_trc_size: 16384,
        },
    )
    .map_err(error)
}

fn is_rgb(image: &DynamicImage) -> bool {
    matches!(
        image,
        DynamicImage::ImageRgb8(_)
            | DynamicImage::ImageRgba8(_)
            | DynamicImage::ImageRgb16(_)
            | DynamicImage::ImageRgba16(_)
            | DynamicImage::ImageRgb32F(_)
            | DynamicImage::ImageRgba32F(_)
    )
}

fn is_gray(image: &DynamicImage) -> bool {
    matches!(
        image,
        DynamicImage::ImageLuma8(_)
            | DynamicImage::ImageLumaA8(_)
            | DynamicImage::ImageLuma16(_)
            | DynamicImage::ImageLumaA16(_)
    )
}

fn normalize_rgb(image: &mut DynamicImage, icc: &[u8], profile: &ColorProfile) -> ImageResult<()> {
    let target = ColorProfile::new_srgb();
    // Our canonical profile needs no transform: preserve exact lossless samples.
    if same_canonical_profile(icc, &target.encode().map_err(error)?) {
        image.set_color_space(image::metadata::Cicp::SRGB)?;
        return Ok(());
    }
    transform_rgb(image, profile, &target)?;
    image.set_color_space(image::metadata::Cicp::SRGB)?;
    Ok(())
}

/// In-place RGB(A) transform between two RGB profiles, any sample type.
fn transform_rgb(
    image: &mut DynamicImage,
    from: &ColorProfile,
    to: &ColorProfile,
) -> ImageResult<()> {
    let options = TransformOptions::default();
    match image {
        DynamicImage::ImageRgb8(buffer) => from
            .create_in_place_transform_8bit(Layout::Rgb, to, options)
            .map_err(error)?
            .transform(buffer.as_mut())
            .map_err(error)?,
        DynamicImage::ImageRgba8(buffer) => from
            .create_in_place_transform_8bit(Layout::Rgba, to, options)
            .map_err(error)?
            .transform(buffer.as_mut())
            .map_err(error)?,
        DynamicImage::ImageRgb16(buffer) => from
            .create_in_place_transform_16bit(Layout::Rgb, to, options)
            .map_err(error)?
            .transform(buffer.as_mut())
            .map_err(error)?,
        DynamicImage::ImageRgba16(buffer) => from
            .create_in_place_transform_16bit(Layout::Rgba, to, options)
            .map_err(error)?
            .transform(buffer.as_mut())
            .map_err(error)?,
        DynamicImage::ImageRgb32F(buffer) => from
            .create_in_place_transform_f32(Layout::Rgb, to, options)
            .map_err(error)?
            .transform(buffer.as_mut())
            .map_err(error)?,
        DynamicImage::ImageRgba32F(buffer) => from
            .create_in_place_transform_f32(Layout::Rgba, to, options)
            .map_err(error)?
            .transform(buffer.as_mut())
            .map_err(error)?,
        _ => return Err(error("layout de pixels incompatível com o perfil RGB")),
    }
    Ok(())
}

/// Gray profile whose tone curve is the sRGB one: what an untagged gray
/// image is displayed as.
fn srgb_gray() -> ColorProfile {
    let mut gray = ColorProfile::new_gray_with_gamma(2.2);
    gray.gray_trc = ColorProfile::new_srgb().red_trc;
    gray
}

fn normalize_gray(image: &mut DynamicImage, profile: &ColorProfile) -> ImageResult<()> {
    let target = srgb_gray();
    let options = TransformOptions::default();
    match image {
        DynamicImage::ImageLuma8(buffer) => profile
            .create_in_place_transform_8bit(Layout::Gray, &target, options)
            .map_err(error)?
            .transform(buffer.as_mut())
            .map_err(error)?,
        DynamicImage::ImageLumaA8(buffer) => profile
            .create_in_place_transform_8bit(Layout::GrayAlpha, &target, options)
            .map_err(error)?
            .transform(buffer.as_mut())
            .map_err(error)?,
        DynamicImage::ImageLuma16(buffer) => profile
            .create_in_place_transform_16bit(Layout::Gray, &target, options)
            .map_err(error)?
            .transform(buffer.as_mut())
            .map_err(error)?,
        DynamicImage::ImageLumaA16(buffer) => profile
            .create_in_place_transform_16bit(Layout::GrayAlpha, &target, options)
            .map_err(error)?
            .transform(buffer.as_mut())
            .map_err(error)?,
        _ => return Err(error("layout de pixels incompatível com o perfil cinza")),
    }
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
    fn gray_profile_on_gray_pixels_is_transformed_not_rejected() {
        // Linear gray (gamma 1.0): mid-gray 128 must brighten to ~188 in sRGB.
        let profile = ColorProfile::new_gray_with_gamma(1.0).encode().unwrap();
        let mut image = DynamicImage::ImageLumaA8(image::ImageBuffer::from_pixel(
            2,
            1,
            image::LumaA([128, 77]),
        ));
        normalize_srgb(&mut image, &profile).unwrap();
        let pixel = image.as_luma_alpha8().unwrap().get_pixel(0, 0).0;
        assert!((185..=191).contains(&pixel[0]), "got {pixel:?}");
        assert_eq!(pixel[1], 77);

        let mut image16 =
            DynamicImage::ImageLuma16(image::ImageBuffer::from_pixel(1, 1, image::Luma([32768])));
        normalize_srgb(&mut image16, &profile).unwrap();
        assert!(image16.as_luma16().unwrap().get_pixel(0, 0).0[0] > 40000);
    }

    #[test]
    fn mismatched_or_cmyk_profiles_are_ignored_without_error() {
        // Gray profile on RGB pixels, RGB profile on gray pixels.
        let gray = ColorProfile::new_gray_with_gamma(1.0).encode().unwrap();
        let mut rgb = DynamicImage::ImageRgb8(image::ImageBuffer::from_pixel(
            1,
            1,
            image::Rgb([10, 20, 30]),
        ));
        normalize_srgb(&mut rgb, &gray).unwrap();
        assert_eq!(rgb.as_rgb8().unwrap().get_pixel(0, 0).0, [10, 20, 30]);

        let p3 = ColorProfile::new_display_p3().encode().unwrap();
        let mut luma =
            DynamicImage::ImageLuma8(image::ImageBuffer::from_pixel(1, 1, image::Luma([99])));
        normalize_srgb(&mut luma, &p3).unwrap();
        assert_eq!(luma.as_luma8().unwrap().get_pixel(0, 0).0, [99]);

        // A CMYK-tagged JPEG arrives already converted to RGB by the decoder.
        let mut cmyk = p3.clone();
        cmyk[16..20].copy_from_slice(b"CMYK");
        assert_eq!(parse(&cmyk).unwrap().color_space, DataColorSpace::Cmyk);
        normalize_srgb(&mut rgb, &cmyk).unwrap();
        assert_eq!(rgb.as_rgb8().unwrap().get_pixel(0, 0).0, [10, 20, 30]);
    }

    #[test]
    fn malformed_profile_does_not_silently_relabel_pixels() {
        let mut image = DynamicImage::new_rgb8(1, 1);
        assert!(normalize_srgb(&mut image, b"invalid ICC").is_err());
    }
}
