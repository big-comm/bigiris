// SPDX-License-Identifier: GPL-3.0-or-later
//! Lossy rotation pipeline (90 / 180 / 270 via `image::imageops`).
//!
//! Arbitrary-angle rotation (with bicubic interpolation) and truly lossless
//! JPEG DCT-level rotation are tracked as follow-up iterations — see PLAN.md.
//! Everything here re-encodes from scratch, which is correct for every format
//! we ship but can lose quality for JPEG sources; the viewer path will use
//! the lossless variant when it lands.

use std::path::Path;

use image::imageops;

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
    let input = input.as_ref();

    let (src, src_format) = pipeline::decode_with_source_format(input)?;
    let target = target.unwrap_or(src_format);

    let output = pipeline::resolve_output(input, Some(rotation.filename_suffix()), target, policy)?;

    if matches!(policy, OverwritePolicy::Skip) && output.exists() {
        tracing::debug!(?output, "rotate: skipping existing output");
        return Ok(ConvertOutcome::Skipped { output });
    }

    tracing::debug!(?input, ?output, ?target, ?rotation, "rotate: computing");
    let rotated = apply_to(&src, rotation);
    pipeline::encode_and_cleanup(rotated, output, target, &crate::EncodeOptions::default(), policy)
}

/// Pure in-memory rotation — shared with preview pipeline.
pub fn apply_to(src: &image::DynamicImage, rotation: Rotation) -> image::DynamicImage {
    match rotation {
        Rotation::Deg90 => imageops::rotate90(src).into(),
        Rotation::Deg180 => imageops::rotate180(src).into(),
        Rotation::Deg270 => imageops::rotate270(src).into(),
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
    let input = input.as_ref();

    // `src` arrives already upright — decode_with_source_format honours the
    // orientation tag for every op. Re-applying it here would double-rotate.
    let (src, src_format) = crate::pipeline::decode_with_source_format(input)?;
    let target = target.unwrap_or(src_format);

    let output = crate::pipeline::resolve_output(input, Some("_auto"), target, policy)?;
    if matches!(policy, OverwritePolicy::Skip) && output.exists() {
        return Ok(ConvertOutcome::Skipped { output });
    }

    tracing::debug!(?input, ?output, "rotate: bake EXIF orientation");
    pipeline::encode_and_cleanup(src, output, target, &crate::EncodeOptions::default(), policy)
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
