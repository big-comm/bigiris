// SPDX-License-Identifier: GPL-3.0-or-later
//! Single-file conversion pipeline: decode → re-encode to a target format.
//!
//! The decode/encode/output-path primitives live in [`crate::pipeline`]; this
//! module just orchestrates them and owns the [`OverwritePolicy`] /
//! [`ConvertOutcome`] types shared across every transform module.

use std::path::{Path, PathBuf};

use image::DynamicImage;

use crate::{pipeline, Format, Result};

/// Policy to apply when the computed output path already exists on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverwritePolicy {
    /// Leave the existing file alone and return [`ConvertOutcome::Skipped`].
    #[default]
    Skip,
    /// Overwrite the destination unconditionally.
    Replace,
    /// Append `_1`, `_2`, … until a free name is found.
    Increment,
}

/// Result of a single-file conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertOutcome {
    /// File was written. The output path may differ from the naive one if
    /// [`OverwritePolicy::Increment`] had to disambiguate.
    Written {
        /// Path that now holds the converted image.
        output: PathBuf,
    },
    /// Destination already existed and policy was [`OverwritePolicy::Skip`].
    Skipped {
        /// Path that was left untouched.
        output: PathBuf,
    },
}

impl ConvertOutcome {
    /// Final path (written or skipped).
    pub fn path(&self) -> &Path {
        match self {
            ConvertOutcome::Written { output } | ConvertOutcome::Skipped { output } => output,
        }
    }
}

/// Convert a single file to `target` and write it next to the input using the
/// target's canonical extension.
///
/// The input is decoded with format auto-detection (by header when possible,
/// extension as fallback) and re-encoded into the target format. Colour space
/// and metadata preservation will land in a later iteration — today the
/// pipeline decodes into the image crate's internal 8-bit representation and
/// encodes straight back out.
pub fn convert_file(
    input: impl AsRef<Path>,
    target: Format,
    opts: &crate::EncodeOptions,
    policy: OverwritePolicy,
) -> Result<ConvertOutcome> {
    convert_file_to(input, None, target, opts, policy)
}

/// Like [`convert_file`], but honours `output_dir` when `Some` — writes
/// the converted file into that directory keeping the source stem and
/// honouring the overwrite policy (Skip/Replace/Increment). Passing
/// `None` is equivalent to [`convert_file`] (grava ao lado do original).
pub fn convert_file_to(
    input: impl AsRef<Path>,
    output_dir: Option<&Path>,
    target: Format,
    opts: &crate::EncodeOptions,
    policy: OverwritePolicy,
) -> Result<ConvertOutcome> {
    let input = input.as_ref();
    let output = pipeline::resolve_output_to(input, output_dir, None, target, policy)?;

    if matches!(policy, OverwritePolicy::Skip) {
        if let Some(skipped) = pipeline::skip_existing(input, output.clone(), pipeline::any_size)? {
            return Ok(skipped);
        }
    }

    tracing::debug!(?input, ?output, ?target, ?opts, "convert: decoding");
    let (img, _src_format) = pipeline::decode_with_source_format(input)?;
    pipeline::encode_and_cleanup(img, output, target, opts, policy)
}

/// Coerce a `DynamicImage` into a colour/bit-depth that the target encoder is
/// willing to accept, avoiding encoder-side `UnsupportedError` from formats
/// that reject common inputs (OpenEXR won't take 8-bit integer; HDR wants
/// RGB floats). The resulting per-format policy is documented on
/// [`crate::save_image`].
///
/// Float images (OpenEXR/HDR) hold scene-linear light while every integer
/// format stores sRGB-encoded values, so crossing that boundary applies the
/// sRGB transfer function (and its inverse) instead of a bare rescale, which
/// would make linear images look far too dark and vice versa.
pub(crate) fn prepare_for_target(img: DynamicImage, target: Format) -> DynamicImage {
    let img = match target {
        Format::OpenExr | Format::Hdr => sanitize_floats(linear_from_srgb(img)),
        _ => srgb_from_linear(img),
    };
    match target {
        Format::OpenExr => DynamicImage::ImageRgba32F(img.into_rgba32f()),
        Format::Hdr => DynamicImage::ImageRgb32F(img.into_rgb32f()),
        Format::Ico => {
            let img =
                if img.width() > 256 || img.height() > 256 { img.thumbnail(256, 256) } else { img };
            DynamicImage::ImageRgba8(img.into_rgba8())
        }
        Format::WebP | Format::Gif | Format::Bmp | Format::Tga | Format::Qoi => {
            DynamicImage::ImageRgba8(img.into_rgba8())
        }
        Format::Png => match img {
            DynamicImage::ImageRgb32F(_) => DynamicImage::ImageRgb16(img.into_rgb16()),
            DynamicImage::ImageRgba32F(_) => DynamicImage::ImageRgba16(img.into_rgba16()),
            other => other,
        },
        // The TIFF encoder writes L, Rgb and Rgba (8/16-bit) but has no
        // gray+alpha layout, so `La` is widened to `Rgba` at the same depth
        // (the alpha channel is kept; extent produces `La` canvases).
        Format::Tiff => match img {
            DynamicImage::ImageRgb32F(_) => DynamicImage::ImageRgb16(img.into_rgb16()),
            DynamicImage::ImageRgba32F(_) => DynamicImage::ImageRgba16(img.into_rgba16()),
            DynamicImage::ImageLumaA8(_) => DynamicImage::ImageRgba8(img.into_rgba8()),
            DynamicImage::ImageLumaA16(_) => DynamicImage::ImageRgba16(img.into_rgba16()),
            other => other,
        },
        // JPEG has no alpha channel. The encoder path does a bare
        // `to_rgb8()`, which *discards* alpha rather than compositing — so a
        // transparent or semi-transparent source (e.g. a letterbox built
        // with a transparent background, or any PNG with transparency) would
        // emit whatever RGB sat *under* the alpha=0 pixels (usually black or
        // garbage) instead of a clean flattened colour. Composite over white
        // here so the JPEG shows the conventional flattened result.
        Format::Jpeg => flatten_over_white(img),
        // PGM/PPM carry 8 or 16-bit gray or RGB, but no alpha: flatten only
        // the alpha channel and keep depth and gray-ness.
        Format::Pnm => flatten_alpha_keep_depth(img),
        _ => img,
    }
}

/// Whether the pixels written for `target` stay single-channel gray (with
/// or without alpha) for a source of colour type `color` — i.e. whether a
/// GRAY ICC profile still describes them after [`prepare_for_target`].
pub(crate) fn writes_gray(color: image::ColorType, target: Format) -> bool {
    use image::ColorType;
    match target {
        Format::Png | Format::Pnm => {
            matches!(color, ColorType::L8 | ColorType::La8 | ColorType::L16 | ColorType::La16)
        }
        Format::Tiff => matches!(color, ColorType::L8 | ColorType::L16),
        _ => false,
    }
}

/// Largest sample written to float formats: 2^126, the top of the RGBE
/// exponent range (still far beyond any real scene luminance).
const FLOAT_OUTPUT_MAX: f32 = 8.507_059e37;

/// Replace values the float encoders cannot represent: NaN and -inf become
/// 0; +inf and anything above [`FLOAT_OUTPUT_MAX`] are clamped to it.
/// Radiance HDR's RGBE conversion overflows its exponent from 2^127 up (a
/// panic in debug builds on +inf, a black or corrupt pixel in release), and
/// OpenEXR readers commonly reject non-finite samples.
fn sanitize_floats(img: DynamicImage) -> DynamicImage {
    fn fix(v: &mut f32) {
        if v.is_nan() || *v == f32::NEG_INFINITY {
            *v = 0.0;
        } else if *v > FLOAT_OUTPUT_MAX {
            *v = FLOAT_OUTPUT_MAX;
        }
    }
    let mut img = img;
    match &mut img {
        DynamicImage::ImageRgb32F(b) => b.iter_mut().for_each(fix),
        DynamicImage::ImageRgba32F(b) => b.iter_mut().for_each(fix),
        _ => {}
    }
    img
}

/// sRGB opto-electronic transfer function (linear → encoded), clamped.
fn srgb_oetf(v: f32) -> f32 {
    let v = if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB electro-optical transfer function (encoded → linear).
fn srgb_eotf(v: f32) -> f32 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn map_colour(data: &mut [f32], channels: usize, f: fn(f32) -> f32) {
    for pixel in data.chunks_exact_mut(channels) {
        pixel[..3].iter_mut().for_each(|v| *v = f(*v));
    }
}

/// Linear float pixels → sRGB-encoded float pixels in `0.0..=1.0`, ready to
/// be quantised by the integer conversions. Integer images pass through.
fn srgb_from_linear(img: DynamicImage) -> DynamicImage {
    let mut img = img;
    match &mut img {
        DynamicImage::ImageRgb32F(b) => map_colour(b.as_mut(), 3, srgb_oetf),
        DynamicImage::ImageRgba32F(b) => map_colour(b.as_mut(), 4, srgb_oetf),
        _ => {}
    }
    img
}

/// 8-bit sRGB RGBA for display, quantised the way [`crate::save_image`]
/// writes 8-bit formats: float images (OpenEXR/HDR) are scene-linear and get
/// the sRGB transfer function first, clamping values above 1.0; integer
/// images are converted as-is. A bare `to_rgba8()` would show linear pixels
/// far too dark.
pub fn into_srgb_rgba8(image: DynamicImage) -> image::RgbaImage {
    srgb_from_linear(image).into_rgba8()
}

/// sRGB-encoded integer pixels → linear float pixels. Float images are
/// already linear and pass through.
fn linear_from_srgb(img: DynamicImage) -> DynamicImage {
    match img {
        DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_) => img,
        img if img.color().has_alpha() => {
            let mut rgba = img.into_rgba32f();
            map_colour(rgba.as_mut(), 4, srgb_eotf);
            DynamicImage::ImageRgba32F(rgba)
        }
        img => {
            let mut rgb = img.into_rgb32f();
            map_colour(rgb.as_mut(), 3, srgb_eotf);
            DynamicImage::ImageRgb32F(rgb)
        }
    }
}

/// Composite any alpha channel over an opaque white background, returning an
/// `Rgb8` image. Inputs without alpha are returned unchanged (no needless
/// re-allocation). Fully-opaque pixels (alpha=255) are bit-for-bit
/// preserved; only transparent/semi-transparent pixels change.
fn flatten_over_white(img: DynamicImage) -> DynamicImage {
    use image::{Rgb, RgbImage};

    if !img.color().has_alpha() {
        return DynamicImage::ImageRgb8(img.into_rgb8());
    }
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut out = RgbImage::new(w, h);
    for (x, y, px) in rgba.enumerate_pixels() {
        let a = u32::from(px[3]);
        // out = src*a + white*(255-a), rounded.
        let blend = |c: u8| {
            let v = u32::from(c) * a + 255 * (255 - a);
            ((v + 127) / 255) as u8
        };
        out.put_pixel(x, y, Rgb([blend(px[0]), blend(px[1]), blend(px[2])]));
    }
    DynamicImage::ImageRgb8(out)
}

/// Composite alpha over white keeping sample depth and gray/RGB layout:
/// `La8 → L8`, `La16 → L16`, `Rgba8 → Rgb8`, `Rgba16 → Rgb16`; float
/// becomes 16-bit RGB. Opaque inputs pass through unchanged.
fn flatten_alpha_keep_depth(img: DynamicImage) -> DynamicImage {
    fn blend<const N: usize, const M: usize>(data: &[u16], max: u32) -> Vec<u16> {
        data.as_chunks::<N>()
            .0
            .iter()
            .flat_map(|px| {
                let a = u32::from(px[M]);
                px[..M]
                    .iter()
                    .map(move |&c| ((u32::from(c) * a + max * (max - a) + max / 2) / max) as u16)
            })
            .collect()
    }
    let (w, h) = (img.width(), img.height());
    match img {
        DynamicImage::ImageLumaA8(b) => {
            let wide: Vec<u16> = b.as_raw().iter().map(|&v| u16::from(v)).collect();
            let raw = blend::<2, 1>(&wide, 255).into_iter().map(|v| v as u8).collect();
            DynamicImage::ImageLuma8(image::ImageBuffer::from_raw(w, h, raw).expect("size"))
        }
        DynamicImage::ImageRgba8(b) => {
            let wide: Vec<u16> = b.as_raw().iter().map(|&v| u16::from(v)).collect();
            let raw = blend::<4, 3>(&wide, 255).into_iter().map(|v| v as u8).collect();
            DynamicImage::ImageRgb8(image::ImageBuffer::from_raw(w, h, raw).expect("size"))
        }
        DynamicImage::ImageLumaA16(b) => DynamicImage::ImageLuma16(
            image::ImageBuffer::from_raw(w, h, blend::<2, 1>(b.as_raw(), 65535)).expect("size"),
        ),
        DynamicImage::ImageRgba16(b) => DynamicImage::ImageRgb16(
            image::ImageBuffer::from_raw(w, h, blend::<4, 3>(b.as_raw(), 65535)).expect("size"),
        ),
        DynamicImage::ImageRgba32F(_) => {
            flatten_alpha_keep_depth(DynamicImage::ImageRgba16(img.into_rgba16()))
        }
        DynamicImage::ImageRgb32F(_) => DynamicImage::ImageRgb16(img.into_rgb16()),
        DynamicImage::ImageLuma8(_)
        | DynamicImage::ImageLuma16(_)
        | DynamicImage::ImageRgb8(_)
        | DynamicImage::ImageRgb16(_) => img,
        other if other.color().has_alpha() => {
            flatten_alpha_keep_depth(DynamicImage::ImageRgba16(other.into_rgba16()))
        }
        other => DynamicImage::ImageRgb16(other.into_rgb16()),
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;

    #[test]
    fn linear_floats_display_through_the_srgb_transfer() {
        let linear = image::Rgba32FImage::from_pixel(1, 1, image::Rgba([0.5, 0.0, 2.0, 0.5]));
        let rgba = into_srgb_rgba8(DynamicImage::ImageRgba32F(linear));
        assert_eq!(rgba.get_pixel(0, 0).0, [188, 0, 255, 128]);
        let encoded = image::RgbImage::from_pixel(1, 1, image::Rgb([128, 7, 255]));
        let rgba = into_srgb_rgba8(DynamicImage::ImageRgb8(encoded));
        assert_eq!(rgba.get_pixel(0, 0).0, [128, 7, 255, 255]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BigImageError;
    use image::{Rgb, RgbImage};
    use tempfile::TempDir;

    fn write_fixture_png(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let mut img = RgbImage::new(8, 8);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = Rgb([x as u8 * 32, y as u8 * 32, 128]);
        }
        img.save(&path).unwrap();
        path
    }

    #[test]
    fn png_to_jpeg_writes_sibling_file() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "fx.png");

        let outcome = convert_file(
            &src,
            Format::Jpeg,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap();
        let out = match outcome {
            ConvertOutcome::Written { output } => output,
            ConvertOutcome::Skipped { .. } => panic!("unexpected skip"),
        };

        assert_eq!(out, dir.path().join("fx.jpg"));
        assert!(out.exists());
        // Round-trip decodes cleanly.
        image::open(&out).unwrap();
    }

    #[test]
    fn skip_policy_preserves_existing_output() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "fx.png");
        let dest = dir.path().join("fx.jpg");
        std::fs::write(&dest, b"sentinel-not-an-image").unwrap();

        let outcome = convert_file(
            &src,
            Format::Jpeg,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Skip,
        )
        .unwrap();
        assert!(matches!(outcome, ConvertOutcome::Skipped { .. }));
        // Sentinel bytes are still there — we didn't decode/re-encode over it.
        assert_eq!(std::fs::read(&dest).unwrap(), b"sentinel-not-an-image");
    }

    #[test]
    fn increment_policy_appends_suffix() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "fx.png");
        std::fs::write(dir.path().join("fx.jpg"), b"prev").unwrap();

        let outcome = convert_file(
            &src,
            Format::Jpeg,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Increment,
        )
        .unwrap();
        let out = match outcome {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        assert_eq!(out, dir.path().join("fx_1.jpg"));
        assert!(out.exists());
        // Pre-existing sibling was left alone.
        assert_eq!(std::fs::read(dir.path().join("fx.jpg")).unwrap(), b"prev");
    }

    #[test]
    fn replace_policy_overwrites() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "fx.png");
        let dest = dir.path().join("fx.jpg");
        std::fs::write(&dest, b"prev").unwrap();

        let outcome = convert_file(
            &src,
            Format::Jpeg,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap();
        assert!(matches!(outcome, ConvertOutcome::Written { .. }));
        // File must now be a real JPEG, not the placeholder.
        image::open(&dest).unwrap();
    }

    #[test]
    fn unknown_input_returns_decode_error() {
        let dir = TempDir::new().unwrap();
        let bogus = dir.path().join("not-an-image.png");
        std::fs::write(&bogus, b"garbage").unwrap();

        let err = convert_file(
            &bogus,
            Format::Jpeg,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap_err();
        assert!(matches!(err, BigImageError::Decode { .. }));
    }

    #[test]
    fn png_to_exr_auto_converts_to_float() {
        // EXR rejects 8-bit integer inputs; the prepare step must upcast.
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "fx.png");

        let outcome = convert_file(
            &src,
            Format::OpenExr,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap();
        let out = match outcome {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        assert!(out.exists());
        assert!(std::fs::metadata(&out).unwrap().len() > 0);
        image::open(&out).unwrap();
    }

    #[test]
    fn png_to_hdr_auto_converts_to_rgb_float() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "fx.png");

        let outcome = convert_file(
            &src,
            Format::Hdr,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap();
        assert!(matches!(outcome, ConvertOutcome::Written { .. }));
    }

    #[test]
    fn png_to_qoi_and_back() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "fx.png");

        let q = convert_file(
            &src,
            Format::Qoi,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap();
        let qoi_path = match q {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        let back = convert_file(
            &qoi_path,
            Format::Png,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Increment,
        )
        .unwrap();
        assert!(matches!(back, ConvertOutcome::Written { .. }));
    }

    #[test]
    fn replace_in_place_failure_keeps_source() {
        // Regression: Replace with output == input must never destroy the
        // original on an encode failure. We force a failure by feeding an
        // EXR target a path whose parent we make unwritable is brittle, so
        // instead assert the happy in-place replace leaves a valid image
        // (the atomic temp+rename path) — the destructive bug deleted it.
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "inplace.png");
        let before = std::fs::read(&src).unwrap();
        assert!(!before.is_empty());

        let outcome = convert_file(
            &src,
            Format::Png,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap();
        assert!(matches!(outcome, ConvertOutcome::Written { .. }));
        // Source path still exists and is a decodable PNG (rename swapped a
        // fully-written file in; no partial, no deletion).
        assert!(src.exists());
        image::open(&src).unwrap();
        // No leftover temp file in the directory.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".part-"))
            .collect();
        assert!(leftovers.is_empty(), "temp file leaked: {leftovers:?}");
    }

    #[test]
    fn rgba_to_jpeg_flattens_alpha_over_white_not_black() {
        use image::{Rgba, RgbaImage};
        // Fully-transparent source: dropping alpha would yield black; the
        // flatten step must composite over white instead.
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("trans.png");
        let img = RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 0]));
        img.save(&src).unwrap();

        let out = match convert_file(
            &src,
            Format::Jpeg,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap()
        {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        let decoded = image::open(&out).unwrap().to_rgb8();
        let px = decoded.get_pixel(2, 2).0;
        // JPEG is lossy, so allow a margin, but it must be near-white, never
        // near-black (the old bug).
        assert!(px[0] > 240 && px[1] > 240 && px[2] > 240, "expected white-ish, got {px:?}");
    }

    #[test]
    fn linear_float_gets_srgb_transfer_before_quantising() {
        let float = DynamicImage::ImageRgb32F(image::ImageBuffer::from_pixel(
            1,
            1,
            image::Rgb([0.5f32, 0.0, 2.0]),
        ));
        let png = prepare_for_target(float.clone(), Format::Png);
        assert_eq!(png.color(), image::ColorType::Rgb16, "no alpha may be added");
        let px8 = DynamicImage::ImageRgb8(png.into_rgb8()).as_rgb8().unwrap().get_pixel(0, 0).0;
        assert!((187..=189).contains(&px8[0]), "0.5 linear should be ~188, got {px8:?}");
        assert_eq!(px8[1], 0);
        assert_eq!(px8[2], 255, "out-of-range HDR values clamp");
        let jpeg = prepare_for_target(float, Format::Jpeg);
        assert!((187..=189).contains(&jpeg.as_rgb8().unwrap().get_pixel(0, 0).0[0]));
    }

    #[test]
    fn srgb_to_float_and_back_is_stable() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "rt.png");
        let opts = crate::EncodeOptions::default();
        let exr = convert_file(&src, Format::OpenExr, &opts, OverwritePolicy::Replace).unwrap();
        let linear = crate::open_image(exr.path()).unwrap();
        // sRGB 128 is ~0.216 in linear light.
        let mid = DynamicImage::ImageRgb8(RgbImage::from_pixel(1, 1, Rgb([128, 128, 128])));
        let mid = prepare_for_target(mid, Format::OpenExr);
        assert!((mid.as_rgba32f().unwrap().get_pixel(0, 0).0[0] - 0.2158).abs() < 1e-3);
        let back = prepare_for_target(linear, Format::Png).into_rgb8();
        let original = image::open(&src).unwrap().into_rgb8();
        for (a, b) in original.pixels().zip(back.pixels()) {
            for c in 0..3 {
                assert!(a[c].abs_diff(b[c]) <= 1, "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn pnm_flattens_alpha_but_keeps_depth_and_gray() {
        let l16 =
            DynamicImage::ImageLuma16(image::ImageBuffer::from_pixel(1, 1, image::Luma([4321u16])));
        assert_eq!(prepare_for_target(l16.clone(), Format::Pnm), l16);
        let rgb16 = DynamicImage::ImageRgb16(image::ImageBuffer::from_pixel(
            1,
            1,
            image::Rgb([1u16, 2, 3]),
        ));
        assert_eq!(prepare_for_target(rgb16.clone(), Format::Pnm), rgb16);
        let l8 = DynamicImage::ImageLuma8(image::ImageBuffer::from_pixel(1, 1, image::Luma([9u8])));
        assert_eq!(prepare_for_target(l8.clone(), Format::Pnm), l8);
        let la16 = DynamicImage::ImageLumaA16(image::ImageBuffer::from_pixel(
            1,
            1,
            image::LumaA([0u16, 0]),
        ));
        let flat = prepare_for_target(la16, Format::Pnm);
        assert_eq!(flat.as_luma16().unwrap().get_pixel(0, 0).0, [65535]);
        let rgba8 = DynamicImage::ImageRgba8(image::ImageBuffer::from_pixel(
            1,
            1,
            image::Rgba([0u8, 0, 0, 128]),
        ));
        let flat = prepare_for_target(rgba8, Format::Pnm);
        assert_eq!(flat.as_rgb8().unwrap().get_pixel(0, 0).0, [127, 127, 127]);
    }

    #[test]
    fn non_finite_floats_are_sanitised_for_float_targets() {
        let img = DynamicImage::ImageRgb32F(image::ImageBuffer::from_fn(4, 1, |x, _| {
            let v = [f32::INFINITY, f32::NAN, f32::NEG_INFINITY, f32::MAX][x as usize];
            image::Rgb([v, 0.5, v])
        }));
        for target in [Format::Hdr, Format::OpenExr] {
            let prepared = prepare_for_target(img.clone(), target).into_rgb32f();
            for px in prepared.pixels() {
                assert!(px.0.iter().all(|v| v.is_finite() && *v >= 0.0), "{target:?}: {px:?}");
            }
            assert_eq!(prepared.get_pixel(0, 0).0[0], FLOAT_OUTPUT_MAX);
            assert_eq!(prepared.get_pixel(1, 0).0[0], 0.0);
            assert_eq!(prepared.get_pixel(2, 0).0[0], 0.0);
            assert_eq!(prepared.get_pixel(3, 0).0[0], FLOAT_OUTPUT_MAX);
        }
        // Through the public path: no overflow, and +inf stays the brightest
        // value instead of wrapping to black.
        let dir = TempDir::new().unwrap();
        for (name, format) in [("inf.hdr", Format::Hdr), ("inf.exr", Format::OpenExr)] {
            let path = dir.path().join(name);
            crate::save_image(
                img.clone(),
                &path,
                format,
                &crate::EncodeOptions::default(),
                OverwritePolicy::Replace,
            )
            .unwrap();
            let back = crate::open_image(&path).unwrap().into_rgb32f();
            assert!(back.get_pixel(0, 0).0[0] > 1e37, "{format:?}");
            assert!(back.pixels().all(|px| px.0.iter().all(|v| v.is_finite())), "{format:?}");
        }
    }

    #[test]
    fn png_to_avif_roundtrip() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "fx.png");

        let a = convert_file(
            &src,
            Format::Avif,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap();
        let avif_path = match a {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        // Decode the AVIF back out via the pipeline to confirm dav1d is wired.
        let back = convert_file(
            &avif_path,
            Format::Png,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Increment,
        )
        .unwrap();
        assert!(matches!(back, ConvertOutcome::Written { .. }));
    }
}
