// SPDX-License-Identifier: GPL-3.0-or-later
//! Plumbing the four models share: where their weights live, whether
//! they are installed, errors, progress, and the loaded model kept for
//! reuse.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use image::DynamicImage;
use thiserror::Error;

/// A model and the BigLinux package that installs its weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelInfo {
    /// Name shown to people, e.g. `BiRefNet-lite`.
    pub name: &'static str,
    /// BigLinux package with the weights, e.g. `birefnet-native`.
    pub package: &'static str,
    /// Where the package installs the weights.
    pub weights: &'static str,
    /// Environment variable that points at another weights file, for tests
    /// and development. Empty values are ignored.
    pub env: &'static str,
    /// SPDX licence of the weights.
    pub license: &'static str,
}

impl ModelInfo {
    /// The weights file to load: the environment override when set, else
    /// the packaged path.
    pub fn weights_path(&self) -> PathBuf {
        resolve_weights(std::env::var_os(self.env), self.weights)
    }

    /// Whether this build can run the model right now.
    pub fn availability(&self) -> Availability {
        if !crate::engines_compiled() {
            return Availability::NotBuilt;
        }
        check_weights(&self.weights_path())
    }

    /// Shell command that installs the weights.
    pub fn install_command(&self) -> String {
        format!("sudo pacman -S {}", self.package)
    }

    /// The weights path, or the error that explains why the model cannot
    /// run.
    pub(crate) fn ready_path(&self) -> Result<PathBuf, AiError> {
        match self.availability() {
            Availability::Ready => Ok(self.weights_path()),
            Availability::NotBuilt => Err(AiError::Disabled),
            Availability::Missing { path } => {
                Err(AiError::Missing { model: self.name, package: self.package, path })
            }
            Availability::Unreadable { path, error } => {
                Err(AiError::Unreadable { model: self.name, path, error })
            }
        }
    }
}

fn resolve_weights(env: Option<std::ffi::OsString>, packaged: &str) -> PathBuf {
    match env {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(packaged),
    }
}

/// Whether a model can run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// Weights installed and readable.
    Ready,
    /// This build has no inference engines (cargo feature `native` off).
    NotBuilt,
    /// The weights file is not there: the package is not installed.
    Missing {
        /// Path that was looked up.
        path: PathBuf,
    },
    /// The weights file exists but cannot be read.
    Unreadable {
        /// Path that was looked up.
        path: PathBuf,
        /// Why it could not be read.
        error: String,
    },
}

impl Availability {
    /// The model can run.
    pub fn is_ready(&self) -> bool {
        matches!(self, Availability::Ready)
    }
}

fn check_weights(path: &Path) -> Availability {
    match std::fs::File::open(path).and_then(|file| file.metadata()) {
        Ok(meta) if meta.is_file() && meta.len() > 0 => Availability::Ready,
        Ok(_) => Availability::Unreadable {
            path: path.to_path_buf(),
            error: "não é um arquivo de pesos".into(),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Availability::Missing { path: path.to_path_buf() }
        }
        Err(error) => {
            Availability::Unreadable { path: path.to_path_buf(), error: error.to_string() }
        }
    }
}

/// Where a loaded model runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Device {
    /// The CPU engine.
    Cpu,
    /// A Vulkan GPU, by the name its driver reports.
    Gpu(String),
}

impl std::fmt::Display for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Device::Cpu => f.write_str("CPU"),
            Device::Gpu(name) => write!(f, "GPU: {name}"),
        }
    }
}

/// What a run is doing, for progress bars.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stage {
    /// Loading the weights (and, the first time on a GPU, building its
    /// shaders, which can take seconds). Not cancellable.
    Loading,
    /// The network runs; the share done, from 0 to 1. Models without
    /// progress inside (BiRefNet) report 0 once.
    Running(f32),
}

/// Progress callback: return `ControlFlow::Break(())` to cancel. Runs stop
/// at the next step of the network with [`AiError::Cancelled`].
pub(crate) type Progress<'a> = &'a mut dyn FnMut(Stage) -> ControlFlow<()>;

/// A progress callback that only watches a cancel flag.
pub fn cancel_on(flag: &AtomicBool) -> impl FnMut(Stage) -> ControlFlow<()> + '_ {
    move |_| {
        if flag.load(Ordering::Acquire) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
}

/// Errors of every model.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AiError {
    /// Built without the `native` feature: no engine to run.
    #[error(
        "esta build não inclui os motores de IA. Recompile com `cargo build --features ai` \
         (requer `glslc`, do pacote shaderc)."
    )]
    Disabled,
    /// The weights package is not installed.
    #[error(
        "{model} não está instalado ({path} não existe). Instale com: sudo pacman -S {package}"
    )]
    Missing {
        /// Model name.
        model: &'static str,
        /// Package to install.
        package: &'static str,
        /// Weights path looked up.
        path: PathBuf,
    },
    /// The weights file exists but cannot be read.
    #[error("não foi possível ler os pesos de {model} em {path}: {error}")]
    Unreadable {
        /// Model name.
        model: &'static str,
        /// Weights path.
        path: PathBuf,
        /// Reason.
        error: String,
    },
    /// The progress callback asked to stop.
    #[error("cancelado")]
    Cancelled,
    /// The GPU failed; the run is retried on the CPU before this escapes.
    #[error("GPU: {0}")]
    Gpu(String),
    /// Engine error (weights, worker threads).
    #[error("inferência: {0}")]
    Inference(String),
    /// Bad input or pre/post-processing error.
    #[error("processamento: {0}")]
    Processing(String),
    /// Not enough memory to start; nothing was allocated.
    #[error("memória para IA: {0}")]
    Memory(String),
}

#[cfg(feature = "native")]
impl From<birefnet_native::Error> for AiError {
    // All four crates re-export the same `native_engine::Error`.
    fn from(error: birefnet_native::Error) -> Self {
        use birefnet_native::Error as E;
        match error {
            E::Cancelled => AiError::Cancelled,
            E::Gpu(message) => AiError::Gpu(message),
            E::Input(message) => AiError::Processing(message),
            other => AiError::Inference(other.to_string()),
        }
    }
}

/// Threads a model loaded from now on uses on the CPU; 0 = one per CPU.
static THREADS: AtomicUsize = AtomicUsize::new(0);

/// Set the CPU threads of models loaded from now on (0 = one per CPU the
/// process may use, the default). The GUI leaves a core to the desktop.
pub fn set_threads(threads: usize) {
    THREADS.store(threads, Ordering::Relaxed);
}

/// CPU threads models are loaded with (0 = one per CPU).
pub fn threads() -> usize {
    THREADS.load(Ordering::Relaxed)
}

/// Turn a native progress share into a [`Stage`] call.
#[cfg(feature = "native")]
pub(crate) fn running(progress: Progress<'_>) -> impl FnMut(f32) -> ControlFlow<()> + '_ {
    move |share| progress(Stage::Running(share.clamp(0.0, 1.0)))
}

/// `ControlFlow` of a callback as a result.
pub(crate) fn check(flow: ControlFlow<()>) -> Result<(), AiError> {
    match flow {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(()) => Err(AiError::Cancelled),
    }
}

/// A native model: loaded on a GPU when there is one, else on the CPU.
#[cfg(feature = "native")]
pub(crate) trait NativeModel: Send + Sized + 'static {
    fn load(path: &Path, threads: usize) -> Result<Self, birefnet_native::Error>;
    fn load_cpu(path: &Path, threads: usize) -> Result<Self, birefnet_native::Error>;
    fn gpu(&self) -> Option<&str>;
}

#[cfg(feature = "native")]
macro_rules! native_model {
    ($ty:ty) => {
        impl $crate::engine::NativeModel for $ty {
            fn load(
                path: &std::path::Path,
                threads: usize,
            ) -> Result<Self, birefnet_native::Error> {
                <$ty>::load(path, threads)
            }
            fn load_cpu(
                path: &std::path::Path,
                threads: usize,
            ) -> Result<Self, birefnet_native::Error> {
                <$ty>::load_cpu(path, threads)
            }
            fn gpu(&self) -> Option<&str> {
                <$ty>::gpu(self)
            }
        }
    };
}
#[cfg(feature = "native")]
pub(crate) use native_model;

/// One loaded model, kept between runs.
#[cfg(feature = "native")]
struct Loaded<M> {
    model: M,
    path: PathBuf,
}

/// Lazily loaded, reused model of one kind. Runs of the same model wait
/// for each other; the engines use every thread they were given anyway.
pub(crate) struct Slot<M> {
    #[cfg(feature = "native")]
    loaded: std::sync::Mutex<Option<Loaded<M>>>,
    #[cfg(not(feature = "native"))]
    _model: std::marker::PhantomData<fn() -> M>,
    /// Where the loaded model runs; readable while a run holds the model.
    device: std::sync::Mutex<Option<Device>>,
    /// The GPU failed once: stay on the CPU for this process.
    #[cfg(feature = "native")]
    cpu_only: AtomicBool,
}

/// Memory a model needs before it may start.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Budget {
    /// Weights and fixed buffers, paid once when the model loads.
    pub load: u64,
    /// Everything one run allocates (network and image buffers).
    pub run: u64,
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl<M> Slot<M> {
    pub(crate) const fn new() -> Self {
        Self {
            #[cfg(feature = "native")]
            loaded: std::sync::Mutex::new(None),
            #[cfg(not(feature = "native"))]
            _model: std::marker::PhantomData,
            device: std::sync::Mutex::new(None),
            #[cfg(feature = "native")]
            cpu_only: AtomicBool::new(false),
        }
    }

    /// Where the loaded model runs, `None` before its first load.
    pub(crate) fn device(&self) -> Option<Device> {
        lock(&self.device).clone()
    }

    /// Drop the loaded model (waits for a run in progress).
    pub(crate) fn unload(&self) {
        #[cfg(feature = "native")]
        {
            lock(&self.loaded).take();
        }
        lock(&self.device).take();
    }
}

#[cfg(feature = "native")]
impl<M: NativeModel> Slot<M> {
    /// Load the model when needed (after checking `budget.load +
    /// budget.run` against free memory, else only `budget.run`) and run
    /// `work` on it. A GPU failure drops the GPU model and runs `work`
    /// again on the CPU.
    pub(crate) fn run<R>(
        &self,
        info: &ModelInfo,
        budget: Budget,
        progress: Progress<'_>,
        mut work: impl FnMut(&mut M, Progress<'_>) -> Result<R, AiError>,
    ) -> Result<R, AiError> {
        let path = info.ready_path()?;
        let mut loaded = lock(&self.loaded);
        if loaded.as_ref().is_some_and(|l| l.path != path) {
            *loaded = None;
        }
        let need = if loaded.is_some() { budget.run } else { budget.load + budget.run };
        crate::memory::admit(info.name, need)?;
        if loaded.is_none() {
            check(progress(Stage::Loading))?;
            let cpu = self.cpu_only.load(Ordering::Relaxed);
            *loaded = Some(Loaded { model: self.load(info, &path, cpu)?, path: path.clone() });
        }
        let model = &mut loaded.as_mut().expect("loaded above").model;
        match work(model, progress) {
            Err(AiError::Gpu(error)) => {
                tracing::warn!(model = info.name, %error, "GPU falhou; repetindo na CPU");
                self.cpu_only.store(true, Ordering::Relaxed);
                *loaded = None;
                check(progress(Stage::Loading))?;
                let mut cpu = Loaded { model: self.load(info, &path, true)?, path };
                let result = work(&mut cpu.model, progress);
                *loaded = Some(cpu);
                result
            }
            other => other,
        }
    }

    fn load(&self, info: &ModelInfo, path: &Path, cpu: bool) -> Result<M, AiError> {
        let started = std::time::Instant::now();
        let threads = threads();
        let model = if cpu { M::load_cpu(path, threads) } else { M::load(path, threads) }.map_err(
            |error| match AiError::from(error) {
                AiError::Inference(message) => {
                    AiError::Inference(format!("{}: {message} ({})", info.name, path.display()))
                }
                other => other,
            },
        )?;
        let device = model.gpu().map_or(Device::Cpu, |name| Device::Gpu(name.to_owned()));
        tracing::info!(
            model = info.name,
            %device,
            path = %path.display(),
            ms = started.elapsed().as_millis() as u64,
            "modelo carregado"
        );
        *lock(&self.device) = Some(device);
        Ok(model)
    }
}

/// 8-bit sRGB RGBA view of `input`, as the core encoders write it: float
/// images go through [`bigimage_core::into_srgb_rgba8`] (sRGB transfer),
/// integer ones are already sRGB-encoded (16-bit ones are rounded to 8 bits).
pub(crate) fn display_rgba8(input: &DynamicImage) -> image::RgbaImage {
    match input {
        DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_) => {
            bigimage_core::into_srgb_rgba8(input.clone())
        }
        other => other.to_rgba8(),
    }
}

/// Bytes of the transient copy [`display_rgba8`] makes beyond the RGBA8
/// result (the float clone).
pub(crate) fn float_copy_bytes(input: &DynamicImage) -> u64 {
    match input {
        DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_) => u64::from(input.width())
            .saturating_mul(u64::from(input.height()))
            .saturating_mul(u64::from(input.color().bytes_per_pixel())),
        _ => 0,
    }
}

/// The RGB bytes of an RGBA image, row by row.
pub(crate) fn rgb_bytes(rgba: &image::RgbaImage) -> Vec<u8> {
    rgba.pixels().flat_map(|p| [p[0], p[1], p[2]]).collect()
}

/// `rgb` as an image of `width × height`, with `alpha` (one byte per
/// pixel) when the source had an alpha channel, else opaque RGB.
pub(crate) fn assemble(
    rgb: Vec<u8>,
    alpha: Option<&[u8]>,
    width: u32,
    height: u32,
) -> Result<DynamicImage, AiError> {
    let wrong = || AiError::Processing("tamanho de saída inesperado".into());
    match alpha {
        None => image::RgbImage::from_raw(width, height, rgb)
            .map(DynamicImage::ImageRgb8)
            .ok_or_else(wrong),
        Some(alpha) => {
            if alpha.len() * 3 != rgb.len() {
                return Err(wrong());
            }
            let rgba: Vec<u8> = rgb
                .as_chunks::<3>()
                .0
                .iter()
                .zip(alpha)
                .flat_map(|(p, &a)| [p[0], p[1], p[2], a])
                .collect();
            image::RgbaImage::from_raw(width, height, rgba)
                .map(DynamicImage::ImageRgba8)
                .ok_or_else(wrong)
        }
    }
}

/// The alpha bytes of `rgba` when `source` has an alpha channel.
pub(crate) fn alpha_of(source: &DynamicImage, rgba: &image::RgbaImage) -> Option<Vec<u8>> {
    source.color().has_alpha().then(|| rgba.pixels().map(|p| p[3]).collect())
}

/// `width × height` as `usize`s for the engines.
pub(crate) fn dims(width: u32, height: u32) -> (usize, usize) {
    (width as usize, height as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_path_prefers_a_non_empty_override() {
        let packaged = "/usr/share/x-native/x.weights";
        assert_eq!(resolve_weights(None, packaged), PathBuf::from(packaged));
        assert_eq!(resolve_weights(Some("".into()), packaged), PathBuf::from(packaged));
        assert_eq!(resolve_weights(Some("/tmp/w".into()), packaged), PathBuf::from("/tmp/w"));
    }

    #[test]
    fn availability_tells_missing_from_unreadable() {
        let dir = tempdir();
        let missing = dir.join("absent.weights");
        assert_eq!(check_weights(&missing), Availability::Missing { path: missing.clone() });
        assert!(matches!(check_weights(&dir), Availability::Unreadable { .. }));
        let empty = dir.join("empty.weights");
        std::fs::write(&empty, b"").unwrap();
        assert!(matches!(check_weights(&empty), Availability::Unreadable { .. }));
        let real = dir.join("real.weights");
        std::fs::write(&real, b"BIREFNET").unwrap();
        assert_eq!(check_weights(&real), Availability::Ready);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_model_error_names_the_package_to_install() {
        let info = ModelInfo {
            name: "Teste",
            package: "teste-native",
            weights: "/nonexistent/bigiris/teste.weights",
            env: "BIGIRIS_TEST_ENGINE_UNSET_WEIGHTS",
            license: "MIT",
        };
        assert_eq!(info.install_command(), "sudo pacman -S teste-native");
        if !crate::engines_compiled() {
            assert!(matches!(info.ready_path(), Err(AiError::Disabled)));
            return;
        }
        let error = info.ready_path().unwrap_err();
        assert!(matches!(error, AiError::Missing { package: "teste-native", .. }));
        assert!(error.to_string().contains("sudo pacman -S teste-native"), "{error}");
    }

    #[test]
    fn device_reads_as_cpu_or_named_gpu() {
        assert_eq!(Device::Cpu.to_string(), "CPU");
        assert_eq!(Device::Gpu("Intel(R) Graphics".into()).to_string(), "GPU: Intel(R) Graphics");
    }

    #[test]
    fn cancel_flag_breaks_the_run() {
        let flag = AtomicBool::new(false);
        let mut progress = cancel_on(&flag);
        assert!(check(progress(Stage::Loading)).is_ok());
        flag.store(true, Ordering::Release);
        assert!(matches!(check(progress(Stage::Running(0.5))), Err(AiError::Cancelled)));
    }

    #[test]
    fn linear_float_sources_are_srgb_encoded_for_the_networks() {
        // Linear 0.5 is sRGB ~188; a bare quantisation would give 128.
        let float = DynamicImage::ImageRgba32F(image::ImageBuffer::from_pixel(
            2,
            1,
            image::Rgba([0.5f32, 0.0, 2.0, 1.0]),
        ));
        let px = display_rgba8(&float).get_pixel(0, 0).0;
        assert!((187..=189).contains(&px[0]), "{px:?}");
        assert_eq!(&px[1..], &[0, 255, 255]);
        assert_eq!(float_copy_bytes(&float), 2 * 16);
        let rgb =
            DynamicImage::ImageRgb8(image::RgbImage::from_pixel(1, 1, image::Rgb([128, 7, 9])));
        assert_eq!(display_rgba8(&rgb).get_pixel(0, 0).0, [128, 7, 9, 255]);
        assert_eq!(float_copy_bytes(&rgb), 0);
        // 16-bit is rounded to 8 bits.
        let deep = DynamicImage::ImageRgb16(image::ImageBuffer::from_pixel(
            1,
            1,
            image::Rgb([0xff00u16, 0x8080, 0]),
        ));
        assert_eq!(display_rgba8(&deep).get_pixel(0, 0).0, [254, 128, 0, 255]);
    }

    #[test]
    fn assemble_keeps_alpha_only_when_given() {
        let rgb = vec![1, 2, 3, 4, 5, 6];
        let opaque = assemble(rgb.clone(), None, 2, 1).unwrap();
        assert!(matches!(opaque, DynamicImage::ImageRgb8(_)));
        let alpha = [10, 20];
        let rgba = assemble(rgb.clone(), Some(&alpha), 2, 1).unwrap().to_rgba8();
        assert_eq!(rgba.as_raw(), &[1, 2, 3, 10, 4, 5, 6, 20]);
        assert!(assemble(rgb, Some(&[1]), 2, 1).is_err());
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bigimage-ai-engine-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
