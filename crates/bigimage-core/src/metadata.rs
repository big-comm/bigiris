// SPDX-License-Identifier: GPL-3.0-or-later
//! EXIF and metadata policy. [`read`] turns a file's EXIF into a
//! [`Metadata`] record the GUI shows, with the GPS and camera signals the
//! privacy prompts use.
//!
//! Metadata reaches an output by two paths only: an in-place save writes
//! the source's EXIF and ICC profile back (see
//! [`crate::save_image_preserving_metadata`] and [`preserved_by`]), and a
//! same-format conversion may publish the original bytes unchanged when
//! they are smaller and carry nothing a stripping encode would remove.
//! Every other encode writes no source metadata.

use std::path::Path;

use crate::{BigImageError, Result};

/// Everything we extracted from the image's EXIF segment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Metadata {
    /// `(tag, value)` pairs in file order: the EXIF tag name (`Make`,
    /// `DateTimeOriginal`) for the caller to translate, and the value as
    /// text, unquoted.
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

/// Largest EXIF block (outside JPEG) or ICC profile, in bytes, that
/// [`crate::save_image_preserving_metadata`] writes back.
pub const MAX_RESAVE_BLOB: usize = 4 * 1024 * 1024;

/// Raw metadata blobs of a source image, kept so an in-place save can write
/// them back (see [`crate::save_image_preserving_metadata`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SourceMetadata {
    /// TIFF-structured EXIF block (as returned by the `image` decoders).
    pub exif: Option<Vec<u8>>,
    /// Embedded ICC profile describing the source pixels.
    pub icc: Option<Vec<u8>>,
    /// The source has an EXIF block that could not be read, so a save
    /// reports [`MetadataLoss::ExifInvalid`].
    pub exif_unreadable: bool,
}

/// Which metadata a target format can carry through
/// [`crate::save_image_preserving_metadata`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetadataSupport {
    /// EXIF (capture date, camera, GPS…) is written back.
    pub exif: bool,
    /// The source ICC profile is embedded as-is (otherwise pixels are
    /// converted to sRGB, clipping wide-gamut colours).
    pub icc: bool,
}

impl MetadataSupport {
    /// Everything the source carried survives the save.
    pub fn is_complete(self) -> bool {
        self.exif && self.icc
    }
}

/// Source metadata a [`crate::save_image_preserving_metadata_report`] call
/// could not carry into the written file, so the UI can tell the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum MetadataLoss {
    /// The target format cannot store EXIF (see [`preserved_by`]).
    ExifUnsupported,
    /// The EXIF block exceeds what the target can hold (64 KiB for JPEG,
    /// 4 MiB elsewhere).
    ExifTooLarge,
    /// The EXIF block is not TIFF-structured and could not be rewritten.
    ExifInvalid,
    /// Some EXIF fields could not be read or written (an unknown value
    /// type); the others were kept.
    ExifFieldsDropped,
    /// The target cannot embed the source colour profile; pixels were
    /// converted to sRGB (wide-gamut colours may be clipped).
    IccUnsupported,
    /// The profile's colour model does not match the written pixels (e.g. a
    /// GRAY profile on pixels an edit made RGB); pixels were converted to
    /// sRGB.
    IccMismatched,
    /// The profile is malformed, 4 MiB or larger, or of a kind that cannot
    /// be applied (CMYK, Lab…): it was dropped and the pixels written
    /// unchanged.
    IccDiscarded,
}

/// Metadata preserved for `format` by the vendored encoders: JPEG, PNG and
/// WebP keep both; AVIF keeps EXIF only; TIFF keeps the ICC profile only;
/// every other format keeps neither.
pub fn preserved_by(format: crate::Format) -> MetadataSupport {
    use crate::Format;
    MetadataSupport {
        exif: matches!(format, Format::Jpeg | Format::Png | Format::WebP | Format::Avif),
        icc: matches!(format, Format::Jpeg | Format::Png | Format::WebP | Format::Tiff),
    }
}

/// Estimated IJG quality (`1..=100`) of the JPEG at `path`, from its
/// luminance quantization table: the quality to pass in
/// [`crate::EncodeOptions::quality`] so an in-place resave of a JPEG keeps
/// roughly its original size and fidelity instead of being re-compressed
/// to the perceptual target.
///
/// The table is compared with the standard IJG luminance table scaled for
/// every quality (`q < 50`: `5000/q` %, otherwise `200 - 2q` %) and the
/// closest match wins. Encoders using other tables (some cameras,
/// Photoshop) get the nearest IJG-equivalent quality. Only the marker
/// segments before the first scan are read (no codec runs, sizes are
/// bounded); returns `None` for anything that is not a JPEG with a
/// luminance table.
pub fn estimate_jpeg_quality(path: impl AsRef<Path>) -> Option<u8> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    estimate_jpeg_quality_from_reader(&mut std::io::BufReader::new(file))
}

/// [`estimate_jpeg_quality`] over any seekable reader positioned at the
/// start of a JPEG stream.
fn estimate_jpeg_quality_from_reader(
    reader: &mut (impl std::io::Read + std::io::Seek),
) -> Option<u8> {
    use std::io::SeekFrom;
    fn byte(reader: &mut impl std::io::Read) -> Option<u8> {
        let mut b = [0];
        reader.read_exact(&mut b).ok()?;
        Some(b[0])
    }
    if (byte(reader)?, byte(reader)?) != (0xff, 0xd8) {
        return None;
    }
    for _ in 0..4096 {
        if byte(reader)? != 0xff {
            return None;
        }
        let mut marker = byte(reader)?;
        while marker == 0xff {
            marker = byte(reader)?;
        }
        match marker {
            0x01 | 0xd0..=0xd7 => continue,
            0xd9 | 0xda => return None,
            _ => {}
        }
        let len = u16::from_be_bytes([byte(reader)?, byte(reader)?]).checked_sub(2)?;
        if marker != 0xdb {
            reader.seek(SeekFrom::Current(i64::from(len))).ok()?;
            continue;
        }
        let mut payload = vec![0; usize::from(len)];
        reader.read_exact(&mut payload).ok()?;
        let mut rest = payload.as_slice();
        while let Some((&info, tail)) = rest.split_first() {
            let wide = info >> 4 == 1;
            let size = if wide { 128 } else { 64 };
            let table = tail.get(..size)?;
            if info & 0x0f == 0 {
                let values: Vec<u16> = if wide {
                    table.as_chunks::<2>().0.iter().map(|b| u16::from_be_bytes(*b)).collect()
                } else {
                    table.iter().map(|&v| u16::from(v)).collect()
                };
                return Some(closest_ijg_quality(&values));
            }
            rest = &tail[size..];
        }
    }
    None
}

/// IJG quality whose scaled standard luminance table is closest to `table`
/// (64 values in zig-zag order, as stored in DQT).
fn closest_ijg_quality(table: &[u16]) -> u8 {
    // ITU T.81 Annex K.1 luminance table, natural (row-major) order.
    const STD_LUMA: [u16; 64] = [
        16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69,
        56, 14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81,
        104, 113, 92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
    ];
    const ZIGZAG: [usize; 64] = [
        0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27,
        20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51,
        58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
    ];
    (1..=100u8)
        .min_by_key(|&quality| {
            let q = u32::from(quality);
            let scale = if q < 50 { 5000 / q } else { 200 - 2 * q };
            ZIGZAG
                .iter()
                .zip(table)
                .map(|(&natural, &actual)| {
                    let expected =
                        ((u32::from(STD_LUMA[natural]) * scale + 50) / 100).clamp(1, 255);
                    expected.abs_diff(u32::from(actual))
                })
                .sum::<u32>()
        })
        .unwrap_or(75)
}

/// Read EXIF from `path`. Returns an empty [`Metadata`] when the file
/// has no EXIF segment (very common for PNG/WebP/GIF outputs from
/// non-camera sources) — the caller decides whether that's notable.
///
/// # Errors
///
/// - [`BigImageError::Io`]: `path` cannot be opened.
/// - [`BigImageError::InvalidInput`]: it is not a regular file or is over 1 GiB.
/// - [`BigImageError::Other`]: its EXIF cannot be read.
///
/// With a configured backend every failure arrives as [`BigImageError::Decode`]
/// carrying the worker's text.
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
        return Err(BigImageError::InvalidInput("invalid metadata input".into()));
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
        let shown = field.display_value().with_unit(&exif).to_string();
        let value = match &field.value {
            // The library quotes and escapes plain text; show it as written.
            exif::Value::Ascii(texts) if shown.starts_with('"') => texts
                .iter()
                .map(|text| String::from_utf8_lossy(text).into_owned())
                .collect::<Vec<_>>()
                .join(", "),
            _ => shown,
        };
        out.tags.push((field.tag.to_string(), value));

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

/// Whether the file's EXIF holds a GPS coordinate, for the privacy banner
/// of the convert and resize dialogs; `false` when it cannot be read.
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
    fn jpeg_quality_is_estimated_from_the_luminance_table() {
        let noise = image::RgbImage::from_fn(32, 32, |x, y| {
            image::Rgb([(x * 7 + y) as u8, (x ^ y) as u8 * 5, (y * 9) as u8])
        });
        for quality in [10u8, 30, 50, 75, 85, 92, 100] {
            let mut bytes = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
                .encode_image(&noise)
                .unwrap();
            let estimate =
                estimate_jpeg_quality_from_reader(&mut std::io::Cursor::new(&bytes)).unwrap();
            assert!(estimate.abs_diff(quality) <= 1, "q{quality} estimated as {estimate}");
        }
        let dir = TempDir::new().unwrap();
        let png = write_plain_png(dir.path());
        assert_eq!(estimate_jpeg_quality(&png), None);
        assert_eq!(estimate_jpeg_quality_from_reader(&mut std::io::Cursor::new(b"\xff\xd8")), None);
        assert_eq!(estimate_jpeg_quality(dir.path()), None);
    }

    /// Tags come by name; text shows as written, a date keeps the
    /// library's rendering.
    #[test]
    fn tags_are_named_and_unquoted() {
        use exif::{Field, In, Tag, Value};
        let ascii = |text: &str| Value::Ascii(vec![text.as_bytes().to_vec()]);
        let fields = [
            Field { tag: Tag::Make, ifd_num: In::PRIMARY, value: ascii("Câmera \"X\"") },
            Field { tag: Tag::DateTime, ifd_num: In::PRIMARY, value: ascii("2024:05:06 07:08:09") },
        ];
        let mut writer = exif::experimental::Writer::new();
        for field in &fields {
            writer.push_field(field);
        }
        let mut blob = std::io::Cursor::new(Vec::new());
        writer.write(&mut blob, true).unwrap();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("camera.jpg");
        let mut encoder =
            image::codecs::jpeg::JpegEncoder::new(std::fs::File::create(&path).unwrap());
        image::ImageEncoder::set_exif_metadata(&mut encoder, blob.into_inner()).unwrap();
        image::ImageEncoder::write_image(encoder, &[0; 12], 2, 2, image::ExtendedColorType::Rgb8)
            .unwrap();
        let tags = read(&path).unwrap().tags;
        assert!(tags.contains(&("Make".into(), "Câmera \"X\"".into())), "{tags:?}");
        assert!(tags.contains(&("DateTime".into(), "2024-05-06 07:08:09".into())), "{tags:?}");
    }

    #[test]
    fn nonexistent_path_returns_io_error() {
        let err = read("/does/not/exist/fx.jpg").unwrap_err();
        assert!(matches!(err, BigImageError::Io(_)));
    }

    #[test]
    fn read_refuses_directories_and_oversized_files() {
        let dir = TempDir::new().unwrap();
        let sparse = dir.path().join("huge.jpg");
        std::fs::File::create(&sparse).unwrap().set_len(1024 * 1024 * 1024 + 1).unwrap();
        for path in [dir.path(), sparse.as_path()] {
            assert!(matches!(read(path), Err(BigImageError::InvalidInput(_))), "{path:?}");
        }
    }
}
