// SPDX-License-Identifier: GPL-3.0-or-later
//! "Smallest file at a guaranteed visual quality" encoders.
//!
//! Every format with a size/quality trade-off gets its best encoder and,
//! unless [`EncodeOptions::quality`] fixes the quality, a search for the
//! smallest output whose SSIMULACRA2 score against the pixels being saved
//! reaches [`crate::QualityTarget::ssimulacra2`] (see [`metric`]).
//!
//! | Format | Strategy |
//! |--------|----------|
//! | JPEG   | mozjpeg (progressive, trellis, optimized Huffman); quality search, 4:2:0 then 4:4:4 when needed |
//! | WebP   | libwebp lossy (`-m 6 -sharp_yuv -af`) quality search; lossless/near-lossless candidates; smallest passing wins |
//! | AVIF   | ravif 10-bit; quality search at speed 10, final encode at speed ≤ 6, re-verified |
//! | PNG    | oxipng lossless; palette (imagequant) only when it passes and is smaller |
//! | GIF    | exact palette, or imagequant with dithering chosen by the metric |
//! | ICO    | optimized lossless PNG entry |
//! | BMP    | 8-bit palette (≤ 256 colours, opaque), else 24-bit (opaque) or 32-bit |
//! | TGA    | RLE, 24-bit when opaque |
//! | TIFF   | Deflate level 9 + horizontal predictor |
//! | OpenEXR| float32 kept exact; ZIP16 vs PIZ, smaller wins |
//! | QOI, PNM, HDR | no tunables: written by the existing encoders |
//!
//! Candidates are decoded in-process to be scored. They are our own encoder
//! output from pixels already in memory, not untrusted input, so this does
//! not widen the sandboxed decoding boundary of the application.

mod avif;
mod formats;
mod jpeg;
pub(crate) mod metric;
mod palette;
mod png;
mod webp;

use image::{DynamicImage, ImageResult, RgbaImage};

pub(crate) use crate::pipeline::Extras;
use crate::{EncodeOptions, Format};

pub(crate) use palette::count_colours;

pub(crate) fn encode_error(
    format: image::ImageFormat,
    message: impl Into<String>,
) -> image::ImageError {
    image::ImageError::Encoding(image::error::EncodingError::new(format.into(), message.into()))
}

/// The colour channels of RGBA pixels.
pub(crate) fn rgb_of(image: &RgbaImage) -> Vec<u8> {
    image.as_raw().as_chunks::<4>().0.iter().flat_map(|px| [px[0], px[1], px[2]]).collect()
}

/// Decode one of our own candidates for scoring.
pub(crate) fn decode(bytes: &[u8], format: image::ImageFormat) -> ImageResult<RgbaImage> {
    image::load_from_memory_with_format(bytes, format).map(DynamicImage::into_rgba8)
}

/// Encode prepared pixels (see [`crate::convert::prepare_for_target`]) to
/// `target`. `icc` is the profile to embed, already resolved by the caller
/// for the formats that carry one. `None` for formats without tunables,
/// which keep their existing encoder.
pub(crate) fn encode(
    image: &DynamicImage,
    target: Format,
    opts: &EncodeOptions,
    icc: Option<&[u8]>,
    exif: Option<&[u8]>,
) -> ImageResult<Option<Vec<u8>>> {
    let extras = Extras { exif: exif.map(<[u8]>::to_vec), icc: icc.map(<[u8]>::to_vec) };
    let rgba = || match image {
        DynamicImage::ImageRgba8(b) => Cow::Borrowed(b),
        other => Cow::Owned(other.to_rgba8()),
    };
    use std::borrow::Cow;
    let bytes = match target {
        Format::Jpeg => {
            let rgb = match image {
                DynamicImage::ImageRgb8(b) => Cow::Borrowed(b),
                other => Cow::Owned(other.to_rgb8()),
            };
            jpeg::encode(&rgb, opts, &extras)?
        }
        Format::WebP => webp::encode(&rgba(), opts, &extras)?,
        Format::Avif => avif::encode(&rgba(), opts, &extras)?,
        Format::Png => png::encode(image, opts, icc, &extras)?,
        Format::Gif => formats::gif(&rgba(), opts)?,
        Format::Bmp => formats::bmp(&rgba())?,
        Format::Tga => formats::tga(&rgba())?,
        Format::Ico => formats::ico(&rgba(), opts)?,
        Format::Tiff => formats::tiff(image, icc)?,
        Format::OpenExr => {
            let float = match image {
                DynamicImage::ImageRgba32F(b) => Cow::Borrowed(b),
                other => Cow::Owned(other.to_rgba32f()),
            };
            formats::exr(&float)?
        }
        _ => return Ok(None),
    };
    Ok(Some(bytes))
}
