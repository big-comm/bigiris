// SPDX-License-Identifier: GPL-3.0-or-later
//! EXIF for an in-place re-save: the source block rebuilt from its parsed
//! primary-image fields, so nothing of the old layout (the IFD1 thumbnail
//! showing the pre-edit image above all) is copied byte for byte.

use std::collections::{HashMap, HashSet};

use exif::{Field, In, Tag, Value};

/// Most entries one IFD written by kamadak-exif may hold: its count is a
/// `u16` that also takes the synthesized sub-IFD pointers.
const MAX_IFD_FIELDS: usize = 65_000;

/// IFDs kamadak-exif follows through the next-IFD links.
const MAX_CHAINED_IFDS: usize = 8;

/// An EXIF block rebuilt by [`resave_exif`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RebuiltExif {
    /// TIFF-structured block, without the `Exif\0\0` prefix.
    pub(crate) tiff: Vec<u8>,
    /// Source fields that could not be read or written were left out.
    pub(crate) dropped_fields: bool,
}

/// The primary-image fields of `exif` (IFD0, Exif, GPS and
/// interoperability) written into a new TIFF block for upright pixels of
/// `width` x `height`: `Orientation` 1, `PixelXDimension` and
/// `PixelYDimension` set where present, no IFD1 and so no thumbnail.
///
/// The `MakerNote` is copied as opaque bytes: a vendor note that addresses
/// its own data by absolute offsets into the block (some cameras do) points
/// at the wrong bytes afterwards, and previews inside it stay. `None` when
/// the block is not TIFF, repeats a sub-IFD pointer, holds an IFD too
/// large to write, or cannot be read at all.
pub(crate) fn resave_exif(exif: &[u8], (width, height): (u32, u32)) -> Option<RebuiltExif> {
    let tiff = exif.strip_prefix(b"Exif\0\0").unwrap_or(exif);
    if !sub_ifd_pointers_unique(tiff) {
        return None;
    }
    let reader = exif::Reader::new().continue_on_error(true).read_raw(tiff.to_vec());
    let (parsed, mut dropped_fields) = match reader {
        Ok(parsed) => (parsed, false),
        Err(exif::Error::PartialResult(partial)) => (partial.into_inner().0, true),
        Err(_) => return None,
    };
    let mut seen = HashSet::new();
    let mut per_ifd: HashMap<exif::Context, usize> = HashMap::new();
    let mut fields = Vec::new();
    for field in parsed.fields().filter(|field| field.ifd_num == In::PRIMARY) {
        if matches!(field.value, Value::Unknown(..)) {
            dropped_fields = true;
            continue;
        }
        // The writer refuses duplicates; a repeated tag keeps its first value.
        if !seen.insert(field.tag) {
            continue;
        }
        let count = per_ifd.entry(field.tag.0).or_default();
        *count += 1;
        if *count > MAX_IFD_FIELDS {
            return None;
        }
        let value = match field.tag {
            Tag::Orientation => Value::Short(vec![1]),
            Tag::PixelXDimension => dimension(&field.value, width),
            Tag::PixelYDimension => dimension(&field.value, height),
            _ => field.value.clone(),
        };
        fields.push(Field { tag: field.tag, ifd_num: In::PRIMARY, value });
    }
    let mut writer = exif::experimental::Writer::new();
    for field in &fields {
        writer.push_field(field);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    writer.write(&mut out, parsed.little_endian()).ok()?;
    Some(RebuiltExif { tiff: out.into_inner(), dropped_fields })
}

/// A pixel dimension in the source field's type, widened to LONG when it
/// does not fit a SHORT.
fn dimension(source: &Value, size: u32) -> Value {
    match (source, u16::try_from(size)) {
        (Value::Short(_), Ok(short)) => Value::Short(vec![short]),
        _ => Value::Long(vec![size]),
    }
}

/// Whether no IFD of `tiff` repeats a sub-IFD pointer. kamadak-exif parses
/// the target of every pointer entry, so one repeated N times in an IFD
/// pointing at itself costs N x N entries; this scan reads each table once.
fn sub_ifd_pointers_unique(tiff: &[u8]) -> bool {
    let little = tiff.starts_with(b"II");
    let word = |at: usize| {
        let b: [u8; 2] = tiff.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if little { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
    };
    let long = |at: usize| {
        let b: [u8; 4] = tiff.get(at..at.checked_add(4)?)?.try_into().ok()?;
        let value = if little { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) };
        usize::try_from(value).ok()
    };
    // The targets of the `pointers` tags in the IFD at `ifd`, and the offset
    // of its next-IFD link; `None` when a tag repeats or the table is cut.
    let scan = |ifd: usize, pointers: &[u16]| -> Option<(Vec<(u16, usize)>, usize)> {
        let count = usize::from(word(ifd)?);
        let mut targets: Vec<(u16, usize)> = Vec::new();
        for at in (0..count).map(|entry| ifd + 2 + entry * 12) {
            let tag = word(at)?;
            if pointers.contains(&tag) {
                if targets.iter().any(|&(seen, _)| seen == tag) {
                    return None;
                }
                targets.push((tag, long(at + 8)?));
            }
        }
        Some((targets, ifd + 2 + count * 12))
    };
    let Some(mut ifd) = long(4) else {
        return false;
    };
    for _ in 0..MAX_CHAINED_IFDS {
        if ifd == 0 {
            break;
        }
        // ExifIFDPointer and GPSInfoIFDPointer; the Exif IFD's own
        // InteropIFDPointer.
        let Some((targets, link)) = scan(ifd, &[0x8769, 0x8825]) else {
            return false;
        };
        for &(_, exif_ifd) in targets.iter().filter(|&&(tag, _)| tag == 0x8769) {
            if scan(exif_ifd, &[0xa005]).is_none() {
                return false;
            }
        }
        let Some(next) = long(link) else {
            return false;
        };
        ifd = next;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use exif::Rational;

    fn field(tag: Tag, ifd_num: In, value: Value) -> Field {
        Field { tag, ifd_num, value }
    }

    fn ascii(text: &str) -> Value {
        Value::Ascii(vec![text.as_bytes().to_vec()])
    }

    /// A camera block written by kamadak-exif: IFD0 with Make, DateTime and
    /// Orientation 6; Exif with the capture date, pixel size and a maker
    /// note; GPS; interoperability; IFD1 with a JPEG thumbnail.
    fn camera_block(little_endian: bool) -> Vec<u8> {
        let fields = [
            field(Tag::Make, In::PRIMARY, ascii("Cam")),
            field(Tag::DateTime, In::PRIMARY, ascii("2024:05:06 07:08:09")),
            field(Tag::Orientation, In::PRIMARY, Value::Short(vec![6])),
            field(Tag::DateTimeOriginal, In::PRIMARY, ascii("2024:05:06 07:08:09")),
            field(Tag::PixelXDimension, In::PRIMARY, Value::Short(vec![4000])),
            field(Tag::PixelYDimension, In::PRIMARY, Value::Long(vec![3000])),
            field(Tag::MakerNote, In::PRIMARY, Value::Undefined(b"VENDORNOTE".to_vec(), 0)),
            field(Tag::GPSLatitudeRef, In::PRIMARY, ascii("N")),
            field(
                Tag::GPSLatitude,
                In::PRIMARY,
                Value::Rational(vec![Rational { num: 48, denom: 1 }; 3]),
            ),
            field(Tag::InteroperabilityIndex, In::PRIMARY, ascii("R98")),
            field(Tag::ImageDescription, In::THUMBNAIL, ascii("thumb")),
        ];
        let mut writer = exif::experimental::Writer::new();
        for field in &fields {
            writer.push_field(field);
        }
        writer.set_jpeg(b"\xff\xd8THUMBMARK\xff\xd9", In::THUMBNAIL);
        let mut out = std::io::Cursor::new(Vec::new());
        writer.write(&mut out, little_endian).unwrap();
        out.into_inner()
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    fn parse(tiff: Vec<u8>) -> exif::Exif {
        exif::Reader::new().read_raw(tiff).unwrap()
    }

    #[test]
    fn rebuilt_block_keeps_primary_fields() {
        for little_endian in [true, false] {
            let source = camera_block(little_endian);
            assert!(contains(&source, b"THUMBMARK"));
            let rebuilt = resave_exif(&[b"Exif\0\0", &source[..]].concat(), (70_000, 20)).unwrap();
            assert!(!rebuilt.dropped_fields);
            assert!(!contains(&rebuilt.tiff, b"THUMBMARK"), "thumbnail bytes survived");
            let after = parse(rebuilt.tiff);
            assert_eq!(after.little_endian(), little_endian);
            assert!(after.fields().all(|f| f.ifd_num == In::PRIMARY), "IFD1 survived");
            let value = |tag| after.get_field(tag, In::PRIMARY).map(|f| f.value.clone());
            let text = |tag| match value(tag) {
                Some(Value::Ascii(lines)) => lines.first().cloned(),
                _ => None,
            };
            assert_eq!(value(Tag::Orientation).and_then(|v| v.get_uint(0)), Some(1));
            assert_eq!(value(Tag::PixelXDimension).and_then(|v| v.get_uint(0)), Some(70_000));
            assert_eq!(value(Tag::PixelYDimension).and_then(|v| v.get_uint(0)), Some(20));
            assert_eq!(text(Tag::Make).as_deref(), Some(&b"Cam"[..]));
            assert_eq!(text(Tag::DateTimeOriginal).as_deref(), Some(&b"2024:05:06 07:08:09"[..]));
            assert_eq!(text(Tag::GPSLatitudeRef).as_deref(), Some(&b"N"[..]));
            assert!(value(Tag::GPSLatitude).is_some());
            assert_eq!(text(Tag::InteroperabilityIndex).as_deref(), Some(&b"R98"[..]));
            assert!(
                matches!(value(Tag::MakerNote), Some(Value::Undefined(b, _)) if b == b"VENDORNOTE")
            );
        }
        // A size that fits keeps a SHORT a SHORT.
        let small = parse(resave_exif(&camera_block(true), (300, 200)).unwrap().tiff);
        let width = small.get_field(Tag::PixelXDimension, In::PRIMARY).unwrap();
        assert!(matches!(width.value, Value::Short(_)));
        assert_eq!(resave_exif(b"not exif", (1, 1)), None);
    }

    /// A JPEG offset without its length tag: nothing locates the bytes, and
    /// none of them is copied.
    #[test]
    fn thumbnail_without_length_leaves_nothing() {
        // IFD0 at 8: Make "Cam"; IFD1 at 26: JPEGInterchangeFormat 44 only.
        let mut t = vec![b'I', b'I', 42, 0, 8, 0, 0, 0];
        t.extend_from_slice(&[
            1, 0, 0x0f, 0x01, 2, 0, 4, 0, 0, 0, b'C', b'a', b'm', 0, 26, 0, 0, 0,
        ]);
        t.extend_from_slice(&[1, 0, 0x01, 0x02, 4, 0, 1, 0, 0, 0, 44, 0, 0, 0, 0, 0, 0, 0]);
        t.extend_from_slice(b"\xff\xd8JPEGDATA\xff\xd9");
        let rebuilt = resave_exif(&t, (1, 1)).unwrap();
        assert!(!contains(&rebuilt.tiff, b"JPEGDATA"));
        assert!(parse(rebuilt.tiff).get_field(Tag::Make, In::PRIMARY).is_some());
    }

    /// IFD0 of 65535 Exif pointers at itself: kamadak-exif alone parses
    /// 65535 x 65535 entries; the block is refused after one table scan.
    #[test]
    fn self_pointing_ifd_stays_linear() {
        let mut t = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 0xff, 0xff];
        for _ in 0..65_535 {
            t.extend_from_slice(&[0x69, 0x87, 4, 0, 1, 0, 0, 0, 8, 0, 0, 0]);
        }
        t.extend_from_slice(&[0; 4]);
        // IFD0 with one Exif pointer to an Exif IFD of 65535 interop
        // pointers at itself.
        let mut nested = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0];
        nested.extend_from_slice(&[0x69, 0x87, 4, 0, 1, 0, 0, 0, 26, 0, 0, 0, 0, 0, 0, 0]);
        nested.extend_from_slice(&[0xff, 0xff]);
        for _ in 0..65_535 {
            nested.extend_from_slice(&[0x05, 0xa0, 4, 0, 1, 0, 0, 0, 26, 0, 0, 0]);
        }
        nested.extend_from_slice(&[0; 4]);
        for block in [t, nested] {
            let start = std::time::Instant::now();
            assert_eq!(resave_exif(&block, (1, 1)), None);
            let elapsed = start.elapsed();
            assert!(elapsed < std::time::Duration::from_millis(500), "{elapsed:?}");
        }
    }

    /// A field type the writer cannot encode is reported, not lost silently.
    #[test]
    fn unwritable_field_is_reported() {
        // IFD0 at 8: Make "Cam" and tag 0x9999 of type 99.
        let mut t = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 2, 0];
        t.extend_from_slice(&[0x0f, 0x01, 2, 0, 4, 0, 0, 0, b'C', b'a', b'm', 0]);
        t.extend_from_slice(&[0x99, 0x99, 99, 0, 1, 0, 0, 0, 1, 2, 3, 4]);
        t.extend_from_slice(&[0; 4]);
        let rebuilt = resave_exif(&t, (1, 1)).unwrap();
        assert!(rebuilt.dropped_fields);
        assert!(parse(rebuilt.tiff).get_field(Tag::Make, In::PRIMARY).is_some());
    }
}
