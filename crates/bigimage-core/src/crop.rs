// SPDX-License-Identifier: GPL-3.0-or-later
//! Rectangular crop pipeline.

use std::path::Path;

use crate::convert::{ConvertOutcome, OverwritePolicy};
use crate::{pipeline, BigImageError, Format, Result};

/// Crop window expressed in pixels from the top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CropRect {
    /// Left edge, pixels from x=0.
    pub x: u32,
    /// Top edge, pixels from y=0.
    pub y: u32,
    /// Width of the window.
    pub width: u32,
    /// Height of the window.
    pub height: u32,
}

impl CropRect {
    /// Convenience constructor.
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    fn filename_suffix(&self) -> String {
        // ImageMagick-style geometry: WxH+X+Y.
        format!("_crop-{}x{}+{}+{}", self.width, self.height, self.x, self.y)
    }

    fn validate_against(&self, img_w: u32, img_h: u32) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(BigImageError::InvalidInput(format!(
                "recorte com dimensão zero: {}x{}",
                self.width, self.height
            )));
        }
        let x_end = self.x.checked_add(self.width).ok_or_else(|| {
            BigImageError::InvalidInput(format!(
                "recorte estoura u32 em x: {} + {}",
                self.x, self.width
            ))
        })?;
        let y_end = self.y.checked_add(self.height).ok_or_else(|| {
            BigImageError::InvalidInput(format!(
                "recorte estoura u32 em y: {} + {}",
                self.y, self.height
            ))
        })?;
        if x_end > img_w || y_end > img_h {
            return Err(BigImageError::InvalidInput(format!(
                "recorte ({}x{}+{}+{}) fora da imagem ({}x{})",
                self.width, self.height, self.x, self.y, img_w, img_h
            )));
        }
        Ok(())
    }
}

/// Crop a rectangular window from a file.
pub fn crop_file(
    input: impl AsRef<Path>,
    rect: CropRect,
    target: Option<Format>,
    policy: OverwritePolicy,
) -> Result<ConvertOutcome> {
    let input = input.as_ref();

    let (src, src_format) = pipeline::decode_with_source_format(input)?;
    let target = target.unwrap_or(src_format);

    rect.validate_against(src.width(), src.height())?;

    let output = pipeline::resolve_output(input, Some(&rect.filename_suffix()), target, policy)?;

    if matches!(policy, OverwritePolicy::Skip) && output.exists() {
        tracing::debug!(?output, "crop: skipping existing output");
        return Ok(ConvertOutcome::Skipped { output });
    }

    tracing::debug!(?input, ?output, ?target, ?rect, "crop: computing");
    let cropped = apply_to(&src, rect)?;

    pipeline::encode_and_cleanup(cropped, output, target, &crate::EncodeOptions::default(), policy)
}

/// Pure in-memory crop — validates `rect` against the image then returns a
/// cropped copy. Shared with the preview pipeline in `crate::preview`.
pub fn apply_to(src: &image::DynamicImage, rect: CropRect) -> Result<image::DynamicImage> {
    rect.validate_against(src.width(), src.height())?;
    Ok(src.crop_imm(rect.x, rect.y, rect.width, rect.height))
}

#[cfg(test)]
mod tests {
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
    fn valid_crop_produces_subimage() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "c.png", 100, 100);
        let out =
            match crop_file(&src, CropRect::new(10, 10, 40, 20), None, OverwritePolicy::Replace)
                .unwrap()
            {
                ConvertOutcome::Written { output } => output,
                _ => panic!("expected write"),
            };
        assert_eq!(out, dir.path().join("c_crop-40x20+10+10.png"));
        assert_eq!(dims(&out), (40, 20));
    }

    #[test]
    fn crop_outside_image_is_rejected() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "c.png", 50, 50);
        let err = crop_file(&src, CropRect::new(40, 40, 20, 20), None, OverwritePolicy::Replace)
            .unwrap_err();
        assert!(matches!(err, BigImageError::InvalidInput(_)));
    }

    #[test]
    fn zero_sized_crop_is_rejected() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "c.png", 50, 50);
        let err = crop_file(&src, CropRect::new(0, 0, 0, 10), None, OverwritePolicy::Replace)
            .unwrap_err();
        assert!(matches!(err, BigImageError::InvalidInput(_)));
    }

    #[test]
    fn full_image_crop_is_identity_in_dims() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "c.png", 50, 50);
        let out = match crop_file(&src, CropRect::new(0, 0, 50, 50), None, OverwritePolicy::Replace)
            .unwrap()
        {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        assert_eq!(dims(&out), (50, 50));
    }
}
