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
    /// JPEG (quality 1..=100).
    Jpeg,
    /// WebP through libwebp: lossy, lossless or near-lossless, whichever
    /// is smallest at the target (lossless at a fixed quality of 100).
    WebP,
    /// AVIF (AV1 Image File Format). Encode via ravif, decode via libdav1d.
    Avif,
    /// TIFF (one page is written).
    Tiff,
    /// BMP.
    Bmp,
    /// GIF (static here; [`crate::animate::make_gif`] writes animations).
    Gif,
    /// ICO (Windows icon, one entry of at most 256 px).
    Ico,
    /// Netpbm: PGM or PPM written, PBM and PAM also read.
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

    /// Extension of an output in this format made from `input`: the input's
    /// own spelling when it already names this format (`photo.JPEG`,
    /// `scan.tif`), so a same-format output keeps the input's name, and
    /// [`Format::extension`] otherwise. PNM always takes the standard one,
    /// since its extension follows the pixels ([`Format::extension_for`]).
    pub fn output_extension(self, input: &std::path::Path) -> &std::ffi::OsStr {
        let names_self = |ext: &&std::ffi::OsStr| {
            self != Format::Pnm
                && ext.to_str().and_then(|ext| Format::parse(ext).ok()) == Some(self)
        };
        input.extension().filter(names_self).unwrap_or(std::ffi::OsStr::new(self.extension()))
    }

    /// Extension for a file of this format holding pixels of colour type
    /// `color`, as the pipelines write it: PNM is `pgm` for gray (PGM, P5)
    /// and `ppm` for colour (PPM, P6) — alpha is flattened, so PAM is never
    /// written. Every other format returns [`Format::extension`].
    ///
    /// The transform pipelines name PNM outputs with this, so the extension
    /// is only known after decoding; `OverwritePolicy::Skip` for a PNM
    /// target is therefore decided after the decode, never before it.
    pub fn extension_for(&self, color: image::ColorType) -> &'static str {
        match self {
            Format::Pnm if crate::convert::writes_gray(color, Format::Pnm) => "pgm",
            other => other.extension(),
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
    fn output_extension_keeps_own_spelling() {
        let ext = |name: &str, format: Format| {
            format.output_extension(std::path::Path::new(name)).to_str().unwrap().to_owned()
        };
        assert_eq!(ext("a.PNG", Format::Png), "PNG");
        assert_eq!(ext("a.jpeg", Format::Jpeg), "jpeg");
        assert_eq!(ext("a.TIF", Format::Tiff), "TIF");
        assert_eq!(ext("a.jpeg", Format::Png), "png");
        assert_eq!(ext("a", Format::Png), "png");
        assert_eq!(ext("a.tar.gz", Format::Png), "png");
        assert_eq!(ext("a.PGM", Format::Pnm), "ppm", "PNM follows the pixels");
    }

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
    fn pnm_extension_follows_the_written_pixels() {
        use image::ColorType;
        for (color, ext) in [
            (ColorType::L8, "pgm"),
            (ColorType::La16, "pgm"),
            (ColorType::Rgb8, "ppm"),
            (ColorType::Rgba16, "ppm"),
            (ColorType::Rgb32F, "ppm"),
        ] {
            assert_eq!(Format::Pnm.extension_for(color), ext, "{color:?}");
        }
        assert_eq!(Format::Png.extension_for(ColorType::L8), "png");
    }
}
