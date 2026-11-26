// SPDX-License-Identifier: GPL-3.0-or-later
//! Library errors as the user reads them. The libraries' own texts stay
//! English; the CLI and the GUI show their errors through these functions,
//! one translated sentence per variant with the library's detail inside.

use std::path::Path;

use bigimage_ai::{AiError, WeightsError};
use bigimage_core::BigImageError;
use gettextrs::gettext;
use image::ImageError;

use crate::codec::{printable_path, printable_text};
use crate::{fill, with_error};

/// A core error in the user's language, one line safe to print.
pub(crate) fn core_error_text(error: &BigImageError) -> String {
    let text = match error {
        BigImageError::Io(error) => {
            with_error(&gettext("could not read or write the file: {error}"), io_error_text(error))
        }
        BigImageError::InvalidInput(detail) => {
            with_error(&gettext("invalid input: {error}"), detail)
        }
        BigImageError::UnsupportedFormat(format) => {
            with_error(&gettext("unsupported format: {error}"), format)
        }
        BigImageError::Decode { path, source } => fill(
            &gettext("could not read the image {path}: {error}"),
            &[("path", &printable_path(path)), ("error", &image_error_text(source))],
        ),
        BigImageError::Encode { path, source } => fill(
            &gettext("could not write {path}: {error}"),
            &[("path", &printable_path(path)), ("error", &image_error_text(source))],
        ),
        BigImageError::Cancelled => gettext("operation canceled"),
        BigImageError::NoFreeName { output } => fill(
            &gettext("too many files with the same name: {path}"),
            &[("path", &printable_path(output))],
        ),
        // Text this app translated before handing it to core, or an
        // internal fault with no sentence of its own.
        BigImageError::Other(text) => text.clone(),
    };
    printable_text(&text)
}

/// An `image` error in the user's language, safe to print. Its I/O variant
/// carries the system's message or one the isolated codec translated where
/// it raised it.
pub(crate) fn image_error_text(error: &ImageError) -> String {
    let text = match error {
        ImageError::IoError(error) => io_error_text(error),
        ImageError::Decoding(detail) => {
            with_error(&gettext("damaged or unreadable image data: {error}"), detail)
        }
        ImageError::Encoding(detail) => with_error(&gettext("the encoder failed: {error}"), detail),
        ImageError::Parameter(detail) => with_error(&gettext("invalid parameter: {error}"), detail),
        ImageError::Limits(_) => gettext("the image is larger than BigIris can process"),
        ImageError::Unsupported(detail) => {
            with_error(&gettext("unsupported image: {error}"), detail)
        }
    };
    printable_text(&text)
}

/// An I/O error as the system words it in the user's language. tempfile
/// wraps OS errors in an English "at path" text that drops the error code,
/// so the common kinds are mapped back to theirs.
pub(crate) fn io_error_text(error: &std::io::Error) -> String {
    use std::io::ErrorKind;
    let code = error.raw_os_error().or(match error.kind() {
        ErrorKind::PermissionDenied => Some(libc::EACCES),
        ErrorKind::NotFound => Some(libc::ENOENT),
        ErrorKind::AlreadyExists => Some(libc::EEXIST),
        ErrorKind::ReadOnlyFilesystem => Some(libc::EROFS),
        ErrorKind::StorageFull => Some(libc::ENOSPC),
        ErrorKind::QuotaExceeded => Some(libc::EDQUOT),
        _ => None,
    });
    match code {
        Some(code) => std::io::Error::from_raw_os_error(code).to_string(),
        None => error.to_string(),
    }
}

/// A model error in the user's language, safe to print.
pub(crate) fn ai_error_text(error: &AiError) -> String {
    let text = match error {
        AiError::Disabled => no_ai_build_message(),
        AiError::Missing { model, package, .. } => missing_model_message(model, package),
        AiError::Unreadable { model, path, error } => {
            weights_unreadable_message(model, path, error)
        }
        AiError::Cancelled => gettext("Canceled"),
        AiError::Memory { model, need, available } => fill(
            &gettext(
                "{model} needs about {need} GiB of free RAM for this step; available now: {available} GiB. Close other programs or use a smaller image. Swap does not count as free RAM.",
            ),
            &[
                ("model", model),
                ("need", &format!("{:.2}", bigimage_ai::gibibytes(*need))),
                ("available", &format!("{:.2}", bigimage_ai::gibibytes(*available))),
            ],
        ),
        AiError::MemoryUnknown(detail) => {
            with_error(&gettext("could not check the memory free for AI: {error}"), detail)
        }
        AiError::Gpu(detail) | AiError::Inference(detail) | AiError::Processing(detail) => {
            with_error(&gettext("AI failed: {error}"), detail)
        }
    };
    printable_text(&text)
}

/// This build has no AI engines.
pub(crate) fn no_ai_build_message() -> String {
    gettext(
        "This build does not include AI. Rebuild with `cargo build --features ai` (requires glslc, from the shaderc package).",
    )
}

/// A model's weights package is not installed.
pub(crate) fn missing_model_message(model: &str, package: &str) -> String {
    fill(
        &gettext("{model} is not installed. Install it with: sudo pacman -S {package}"),
        &[("model", model), ("package", package)],
    )
}

/// A model's weights file exists but cannot be loaded.
pub(crate) fn weights_unreadable_message(model: &str, path: &Path, error: &WeightsError) -> String {
    let reason = match error {
        WeightsError::NotAFile => gettext("not a weights file"),
        WeightsError::Io(message) => message.clone(),
    };
    fill(
        &gettext("could not read the {model} weights at {path}: {error}"),
        &[("model", model), ("path", &printable_path(path)), ("error", &reason)],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_texts_neutralize_paths() {
        let path = std::path::PathBuf::from("a\x1b]0;x\x07\nb.png");
        let source = ImageError::IoError(std::io::Error::other("worker said no"));
        let text = core_error_text(&BigImageError::Decode { path: path.clone(), source });
        assert!(text.contains("worker said no"), "{text}");
        assert!(!text.contains(['\x1b', '\x07', '\n']), "{text:?}");
        let text = core_error_text(&BigImageError::NoFreeName { output: path });
        assert!(text.starts_with("too many files with the same name: a"), "{text:?}");
        let detail = BigImageError::InvalidInput("animated or multi-page image".into());
        assert!(core_error_text(&detail).ends_with("animated or multi-page image"));
        // Library detail may carry a raw name: it cannot start a report line.
        let forged = BigImageError::UnsupportedFormat("x\nok  a.png -> b.png".into());
        assert!(!core_error_text(&forged).contains('\n'));
        assert!(!ai_error_text(&AiError::Processing("x\nok".into())).contains('\n'));
        let image = ImageError::IoError(std::io::Error::other("x\nok"));
        assert!(!image_error_text(&image).contains('\n'));
    }

    #[test]
    fn memory_refusal_states_both_amounts() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let error =
            AiError::Memory { model: "BiRefNet-lite", need: GIB + GIB / 5, available: GIB / 2 };
        let text = ai_error_text(&error);
        assert!(text.contains("BiRefNet-lite needs about 1.20 GiB"), "{text}");
        assert!(text.contains("available now: 0.50 GiB"), "{text}");
    }
}
