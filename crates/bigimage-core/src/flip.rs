// SPDX-License-Identifier: GPL-3.0-or-later
//! Horizontal / vertical mirror pipeline.

use std::path::Path;

use image::imageops;

use crate::convert::{ConvertOutcome, OverwritePolicy};
use crate::{pipeline, Format, Result};

/// Mirror axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlipAxis {
    /// Mirror left ↔ right.
    Horizontal,
    /// Mirror top ↔ bottom.
    Vertical,
}

impl FlipAxis {
    fn filename_suffix(&self) -> &'static str {
        match self {
            FlipAxis::Horizontal => "_flipH",
            FlipAxis::Vertical => "_flipV",
        }
    }
}

/// Flip a single file along the given axis.
pub fn flip_file(
    input: impl AsRef<Path>,
    axis: FlipAxis,
    target: Option<Format>,
    policy: OverwritePolicy,
) -> Result<ConvertOutcome> {
    let input = input.as_ref();

    let (src, src_format) = pipeline::decode_with_source_format(input)?;
    let target = target.unwrap_or(src_format);

    let output = pipeline::resolve_output(input, Some(axis.filename_suffix()), target, policy)?;

    if matches!(policy, OverwritePolicy::Skip) && output.exists() {
        tracing::debug!(?output, "flip: skipping existing output");
        return Ok(ConvertOutcome::Skipped { output });
    }

    tracing::debug!(?input, ?output, ?target, ?axis, "flip: computing");
    let flipped = apply_to(&src, axis);
    pipeline::encode_and_cleanup(flipped, output, target, &crate::EncodeOptions::default(), policy)
}

/// Pure in-memory flip — shared with preview pipeline.
pub fn apply_to(src: &image::DynamicImage, axis: FlipAxis) -> image::DynamicImage {
    match axis {
        FlipAxis::Horizontal => imageops::flip_horizontal(src).into(),
        FlipAxis::Vertical => imageops::flip_vertical(src).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, Rgb, RgbImage};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn write_fixture(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let mut img = RgbImage::new(4, 2);
        // Column-coloured so flips are visually distinguishable.
        img.put_pixel(0, 0, Rgb([255, 0, 0]));
        img.put_pixel(1, 0, Rgb([0, 255, 0]));
        img.put_pixel(2, 0, Rgb([0, 0, 255]));
        img.put_pixel(3, 0, Rgb([255, 255, 0]));
        img.put_pixel(0, 1, Rgb([0, 0, 0]));
        img.put_pixel(1, 1, Rgb([0, 0, 0]));
        img.put_pixel(2, 1, Rgb([0, 0, 0]));
        img.put_pixel(3, 1, Rgb([0, 0, 0]));
        img.save(&path).unwrap();
        path
    }

    #[test]
    fn horizontal_flip_mirrors_columns() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "h.png");
        let out =
            match flip_file(&src, FlipAxis::Horizontal, None, OverwritePolicy::Replace).unwrap() {
                ConvertOutcome::Written { output } => output,
                _ => panic!("expected write"),
            };
        let img = image::open(&out).unwrap();
        // Original row 0 was [255,0,0][0,255,0][0,0,255][255,255,0];
        // flipped horizontally: [255,255,0][0,0,255][0,255,0][255,0,0].
        assert_eq!(img.get_pixel(0, 0).0[..3], [255, 255, 0]);
        assert_eq!(img.get_pixel(3, 0).0[..3], [255, 0, 0]);
    }

    #[test]
    fn vertical_flip_swaps_rows() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "v.png");
        let out = match flip_file(&src, FlipAxis::Vertical, None, OverwritePolicy::Replace).unwrap()
        {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        let img = image::open(&out).unwrap();
        // Row 0 (coloured) should now be at y=1, and row 1 (black) at y=0.
        assert_eq!(img.get_pixel(0, 0).0[..3], [0, 0, 0]);
        assert_eq!(img.get_pixel(0, 1).0[..3], [255, 0, 0]);
    }

    #[test]
    fn suffix_picks_distinct_filenames() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "s.png");
        let h = flip_file(&src, FlipAxis::Horizontal, None, OverwritePolicy::Replace).unwrap();
        let v = flip_file(&src, FlipAxis::Vertical, None, OverwritePolicy::Replace).unwrap();
        assert_ne!(h.path(), v.path());
        assert!(h.path().to_string_lossy().contains("flipH"));
        assert!(v.path().to_string_lossy().contains("flipV"));
    }
}
