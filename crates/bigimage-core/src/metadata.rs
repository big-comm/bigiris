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

/// An EXIF blob for upright pixels: no `Exif\0\0` prefix, IFD0 `Orientation`
/// 1 and, with the saved `size`, the pixel dimensions set and the pre-edit
/// IFD1 thumbnail erased (what other IFDs reference stays). `None` when the
/// blob is not TIFF or the thumbnail's offsets cannot be read.
pub(crate) fn resave_exif(exif: &[u8], size: Option<(u32, u32)>) -> Option<Vec<u8>> {
    let mut tiff = Tiff::new(exif.strip_prefix(b"Exif\0\0").unwrap_or(exif))?;
    let ifd0 = usize::try_from(tiff.long(4)?).ok()?;
    let count = usize::from(tiff.word(ifd0)?);
    let mut exif_ifd = None;
    for at in (0..count).map(|entry| ifd0 + 2 + entry * 12) {
        let (tag, kind, n, value) =
            (tiff.word(at)?, tiff.word(at + 2)?, tiff.long(at + 4)?, tiff.long(at + 8)?);
        // Orientation, SHORT, count 1: value is inline in the first 2 bytes.
        if tag == 0x0112 && kind == 3 && n == 1 {
            tiff.put_word(at + 8, 1);
        }
        // Exif IFD pointer (LONG or IFD type).
        if tag == 0x8769 && (kind == 4 || kind == 13) && n == 1 {
            exif_ifd = usize::try_from(value).ok();
        }
    }
    let Some((width, height)) = size else {
        return Some(tiff.bytes);
    };
    let link = ifd0 + 2 + count * 12;
    let ifd1 = usize::try_from(tiff.long(link)?).ok()?;
    let referenced = tiff.referenced_ranges(ifd0);
    tiff.put_long(link, 0);
    if ifd1 != 0 {
        tiff.erase_thumbnail(ifd1, &referenced)?;
    }
    if let Some(sub) = exif_ifd {
        tiff.set_pixel_dimensions(sub, width, height);
    }
    Some(tiff.bytes)
}

/// A TIFF-structured EXIF block rewritten in place.
struct Tiff {
    bytes: Vec<u8>,
    little: bool,
}

impl Tiff {
    /// `None` unless `bytes` starts with a TIFF header.
    fn new(bytes: &[u8]) -> Option<Self> {
        let little = match bytes.get(..4)? {
            [b'I', b'I', 42, 0] => true,
            [b'M', b'M', 0, 42] => false,
            _ => return None,
        };
        Some(Self { bytes: bytes.to_vec(), little })
    }

    fn word(&self, at: usize) -> Option<u16> {
        let b = self.bytes.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if self.little { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
    }

    fn long(&self, at: usize) -> Option<u32> {
        let b = self.bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if self.little { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }

    /// Write at an offset a read just validated.
    fn put_word(&mut self, at: usize, value: u16) {
        let b = if self.little { value.to_le_bytes() } else { value.to_be_bytes() };
        self.bytes[at..at + 2].copy_from_slice(&b);
    }

    fn put_long(&mut self, at: usize, value: u32) {
        let b = if self.little { value.to_le_bytes() } else { value.to_be_bytes() };
        self.bytes[at..at + 4].copy_from_slice(&b);
    }

    /// Bytes of the IFD table at `ifd`: count, entries and next-IFD link.
    fn table(&self, ifd: usize) -> Option<std::ops::Range<usize>> {
        let count = usize::from(self.word(ifd)?);
        Some(ifd..ifd + 2 + count * 12 + 4)
    }

    /// Where the value of the entry at `at` lies when it does not fit the
    /// entry's 4-byte field.
    fn out_of_line(&self, at: usize) -> Option<std::ops::Range<usize>> {
        let unit = match self.word(at + 2)? {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 | 13 => 4,
            5 | 10 | 12 => 8,
            _ => return None,
        };
        let len = usize::try_from(self.long(at + 4)?).ok()?.checked_mul(unit)?;
        let start = usize::try_from(self.long(at + 8)?).ok()?;
        (len > 4).then(|| start..start.saturating_add(len))
    }

    /// SHORT or LONG values of the entry at `at`, inline when they fit.
    fn values(&self, at: usize) -> Option<Vec<u32>> {
        let unit = match self.word(at + 2)? {
            3 => 2,
            4 => 4,
            _ => return None,
        };
        let count = usize::try_from(self.long(at + 4)?).ok()?;
        let start = self.out_of_line(at).map_or(at + 8, |range| range.start);
        let read = |i: usize| {
            let at = start.checked_add(i.checked_mul(unit)?)?;
            if unit == 2 { self.word(at).map(u32::from) } else { self.long(at) }
        };
        (0..count).map(read).collect()
    }

    /// Header, IFD0 and the Exif, GPS and interoperability IFDs reachable
    /// from it, each with the values stored outside its entries: what
    /// erasing the thumbnail must leave intact.
    fn referenced_ranges(&self, ifd0: usize) -> Vec<std::ops::Range<usize>> {
        let mut ranges = Vec::new();
        // The TIFF header.
        ranges.push(0..8);
        let mut pending = vec![ifd0];
        while let Some(ifd) = pending.pop() {
            let Some(table) = self.table(ifd) else {
                continue;
            };
            for at in (table.start + 2..table.end - 4).step_by(12) {
                if let Some(value) = self.out_of_line(at) {
                    ranges.push(value);
                }
                let sub_ifd = matches!(self.word(at), Some(0x8769 | 0x8825 | 0xa005));
                if let (true, Some(sub)) = (sub_ifd, self.long(at + 8))
                    && let Ok(sub) = usize::try_from(sub)
                    && ranges.len() < 4096
                    && !ranges.iter().any(|range| range.start == sub)
                {
                    pending.push(sub);
                }
            }
            ranges.push(table);
        }
        ranges
    }

    /// Zero the IFD1 table and its JPEG or strips outside the `referenced`
    /// ranges, cutting what ends the blob; data without a length stays.
    /// `None` when the thumbnail's offsets cannot be read.
    fn erase_thumbnail(
        &mut self,
        ifd1: usize,
        referenced: &[std::ops::Range<usize>],
    ) -> Option<()> {
        // A dangling IFD1 pointer: nothing to erase.
        let Some(table) = self.table(ifd1) else {
            return Some(());
        };
        let (offsets, lengths) = self.thumbnail_data(table.clone())?;
        let overlaps = |range: &std::ops::Range<usize>| {
            referenced.iter().any(|kept| kept.start < range.end && range.start < kept.end)
        };
        let mut truncate_at = self.bytes.len();
        for (offset, length) in offsets.into_iter().zip(lengths) {
            let start = usize::try_from(offset).ok()?;
            let end = start.saturating_add(usize::try_from(length).ok()?).min(self.bytes.len());
            // Stop at the first referenced value after the start.
            let end = referenced
                .iter()
                .map(|kept| kept.start)
                .filter(|&kept| kept > start)
                .fold(end, usize::min);
            if start >= end || overlaps(&(start..end)) {
                continue;
            }
            self.bytes[start..end].fill(0);
            if end == self.bytes.len() {
                truncate_at = truncate_at.min(start);
            }
        }
        let table = table.start..table.end.min(self.bytes.len());
        if !overlaps(&table) {
            self.bytes[table.clone()].fill(0);
            if table.end >= truncate_at {
                truncate_at = truncate_at.min(table.start);
            }
        }
        self.bytes.truncate(truncate_at);
        Some(())
    }

    /// Offsets and lengths of the thumbnail's JPEG or strips listed in the
    /// IFD1 `table`; no lengths when a JPEG offset comes without one.
    fn thumbnail_data(&self, table: std::ops::Range<usize>) -> Option<(Vec<u32>, Vec<u32>)> {
        let (mut offsets, mut lengths) = (Vec::new(), Vec::new());
        for at in (table.start + 2..table.end - 4).step_by(12) {
            match self.word(at) {
                // JPEGInterchangeFormat, StripOffsets.
                Some(0x0201 | 0x0111) => offsets.extend(self.values(at)?),
                // JPEGInterchangeFormatLength, StripByteCounts.
                Some(0x0202 | 0x0117) => lengths.extend(self.values(at)?),
                Some(_) => {}
                None => break,
            }
        }
        if lengths.is_empty() {
            offsets.clear();
        }
        (offsets.len() == lengths.len()).then_some((offsets, lengths))
    }

    /// Set `PixelXDimension`/`PixelYDimension` in the Exif IFD at `ifd`; a
    /// SHORT too small for the new size becomes a LONG (both fit the
    /// entry's 4-byte field).
    fn set_pixel_dimensions(&mut self, ifd: usize, width: u32, height: u32) {
        let Some(table) = self.table(ifd) else {
            return;
        };
        for at in (table.start + 2..table.end - 4).step_by(12) {
            let (Some(tag), Some(kind), Some(n), Some(_)) =
                (self.word(at), self.word(at + 2), self.long(at + 4), self.long(at + 8))
            else {
                break;
            };
            let value = match tag {
                0xa002 => width,
                0xa003 => height,
                _ => continue,
            };
            if n != 1 || !(kind == 3 || kind == 4) {
                continue;
            }
            match u16::try_from(value) {
                Ok(short) if kind == 3 => {
                    self.put_word(at + 8, short);
                    self.bytes[at + 10..at + 12].fill(0);
                }
                _ => {
                    self.put_word(at + 2, 4);
                    self.put_long(at + 8, value);
                }
            }
        }
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
        // IFD1: a JPEG thumbnail of 15 bytes at 98, no next IFD.
        t.extend_from_slice(&[2, 0]);
        t.extend_from_slice(&[0x01, 0x02, 4, 0, 1, 0, 0, 0, 98, 0, 0, 0]);
        t.extend_from_slice(&[0x02, 0x02, 4, 0, 1, 0, 0, 0, 15, 0, 0, 0]);
        t.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(t.len(), 98);
        t.extend_from_slice(b"\xff\xd8THUMBMARK\xff\xd9");
        t
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn thumbnail_strips_are_erased() {
        // IFD0 with no entries; IFD1 at 14: StripOffsets LONG x2 at 44,
        // StripByteCounts SHORT x2 inline; strips at 52 and 60, then a tail.
        let mut t = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 0, 0, 14, 0, 0, 0];
        t.extend_from_slice(&[2, 0]);
        t.extend_from_slice(&[0x11, 0x01, 4, 0, 2, 0, 0, 0, 44, 0, 0, 0]);
        t.extend_from_slice(&[0x17, 0x01, 3, 0, 2, 0, 0, 0, 8, 0, 8, 0]);
        t.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(t.len(), 44);
        t.extend_from_slice(&[52, 0, 0, 0, 60, 0, 0, 0]);
        t.extend_from_slice(b"STRIPONESTRIPTWOtail");
        let fixed = resave_exif(&t, Some((1, 1))).unwrap();
        assert_eq!(fixed.len(), t.len(), "data before the end is zeroed in place");
        assert!(!contains(&fixed, b"STRIPONE") && !contains(&fixed, b"STRIPTWO"));
        assert!(fixed.ends_with(b"tail"));
        assert_eq!(&fixed[10..14], &[0; 4], "IFD1 unlinked");

        // A strip pointing into the header leaves the header intact.
        let mut bad = t.clone();
        bad[44] = 2;
        let fixed = resave_exif(&bad, Some((1, 1))).unwrap();
        assert_eq!(&fixed[..8], &bad[..8]);
        assert!(!contains(&fixed, b"STRIPTWO"));
    }

    fn parse_exif(tiff: Vec<u8>) -> exif::Exif {
        exif::Reader::new().read_raw(tiff).unwrap()
    }

    /// TIFF lets IFD1 and its thumbnail come before IFD0.
    #[test]
    fn thumbnail_before_ifd0_is_erased() {
        let mut t = vec![b'I', b'I', 42, 0, 48, 0, 0, 0];
        // IFD1 at 8: a 10-byte JPEG at 38, no next IFD.
        t.extend_from_slice(&[2, 0]);
        t.extend_from_slice(&[0x01, 0x02, 4, 0, 1, 0, 0, 0, 38, 0, 0, 0]);
        t.extend_from_slice(&[0x02, 0x02, 4, 0, 1, 0, 0, 0, 10, 0, 0, 0]);
        t.extend_from_slice(&[0, 0, 0, 0]);
        t.extend_from_slice(b"THUMBDATA!");
        assert_eq!(t.len(), 48);
        // IFD0 at 48: Make "Cam", next IFD 8.
        t.extend_from_slice(&[1, 0, 0x0f, 0x01, 2, 0, 4, 0, 0, 0, b'C', b'a', b'm', 0, 8, 0, 0, 0]);
        let fixed = resave_exif(&t, Some((1, 1))).unwrap();
        assert!(!contains(&fixed, b"THUMBDATA"));
        let after = parse_exif(fixed);
        assert!(after.get_field(exif::Tag::Make, exif::In::PRIMARY).is_some());
        assert!(after.fields().all(|f| f.ifd_num != exif::In::THUMBNAIL));
    }

    #[test]
    fn jpeg_offset_without_length_unlinks() {
        let mut t = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 0, 0, 14, 0, 0, 0];
        t.extend_from_slice(&[1, 0, 0x01, 0x02, 4, 0, 1, 0, 0, 0, 32, 0, 0, 0, 0, 0, 0, 0]);
        t.extend_from_slice(b"JPEGDATA");
        let fixed = resave_exif(&t, Some((1, 1))).unwrap();
        assert_eq!(&fixed[10..14], &[0; 4], "IFD1 unlinked");
        assert!(parse_exif(fixed).fields().all(|f| f.ifd_num != exif::In::THUMBNAIL));
    }

    /// An overstated thumbnail length must not reach the GPS IFD after it.
    #[test]
    fn overstated_length_spares_gps() {
        let mut t = vec![b'I', b'I', 42, 0, 8, 0, 0, 0];
        // IFD0 at 8: GPS IFD pointer to 62, next IFD 26.
        t.extend_from_slice(&[1, 0, 0x25, 0x88, 4, 0, 1, 0, 0, 0, 62, 0, 0, 0, 26, 0, 0, 0]);
        // IFD1 at 26: a JPEG at 56 whose length runs to the end of the blob.
        t.extend_from_slice(&[2, 0]);
        t.extend_from_slice(&[0x01, 0x02, 4, 0, 1, 0, 0, 0, 56, 0, 0, 0]);
        t.extend_from_slice(&[0x02, 0x02, 4, 0, 1, 0, 0, 0, 200, 0, 0, 0]);
        t.extend_from_slice(&[0, 0, 0, 0]);
        t.extend_from_slice(b"THUMB!");
        assert_eq!(t.len(), 62);
        // GPS IFD at 62: GPSLatitudeRef "N".
        t.extend_from_slice(&[1, 0, 0x01, 0x00, 2, 0, 2, 0, 0, 0, b'N', 0, 0, 0, 0, 0, 0, 0]);
        let fixed = resave_exif(&t, Some((1, 1))).unwrap();
        assert!(!contains(&fixed, b"THUMB!"));
        let after = parse_exif(fixed);
        assert!(after.get_field(exif::Tag::GPSLatitudeRef, exif::In::PRIMARY).is_some());
    }

    #[test]
    fn resave_refreshes_size_drops_thumbnail() {
        let source = exif_with_dimensions_and_thumbnail();
        let parse = parse_exif;
        let before = parse(source.clone());
        assert!(before.fields().any(|f| f.ifd_num == exif::In::THUMBNAIL));

        let fixed = resave_exif(&source, Some((70_000, 20))).unwrap();
        assert!(contains(&source, b"THUMBMARK"));
        assert!(!contains(&fixed, b"THUMBMARK"), "thumbnail bytes erased");
        assert_eq!(fixed.len(), 68, "IFD1 and the thumbnail ending the blob are cut");
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
