// SPDX-License-Identifier: GPL-3.0-or-later
//! Validate BMFF box boundaries before codecs allocate from declared sizes.
use std::io::{Read, Seek, SeekFrom};
fn invalid() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "AVIF inválido: caixa fora do contêiner ou complexidade excessiva",
    )
}
pub(crate) fn inspect(reader: &mut (impl Read + Seek)) -> std::io::Result<()> {
    let end = reader.seek(SeekFrom::End(0))?;
    let mut count = 0;
    boxes(reader, 0, end, 0, &mut count)
}
fn boxes(
    reader: &mut (impl Read + Seek),
    mut position: u64,
    end: u64,
    depth: u8,
    count: &mut u32,
) -> std::io::Result<()> {
    if depth > 16 {
        return Err(invalid());
    }
    while position < end {
        *count += 1;
        if *count > 100_000 || end - position < 8 {
            return Err(invalid());
        }
        reader.seek(SeekFrom::Start(position))?;
        let mut header = [0; 8];
        reader.read_exact(&mut header)?;
        let short = u32::from_be_bytes(header[..4].try_into().unwrap());
        let (size, header_size) = match short {
            0 => (end - position, 8u64),
            1 => {
                if end - position < 16 {
                    return Err(invalid());
                }
                let mut size = [0; 8];
                reader.read_exact(&mut size)?;
                (u64::from_be_bytes(size), 16)
            }
            size => (u64::from(size), 8),
        };
        let next = position.checked_add(size).filter(|next| *next <= end).ok_or_else(invalid)?;
        if size < header_size {
            return Err(invalid());
        }
        let content = position + header_size;
        match &header[4..8] {
            b"meta" | b"iref" => {
                if size < header_size + 4 {
                    return Err(invalid());
                }
                boxes(reader, content + 4, next, depth + 1, count)?;
            }
            b"iprp" | b"ipco" | b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"edts"
            | b"dinf" => {
                boxes(reader, content, next, depth + 1, count)?;
            }
            b"iinf" | b"ipro" => {
                if size < header_size + 6 {
                    return Err(invalid());
                }
                let mut full = [0; 4];
                reader.read_exact(&mut full)?;
                let entries = if &header[4..8] == b"iinf" && full[0] != 0 { 4 } else { 2 };
                if size < header_size + 4 + entries {
                    return Err(invalid());
                }
                boxes(reader, content + 4 + entries, next, depth + 1, count)?;
            }
            _ => {}
        }
        position = next;
    }
    Ok(())
}

/// Display transform of the primary AVIF item, from its `irot`/`imir`
/// properties. The `image` AVIF decoder ignores both, so phone pictures
/// stored sideways would otherwise be shown and edited sideways.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ItemTransform {
    /// Anticlockwise quarter turns (`irot` angle, 0..=3).
    pub(crate) quarter_turns_ccw: u8,
    /// `imir` mode: `Some(false)` exchanges top and bottom, `Some(true)`
    /// exchanges left and right (HEIF amendment / libavif / mp4parse).
    pub(crate) mirror_left_right: Option<bool>,
}

impl ItemTransform {
    /// MIAF order: rotation first, then mirror.
    pub(crate) fn apply(self, image: image::DynamicImage) -> image::DynamicImage {
        let image = match self.quarter_turns_ccw & 3 {
            1 => image.rotate270(),
            2 => image.rotate180(),
            3 => image.rotate90(),
            _ => image,
        };
        match self.mirror_left_right {
            Some(true) => image.fliph(),
            Some(false) => image.flipv(),
            None => image,
        }
    }
}

/// Read the primary item's transform. Only the top-level `meta` box is
/// loaded (bounded to 1 MiB); malformed or absent properties mean "no
/// transform" rather than an error, since pixels can still be shown.
pub(crate) fn primary_item_transform(
    reader: &mut (impl Read + Seek),
) -> std::io::Result<ItemTransform> {
    const MAX_META: u64 = 1024 * 1024;
    let end = reader.seek(SeekFrom::End(0))?;
    let mut position = 0;
    while end - position >= 8 {
        reader.seek(SeekFrom::Start(position))?;
        let mut header = [0; 8];
        reader.read_exact(&mut header)?;
        let (size, header_size) = match u32::from_be_bytes(header[..4].try_into().unwrap()) {
            0 => (end - position, 8),
            1 => {
                let mut size = [0; 8];
                reader.read_exact(&mut size)?;
                (u64::from_be_bytes(size), 16)
            }
            size => (u64::from(size), 8),
        };
        if size < header_size || size > end - position {
            break;
        }
        if &header[4..8] == b"meta" {
            if size - header_size > MAX_META {
                break;
            }
            let mut meta = vec![0; (size - header_size) as usize];
            reader.read_exact(&mut meta)?;
            return Ok(parse_meta(&meta).unwrap_or_default());
        }
        position += size;
    }
    Ok(ItemTransform::default())
}

/// Child boxes of `data` as `(type, content)` pairs; `None` if malformed.
fn children(data: &[u8]) -> Option<Vec<([u8; 4], &[u8])>> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let short = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?);
        let kind: [u8; 4] = rest.get(4..8)?.try_into().ok()?;
        let (size, header) = match short {
            0 => (rest.len(), 8),
            1 => (usize::try_from(u64::from_be_bytes(rest.get(8..16)?.try_into().ok()?)).ok()?, 16),
            size => (size as usize, 8),
        };
        if size < header || size > rest.len() || out.len() >= 10_000 {
            return None;
        }
        out.push((kind, &rest[header..size]));
        rest = &rest[size..];
    }
    Some(out)
}

fn parse_meta(meta: &[u8]) -> Option<ItemTransform> {
    let boxes = children(meta.get(4..)?)?; // FullBox version/flags.
    fn find<'a>(list: &[([u8; 4], &'a [u8])], kind: &[u8; 4]) -> Option<&'a [u8]> {
        list.iter().find(|(k, _)| k == kind).map(|(_, content)| *content)
    }
    let pitm = find(&boxes, b"pitm")?;
    let primary = if *pitm.first()? == 0 {
        u32::from(u16::from_be_bytes(pitm.get(4..6)?.try_into().ok()?))
    } else {
        u32::from_be_bytes(pitm.get(4..8)?.try_into().ok()?)
    };
    let iprp = children(find(&boxes, b"iprp")?)?;
    let properties = children(find(&iprp, b"ipco")?)?;
    let mut transform = ItemTransform::default();
    for ipma in iprp.iter().filter(|(k, _)| k == b"ipma").map(|(_, content)| *content) {
        let version = *ipma.first()?;
        let wide_index = ipma.get(3)? & 1 == 1;
        let count = u32::from_be_bytes(ipma.get(4..8)?.try_into().ok()?);
        let mut cursor = 8;
        for _ in 0..count {
            let item = if version < 1 {
                cursor += 2;
                u32::from(u16::from_be_bytes(ipma.get(cursor - 2..cursor)?.try_into().ok()?))
            } else {
                cursor += 4;
                u32::from_be_bytes(ipma.get(cursor - 4..cursor)?.try_into().ok()?)
            };
            let associations = *ipma.get(cursor)?;
            cursor += 1;
            for _ in 0..associations {
                let index = if wide_index {
                    cursor += 2;
                    usize::from(
                        u16::from_be_bytes(ipma.get(cursor - 2..cursor)?.try_into().ok()?) & 0x7fff,
                    )
                } else {
                    cursor += 1;
                    usize::from(ipma.get(cursor - 1)? & 0x7f)
                };
                if item != primary || index == 0 {
                    continue;
                }
                match properties.get(index - 1) {
                    Some(([b'i', b'r', b'o', b't'], content)) => {
                        transform.quarter_turns_ccw = content.first()? & 3;
                    }
                    Some(([b'i', b'm', b'i', b'r'], content)) => {
                        transform.mirror_left_right = Some(content.first()? & 1 == 1);
                    }
                    _ => {}
                }
            }
        }
    }
    Some(transform)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
        let mut out = ((content.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(content);
        out
    }

    /// `ftyp` + `meta` with primary item 1, properties [ispe, irot, imir]
    /// and the given ipma associations for items 1 and 2.
    fn file(primary_assoc: &[u8], other_assoc: &[u8]) -> Vec<u8> {
        let ipco = [
            bx(b"ispe", &[0; 12]),
            bx(b"irot", &[1]), // 90° anticlockwise
            bx(b"imir", &[1]), // left ↔ right
            bx(b"irot", &[2]), // 180°, only on item 2
        ]
        .concat();
        let mut ipma = vec![0, 0, 0, 0, 0, 0, 0, 2];
        ipma.extend_from_slice(&[0, 1, primary_assoc.len() as u8]);
        ipma.extend_from_slice(primary_assoc);
        ipma.extend_from_slice(&[0, 2, other_assoc.len() as u8]);
        ipma.extend_from_slice(other_assoc);
        let iprp = bx(b"iprp", &[bx(b"ipco", &ipco), bx(b"ipma", &ipma)].concat());
        let pitm = bx(b"pitm", &[0, 0, 0, 0, 0, 1]);
        let meta = bx(b"meta", &[vec![0, 0, 0, 0], pitm, iprp].concat());
        [bx(b"ftyp", b"avifmif1"), meta, bx(b"mdat", &[0; 4])].concat()
    }

    fn parse(bytes: Vec<u8>) -> ItemTransform {
        primary_item_transform(&mut std::io::Cursor::new(bytes)).unwrap()
    }

    #[test]
    fn primary_item_rotation_and_mirror_are_read() {
        let t = parse(file(&[1, 0x82, 0x83], &[0x84]));
        assert_eq!(t, ItemTransform { quarter_turns_ccw: 1, mirror_left_right: Some(true) });
        // Properties of other items are ignored.
        assert_eq!(parse(file(&[1], &[0x84, 2])), ItemTransform::default());
        assert_eq!(parse(file(&[], &[])), ItemTransform::default());
        // Truncated meta degrades to "no transform".
        let mut bytes = file(&[2], &[]);
        bytes.truncate(40);
        assert_eq!(parse(bytes), ItemTransform::default());
    }

    #[test]
    fn transform_rotates_then_mirrors() {
        // 2x1: red | green.
        let mut img = image::RgbImage::new(2, 1);
        img.put_pixel(0, 0, image::Rgb([255, 0, 0]));
        img.put_pixel(1, 0, image::Rgb([0, 255, 0]));
        let img = image::DynamicImage::ImageRgb8(img);
        // 90° anticlockwise: right column goes to the top.
        let ccw =
            ItemTransform { quarter_turns_ccw: 1, mirror_left_right: None }.apply(img.clone());
        assert_eq!(ccw.to_rgb8().dimensions(), (1, 2));
        assert_eq!(ccw.to_rgb8().get_pixel(0, 0).0, [0, 255, 0]);
        let both = ItemTransform { quarter_turns_ccw: 0, mirror_left_right: Some(true) }.apply(img);
        assert_eq!(both.to_rgb8().get_pixel(0, 0).0, [0, 255, 0]);
    }
}
