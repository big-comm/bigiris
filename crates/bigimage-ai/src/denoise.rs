// SPDX-License-Identifier: GPL-3.0-or-later
//! Noise and compression-artifact removal with SCUNet (`scunet-native`).
//!
//! SCUNet's "real PSNR" color model was trained on sensor noise, blur and
//! JPEG compression together, so it cleans all three. The network sees
//! RGB only; an alpha channel comes back untouched. It works in tiles of
//! up to 896² with a 64-pixel margin, so memory stays near 0.7 GB whatever
//! the size, but time grows with the area (2.3 TFLOP per megapixel: about
//! a minute and a half for 12 MP on a laptop CPU).
//!
//! SCUNet works on 8-bit sRGB: 16-bit and float sources come out as
//! 8-bit (`Rgb8`, or `Rgba8` with alpha).
//!
//! The weights come from the BigLinux package `scunet-native`
//! (Apache-2.0); `BIGIRIS_SCUNET_WEIGHTS` points at another file.

use std::ops::ControlFlow;

use image::DynamicImage;

use crate::engine::{self, Budget, Slot};
use crate::memory::MIB;
use crate::{AiError, Availability, Device, ModelInfo, Stage};

/// SCUNet color "real PSNR", half-float weights.
pub const MODEL: ModelInfo = ModelInfo {
    name: "SCUNet",
    package: "scunet-native",
    weights: "/usr/share/scunet-native/scunet-color-real-psnr-f16.weights",
    env: "BIGIRIS_SCUNET_WEIGHTS",
    license: "Apache-2.0",
};

/// Measured: 37 MB of weights; 0.41 GB peak for the whole process on a
/// 768×512 photo, 0.71 GB on tiled 12 MP.
const LOAD_BYTES: u64 = 64 * MIB;
/// One 896² tile's maps with its margin, plus margin of our own.
const TILE_BYTES: u64 = 768 * MIB;
/// Largest image, as RGBA8 (the core buffer limit).
const MAX_BYTES: u64 = 512 * MIB;

#[cfg(feature = "native")]
static SLOT: Slot<scunet_native::Model> = Slot::new();
#[cfg(not(feature = "native"))]
static SLOT: Slot<()> = Slot::new();

#[cfg(feature = "native")]
engine::native_model!(scunet_native::Model);

/// Whether denoising can run now.
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

/// Memory one run needs: a tile, the RGBA8 source, its RGB8 copy, the
/// RGB8 result and the RGBA8 (or RGB8) output.
fn budget(width: u32, height: u32) -> Budget {
    let pixels = u64::from(width) * u64::from(height);
    Budget { load: LOAD_BYTES, run: TILE_BYTES + pixels * (4 + 3 + 3 + 4) }
}

fn check_size(width: u32, height: u32) -> Result<(), AiError> {
    if width == 0 || height == 0 {
        return Err(AiError::Processing("imagem vazia".into()));
    }
    if u64::from(width) * u64::from(height) * 4 > MAX_BYTES {
        return Err(AiError::Processing("imagem excede o limite de 512 MiB em RGBA".into()));
    }
    Ok(())
}

/// Remove noise and compression artifacts from `input`.
pub fn denoise(
    input: &DynamicImage,
    mut progress: impl FnMut(Stage) -> ControlFlow<()>,
) -> Result<DynamicImage, AiError> {
    let (width, height) = (input.width(), input.height());
    check_size(width, height)?;
    let mut budget = budget(width, height);
    budget.run = budget.run.saturating_add(engine::float_copy_bytes(input));
    #[cfg(feature = "native")]
    {
        SLOT.run(&MODEL, budget, &mut progress, |model, progress| {
            let source = engine::display_rgba8(input);
            let alpha = engine::alpha_of(input, &source);
            let rgb = engine::rgb_bytes(&source);
            drop(source);
            let (w, h) = engine::dims(width, height);
            let clean = model.run_with_progress(&rgb, w, h, engine::running(progress))?;
            drop(rgb);
            engine::assemble(clean, alpha.as_deref(), width, height)
        })
    }
    #[cfg(not(feature = "native"))]
    {
        let _ = (budget, &mut progress);
        Err(AiError::Disabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_is_a_tile_plus_image_buffers() {
        let small = budget(768, 512);
        assert_eq!(small.run, TILE_BYTES + 768 * 512 * 14);
        // 12 MP: 0.71 GB measured for the engine; still under 1 GiB of
        // engine budget plus our 168 MB of buffers.
        let photo = budget(4000, 3000);
        assert!(photo.load + photo.run < 1024 * MIB + 4000 * 3000 * 14);
    }

    #[test]
    fn empty_and_oversized_inputs_are_refused() {
        let none = |_| ControlFlow::Continue(());
        assert!(matches!(
            denoise(&DynamicImage::new_rgb8(0, 4), none),
            Err(AiError::Processing(_))
        ));
        assert!(check_size(8192, 8192).is_ok());
        assert!(check_size(20_000, 20_000).is_err());
    }
}
