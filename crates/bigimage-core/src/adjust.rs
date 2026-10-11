// SPDX-License-Identifier: GPL-3.0-or-later
//! Tone / colour adjustments: brightness, contrast, saturation, gamma.
//!
//! Applied in that order — matching the mental model most photo editors
//! expose (exposure-ish first, then global contrast, then colour pull,
//! finally gamma for tone response). Everything works on the decoded
//! `DynamicImage`, reusing the shared decode/encode pipeline.

use std::path::Path;

use image::{DynamicImage, GenericImageView, Rgba};

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
    ops.validate()?;

    let input = input.as_ref();
    let suffix_tag = ops.suffix_tag();
    let suffix = format!("_{suffix_tag}");

    let (src, src_format) = pipeline::decode_with_source_format(input)?;
    let target = target.unwrap_or(src_format);

    let output = pipeline::resolve_output(input, Some(&suffix), target, policy)?;

    if matches!(policy, OverwritePolicy::Skip) && output.exists() {
        tracing::debug!(?output, "adjust: skipping existing output");
        return Ok(ConvertOutcome::Skipped { output });
    }

    tracing::debug!(?input, ?output, ?ops, "adjust: applying");
    let tweaked = if ops.is_identity() { src } else { apply_to(src, ops) };

    pipeline::encode_and_cleanup(tweaked, output, target, &crate::EncodeOptions::default(), policy)
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

fn apply(img: DynamicImage, ops: AdjustOps) -> DynamicImage {
    let mut img = img;
    if ops.brightness != 0 {
        img = img.brighten(ops.brightness);
    }
    if ops.contrast.abs() > 0.01 {
        img = img.adjust_contrast(ops.contrast);
    }
    if ops.saturation.abs() > 0.01 {
        img = apply_saturation(img, ops.saturation);
    }
    if (ops.gamma - 1.0).abs() > 0.001 {
        img = apply_gamma(img, ops.gamma);
    }
    img
}

/// Linear blend between the input image and its luma projection.
///
/// `s = +100` pushes twice as far from grey (factor 2.0), `s = 0` leaves
/// the image untouched, `s = -100` collapses to full greyscale. Alpha is
/// always preserved; colours are clamped back into `u8`.
fn apply_saturation(img: DynamicImage, sat: f32) -> DynamicImage {
    let factor = 1.0 + sat / 100.0;
    let (w, h) = img.dimensions();
    let src = img.to_rgba8();
    let mut out = image::RgbaImage::new(w, h);
    for (x, y, px) in src.enumerate_pixels() {
        let r = px[0] as f32;
        let g = px[1] as f32;
        let b = px[2] as f32;
        // Rec. 601 luma weights — cheap and visually decent for 8-bit RGB.
        let luma = 0.299 * r + 0.587 * g + 0.114 * b;
        let nr = clamp_u8(luma + (r - luma) * factor);
        let ng = clamp_u8(luma + (g - luma) * factor);
        let nb = clamp_u8(luma + (b - luma) * factor);
        out.put_pixel(x, y, Rgba([nr, ng, nb, px[3]]));
    }
    DynamicImage::ImageRgba8(out)
}

/// Per-pixel gamma via a 256-entry LUT — fast enough for batch work and
/// exact for 8-bit RGB. Alpha is pass-through. Convention matches
/// Photoshop/GIMP's "Levels gamma": `output = input^gamma`, so `gamma < 1`
/// brightens midtones and `gamma > 1` darkens them.
fn apply_gamma(img: DynamicImage, gamma: f32) -> DynamicImage {
    let mut lut = [0u8; 256];
    for (i, slot) in lut.iter_mut().enumerate() {
        let normalized = (i as f32) / 255.0;
        let corrected = normalized.powf(gamma).clamp(0.0, 1.0);
        *slot = (corrected * 255.0).round() as u8;
    }
    let (w, h) = img.dimensions();
    let src = img.to_rgba8();
    let mut out = image::RgbaImage::new(w, h);
    for (x, y, px) in src.enumerate_pixels() {
        out.put_pixel(
            x,
            y,
            Rgba([lut[px[0] as usize], lut[px[1] as usize], lut[px[2] as usize], px[3]]),
        );
    }
    DynamicImage::ImageRgba8(out)
}

fn clamp_u8(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
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
