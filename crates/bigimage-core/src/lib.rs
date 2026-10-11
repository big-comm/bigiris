// SPDX-License-Identifier: GPL-3.0-or-later
//! bigimage-core — shared image-processing library.
//!
//! Brand-agnostic on purpose: this crate must stay usable if the project ever
//! splits into multiple binaries. It owns decode/encode/transform pipelines;
//! GUI code never lives here.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::PathBuf;

use thiserror::Error;

pub mod adjust;
pub mod animate;
pub mod backend;
pub mod color;
mod container_bounds;
pub mod content;
mod convert;
pub mod crop;
mod encode;
pub mod extent;
pub mod flip;
mod format;
pub mod metadata;
mod pipeline;
pub mod preview;
pub mod resize;
pub mod resources;
pub mod rotate;

pub use adjust::{adjust_file, AdjustOps};
pub use animate::{make_gif, AnimateOptions, LoopMode};
pub use convert::{convert_file, convert_file_to, ConvertOutcome, OverwritePolicy};
pub use crop::{crop_file, CropRect};
pub use encode::EncodeOptions;
pub use extent::{extent_file, parse_hex_color, ExtentOptions, ExtentSize, Gravity};
pub use flip::{flip_file, FlipAxis};
pub use format::Format;
pub use metadata::Metadata;
pub use preview::{PreviewOp, PreviewSession};
pub use resize::{resize_file, Filter, ResizeMode};
pub use rotate::{rotate_file, rotate_file_auto, Rotation};

/// Crate version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Open and decode an image, dispatching the decoder by **content** (magic
/// bytes) instead of file extension.
///
/// `image::open` routes through the extension, so files like
/// `foo.png-chunking-3510624803-2-0` (Nextcloud sync), `foo.png.part`
/// (Firefox), `foo.png.crdownload` (Chrome), `foo.tmp` and friends fail with
/// "extension not recognized as an image format" even though the bytes are a
/// valid PNG/JPEG/etc. Sniffing the header avoids the false negative; on a
/// well-named file it costs the same.
pub fn open_image(path: impl AsRef<std::path::Path>) -> image::ImageResult<image::DynamicImage> {
    let path = path.as_ref();
    pipeline::decode_image(path).map(|(img, _)| img)
}

/// Read just the dimensions of an image, dispatching the decoder by content.
/// See [`open_image`] for why this matters.
pub fn image_dimensions(path: impl AsRef<std::path::Path>) -> image::ImageResult<(u32, u32)> {
    if let Some(backend) = backend::configured() {
        return (backend.dimensions)(path.as_ref());
    }
    pipeline::bounded_dimensions(path.as_ref())
}

/// Encode pixels atomically with shared color, format and privacy policy.
/// This is also used by the viewer and AI outputs instead of ad-hoc writes.
pub fn save_image(
    image: image::DynamicImage,
    output: impl AsRef<std::path::Path>,
    target: Format,
    options: &EncodeOptions,
    policy: OverwritePolicy,
) -> Result<ConvertOutcome> {
    pipeline::encode_and_cleanup(image, output.as_ref().to_path_buf(), target, options, policy)
}

/// Errors returned by core pipelines.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BigImageError {
    /// IO failure.
    #[error("io: {0}")]
    Io(#[source] std::io::Error),

    /// User-supplied argument or path is malformed.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// Requested format is not implemented.
    #[error("unsupported format: {0}")]
    UnsupportedFormat(String),

    /// Decoder failed on a given file.
    #[error("decode failed for {path}: {source}")]
    Decode {
        /// Path of the file that failed to decode.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: image::ImageError,
    },

    /// Encoder failed while writing a target.
    #[error("encode failed for {path}: {source}")]
    Encode {
        /// Destination path we were writing to.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: image::ImageError,
    },

    /// Pipeline cancelled by caller.
    #[error("cancelled")]
    Cancelled,

    /// Catch-all for messages not yet promoted to variants.
    #[error("{0}")]
    Other(String),
}

/// Alias for pipeline results.
pub type Result<T> = std::result::Result<T, BigImageError>;

/// One conversion job, input to output. Left intentionally thin in M1 —
/// batch orchestration (`Pipeline` trait with progress + cancel) will layer
/// on top once more than a single transform type exists.
#[derive(Debug, Clone)]
pub struct ConvertJob {
    /// Source path.
    pub input: PathBuf,
    /// Destination path (resolved by naming strategy upstream).
    pub output: PathBuf,
    /// Target format.
    pub target_format: Format,
}

impl ConvertJob {
    /// Create a new job with input, output, and target format.
    pub fn new(
        input: impl Into<PathBuf>,
        output: impl Into<PathBuf>,
        target_format: Format,
    ) -> Self {
        Self { input: input.into(), output: output.into(), target_format }
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
    fn convert_job_roundtrip() {
        let job = ConvertJob::new("a.jpg", "a.png", Format::Png);
        assert_eq!(job.target_format, Format::Png);
    }

    #[test]
    fn sniffed_helpers_open_files_with_misleading_extensions() {
        // Regression: Nextcloud `.chunking-*`, browser `.part`/`.crdownload`,
        // and `.tmp` files routinely contain valid PNG/JPEG bytes under a
        // suffix that `image::open` / `image::image_dimensions` reject.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.png");
        image::RgbImage::from_pixel(8, 4, image::Rgb([10, 20, 30])).save(&real).unwrap();
        let chunked = dir.path().join("real.png-chunking-3510624803-2-0");
        std::fs::copy(&real, &chunked).unwrap();

        // Upstream API fails on the bad extension…
        assert!(image::open(&chunked).is_err());
        assert!(image::image_dimensions(&chunked).is_err());
        // …our sniffed helpers handle it.
        assert_eq!(image_dimensions(&chunked).unwrap(), (8, 4));
        assert_eq!(open_image(&chunked).unwrap().to_rgb8().dimensions(), (8, 4));
    }
}
