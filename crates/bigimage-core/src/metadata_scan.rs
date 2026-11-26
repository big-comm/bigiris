// SPDX-License-Identifier: GPL-3.0-or-later
//! Whether an encoded file holds anything beyond what the encoders here
//! write, so the never-larger shortcut of a stripping conversion may publish
//! it unchanged.
//!
//! Each walker accepts only the structures this crate's encoders produce,
//! each at most once where the format allows one, colour profiles only when
//! they are the sRGB profile the encoders embed, and a clean end; anything
//! else (EXIF, XMP, text, C2PA, private chunks, bytes after the image)
//! counts as metadata.

use std::sync::OnceLock;

use crate::Format;

/// Whether `bytes`, a file in `format`, carries anything a stripping
/// re-encode would drop. `false` only for files built from the structures
/// the encoders write, ending where the format ends; formats that are not
/// walked (TIFF, EXR, HDR, PNM, TGA, ICO) always answer `true`.
pub(crate) fn carries_metadata(bytes: &[u8], format: Format) -> bool {
    match format {
        Format::Jpeg => jpeg_carries_metadata(bytes),
        Format::Png => png_carries_metadata(bytes),
        Format::WebP => webp_carries_metadata(bytes),
        Format::Avif => avif_carries_metadata(bytes),
        Format::Gif => gif_carries_metadata(bytes),
        Format::Bmp => bmp_carries_metadata(bytes),
        Format::Qoi => qoi_carries_metadata(bytes),
        Format::Tiff | Format::OpenExr | Format::Hdr | Format::Pnm | Format::Tga | Format::Ico => {
            true
        }
    }
}

/// Whether `profile` is the sRGB profile the encoders embed.
fn is_own_srgb(profile: &[u8]) -> bool {
    crate::color::srgb_profile().is_ok_and(|srgb| srgb == profile)
}

/// `false` only for a JPEG of one JFIF (without thumbnail), Adobe and
/// sRGB ICC segment, tables, frames and scans, ending at EOI.
fn jpeg_carries_metadata(bytes: &[u8]) -> bool {
    let mut seen = Vec::new();
    let mut at = 2;
    while let Some(&[prefix, marker]) = bytes.get(at..at + 2) {
        if prefix != 0xff {
            return true;
        }
        match marker {
            // Fill byte before a marker.
            0xff => {
                at += 1;
                continue;
            }
            0xd9 => return at + 2 != bytes.len(),
            _ => {}
        }
        let Some(&[high, low]) = bytes.get(at + 2..at + 4) else {
            return true;
        };
        let len = usize::from(u16::from_be_bytes([high, low]));
        let Some(payload) = len.checked_sub(2).and_then(|n| bytes.get(at + 4..at + 4 + n)) else {
            return true;
        };
        let written = match marker {
            // JFIF with a zero-size thumbnail: 14 bytes.
            0xe0 => {
                payload.len() == 14 && payload.starts_with(b"JFIF\0") && payload[12..] == [0, 0]
            }
            0xee => payload.len() == 12 && payload.starts_with(b"Adobe"),
            // One segment (sequence 1 of 1) holding the sRGB profile.
            0xe2 => payload.strip_prefix(b"ICC_PROFILE\0\x01\x01").is_some_and(is_own_srgb),
            // DQT, DHT, DRI, DAC, SOS and the SOF markers.
            0xdb | 0xc4 | 0xdd | 0xcc | 0xda => true,
            0xc0..=0xcf => marker != 0xc8,
            _ => false,
        };
        let once = matches!(marker, 0xe0 | 0xe2 | 0xee);
        if !written || (once && seen.contains(&marker)) {
            return true;
        }
        seen.push(marker);
        at += 2 + len;
        if marker == 0xda {
            let Some(end) = scan_end(bytes, at) else {
                return true;
            };
            at = end;
        }
    }
    true
}

/// Start of the marker that ends the entropy-coded data at `at`: the next
/// 0xFF that is neither a stuffed zero nor a restart marker.
fn scan_end(bytes: &[u8], mut at: usize) -> Option<usize> {
    loop {
        at += bytes.get(at..)?.iter().position(|&b| b == 0xff)?;
        match bytes.get(at + 1)? {
            0x00 | 0xd0..=0xd7 => at += 2,
            _ => return Some(at),
        }
    }
}

/// `(type, data)` of each chunk of a PNG, `None` when one runs past the
/// end or the last is not IEND ending the file.
fn png_chunks(bytes: &[u8]) -> Option<Vec<(&[u8], &[u8])>> {
    let mut chunks = Vec::new();
    let mut at = 8;
    while let Some(&[a, b, c, d, ref kind @ ..]) = bytes.get(at..at + 8) {
        let len = usize::try_from(u32::from_be_bytes([a, b, c, d])).ok()?;
        let data = bytes.get(at + 8..at.checked_add(8)?.checked_add(len)?)?;
        chunks.push((kind, data));
        at = at.checked_add(12)?.checked_add(len)?;
        if kind == b"IEND" {
            return (at == bytes.len()).then_some(chunks);
        }
    }
    None
}

/// `false` only for a PNG of the critical, colour and transparency chunks,
/// each once (but IDAT), with an iCCP only as the encoder writes the sRGB
/// profile.
fn png_carries_metadata(bytes: &[u8]) -> bool {
    let Some(chunks) = png_chunks(bytes) else {
        return true;
    };
    let mut seen: Vec<&[u8]> = Vec::new();
    for (kind, data) in chunks {
        let written = match kind {
            b"IDAT" | b"IEND" => true,
            b"IHDR" => data.len() == 13,
            b"PLTE" => data.len() <= 768 && data.len().is_multiple_of(3),
            b"tRNS" => data.len() <= 256,
            b"sRGB" => data.len() == 1,
            b"gAMA" => data.len() == 4,
            b"cHRM" => data.len() == 32,
            b"iCCP" => own_srgb_iccp().iter().any(|iccp| iccp == data),
            _ => false,
        };
        if !written || (kind != b"IDAT" && seen.contains(&kind)) {
            return true;
        }
        seen.push(kind);
    }
    false
}

/// The iCCP data the PNG encoder writes for the sRGB profile, per
/// compression preset; comparing the compressed bytes needs no inflater.
fn own_srgb_iccp() -> &'static [Vec<u8>] {
    static CHUNKS: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    CHUNKS.get_or_init(|| {
        let Ok(srgb) = crate::color::srgb_profile() else {
            return Vec::new();
        };
        let pixel = image::DynamicImage::ImageRgb8(image::RgbImage::new(1, 1));
        [false, true]
            .into_iter()
            .filter_map(|optimize| {
                let opts = crate::EncodeOptions { optimize, ..crate::EncodeOptions::default() };
                let png = crate::optimize::encode(&pixel, Format::Png, &opts, Some(&srgb), None);
                let png = png.ok().flatten()?;
                let chunks = png_chunks(&png)?;
                let (_, data) = chunks.into_iter().find(|(kind, _)| *kind == b"iCCP")?;
                Some(data.to_vec())
            })
            .collect()
    })
}

/// `false` only for a WebP of one image, alpha, extended-header and sRGB
/// ICC chunk each, ending exactly at the end of the file.
fn webp_carries_metadata(bytes: &[u8]) -> bool {
    let mut seen: Vec<&[u8]> = Vec::new();
    let mut at = 12;
    while let Some(&[ref kind @ .., a, b, c, d]) = bytes.get(at..at + 8) {
        let len = usize::try_from(u32::from_le_bytes([a, b, c, d])).unwrap_or(usize::MAX);
        let written = match kind {
            b"VP8 " | b"VP8L" | b"VP8X" | b"ALPH" => true,
            b"ICCP" => {
                bytes.get(at + 8..at.saturating_add(8).saturating_add(len)).is_some_and(is_own_srgb)
            }
            _ => false,
        };
        // One image chunk, lossy or lossless.
        let image = |kind: &[u8]| matches!(kind, b"VP8 " | b"VP8L");
        let repeated = seen.iter().any(|&other| other == kind || (image(kind) && image(other)));
        if !written || repeated {
            return true;
        }
        seen.push(kind);
        at = at.saturating_add(8).saturating_add(len).saturating_add(len & 1);
    }
    at != bytes.len()
}

/// `false` only for an AVIF of one `ftyp`, `meta` and `mdat` box ending at
/// the end of the file, whose `meta` holds what the AV1 encoder writes.
fn avif_carries_metadata(bytes: &[u8]) -> bool {
    let mut seen: Vec<&[u8]> = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let Some(&[a, b, c, d, ref kind @ ..]) = bytes.get(at..at + 8) else {
            return true;
        };
        let size = match u32::from_be_bytes([a, b, c, d]) {
            0 => bytes.len() - at,
            1 => {
                let Some(large) = bytes.get(at + 8..at + 16) else {
                    return true;
                };
                let large = u64::from_be_bytes(large.try_into().expect("eight bytes"));
                usize::try_from(large).unwrap_or(usize::MAX)
            }
            size => usize::try_from(size).unwrap_or(usize::MAX),
        };
        let Some(content) = bytes.get(at + 8..at.saturating_add(size)).filter(|_| size >= 8) else {
            return true;
        };
        let written = match kind {
            b"meta" => !avif_meta_carries_metadata(content),
            b"ftyp" | b"mdat" => true,
            _ => false,
        };
        if !written || seen.contains(&kind) {
            return true;
        }
        seen.push(kind);
        at += size;
    }
    false
}

/// Whether an AVIF `meta` box (`content` after its header) holds anything
/// but the handler, item locations and references, AV1 items without names
/// and the image, colour and alpha properties.
fn avif_meta_carries_metadata(content: &[u8]) -> bool {
    use crate::container_bounds::children;
    let empty_name = |infe: &[u8]| {
        // FullBox, item ID (2 bytes in version 2, 4 in 3), protection index.
        let fields = match infe.first() {
            Some(2) => 4 + 2 + 2,
            Some(3) => 4 + 4 + 2,
            _ => return false,
        };
        infe.get(fields..) == Some(b"av01\0")
    };
    let property = |(kind, content): &([u8; 4], &[u8])| match kind {
        b"ispe" | b"av1C" | b"pixi" | b"irot" | b"imir" => true,
        b"colr" => content.starts_with(b"nclx") && content.len() == 11,
        b"auxC" => content == b"\0\0\0\0urn:mpeg:mpegB:cicp:systems:auxiliary:alpha\0",
        _ => false,
    };
    let Some(boxes) = content.get(4..).and_then(children) else {
        return true;
    };
    !boxes.iter().all(|(kind, content)| match kind {
        b"pitm" | b"iloc" | b"iref" => true,
        // FullBox, `pict`, reserved, empty name.
        b"hdlr" => content.len() == 25 && content[8..12] == *b"pict" && content[24] == 0,
        b"iinf" => {
            let entries = if content.first() == Some(&0) { 6 } else { 8 };
            content.get(entries..).and_then(children).is_some_and(|infes| {
                infes.iter().all(|(kind, infe)| kind == b"infe" && empty_name(infe))
            })
        }
        b"iprp" => children(content).is_some_and(|parts| {
            parts.iter().all(|(kind, part)| match kind {
                b"ipco" => children(part).is_some_and(|props| props.iter().all(property)),
                b"ipma" => true,
                _ => false,
            })
        }),
        _ => false,
    })
}

/// `false` only for a GIF of images, graphic controls and loop extensions
/// whose trailer is its last byte.
fn gif_carries_metadata(bytes: &[u8]) -> bool {
    let Some(&screen) = bytes.get(10) else {
        return true;
    };
    let mut at = 13 + colour_table_len(screen);
    loop {
        let next = match bytes.get(at) {
            Some(0x3b) => return at + 1 != bytes.len(),
            Some(0x21) => {
                let loop_extension = bytes.get(at + 1) == Some(&0xff)
                    && matches!(
                        bytes.get(at + 2..at + 14),
                        Some(b"\x0bNETSCAPE2.0" | b"\x0bANIMEXTS1.0")
                    );
                if bytes.get(at + 1) != Some(&0xf9) && !loop_extension {
                    return true;
                }
                skip_sub_blocks(bytes, at + 2)
            }
            Some(0x2c) => {
                let Some(&local) = bytes.get(at + 9) else {
                    return true;
                };
                // Descriptor, local table, LZW code size, then the data.
                skip_sub_blocks(bytes, at + 10 + colour_table_len(local) + 1)
            }
            _ => return true,
        };
        let Some(next) = next else {
            return true;
        };
        at = next;
    }
}

/// Bytes of the colour table a GIF screen or image descriptor's packed
/// `flags` declare.
fn colour_table_len(flags: u8) -> usize {
    if flags & 0x80 == 0 { 0 } else { 3 << (1 + (flags & 7)) }
}

/// Offset after the data sub-blocks starting at `at` and their terminator.
fn skip_sub_blocks(bytes: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let size = usize::from(*bytes.get(at)?);
        at += 1 + size;
        if size == 0 {
            return Some(at);
        }
    }
}

/// `false` only for an uncompressed or bit-field BMP whose pixel array
/// follows its headers and palette and ends the file, with no embedded or
/// linked colour profile.
fn bmp_carries_metadata(bytes: &[u8]) -> bool {
    let field = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    let layout = || -> Option<bool> {
        let (file_size, offset, header) = (field(2)?, field(10)?, field(14)?);
        if !matches!(header, 40 | 108 | 124) {
            return None;
        }
        let width = u64::from(field(18)?);
        let height = u64::from(field(22)?.cast_signed().unsigned_abs());
        let bits = u64::from(u16::from_le_bytes(bytes.get(28..30)?.try_into().ok()?));
        let compression = field(30)?;
        let colours = u64::from(field(46)?);
        // bV4CSType: an embedded or linked profile follows the pixels.
        if header > 40 && matches!(&field(70)?.to_be_bytes(), b"MBED" | b"LINK") {
            return None;
        }
        let masks = match (header, compression) {
            (40, 3) => 12,
            (40, 6) => 16,
            (_, 0 | 3 | 6) => 0,
            _ => return None,
        };
        let palette = if colours == 0 && bits <= 8 { 1 << bits } else { colours };
        let headers = 14 + u64::from(header) + masks + 4 * palette;
        let row = (width.checked_mul(bits)?.checked_add(31)? / 32).checked_mul(4)?;
        let end = u64::from(offset).checked_add(row.checked_mul(height)?)?;
        let len = bytes.len() as u64;
        Some(u64::from(offset) == headers && end == len && u64::from(file_size) == len)
    };
    layout() != Some(true)
}

/// `false` only for a QOI whose end marker follows its last pixel and
/// ends the file.
fn qoi_carries_metadata(bytes: &[u8]) -> bool {
    const END: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 1];
    let size = |at: usize| {
        bytes
            .get(at..at + 4)
            .map(|b| u64::from(u32::from_be_bytes(b.try_into().expect("four bytes"))))
    };
    let (Some(width), Some(height)) = (size(4), size(8)) else {
        return true;
    };
    let pixels = width * height;
    let (mut at, mut decoded) = (14, 0u64);
    while decoded < pixels {
        let Some(&op) = bytes.get(at) else {
            return true;
        };
        let (len, count) = match op {
            0xfe => (4, 1),
            0xff => (5, 1),
            // QOI_OP_LUMA: one more byte.
            0x80..=0xbf => (2, 1),
            // QOI_OP_RUN.
            0xc0..=0xfd => (1, u64::from(op & 0x3f) + 1),
            // QOI_OP_INDEX and QOI_OP_DIFF.
            _ => (1, 1),
        };
        at += len;
        decoded += count;
    }
    decoded != pixels || bytes.get(at..) != Some(&END[..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb() -> image::DynamicImage {
        image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(8, 8, |x, y| {
            image::Rgb([x as u8 * 30, y as u8 * 30, 9])
        }))
    }

    fn encode(image: &image::DynamicImage, format: image::ImageFormat) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    fn splice(bytes: &[u8], at: usize, insert: &[u8]) -> Vec<u8> {
        [&bytes[..at], insert, &bytes[at..]].concat()
    }

    fn check(cases: &[(&str, Vec<u8>, bool)], format: Format) {
        for (name, bytes, expected) in cases {
            assert_eq!(carries_metadata(bytes, format), *expected, "{name}");
        }
    }

    /// What the encoders write must stay publishable as it is.
    #[test]
    fn own_encoder_output_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        let rgba = image::DynamicImage::ImageRgba8(rgb().to_rgba8());
        for format in [
            Format::Png,
            Format::Jpeg,
            Format::WebP,
            Format::Avif,
            Format::Gif,
            Format::Bmp,
            Format::Qoi,
        ] {
            for (name, image) in [("rgb", rgb()), ("rgba", rgba.clone())] {
                let path = dir.path().join(format!("{name}.{}", format.extension()));
                let opts =
                    crate::EncodeOptions { quality: Some(60), ..crate::EncodeOptions::default() };
                crate::save_image(image, &path, format, &opts, crate::OverwritePolicy::Replace)
                    .unwrap();
                let bytes = std::fs::read(&path).unwrap();
                assert!(!carries_metadata(&bytes, format), "{format:?} {name}");
            }
        }
    }

    /// Guards: privacy: chunks the PNG encoders never write.
    #[test]
    fn png_unlisted_chunks_carry_metadata() {
        // The walker does not check CRCs: zeros stand in for them.
        let chunk = |kind: &[u8; 4], data: &[u8]| {
            [&(data.len() as u32).to_be_bytes()[..], kind, data, &[0; 4]].concat()
        };
        let png = encode(&rgb(), image::ImageFormat::Png);
        let before_iend = png.len() - 12;
        let with = |kind: &[u8; 4], data: &[u8]| splice(&png, before_iend, &chunk(kind, data));
        check(
            &[
                ("plain", png.clone(), false),
                ("eXIf", with(b"eXIf", b"II*\0"), true),
                ("tEXt", with(b"tEXt", b"Author\0me"), true),
                ("zTXt", with(b"zTXt", b"k\0\0x"), true),
                ("iTXt", with(b"iTXt", b"k\0\0\0\0\0x"), true),
                ("tIME", with(b"tIME", &[7, 234, 1, 1, 0, 0, 0]), true),
                ("caBX (C2PA)", with(b"caBX", b"jumb"), true),
                ("private prVt", with(b"prVt", b"secret"), true),
                ("data after IEND", [&png[..], b"secret"].concat(), true),
                ("iCCP of another profile", with(b"iCCP", b"icc\0\0x\x9c\x03\0"), true),
                (
                    "second gAMA",
                    splice(&with(b"gAMA", &[0; 4]), before_iend, &chunk(b"gAMA", &[0; 4])),
                    true,
                ),
            ],
            Format::Png,
        );
    }

    /// Guards: privacy: segments the JPEG encoders never write.
    #[test]
    fn jpeg_unlisted_segments_carry_metadata() {
        let jpeg = encode(&rgb(), image::ImageFormat::Jpeg);
        let after_soi = 2;
        let before_eoi = jpeg.len() - 2;
        let segment = |marker: u8, payload: &[u8]| {
            let len = (payload.len() as u16 + 2).to_be_bytes();
            [&[0xff, marker][..], &len, payload].concat()
        };
        let thumbnail = [&b"JFIF\0\x01\x02\0\0\x01\0\x01\x01\x01"[..], &[9, 9, 9]].concat();
        let jfif = b"JFIF\0\x01\x02\0\0\x01\0\x01\0\0";
        let other_icc = [&b"ICC_PROFILE\0\x01\x01"[..], b"a profile holding text"].concat();
        check(
            &[
                ("plain", jpeg.clone(), false),
                ("APP1 EXIF", splice(&jpeg, after_soi, &segment(0xe1, b"Exif\0\0II*\0")), true),
                ("APP0 not JFIF", splice(&jpeg, after_soi, &segment(0xe0, b"AVI1\0\0\0\0")), true),
                ("JFIF thumbnail", splice(&jpeg, after_soi, &segment(0xe0, &thumbnail)), true),
                (
                    "APP14 not Adobe",
                    splice(&jpeg, after_soi, &segment(0xee, b"Ducky\0\0\0\0\0\0\0")),
                    true,
                ),
                ("JPG0 segment", splice(&jpeg, after_soi, &segment(0xf0, b"secret")), true),
                ("COM after the scan", splice(&jpeg, before_eoi, &segment(0xfe, b"abc")), true),
                ("data after EOI", [&jpeg[..], b"secret"].concat(), true),
                ("cut inside the scan", jpeg[..jpeg.len() - 4].to_vec(), true),
                ("second JFIF", splice(&jpeg, after_soi, &segment(0xe0, jfif)), true),
                (
                    "ICC of another profile",
                    splice(&jpeg, after_soi, &segment(0xe2, &other_icc)),
                    true,
                ),
            ],
            Format::Jpeg,
        );
    }

    /// Guards: privacy: WebP chunks and AVIF boxes the encoders never write.
    #[test]
    fn webp_avif_extras_carry_metadata() {
        let webp = encode(&rgb(), image::ImageFormat::WebP);
        check(
            &[
                ("plain", webp.clone(), false),
                ("XMP", [&webp[..], b"XMP \x02\0\0\0<x"].concat(), true),
                ("C2PA", [&webp[..], b"C2PA\x02\0\0\0jb"].concat(), true),
                ("ICCP holding text", [&webp[..], b"ICCP\x04\0\0\0text"].concat(), true),
                ("truncated", webp[..webp.len() - 1].to_vec(), true),
            ],
            Format::WebP,
        );
        let opts = crate::EncodeOptions { quality: Some(60), ..crate::EncodeOptions::default() };
        let avif = |exif: Option<&[u8]>| {
            crate::optimize::encode(&rgb(), Format::Avif, &opts, None, exif).unwrap().unwrap()
        };
        let plain = avif(None);
        check(
            &[
                ("plain", plain.clone(), false),
                ("Exif item", avif(Some(b"II*\0\x08\0\0\0\0\0\0\0\0\0")), true),
                ("trailing uuid", [&plain[..], b"\0\0\0\x0cuuidabcd"].concat(), true),
                ("bytes after the boxes", [&plain[..], b"tail"].concat(), true),
            ],
            Format::Avif,
        );
    }

    fn bx(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
        [&((content.len() + 8) as u32).to_be_bytes()[..], kind, content].concat()
    }

    /// Guards: privacy: what an AVIF `meta` may hold besides the image.
    #[test]
    fn avif_meta_extras_carry_metadata() {
        let infe =
            |name: &[u8]| bx(b"infe", &[&[2, 0, 0, 0, 0, 1, 0, 0][..], b"av01", name].concat());
        let meta = |infe: Vec<u8>, property: Vec<u8>| {
            let hdlr = bx(b"hdlr", &[&[0; 8][..], b"pict", &[0; 13]].concat());
            let iinf = bx(b"iinf", &[&[0, 0, 0, 0, 0, 1][..], &infe].concat());
            let ipco = bx(b"ipco", &[bx(b"ispe", &[0; 12]), property].concat());
            let iprp = bx(b"iprp", &[ipco, bx(b"ipma", &[0; 8])].concat());
            [&[0, 0, 0, 0][..], &hdlr, &bx(b"pitm", &[0; 6]), &iinf, &iprp].concat()
        };
        let cases = [
            ("as encoded", meta(infe(b"\0"), bx(b"pixi", &[0; 8])), false),
            ("named item", meta(infe(b"holiday.jpg\0"), bx(b"pixi", &[0; 8])), true),
            ("udes property", meta(infe(b"\0"), bx(b"udes", b"\0\0\0\0en\0Name\0")), true),
            ("ICC colour", meta(infe(b"\0"), bx(b"colr", b"profsome profile")), true),
        ];
        for (name, content, expected) in cases {
            assert_eq!(avif_meta_carries_metadata(&content), expected, "{name}");
        }
    }

    /// Guards: privacy: GIF, BMP and QOI must end where their image ends.
    #[test]
    fn gif_bmp_qoi_need_clean_end() {
        let gif = encode(&rgb(), image::ImageFormat::Gif);
        let before_trailer = gif.len() - 1;
        let xmp = b"\x21\xff\x0bXMP DataXMP\x03abc\x00";
        check(
            &[
                ("plain", gif.clone(), false),
                ("comment", splice(&gif, before_trailer, b"\x21\xfe\x03abc\x00"), true),
                ("plain text", splice(&gif, before_trailer, b"\x21\x01\x03abc\x00"), true),
                ("XMP application", splice(&gif, before_trailer, xmp), true),
                ("bytes after the trailer", [&gif[..], b"secret"].concat(), true),
            ],
            Format::Gif,
        );
        let bmp = encode(&rgb(), image::ImageFormat::Bmp);
        check(
            &[
                ("plain", bmp.clone(), false),
                ("bytes after the pixels", [&bmp[..], b"secret"].concat(), true),
            ],
            Format::Bmp,
        );
        let qoi = encode(&rgb(), image::ImageFormat::Qoi);
        check(
            &[
                ("plain", qoi.clone(), false),
                ("bytes after the end marker", [&qoi[..], b"secret"].concat(), true),
                ("two images in a row", [&qoi[..], &qoi[..]].concat(), true),
            ],
            Format::Qoi,
        );
        for format in
            [Format::Tiff, Format::OpenExr, Format::Hdr, Format::Pnm, Format::Tga, Format::Ico]
        {
            assert!(carries_metadata(&[0; 16], format), "{format:?}");
        }
    }
}
