// SPDX-License-Identifier: GPL-3.0-or-later
//! Palette helpers shared by PNG, GIF and BMP: exact palettes for images
//! that already have few colours, and imagequant (libimagequant, the
//! pngquant engine) for the rest.

use std::collections::HashMap;

use image::{ImageResult, RgbaImage};

use super::encode_error;

/// An 8-bit indexed image.
pub(crate) struct Paletted {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) palette: Vec<[u8; 4]>,
    pub(crate) indices: Vec<u8>,
}

impl Paletted {
    /// The pixels a viewer shows.
    pub(crate) fn to_rgba(&self) -> RgbaImage {
        let raw = self.indices.iter().flat_map(|&i| self.palette[usize::from(i)]).collect();
        RgbaImage::from_raw(self.width, self.height, raw).expect("palette size")
    }
}

/// The image's own palette when it has at most `limit` (≤ 256) distinct
/// RGBA values — lossless.
pub(crate) fn exact(image: &RgbaImage, limit: usize) -> Option<Paletted> {
    let limit = limit.min(256);
    let mut lookup: HashMap<[u8; 4], u8> = HashMap::new();
    let mut palette = Vec::new();
    let mut indices = Vec::with_capacity(image.as_raw().len() / 4);
    for px in image.pixels() {
        let index = match lookup.get(&px.0) {
            Some(&index) => index,
            None => {
                if palette.len() == limit {
                    return None;
                }
                let index = palette.len() as u8;
                palette.push(px.0);
                lookup.insert(px.0, index);
                index
            }
        };
        indices.push(index);
    }
    Some(Paletted { width: image.width(), height: image.height(), palette, indices })
}

fn quant_error(error: imagequant::Error) -> image::ImageError {
    encode_error(image::ImageFormat::Png, format!("imagequant: {error}"))
}

/// Quantize to at most `max_colors` colours, once per dithering level
/// (0.0 = none, 1.0 = full Floyd–Steinberg), sharing one palette search.
pub(crate) fn quantize(
    image: &RgbaImage,
    max_colors: u32,
    dithering: &[f32],
) -> ImageResult<Vec<Paletted>> {
    let (w, h) = image.dimensions();
    let mut attr = imagequant::new();
    attr.set_max_colors(max_colors.clamp(2, 256)).map_err(quant_error)?;
    attr.set_quality(0, 100).map_err(quant_error)?;
    attr.set_speed(4).map_err(quant_error)?;
    let pixels: &[imagequant::RGBA] = rgb::bytemuck::cast_slice(image.as_raw());
    let mut img =
        attr.new_image_borrowed(pixels, w as usize, h as usize, 0.0).map_err(quant_error)?;
    let mut result = attr.quantize(&mut img).map_err(quant_error)?;
    let mut out = Vec::with_capacity(dithering.len());
    for &level in dithering {
        result.set_dithering_level(level).map_err(quant_error)?;
        let (palette, indices) = result.remapped(&mut img).map_err(quant_error)?;
        let palette = palette.iter().map(|c| [c.r, c.g, c.b, c.a]).collect();
        out.push(Paletted { width: w, height: h, palette, indices });
    }
    Ok(out)
}

/// Unique RGBA colours, or `None` past `limit`.
pub(crate) fn count_colours(image: &RgbaImage, limit: usize) -> Option<usize> {
    let mut seen = std::collections::HashSet::new();
    for px in image.pixels() {
        seen.insert(u32::from_ne_bytes(px.0));
        if seen.len() > limit {
            return None;
        }
    }
    Some(seen.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_palette_round_trips_and_refuses_too_many_colours() {
        let image =
            RgbaImage::from_fn(16, 16, |x, y| image::Rgba([(x * 16) as u8, (y % 2) as u8, 0, 255]));
        let paletted = exact(&image, 256).unwrap();
        assert_eq!(paletted.palette.len(), 32);
        assert_eq!(paletted.to_rgba(), image);
        assert!(exact(&image, 31).is_none());
        assert_eq!(count_colours(&image, 100), Some(32));
        assert_eq!(count_colours(&image, 10), None);
    }

    #[test]
    fn quantize_returns_one_result_per_dithering_level() {
        let image = RgbaImage::from_fn(64, 64, |x, y| {
            image::Rgba([(x * 4) as u8, (y * 4) as u8, 128, 255])
        });
        let results = quantize(&image, 256, &[0.0, 1.0]).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|p| p.palette.len() <= 256 && p.indices.len() == 64 * 64));
    }
}
