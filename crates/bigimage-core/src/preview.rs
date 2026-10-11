// SPDX-License-Identifier: GPL-3.0-or-later
//! Preview pipeline — stateful session that decodes a file once, keeps a
//! downscaled thumbnail in memory, and re-applies arbitrary chains of
//! transformations to that thumbnail on demand. The viewer/dialog code
//! owns a [`PreviewSession`] per dialog and feeds it [`PreviewOp`]s as the
//! user drags sliders; each call returns a new `DynamicImage` that the GUI
//! can wrap in a `gdk::MemoryTexture` and show beside the controls.
//!
//! The whole point is to avoid two otherwise-unavoidable costs on every
//! slider tick: re-decoding the PNG/JPEG from disk, and operating on the
//! full-resolution pixel data when a 600px preview is what the user sees.
//! So we decode once, down-sample to a bounded thumbnail, and chain the
//! pure `apply_to` functions across all five transform modules.

use std::path::Path;

use fast_image_resize::{ResizeAlg, ResizeOptions, Resizer};
use image::DynamicImage;

use crate::{
    adjust, crop, extent, flip, pipeline, resize, rotate, AdjustOps, CropRect, ExtentOptions,
    ExtentSize, Filter, FlipAxis, Format, ResizeMode, Result, Rotation,
};

/// Max edge for the cached thumbnail. 800px keeps the whole preview image
/// comfortably under 2.5 MB in RAM (RGBA) while staying sharp on today's
/// 2K/4K displays when the dialog occupies half the screen.
const PREVIEW_MAX_EDGE: u32 = 800;

/// A single transformation we know how to render into the preview. The
/// convert path doesn't appear as its own variant because format-level
/// previews are handled at the GUI layer (colour rendering is the same,
/// file size is estimated separately).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PreviewOp {
    /// Brightness / contrast / saturation / gamma.
    Adjust(AdjustOps),
    /// Resize with a filter kernel (for preview we only care about the
    /// final dimensions relative to the thumbnail — Lanczos3 stays the
    /// quality baseline even for the 800px preview).
    Resize {
        /// How dimensions are derived (MaxEdge / Percent / Exact / Fit).
        mode: ResizeMode,
        /// Interpolation kernel used by `fast_image_resize`.
        filter: Filter,
    },
    /// Cardinal rotation.
    Rotate(Rotation),
    /// Horizontal / vertical mirror.
    Flip(FlipAxis),
    /// Rectangular crop — validated against the source's *natural* size,
    /// not the thumbnail (see [`PreviewSession::apply`] for the scaling).
    Crop(CropRect),
    /// Canvas extension with letterbox — destination size is derived
    /// against the source's *natural* dimensions and then scaled into the
    /// thumbnail space inside [`PreviewSession::apply`].
    Extent(ExtentOptions),
    /// No-op; useful when the GUI wants to show the original thumbnail.
    Identity,
}

/// Cached decoded image + downsampled thumbnail, built once per dialog.
#[derive(Debug, Clone)]
pub struct PreviewSession {
    /// Path of the source file — kept for error messages only.
    source_path: std::path::PathBuf,
    /// Natural pixel dimensions of the source (used to scale crop/resize
    /// parameters into the thumbnail space).
    natural_size: (u32, u32),
    /// The resolved source format, so dialogs can show it without
    /// poking at the path again.
    source_format: Format,
    /// Thumbnail bounded by [`PREVIEW_MAX_EDGE`]. Every call to
    /// [`apply`](Self::apply) starts from a clone of this.
    thumbnail: DynamicImage,
    /// Ratio of `thumbnail` width to `natural_size.0`. Needed to scale
    /// crop rectangles from the caller's "source pixel" coordinates into
    /// the thumbnail coordinates before applying the crop.
    thumbnail_scale: f64,
    has_gps: bool,
}

impl PreviewSession {
    /// Open a file, decode it, downsample to a preview thumbnail, and
    /// cache everything in memory.
    pub fn open(input: impl AsRef<Path>) -> Result<Self> {
        let input = input.as_ref();
        if let Some(backend) = crate::backend::configured() {
            return (backend.preview)(input);
        }
        let (src, source_format) = pipeline::decode_with_source_format(input)?;
        let natural_size = (src.width(), src.height());

        let thumbnail = downscale(&src, PREVIEW_MAX_EDGE)?;
        let thumbnail_scale = thumbnail.width() as f64 / natural_size.0.max(1) as f64;

        Ok(Self {
            source_path: input.to_path_buf(),
            natural_size,
            source_format,
            thumbnail,
            thumbnail_scale,
            has_gps: crate::metadata::has_gps(input),
        })
    }

    /// Reconstruct a validated preview returned by an isolated decoder.
    pub fn from_cached(
        input: std::path::PathBuf,
        natural_size: (u32, u32),
        source_format: Format,
        thumbnail: DynamicImage,
        has_gps: bool,
    ) -> Result<Self> {
        if natural_size.0 == 0
            || natural_size.1 == 0
            || thumbnail.width() == 0
            || thumbnail.height() == 0
            || thumbnail.width() > PREVIEW_MAX_EDGE
            || thumbnail.height() > PREVIEW_MAX_EDGE
        {
            return Err(crate::BigImageError::InvalidInput("preview inválido".into()));
        }
        Ok(Self {
            source_path: input,
            natural_size,
            source_format,
            thumbnail_scale: thumbnail.width() as f64 / natural_size.0 as f64,
            thumbnail,
            has_gps,
        })
    }

    /// The file's natural `(width, height)` in source pixels — useful for
    /// dialogs that display dimensions or compute "new size" hints.
    pub fn natural_size(&self) -> (u32, u32) {
        self.natural_size
    }

    /// Format detected at decode time.
    pub fn source_format(&self) -> Format {
        self.source_format
    }

    /// Path of the source file.
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    /// Apply `op` to the cached thumbnail and return the result. The
    /// thumbnail itself is never mutated, so callers can keep calling
    /// with different ops freely.
    pub fn apply(&self, op: &PreviewOp) -> Result<DynamicImage> {
        match op {
            PreviewOp::Identity => Ok(self.thumbnail.clone()),
            PreviewOp::Adjust(ops) => Ok(adjust::apply_to(self.thumbnail.clone(), *ops)),
            PreviewOp::Resize { mode, filter } => {
                // Resize targets scale with the thumbnail so a
                // "50% of source" preview genuinely looks like 50%.
                let (w, h) = mode.target_dims(self.thumbnail.width(), self.thumbnail.height())?;
                let (width, height) = bounded_size(w, h);
                resize::apply_to(&self.thumbnail, ResizeMode::Exact { width, height }, *filter)
            }
            PreviewOp::Rotate(r) => Ok(rotate::apply_to(&self.thumbnail, *r)),
            PreviewOp::Flip(a) => Ok(flip::apply_to(&self.thumbnail, *a)),
            PreviewOp::Crop(rect) => {
                let scaled = scale_rect(*rect, self.thumbnail_scale);
                crop::apply_to(&self.thumbnail, scaled)
            }
            PreviewOp::Extent(opts) => {
                // The thumbnail preserves the source aspect ratio, so an
                // `Aspect` extent resolved against the thumbnail picks the
                // same grow-axis and yields a visually identical letterbox —
                // and being extend-only it can never come out smaller than
                // the thumbnail. For an `Exact` target we scale it into
                // thumbnail space but clamp to the thumbnail's own size so
                // the canvas always contains it (independent rounding of the
                // canvas vs. the thumbnail used to drift a pixel below it and
                // make `resolve` reject the preview).
                let (tw, th) = (self.thumbnail.width(), self.thumbnail.height());
                let preview_opts = match opts.size {
                    ExtentSize::Aspect { .. } => *opts,
                    ExtentSize::Exact { width, height } => {
                        let pw = ((width as f64) * self.thumbnail_scale).round().max(f64::from(tw))
                            as u32;
                        let ph = ((height as f64) * self.thumbnail_scale).round().max(f64::from(th))
                            as u32;
                        ExtentOptions { size: ExtentSize::Exact { width: pw, height: ph }, ..*opts }
                    }
                };
                let (w, h) = preview_opts.size.resolve(tw, th)?;
                let (width, height) = bounded_size(w, h);
                let scale = f64::from(width) / f64::from(w);
                let src = resize::apply_to(
                    &self.thumbnail,
                    ResizeMode::Exact {
                        width: ((f64::from(tw) * scale).round() as u32).clamp(1, width),
                        height: ((f64::from(th) * scale).round() as u32).clamp(1, height),
                    },
                    Filter::Lanczos3,
                )?;
                extent::apply_to(
                    &src,
                    &ExtentOptions { size: ExtentSize::Exact { width, height }, ..preview_opts },
                )
            }
        }
    }

    /// How many bytes the cached thumbnail occupies if we upload it to the
    /// GPU as an RGBA texture. Useful to sanity-check memory on very
    /// large selections.
    pub fn thumbnail_rgba_bytes(&self) -> usize {
        (self.thumbnail.width() as usize) * (self.thumbnail.height() as usize) * 4
    }

    /// Expose the thumbnail directly — GUI uses this when no op has been
    /// chosen yet (initial render of the dialog).
    pub fn thumbnail(&self) -> &DynamicImage {
        &self.thumbnail
    }

    /// Whether the image has an alpha (transparency) channel. Callers use
    /// this to flag the "JPEG loses transparency" case in smart prompts.
    pub fn has_alpha_channel(&self) -> bool {
        use image::ColorType;
        matches!(
            self.thumbnail.color(),
            ColorType::La8
                | ColorType::La16
                | ColorType::Rgba8
                | ColorType::Rgba16
                | ColorType::Rgba32F
        )
    }

    /// Whether the source file has GPS coordinates in its EXIF. Used by
    /// the smart-prompt banner ("Esta imagem contém sua localização…")
    /// in the convert / resize dialogs. Cached during background preparation.
    pub fn source_has_gps(&self) -> bool {
        self.has_gps
    }
}

fn bounded_size(w: u32, h: u32) -> (u32, u32) {
    let scale = (f64::from(PREVIEW_MAX_EDGE) / f64::from(w.max(h))).min(1.0);
    (((f64::from(w) * scale).round() as u32).max(1), ((f64::from(h) * scale).round() as u32).max(1))
}

/// Resize `img` so the longest edge is ≤ `max_edge`; returns a clone when
/// the image already fits. Uses `fast_image_resize` with Lanczos3 so the
/// thumbnail is a faithful miniature, not a blocky nearest-neighbour
/// preview.
fn downscale(img: &DynamicImage, max_edge: u32) -> Result<DynamicImage> {
    let (w, h) = (img.width(), img.height());
    if w <= max_edge && h <= max_edge {
        return Ok(img.clone());
    }

    let (new_w, new_h) = if w >= h {
        let nw = max_edge;
        let nh = ((h as u64) * (nw as u64) / (w as u64)) as u32;
        (nw, nh.max(1))
    } else {
        let nh = max_edge;
        let nw = ((w as u64) * (nh as u64) / (h as u64)) as u32;
        (nw.max(1), nh)
    };

    let mut dst = DynamicImage::new(new_w, new_h, img.color());
    let mut resizer = Resizer::new();
    let opts = ResizeOptions::new()
        .resize_alg(ResizeAlg::Convolution(fast_image_resize::FilterType::Lanczos3));
    resizer
        .resize(img, &mut dst, &opts)
        .map_err(|e| crate::BigImageError::Other(format!("preview downscale: {e}")))?;
    Ok(dst)
}

fn scale_rect(rect: CropRect, ratio: f64) -> CropRect {
    CropRect {
        x: ((rect.x as f64) * ratio).round() as u32,
        y: ((rect.y as f64) * ratio).round() as u32,
        width: ((rect.width as f64) * ratio).round().max(1.0) as u32,
        height: ((rect.height as f64) * ratio).round().max(1.0) as u32,
    }
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
            *px = Rgb([(x % 256) as u8, (y % 256) as u8, ((x ^ y) % 256) as u8]);
        }
        img.save(&path).unwrap();
        path
    }

    #[test]
    fn session_caches_natural_size_and_format() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "s.png", 1600, 900);
        let sess = PreviewSession::open(&src).unwrap();
        assert_eq!(sess.natural_size(), (1600, 900));
        assert_eq!(sess.source_format(), Format::Png);
    }

    #[test]
    fn thumbnail_is_bounded_by_max_edge() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "big.png", 4000, 2000);
        let sess = PreviewSession::open(&src).unwrap();
        let (tw, th) = (sess.thumbnail().width(), sess.thumbnail().height());
        assert!(tw <= PREVIEW_MAX_EDGE);
        assert!(th <= PREVIEW_MAX_EDGE);
        assert_eq!(tw, PREVIEW_MAX_EDGE); // landscape → width hits cap
    }

    #[test]
    fn small_images_pass_through_thumbnail_untouched() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "tiny.png", 200, 100);
        let sess = PreviewSession::open(&src).unwrap();
        assert_eq!((sess.thumbnail().width(), sess.thumbnail().height()), (200, 100));
    }

    #[test]
    fn apply_adjust_changes_pixels() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "a.png", 400, 400);
        let sess = PreviewSession::open(&src).unwrap();
        let original = sess.thumbnail().clone();
        let bright = sess
            .apply(&PreviewOp::Adjust(AdjustOps { brightness: 50, ..AdjustOps::default() }))
            .unwrap();
        let a = original.to_rgba8();
        let b = bright.to_rgba8();
        // At least one non-saturated pixel must have grown.
        let bumped = a.pixels().zip(b.pixels()).any(|(pa, pb)| pb[0] > pa[0]);
        assert!(bumped, "brilho +50 nao alterou nenhum pixel");
    }

    #[test]
    fn apply_identity_is_equal_to_thumbnail() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "i.png", 500, 500);
        let sess = PreviewSession::open(&src).unwrap();
        let out = sess.apply(&PreviewOp::Identity).unwrap();
        assert_eq!(out.to_rgba8().as_raw(), sess.thumbnail().to_rgba8().as_raw());
    }

    #[test]
    fn rotate_90_swaps_dims_in_preview() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "r.png", 400, 200);
        let sess = PreviewSession::open(&src).unwrap();
        let rotated = sess.apply(&PreviewOp::Rotate(Rotation::Deg90)).unwrap();
        assert_eq!((rotated.width(), rotated.height()), (200, 400));
    }

    #[test]
    fn flip_horizontal_preserves_dims() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "f.png", 300, 200);
        let sess = PreviewSession::open(&src).unwrap();
        let flipped = sess.apply(&PreviewOp::Flip(FlipAxis::Horizontal)).unwrap();
        assert_eq!((flipped.width(), flipped.height()), (300, 200));
    }

    #[test]
    fn resize_percent_scales_thumbnail() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "rs.png", 400, 400);
        let sess = PreviewSession::open(&src).unwrap();
        let halved = sess
            .apply(&PreviewOp::Resize { mode: ResizeMode::Percent(50.0), filter: Filter::Lanczos3 })
            .unwrap();
        // 400x400 thumbnail at 50% → 200x200.
        assert_eq!((halved.width(), halved.height()), (200, 200));
    }

    #[test]
    fn extent_aspect_grows_preview_canvas() {
        // Source 800x400 (2:1, fits the cap so thumbnail == source).
        // Aspect 1:1 → final 800x800 → thumbnail-space preview 800x800.
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "e.png", 800, 400);
        let sess = PreviewSession::open(&src).unwrap();
        let opts = ExtentOptions {
            size: ExtentSize::Aspect { w: 1, h: 1 },
            background: [255, 0, 0, 0xFF],
            gravity: crate::Gravity::Center,
        };
        let out = sess.apply(&PreviewOp::Extent(opts)).unwrap();
        assert_eq!((out.width(), out.height()), (800, 800));
        // Top-row corner is bg (red), source sits centered vertically.
        let rgba = out.to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0).0, [255, 0, 0, 0xFF]);
    }

    #[test]
    fn crop_in_source_coords_scales_into_thumbnail() {
        // Source is 2000x1000 → thumbnail becomes 800x400 (scale 0.4).
        // A crop of 1000x500+500+250 in source coords should map to
        // 400x200+200+100 in thumbnail coords.
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "c.png", 2000, 1000);
        let sess = PreviewSession::open(&src).unwrap();
        let cropped = sess.apply(&PreviewOp::Crop(CropRect::new(500, 250, 1000, 500))).unwrap();
        assert_eq!((cropped.width(), cropped.height()), (400, 200));
    }
}
