// SPDX-License-Identifier: GPL-3.0-or-later
//! PNG through oxipng: lossless reductions (bit depth, colour type, palette
//! when the image already has ≤ 256 colours), filter search and libdeflate.
//!
//! In [`QualityTarget::Balanced`] and [`QualityTarget::Smallest`] an 8-bit
//! colour image with more than 256 colours is also quantized to a palette
//! (imagequant, with and without dithering); the palette file is used only
//! when it meets the target *and* is smaller than the lossless one. 16-bit
//! sources are only quantized in `Smallest`; `Maximum` and an explicit
//! [`EncodeOptions::quality`] are always lossless.

use image::{DynamicImage, ImageResult};
use oxipng::{BitDepth, ColorType, Options, RawImage, StripChunks};

use super::metric::{Metric, FULL_RES_PIXELS};
use super::palette::{self, Paletted};
use super::{encode_error, Extras};
use crate::encode::{EncodeOptions, QualityTarget};

fn png_error(error: oxipng::PngError) -> image::ImageError {
    encode_error(image::ImageFormat::Png, format!("oxipng: {error}"))
}

fn options(opts: &EncodeOptions) -> Options {
    let mut options = Options::from_preset(if opts.optimize { 4 } else { 2 });
    options.strip = StripChunks::None;
    // Clearing colour under fully transparent pixels is invisible but not
    // bit-exact; only the "smallest file" target accepts it.
    options.optimize_alpha = opts.quality.is_none() && opts.target == QualityTarget::Smallest;
    options
}

fn be16(samples: &[u16]) -> Vec<u8> {
    samples.iter().flat_map(|v| v.to_be_bytes()).collect()
}

fn raw_image(image: &DynamicImage) -> ImageResult<RawImage> {
    let (w, h) = (image.width(), image.height());
    let gray = ColorType::Grayscale { transparent_shade: None };
    let rgb = ColorType::RGB { transparent_color: None };
    let (color, depth, data) = match image {
        DynamicImage::ImageLuma8(b) => (gray, BitDepth::Eight, b.as_raw().clone()),
        DynamicImage::ImageLumaA8(b) => {
            (ColorType::GrayscaleAlpha, BitDepth::Eight, b.as_raw().clone())
        }
        DynamicImage::ImageRgb8(b) => (rgb, BitDepth::Eight, b.as_raw().clone()),
        DynamicImage::ImageRgba8(b) => (ColorType::RGBA, BitDepth::Eight, b.as_raw().clone()),
        DynamicImage::ImageLuma16(b) => (gray, BitDepth::Sixteen, be16(b.as_raw())),
        DynamicImage::ImageLumaA16(b) => {
            (ColorType::GrayscaleAlpha, BitDepth::Sixteen, be16(b.as_raw()))
        }
        DynamicImage::ImageRgb16(b) => (rgb, BitDepth::Sixteen, be16(b.as_raw())),
        DynamicImage::ImageRgba16(b) => (ColorType::RGBA, BitDepth::Sixteen, be16(b.as_raw())),
        other if other.color().has_alpha() => {
            return raw_image(&DynamicImage::ImageRgba16(other.to_rgba16()));
        }
        other => return raw_image(&DynamicImage::ImageRgb16(other.to_rgb16())),
    };
    RawImage::new(w, h, color, depth, data).map_err(png_error)
}

fn finish(
    mut raw: RawImage,
    icc: Option<&[u8]>,
    extras: &Extras,
    opts: &Options,
) -> ImageResult<Vec<u8>> {
    if let Some(icc) = icc {
        raw.add_icc_profile(icc);
    }
    if let Some(exif) = extras.exif.as_ref().filter(|exif| !exif.is_empty()) {
        raw.add_png_chunk(*b"eXIf", exif.clone());
    }
    raw.create_optimized_png(opts).map_err(png_error)
}

fn paletted_png(
    paletted: &Paletted,
    icc: Option<&[u8]>,
    extras: &Extras,
    opts: &Options,
) -> ImageResult<Vec<u8>> {
    let palette =
        paletted.palette.iter().map(|&[r, g, b, a]| rgb::RGBA8::new(r, g, b, a)).collect();
    let raw = RawImage::new(
        paletted.width,
        paletted.height,
        ColorType::Indexed { palette },
        BitDepth::Eight,
        paletted.indices.clone(),
    )
    .map_err(png_error)?;
    finish(raw, icc, extras, opts)
}

/// Lossless RGBA8 PNG with no colour-type or depth reduction, for ICO
/// entries: icon readers (image's own included) expect 32-bit RGBA PNGs.
pub(crate) fn rgba8_exact(image: &image::RgbaImage, opts: &EncodeOptions) -> ImageResult<Vec<u8>> {
    let mut options = options(&EncodeOptions { quality: Some(100), ..*opts });
    options.bit_depth_reduction = false;
    options.color_type_reduction = false;
    options.palette_reduction = false;
    options.grayscale_reduction = false;
    let raw = raw_image(&DynamicImage::ImageRgba8(image.clone()))?;
    finish(raw, None, &Extras::default(), &options)
}

/// Encode prepared pixels; `icc` is the profile to embed (if any).
pub(crate) fn encode(
    image: &DynamicImage,
    opts: &EncodeOptions,
    icc: Option<&[u8]>,
    extras: &Extras,
) -> ImageResult<Vec<u8>> {
    let options = options(opts);
    let lossless = finish(raw_image(image)?, icc, extras, &options)?;
    let sixteen = matches!(image, DynamicImage::ImageRgb16(_) | DynamicImage::ImageRgba16(_));
    let quantizable = opts.quality.is_none()
        && match opts.target {
            QualityTarget::Maximum => false,
            QualityTarget::Smallest => {
                sixteen || matches!(image, DynamicImage::ImageRgb8(_) | DynamicImage::ImageRgba8(_))
            }
            _ => matches!(image, DynamicImage::ImageRgb8(_) | DynamicImage::ImageRgba8(_)),
        }
        && u64::from(image.width()) * u64::from(image.height()) <= FULL_RES_PIXELS;
    if !quantizable {
        return Ok(lossless);
    }
    let rgba = image.to_rgba8();
    if palette::count_colours(&rgba, 256).is_some() {
        // oxipng already wrote the exact palette, losslessly.
        return Ok(lossless);
    }
    let metric = Metric::new(&rgba)?;
    let threshold = opts.target.ssimulacra2();
    let mut best = lossless;
    for candidate in palette::quantize(&rgba, 256, &[0.0, 1.0])? {
        if metric.score(&candidate.to_rgba())? < threshold {
            continue;
        }
        let bytes = paletted_png(&candidate, icc, extras, &options)?;
        if bytes.len() < best.len() {
            best = bytes;
        }
    }
    Ok(best)
}
