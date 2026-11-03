// SPDX-License-Identifier: GPL-3.0-or-later
//! WebP through libwebp: lossy (VP8), lossless (VP8L) and near-lossless.
//!
//! Lossy settings match `cwebp -m 6 -sharp_yuv -af` (default SNS 50; `-pass`
//! is left alone: it only matters with a size/PSNR target). Alpha is
//! compressed losslessly (`alpha_quality` 100) — the metric scores the
//! composite, but alpha banding is cheap to avoid and costs few bytes.
//!
//! The lossy quality is searched against the target; lossless (and
//! near-lossless) candidates are tried when the image looks like a graphic
//! (few colours) or when lossy needs a very high quality, and the smallest
//! file that meets the target wins.

use image::{ImageResult, RgbaImage};

use super::metric::{search, Bounds, Probe};
use super::{count_colours, decode, encode_error, rgb_of, Extras};
use crate::encode::{EncodeOptions, QualityTarget};

const LO: u8 = 30;
const HI: u8 = 98;
/// Lossy needing at least this quality hints that lossless may win.
const TRY_LOSSLESS_FROM: u8 = 90;
/// Images with at most this many colours are graphics: try lossless.
const GRAPHIC_COLOURS: usize = 4096;
/// Images up to this many pixels get the slowest lossless effort (`-z 9`).
const MAX_EFFORT_PIXELS: u64 = 1_000_000;

#[derive(Debug, Clone, Copy)]
enum Mode {
    Lossy(u8),
    /// `cwebp -z level` equivalent.
    Lossless(u8),
    /// `cwebp -near_lossless value -z 6`.
    NearLossless(u8),
}

fn config(mode: Mode) -> ImageResult<webp::WebPConfig> {
    let mut config = webp::WebPConfig::new()
        .map_err(|()| encode_error(image::ImageFormat::WebP, "configuração inválida"))?;
    config.thread_level = 1;
    match mode {
        Mode::Lossy(quality) => {
            config.lossless = 0;
            config.quality = f32::from(quality.min(100));
            config.method = 6;
            config.use_sharp_yuv = 1;
            config.autofilter = 1;
            config.alpha_compression = 1;
            config.alpha_filtering = 2;
            config.alpha_quality = 100;
        }
        Mode::Lossless(level) | Mode::NearLossless(level) => {
            // WebPConfigLosslessPreset's table.
            const PRESETS: [(i32, f32); 10] = [
                (0, 0.0),
                (1, 20.0),
                (2, 25.0),
                (3, 30.0),
                (3, 50.0),
                (4, 50.0),
                (4, 75.0),
                (4, 90.0),
                (5, 90.0),
                (6, 100.0),
            ];
            let (method, quality) = PRESETS[usize::from(level.min(9))];
            config.lossless = 1;
            config.method = method;
            config.quality = quality;
            config.exact = 0;
            if let Mode::NearLossless(value) = mode {
                config.near_lossless = i32::from(value.min(100));
                config.method = 4;
                config.quality = 75.0;
            }
        }
    }
    Ok(config)
}

fn encode_raw(image: &RgbaImage, opaque: bool, mode: Mode) -> ImageResult<Vec<u8>> {
    let (w, h) = image.dimensions();
    let config = config(mode)?;
    let rgb;
    let encoder = if opaque {
        rgb = rgb_of(image);
        webp::Encoder::from_rgb(&rgb, w, h)
    } else {
        webp::Encoder::from_rgba(image.as_raw(), w, h)
    };
    let memory = encoder
        .encode_advanced(&config)
        .map_err(|error| encode_error(image::ImageFormat::WebP, format!("{error:?}")))?;
    Ok(memory.to_vec())
}

/// Wrap a simple-format WebP into the extended (VP8X) container carrying
/// ICC and EXIF chunks. Returns the input untouched when there is nothing
/// to add.
pub(crate) fn mux(webp: Vec<u8>, width: u32, height: u32, extras: &Extras) -> ImageResult<Vec<u8>> {
    let icc = extras.icc.as_deref().filter(|icc| !icc.is_empty());
    let exif = extras.exif.as_deref().filter(|exif| !exif.is_empty());
    if icc.is_none() && exif.is_none() {
        return Ok(webp);
    }
    let bad = || encode_error(image::ImageFormat::WebP, "saída do libwebp inesperada");
    if webp.len() < 12 || &webp[..4] != b"RIFF" || &webp[8..12] != b"WEBP" {
        return Err(bad());
    }
    // Keep the image chunks (ALPH, VP8, VP8L); drop libwebp's own VP8X.
    let mut image_chunks = Vec::new();
    let mut alpha = false;
    let mut at = 12;
    while at + 8 <= webp.len() {
        let fourcc = &webp[at..at + 4];
        let size = u32::from_le_bytes(webp[at + 4..at + 8].try_into().map_err(|_| bad())?) as usize;
        let end = at + 8 + size + (size & 1);
        let chunk = webp.get(at..end.min(webp.len())).ok_or_else(bad)?;
        match fourcc {
            b"VP8X" => alpha |= webp.get(at + 8).is_some_and(|flags| flags & 0x10 != 0),
            b"ALPH" => {
                alpha = true;
                image_chunks.extend_from_slice(chunk);
            }
            // VP8L header: signature 0x2f, then 14+14 bits of size and one
            // "alpha is used" bit.
            b"VP8L" => {
                alpha |= webp.get(at + 12).is_some_and(|b| b & 0x10 != 0);
                image_chunks.extend_from_slice(chunk);
            }
            b"VP8 " => image_chunks.extend_from_slice(chunk),
            _ => {}
        }
        at = end;
    }
    if image_chunks.is_empty() {
        return Err(bad());
    }
    let chunk = |out: &mut Vec<u8>, fourcc: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(fourcc);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(0);
        }
    };
    let mut flags = 0u8;
    if icc.is_some() {
        flags |= 0x20;
    }
    if alpha {
        flags |= 0x10;
    }
    if exif.is_some() {
        flags |= 0x08;
    }
    let mut vp8x = vec![flags, 0, 0, 0];
    vp8x.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
    vp8x.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
    let mut body = b"WEBP".to_vec();
    chunk(&mut body, b"VP8X", &vp8x);
    if let Some(icc) = icc {
        chunk(&mut body, b"ICCP", icc);
    }
    body.extend_from_slice(&image_chunks);
    if let Some(exif) = exif {
        chunk(&mut body, b"EXIF", exif);
    }
    let size = u32::try_from(body.len()).map_err(|_| bad())?;
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

fn lossless_level(image: &RgbaImage) -> u8 {
    if u64::from(image.width()) * u64::from(image.height()) <= MAX_EFFORT_PIXELS {
        9
    } else {
        6
    }
}

/// Encode an `Rgba8` image.
pub(crate) fn encode(
    image: &RgbaImage,
    opts: &EncodeOptions,
    extras: &Extras,
) -> ImageResult<Vec<u8>> {
    let (w, h) = image.dimensions();
    let opaque = image.pixels().all(|px| px[3] == 255);
    if let Some(quality) = opts.quality {
        let mode = if quality >= 100 {
            Mode::Lossless(lossless_level(image))
        } else {
            Mode::Lossy(quality)
        };
        return mux(encode_raw(image, opaque, mode)?, w, h, extras);
    }
    let threshold = opts.target.ssimulacra2();
    let probe = Probe::new(image)?;
    let start = match opts.target {
        QualityTarget::Maximum => 90,
        QualityTarget::Smallest => 68,
        _ => 82,
    };
    let lossy = search(&probe, Bounds { lo: LO, hi: HI, start }, threshold, |pixels, q| {
        let region_opaque = pixels.pixels().all(|px| px[3] == 255);
        let bytes = encode_raw(pixels, region_opaque, Mode::Lossy(q))?;
        let decoded = decode(&bytes, image::ImageFormat::WebP)?;
        Ok((bytes, decoded))
    })?;
    let mut best: Option<Vec<u8>> = None;
    let mut offer = |bytes: Vec<u8>| {
        if best.as_ref().is_none_or(|b| bytes.len() < b.len()) {
            best = Some(bytes);
        }
    };
    if lossy.passed {
        offer(match lossy.bytes {
            Some(bytes) => bytes,
            None => encode_raw(image, opaque, Mode::Lossy(lossy.quality))?,
        });
    }
    let struggling = !lossy.passed || lossy.quality >= TRY_LOSSLESS_FROM;
    if struggling || count_colours(image, GRAPHIC_COLOURS).is_some() {
        offer(encode_raw(image, opaque, Mode::Lossless(lossless_level(image)))?);
    }
    if struggling {
        // Near-lossless only changes pixel values slightly; verify it like
        // a lossy candidate when the whole image is scored.
        let candidate = encode_raw(image, opaque, Mode::NearLossless(60))?;
        let decoded = decode(&candidate, image::ImageFormat::WebP)?;
        if probe.score_whole(&decoded)?.is_some_and(|score| score >= threshold) {
            offer(candidate);
        }
    }
    let bytes = match best {
        Some(bytes) => bytes,
        None => encode_raw(image, opaque, Mode::Lossless(lossless_level(image)))?,
    };
    mux(bytes, w, h, extras)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mux_adds_icc_and_exif_chunks_that_image_reads_back() {
        let image =
            RgbaImage::from_fn(20, 10, |x, y| image::Rgba([x as u8 * 12, y as u8 * 20, 90, 255]));
        let icc = crate::color::srgb_profile().unwrap();
        let exif = b"II*\0\x08\0\0\0\0\0\0\0\0\0".to_vec();
        let extras = Extras { exif: Some(exif.clone()), icc: Some(icc.clone()) };
        for mode in [Mode::Lossy(80), Mode::Lossless(6)] {
            let raw = encode_raw(&image, true, mode).unwrap();
            let muxed = mux(raw, 20, 10, &extras).unwrap();
            use image::ImageDecoder;
            let mut decoder =
                image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(&muxed)).unwrap();
            assert_eq!(decoder.icc_profile().unwrap(), Some(icc.clone()), "{mode:?}");
            assert_eq!(decoder.exif_metadata().unwrap(), Some(exif.clone()), "{mode:?}");
            assert_eq!(decoder.dimensions(), (20, 10));
        }
        // Lossy with alpha: libwebp already writes VP8X + ALPH.
        let translucent =
            RgbaImage::from_fn(20, 10, |x, _| image::Rgba([200, 10, 10, x as u8 * 12]));
        let raw = encode_raw(&translucent, false, Mode::Lossy(80)).unwrap();
        let muxed = mux(raw, 20, 10, &extras).unwrap();
        let back = image::load_from_memory_with_format(&muxed, image::ImageFormat::WebP).unwrap();
        assert_eq!(back.to_rgba8().get_pixel(5, 0)[3], 60);
    }
}
