// SPDX-License-Identifier: GPL-3.0-or-later
//! Single-file resize pipeline built on `fast_image_resize` (SIMD AVX2/NEON).
//!
//! Scope in this cut: mode-based resizing with the usual filter catalogue,
//! format-preserving by default. Cropping/fill and rotation land in their own
//! modules.

use std::path::Path;

use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
use image::DynamicImage;

use crate::convert::{ConvertOutcome, OverwritePolicy};
use crate::{pipeline, BigImageError, Format, Result};

/// How the target dimensions are computed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResizeMode {
    /// Longest edge is clamped to N pixels, aspect preserved. Images already
    /// below the cap are passed through unchanged.
    MaxEdge(u32),
    /// Scale both dimensions by this percentage (100.0 = identity).
    Percent(f32),
    /// Ignore aspect and force exactly `width × height`.
    Exact {
        /// Target width in pixels.
        width: u32,
        /// Target height in pixels.
        height: u32,
    },
    /// Fit inside `width × height` preserving aspect — one edge touches the
    /// bound, the other sits below it. No cropping.
    Fit {
        /// Bounding box width.
        width: u32,
        /// Bounding box height.
        height: u32,
    },
}

impl ResizeMode {
    fn filename_suffix(&self) -> String {
        match self {
            ResizeMode::MaxEdge(e) => format!("_{e}"),
            ResizeMode::Percent(p) => format!("_{p}pct"),
            ResizeMode::Exact { width, height } => format!("_{width}x{height}"),
            ResizeMode::Fit { width, height } => format!("_fit-{width}x{height}"),
        }
    }

    pub(crate) fn target_dims(&self, src_w: u32, src_h: u32) -> Result<(u32, u32)> {
        if src_w == 0 || src_h == 0 {
            return Err(BigImageError::InvalidInput(format!(
                "origem com dimensão zero: {src_w}x{src_h}"
            )));
        }

        match *self {
            ResizeMode::MaxEdge(edge) => {
                let edge = edge.max(1);
                if src_w <= edge && src_h <= edge {
                    return Ok((src_w, src_h));
                }
                if src_w >= src_h {
                    let new_w = edge;
                    let new_h = ((src_h as u64) * (new_w as u64) / (src_w as u64)) as u32;
                    Ok((new_w, new_h.max(1)))
                } else {
                    let new_h = edge;
                    let new_w = ((src_w as u64) * (new_h as u64) / (src_h as u64)) as u32;
                    Ok((new_w.max(1), new_h))
                }
            }
            ResizeMode::Percent(p) => {
                if !p.is_finite() || p <= 0.0 || p > 1000.0 {
                    return Err(BigImageError::InvalidInput(format!(
                        "percent fora do intervalo (0, 1000]: {p}"
                    )));
                }
                let f = f64::from(p) / 100.0;
                let w = ((src_w as f64) * f).round() as u32;
                let h = ((src_h as f64) * f).round() as u32;
                Ok((w.max(1), h.max(1)))
            }
            ResizeMode::Exact { width, height } => {
                if width == 0 || height == 0 {
                    return Err(BigImageError::InvalidInput(format!(
                        "destino com dimensão zero: {width}x{height}"
                    )));
                }
                Ok((width, height))
            }
            ResizeMode::Fit { width, height } => {
                if width == 0 || height == 0 {
                    return Err(BigImageError::InvalidInput(format!(
                        "caixa de encaixe com dimensão zero: {width}x{height}"
                    )));
                }
                let scale = f64::min(
                    f64::from(width) / f64::from(src_w),
                    f64::from(height) / f64::from(src_h),
                );
                let w = ((src_w as f64) * scale).round() as u32;
                let h = ((src_h as f64) * scale).round() as u32;
                Ok((w.max(1), h.max(1)))
            }
        }
    }
}

/// Filter kernel used when the mode actually changes dimensions. Mode with
/// identical source/target dimensions short-circuits and never touches this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    /// Nearest neighbour — fastest, pixel-art friendly, blocky for photos.
    Nearest,
    /// Bilinear — cheap, softens detail.
    Bilinear,
    /// Catmull-Rom cubic — sharper than Mitchell.
    CatmullRom,
    /// Mitchell-Netravali — smooth all-rounder.
    Mitchell,
    /// Lanczos3 — default, best perceptual quality for photos.
    #[default]
    Lanczos3,
}

impl Filter {
    fn to_alg(self) -> ResizeAlg {
        match self {
            Filter::Nearest => ResizeAlg::Nearest,
            Filter::Bilinear => ResizeAlg::Convolution(FilterType::Bilinear),
            Filter::CatmullRom => ResizeAlg::Convolution(FilterType::CatmullRom),
            Filter::Mitchell => ResizeAlg::Convolution(FilterType::Mitchell),
            Filter::Lanczos3 => ResizeAlg::Convolution(FilterType::Lanczos3),
        }
    }
}

/// Resize a single file.
///
/// `target = None` preserves the source encoding; the output file always
/// carries a suffix describing the resize (e.g. `foto_1080.jpg`) so we never
/// overwrite the original unless policy explicitly asks for it.
pub fn resize_file(
    input: impl AsRef<Path>,
    mode: ResizeMode,
    filter: Filter,
    target: Option<Format>,
    opts: &crate::EncodeOptions,
    policy: OverwritePolicy,
) -> Result<ConvertOutcome> {
    let input = input.as_ref();
    let suffix = mode.filename_suffix();

    let (src, src_format) = pipeline::decode_with_source_format(input)?;
    let target = target.unwrap_or(src_format);

    let output = pipeline::resolve_output(input, Some(&suffix), target, policy)?;

    if matches!(policy, OverwritePolicy::Skip) && output.exists() {
        tracing::debug!(?output, "resize: skipping existing output");
        return Ok(ConvertOutcome::Skipped { output });
    }

    tracing::debug!(?input, ?output, ?target, ?mode, ?filter, "resize: computing");
    let resized = apply_to(&src, mode, filter)?;
    pipeline::encode_and_cleanup(resized, output, target, opts, policy)
}

/// Pure in-memory variant — resizes an already-decoded image and returns
/// the result. Shared by [`resize_file`] and by the preview pipeline in
/// `crate::preview`.
pub fn apply_to(src: &DynamicImage, mode: ResizeMode, filter: Filter) -> Result<DynamicImage> {
    let (new_w, new_h) = mode.target_dims(src.width(), src.height())?;
    pipeline::check_dimensions(new_w, new_h, src.color().bytes_per_pixel())?;
    if (new_w, new_h) == (src.width(), src.height()) {
        return Ok(src.clone());
    }
    let mut dst = DynamicImage::new(new_w, new_h, src.color());
    let mut resizer = Resizer::new();
    let opts = ResizeOptions::new().resize_alg(filter.to_alg());
    resizer
        .resize(src, &mut dst, &opts)
        .map_err(|e| BigImageError::Other(format!("fast_image_resize: {e}")))?;
    Ok(dst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn write_fixture_png(dir: &Path, name: &str, w: u32, h: u32) -> PathBuf {
        let path = dir.join(name);
        let mut img = RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = Rgb([(x % 256) as u8, (y % 256) as u8, ((x ^ y) % 256) as u8]);
        }
        img.save(&path).unwrap();
        path
    }

    fn dims_of(p: &Path) -> (u32, u32) {
        let img = image::open(p).unwrap();
        (img.width(), img.height())
    }

    #[test]
    fn max_edge_clamps_longest_side() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "l.png", 400, 200);

        let out = match resize_file(
            &src,
            ResizeMode::MaxEdge(100),
            Filter::Lanczos3,
            None,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap()
        {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };

        assert_eq!(out, dir.path().join("l_100.png"));
        assert_eq!(dims_of(&out), (100, 50));
    }

    #[test]
    fn max_edge_passes_through_small_images() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "s.png", 32, 16);
        let out = match resize_file(
            &src,
            ResizeMode::MaxEdge(1080),
            Filter::Lanczos3,
            None,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap()
        {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        assert_eq!(dims_of(&out), (32, 16));
    }

    #[test]
    fn percent_100_is_identity() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "p.png", 40, 25);
        let out = match resize_file(
            &src,
            ResizeMode::Percent(100.0),
            Filter::Lanczos3,
            None,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap()
        {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        assert_eq!(dims_of(&out), (40, 25));
    }

    #[test]
    fn percent_50_halves_dimensions() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "h.png", 40, 20);
        let out = match resize_file(
            &src,
            ResizeMode::Percent(50.0),
            Filter::Lanczos3,
            None,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap()
        {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        assert_eq!(dims_of(&out), (20, 10));
    }

    #[test]
    fn exact_mode_ignores_aspect() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "e.png", 40, 40);
        let out = match resize_file(
            &src,
            ResizeMode::Exact { width: 30, height: 90 },
            Filter::Lanczos3,
            None,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap()
        {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        assert_eq!(dims_of(&out), (30, 90));
    }

    #[test]
    fn fit_preserves_aspect_inside_box() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "f.png", 400, 100);
        let out = match resize_file(
            &src,
            ResizeMode::Fit { width: 200, height: 200 },
            Filter::Lanczos3,
            None,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap()
        {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        // Width-bound case: scale 0.5 → 200x50.
        assert_eq!(dims_of(&out), (200, 50));
    }

    #[test]
    fn resize_changes_format_when_requested() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "c.png", 40, 20);
        let out = match resize_file(
            &src,
            ResizeMode::MaxEdge(10),
            Filter::Lanczos3,
            Some(Format::Jpeg),
            &crate::EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap()
        {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        assert_eq!(out.extension().unwrap(), "jpg");
        image::open(&out).unwrap();
    }

    #[test]
    fn skip_policy_when_suffixed_output_exists() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "k.png", 40, 20);
        let already = dir.path().join("k_10.png");
        std::fs::write(&already, b"sentinel").unwrap();

        let outcome = resize_file(
            &src,
            ResizeMode::MaxEdge(10),
            Filter::Lanczos3,
            None,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Skip,
        )
        .unwrap();
        assert!(matches!(outcome, ConvertOutcome::Skipped { .. }));
        assert_eq!(std::fs::read(&already).unwrap(), b"sentinel");
    }

    #[test]
    fn invalid_percent_is_rejected() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture_png(dir.path(), "x.png", 10, 10);
        let err = resize_file(
            &src,
            ResizeMode::Percent(0.0),
            Filter::Lanczos3,
            None,
            &crate::EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap_err();
        assert!(matches!(err, BigImageError::InvalidInput(_)));
    }
}
