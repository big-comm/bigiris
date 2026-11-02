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

/// Raw metadata blobs of a source image, kept so an in-place save can write
/// them back (see [`crate::save_image_preserving_metadata`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SourceMetadata {
    /// TIFF-structured EXIF block (as returned by the `image` decoders).
    pub exif: Option<Vec<u8>>,
    /// Embedded ICC profile describing the source pixels.
    pub icc: Option<Vec<u8>>,
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
    /// The target cannot embed the source colour profile; pixels were
    /// converted to sRGB (wide-gamut colours may be clipped).
    IccUnsupported,
    /// The profile exceeds 4 MiB; pixels were converted to sRGB.
    IccTooLarge,
    /// The profile's colour model does not match the written pixels (e.g. a
    /// GRAY profile on pixels an edit made RGB); pixels were converted to
    /// sRGB.
    IccMismatched,
    /// The profile is malformed or of a kind that cannot be applied (CMYK,
    /// Lab…): it was dropped and the pixels written unchanged.
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

/// Normalise an EXIF blob for re-attachment: drop an `Exif\0\0` prefix and
/// reset IFD0 `Orientation` to 1, because the pixels being saved are already
/// upright. Returns `None` for blobs that are not TIFF-structured.
///
/// With `size` (the saved pixels, after rotate/crop/extent),
/// `PixelXDimension`/`PixelYDimension` in the Exif IFD are set to it and
/// the IFD1 thumbnail is unlinked (IFD0's next-IFD offset set to 0): it
/// shows the image before the edit, and file managers display it instead
/// of the real pixels. With `size = None` only the orientation changes.
pub(crate) fn resave_exif(exif: &[u8], size: Option<(u32, u32)>) -> Option<Vec<u8>> {
    let mut tiff = exif.strip_prefix(b"Exif\0\0").unwrap_or(exif).to_vec();
    let little = match tiff.get(..4)? {
        [b'I', b'I', 42, 0] => true,
        [b'M', b'M', 0, 42] => false,
        _ => return None,
    };
    let word = |b: &[u8]| {
        let b = [b[0], b[1]];
        if little {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        }
    };
    let long = |b: &[u8]| {
        let b = [b[0], b[1], b[2], b[3]];
        if little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        }
    };
    let put_word = |v: u16| if little { v.to_le_bytes() } else { v.to_be_bytes() };
    let put_long = |v: u32| if little { v.to_le_bytes() } else { v.to_be_bytes() };
    let ifd = usize::try_from(long(tiff.get(4..8)?)).ok()?;
    let count = usize::from(word(tiff.get(ifd..ifd + 2)?));
    let mut exif_ifd = None;
    for entry in 0..count {
        let at = ifd + 2 + entry * 12;
        let field: [u8; 12] = tiff.get(at..at + 12)?.try_into().ok()?;
        let (tag, kind, n) = (word(&field[..2]), word(&field[2..4]), long(&field[4..8]));
        // Orientation, SHORT, count 1: value is inline in the first 2 bytes.
        if tag == 0x0112 && kind == 3 && n == 1 {
            tiff[at + 8..at + 10].copy_from_slice(&put_word(1));
        }
        // Exif IFD pointer (LONG or IFD type).
        if tag == 0x8769 && (kind == 4 || kind == 13) && n == 1 {
            exif_ifd = usize::try_from(long(&field[8..12])).ok();
        }
    }
    let Some((width, height)) = size else {
        return Some(tiff);
    };
    let next = ifd + 2 + count * 12;
    tiff.get(next..next + 4)?;
    tiff[next..next + 4].copy_from_slice(&put_long(0));
    let Some(sub) = exif_ifd else {
        return Some(tiff);
    };
    let Some(sub_count) = tiff.get(sub..sub + 2).map(|b| usize::from(word(b))) else {
        return Some(tiff);
    };
    for entry in 0..sub_count {
        let at = sub + 2 + entry * 12;
        let Some(field) = tiff.get(at..at + 12).and_then(|f| <[u8; 12]>::try_from(f).ok()) else {
            break;
        };
        let (tag, kind, n) = (word(&field[..2]), word(&field[2..4]), long(&field[4..8]));
        let value = match tag {
            0xa002 => width,
            0xa003 => height,
            _ => continue,
        };
        if n != 1 || !(kind == 3 || kind == 4) {
            continue;
        }
        // SHORT or LONG, inline. A SHORT too small for the new size
        // becomes a LONG (both fit the 4-byte value field).
        match u16::try_from(value) {
            Ok(short) if kind == 3 => {
                tiff[at + 8..at + 10].copy_from_slice(&put_word(short));
                tiff[at + 10..at + 12].fill(0);
            }
            _ => {
                tiff[at + 2..at + 4].copy_from_slice(&put_word(4));
                tiff[at + 8..at + 12].copy_from_slice(&put_long(value));
            }
        }
    }
    Some(tiff)
}

/// Estimated IJG quality (`1..=100`) of the JPEG at `path`, from its
/// luminance quantization table: the quality to pass in
/// [`crate::EncodeOptions::quality`] so an in-place resave of a JPEG keeps
/// roughly its original size and fidelity instead of falling back to 75.
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
pub fn estimate_jpeg_quality_from_reader(
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
    fn upright_exif_resets_orientation_in_both_byte_orders() {
        let le: &[u8] = &[
            b'E', b'x', b'i', b'f', 0, 0, b'I', b'I', 42, 0, 8, 0, 0, 0, 2,
            0, // header, 2 entries
            0x0f, 0x01, 2, 0, 4, 0, 0, 0, b'A', b'B', b'C', 0, // Make "ABC"
            0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, // Orientation = 6
            0, 0, 0, 0,
        ];
        let fixed = resave_exif(le, None).unwrap();
        assert_eq!(&fixed[..2], b"II", "Exif prefix dropped");
        assert_eq!(&fixed[30..32], &[1, 0]);
        assert_eq!(&fixed[18..22], &[b'A', b'B', b'C', 0], "other tags untouched");
        let be: &[u8] = &[
            b'M', b'M', 0, 42, 0, 0, 0, 8, 0, 1, // header, 1 entry
            0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 8, 0, 0, // Orientation = 8
            0, 0, 0, 0,
        ];
        assert_eq!(&resave_exif(be, None).unwrap()[18..20], &[0, 1]);
        assert_eq!(resave_exif(b"not exif", None), None);
        // Truncated IFD: rejected instead of panicking.
        assert_eq!(resave_exif(&le[..20], None), None);
    }

    /// IFD0: Orientation 6 + Exif IFD pointer, next IFD → IFD1 (thumbnail).
    /// Exif IFD: PixelXDimension SHORT 4000, PixelYDimension LONG 3000.
    fn exif_with_dimensions_and_thumbnail() -> Vec<u8> {
        let mut t = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 2, 0];
        t.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]);
        t.extend_from_slice(&[0x69, 0x87, 4, 0, 1, 0, 0, 0, 38, 0, 0, 0]);
        t.extend_from_slice(&[68, 0, 0, 0]); // next IFD (IFD1) at 68
        assert_eq!(t.len(), 38);
        t.extend_from_slice(&[2, 0]);
        t.extend_from_slice(&[0x02, 0xa0, 3, 0, 1, 0, 0, 0, 0xa0, 0x0f, 0, 0]);
        t.extend_from_slice(&[0x03, 0xa0, 4, 0, 1, 0, 0, 0, 0xb8, 0x0b, 0, 0]);
        t.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(t.len(), 68);
        // IFD1: JPEGInterchangeFormatLength = 0, no next IFD.
        t.extend_from_slice(&[1, 0, 0x02, 0x02, 4, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        t
    }

    #[test]
    fn resave_exif_refreshes_dimensions_and_drops_the_stale_thumbnail() {
        let source = exif_with_dimensions_and_thumbnail();
        let parse = |tiff: Vec<u8>| exif::Reader::new().read_raw(tiff).unwrap();
        let before = parse(source.clone());
        assert!(before.fields().any(|f| f.ifd_num == exif::In::THUMBNAIL));

        let fixed = resave_exif(&source, Some((70_000, 20))).unwrap();
        let after = parse(fixed);
        let value = |tag| after.get_field(tag, exif::In::PRIMARY).unwrap().value.get_uint(0);
        assert_eq!(value(exif::Tag::Orientation), Some(1));
        assert_eq!(value(exif::Tag::PixelXDimension), Some(70_000), "SHORT widened to LONG");
        assert_eq!(value(exif::Tag::PixelYDimension), Some(20));
        assert!(after.fields().all(|f| f.ifd_num != exif::In::THUMBNAIL));

        let small = parse(resave_exif(&source, Some((300, 200))).unwrap());
        let field = small.get_field(exif::Tag::PixelXDimension, exif::In::PRIMARY).unwrap();
        assert!(matches!(field.value, exif::Value::Short(_)), "fitting SHORT stays SHORT");
        assert_eq!(field.value.get_uint(0), Some(300));
        // Without a size, only the orientation changes.
        let kept = parse(resave_exif(&source, None).unwrap());
        assert!(kept.fields().any(|f| f.ifd_num == exif::In::THUMBNAIL));
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

    #[test]
    fn nonexistent_path_returns_io_error() {
        let err = read("/nao/existe/fx.jpg").unwrap_err();
        assert!(matches!(err, BigImageError::Io(_)));
    }
}
