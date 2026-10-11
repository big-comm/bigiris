// SPDX-License-Identifier: GPL-3.0-or-later
//! Canvas extension pipeline — wraps the source in a larger canvas with
//! configurable background colour and gravity ("letterbox"). The dual of
//! `crop`: never removes pixels, only adds padding so the result fits a
//! target aspect ratio or explicit size.
//!
//! Driving use case is social-media posts (YouTube community 1:1, Instagram
//! feed 4:5, Stories 9:16) where the platform thumbnail forces a fixed
//! ratio. Cropping loses the corners; this lets the user keep everything
//! inside coloured bars.

use std::path::Path;

use image::{imageops, DynamicImage, ImageBuffer, Rgba};

use crate::convert::{ConvertOutcome, OverwritePolicy};
use crate::{pipeline, BigImageError, EncodeOptions, Format, Result};

/// Target size, either explicit or derived from an aspect ratio applied
/// to the source dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtentSize {
    /// Exact destination dimensions in pixels.
    Exact {
        /// Destination width.
        width: u32,
        /// Destination height.
        height: u32,
    },
    /// Aspect ratio numerator:denominator. The destination is the smallest
    /// canvas of that ratio that contains the source — i.e. *extend only*,
    /// never crop.
    Aspect {
        /// Numerator (width side of the ratio).
        w: u32,
        /// Denominator (height side of the ratio).
        h: u32,
    },
}

impl ExtentSize {
    /// Resolve to concrete `(width, height)` against a `(src_w, src_h)`.
    /// Validates that the resolved canvas can contain the source — extent
    /// is an extend-only operation; smaller targets are rejected so users
    /// don't silently lose pixels.
    pub fn resolve(self, src_w: u32, src_h: u32) -> Result<(u32, u32)> {
        if src_w == 0 || src_h == 0 {
            return Err(BigImageError::InvalidInput(format!(
                "extent: imagem origem com dimensão zero ({src_w}x{src_h})"
            )));
        }
        match self {
            ExtentSize::Exact { width, height } => {
                if width == 0 || height == 0 {
                    return Err(BigImageError::InvalidInput(format!(
                        "extent: tamanho destino com dimensão zero ({width}x{height})"
                    )));
                }
                if width < src_w || height < src_h {
                    return Err(BigImageError::InvalidInput(format!(
                        "extent: destino ({width}x{height}) menor que origem ({src_w}x{src_h}); \
                         a operação só estende a tela, use `crop` para recortar"
                    )));
                }
                Ok((width, height))
            }
            ExtentSize::Aspect { w, h } => {
                if w == 0 || h == 0 {
                    return Err(BigImageError::InvalidInput(format!(
                        "extent: razão com componente zero ({w}:{h})"
                    )));
                }
                // Compare src_w/src_h against w/h via cross product to stay
                // in integer space (avoids float rounding for clean ratios).
                let lhs = u64::from(src_w) * u64::from(h);
                let rhs = u64::from(src_h) * u64::from(w);
                if lhs >= rhs {
                    // Source is at least as wide as the target ratio: keep
                    // width, grow height. lhs/w gives src_w*h/w; cast OK
                    // because the quotient is bounded by src_h * (lhs/rhs)
                    // which fits u32 for any sane image.
                    let new_h_u64 = lhs / u64::from(w);
                    let new_h = u32::try_from(new_h_u64).map_err(|_| {
                        BigImageError::InvalidInput(format!(
                            "extent: altura calculada excede u32 ({new_h_u64})"
                        ))
                    })?;
                    Ok((src_w, new_h.max(src_h)))
                } else {
                    let new_w_u64 = rhs / u64::from(h);
                    let new_w = u32::try_from(new_w_u64).map_err(|_| {
                        BigImageError::InvalidInput(format!(
                            "extent: largura calculada excede u32 ({new_w_u64})"
                        ))
                    })?;
                    Ok((new_w.max(src_w), src_h))
                }
            }
        }
    }
}

/// Where the source sits on the extended canvas. Same nine positions
/// ImageMagick exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Gravity {
    /// Top-left corner.
    NorthWest,
    /// Centered horizontally, top edge.
    North,
    /// Top-right corner.
    NorthEast,
    /// Left edge, centered vertically.
    West,
    /// Centered both ways.
    #[default]
    Center,
    /// Right edge, centered vertically.
    East,
    /// Bottom-left corner.
    SouthWest,
    /// Centered horizontally, bottom edge.
    South,
    /// Bottom-right corner.
    SouthEast,
}

impl Gravity {
    /// Top-left offset of the source `(src_w x src_h)` on a destination
    /// `(dst_w x dst_h)` canvas. Assumes dst >= src on both axes (caller
    /// must validate via [`ExtentSize::resolve`]).
    fn place(self, src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> (u32, u32) {
        let dx_center = dst_w.saturating_sub(src_w) / 2;
        let dy_center = dst_h.saturating_sub(src_h) / 2;
        let dx_right = dst_w.saturating_sub(src_w);
        let dy_bottom = dst_h.saturating_sub(src_h);
        match self {
            Gravity::NorthWest => (0, 0),
            Gravity::North => (dx_center, 0),
            Gravity::NorthEast => (dx_right, 0),
            Gravity::West => (0, dy_center),
            Gravity::Center => (dx_center, dy_center),
            Gravity::East => (dx_right, dy_center),
            Gravity::SouthWest => (0, dy_bottom),
            Gravity::South => (dx_center, dy_bottom),
            Gravity::SouthEast => (dx_right, dy_bottom),
        }
    }
}

/// Full set of options for an extent operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtentOptions {
    /// How destination dimensions are derived.
    pub size: ExtentSize,
    /// Background colour as RGBA. Alpha respected on formats that support
    /// it (PNG/WebP/AVIF); flattened on opaque formats (JPG).
    pub background: [u8; 4],
    /// Where the source sits on the extended canvas.
    pub gravity: Gravity,
}

impl ExtentOptions {
    /// Convenience constructor with sensible defaults (center gravity,
    /// solid black background).
    pub fn new(size: ExtentSize) -> Self {
        Self { size, background: [0, 0, 0, 0xFF], gravity: Gravity::Center }
    }

    /// Builder: change background colour.
    #[must_use]
    pub fn with_background(mut self, rgba: [u8; 4]) -> Self {
        self.background = rgba;
        self
    }

    /// Builder: change gravity.
    #[must_use]
    pub fn with_gravity(mut self, gravity: Gravity) -> Self {
        self.gravity = gravity;
        self
    }
}

/// Parse a hex colour string (`#RRGGBB` or `#RRGGBBAA`, with or without
/// leading `#`) into RGBA bytes. Returns a user-friendly error string for
/// CLI/GUI surfacing.
pub fn parse_hex_color(s: &str) -> std::result::Result<[u8; 4], String> {
    let stripped = s.strip_prefix('#').unwrap_or(s);
    let bytes: Vec<u8> = match stripped.len() {
        6 | 8 => stripped
            .as_bytes()
            .chunks(2)
            .map(|c| {
                let pair = std::str::from_utf8(c).map_err(|_| ())?;
                u8::from_str_radix(pair, 16).map_err(|_| ())
            })
            .collect::<std::result::Result<Vec<u8>, ()>>()
            .map_err(|_| format!("cor hex inválida: {s:?}; use dígitos 0-9 a-f"))?,
        _ => {
            return Err(format!(
                "cor hex inválida: {s:?}; use #RRGGBB ou #RRGGBBAA (6 ou 8 dígitos)"
            ));
        }
    };
    let alpha = if bytes.len() == 4 { bytes[3] } else { 0xFF };
    Ok([bytes[0], bytes[1], bytes[2], alpha])
}

/// Extend the canvas of a file and write it next to the source.
pub fn extent_file(
    input: impl AsRef<Path>,
    opts: ExtentOptions,
    target: Option<Format>,
    policy: OverwritePolicy,
) -> Result<ConvertOutcome> {
    let input = input.as_ref();

    let (src, src_format) = pipeline::decode_with_source_format(input)?;
    let target = target.unwrap_or(src_format);

    let (nw, nh) = opts.size.resolve(src.width(), src.height())?;
    let suffix = filename_suffix(nw, nh);
    let output = pipeline::resolve_output(input, Some(&suffix), target, policy)?;

    if matches!(policy, OverwritePolicy::Skip) && output.exists() {
        tracing::debug!(?output, "extent: skipping existing output");
        return Ok(ConvertOutcome::Skipped { output });
    }

    tracing::debug!(?input, ?output, ?target, ?opts, "extent: computing");
    let extended = apply_to(&src, &opts)?;
    pipeline::encode_and_cleanup(extended, output, target, &EncodeOptions::default(), policy)
}

/// Pure in-memory extent — shared with the preview pipeline.
pub fn apply_to(src: &DynamicImage, opts: &ExtentOptions) -> Result<DynamicImage> {
    let (sw, sh) = (src.width(), src.height());
    let (nw, nh) = opts.size.resolve(sw, sh)?;

    pipeline::check_dimensions(nw, nh, 4)?;
    let (dx, dy) = opts.gravity.place(sw, sh, nw, nh);

    let bg = Rgba(opts.background);
    let mut canvas: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_pixel(nw, nh, bg);

    // Normalise source to RGBA so the overlay step always agrees on
    // pixel format. Costs a copy but keeps the placement code trivial
    // and correct for paletted/luminance inputs.
    let src_rgba = src.to_rgba8();
    imageops::overlay(&mut canvas, &src_rgba, i64::from(dx), i64::from(dy));

    Ok(DynamicImage::ImageRgba8(canvas))
}

fn filename_suffix(width: u32, height: u32) -> String {
    format!("_extent-{width}x{height}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn write_fixture(dir: &Path, name: &str, w: u32, h: u32, fill: [u8; 3]) -> PathBuf {
        let path = dir.join(name);
        let img = RgbImage::from_pixel(w, h, Rgb(fill));
        img.save(&path).unwrap();
        path
    }

    #[test]
    fn parse_hex_accepts_6_and_8_digit_forms() {
        assert_eq!(parse_hex_color("#0d0e12").unwrap(), [0x0d, 0x0e, 0x12, 0xFF]);
        assert_eq!(parse_hex_color("0d0e12").unwrap(), [0x0d, 0x0e, 0x12, 0xFF]);
        assert_eq!(parse_hex_color("#00000080").unwrap(), [0, 0, 0, 0x80]);
    }

    #[test]
    fn parse_hex_rejects_bad_input() {
        assert!(parse_hex_color("#zzz").is_err());
        assert!(parse_hex_color("#12345").is_err());
        assert!(parse_hex_color("notacolor").is_err());
    }

    #[test]
    fn aspect_ratio_landscape_to_square_grows_height() {
        // 1920x1080 (16:9) → 1:1 should produce 1920x1920.
        let size = ExtentSize::Aspect { w: 1, h: 1 };
        assert_eq!(size.resolve(1920, 1080).unwrap(), (1920, 1920));
    }

    #[test]
    fn aspect_ratio_landscape_to_portrait_grows_height_more() {
        // 1920x1080 (16:9) → 9:16 should produce 1920 × (1920*16/9) = 3413.
        let size = ExtentSize::Aspect { w: 9, h: 16 };
        let (w, h) = size.resolve(1920, 1080).unwrap();
        assert_eq!(w, 1920);
        assert_eq!(h, 3413);
    }

    #[test]
    fn aspect_ratio_already_matches_returns_source_dims() {
        // 1920x1080 is exactly 16:9.
        let size = ExtentSize::Aspect { w: 16, h: 9 };
        assert_eq!(size.resolve(1920, 1080).unwrap(), (1920, 1080));
    }

    #[test]
    fn aspect_ratio_portrait_to_square_grows_width() {
        // 1080x1920 (9:16) → 1:1 → 1920x1920.
        let size = ExtentSize::Aspect { w: 1, h: 1 };
        assert_eq!(size.resolve(1080, 1920).unwrap(), (1920, 1920));
    }

    #[test]
    fn exact_size_smaller_than_source_is_rejected() {
        let size = ExtentSize::Exact { width: 100, height: 100 };
        let err = size.resolve(200, 200).unwrap_err();
        assert!(matches!(err, BigImageError::InvalidInput(_)));
    }

    #[test]
    fn exact_size_equal_to_source_passes_through() {
        let size = ExtentSize::Exact { width: 200, height: 200 };
        assert_eq!(size.resolve(200, 200).unwrap(), (200, 200));
    }

    #[test]
    fn zero_dimensions_are_rejected() {
        assert!(ExtentSize::Exact { width: 0, height: 100 }.resolve(50, 50).is_err());
        assert!(ExtentSize::Aspect { w: 0, h: 1 }.resolve(50, 50).is_err());
        assert!(ExtentSize::Exact { width: 100, height: 100 }.resolve(0, 50).is_err());
    }

    #[test]
    fn gravity_center_centers_source_on_canvas() {
        assert_eq!(Gravity::Center.place(100, 100, 300, 300), (100, 100));
    }

    #[test]
    fn gravity_corners_pin_to_corners() {
        assert_eq!(Gravity::NorthWest.place(100, 100, 300, 300), (0, 0));
        assert_eq!(Gravity::NorthEast.place(100, 100, 300, 300), (200, 0));
        assert_eq!(Gravity::SouthWest.place(100, 100, 300, 300), (0, 200));
        assert_eq!(Gravity::SouthEast.place(100, 100, 300, 300), (200, 200));
    }

    #[test]
    fn gravity_cardinals_center_one_axis() {
        assert_eq!(Gravity::North.place(100, 100, 300, 300), (100, 0));
        assert_eq!(Gravity::South.place(100, 100, 300, 300), (100, 200));
        assert_eq!(Gravity::East.place(100, 100, 300, 300), (200, 100));
        assert_eq!(Gravity::West.place(100, 100, 300, 300), (0, 100));
    }

    #[test]
    fn apply_letterbox_keeps_source_pixels_and_fills_background() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "src.png", 4, 2, [200, 100, 50]);
        let img = image::open(&src).unwrap();
        // Extend to 8x6 with red background, source centered → src lives
        // at (2, 2)..(6, 4); rest is red.
        let opts = ExtentOptions {
            size: ExtentSize::Exact { width: 8, height: 6 },
            background: [255, 0, 0, 0xFF],
            gravity: Gravity::Center,
        };
        let out = apply_to(&img, &opts).unwrap();
        let rgba = out.to_rgba8();
        assert_eq!(rgba.dimensions(), (8, 6));
        // Corner pixel is bg.
        assert_eq!(rgba.get_pixel(0, 0).0, [255, 0, 0, 0xFF]);
        // Center pixel is from source.
        assert_eq!(rgba.get_pixel(3, 3).0, [200, 100, 50, 0xFF]);
    }

    #[test]
    fn extent_file_writes_letterboxed_output() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "in.png", 10, 5, [128, 128, 128]);
        let opts = ExtentOptions {
            size: ExtentSize::Aspect { w: 1, h: 1 },
            background: [0, 0, 0, 0xFF],
            gravity: Gravity::Center,
        };
        let out = match extent_file(&src, opts, None, OverwritePolicy::Replace).unwrap() {
            ConvertOutcome::Written { output } => output,
            _ => panic!("expected write"),
        };
        assert_eq!(out, dir.path().join("in_extent-10x10.png"));
        let img = image::open(&out).unwrap();
        assert_eq!((img.width(), img.height()), (10, 10));
    }

    #[test]
    fn aspect_with_alpha_background_preserves_alpha() {
        let dir = TempDir::new().unwrap();
        let src = write_fixture(dir.path(), "a.png", 4, 4, [255, 255, 255]);
        let img = image::open(&src).unwrap();
        let opts = ExtentOptions {
            size: ExtentSize::Exact { width: 8, height: 8 },
            background: [0, 0, 0, 0],
            gravity: Gravity::Center,
        };
        let out = apply_to(&img, &opts).unwrap();
        let rgba = out.to_rgba8();
        // Top-left is transparent bg.
        assert_eq!(rgba.get_pixel(0, 0).0[3], 0);
        // Source pixel sits at (2, 2) and is opaque.
        assert_eq!(rgba.get_pixel(3, 3).0[3], 0xFF);
    }
}
