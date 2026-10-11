// SPDX-License-Identifier: GPL-3.0-or-later
//! Background removal — wraps a BiRefNet-lite ONNX model to produce an
//! alpha mask, then composites it onto the original to yield an RGBA
//! image whose background is transparent.
//!
//! **Feature gate:** real inference lives behind `onnx`. Without the
//! feature, [`remove_background`] returns a clear error so the CLI /
//! dialog can offer a "rebuild with `--features ai` / install
//! `onnxruntime`" message instead of silently failing.
//!
//! **Batch use:** for multi-file dialogs, prefer [`BgSession`] over
//! [`remove_background`]. The session loads the 224 MB of ONNX weights
//! once and reuses them across every `process()` call — a 50-image
//! batch goes from "load + run × 50" to "load once + run × 50".
//!
//! Licensing: BiRefNet ships under MIT. We refuse any model whose SPDX
//! identifier isn't on the FOSS allowlist in [`super::download`].

use image::DynamicImage;
#[cfg(feature = "onnx")]
use image::GenericImageView;
use thiserror::Error;

#[cfg(feature = "onnx")]
use crate::download;
use crate::download::{DownloadError, ModelSource};

#[cfg(any(feature = "onnx", test))]
mod memory;

/// BiRefNet-lite ONNX — mirror oficial da comunidade ONNX, exportado a
/// partir do repositório canônico [`ZhengPeng7/BiRefNet_lite`] (MIT)
/// pela equipe Xenova / onnx-community. Revisão fixada por commit do repositório;
/// o hash abaixo cobre o `.onnx` FP32 (~214 MB) tal como publicado em
/// 2024-09. Se o mirror re-envelopar os pesos, a verificação em
/// [`super::download::ensure`] rejeita o arquivo antes de gravá-lo na
/// cache do usuário.
pub const BIREFNET_LITE: ModelSource = ModelSource {
    id: "birefnet-lite",
    url: "https://huggingface.co/onnx-community/BiRefNet_lite-ONNX/resolve/de15b22ba131738a16dff04aab8bdf8dc32e3ac1/onnx/model.onnx",
    license_spdx: "MIT",
    sha256: "5600024376f572a557870a5eb0afb1e5961636bef4e1e22132025467d0f03333",
    size_bytes: 224_005_088,
    description: "BiRefNet-lite — remoção de fundo FP32, MIT (onnx-community mirror).",
};

/// Inference resolution the BiRefNet ONNX export expects.
#[cfg(feature = "onnx")]
const INPUT_SIDE: usize = 1024;
#[cfg(feature = "onnx")]
const INPUT_PIXELS: usize = INPUT_SIDE * INPUT_SIDE;

/// Progress signal emitted by [`remove_background_with_progress`]. The
/// UI layer uses it to drive a `gtk::ProgressBar`; the CLI discards it.
/// Both variants are `Copy` so the callback can be `FnMut(BgStage)`
/// without borrowing gymnastics in worker threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BgStage {
    /// Downloading the ONNX weights. First call only; hash-verified cache
    /// hits skip this phase entirely. `total` may be 0 briefly if the
    /// server didn't send `Content-Length` — callers should guard.
    Download {
        /// Bytes downloaded so far.
        done: u64,
        /// Expected total bytes (from `Content-Length` or the catalogue
        /// `size_bytes`; 0 if neither is available).
        total: u64,
    },
    /// Weights on disk, inference running. Atomic from the UI's POV —
    /// we don't get intermediate signal out of BiRefNet — so we emit
    /// this once right before `session.run()` and call it a day.
    Infer,
}

/// Errors that can escape [`remove_background`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BgError {
    /// The `onnx` cargo feature wasn't enabled at build time — no
    /// inference backend to run.
    #[error(
        "feature `onnx` desabilitada nesta build. Recompile com `cargo build --features ai` \
         e instale `onnxruntime` (ArchLinux: `pacman -S onnxruntime`)."
    )]
    OnnxDisabled,
    /// Downloading / caching the model failed.
    #[error("modelo: {0}")]
    Model(#[from] DownloadError),
    /// ORT / inference backend error.
    #[error("inferência: {0}")]
    Inference(String),
    /// Preprocessing / postprocessing error.
    #[error("processamento: {0}")]
    Processing(String),
    /// Insufficient or unknown memory budget; model loading is refused.
    #[error("memória para IA: {0}")]
    Memory(String),
}

/// Check the conservative RAM/cgroup/address-space budget before downloading.
/// This is an admission check, not a hard quota or a guarantee against OOM.
pub fn check_resources() -> Result<(), BgError> {
    #[cfg(feature = "onnx")]
    {
        memory::MemoryGuard::new().map(|_| ())
    }
    #[cfg(not(feature = "onnx"))]
    {
        Err(BgError::OnnxDisabled)
    }
}

/// One-shot wrapper: ensure the model is on disk + load a session +
/// run inference + drop everything. Convenient for the CLI's single-
/// file path. **Don't use this in a batch loop** — every call reloads
/// 224 MB of weights. See [`BgSession`] for the cached variant.
///
/// Pure in-memory; safe to call from worker threads.
pub fn remove_background(input: &DynamicImage) -> Result<DynamicImage, BgError> {
    remove_background_with_progress(input, |_| {})
}

/// Same as [`remove_background`], but forwards a [`BgStage`] signal at
/// each phase change so the caller can drive a progress bar.
pub fn remove_background_with_progress(
    input: &DynamicImage,
    mut progress: impl FnMut(BgStage),
) -> Result<DynamicImage, BgError> {
    let mut sess = BgSession::new(&mut progress)?;
    sess.process(input)
}

/// Cached background-removal pipeline.
///
/// Constructing a [`BgSession`] downloads (if needed) and loads the
/// BiRefNet weights into an `ort` session. Each call to [`process`]
/// reuses that session. Inference time and RAM vary with the runtime and
/// hardware; consult the measured release report. Batch flows
/// (dialog with N files, CLI passing many paths) should construct one
/// session and call `process` per file.
///
/// [`process`]: BgSession::process
pub struct BgSession {
    /// Loaded ORT inference session. Behind `onnx` so non-AI builds
    /// don't pay the dep cost.
    #[cfg(feature = "onnx")]
    session: ort::session::Session,
    #[cfg(feature = "onnx")]
    memory: memory::MemoryGuard,
}

impl BgSession {
    /// Ensure the model is on disk (downloading if needed) and load it
    /// into a reusable inference session. `progress` fires with
    /// [`BgStage::Download`] during download (when the cache is cold)
    /// and once with [`BgStage::Infer`] when the session is ready.
    pub fn new(mut progress: impl FnMut(BgStage)) -> Result<Self, BgError> {
        #[cfg(not(feature = "onnx"))]
        {
            let _ = &mut progress;
            Err(BgError::OnnxDisabled)
        }

        #[cfg(feature = "onnx")]
        {
            let mut memory = memory::MemoryGuard::new()?;
            let model_path = download::ensure(&BIREFNET_LITE, |done, total| {
                progress(BgStage::Download { done, total });
            })?;
            progress(BgStage::Infer);
            memory.check(0)?;
            let before = memory::resident_bytes()?;
            let session = build_session(&model_path)?;
            memory.account_native_growth(before)?;
            Ok(Self { session, memory })
        }
    }

    /// Run BiRefNet on `input` and return an RGBA image whose alpha
    /// channel tracks the foreground mask. Reuses the loaded session;
    /// no disk I/O.
    pub fn process(&mut self, input: &DynamicImage) -> Result<DynamicImage, BgError> {
        #[cfg(not(feature = "onnx"))]
        {
            let _ = input;
            Err(BgError::OnnxDisabled)
        }

        #[cfg(feature = "onnx")]
        {
            use image::imageops::FilterType;

            let (orig_w, orig_h) = input.dimensions();
            if u64::from(orig_w) * u64::from(orig_h) * 4 > 512 * 1024 * 1024 {
                return Err(BgError::Processing(
                    "imagem excede o limite de saída RGBA de 512 MiB".into(),
                ));
            }
            // RGBA source/output plus the full-size mask, beyond the model's
            // fixed tensor budget. Memory is rechecked for every input.
            self.memory.check(u64::from(orig_w) * u64::from(orig_h) * 9)?;
            let resized = input
                .resize_exact(INPUT_SIDE as u32, INPUT_SIDE as u32, FilterType::Lanczos3)
                .to_rgb8();

            // Preprocess: HWC u8 → CHW f32 normalised by ImageNet stats.
            // Walking each channel as a contiguous slice lets the inner
            // loop auto-vectorise; the previous nested-index version was
            // bounds-checked on every iteration and sat ~5x slower.
            let mean = [0.485f32, 0.456, 0.406];
            let std = [0.229f32, 0.224, 0.225];
            let raw: &[u8] = resized.as_raw();
            let mut tensor = ndarray::Array4::<f32>::zeros((1, 3, INPUT_SIDE, INPUT_SIDE));
            {
                let plane = INPUT_PIXELS;
                let dst = tensor.as_slice_mut().expect("CHW tensor must be contiguous");
                for c in 0..3 {
                    let m = mean[c];
                    let s = std[c];
                    let dst_chan = &mut dst[c * plane..(c + 1) * plane];
                    for (i, out) in dst_chan.iter_mut().enumerate() {
                        let v = f32::from(raw[i * 3 + c]) / 255.0;
                        *out = (v - m) / s;
                    }
                }
            }

            let before = memory::resident_bytes()?;
            let mask = run_session(&mut self.session, tensor)?;
            self.memory.account_native_growth(before)?;

            // Mask back to 1024² u8, then resize to original dims.
            let mask_data: &[f32] = mask.as_slice().expect("CHW mask must be contiguous");
            let mut mask_u8: Vec<u8> = Vec::with_capacity(INPUT_PIXELS);
            for &v in &mask_data[..INPUT_PIXELS] {
                mask_u8.push(logit_alpha(v));
            }
            let mask_img = image::ImageBuffer::<image::Luma<u8>, Vec<u8>>::from_vec(
                INPUT_SIDE as u32,
                INPUT_SIDE as u32,
                mask_u8,
            )
            .expect("mask buffer matches dimensions");
            let mask_full = image::imageops::resize(
                &mask_img,
                orig_w,
                orig_h,
                image::imageops::FilterType::Lanczos3,
            );

            // Composite RGB src + mask alpha → RGBA dest. Both source
            // and dest are HWC; iterate as flat slices.
            let src_rgba = input.to_rgba8();
            let src: &[u8] = src_rgba.as_raw();
            let mask_raw: &[u8] = mask_full.as_raw();
            let n = (orig_w as usize) * (orig_h as usize);
            let mut out_buf: Vec<u8> = Vec::with_capacity(n * 4);
            for i in 0..n {
                out_buf.push(src[i * 4]);
                out_buf.push(src[i * 4 + 1]);
                out_buf.push(src[i * 4 + 2]);
                out_buf
                    .push(((u16::from(src[i * 4 + 3]) * u16::from(mask_raw[i]) + 127) / 255) as u8);
            }
            let out =
                image::ImageBuffer::<image::Rgba<u8>, Vec<u8>>::from_vec(orig_w, orig_h, out_buf)
                    .expect("RGBA buffer matches dimensions");
            Ok(DynamicImage::ImageRgba8(out))
        }
    }
}

#[cfg(any(feature = "onnx", test))]
fn logit_alpha(value: f32) -> u8 {
    let probability = if value >= 0.0 {
        1.0 / (1.0 + (-value).exp())
    } else {
        let e = value.exp();
        e / (1.0 + e)
    };
    (probability * 255.0).round() as u8
}

/// Build an ORT session for `path`, capping intra-op threads so the
/// inference doesn't starve the GUI / file-manager processes of CPU.
/// Default ORT behavior is "use every core for one op" — fine for
/// servers, hostile for an interactive desktop tool.
#[cfg(feature = "onnx")]
fn build_session(path: &std::path::Path) -> Result<ort::session::Session, BgError> {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    let intra = cores.saturating_sub(1).clamp(1, 4);
    // ort's dynamic loader panics on a missing/incompatible optional library.
    // Translate that into a completed GUI/CLI error instead of a dead worker.
    std::panic::catch_unwind(|| {
        ort::session::Session::builder()
            .and_then(|b| b.with_intra_threads(intra))
            .and_then(|b| b.commit_from_file(path))
            .map_err(|e| BgError::Inference(format!("load session: {e}")))
    })
    .unwrap_or_else(|_| {
        Err(BgError::Inference(
            "ONNX Runtime indisponível ou incompatível; instale onnxruntime para usar a IA".into(),
        ))
    })
}

#[cfg(feature = "onnx")]
fn run_session(
    session: &mut ort::session::Session,
    input: ndarray::Array4<f32>,
) -> Result<ndarray::Array4<f32>, BgError> {
    use ort::value::Tensor;

    let input_tensor =
        Tensor::from_array(input).map_err(|e| BgError::Processing(format!("tensor: {e}")))?;
    let inputs = ort::inputs![input_tensor];
    let outputs = session.run(inputs).map_err(|e| BgError::Inference(format!("run: {e}")))?;

    // BiRefNet normally emits a single (1,1,H,W) float tensor. Grab the
    // first one regardless of the exact output name so we survive minor
    // model revisions.
    let (_, first) =
        outputs.iter().next().ok_or_else(|| BgError::Inference("sem saidas".to_string()))?;
    let (shape, data) = first
        .try_extract_tensor::<f32>()
        .map_err(|e| BgError::Inference(format!("extract: {e}")))?;
    let shape_slice: &[i64] = shape;
    let dims: Vec<usize> = shape_slice.iter().map(|&d| d as usize).collect();
    if dims != [1, 1, INPUT_SIDE, INPUT_SIDE] || data.len() != INPUT_PIXELS {
        return Err(BgError::Processing(format!("saída não 4D: dims = {dims:?}")));
    }
    ndarray::Array4::<f32>::from_shape_vec((dims[0], dims[1], dims[2], dims[3]), data.to_vec())
        .map_err(|e| BgError::Processing(format!("reshape: {e}")))
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
        assert!(logit_alpha(-1.0) > 0 && logit_alpha(1.0) < 255);
    }

    #[test]
    fn birefnet_lite_is_on_license_allowlist() {
        assert!(crate::download::allowed_licenses().contains(&BIREFNET_LITE.license_spdx));
    }

    #[cfg(not(feature = "onnx"))]
    #[test]
    fn without_onnx_returns_clear_error() {
        let img = DynamicImage::new_rgb8(8, 8);
        let err = remove_background(&img).unwrap_err();
        assert!(matches!(err, BgError::OnnxDisabled));
    }

    #[cfg(not(feature = "onnx"))]
    #[test]
    fn session_new_returns_clear_error_without_feature() {
        // BgSession holds an ort::Session under `onnx` and nothing
        // otherwise — neither implements Debug, so we pattern-match
        // instead of `.unwrap_err()` which would require Debug on Ok.
        match BgSession::new(|_| {}) {
            Err(BgError::OnnxDisabled) => {}
            Err(other) => panic!("expected OnnxDisabled, got {other}"),
            Ok(_) => panic!("expected OnnxDisabled error without `onnx` feature"),
        }
    }
}
