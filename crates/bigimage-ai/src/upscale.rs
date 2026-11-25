// SPDX-License-Identifier: GPL-3.0-or-later
//! Super-resolution with Real-ESRGAN general x4v3 (`realesrgan-native`),
//! with Lanczos3 as the fallback when the weights are not installed.
//!
//! The network always enlarges 4×. For 2× and 3× its output is reduced
//! to the requested size with Lanczos3, which keeps the detail the
//! network drew. The network sees RGB only: an alpha channel is enlarged
//! with Lanczos3 straight to the final size. The engine works in tiles,
//! so only the input and the output grow with the image.
//!
//! Real-ESRGAN works on 8-bit sRGB: 16-bit and float sources come out as
//! 8-bit (`Rgb8`, or `Rgba8` with alpha). [`Method::Lanczos`] keeps the
//! source's depth.
//!
//! The weights come from the BigLinux package `realesrgan-native`
//! (BSD-3-Clause); `BIGIRIS_REALESRGAN_WEIGHTS` points at another file.

use std::ops::ControlFlow;

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
/// Largest 4× intermediate (RGB8), the core pixel limit.
const MAX_NETWORK_PIXELS: u64 = 256 * 1024 * 1024;
/// Largest result, as RGBA8 (the core buffer limit).
const MAX_OUTPUT_BYTES: u64 = 512 * MIB;

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
        Err(AiError::Processing(format!("fator {factor} inválido; use 2, 3 ou 4")))
    }
}

/// Refuse results the core could not hold or encode.
fn check_limits(width: u32, height: u32, factor: u8) -> Result<(), AiError> {
    let network = u64::from(width) * u64::from(height) * u64::from(SCALE * SCALE);
    let (fw, fh) = output_size(width, height, factor);
    let output = u64::from(fw) * u64::from(fh);
    if width == 0 || height == 0 {
        return Err(AiError::Processing("imagem vazia".into()));
    }
    if network > MAX_NETWORK_PIXELS || output.saturating_mul(4) > MAX_OUTPUT_BYTES {
        return Err(AiError::Processing(format!(
            "{fw}×{fh} excede o limite de saída de 512 MiB; reduza a imagem ou o fator"
        )));
    }
    Ok(())
}

/// Memory one Real-ESRGAN run needs: a tile, the source (RGBA8 + RGB8),
/// the 4× RGB8 result, the final RGB8/RGBA8 image and, for 2× and 3×,
/// the resampled copy.
fn budget(width: u32, height: u32, factor: u8) -> Budget {
    let input = u64::from(width) * u64::from(height);
    let network = input * u64::from(SCALE * SCALE);
    let (fw, fh) = output_size(width, height, factor);
    let output = u64::from(fw) * u64::from(fh);
    let resample = if u32::from(factor) == SCALE { 0 } else { output * 4 };
    Budget { load: LOAD_BYTES, run: TILE_BYTES + input * 7 + network * 3 + output * 5 + resample }
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
    let mut budget = budget(width, height, factor);
    budget.run = budget.run.saturating_add(engine::float_copy_bytes(input));
    #[cfg(feature = "native")]
    {
        SLOT.run(&MODEL, budget, progress, |model, progress| {
            let source = engine::display_rgba8(input);
            let alpha = engine::alpha_of(input, &source);
            let rgb = engine::rgb_bytes(&source);
            drop(source);
            let (w, h) = engine::dims(width, height);
            let large = model.run_with_progress(&rgb, w, h, engine::running(progress))?;
            drop(rgb);
            finish(large, alpha.as_deref(), (width, height), factor)
        })
    }
    #[cfg(not(feature = "native"))]
    {
        let _ = (budget, progress);
        Err(AiError::Disabled)
    }
}

/// From the network's 4× RGB to the requested factor, with the alpha
/// channel (`width × height`, one byte per pixel) enlarged by Lanczos3.
pub(crate) fn finish(
    large: Vec<u8>,
    alpha: Option<&[u8]>,
    (width, height): (u32, u32),
    factor: u8,
) -> Result<DynamicImage, AiError> {
    let resize = |image: DynamicImage, (w, h): (u32, u32)| {
        bigimage_core::resize::apply_to(
            &image,
            bigimage_core::ResizeMode::Exact { width: w, height: h },
            bigimage_core::Filter::Lanczos3,
        )
        .map_err(|e| AiError::Processing(e.to_string()))
    };
    let target = output_size(width, height, factor);
    let large = image::RgbImage::from_raw(width * SCALE, height * SCALE, large)
        .ok_or_else(|| AiError::Processing("saída do Real-ESRGAN com tamanho inesperado".into()))?;
    let rgb = if u32::from(factor) == SCALE {
        large
    } else {
        resize(DynamicImage::ImageRgb8(large), target)?.into_rgb8()
    };
    let alpha = match alpha {
        None => None,
        Some(alpha) => {
            let alpha = image::GrayImage::from_raw(width, height, alpha.to_vec())
                .ok_or_else(|| AiError::Processing("alfa com tamanho inesperado".into()))?;
            Some(resize(DynamicImage::ImageLuma8(alpha), target)?.into_luma8().into_raw())
        }
    };
    engine::assemble(rgb.into_raw(), alpha.as_deref(), target.0, target.1)
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

    #[test]
    fn network_output_is_resampled_to_the_requested_factor() {
        // A 4× gray 3×2 image: 12×8 RGB.
        let (w, h) = (3u32, 2u32);
        let large = vec![200u8; (w * 4 * h * 4 * 3) as usize];
        for factor in [2u8, 3, 4] {
            let out = finish(large.clone(), None, (w, h), factor).unwrap();
            assert!(matches!(out, DynamicImage::ImageRgb8(_)));
            assert_eq!((out.width(), out.height()), output_size(w, h, factor));
            assert!(out.to_rgb8().pixels().all(|p| p.0 == [200, 200, 200]));
        }
        assert!(finish(vec![0; 5], None, (w, h), 2).is_err());
    }

    #[test]
    fn alpha_is_enlarged_beside_the_network_output() {
        let (w, h) = (16u32, 2u32);
        let large = vec![10u8; (w * 4 * h * 4 * 3) as usize];
        // Left half opaque, right half transparent.
        let alpha: Vec<u8> = (0..w * h).map(|i| if i % w < 8 { 255 } else { 0 }).collect();
        let out = finish(large, Some(&alpha), (w, h), 2).unwrap();
        let DynamicImage::ImageRgba8(out) = out else { panic!("RGBA expected") };
        assert_eq!(out.dimensions(), (32, 4));
        assert_eq!(out.get_pixel(0, 0).0, [10, 10, 10, 255]);
        assert_eq!(out.get_pixel(31, 3)[3], 0);
    }

    #[test]
    fn limits_and_budget_follow_the_output_size() {
        assert!(check_limits(1000, 1000, 4).is_ok());
        assert!(check_limits(0, 10, 2).is_err());
        // 8192² × 4 = 32768² RGBA is far beyond 512 MiB.
        assert!(check_limits(8192, 8192, 4).is_err());
        // 3000×2000 at 2× = 6000×4000 RGBA, 96 MB: fine, but its 4×
        // network pass is 96 M pixels: still under the core pixel limit.
        assert!(check_limits(3000, 2000, 2).is_ok());
        let small = budget(384, 256, 4);
        let large = budget(3000, 2000, 4);
        assert!(large.run > small.run);
        // 4× of 3000×2000 holds 96 M RGB pixels (288 MB) beside the tile.
        assert!(large.run > TILE_BYTES + 288_000_000);
        // Asking for 2× still pays for the network's 4× pass.
        assert!(budget(3000, 2000, 2).run > 288_000_000);
    }
}
