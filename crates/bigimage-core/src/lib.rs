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

pub use adjust::{adjust_file, adjust_file_with_options, AdjustOps};
pub use animate::{make_gif, make_gif_with_policy, AnimateOptions, LoopMode};
pub use convert::{
    convert_file, convert_file_to, into_srgb_rgba8, ConvertOutcome, OverwritePolicy,
};
pub use crop::{crop_file, crop_file_with_options, CropRect};
pub use encode::EncodeOptions;
pub use extent::{
    extent_file, extent_file_with_options, parse_hex_color, ExtentOptions, ExtentSize, Gravity,
};
pub use flip::{flip_file, flip_file_with_options, FlipAxis};
pub use format::Format;
pub use metadata::{Metadata, MetadataLoss, MetadataSupport, SourceMetadata};
pub use preview::{PreviewOp, PreviewSession};
pub use resize::{resize_file, Filter, ResizeMode};
pub use rotate::{
    rotate_file, rotate_file_auto, rotate_file_auto_with_options, rotate_file_with_options,
    Rotation,
};

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

/// Format of an image file by its content (magic bytes), with the extension
/// as fallback, without decoding it. This is the format transforms keep (and
/// name their output after) when no target format is given.
pub fn sniff_format(path: impl AsRef<std::path::Path>) -> Option<Format> {
    pipeline::sniff_format(path.as_ref())
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
///
/// # Depth and alpha policy per target
///
/// Every pipeline (`convert`, `resize`, `crop`, …) funnels through the same
/// preparation step, so this table is the whole contract. "8-bit" means a
/// 16-bit or float source is reduced; "adds alpha" means an opaque source is
/// written with an opaque alpha channel.
///
/// | Target              | Written as                                                       |
/// |---------------------|------------------------------------------------------------------|
/// | PNG                 | Source type kept (L/La/Rgb/Rgba, 8 or 16-bit); float → `Rgb16`/`Rgba16` |
/// | TIFF                | As PNG, except gray+alpha (`La8`/`La16`) → `Rgba8`/`Rgba16` (no La TIFF encoder) |
/// | PNM (PGM/PPM)       | `L8`/`L16`/`Rgb8`/`Rgb16` kept; alpha flattened over white keeping depth; float → `Rgb16` |
/// | JPEG                | `Rgb8`; alpha flattened over white; 8-bit                         |
/// | AVIF                | `Rgba8`: 8-bit, adds alpha                                        |
/// | WebP, GIF, BMP, TGA, QOI | `Rgba8`: 8-bit, adds alpha                                   |
/// | ICO                 | `Rgba8`, longest edge ≤ 256 px                                    |
/// | OpenEXR             | `Rgba32F`, linear light                                           |
/// | Radiance HDR        | `Rgb32F`, linear light                                            |
///
/// PNM is written as binary PGM (P5) for gray and PPM (P6) for colour.
/// The transform pipelines name it `.pgm`/`.ppm` accordingly (see
/// [`Format::extension_for`]); with `save_image` the caller picks the path.
///
/// Float images are scene-linear; integer images are sRGB-encoded. Crossing
/// between the two applies the sRGB transfer function (clamping HDR values
/// above 1.0). Float outputs replace NaN and -inf with 0 and clamp +inf to
/// a large finite value the encoders can represent. PNG and JPEG outputs
/// carry a generated sRGB ICC profile; no source metadata is written (see [`save_image_preserving_metadata`]).
pub fn save_image(
    image: image::DynamicImage,
    output: impl AsRef<std::path::Path>,
    target: Format,
    options: &EncodeOptions,
    policy: OverwritePolicy,
) -> Result<ConvertOutcome> {
    pipeline::encode_and_cleanup(image, output.as_ref().to_path_buf(), target, options, policy)
}

/// Decode an image for an in-place re-save: upright pixels kept in the
/// source colour space, plus the source EXIF and ICC blobs. Pair with
/// [`save_image_preserving_metadata`].
///
/// With a process backend configured, this goes through the decoder
/// installed by [`backend::configure_resave`]; if none was installed the
/// pixels come back sRGB-normalised with empty metadata (nothing can be
/// re-attached, and no codec runs in this process).
pub fn open_image_for_resave(
    path: impl AsRef<std::path::Path>,
) -> image::ImageResult<(image::DynamicImage, SourceMetadata)> {
    let path = path.as_ref();
    if let Some(decode) = backend::configured_resave() {
        return decode(path).map(|(image, _, meta)| (image, meta));
    }
    if backend::configured().is_some() {
        tracing::warn!(
            ?path,
            "backend sem decodificação para regravação; metadados não preservados"
        );
        return open_image(path).map(|image| (image, SourceMetadata::default()));
    }
    pipeline::decode_local_with(path, false).map(|(image, _, meta)| (image, meta))
}

/// Like [`save_image`], but writes the source metadata back — for saving
/// edits over the original file without losing capture date, camera, GPS
/// or a wide-gamut colour profile.
///
/// `image` must be in the colour space described by `source.icc` (as
/// returned by [`open_image_for_resave`]) and already upright: EXIF is
/// re-attached with `Orientation` reset to 1. Where the target cannot embed
/// that profile (or its colour model does not match the written pixels),
/// pixels are converted to sRGB instead. What each format keeps is reported
/// by [`metadata::preserved_by`]; what a particular save dropped, by
/// [`save_image_preserving_metadata_report`]. `options.strip_metadata` is
/// ignored: the whole point of this call is to keep the user's own metadata.
///
/// EXIF `PixelXDimension`/`PixelYDimension` are updated to the saved size
/// and the embedded IFD1 thumbnail (which would show the image before the
/// edit) is dropped. XMP is not carried: the vendored encoders cannot write
/// it.
///
/// `options` is used as given. For an in-place JPEG save pass the source's
/// quality, e.g. `quality: metadata::estimate_jpeg_quality(path)`, or the
/// encoder default (75) re-compresses a high-quality original.
pub fn save_image_preserving_metadata(
    image: image::DynamicImage,
    output: impl AsRef<std::path::Path>,
    target: Format,
    options: &EncodeOptions,
    policy: OverwritePolicy,
    source: &SourceMetadata,
) -> Result<ConvertOutcome> {
    save_image_preserving_metadata_report(image, output, target, options, policy, source)
        .map(|report| report.outcome)
}

/// Result of [`save_image_preserving_metadata_report`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResaveOutcome {
    /// What happened to the output file.
    pub outcome: ConvertOutcome,
    /// Source metadata that is not in the written file (empty when
    /// everything was kept or the source had none).
    pub dropped: Vec<metadata::MetadataLoss>,
}

/// [`save_image_preserving_metadata`], also reporting which source
/// metadata could not be written, so the UI can tell the user (e.g. "the
/// colour profile was converted to sRGB", "EXIF too large for JPEG").
pub fn save_image_preserving_metadata_report(
    image: image::DynamicImage,
    output: impl AsRef<std::path::Path>,
    target: Format,
    options: &EncodeOptions,
    policy: OverwritePolicy,
    source: &SourceMetadata,
) -> Result<ResaveOutcome> {
    use metadata::MetadataLoss;
    const MAX_BLOB: usize = 4 * 1024 * 1024;
    // JPEG APP1 segments hold at most 65533 bytes, including `Exif\0\0`.
    const MAX_JPEG_EXIF: usize = 65_533 - 6;
    let mut image = image;
    let support = metadata::preserved_by(target);
    let mut extras = pipeline::Extras::default();
    let mut dropped = Vec::new();
    if let Some(icc) = source.icc.as_deref().filter(|icc| !icc.is_empty()) {
        let gray = convert::writes_gray(image.color(), target);
        let matches_pixels = match icc.get(16..20) {
            Some(b"RGB ") => !gray,
            Some(b"GRAY") => gray,
            _ => false,
        };
        if support.icc && matches_pixels && icc.len() <= MAX_BLOB {
            extras.icc = Some(icc.to_vec());
        } else {
            let loss = match color::normalize_srgb_applied(&mut image, icc) {
                Ok(true) if !support.icc => MetadataLoss::IccUnsupported,
                Ok(true) if icc.len() > MAX_BLOB => MetadataLoss::IccTooLarge,
                Ok(true) => MetadataLoss::IccMismatched,
                Ok(false) => MetadataLoss::IccDiscarded,
                Err(error) => {
                    tracing::warn!(%error, "perfil ICC de origem descartado ao regravar");
                    MetadataLoss::IccDiscarded
                }
            };
            dropped.push(loss);
        }
    }
    if let Some(exif) = source.exif.as_deref().filter(|exif| !exif.is_empty()) {
        let limit = if target == Format::Jpeg { MAX_JPEG_EXIF } else { MAX_BLOB };
        match metadata::resave_exif(exif, Some((image.width(), image.height()))) {
            _ if !support.exif => dropped.push(MetadataLoss::ExifUnsupported),
            None => dropped.push(MetadataLoss::ExifInvalid),
            Some(exif) if exif.len() > limit => dropped.push(MetadataLoss::ExifTooLarge),
            Some(exif) => extras.exif = Some(exif),
        }
    }
    let outcome = pipeline::encode_and_cleanup_with(
        image,
        output.as_ref().to_path_buf(),
        target,
        options,
        policy,
        &extras,
    )?;
    Ok(ResaveOutcome { outcome, dropped })
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
