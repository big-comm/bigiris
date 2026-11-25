// SPDX-License-Identifier: GPL-3.0-or-later
//! Super-resolution with Real-ESRGAN general x4v3 (`realesrgan-native`),
//! with Lanczos3 as the fallback when the weights are not installed.
//!
//! The network always enlarges 4×. It runs on bands of rows, each with
//! the context the network and the resampling reach, and for 2× and 3×
//! each band is reduced to the requested size with Lanczos3 as it comes
//! out, which keeps the detail the network drew. The full 4× image is
//! never held, so only the input and the output grow with the image. The
//! network sees RGB only: an alpha channel is enlarged with Lanczos3
//! straight to the final size.
//!
//! Real-ESRGAN works on 8-bit sRGB: 16-bit and float sources come out as
//! 8-bit (`Rgb8`, or `Rgba8` with alpha). [`Method::Lanczos`] keeps the
//! source's depth.
//!
//! The weights come from the BigLinux package `realesrgan-native`
//! (BSD-3-Clause); `BIGIRIS_REALESRGAN_WEIGHTS` points at another file.

use std::ops::{ControlFlow, Range};

use image::DynamicImage;

use crate::engine::{self, Budget, Slot};
use crate::memory::MIB;
use crate::{AiError, Availability, Device, ModelInfo, Stage};

/// Real-ESRGAN general x4v3, blended half and half with its weak-denoise
/// twin, as Real-ESRGAN's own command line does by default.
pub const MODEL: ModelInfo = ModelInfo {
    name: "Real-ESRGAN",
    package: "realesrgan-native",
    weights: "/usr/share/realesrgan-native/realesr-general-x4v3-dn50.weights",
    env: "BIGIRIS_REALESRGAN_WEIGHTS",
    license: "BSD-3-Clause",
};

/// The network's fixed enlargement.
pub const SCALE: u32 = 4;
/// Factors offered (the CLI's `--factor` range).
pub const FACTORS: std::ops::RangeInclusive<u8> = 2..=4;

/// Measured: 4.9 MB of weights; the whole process peaks at 0.1 GB for a
/// 384×256 input and 0.4 GB plus the output for large tiled ones.
const LOAD_BYTES: u64 = 32 * MIB;
/// One tile (512² on the CPU, 768² on a GPU) of 64-channel maps, margin
/// included.
const TILE_BYTES: u64 = 512 * MIB;
/// Largest result, as RGBA8 (the core buffer limit).
const MAX_OUTPUT_BYTES: u64 = 512 * MIB;
/// Source rows the network sees per band, context included: the CPU tile,
/// so each band is a single row of the engine's tiles there.
const BAND_ROWS: u32 = 512;
/// Network context on each side of a band.
const NETWORK_MARGIN: u32 = 34;
#[cfg(feature = "native")]
const _: () = assert!(NETWORK_MARGIN as usize == realesrgan_native::MARGIN);
/// Source rows resampled beyond a band on each side: 8 network rows,
/// beyond Lanczos3's 6-row reach when 4× is reduced to 2×.
const RESAMPLE_ROWS: u32 = 2;
/// Source rows each band produces.
const BAND_STEP: u32 = BAND_ROWS - 2 * (NETWORK_MARGIN + RESAMPLE_ROWS);

#[cfg(feature = "native")]
static SLOT: Slot<realesrgan_native::Model> = Slot::new();
#[cfg(not(feature = "native"))]
static SLOT: Slot<()> = Slot::new();

#[cfg(feature = "native")]
engine::native_model!(realesrgan_native::Model);

/// How to enlarge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    /// Real-ESRGAN (needs `realesrgan-native`).
    RealEsrgan,
    /// Lanczos3 resampling, always available.
    Lanczos,
}

impl Method {
    /// Real-ESRGAN when its weights are installed, else Lanczos3.
    pub fn auto() -> Self {
        if availability().is_ready() {
            Method::RealEsrgan
        } else {
            Method::Lanczos
        }
    }
}

/// Whether Real-ESRGAN can run now.
pub fn availability() -> Availability {
    MODEL.availability()
}

/// Where the loaded model runs; `None` before the first run.
pub fn device() -> Option<Device> {
    SLOT.device()
}

/// Drop the loaded model.
pub fn unload() {
    SLOT.unload();
}

/// Size of `width × height` enlarged `factor` times.
pub fn output_size(width: u32, height: u32, factor: u8) -> (u32, u32) {
    let f = u32::from(factor);
    (width.saturating_mul(f), height.saturating_mul(f))
}

fn check_factor(factor: u8) -> Result<(), AiError> {
    if FACTORS.contains(&factor) {
        Ok(())
    } else {
        Err(AiError::Processing(format!("invalid factor {factor}; use 2, 3 or 4")))
    }
}

/// Refuse results the core could not hold or encode.
fn check_limits(width: u32, height: u32, factor: u8) -> Result<(), AiError> {
    let (fw, fh) = output_size(width, height, factor);
    let output = u64::from(fw) * u64::from(fh);
    if width == 0 || height == 0 {
        return Err(AiError::Processing("empty image".into()));
    }
    if output.saturating_mul(4) > MAX_OUTPUT_BYTES {
        return Err(AiError::Processing(format!(
            "{fw}×{fh} exceeds the 512 MiB output limit; use a smaller image or factor"
        )));
    }
    Ok(())
}

/// Memory one Real-ESRGAN run needs: a tile; the source as RGBA8, RGB8
/// and alpha; one band's 4× RGB8 and, for 2× and 3×, the resampler's
/// horizontal pass and the reduced band; the final RGB8, and with alpha
/// the enlarged alpha and the RGBA8 copy.
fn budget(width: u32, height: u32, factor: u8, alpha: bool) -> Budget {
    let input = u64::from(width) * u64::from(height);
    let f = u64::from(factor);
    let band_rows = u64::from(BAND_ROWS.min(height));
    let band_network = u64::from(width) * band_rows * u64::from(SCALE * SCALE) * 3;
    let band_resample = if u32::from(factor) == SCALE {
        0
    } else {
        let resampled_width = u64::from(width) * f;
        resampled_width * band_rows * u64::from(SCALE) * 3 + resampled_width * band_rows * f * 3
    };
    let (fw, fh) = output_size(width, height, factor);
    let output = u64::from(fw) * u64::from(fh);
    let output_bytes = if alpha { output * 8 } else { output * 3 };
    Budget {
        load: LOAD_BYTES,
        run: TILE_BYTES + input * 8 + band_network + band_resample + output_bytes,
    }
}

/// Enlarge `input` `factor` times (2, 3 or 4) with `method`.
pub fn upscale(
    input: &DynamicImage,
    factor: u8,
    method: Method,
    mut progress: impl FnMut(Stage) -> ControlFlow<()>,
) -> Result<DynamicImage, AiError> {
    check_factor(factor)?;
    match method {
        Method::Lanczos => lanczos(input, factor),
        Method::RealEsrgan => {
            check_limits(input.width(), input.height(), factor)?;
            real_esrgan(input, factor, &mut progress)
        }
    }
}

/// Lanczos3 enlargement keeping the source's pixel type.
pub fn lanczos(input: &DynamicImage, factor: u8) -> Result<DynamicImage, AiError> {
    check_factor(factor)?;
    bigimage_core::resize::apply_to(
        input,
        bigimage_core::ResizeMode::Percent(f32::from(factor) * 100.0),
        bigimage_core::Filter::Lanczos3,
    )
    .map_err(|e| AiError::Processing(e.to_string()))
}

fn real_esrgan(
    input: &DynamicImage,
    factor: u8,
    progress: &mut dyn FnMut(Stage) -> ControlFlow<()>,
) -> Result<DynamicImage, AiError> {
    let (width, height) = (input.width(), input.height());
    let mut budget = budget(width, height, factor, input.color().has_alpha());
    budget.run = budget.run.saturating_add(engine::float_copy_bytes(input));
    #[cfg(feature = "native")]
    {
        SLOT.run(&MODEL, budget, progress, |model, progress| {
            let source = engine::display_rgba8(input);
            let alpha = engine::alpha_of(input, &source);
            let rgb = engine::rgb_bytes(&source);
            drop(source);
            let row = width as usize * 3;
            // Bands overlap by their resampling context: report the furthest share.
            let mut reported = 0.0f32;
            let rgb = banded((width, height), factor, BAND_STEP, |rows| {
                let first = rows.start.saturating_sub(NETWORK_MARGIN);
                let last = rows.end.saturating_add(NETWORK_MARGIN).min(height);
                let source = &rgb[first as usize * row..last as usize * row];
                let (w, h) = engine::dims(width, last - first);
                let done = f64::from(rows.start) / f64::from(height);
                let share = f64::from(rows.end - rows.start) / f64::from(height);
                let mut large = model.run_with_progress(source, w, h, |part: f32| {
                    reported = reported.max((done + share * f64::from(part)) as f32);
                    progress(Stage::Running(reported.min(1.0)))
                })?;
                let large_row = row * (SCALE * SCALE) as usize;
                large.truncate((rows.end - first) as usize * large_row);
                large.drain(..(rows.start - first) as usize * large_row);
                Ok(large)
            })?;
            let alpha = enlarge_alpha(alpha.as_deref(), (width, height), factor)?;
            let (fw, fh) = output_size(width, height, factor);
            engine::assemble(rgb, alpha.as_deref(), fw, fh)
        })
    }
    #[cfg(not(feature = "native"))]
    {
        let _ = (budget, progress);
        Err(AiError::Disabled)
    }
}

fn resize(image: DynamicImage, (width, height): (u32, u32)) -> Result<DynamicImage, AiError> {
    bigimage_core::resize::apply_to(
        &image,
        bigimage_core::ResizeMode::Exact { width, height },
        bigimage_core::Filter::Lanczos3,
    )
    .map_err(|e| AiError::Processing(e.to_string()))
}

/// The `factor`× RGB8 of a `width × height` image, built `step` source rows
/// at a time from `network(rows)`, the 4× RGB8 of exactly those rows. For
/// 2× and 3× each band is reduced with `RESAMPLE_ROWS` of context on each
/// side, so the result is the reduction of the whole 4× image.
pub(crate) fn banded(
    (width, height): (u32, u32),
    factor: u8,
    step: u32,
    mut network: impl FnMut(Range<u32>) -> Result<Vec<u8>, AiError>,
) -> Result<Vec<u8>, AiError> {
    let wrong = || AiError::Processing("Real-ESRGAN output has an unexpected size".into());
    let (fw, fh) = output_size(width, height, factor);
    let f = u32::from(factor);
    let row = fw as usize * 3;
    let mut out = Vec::with_capacity(row * fh as usize);
    let mut start = 0;
    while start < height {
        let end = start.saturating_add(step).min(height);
        if f == SCALE {
            let large = network(start..end)?;
            if large.len() != row * ((end - start) * SCALE) as usize {
                return Err(wrong());
            }
            out.extend_from_slice(&large);
        } else {
            let first = start.saturating_sub(RESAMPLE_ROWS);
            let last = end.saturating_add(RESAMPLE_ROWS).min(height);
            let large = image::RgbImage::from_raw(
                width * SCALE,
                (last - first) * SCALE,
                network(first..last)?,
            )
            .ok_or_else(wrong)?;
            let reduced = resize(DynamicImage::ImageRgb8(large), (fw, (last - first) * f))?;
            let skip = row * ((start - first) * f) as usize;
            let len = row * ((end - start) * f) as usize;
            out.extend_from_slice(&reduced.as_bytes()[skip..skip + len]);
        }
        start = end;
    }
    Ok(out)
}

/// The alpha channel (`width × height`, one byte per pixel) enlarged by
/// Lanczos3 to the requested factor.
fn enlarge_alpha(
    alpha: Option<&[u8]>,
    (width, height): (u32, u32),
    factor: u8,
) -> Result<Option<Vec<u8>>, AiError> {
    let Some(alpha) = alpha else { return Ok(None) };
    let alpha = image::GrayImage::from_raw(width, height, alpha.to_vec())
        .ok_or_else(|| AiError::Processing("alpha has an unexpected size".into()))?;
    let target = output_size(width, height, factor);
    Ok(Some(resize(DynamicImage::ImageLuma8(alpha), target)?.into_luma8().into_raw()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factors_outside_two_to_four_are_refused() {
        let img = DynamicImage::new_rgb8(4, 4);
        for factor in [0, 1, 5] {
            assert!(upscale(&img, factor, Method::Lanczos, |_| ControlFlow::Continue(())).is_err());
        }
        let out = upscale(&img, 3, Method::Lanczos, |_| ControlFlow::Continue(())).unwrap();
        assert_eq!((out.width(), out.height()), (12, 12));
    }

    #[test]
    fn lanczos_keeps_the_pixel_type() {
        let deep = DynamicImage::ImageRgba16(image::ImageBuffer::new(3, 2));
        let out = lanczos(&deep, 2).unwrap();
        assert_eq!((out.width(), out.height()), (6, 4));
        assert!(matches!(out, DynamicImage::ImageRgba16(_)));
    }

    /// Rows `rows` of the 4× RGB8 image `large` of a `width`-wide source.
    fn rows_of(large: &[u8], width: u32, rows: Range<u32>) -> Vec<u8> {
        let row = (width * SCALE * 3 * SCALE) as usize;
        large[rows.start as usize * row..rows.end as usize * row].to_vec()
    }

    #[test]
    fn network_output_is_resampled_to_the_requested_factor() {
        // A 4× gray 3×2 image: 12×8 RGB.
        let (w, h) = (3u32, 2u32);
        let large = vec![200u8; (w * 4 * h * 4 * 3) as usize];
        for factor in [2u8, 3, 4] {
            let out = banded((w, h), factor, 1, |rows| Ok(rows_of(&large, w, rows))).unwrap();
            let (fw, fh) = output_size(w, h, factor);
            assert_eq!(out.len(), (fw * fh * 3) as usize);
            assert!(out.iter().all(|&v| v == 200));
            assert!(banded((w, h), factor, 1, |_| Ok(vec![0; 5])).is_err());
        }
    }

    #[test]
    fn bands_match_the_whole_image_reduction() {
        // Oracle: Lanczos3 over the whole 4× image, the unbanded path.
        let (w, h) = (7u32, 23u32);
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let large: Vec<u8> = (0..w * SCALE * h * SCALE * 3)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 56) as u8
            })
            .collect();
        let whole = image::RgbImage::from_raw(w * SCALE, h * SCALE, large.clone()).unwrap();
        for factor in [2u8, 3, 4] {
            let reference =
                resize(DynamicImage::ImageRgb8(whole.clone()), output_size(w, h, factor))
                    .unwrap()
                    .into_bytes();
            for step in [1, 3, 5, h, 100] {
                let out =
                    banded((w, h), factor, step, |rows| Ok(rows_of(&large, w, rows))).unwrap();
                assert!(out == reference, "factor {factor}, step {step}");
            }
        }
    }

    #[test]
    fn alpha_is_enlarged_beside_the_network_output() {
        let (w, h) = (16u32, 2u32);
        let large = vec![10u8; (w * 4 * h * 4 * 3) as usize];
        // Left half opaque, right half transparent.
        let alpha: Vec<u8> = (0..w * h).map(|i| if i % w < 8 { 255 } else { 0 }).collect();
        let rgb = banded((w, h), 2, BAND_STEP, |rows| Ok(rows_of(&large, w, rows))).unwrap();
        let alpha = enlarge_alpha(Some(&alpha), (w, h), 2).unwrap();
        let out = engine::assemble(rgb, alpha.as_deref(), 32, 4).unwrap();
        let DynamicImage::ImageRgba8(out) = out else { panic!("RGBA expected") };
        assert_eq!(out.get_pixel(0, 0).0, [10, 10, 10, 255]);
        assert_eq!(out.get_pixel(31, 3)[3], 0);
    }

    #[test]
    fn limits_follow_the_output_size() {
        assert!(check_limits(0, 10, 2).is_err());
        assert!(check_limits(1000, 1000, 4).is_ok());
        assert!(check_limits(3000, 2000, 2).is_ok());
        assert!(check_limits(8192, 8192, 4).is_err());
        // The RGBA output may reach 512 MiB, 128 Mi pixels, exactly.
        assert!(check_limits(8192, 4096, 2).is_ok());
        assert!(check_limits(8192, 4097, 2).is_err());
        assert!(check_limits(4096, 2048, 4).is_ok());
        assert!(check_limits(4096, 2049, 4).is_err());
        // 3861² × 9 is 134 165 889 pixels, 3862² × 9 is 134 235 396.
        assert!(check_limits(3861, 3861, 3).is_ok());
        assert!(check_limits(3862, 3862, 3).is_err());
        // A 20 MP phone photo at 2×, whose 4× pass alone is 319 M pixels.
        assert!(check_limits(5472, 3648, 2).is_ok());
    }

    #[test]
    fn budget_holds_one_network_band() {
        assert!(budget(3000, 2000, 4, false).run > budget(384, 256, 4, false).run);
        // Twice the rows add only the source and the output, never a larger
        // network pass.
        let (short, tall) = (budget(1000, 5000, 2, false), budget(1000, 10_000, 2, false));
        assert_eq!(tall.run - short.run, 1000 * 5000 * 8 + 2000 * 10_000 * 3);
        // With alpha the output is also held as alpha and as RGBA8.
        assert_eq!(budget(1000, 5000, 2, true).run - short.run, 2000 * 10_000 * 5);
    }
}
