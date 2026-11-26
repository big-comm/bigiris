// SPDX-License-Identifier: GPL-3.0-or-later
//! ICC normalization without retaining identifying profile metadata.
use image::{DynamicImage, ImageError, ImageResult};
use moxcms::{ColorProfile, DataColorSpace, Layout, ParsingOptions, TransformOptions};

fn error(message: impl std::fmt::Display) -> ImageError {
    ImageError::IoError(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("ICC profile: {message}"),
    ))
}
/// Convert ICC-tagged pixels into canonical sRGB, preserving sample
/// precision and alpha.
///
/// * RGB profile on RGB pixels: colorimetric transform into sRGB.
/// * Gray profile on gray pixels: transform into a gray profile with the
///   sRGB tone curve, so the samples display like untagged gray.
/// * Gray profile on RGB pixels (gray samples widened to RGB by an edit,
///   e.g. a coloured extent canvas): each channel goes through the gray
///   tone curve into sRGB, so neutral pixels look exactly as before.
/// * Any other combination (CMYK profile on pixels the decoder already
///   converted to RGB, RGB profile on gray pixels, Lab/XYZ…): the profile is
///   ignored with a warning — refusing to open the image would be worse.
///
/// Only a malformed profile fails. Decoding for display then keeps the
/// pixels as sRGB and records it with [`note_ignored_profile`], so the
/// viewer and the CLI can say the colours may be off instead of refusing the
/// photo or relabeling it silently.
pub fn normalize_srgb(image: &mut DynamicImage, icc: &[u8]) -> ImageResult<()> {
    normalize_srgb_applied(image, icc).map(drop)
}

/// [`normalize_srgb`], reporting whether the profile was applied (`false`:
/// incompatible combination, profile ignored and pixels unchanged).
pub(crate) fn normalize_srgb_applied(image: &mut DynamicImage, icc: &[u8]) -> ImageResult<bool> {
    let profile = parse(icc)?;
    match profile.color_space {
        DataColorSpace::Rgb if is_rgb(image) => normalize_rgb(image, icc, &profile),
        DataColorSpace::Gray if is_gray(image) => normalize_gray(image, &profile),
        DataColorSpace::Gray if is_rgb(image) && profile.gray_trc.is_some() => {
            // An RGB profile with the gray tone curve on every channel and
            // sRGB primaries maps neutral samples like the gray profile does.
            let mut widened = ColorProfile::new_srgb();
            widened.cicp = None;
            widened.red_trc.clone_from(&profile.gray_trc);
            widened.green_trc.clone_from(&profile.gray_trc);
            widened.blue_trc.clone_from(&profile.gray_trc);
            transform_rgb(image, &widened, &ColorProfile::new_srgb())?;
            image.set_color_space(image::metadata::Cicp::SRGB)?;
            Ok(())
        }
        space => {
            tracing::warn!(
                ?space,
                color = ?image.color(),
                "ICC profile does not match the decoded pixels; profile ignored"
            );
            return Ok(false);
        }
    }
    .map(|()| true)
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
    // Our canonical profile needs no transform: preserve exact lossless
    // samples. Files written before it was made well-formed carry moxcms's
    // plain encoding.
    if same_canonical_profile(icc, &srgb_profile()?)
        || same_canonical_profile(icc, &target.encode().map_err(error)?)
    {
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
        _ => return Err(error("pixel layout does not match the RGB profile")),
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
        _ => return Err(error("pixel layout does not match the gray profile")),
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

thread_local! {
    static IGNORED_PROFILE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Record that a decode on this thread showed the pixels of a malformed ICC
/// profile as sRGB. The sandboxed codec relays it to the thread that asked.
pub fn note_ignored_profile() {
    IGNORED_PROFILE.with(|flag| flag.set(true));
}

/// Whether a decode on this thread ignored a malformed ICC profile since the
/// last call; clears the note.
pub fn take_ignored_profile() -> bool {
    IGNORED_PROFILE.with(|flag| flag.replace(false))
}

/// Creation date stamped into the generated profile: 2026-01-01 00:00:00,
/// as six big-endian u16 (ICC dateTimeNumber).
const PROFILE_DATE: [u8; 12] = [0x07, 0xEA, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0];

/// PCS illuminant every ICC header must carry (ICC.1 7.2.16): D50 as
/// `XYZNumber` 0x0000F6D6, 0x00010000, 0x0000D32D.
const PCS_D50: moxcms::Xyzd = moxcms::Xyzd { x: 0.9642, y: 1.0, z: 0.8249 };

/// A generated sRGB profile contains no source camera, GPS or author fields.
/// moxcms stamps the current time and an MD5 profile ID over it, which made
/// two saves of the same pixels differ byte for byte; the date is fixed and
/// the ID left zero ("not computed", ICC.1 7.2.18). Its header illuminant is
/// set to the PCS D50 and its tags realigned, since libpng and other strict
/// readers drop a profile that breaks either rule.
pub(crate) fn srgb_profile() -> ImageResult<Vec<u8>> {
    static PROFILE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    if let Some(profile) = PROFILE.get() {
        return Ok(profile.clone());
    }
    let mut srgb = ColorProfile::new_srgb();
    srgb.white_point = PCS_D50;
    // ICC.1 9.2.36: a v4 display profile's media white point is the PCS
    // D50, and `chad` adapts the D65 display white to it (moxcms stores
    // the bare Bradford cone matrix there).
    srgb.media_white_point = Some(PCS_D50);
    let pcs = moxcms::Xyz { x: 0.9642, y: 1.0, z: 0.8249 };
    srgb.chromatic_adaptation =
        Some(moxcms::adaption_matrix_d(moxcms::WHITE_POINT_D65.to_xyz(), pcs));
    let encoded = srgb.encode().map_err(error)?;
    let mut icc = align_tags(&encoded).ok_or_else(|| error("generated sRGB tag table"))?;
    icc[24..36].copy_from_slice(&PROFILE_DATE);
    icc[84..100].fill(0);
    Ok(PROFILE.get_or_init(|| icc).clone())
}

/// `icc` with each tag's data moved to a 4-byte boundary and the size padded
/// to one (ICC.1 7.1.2, 7.3.1); `None` when the tag table does not fit.
fn align_tags(icc: &[u8]) -> Option<Vec<u8>> {
    let be = |at: usize| Some(u32::from_be_bytes(icc.get(at..at + 4)?.try_into().ok()?));
    let count = usize::try_from(be(128)?).ok()?;
    let table_end = count.checked_mul(12)?.checked_add(132)?;
    let mut out = icc.get(..table_end)?.to_vec();
    for entry in (132..table_end).step_by(12) {
        let offset = usize::try_from(be(entry + 4)?).ok()?;
        let size = usize::try_from(be(entry + 8)?).ok()?;
        let tag_data = icc.get(offset..offset.checked_add(size)?)?;
        out.resize(out.len().next_multiple_of(4), 0);
        let start = u32::try_from(out.len()).ok()?;
        out[entry + 4..entry + 8].copy_from_slice(&start.to_be_bytes());
        out.extend_from_slice(tag_data);
    }
    out.resize(out.len().next_multiple_of(4), 0);
    let size = u32::try_from(out.len()).ok()?;
    out[..4].copy_from_slice(&size.to_be_bytes());
    Some(out)
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
    /// Saves embed this profile: a clock-dependent stamp made identical
    /// saves differ, so it must not depend on when it was generated.
    /// Guards: interoperability: ICC.1 7.2.16 and 7.3.1, which libpng
    /// enforces by dropping the profile.
    #[test]
    fn srgb_profile_is_well_formed() {
        let icc = srgb_profile().unwrap();
        let be = |at: usize| u32::from_be_bytes(icc[at..at + 4].try_into().unwrap());
        assert_eq!(be(0) as usize, icc.len());
        assert_eq!(icc.len() % 4, 0);
        assert_eq!([be(68), be(72), be(76)], [0xf6d6, 0x1_0000, 0xd32d], "PCS illuminant D50");
        let count = be(128) as usize;
        assert!(count > 0);
        for entry in (132..132 + 12 * count).step_by(12) {
            let (offset, size) = (be(entry + 4) as usize, be(entry + 8) as usize);
            assert_eq!(offset % 4, 0, "tag {:?}", &icc[entry..entry + 4]);
            assert!(offset + size <= icc.len());
        }
        let tag = |signature: &[u8; 4]| {
            let entry = (132..132 + 12 * count).step_by(12).find(|&e| &icc[e..e + 4] == signature);
            be(entry.unwrap_or_else(|| panic!("no {signature:?} tag")) + 4) as usize
        };
        // ICC.1 9.2.36: the media white point of a display profile is D50.
        let wtpt = tag(b"wtpt") + 8;
        assert_eq!([be(wtpt), be(wtpt + 4), be(wtpt + 8)], [0xf6d6, 0x1_0000, 0xd32d]);
        // Bradford D65 to D50 (Lindbloom), as s15Fixed16 numbers.
        let bradford = [
            1.047_811_2,
            0.022_886_6,
            -0.050_127_0,
            0.029_542_4,
            0.990_484_4,
            -0.017_049_1,
            -0.009_234_5,
            0.015_043_6,
            0.752_131_6,
        ];
        let chad = tag(b"chad") + 8;
        for (i, expected) in bradford.into_iter().enumerate() {
            let value = f64::from(be(chad + 4 * i).cast_signed()) / 65536.0;
            assert!((value - expected).abs() < 2e-3, "chad[{i}] = {value}, want {expected}");
        }
    }

    #[test]
    fn srgb_profile_is_stable() {
        let icc = srgb_profile().unwrap();
        assert_eq!(&icc[24..36], &PROFILE_DATE);
        assert!(icc[84..100].iter().all(|&b| b == 0));
        assert_eq!(parse(&icc).unwrap().color_space, DataColorSpace::Rgb);
        let mut image = DynamicImage::ImageRgb8(image::ImageBuffer::from_pixel(
            1,
            1,
            image::Rgb([1, 128, 254]),
        ));
        normalize_srgb(&mut image, &icc).unwrap();
        assert_eq!(image.as_rgb8().unwrap().get_pixel(0, 0).0, [1, 128, 254]);
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
        // RGB profile on gray pixels.
        let mut rgb =
            DynamicImage::ImageRgb8(image::ImageBuffer::from_pixel(1, 1, image::Rgb([10, 20, 30])));

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
    fn gray_profile_on_rgb_pixels_goes_through_the_gray_curve() {
        // Linear gray profile: mid-gray 128 must brighten to ~188 on every
        // channel, alpha and neutrality preserved; sRGB-curve gray is a no-op.
        let linear = ColorProfile::new_gray_with_gamma(1.0).encode().unwrap();
        let mut rgba = DynamicImage::ImageRgba8(image::ImageBuffer::from_pixel(
            1,
            1,
            image::Rgba([128, 128, 128, 77]),
        ));
        assert!(normalize_srgb_applied(&mut rgba, &linear).unwrap());
        let px = rgba.as_rgba8().unwrap().get_pixel(0, 0).0;
        assert!((185..=191).contains(&px[0]), "{px:?}");
        assert!(px[0].abs_diff(px[1]) <= 1 && px[1].abs_diff(px[2]) <= 1, "{px:?}");
        assert_eq!(px[3], 77);
        let mut float =
            DynamicImage::ImageRgb32F(image::ImageBuffer::from_pixel(1, 1, image::Rgb([0.5; 3])));
        normalize_srgb(&mut float, &linear).unwrap();
        let v = float.as_rgb32f().unwrap().get_pixel(0, 0).0[0];
        assert!((0.72..0.75).contains(&v), "{v}");
        let srgb_gray = srgb_gray().encode().unwrap();
        let mut rgb16 = DynamicImage::ImageRgb16(image::ImageBuffer::from_pixel(
            1,
            1,
            image::Rgb([30000u16, 30000, 30000]),
        ));
        normalize_srgb(&mut rgb16, &srgb_gray).unwrap();
        for c in rgb16.as_rgb16().unwrap().get_pixel(0, 0).0 {
            assert!(c.abs_diff(30000) < 80, "{c}");
        }
    }

    #[test]
    fn malformed_profile_does_not_silently_relabel_pixels() {
        let mut image = DynamicImage::new_rgb8(1, 1);
        assert!(normalize_srgb(&mut image, b"invalid ICC").is_err());
    }
}
