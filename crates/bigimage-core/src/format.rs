// SPDX-License-Identifier: GPL-3.0-or-later
//! Target encoding formats and their metadata.
//!
//! The enum is the single source of truth for formats the current build can
//! actually decode and encode. Future formats are added only with a working
//! implementation, so feature discovery and desktop metadata stay honest.

use image::ImageFormat;

use crate::BigImageError;

/// Supported target encoding formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum Format {
    /// PNG (lossless, 8/16-bit).
    Png,
    /// JPEG (quality 0..=100).
    Jpeg,
    /// WebP (lossless via image's built-in encoder; lossy needs libwebp).
    WebP,
    /// AVIF (AV1 Image File Format). Encode via ravif, decode via libdav1d.
    Avif,
    /// TIFF (multi-page support landing later).
    Tiff,
    /// BMP.
    Bmp,
    /// GIF (static; animated support later).
    Gif,
    /// ICO (Windows icon, multi-size).
    Ico,
    /// Netpbm: PPM/PGM/PBM.
    Pnm,
    /// Truevision TGA (game-dev textures).
    Tga,
    /// Quite OK Image — fast simple lossless.
    Qoi,
    /// Radiance HDR (.hdr, 32bpc float).
    Hdr,
    /// OpenEXR (cinema / VFX, 16/32bpc float).
    OpenExr,
}

impl Format {
    /// Canonical file extension without the dot (e.g. `"png"`, `"jpg"`).
    pub fn extension(&self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpeg => "jpg",
            Format::WebP => "webp",
            Format::Avif => "avif",
            Format::Tiff => "tiff",
            Format::Bmp => "bmp",
            Format::Gif => "gif",
            Format::Ico => "ico",
            Format::Pnm => "ppm",
            Format::Tga => "tga",
            Format::Qoi => "qoi",
            Format::Hdr => "hdr",
            Format::OpenExr => "exr",
        }
    }

    /// MIME type.
    pub fn mime(&self) -> &'static str {
        match self {
            Format::Png => "image/png",
            Format::Jpeg => "image/jpeg",
            Format::WebP => "image/webp",
            Format::Avif => "image/avif",
            Format::Tiff => "image/tiff",
            Format::Bmp => "image/bmp",
            Format::Gif => "image/gif",
            Format::Ico => "image/vnd.microsoft.icon",
            Format::Pnm => "image/x-portable-anymap",
            Format::Tga => "image/x-tga",
            Format::Qoi => "image/qoi",
            Format::Hdr => "image/vnd.radiance",
            Format::OpenExr => "image/x-exr",
        }
    }

    /// Parse a format from a user-provided string (case-insensitive). Accepts
    /// extension-like and name-like spellings.
    pub fn parse(s: &str) -> Result<Self, BigImageError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "png" => Ok(Format::Png),
            "jpg" | "jpeg" | "jpe" => Ok(Format::Jpeg),
            "webp" => Ok(Format::WebP),
            "avif" => Ok(Format::Avif),
            "tif" | "tiff" => Ok(Format::Tiff),
            "bmp" | "dib" => Ok(Format::Bmp),
            "gif" => Ok(Format::Gif),
            "ico" => Ok(Format::Ico),
            "pnm" | "ppm" | "pgm" | "pbm" | "pam" => Ok(Format::Pnm),
            "tga" | "targa" => Ok(Format::Tga),
            "qoi" => Ok(Format::Qoi),
            "hdr" | "rgbe" => Ok(Format::Hdr),
            "exr" | "openexr" => Ok(Format::OpenExr),
            other => Err(BigImageError::UnsupportedFormat(other.to_string())),
        }
    }

    /// Map an [`ImageFormat`] back to one of our variants. Returns `None` if
    /// the `image` crate surfaced a format we haven't promoted here yet.
    pub fn from_image_format(fmt: ImageFormat) -> Option<Self> {
        Some(match fmt {
            ImageFormat::Png => Format::Png,
            ImageFormat::Jpeg => Format::Jpeg,
            ImageFormat::WebP => Format::WebP,
            ImageFormat::Avif => Format::Avif,
            ImageFormat::Tiff => Format::Tiff,
            ImageFormat::Bmp => Format::Bmp,
            ImageFormat::Gif => Format::Gif,
            ImageFormat::Ico => Format::Ico,
            ImageFormat::Pnm => Format::Pnm,
            ImageFormat::Tga => Format::Tga,
            ImageFormat::Qoi => Format::Qoi,
            ImageFormat::Hdr => Format::Hdr,
            ImageFormat::OpenExr => Format::OpenExr,
            _ => return None,
        })
    }

    pub(crate) fn to_image_format(self) -> ImageFormat {
        match self {
            Format::Png => ImageFormat::Png,
            Format::Jpeg => ImageFormat::Jpeg,
            Format::WebP => ImageFormat::WebP,
            Format::Avif => ImageFormat::Avif,
            Format::Tiff => ImageFormat::Tiff,
            Format::Bmp => ImageFormat::Bmp,
            Format::Gif => ImageFormat::Gif,
            Format::Ico => ImageFormat::Ico,
            Format::Pnm => ImageFormat::Pnm,
            Format::Tga => ImageFormat::Tga,
            Format::Qoi => ImageFormat::Qoi,
            Format::Hdr => ImageFormat::Hdr,
            Format::OpenExr => ImageFormat::OpenExr,
        }
    }

    /// Every format currently supported by this build. Stable iteration order
    /// so help/completions stay deterministic.
    pub fn all() -> &'static [Format] {
        &[
            Format::Png,
            Format::Jpeg,
            Format::WebP,
            Format::Avif,
            Format::Tiff,
            Format::Bmp,
            Format::Gif,
            Format::Ico,
            Format::Pnm,
            Format::Tga,
            Format::Qoi,
            Format::Hdr,
            Format::OpenExr,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_common_spellings() {
        assert_eq!(Format::parse("png").unwrap(), Format::Png);
        assert_eq!(Format::parse("JPG").unwrap(), Format::Jpeg);
        assert_eq!(Format::parse("jpeg").unwrap(), Format::Jpeg);
        assert_eq!(Format::parse(" Tiff ").unwrap(), Format::Tiff);
        assert_eq!(Format::parse("AVIF").unwrap(), Format::Avif);
        assert_eq!(Format::parse("pgm").unwrap(), Format::Pnm);
        assert_eq!(Format::parse("openexr").unwrap(), Format::OpenExr);
    }

    #[test]
    fn parse_rejects_unknown() {
        let err = Format::parse("xyz").unwrap_err();
        assert!(matches!(err, BigImageError::UnsupportedFormat(_)));
    }

    #[test]
    fn extensions_are_lowercase_no_dot() {
        for f in Format::all() {
            let ext = f.extension();
            assert!(!ext.starts_with('.'), "ext has leading dot: {ext:?}");
            assert_eq!(ext, ext.to_ascii_lowercase());
        }
    }

    #[test]
    fn every_variant_parses_back_from_its_extension() {
        for f in Format::all() {
            let parsed = Format::parse(f.extension()).unwrap_or_else(|e| {
                panic!("ext {:?} for {f:?} does not round-trip: {e}", f.extension())
            });
            assert_eq!(parsed, *f);
        }
    }

    #[test]
    fn mime_never_empty() {
        for f in Format::all() {
            assert!(!f.mime().is_empty());
            assert!(f.mime().contains('/'));
        }
    }
}
