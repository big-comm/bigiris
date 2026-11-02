// SPDX-License-Identifier: GPL-3.0-or-later
//! Tone / colour adjustments: brightness, contrast, saturation, gamma.
//!
//! Applied in that order — matching the mental model most photo editors
//! expose (exposure-ish first, then global contrast, then colour pull,
//! finally gamma for tone response). Everything works on the decoded
//! `DynamicImage`, reusing the shared decode/encode pipeline.

use std::path::Path;

use image::DynamicImage;

use crate::convert::{ConvertOutcome, OverwritePolicy};
use crate::{pipeline, BigImageError, Format, Result};

/// All four adjustments bundled. Any axis left at its neutral value is
/// skipped entirely to avoid needless pixel churn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdjustOps {
    /// Brightness in the range `-100..=100`; `0` = no change. Positive
    /// values add luma, negative values subtract.
    pub brightness: i32,
    /// Contrast in the range `-100.0..=100.0`; `0.0` = no change.
    pub contrast: f32,
    /// Saturation in the range `-100.0..=100.0`; `-100` = greyscale,
    /// `0` = no change, `100` = double saturation.
    pub saturation: f32,
    /// Gamma in the range `(0.1..=10.0)`; `1.0` = no change. Values below
    /// 1.0 brighten midtones, above 1.0 darken them.
    pub gamma: f32,
}

impl Default for AdjustOps {
    fn default() -> Self {
        Self { brightness: 0, contrast: 0.0, saturation: 0.0, gamma: 1.0 }
    }
}

impl AdjustOps {
    /// Short human-readable tag suitable for file suffixes, showing only
    /// non-neutral axes: `"b10"`, `"c-20_s40"`, `"g1.8"`, etc.
    pub fn suffix_tag(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.brightness != 0 {
            parts.push(format!("b{}", self.brightness));
        }
        if self.contrast.abs() > 0.01 {
            parts.push(format!("c{}", self.contrast.round() as i32));
        }
        if self.saturation.abs() > 0.01 {
            parts.push(format!("s{}", self.saturation.round() as i32));
        }
        if (self.gamma - 1.0).abs() > 0.001 {
            parts.push(format!("g{:.2}", self.gamma));
        }
        if parts.is_empty() {
            "adj".to_string()
        } else {
            parts.join("_")
        }
    }

    fn validate(&self) -> Result<()> {
        if !(-100..=100).contains(&self.brightness) {
            return Err(BigImageError::InvalidInput(format!(
                "brilho fora do intervalo [-100, 100]: {}",
                self.brightness
            )));
        }
        if !self.contrast.is_finite() || !(-100.0..=100.0).contains(&self.contrast) {
            return Err(BigImageError::InvalidInput(format!(
                "contraste fora do intervalo [-100, 100]: {}",
                self.contrast
            )));
        }
        if !self.saturation.is_finite() || !(-100.0..=100.0).contains(&self.saturation) {
            return Err(BigImageError::InvalidInput(format!(
                "saturação fora do intervalo [-100, 100]: {}",
                self.saturation
            )));
        }
        if !self.gamma.is_finite() || !(0.1..=10.0).contains(&self.gamma) {
            return Err(BigImageError::InvalidInput(format!(
                "gamma fora do intervalo (0.1, 10.0]: {}",
                self.gamma
            )));
        }
        Ok(())
    }

    fn is_identity(&self) -> bool {
        self.brightness == 0
            && self.contrast.abs() < 0.01
            && self.saturation.abs() < 0.01
            && (self.gamma - 1.0).abs() < 0.001
    }
}

/// Apply a chain of colour adjustments to `input` and write the result.
///
/// Output naming carries a compact tag from [`AdjustOps::suffix_tag`] so
/// independent tweaks don't step on each other: `foo_b10_c-20.png`.
pub fn adjust_file(
    input: impl AsRef<Path>,
    ops: AdjustOps,
    target: Option<Format>,
    policy: OverwritePolicy,
) -> Result<ConvertOutcome> {
    adjust_file_with_options(input, ops, target, &crate::EncodeOptions::default(), policy)
}

/// Like [`adjust_file`], with caller-supplied encoder options.
pub fn adjust_file_with_options(
    input: impl AsRef<Path>,
    ops: AdjustOps,
    target: Option<Format>,
    opts: &crate::EncodeOptions,
    policy: OverwritePolicy,
) -> Result<ConvertOutcome> {
    ops.validate()?;

    let input = input.as_ref();
    let suffix_tag = ops.suffix_tag();
    let suffix = format!("_{suffix_tag}");
    if let Some(skipped) =
        pipeline::skip_before_decode(input, &suffix, target, policy, pipeline::any_size)?
    {
        return Ok(skipped);
    }

    let (src, src_format) = pipeline::decode_with_source_format(input)?;
    let target = target.unwrap_or(src_format);

    let output = pipeline::resolve_output(input, Some(&suffix), target, policy)?;

    if pipeline::existing_output(&output, target, policy) {
        tracing::debug!(?output, "adjust: skipping existing output");
        return Ok(ConvertOutcome::Skipped { output });
    }

    tracing::debug!(?input, ?output, ?ops, "adjust: applying");
    let tweaked = if ops.is_identity() { src } else { apply_to(src, ops) };

    pipeline::encode_output(tweaked, output, target, opts, policy)
}

/// Pure in-memory variant of [`adjust_file`] — applies `ops` to an already-
/// decoded image and returns the result without touching disk. Used by the
/// preview pipeline (see `crate::preview`) to refresh thumbnails on every
/// slider change without reloading or re-encoding the file.
pub fn apply_to(img: DynamicImage, ops: AdjustOps) -> DynamicImage {
    if ops.is_identity() {
        return img;
    }
    apply(img, ops)
}

/// Apply every active axis in one pass over normalised samples, keeping the
/// source colour type (8/16-bit integer or float, gray or RGB, with or
/// without alpha). Integer samples are clamped to their range after each
/// step, like the former 8-bit pipeline did; float samples may exceed 1.0
/// (HDR) and are only kept non-negative where gamma needs it.
fn apply(img: DynamicImage, ops: AdjustOps) -> DynamicImage {
    let mut img = img;
    let tone = Tone::new(ops);
    match &mut img {
        DynamicImage::ImageLuma8(b) => process(b.as_mut(), 1, 1, u8_to, u8_from, true, &tone),
        DynamicImage::ImageLumaA8(b) => process(b.as_mut(), 2, 1, u8_to, u8_from, true, &tone),
        DynamicImage::ImageRgb8(b) => process(b.as_mut(), 3, 3, u8_to, u8_from, true, &tone),
        DynamicImage::ImageRgba8(b) => process(b.as_mut(), 4, 3, u8_to, u8_from, true, &tone),
        DynamicImage::ImageLuma16(b) => process(b.as_mut(), 1, 1, u16_to, u16_from, true, &tone),
        DynamicImage::ImageLumaA16(b) => process(b.as_mut(), 2, 1, u16_to, u16_from, true, &tone),
        DynamicImage::ImageRgb16(b) => process(b.as_mut(), 3, 3, u16_to, u16_from, true, &tone),
        DynamicImage::ImageRgba16(b) => process(b.as_mut(), 4, 3, u16_to, u16_from, true, &tone),
        DynamicImage::ImageRgb32F(b) => process(b.as_mut(), 3, 3, f32_id, f32_id, false, &tone),
        DynamicImage::ImageRgba32F(b) => process(b.as_mut(), 4, 3, f32_id, f32_id, false, &tone),
        // Future variants: fall back to 16-bit RGBA, which loses nothing
        // from any integer type the crate currently knows.
        other => {
            let mut rgba = other.to_rgba16();
            process(rgba.as_mut(), 4, 3, u16_to, u16_from, true, &tone);
            *other = DynamicImage::ImageRgba16(rgba);
        }
    }
    img
}

fn u8_to(v: u8) -> f32 {
    f32::from(v) / 255.0
}
fn u8_from(v: f32) -> u8 {
    (v * 255.0).round().clamp(0.0, 255.0) as u8
}
fn u16_to(v: u16) -> f32 {
    f32::from(v) / 65535.0
}
fn u16_from(v: f32) -> u16 {
    (v * 65535.0).round().clamp(0.0, 65535.0) as u16
}
fn f32_id(v: f32) -> f32 {
    v
}

/// Resolved adjustment parameters, in normalised `0.0..=1.0` units.
struct Tone {
    /// Additive offset; `brightness` is expressed in 1/255 steps so 8-bit
    /// results match the historical `brighten` behaviour, and the same
    /// visual shift applies to 16-bit and float images.
    offset: Option<f32>,
    /// Contrast multiplier around mid-grey (same curve as `image`'s
    /// `adjust_contrast`).
    contrast: Option<f32>,
    /// Distance-from-luma multiplier (`1 + s/100`).
    saturation: Option<f32>,
    /// Exponent; `output = input^gamma` (Levels convention).
    gamma: Option<f32>,
}

impl Tone {
    fn new(ops: AdjustOps) -> Self {
        Self {
            offset: (ops.brightness != 0).then(|| ops.brightness as f32 / 255.0),
            contrast: (ops.contrast.abs() > 0.01).then(|| ((100.0 + ops.contrast) / 100.0).powi(2)),
            saturation: (ops.saturation.abs() > 0.01).then(|| 1.0 + ops.saturation / 100.0),
            gamma: ((ops.gamma - 1.0).abs() > 0.001).then_some(ops.gamma),
        }
    }

    fn apply(&self, colour: &mut [f32], clamp: bool) {
        let limit = |v: f32| if clamp { v.clamp(0.0, 1.0) } else { v };
        if let Some(offset) = self.offset {
            colour.iter_mut().for_each(|v| *v = limit(*v + offset));
        }
        if let Some(factor) = self.contrast {
            colour.iter_mut().for_each(|v| *v = limit((*v - 0.5) * factor + 0.5));
        }
        if let (Some(factor), [r, g, b]) = (self.saturation, &mut *colour) {
            // Rec. 601 luma weights — cheap and visually decent.
            let luma = 0.299 * *r + 0.587 * *g + 0.114 * *b;
            for v in [r, g, b] {
                *v = limit(luma + (*v - luma) * factor);
            }
        }
        if let Some(gamma) = self.gamma {
            colour.iter_mut().for_each(|v| *v = limit(v.max(0.0).powf(gamma)));
        }
    }
}

/// Run `tone` over interleaved samples: `channels` per pixel, of which the
/// first `colour` are colour (the rest — alpha — passes through untouched).
fn process<S: Copy>(
    data: &mut [S],
    channels: usize,
    colour: usize,
    to: fn(S) -> f32,
    from: fn(f32) -> S,
    clamp: bool,
    tone: &Tone,
) {
    for pixel in data.chunks_exact_mut(channels) {
        let mut values = [0.0f32; 3];
        for (value, sample) in values.iter_mut().zip(pixel.iter()).take(colour) {
            *value = to(*sample);
        }
        tone.apply(&mut values[..colour], clamp);
        for (sample, value) in pixel.iter_mut().zip(values).take(colour) {
            *sample = from(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn write_fixture(dir: &std::path::Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let mut img = RgbImage::new(4, 4);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = Rgb([x as u8 * 60, y as u8 * 60, 120]);
        }
        img.save(&path).unwrap();
        path
    }

    #[test]
    fn identity_ops_round_trip_without_mutation() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "id.png");
        let out = match adjust_file(&src, AdjustOps::default(), None, OverwritePolicy::Replace)
            .unwrap()
        {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        // Identity adjust is a re-encode, not a byte-for-byte copy, but the
        // decoded pixels should match exactly.
        let a = image::open(&src).unwrap().to_rgba8();
        let b = image::open(&out).unwrap().to_rgba8();
        assert_eq!(a.dimensions(), b.dimensions());
        for (pa, pb) in a.pixels().zip(b.pixels()) {
            assert_eq!(pa, pb);
        }
    }

    #[test]
    fn brightness_bumps_all_channels() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "b.png");
        let ops = AdjustOps { brightness: 30, ..AdjustOps::default() };
        let out = match adjust_file(&src, ops, None, OverwritePolicy::Replace).unwrap() {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        let before = image::open(&src).unwrap().to_rgba8();
        let after = image::open(&out).unwrap().to_rgba8();
        // Every non-saturated channel must have grown.
        for (a, b) in before.pixels().zip(after.pixels()) {
            for c in 0..3 {
                if a[c] < 225 {
                    assert!(b[c] > a[c], "canal {c}: antes {}, depois {}", a[c], b[c]);
                }
            }
        }
    }

    #[test]
    fn saturation_neg100_produces_greyscale() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "s.png");
        let ops = AdjustOps { saturation: -100.0, ..AdjustOps::default() };
        let out = match adjust_file(&src, ops, None, OverwritePolicy::Replace).unwrap() {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        let img = image::open(&out).unwrap().to_rgba8();
        for px in img.pixels() {
            assert_eq!(px[0], px[1]);
            assert_eq!(px[1], px[2]);
        }
    }

    #[test]
    fn gamma_below_one_brightens_midtones() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "g.png");
        let ops = AdjustOps { gamma: 0.5, ..AdjustOps::default() };
        let out = match adjust_file(&src, ops, None, OverwritePolicy::Replace).unwrap() {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        let before = image::open(&src).unwrap().to_rgba8();
        let after = image::open(&out).unwrap().to_rgba8();
        // Mid-grey pixels should strictly brighten with gamma < 1.
        for (a, b) in before.pixels().zip(after.pixels()) {
            for c in 0..3 {
                if a[c] >= 20 && a[c] <= 220 {
                    assert!(b[c] > a[c], "canal {c}: {} -> {}", a[c], b[c]);
                }
            }
        }
    }

    #[test]
    fn sixteen_bit_and_float_sources_keep_depth_and_react_to_every_axis() {
        let rgb16 = DynamicImage::ImageRgb16(image::ImageBuffer::from_pixel(
            2,
            2,
            image::Rgb([20000u16, 30000, 40000]),
        ));
        let all = AdjustOps { brightness: 20, contrast: 10.0, saturation: 30.0, gamma: 0.8 };
        let out = apply_to(rgb16.clone(), all);
        assert_eq!(out.color(), image::ColorType::Rgb16);
        assert_ne!(
            out.as_rgb16().unwrap().get_pixel(0, 0),
            rgb16.as_rgb16().unwrap().get_pixel(0, 0)
        );

        // Brightness alone must be visible on 16-bit (it used to add 20 of
        // 65535) and match the 8-bit shift: +20/255 of full scale.
        let bright = apply_to(rgb16.clone(), AdjustOps { brightness: 20, ..AdjustOps::default() });
        let px = bright.as_rgb16().unwrap().get_pixel(0, 0).0;
        let expected = 20000 + (20.0f32 * 65535.0 / 255.0).round() as u16;
        assert!(px[0].abs_diff(expected) <= 1, "got {px:?}, expected {expected}");

        let gray16 = DynamicImage::ImageLumaA16(image::ImageBuffer::from_pixel(
            1,
            1,
            image::LumaA([30000u16, 1234]),
        ));
        let out =
            apply_to(gray16, AdjustOps { saturation: 50.0, gamma: 0.5, ..AdjustOps::default() });
        assert_eq!(out.color(), image::ColorType::La16);
        let px = out.as_luma_alpha16().unwrap().get_pixel(0, 0).0;
        assert!(px[0] > 30000);
        assert_eq!(px[1], 1234);

        let float = DynamicImage::ImageRgba32F(image::ImageBuffer::from_pixel(
            1,
            1,
            image::Rgba([0.25f32, 0.5, 0.75, 0.5]),
        ));
        let out = apply_to(float, AdjustOps { brightness: 51, ..AdjustOps::default() });
        assert_eq!(out.color(), image::ColorType::Rgba32F);
        let px = out.as_rgba32f().unwrap().get_pixel(0, 0).0;
        assert!((px[0] - 0.45).abs() < 1e-4 && (px[3] - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn adjust_file_writes_sixteen_bit_png_for_sixteen_bit_source() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("deep.png");
        image::ImageBuffer::from_pixel(3, 3, image::Rgb([20000u16, 30000, 40000]))
            .save(&src)
            .unwrap();
        let ops = AdjustOps { brightness: 10, saturation: -20.0, ..AdjustOps::default() };
        let out = adjust_file(&src, ops, None, OverwritePolicy::Replace).unwrap();
        let decoded = image::open(out.path()).unwrap();
        assert_eq!(decoded.color(), image::ColorType::Rgb16);
    }

    #[test]
    fn invalid_gamma_rejected() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "x.png");
        let ops = AdjustOps { gamma: 50.0, ..AdjustOps::default() }; // > 10
        let err = adjust_file(&src, ops, None, OverwritePolicy::Replace).unwrap_err();
        assert!(matches!(err, BigImageError::InvalidInput(_)));
    }

    #[test]
    fn suffix_tag_captures_active_axes() {
        let ops = AdjustOps { brightness: 10, contrast: -20.0, saturation: 0.0, gamma: 1.0 };
        assert_eq!(ops.suffix_tag(), "b10_c-20");

        let only_gamma = AdjustOps { brightness: 0, contrast: 0.0, saturation: 0.0, gamma: 1.8 };
        assert_eq!(only_gamma.suffix_tag(), "g1.80");

        let identity = AdjustOps::default();
        assert_eq!(identity.suffix_tag(), "adj");
    }
}
