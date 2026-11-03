// SPDX-License-Identifier: GPL-3.0-or-later
//! AVIF through ravif (rav1e), 10-bit 4:4:4.
//!
//! The quality is searched with a fast encoder speed, then the whole image
//! is encoded once at the final (slower, better) speed and re-checked: a
//! slower speed almost always scores the same or better at the same
//! quality, and when it does not the quality is nudged up until it passes.

use image::{ImageResult, RgbaImage};

use super::metric::{search, Bounds, Probe};
use super::{decode, encode_error, Extras};
use crate::encode::{EncodeOptions, QualityTarget};

const LO: u8 = 30;
const HI: u8 = 95;
/// rav1e speed used while searching.
const SEARCH_SPEED: u8 = 10;
/// Slowest speed used for the final encode in target mode.
const FINAL_SPEED: u8 = 6;
/// Quality steps tried when the final encode misses the target.
const NUDGES: [u8; 2] = [3, 6];
/// Final score this far above the target triggers one lower-quality try.
const REFINE_MARGIN: f64 = 1.5;

fn threads() -> Option<usize> {
    Some(
        std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(1).clamp(1, 4))
            .unwrap_or(1),
    )
}

fn encode_raw(
    image: &RgbaImage,
    quality: u8,
    speed: u8,
    exif: Option<&[u8]>,
) -> ImageResult<Vec<u8>> {
    let quality = f32::from(quality.clamp(1, 100));
    let mut encoder = ravif::Encoder::new()
        .with_quality(quality)
        // The composite is what the metric scores; keep alpha at least as
        // good as colour so edges do not fringe.
        .with_alpha_quality(quality.clamp(80.0, 100.0))
        .with_speed(speed.clamp(1, 10))
        .with_bit_depth(ravif::BitDepth::Ten)
        .with_alpha_color_mode(ravif::AlphaColorMode::UnassociatedClean)
        .with_num_threads(threads());
    if let Some(exif) = exif {
        encoder = encoder.with_exif(exif.to_vec());
    }
    let pixels: &[rgb::RGBA8] = rgb::bytemuck::cast_slice(image.as_raw());
    let img = imgref::Img::new(pixels, image.width() as usize, image.height() as usize);
    encoder
        .encode_rgba(img)
        .map(|encoded| encoded.avif_file)
        .map_err(|error| encode_error(image::ImageFormat::Avif, error.to_string()))
}

/// Encode an `Rgba8` image.
pub(crate) fn encode(
    image: &RgbaImage,
    opts: &EncodeOptions,
    extras: &Extras,
) -> ImageResult<Vec<u8>> {
    let exif = extras.exif.as_deref().filter(|exif| !exif.is_empty());
    if let Some(quality) = opts.quality {
        return encode_raw(image, quality, opts.effective_avif_speed(), exif);
    }
    let threshold = opts.target.ssimulacra2();
    let final_speed = opts.effective_avif_speed().min(FINAL_SPEED);
    let probe = Probe::new(image)?;
    let start = match opts.target {
        QualityTarget::Maximum => 75,
        QualityTarget::Smallest => 50,
        _ => 65,
    };
    let found = search(&probe, Bounds { lo: LO, hi: HI, start }, threshold, |pixels, q| {
        let bytes = encode_raw(pixels, q, SEARCH_SPEED, None)?;
        let decoded = decode(&bytes, image::ImageFormat::Avif)?;
        Ok((bytes, decoded))
    })?;
    let mut bytes = encode_raw(image, found.quality, final_speed, exif)?;
    if !(found.passed && probe.is_whole()) {
        return Ok(bytes);
    }
    let score = probe.score_whole(&decode(&bytes, image::ImageFormat::Avif)?)?.unwrap_or(threshold);
    if score < threshold {
        // The final speed scored lower than the search speed: step up.
        for step in NUDGES {
            let quality = found.quality.saturating_add(step).min(100);
            bytes = encode_raw(image, quality, final_speed, exif)?;
            let decoded = decode(&bytes, image::ImageFormat::Avif)?;
            if probe.score_whole(&decoded)?.is_none_or(|s| s >= threshold) {
                break;
            }
        }
    } else if score - threshold > REFINE_MARGIN {
        // The final speed scored clearly higher: one step down usually
        // still passes and saves bytes.
        let lower = found.quality.saturating_sub(((score - threshold) / 0.5) as u8).max(LO);
        if lower < found.quality {
            let candidate = encode_raw(image, lower, final_speed, exif)?;
            let decoded = decode(&candidate, image::ImageFormat::Avif)?;
            if candidate.len() < bytes.len()
                && probe.score_whole(&decoded)?.is_some_and(|s| s >= threshold)
            {
                bytes = candidate;
            }
        }
    }
    Ok(bytes)
}
