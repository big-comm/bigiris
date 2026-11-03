// SPDX-License-Identifier: GPL-3.0-or-later
//! Lossy rotation pipeline (90 / 180 / 270 via `image::imageops`).
//!
//! Arbitrary-angle rotation (with bicubic interpolation) and truly lossless
//! JPEG DCT-level rotation are tracked as follow-up iterations — see PLAN.md.
//! Everything here re-encodes from scratch, which is correct for every format
//! we ship but can lose quality for JPEG sources; the viewer path will use
//! the lossless variant when it lands.

use std::path::Path;

use crate::convert::{ConvertOutcome, OverwritePolicy};
use crate::{pipeline, Format, Result};

/// Cardinal rotation amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    /// 90° clockwise.
    Deg90,
    /// 180°.
    Deg180,
    /// 270° clockwise (= 90° counter-clockwise).
    Deg270,
}

impl Rotation {
    fn filename_suffix(&self) -> &'static str {
        match self {
            Rotation::Deg90 => "_rot90",
            Rotation::Deg180 => "_rot180",
            Rotation::Deg270 => "_rot270",
        }
    }
}

/// Rotate a single file.
pub fn rotate_file(
    input: impl AsRef<Path>,
    rotation: Rotation,
    target: Option<Format>,
    policy: OverwritePolicy,
) -> Result<ConvertOutcome> {
    rotate_file_with_options(input, rotation, target, &crate::EncodeOptions::default(), policy)
}

/// Like [`rotate_file`], with caller-supplied encoder options.
pub fn rotate_file_with_options(
    input: impl AsRef<Path>,
    rotation: Rotation,
    target: Option<Format>,
    opts: &crate::EncodeOptions,
    policy: OverwritePolicy,
) -> Result<ConvertOutcome> {
    let input = input.as_ref();
    if let Some(skipped) = pipeline::skip_before_decode(
        input,
        rotation.filename_suffix(),
        target,
        policy,
        pipeline::any_size,
    )? {
        return Ok(skipped);
    }

    let (src, src_format) = pipeline::decode_with_source_format(input)?;
    let target = target.unwrap_or(src_format);

    let output = pipeline::resolve_output(input, Some(rotation.filename_suffix()), target, policy)?;

    if pipeline::existing_output(&output, target, policy) {
        tracing::debug!(?output, "rotate: skipping existing output");
        return Ok(ConvertOutcome::Skipped { output });
    }

    tracing::debug!(?input, ?output, ?target, ?rotation, "rotate: computing");
    let rotated = apply_to(&src, rotation);
    pipeline::encode_output(rotated, output, target, opts, policy)
}

/// Pure in-memory rotation — shared with preview pipeline.
pub fn apply_to(src: &image::DynamicImage, rotation: Rotation) -> image::DynamicImage {
    match rotation {
        Rotation::Deg90 => src.rotate90(),
        Rotation::Deg180 => src.rotate180(),
        Rotation::Deg270 => src.rotate270(),
    }
}

/// Apply the transformation implied by an EXIF orientation tag (1..=8).
/// The tag describes how the pixel array was stored relative to the scene
/// the camera saw, so "un-applying" it produces the upright image the
/// user actually wants. Unknown values pass through untouched — better
/// than silently garbling orientation.
///
/// Mapping (per EXIF 2.32):
/// 1. Top-left (identity)
/// 2. Top-right → horizontal flip
/// 3. Bottom-right → 180° rotation
/// 4. Bottom-left → vertical flip
/// 5. Left-top → rotate 90° then flip horizontal (transpose)
/// 6. Right-top → rotate 90°
/// 7. Right-bottom → rotate 270° then flip horizontal (transverse)
/// 8. Left-bottom → rotate 270°
pub fn apply_exif_orientation(img: image::DynamicImage, orientation: u16) -> image::DynamicImage {
    match orientation {
        1 => img,
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

/// "Bake" the EXIF orientation of `input` into the pixels and write the
/// upright result with an `_auto` suffix.
///
/// The central decode path ([`crate::pipeline::decode_with_source_format`])
/// already applies the orientation tag, so this command no longer rotates a
/// second time — it simply materialises that upright image to disk (the
/// encode step strips the now-redundant tag). Images without EXIF or with
/// orientation `1` are a harmless re-encode, keeping the return type
/// consistent for callers counting "written vs skipped".
pub fn rotate_file_auto(
    input: impl AsRef<Path>,
    target: Option<crate::Format>,
    policy: OverwritePolicy,
) -> Result<ConvertOutcome> {
    rotate_file_auto_with_options(input, target, &crate::EncodeOptions::default(), policy)
}

/// Like [`rotate_file_auto`], with caller-supplied encoder options.
pub fn rotate_file_auto_with_options(
    input: impl AsRef<Path>,
    target: Option<crate::Format>,
    opts: &crate::EncodeOptions,
    policy: OverwritePolicy,
) -> Result<ConvertOutcome> {
    let input = input.as_ref();
    if let Some(skipped) =
        pipeline::skip_before_decode(input, "_auto", target, policy, pipeline::any_size)?
    {
        return Ok(skipped);
    }

    // `src` arrives already upright — decode_with_source_format honours the
    // orientation tag for every op. Re-applying it here would double-rotate.
    let (src, src_format) = crate::pipeline::decode_with_source_format(input)?;
    let target = target.unwrap_or(src_format);

    let output = crate::pipeline::resolve_output(input, Some("_auto"), target, policy)?;
    if pipeline::existing_output(&output, target, policy) {
        return Ok(ConvertOutcome::Skipped { output });
    }

    tracing::debug!(?input, ?output, "rotate: bake EXIF orientation");
    pipeline::encode_output(src, output, target, opts, policy)
}

/// Largest axis-aligned rectangle (width, height) that fits inside a
/// `width × height` rectangle rotated by `degrees` about its centre.
///
/// This is the crop that removes every empty corner after straightening a
/// photo. Exact (unrounded) values; the rectangle is centred.
pub fn inscribed_size(width: u32, height: u32, degrees: f64) -> (f64, f64) {
    let (w, h) = (f64::from(width), f64::from(height));
    if width == 0 || height == 0 || !degrees.is_finite() {
        return (0.0, 0.0);
    }
    let angle = degrees.to_radians();
    let (sin, cos) = (angle.sin().abs(), angle.cos().abs());
    if sin < 1e-12 {
        return (w, h);
    }
    if cos < 1e-12 {
        return (h, w);
    }
    let wide = w >= h;
    let (long, short) = if wide { (w, h) } else { (h, w) };
    if short <= 2.0 * sin * cos * long || (sin - cos).abs() < 1e-12 {
        // Half-constrained: two opposite corners touch the longer sides.
        let x = 0.5 * short;
        if wide {
            (x / sin, x / cos)
        } else {
            (x / cos, x / sin)
        }
    } else {
        // Fully constrained: all four corners touch the rotated sides.
        let cos2 = cos * cos - sin * sin;
        ((w * cos - h * sin) / cos2, (h * cos - w * sin) / cos2)
    }
}

/// Whole-pixel size of the straightened result: [`inscribed_size`]
/// rounded down, never below one pixel.
pub fn straightened_size(width: u32, height: u32, degrees: f64) -> (u32, u32) {
    let (w, h) = inscribed_size(width, height, degrees);
    // Floating-point error must not cost a pixel on exact sizes.
    let side = |v: f64| {
        let whole = if (v - v.round()).abs() < 1e-6 { v.round() } else { v.floor() };
        (whole.max(1.0) as u32).min(width.max(height))
    };
    (side(w), side(h))
}

/// Rotate `src` by `degrees` (positive = clockwise) and crop to the
/// largest axis-aligned rectangle inside, so no empty corner is left.
/// Bilinear sampling, clamped to the source edge; the sample type and
/// channel layout of the source are kept.
pub fn straighten(src: &image::DynamicImage, degrees: f64) -> image::DynamicImage {
    use image::DynamicImage as D;
    if !degrees.is_finite() || degrees.abs() < 1e-9 || src.width() == 0 || src.height() == 0 {
        return src.clone();
    }
    let size = straightened_size(src.width(), src.height(), degrees);
    match src {
        D::ImageRgb32F(_) | D::ImageRgba32F(_) => {
            let out = D::ImageRgba32F(rotate_buffer(&src.to_rgba32f(), degrees, size));
            if src.color().has_alpha() {
                out
            } else {
                D::ImageRgb32F(out.to_rgb32f())
            }
        }
        D::ImageLuma16(_) | D::ImageLumaA16(_) | D::ImageRgb16(_) | D::ImageRgba16(_) => {
            let out = D::ImageRgba16(rotate_buffer(&src.to_rgba16(), degrees, size));
            match src {
                D::ImageLuma16(_) => D::ImageLuma16(out.to_luma16()),
                D::ImageLumaA16(_) => D::ImageLumaA16(out.to_luma_alpha16()),
                D::ImageRgb16(_) => D::ImageRgb16(out.to_rgb16()),
                _ => out,
            }
        }
        _ => {
            let out = D::ImageRgba8(rotate_buffer(&src.to_rgba8(), degrees, size));
            match src {
                D::ImageLuma8(_) => D::ImageLuma8(out.to_luma8()),
                D::ImageLumaA8(_) => D::ImageLumaA8(out.to_luma_alpha8()),
                D::ImageRgb8(_) => D::ImageRgb8(out.to_rgb8()),
                _ => out,
            }
        }
    }
}

/// A sample type [`straighten`] can interpolate.
trait Sample: Copy {
    fn to_f32(self) -> f32;
    fn from_f32(value: f32) -> Self;
}

impl Sample for u8 {
    fn to_f32(self) -> f32 {
        f32::from(self)
    }
    fn from_f32(value: f32) -> Self {
        value.round().clamp(0.0, 255.0) as u8
    }
}

impl Sample for u16 {
    fn to_f32(self) -> f32 {
        f32::from(self)
    }
    fn from_f32(value: f32) -> Self {
        value.round().clamp(0.0, 65535.0) as u16
    }
}

impl Sample for f32 {
    fn to_f32(self) -> f32 {
        self
    }
    fn from_f32(value: f32) -> Self {
        value
    }
}

/// RGBA samples of a `w × h` image rotated and cropped to `out_w × out_h`.
fn rotate_raw<T: Sample>(
    src: &[T],
    (w, h): (u32, u32),
    degrees: f64,
    (out_w, out_h): (u32, u32),
) -> Vec<T> {
    let (sin, cos) = degrees.to_radians().sin_cos();
    let (cx, cy) = (f64::from(w) / 2.0, f64::from(h) / 2.0);
    let (ox, oy) = (f64::from(out_w) / 2.0, f64::from(out_h) / 2.0);
    let (max_x, max_y) = (f64::from(w) - 1.0, f64::from(h) - 1.0);
    let at = |x: u32, y: u32| (y as usize * w as usize + x as usize) * 4;
    let mut out = Vec::with_capacity(out_w as usize * out_h as usize * 4);
    for y in 0..out_h {
        for x in 0..out_w {
            // Centre of the output pixel, rotated back into the source.
            let dx = f64::from(x) + 0.5 - ox;
            let dy = f64::from(y) + 0.5 - oy;
            let sx = (cos * dx + sin * dy + cx - 0.5).clamp(0.0, max_x);
            let sy = (-sin * dx + cos * dy + cy - 0.5).clamp(0.0, max_y);
            let (x0, y0) = (sx.floor() as u32, sy.floor() as u32);
            let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
            let (fx, fy) = ((sx - f64::from(x0)) as f32, (sy - f64::from(y0)) as f32);
            let (a, b, c, d) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
            for i in 0..4 {
                let top = src[a + i].to_f32() * (1.0 - fx) + src[b + i].to_f32() * fx;
                let bottom = src[c + i].to_f32() * (1.0 - fx) + src[d + i].to_f32() * fx;
                out.push(T::from_f32(top * (1.0 - fy) + bottom * fy));
            }
        }
    }
    out
}

fn rotate_buffer<T: Sample>(
    src: &image::ImageBuffer<image::Rgba<T>, Vec<T>>,
    degrees: f64,
    size: (u32, u32),
) -> image::ImageBuffer<image::Rgba<T>, Vec<T>>
where
    image::Rgba<T>: image::Pixel<Subpixel = T>,
{
    let raw = rotate_raw(src.as_raw(), src.dimensions(), degrees, size);
    image::ImageBuffer::from_raw(size.0, size.1, raw).expect("buffer sized for the output")
}

#[cfg(test)]
mod straighten_tests {
    use super::*;

    #[test]
    fn inscribed_rectangle_matches_known_cases() {
        assert_eq!(inscribed_size(400, 300, 0.0), (400.0, 300.0));
        let (w, h) = inscribed_size(100, 100, 45.0);
        assert!((w - 70.710678).abs() < 1e-4 && (h - 70.710678).abs() < 1e-4, "{w}x{h}");
        // Symmetric in the sign of the angle.
        assert_eq!(inscribed_size(640, 480, 7.5), inscribed_size(640, 480, -7.5));
        assert_eq!(inscribed_size(0, 10, 5.0), (0.0, 0.0));
        assert_eq!(straightened_size(10, 10, 0.0), (10, 10));
        assert_eq!(straightened_size(3, 1, 45.0), (1, 1));
    }

    /// Every corner of the inscribed rectangle lies inside the rotated
    /// source, and the rectangle shrinks as the angle grows.
    #[test]
    fn inscribed_rectangle_fits_inside_the_rotated_source() {
        for (width, height) in [(640u32, 480u32), (480, 640), (1000, 100), (100, 1000), (50, 50)] {
            let mut previous = f64::INFINITY;
            for tenths in 1..=450 {
                let degrees = f64::from(tenths) / 10.0;
                let (w, h) = inscribed_size(width, height, degrees);
                assert!(w > 0.0 && h > 0.0);
                assert!(w * h <= previous + 1e-6, "{width}x{height} grew at {degrees}");
                previous = w * h;
                let (sin, cos) = degrees.to_radians().sin_cos();
                for (px, py) in [(w, h), (-w, h), (w, -h), (-w, -h)] {
                    // Corner rotated back into source coordinates.
                    let (x, y) = (px / 2.0, py / 2.0);
                    let sx = cos * x + sin * y;
                    let sy = -sin * x + cos * y;
                    let slack = 1e-6;
                    assert!(
                        sx.abs() <= f64::from(width) / 2.0 + slack
                            && sy.abs() <= f64::from(height) / 2.0 + slack,
                        "{width}x{height} @ {degrees}: corner outside"
                    );
                }
            }
        }
    }

    #[test]
    fn straighten_leaves_no_empty_corners_and_keeps_the_type() {
        let white = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            200,
            120,
            image::Rgb([255, 255, 255]),
        ));
        for degrees in [-45.0, -12.3, 0.1, 3.0, 30.0] {
            let out = straighten(&white, degrees);
            assert_eq!(out.color(), image::ColorType::Rgb8);
            assert_eq!(
                (out.width(), out.height()),
                straightened_size(200, 120, degrees),
                "{degrees}"
            );
            assert!(out.to_rgb8().pixels().all(|p| p.0 == [255, 255, 255]), "{degrees}");
        }
        let alpha = image::DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
            40,
            40,
            image::Rgba([1000u16, 2000, 3000, 65535]),
        ));
        let out = straighten(&alpha, 10.0);
        assert_eq!(out.color(), image::ColorType::Rgba16);
        assert!(out.to_rgba16().pixels().all(|p| p.0 == [1000, 2000, 3000, 65535]));
        assert_eq!(straighten(&white, 0.0), white);
    }

    #[test]
    fn positive_angles_turn_clockwise() {
        // A dark dot right of centre on a light canvas: turned clockwise
        // it moves down.
        let mut img = image::GrayImage::from_pixel(101, 101, image::Luma([255]));
        for y in 48..53 {
            for x in 78..83 {
                img.put_pixel(x, y, image::Luma([0]));
            }
        }
        let out = straighten(&image::DynamicImage::ImageLuma8(img), 20.0).to_luma8();
        let (w, h) = out.dimensions();
        let darkest = out
            .enumerate_pixels()
            .min_by_key(|(_, _, p)| p.0[0])
            .map(|(x, y, _)| (x as i64 - w as i64 / 2, y as i64 - h as i64 / 2))
            .unwrap();
        assert!(darkest.0 > 0 && darkest.1 > 5, "dot at {darkest:?}");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rotate_and_flip_keep_sample_type() {
        use image::DynamicImage;
        let images = [
            DynamicImage::ImageLuma8(image::ImageBuffer::from_pixel(3, 2, image::Luma([7u8]))),
            DynamicImage::ImageLuma16(image::ImageBuffer::from_pixel(3, 2, image::Luma([4321u16]))),
            DynamicImage::ImageRgb16(image::ImageBuffer::from_pixel(
                3,
                2,
                image::Rgb([1u16, 2, 60000]),
            )),
            DynamicImage::ImageRgba32F(image::ImageBuffer::from_pixel(
                3,
                2,
                image::Rgba([0.25f32, 2.0, 0.0, 0.5]),
            )),
        ];
        for img in images {
            for rotation in
                [super::Rotation::Deg90, super::Rotation::Deg180, super::Rotation::Deg270]
            {
                let out = super::apply_to(&img, rotation);
                assert_eq!(out.color(), img.color(), "{rotation:?}");
                assert_eq!(out.as_bytes()[..6], img.as_bytes()[..6]);
            }
            for axis in [crate::FlipAxis::Horizontal, crate::FlipAxis::Vertical] {
                let out = crate::flip::apply_to(&img, axis);
                assert_eq!(out.color(), img.color(), "{axis:?}");
                assert_eq!(out.as_bytes()[..6], img.as_bytes()[..6]);
            }
        }
    }

    use super::*;
    use image::{Rgb, RgbImage};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn write_fixture(dir: &Path, name: &str, w: u32, h: u32) -> PathBuf {
        let path = dir.join(name);
        let mut img = RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = Rgb([(x % 256) as u8, (y % 256) as u8, 0]);
        }
        img.save(&path).unwrap();
        path
    }

    fn dims(p: &Path) -> (u32, u32) {
        let img = image::open(p).unwrap();
        (img.width(), img.height())
    }

    #[test]
    fn rotate_90_swaps_dimensions() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "r.png", 40, 20);
        let out = match rotate_file(&src, Rotation::Deg90, None, OverwritePolicy::Replace).unwrap()
        {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        assert_eq!(out, dir.path().join("r_rot90.png"));
        assert_eq!(dims(&out), (20, 40));
    }

    #[test]
    fn rotate_180_preserves_dimensions() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "r.png", 40, 20);
        let out = match rotate_file(&src, Rotation::Deg180, None, OverwritePolicy::Replace).unwrap()
        {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        assert_eq!(dims(&out), (40, 20));
    }

    #[test]
    fn rotate_270_swaps_dimensions() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "r.png", 40, 20);
        let out = match rotate_file(&src, Rotation::Deg270, None, OverwritePolicy::Replace).unwrap()
        {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        assert_eq!(dims(&out), (20, 40));
    }

    #[test]
    fn exif_orientation_dispatches_correctly() {
        let src = image::DynamicImage::new_rgb8(40, 20);
        let wh = |img: &image::DynamicImage| (img.width(), img.height());
        // Case 1: identity
        assert_eq!(wh(&apply_exif_orientation(src.clone(), 1)), (40, 20));
        // Case 3: 180° keeps dims
        assert_eq!(wh(&apply_exif_orientation(src.clone(), 3)), (40, 20));
        // Case 6: 90° swaps dims
        assert_eq!(wh(&apply_exif_orientation(src.clone(), 6)), (20, 40));
        // Case 8: 270° swaps dims
        assert_eq!(wh(&apply_exif_orientation(src.clone(), 8)), (20, 40));
        // Unknown → pass-through
        assert_eq!(wh(&apply_exif_orientation(src.clone(), 99)), (40, 20));
    }

    #[test]
    fn rotate_auto_on_png_without_exif_is_passthrough() {
        // PNG saved via the image crate carries no EXIF → orientation is
        // None → rotate_file_auto still re-encodes to the `_auto` suffix
        // but dimensions match.
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "p.png", 40, 20);
        let out = match rotate_file_auto(&src, None, OverwritePolicy::Replace).unwrap() {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        assert_eq!(out, dir.path().join("p_auto.png"));
        assert_eq!(dims(&out), (40, 20));
    }

    #[test]
    fn auto_orient_bakes_exif_orientation_on_central_decode() {
        // Craft a JPEG carrying EXIF Orientation=6 (90° CW) by splicing a
        // minimal APP1 segment right after SOI, then confirm the central
        // decode returns the *upright* image (landscape 40×20 → 20×40) — and
        // that rotate_file_auto does NOT rotate a second time.
        let dir = TempDir::new().unwrap();
        let base = write_fixture(dir.path(), "o.jpg", 40, 20);
        let bytes = std::fs::read(&base).unwrap();
        // EXIF APP1: little-endian TIFF, single IFD0 entry Orientation=6.
        let app1: &[u8] = &[
            0xFF, 0xE1, 0x00, 0x22, // APP1 marker + length(34)
            b'E', b'x', b'i', b'f', 0x00, 0x00, // "Exif\0\0"
            b'I', b'I', 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00, // TIFF header, IFD0 @ 8
            0x01, 0x00, // entry count = 1
            0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00,
            0x00, // tag=Orientation type=SHORT count=1 value=6
            0x00, 0x00, 0x00, 0x00, // next IFD = none
        ];
        let mut spliced = Vec::with_capacity(bytes.len() + app1.len());
        spliced.extend_from_slice(&bytes[..2]); // SOI
        spliced.extend_from_slice(app1);
        spliced.extend_from_slice(&bytes[2..]);
        let p = dir.path().join("oriented.jpg");
        std::fs::write(&p, &spliced).unwrap();

        // The tag is readable…
        assert_eq!(crate::metadata::read(&p).unwrap().orientation, Some(6));
        // …and the central decode returns it upright (dims swapped once).
        let (img, _) = crate::pipeline::decode_with_source_format(&p).unwrap();
        assert_eq!((img.width(), img.height()), (20, 40), "expected single 90° upright");
        // rotate_file_auto must not double-rotate: output stays 20×40.
        let out = match rotate_file_auto(&p, Some(Format::Png), OverwritePolicy::Replace).unwrap() {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        assert_eq!(dims(&out), (20, 40), "double-rotation regression");
    }

    #[test]
    fn rotate_can_change_format() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "r.png", 10, 20);
        let out =
            match rotate_file(&src, Rotation::Deg90, Some(Format::Jpeg), OverwritePolicy::Replace)
                .unwrap()
            {
                ConvertOutcome::Written { output } => output,
                _ => panic!("expected write"),
            };
        assert_eq!(out.extension().unwrap(), "jpg");
    }
}
