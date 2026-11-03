// SPDX-License-Identifier: GPL-3.0-or-later
//! JPEG through mozjpeg: progressive scans, optimized Huffman tables and
//! trellis quantization (mozjpeg's defaults), about 30–40 % smaller than a
//! baseline libjpeg file at the same visual quality.
//!
//! The search starts with 4:2:0 chroma, right for photos. When that needs a
//! high quality to pass (sharp coloured edges: screenshots, charts, text)
//! — or for [`QualityTarget::Maximum`] — 4:4:4 is searched too and the
//! smaller passing file wins.

use image::{ImageResult, RgbImage};

use super::metric::{search, Bounds, Probe};
use super::{decode, encode_error, rgb_of, Extras};
use crate::encode::{EncodeOptions, QualityTarget};

/// Search bounds (mozjpeg quality scale).
const LO: u8 = 30;
const HI: u8 = 95;
/// Upper bound of the 4:4:4 retry: past 95 only full chroma helps.
const HI_444: u8 = 100;
/// A 4:2:0 result at or above this quality suggests chroma-heavy content.
const TRY_444_FROM: u8 = 86;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chroma {
    Sub420,
    Full444,
}

fn encode_rgb(
    rgb: &[u8],
    width: u32,
    height: u32,
    quality: u8,
    chroma: Chroma,
    extras: &Extras,
) -> ImageResult<Vec<u8>> {
    encode_rgb_with(rgb, width, height, quality, chroma, extras, false)
}

/// `annex_k`: the JPEG standard's example tables scaled like libjpeg's
/// `quality`, so a fixed quality means what it meant with the previous
/// encoder (and what [`crate::metadata::estimate_jpeg_quality`] reads back).
/// Otherwise mozjpeg's own tables, which are better per byte.
fn encode_rgb_with(
    rgb: &[u8],
    width: u32,
    height: u32,
    quality: u8,
    chroma: Chroma,
    extras: &Extras,
    annex_k: bool,
) -> ImageResult<Vec<u8>> {
    let run = || -> std::io::Result<Vec<u8>> {
        let mut compress = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
        compress.set_size(width as usize, height as usize);
        let q = f32::from(quality.clamp(1, 100));
        compress.set_quality(q);
        if annex_k {
            // Same arithmetic as libjpeg's jpeg_quality_scaling.
            compress.set_luma_qtable(&mozjpeg::qtable::AnnexK_Luma.scaled(q, q));
            compress.set_chroma_qtable(&mozjpeg::qtable::AnnexK_Chroma.scaled(q, q));
        }
        compress.set_progressive_mode();
        compress.set_optimize_coding(true);
        compress.set_optimize_scans(true);
        if chroma == Chroma::Full444 {
            compress.set_chroma_sampling_pixel_sizes((1, 1), (1, 1));
        } else {
            compress.set_chroma_sampling_pixel_sizes((2, 2), (2, 2));
        }
        let mut started = compress.start_compress(Vec::new())?;
        if let Some(exif) = &extras.exif {
            let mut app1 = Vec::with_capacity(exif.len() + 6);
            app1.extend_from_slice(b"Exif\0\0");
            app1.extend_from_slice(exif);
            started.write_marker(mozjpeg::Marker::APP(1), &app1);
        }
        if let Some(icc) = extras.icc.as_deref().filter(|icc| !icc.is_empty()) {
            // Written by hand: the wrapper's `write_icc_profile` numbers the
            // chunks from 0, but the ICC spec (and image's decoder) count
            // from 1. 65519 = 65533 marker bytes − 14 bytes of header.
            let chunks: Vec<&[u8]> = icc.chunks(65_519).collect();
            let total = u8::try_from(chunks.len()).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "perfil ICC grande demais")
            })?;
            for (index, chunk) in chunks.iter().enumerate() {
                let mut app2 = Vec::with_capacity(chunk.len() + 14);
                app2.extend_from_slice(b"ICC_PROFILE\0");
                app2.extend_from_slice(&[index as u8 + 1, total]);
                app2.extend_from_slice(chunk);
                started.write_marker(mozjpeg::Marker::APP(2), &app2);
            }
        }
        started.write_scanlines(rgb)?;
        started.finish()
    };
    // mozjpeg reports libjpeg errors by unwinding; keep them as errors.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        Ok(result) => result.map_err(image::ImageError::IoError),
        Err(panic) => {
            let message = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "erro interno do mozjpeg".to_owned());
            Err(encode_error(image::ImageFormat::Jpeg, message))
        }
    }
}

/// Encode an opaque `Rgb8` image (alpha already flattened).
pub(crate) fn encode(
    image: &RgbImage,
    opts: &EncodeOptions,
    extras: &Extras,
) -> ImageResult<Vec<u8>> {
    let (w, h) = image.dimensions();
    if let Some(quality) = opts.quality {
        return encode_rgb_with(image.as_raw(), w, h, quality, Chroma::Sub420, extras, true);
    }
    let threshold = opts.target.ssimulacra2();
    let rgba = image::DynamicImage::ImageRgb8(image.clone()).into_rgba8();
    let probe = Probe::new(&rgba)?;
    // Search without metadata (identical pixels, fewer bytes to copy).
    let none = Extras::default();
    let attempt = |chroma: Chroma, bounds: Bounds| {
        search(&probe, bounds, threshold, |pixels, q| {
            let (pw, ph) = pixels.dimensions();
            let bytes = encode_rgb(&rgb_of(pixels), pw, ph, q, chroma, &none)?;
            let decoded = decode(&bytes, image::ImageFormat::Jpeg)?;
            Ok((bytes, decoded))
        })
    };
    let start = match opts.target {
        QualityTarget::Maximum => 85,
        QualityTarget::Smallest => 65,
        _ => 78,
    };
    let sub = attempt(Chroma::Sub420, Bounds { lo: LO, hi: HI, start })?;
    let try_full =
        !sub.passed || sub.quality >= TRY_444_FROM || opts.target == QualityTarget::Maximum;
    // 4:4:4 costs more bytes per quality step, so it can only win below
    // the 4:2:0 answer.
    let full_bounds = if sub.passed {
        Bounds {
            lo: LO,
            hi: sub.quality.saturating_sub(1).max(LO),
            start: sub.quality.saturating_sub(6).max(LO),
        }
    } else {
        Bounds { lo: HI.saturating_sub(10), hi: HI_444, start: HI }
    };
    let mut found = vec![(Chroma::Sub420, sub)];
    if try_full && full_bounds.lo <= full_bounds.hi {
        found.push((Chroma::Full444, attempt(Chroma::Full444, full_bounds)?));
    }
    let any_passed = found.iter().any(|(_, f)| f.passed);
    if !any_passed {
        tracing::debug!(threshold, "JPEG não atingiu a meta; usando a maior qualidade");
    }
    let mut best: Option<Vec<u8>> = None;
    // Without a pass, the last attempt (4:4:4 at its top quality) is the
    // closest to the target.
    let keep = found.len() - 1;
    for (index, (chroma, f)) in found.into_iter().enumerate() {
        if any_passed != f.passed || (!any_passed && index != keep) {
            continue;
        }
        let bytes = match f.bytes {
            Some(bytes) if extras.is_empty() => bytes,
            _ => encode_rgb(image.as_raw(), w, h, f.quality, chroma, extras)?,
        };
        if best.as_ref().is_none_or(|b| bytes.len() < b.len()) {
            best = Some(bytes);
        }
    }
    best.map_or_else(|| encode_rgb(image.as_raw(), w, h, HI, Chroma::Sub420, extras), Ok)
}
