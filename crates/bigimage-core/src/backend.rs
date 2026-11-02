// SPDX-License-Identifier: GPL-3.0-or-later
//! Optional process boundary supplied by the executable. Library users may
//! retain local decoding; BigIris installs a sandboxed backend before any UI.
use crate::Format;
use image::{DynamicImage, ImageResult};
use std::path::Path;
use std::sync::OnceLock;

/// Decoded, upright pixels normalized to sRGB when an ICC profile is present.
pub type Decoded = (DynamicImage, Option<Format>);
/// Process backend callbacks. No image codec runs in the calling process.
pub struct Backend {
    /// Decode complete pixels through the process boundary.
    pub decode: fn(&Path) -> ImageResult<Decoded>,
    /// Inspect dimensions through the process boundary.
    pub dimensions: fn(&Path) -> ImageResult<(u32, u32)>,
    /// A bounded thumbnail and its natural dimensions.
    pub preview: fn(&Path) -> crate::Result<crate::PreviewSession>,
    /// Refuse static editing of animations and multipage containers.
    pub ensure_static: fn(&Path) -> crate::Result<()>,
    /// Inspect EXIF in the confined process.
    pub metadata: fn(&Path) -> crate::Result<crate::Metadata>,
}
static BACKEND: OnceLock<Backend> = OnceLock::new();

/// Upright pixels in the *source* colour space (no sRGB conversion), the
/// detected format and the raw EXIF/ICC blobs, for in-place re-saves.
pub type ResaveDecoded = (DynamicImage, Option<Format>, crate::metadata::SourceMetadata);
static RESAVE: OnceLock<fn(&Path) -> ImageResult<ResaveDecoded>> = OnceLock::new();

/// Install the process-boundary decoder used by
/// [`crate::open_image_for_resave`]. Without it, a configured [`Backend`]
/// still decodes (sRGB-normalised, via [`Backend::decode`]) but the source
/// metadata cannot be recovered, so it is not re-attached.
pub fn configure_resave(decode: fn(&Path) -> ImageResult<ResaveDecoded>) -> bool {
    RESAVE.set(decode).is_ok()
}
pub(crate) fn configured_resave() -> Option<fn(&Path) -> ImageResult<ResaveDecoded>> {
    RESAVE.get().copied()
}
/// Configure the backend once at executable startup, before processing files.
pub fn configure(backend: Backend) -> bool {
    BACKEND.set(backend).is_ok()
}
pub(crate) fn configured() -> Option<&'static Backend> {
    BACKEND.get()
}
/// Local decoding entry point reserved for the isolated codec process and
/// standalone library consumers. Never calls the configured backend.
pub fn decode_in_process(path: &Path) -> ImageResult<Decoded> {
    crate::pipeline::decode_local(path)
}
/// Resave decoding inside the isolated codec process (see
/// [`configure_resave`]). Never calls the configured backend.
pub fn decode_for_resave_in_process(path: &Path) -> ImageResult<ResaveDecoded> {
    crate::pipeline::decode_local_with(path, false)
}
/// Header inspection inside the isolated codec process.
pub fn dimensions_in_process(path: &Path) -> ImageResult<(u32, u32)> {
    crate::pipeline::bounded_dimensions(path)
}
