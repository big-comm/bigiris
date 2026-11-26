// SPDX-License-Identifier: GPL-3.0-or-later
//! bigimage-ai — local image AI for BigIris.
//!
//! Four models from BigLinux's [image-engine-native], written in plain
//! Rust and running on the CPU or on a Vulkan 1.2 GPU, with no ONNX
//! Runtime, PyTorch or Python underneath:
//!
//! | Module         | Model                        | BigLinux package    |
//! |----------------|------------------------------|---------------------|
//! | [`background`] | BiRefNet-lite                | `birefnet-native`   |
//! | [`upscale`]    | Real-ESRGAN general x4v3     | `realesrgan-native` |
//! | [`denoise`]    | SCUNet (color, real PSNR)    | `scunet-native`     |
//! | [`inpaint`]    | Big LaMa                     | `lama-native`       |
//!
//! Every module has the same small API: [`ModelInfo`] with the weights
//! path and the package that installs them, an availability check, a
//! loaded model kept for reuse across calls (loading is cheap, starting
//! the GPU is not), and runs that report [`Stage`]s and stop when the
//! callback breaks. Images go in and come out as [`image::DynamicImage`]:
//! the networks see 8-bit sRGB (16-bit and float sources are converted,
//! float ones through the sRGB transfer function), and alpha is kept
//! where it still means something.
//!
//! Weights are never downloaded. They come only from the system packages
//! (`/usr/share/<package>/…`), or from a per-model environment variable
//! for tests and development (see [`ModelInfo::env`]).
//!
//! Inference is behind the `native` cargo feature; without it every run
//! returns [`AiError::Disabled`], so headless builds stay lean.
//!
//! [image-engine-native]: https://github.com/biglinux/image-engine-native

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// Without engines, the pre/post-processing helpers only serve the tests.
#![cfg_attr(not(feature = "native"), allow(dead_code))]

pub mod background;
pub mod denoise;
mod engine;
pub mod inpaint;
mod memory;
pub mod upscale;

pub use engine::{
    AiError, Availability, Device, ModelInfo, Stage, cancel_on, set_threads, threads,
};

/// Crate version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Whether the native engines are compiled in (cargo feature `native`,
/// enabled by BigIris' `ai` feature). Each model still needs its weights
/// package at run time: see [`ModelInfo::availability`].
pub fn engines_compiled() -> bool {
    cfg!(feature = "native")
}

/// Every model this crate runs, in the order the UI lists them.
pub const MODELS: [&ModelInfo; 4] =
    [&background::MODEL, &upscale::MODEL, &denoise::MODEL, &inpaint::MODEL];

/// Drop every loaded model, giving its memory (and GPU) back. The next
/// run loads it again.
pub fn unload_all() {
    background::unload();
    upscale::unload();
    denoise::unload();
    inpaint::unload();
}

/// IA task identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Task {
    /// Background removal (BiRefNet-lite).
    RemoveBackground,
    /// Super-resolution (Real-ESRGAN).
    Upscale,
    /// Denoise (SCUNet).
    Denoise,
    /// Fill a masked area (LaMa).
    Inpaint,
}

impl Task {
    /// The model that runs this task.
    pub fn model(self) -> &'static ModelInfo {
        match self {
            Task::RemoveBackground => &background::MODEL,
            Task::Upscale => &upscale::MODEL,
            Task::Denoise => &denoise::MODEL,
            Task::Inpaint => &inpaint::MODEL,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!version().is_empty());
    }

    #[test]
    fn every_task_names_a_distinct_package_and_variable() {
        let tasks = [Task::RemoveBackground, Task::Upscale, Task::Denoise, Task::Inpaint];
        for (task, model) in tasks.iter().zip(MODELS) {
            assert_eq!(task.model(), model);
            assert!(model.weights.starts_with(&format!("/usr/share/{}/", model.package)));
            assert!(model.env.starts_with("BIGIRIS_") && model.env.ends_with("_WEIGHTS"));
        }
        let mut packages: Vec<_> = MODELS.iter().map(|m| m.package).collect();
        packages.dedup();
        assert_eq!(packages.len(), 4);
    }
}
