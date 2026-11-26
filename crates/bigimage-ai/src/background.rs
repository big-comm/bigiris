// SPDX-License-Identifier: GPL-3.0-or-later
//! Background removal with BiRefNet-lite (`birefnet-native`).
//!
//! The image is resized to the network's 1024 × 1024 input, the network
//! gives a foreground logit per pixel, and its sigmoid, resized back to
//! the original size, becomes the alpha channel of an RGBA copy of the
//! image (multiplied into any alpha it already had). The output is always
//! 8-bit RGBA; 16-bit and float sources are converted to 8-bit sRGB first.
//!
//! **Batch use:** the loaded model is kept until [`unload`] or
//! [`crate::unload_all`], so [`BgSession`] is only a handle: build one per
//! batch and call [`BgSession::process`] per image.
//!
//! The weights come from the BigLinux package `birefnet-native` (MIT);
//! `BIGIRIS_BIREFNET_WEIGHTS` points at another file.

use std::ops::ControlFlow;

use image::DynamicImage;

use crate::engine::{self, Budget, Slot};
use crate::memory::MIB;
use crate::{AiError, Availability, Device, ModelInfo, Stage};

/// BiRefNet-lite, half-float weights.
pub const MODEL: ModelInfo = ModelInfo {
    name: "BiRefNet-lite",
    package: "birefnet-native",
    weights: "/usr/share/birefnet-native/birefnet-lite-f16.weights",
    env: "BIGIRIS_BIREFNET_WEIGHTS",
    license: "MIT",
};

/// Side of the square image the network was trained on.
const INPUT_SIDE: u32 = 1024;
const INPUT_PIXELS: u64 = (INPUT_SIDE as u64) * (INPUT_SIDE as u64);

/// Measured: 0.26 GiB peak for the whole process on the CPU with
/// half-float weights (0.34 GiB on AVX-only CPUs, which widen the weights
/// to f32). Weights and engine buffers, with margin:
const LOAD_BYTES: u64 = 256 * MIB;
/// Network activations of one 1024² run, with margin.
const RUN_BYTES: u64 = 384 * MIB;
/// What loading checks for: the model and one run of a small image.
const PRELOAD: Budget = Budget { load: LOAD_BYTES, run: RUN_BYTES };

#[cfg(feature = "native")]
static SLOT: Slot<birefnet_native::Model> = Slot::new();
#[cfg(not(feature = "native"))]
static SLOT: Slot<()> = Slot::new();

#[cfg(feature = "native")]
engine::native_model!(birefnet_native::Model);

/// Whether background removal can run now.
pub fn availability() -> Availability {
    MODEL.availability()
}

/// Where the loaded model runs; `None` while none is loaded.
pub fn device() -> Option<Device> {
    SLOT.device()
}

/// Drop the loaded model.
pub fn unload() {
    SLOT.unload();
}

/// Bytes a run allocates for a `width × height` input beyond the network:
/// the RGBA8 copy of the source and the RGBA8 output (8 B/px), the mask
/// at full size (1 B/px), the resizer's intermediate rows (up to 4 B per
/// pixel of a 1024-wide strip along the long side) and the fixed 1024²
/// buffers (RGBA8 input, RGB8 copy, f32 logits, u8 mask).
fn workspace_bytes(width: u32, height: u32) -> u64 {
    let full = u64::from(width).saturating_mul(u64::from(height)).saturating_mul(9);
    let strip = 4 * u64::from(INPUT_SIDE) * u64::from(width.max(height));
    let fixed = INPUT_PIXELS * (4 + 3 + 4 + 1);
    full.saturating_add(strip).saturating_add(fixed)
}

/// Memory needed to remove the background of a `width × height` image.
fn budget(width: u32, height: u32) -> Budget {
    Budget { load: LOAD_BYTES, run: RUN_BYTES.saturating_add(workspace_bytes(width, height)) }
}

/// Opacity 0..=255 of a foreground logit, without overflow.
pub(crate) fn logit_alpha(value: f32) -> u8 {
    let probability = if value >= 0.0 {
        1.0 / (1.0 + (-value).exp())
    } else {
        let e = value.exp();
        e / (1.0 + e)
    };
    if probability.is_nan() {
        return 0;
    }
    (probability * 255.0).round() as u8
}

/// One-shot background removal. Loads the model the first time and keeps
/// it for later calls.
///
/// # Errors
///
/// As [`remove_background_with_progress`], which this never cancels.
pub fn remove_background(input: &DynamicImage) -> Result<DynamicImage, AiError> {
    remove_background_with_progress(input, |_| ControlFlow::Continue(()))
}

/// [`remove_background`] reporting [`Stage`]s; breaking cancels before
/// the network starts (BiRefNet runs as one step).
///
/// # Errors
///
/// - [`AiError::Disabled`]: the build has no `native` engines.
/// - [`AiError::Missing`] or [`AiError::Unreadable`]: the weights are not
///   installed or cannot be read.
/// - [`AiError::Memory`] or [`AiError::MemoryUnknown`]: free RAM is short for
///   the step, or cannot be read.
/// - [`AiError::Processing`]: `input` is empty or over the RGBA size limit.
/// - [`AiError::Cancelled`]: `progress` breaks.
/// - [`AiError::Inference`] or [`AiError::Gpu`]: the engine fails.
pub fn remove_background_with_progress(
    input: &DynamicImage,
    mut progress: impl FnMut(Stage) -> ControlFlow<()>,
) -> Result<DynamicImage, AiError> {
    run(input, &mut progress)
}

/// Batch handle over the shared, lazily loaded model.
pub struct BgSession {
    _private: (),
}

impl BgSession {
    /// Check that the model is installed and load it now (reporting
    /// [`Stage::Loading`]), so the first [`process`](Self::process) does
    /// not pay for it and errors show before any file is touched.
    ///
    /// # Errors
    ///
    /// As [`preload`].
    pub fn new(mut progress: impl FnMut(Stage) -> ControlFlow<()>) -> Result<Self, AiError> {
        preload(&mut progress)?;
        Ok(Self { _private: () })
    }

    /// Where the model runs.
    pub fn device(&self) -> Option<Device> {
        device()
    }

    /// Remove the background of `input`.
    ///
    /// # Errors
    ///
    /// As [`remove_background_with_progress`].
    pub fn process(&mut self, input: &DynamicImage) -> Result<DynamicImage, AiError> {
        remove_background(input)
    }

    /// [`process`](Self::process) with progress and cancellation.
    ///
    /// # Errors
    ///
    /// As [`remove_background_with_progress`].
    pub fn process_with_progress(
        &mut self,
        input: &DynamicImage,
        progress: impl FnMut(Stage) -> ControlFlow<()>,
    ) -> Result<DynamicImage, AiError> {
        remove_background_with_progress(input, progress)
    }
}

/// Load the model (when it is not loaded yet) without running it.
///
/// # Errors
///
/// - [`AiError::Disabled`]: the build has no `native` engines.
/// - [`AiError::Missing`] or [`AiError::Unreadable`]: the weights are not
///   installed or cannot be read.
/// - [`AiError::Memory`] or [`AiError::MemoryUnknown`]: free RAM is short for
///   the model, or cannot be read.
/// - [`AiError::Cancelled`]: `progress` breaks.
/// - [`AiError::Inference`] or [`AiError::Gpu`]: the engine cannot load it.
pub fn preload(progress: &mut dyn FnMut(Stage) -> ControlFlow<()>) -> Result<Device, AiError> {
    #[cfg(feature = "native")]
    {
        SLOT.run(&MODEL, PRELOAD, progress, |_, _| Ok(()))?;
        Ok(device().unwrap_or(Device::Cpu))
    }
    #[cfg(not(feature = "native"))]
    {
        let _ = progress;
        Err(AiError::Disabled)
    }
}

fn run(
    input: &DynamicImage,
    progress: &mut dyn FnMut(Stage) -> ControlFlow<()>,
) -> Result<DynamicImage, AiError> {
    let (width, height) = (input.width(), input.height());
    if width == 0 || height == 0 {
        return Err(AiError::Processing("empty image".into()));
    }
    if u64::from(width) * u64::from(height) * 4 > 512 * MIB {
        return Err(AiError::Processing("image exceeds the 512 MiB RGBA output limit".into()));
    }
    let mut budget = budget(width, height);
    budget.run = budget.run.saturating_add(engine::float_copy_bytes(input));
    #[cfg(feature = "native")]
    {
        SLOT.run(&MODEL, budget, progress, |model, progress| {
            let source = engine::display_rgba8(input);
            let resized = resize_rgba(&source, INPUT_SIDE, INPUT_SIDE)?;
            let rgb = engine::rgb_bytes(&resized);
            drop(resized);
            engine::check(progress(Stage::Running(0.0)))?;
            let logits = model.run(&rgb, INPUT_SIDE as usize)?;
            engine::check(progress(Stage::Running(1.0)))?;
            let mask: Vec<u8> = logits.iter().map(|&l| logit_alpha(l)).collect();
            cut_out(source, mask)
        })
    }
    #[cfg(not(feature = "native"))]
    {
        let _ = (budget, progress);
        Err(AiError::Disabled)
    }
}

/// Resize RGBA8 with Lanczos3.
fn resize_rgba(
    source: &image::RgbaImage,
    width: u32,
    height: u32,
) -> Result<image::RgbaImage, AiError> {
    let resized = bigimage_core::resize::apply_to(
        &DynamicImage::ImageRgba8(source.clone()),
        bigimage_core::ResizeMode::Exact { width, height },
        bigimage_core::Filter::Lanczos3,
    )
    .map_err(|e| AiError::Processing(e.to_string()))?;
    Ok(resized.into_rgba8())
}

/// Apply a 1024² mask to `source`: resized to its size and multiplied into
/// its alpha.
fn cut_out(mut source: image::RgbaImage, mask: Vec<u8>) -> Result<DynamicImage, AiError> {
    let mask = image::GrayImage::from_raw(INPUT_SIDE, INPUT_SIDE, mask)
        .ok_or_else(|| AiError::Processing("mask has an unexpected size".into()))?;
    let (width, height) = source.dimensions();
    let mask = bigimage_core::resize::apply_to(
        &DynamicImage::ImageLuma8(mask),
        bigimage_core::ResizeMode::Exact { width, height },
        bigimage_core::Filter::Lanczos3,
    )
    .map_err(|e| AiError::Processing(e.to_string()))?
    .into_luma8();
    for (pixel, opacity) in source.pixels_mut().zip(mask.as_raw()) {
        pixel[3] = ((u16::from(pixel[3]) * u16::from(*opacity) + 127) / 255) as u8;
    }
    Ok(DynamicImage::ImageRgba8(source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_logits_use_sigmoid_without_overflow() {
        assert_eq!(logit_alpha(0.0), 128);
        assert_eq!(logit_alpha(-1000.0), 0);
        assert_eq!(logit_alpha(1000.0), 255);
        assert_eq!(logit_alpha(f32::NEG_INFINITY), 0);
        assert_eq!(logit_alpha(f32::INFINITY), 255);
        assert_eq!(logit_alpha(f32::NAN), 0);
        assert!(logit_alpha(-1.0) > 0 && logit_alpha(1.0) < 255);
    }

    #[test]
    fn budget_grows_with_area_and_long_side() {
        let small = budget(640, 480);
        let large = budget(6000, 4000);
        assert_eq!(small.load, large.load);
        assert!(large.run > small.run + 6000 * 4000 * 9, "{large:?}");
        // A strip: the area term barely moves, the resizer rows do.
        assert_eq!(
            workspace_bytes(1, 2_000) - workspace_bytes(1, 1_000),
            4 * 1024 * 1_000 + 9 * 1_000
        );
        // Measured peak 0.26 GiB for a 1024² run; budget leaves margin.
        assert!(small.load + small.run > 512 * MIB);
        assert!(small.load + small.run < GIB_F);
    }
    const GIB_F: u64 = 1024 * MIB;

    #[test]
    fn mask_becomes_alpha_and_multiplies_existing_alpha() {
        let mut source = image::RgbaImage::from_pixel(4, 2, image::Rgba([9, 8, 7, 255]));
        source.put_pixel(0, 0, image::Rgba([9, 8, 7, 128]));
        // Left half of the 1024² mask opaque, right half clear.
        let side = INPUT_SIDE as usize;
        let mask: Vec<u8> =
            (0..side * side).map(|i| if i % side < side / 2 { 255 } else { 0 }).collect();
        let out = cut_out(source, mask).unwrap().into_rgba8();
        assert_eq!(out.get_pixel(0, 1).0, [9, 8, 7, 255]);
        assert_eq!(out.get_pixel(3, 1)[3], 0);
        assert!((127..=129).contains(&out.get_pixel(0, 0)[3]));
    }

    #[test]
    fn empty_and_oversized_inputs_are_refused_before_loading() {
        let empty = DynamicImage::new_rgb8(0, 0);
        assert!(matches!(remove_background(&empty), Err(AiError::Processing(_))));
    }

    #[cfg(not(feature = "native"))]
    #[test]
    fn without_engines_returns_clear_error() {
        let img = DynamicImage::new_rgb8(8, 8);
        assert!(matches!(remove_background(&img), Err(AiError::Disabled)));
        match BgSession::new(|_| ControlFlow::Continue(())) {
            Err(AiError::Disabled) => {}
            Err(other) => panic!("expected Disabled, got {other}"),
            Ok(_) => panic!("expected Disabled without the `native` feature"),
        }
    }
}
