// SPDX-License-Identifier: GPL-3.0-or-later
//! EXIF / metadata inspection. Reads tags from image files via
//! `kamadak-exif`, exposes a friendly [`Metadata`] record the GUI can
//! render in sidebars, and surfaces the two signals we care about most
//! for smart-prompts: "has GPS coordinates" and "has camera info".
//!
//! Does **not** write metadata yet — today our encode pipeline strips all
//! ancillary chunks anyway (it decodes to `DynamicImage` which has no
//! metadata carrier). A future iteration will add `preserve_exif` that
//! reads bytes pre-decode and re-injects them post-encode.

use std::path::Path;

use crate::{BigImageError, Result};

/// Everything we extracted from the image's EXIF segment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Metadata {
    /// Human-readable `(name, value)` pairs, ready to show in a sidebar.
    pub tags: Vec<(String, String)>,
    /// Whether the file encodes a GPS coordinate (either `GPSLatitude` or
    /// `GPSLongitude` is present in the primary IFD).
    pub has_gps: bool,
    /// Whether the file identifies the camera (Make/Model/Software).
    pub has_camera_info: bool,
    /// `Orientation` tag, if present — used by auto-rotate.
    pub orientation: Option<u16>,
}

impl Metadata {
    /// Convenience: does the image carry any EXIF data at all?
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
    }
}

/// Read EXIF from `path`. Returns an empty [`Metadata`] when the file
/// has no EXIF segment (very common for PNG/WebP/GIF outputs from
/// non-camera sources) — the caller decides whether that's notable.
pub fn read(path: impl AsRef<Path>) -> Result<Metadata> {
    let path = path.as_ref();
    if let Some(backend) = crate::backend::configured() {
        return (backend.metadata)(path);
    }
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(path)
        .map_err(BigImageError::Io)?;
    let info = file.metadata().map_err(BigImageError::Io)?;
    if !info.is_file() || info.len() > 1024 * 1024 * 1024 {
        return Err(BigImageError::InvalidInput("entrada de metadados inválida".into()));
    }
    let mut reader = std::io::BufReader::new(file);

    let exif_reader = exif::Reader::new();
    let exif = match exif_reader.read_from_container(&mut reader) {
        Ok(e) => e,
        // No EXIF / unsupported container → not an error, just empty metadata.
        Err(exif::Error::NotFound(_)) | Err(exif::Error::InvalidFormat(_)) => {
            return Ok(Metadata::default());
        }
        Err(e) => {
            return Err(BigImageError::Other(format!("exif: {e}")));
        }
    };

    let mut out = Metadata::default();
    for field in exif.fields() {
        let name = field.tag.description().unwrap_or("").to_string();
        let name = if name.is_empty() { format!("{}", field.tag) } else { name };
        let value = field.display_value().with_unit(&exif).to_string();
        out.tags.push((name, value));

        // Surface signals we feed back to smart-prompts.
        if field.ifd_num == exif::In::PRIMARY {
            match field.tag {
                exif::Tag::GPSLatitude | exif::Tag::GPSLongitude => {
                    out.has_gps = true;
                }
                exif::Tag::Make | exif::Tag::Model | exif::Tag::Software => {
                    out.has_camera_info = true;
                }
                exif::Tag::Orientation => {
                    if let Some(v) = field.value.get_uint(0) {
                        out.orientation = Some(v as u16);
                    }
                }
                _ => {}
            }
        }
    }

    Ok(out)
}

/// Quick-path: just check if the file has GPS data. Used by the smart
/// prompts in the convert/resize dialogs to display a privacy banner
/// without scanning the full EXIF table into strings.
pub fn has_gps(path: impl AsRef<Path>) -> bool {
    read(path).map(|m| m.has_gps).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use tempfile::TempDir;

    fn write_plain_png(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("plain.png");
        let img = RgbImage::from_pixel(4, 4, Rgb([10, 20, 30]));
        img.save(&path).unwrap();
        path
    }

    #[test]
    fn png_without_exif_returns_empty_metadata() {
        let dir = TempDir::new().unwrap();
        let path = write_plain_png(dir.path());
        let meta = read(&path).unwrap();
        assert!(meta.is_empty());
        assert!(!meta.has_gps);
        assert!(!meta.has_camera_info);
        assert_eq!(meta.orientation, None);
    }

    #[test]
    fn has_gps_shortcut_matches_full_read() {
        let dir = TempDir::new().unwrap();
        let path = write_plain_png(dir.path());
        assert_eq!(has_gps(&path), read(&path).unwrap().has_gps);
    }

    #[test]
    fn nonexistent_path_returns_io_error() {
        let err = read("/nao/existe/fx.jpg").unwrap_err();
        assert!(matches!(err, BigImageError::Io(_)));
    }
}
