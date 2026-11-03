// SPDX-License-Identifier: GPL-3.0-or-later
//! Lossless containers and palette formats: GIF, BMP, TGA, ICO, TIFF and
//! OpenEXR. None of them has a quality knob; each gets its smallest
//! lossless layout, and GIF (palette-only by nature) its best palette.

use std::borrow::Cow;

use image::{DynamicImage, ExtendedColorType, ImageEncoder, ImageResult, RgbaImage};

use super::metric::{Metric, FULL_RES_PIXELS};
use super::palette::{self, Paletted};
use super::{encode_error, rgb_of};
use crate::encode::EncodeOptions;

fn opaque(image: &RgbaImage) -> bool {
    image.pixels().all(|px| px[3] == 255)
}

/// GIF stores one fully transparent palette entry, nothing in between.
fn binary_alpha(image: &RgbaImage) -> RgbaImage {
    let mut out = image.clone();
    for px in out.pixels_mut() {
        if px[3] < 128 {
            *px = image::Rgba([0, 0, 0, 0]);
        } else {
            px[3] = 255;
        }
    }
    out
}

fn write_gif(paletted: &Paletted) -> ImageResult<Vec<u8>> {
    let gif_error = |e: gif::EncodingError| encode_error(image::ImageFormat::Gif, e.to_string());
    let (w, h) = (paletted.width, paletted.height);
    let too_big = || {
        encode_error(image::ImageFormat::Gif, format!("dimensões {w}×{h} grandes demais para GIF"))
    };
    let width = u16::try_from(w).map_err(|_| too_big())?;
    let height = u16::try_from(h).map_err(|_| too_big())?;
    let transparent = paletted.palette.iter().position(|c| c[3] < 128);
    let mut indices = Cow::Borrowed(&paletted.indices[..]);
    if let Some(t) = transparent {
        // Fold every transparent entry into the first one.
        let folded: Vec<u8> = paletted
            .indices
            .iter()
            .map(|&i| if paletted.palette[usize::from(i)][3] < 128 { t as u8 } else { i })
            .collect();
        indices = Cow::Owned(folded);
    }
    let rgb: Vec<u8> = paletted.palette.iter().flat_map(|c| [c[0], c[1], c[2]]).collect();
    let mut out = Vec::new();
    {
        let mut encoder = gif::Encoder::new(&mut out, width, height, &rgb).map_err(gif_error)?;
        let frame = gif::Frame {
            width,
            height,
            buffer: indices,
            transparent: transparent.map(|t| t as u8),
            ..gif::Frame::default()
        };
        encoder.write_frame(&frame).map_err(gif_error)?;
    }
    Ok(out)
}

/// Single-frame GIF. An image with ≤ 256 colours is written exactly;
/// otherwise imagequant builds the palette and the dithering (none or
/// full) is chosen by SSIMULACRA2: the smallest file meeting the target,
/// or the best-scoring one when neither does.
pub(crate) fn gif(image: &RgbaImage, opts: &EncodeOptions) -> ImageResult<Vec<u8>> {
    let binary = binary_alpha(image);
    if let Some(exact) = palette::exact(&binary, 256) {
        return write_gif(&exact);
    }
    let candidates = palette::quantize(&binary, 256, &[0.0, 1.0])?;
    let (w, h) = image.dimensions();
    if u64::from(w) * u64::from(h) > FULL_RES_PIXELS {
        return write_gif(candidates.last().expect("dithered candidate"));
    }
    let metric = Metric::new(image)?;
    let threshold = opts.target.ssimulacra2();
    let mut passing: Option<Vec<u8>> = None;
    let mut best_score: Option<(f64, Vec<u8>)> = None;
    for candidate in &candidates {
        let score = metric.score(&candidate.to_rgba())?;
        let bytes = write_gif(candidate)?;
        if score >= threshold && passing.as_ref().is_none_or(|p| bytes.len() < p.len()) {
            passing = Some(bytes.clone());
        }
        if best_score.as_ref().is_none_or(|(s, _)| score > *s) {
            best_score = Some((score, bytes));
        }
    }
    Ok(passing.or(best_score.map(|(_, b)| b)).expect("at least one candidate"))
}

/// BMP: opaque images with ≤ 256 colours as 8-bit palette, other opaque
/// images as 24-bit, transparent ones as 32-bit BGRA. All lossless.
pub(crate) fn bmp(image: &RgbaImage) -> ImageResult<Vec<u8>> {
    let (w, h) = image.dimensions();
    let mut out = Vec::new();
    let mut encoder = image::codecs::bmp::BmpEncoder::new(&mut out);
    if !opaque(image) {
        encoder.encode(image.as_raw(), w, h, ExtendedColorType::Rgba8)?;
    } else if let Some(paletted) = palette::exact(image, 256) {
        let rgb: Vec<[u8; 3]> = paletted.palette.iter().map(|c| [c[0], c[1], c[2]]).collect();
        encoder.encode_with_palette(&paletted.indices, w, h, ExtendedColorType::L8, Some(&rgb))?;
    } else {
        encoder.encode(&rgb_of(image), w, h, ExtendedColorType::Rgb8)?;
    }
    Ok(out)
}

/// TGA: run-length encoded, 24-bit when opaque.
pub(crate) fn tga(image: &RgbaImage) -> ImageResult<Vec<u8>> {
    let (w, h) = image.dimensions();
    let mut out = Vec::new();
    let encoder = image::codecs::tga::TgaEncoder::new(&mut out);
    if opaque(image) {
        encoder.write_image(&rgb_of(image), w, h, ExtendedColorType::Rgb8)?;
    } else {
        encoder.write_image(image.as_raw(), w, h, ExtendedColorType::Rgba8)?;
    }
    Ok(out)
}

/// ICO with one optimized, lossless 32-bit PNG entry.
pub(crate) fn ico(image: &RgbaImage, opts: &EncodeOptions) -> ImageResult<Vec<u8>> {
    let png = super::png::rgba8_exact(image, opts)?;
    let frame = image::codecs::ico::IcoFrame::with_encoded(
        png,
        image.width(),
        image.height(),
        ExtendedColorType::Rgba8,
    )?;
    let mut out = Vec::new();
    image::codecs::ico::IcoEncoder::new(&mut out).encode_images(&[frame])?;
    Ok(out)
}

/// TIFF: Deflate (level 9) with the horizontal predictor; ICC profile tag
/// when one is given. Lossless.
pub(crate) fn tiff(image: &DynamicImage, icc: Option<&[u8]>) -> ImageResult<Vec<u8>> {
    use tiff::encoder::{colortype, Compression, DeflateLevel, TiffEncoder};
    use tiff::tags::{Predictor, Tag};
    let tiff_error = |e: tiff::TiffError| encode_error(image::ImageFormat::Tiff, e.to_string());
    let (w, h) = (image.width(), image.height());
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut encoder = TiffEncoder::new(&mut cursor)
            .map_err(tiff_error)?
            .with_compression(Compression::Deflate(DeflateLevel::Best))
            .with_predictor(Predictor::Horizontal);
        macro_rules! write {
            ($kind:ty, $data:expr) => {{
                let mut img = encoder.new_image::<$kind>(w, h).map_err(tiff_error)?;
                if let Some(icc) = icc {
                    img.encoder().write_tag(Tag::IccProfile, icc).map_err(tiff_error)?;
                }
                img.write_data($data).map_err(tiff_error)?;
            }};
        }
        match image {
            DynamicImage::ImageLuma8(b) => write!(colortype::Gray8, b.as_raw()),
            DynamicImage::ImageRgb8(b) => write!(colortype::RGB8, b.as_raw()),
            DynamicImage::ImageRgba8(b) => write!(colortype::RGBA8, b.as_raw()),
            DynamicImage::ImageLuma16(b) => write!(colortype::Gray16, b.as_raw()),
            DynamicImage::ImageRgb16(b) => write!(colortype::RGB16, b.as_raw()),
            DynamicImage::ImageRgba16(b) => write!(colortype::RGBA16, b.as_raw()),
            other if other.color().has_alpha() => {
                return tiff(&DynamicImage::ImageRgba16(other.to_rgba16()), icc);
            }
            other => return tiff(&DynamicImage::ImageRgb16(other.to_rgb16()), icc),
        }
    }
    Ok(cursor.into_inner())
}

/// OpenEXR, 32-bit float RGBA kept bit-exact: ZIP (16 lines) and PIZ are
/// both tried and the smaller file is kept.
pub(crate) fn exr(image: &image::Rgba32FImage) -> ImageResult<Vec<u8>> {
    use exr::prelude::*;
    let (w, h) = (image.width() as usize, image.height() as usize);
    let encode = |encoding: Encoding| -> ImageResult<Vec<u8>> {
        let channels = SpecificChannels::rgba(|pos: Vec2<usize>| {
            let px = image.get_pixel(pos.x() as u32, pos.y() as u32).0;
            (px[0], px[1], px[2], px[3])
        });
        let mut cursor = std::io::Cursor::new(Vec::new());
        Image::from_encoded_channels((w, h), encoding, channels)
            .write()
            .to_buffered(&mut cursor)
            .map_err(|e| encode_error(image::ImageFormat::OpenExr, e.to_string()))?;
        Ok(cursor.into_inner())
    };
    let zip = encode(Encoding::SMALL_LOSSLESS)?;
    let piz = encode(Encoding::SMALL_FAST_LOSSLESS)?;
    Ok(if piz.len() < zip.len() { piz } else { zip })
}
