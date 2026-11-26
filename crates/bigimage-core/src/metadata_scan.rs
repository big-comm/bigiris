// SPDX-License-Identifier: GPL-3.0-or-later
//! Whether an encoded file holds anything beyond what the encoders here
//! write, so the never-larger shortcut of a stripping conversion may publish
//! it unchanged.
//!
//! Each walker accepts only the structures this crate's encoders produce
//! and a clean end; anything else (EXIF, XMP, text, C2PA, private chunks,
//! bytes after the image) counts as metadata.

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
        // The end marker: seven zero bytes and a one.
        Format::Qoi => bytes.len() < 22 || !bytes.ends_with(&[0, 0, 0, 0, 0, 0, 0, 1]),
        Format::Tiff | Format::OpenExr | Format::Hdr | Format::Pnm | Format::Tga | Format::Ico => {
            true
        }
    }
}

/// `false` only for a JPEG of JFIF (without thumbnail), Adobe and ICC
/// segments, tables, frames and scans, ending at EOI.
fn jpeg_carries_metadata(bytes: &[u8]) -> bool {
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
            0xe2 => payload.starts_with(b"ICC_PROFILE\0"),
            // DQT, DHT, DRI, DAC, SOS and the SOF markers.
            0xdb | 0xc4 | 0xdd | 0xcc | 0xda => true,
            0xc0..=0xcf => marker != 0xc8,
            _ => false,
        };
        if !written {
            return true;
        }
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

/// `false` only for a PNG of the critical, colour and transparency chunks,
/// whose last chunk is IEND.
fn png_carries_metadata(bytes: &[u8]) -> bool {
    let mut at = 8;
    while let Some(&[a, b, c, d, ref kind @ ..]) = bytes.get(at..at + 8) {
        let len = usize::try_from(u32::from_be_bytes([a, b, c, d])).unwrap_or(usize::MAX);
        let next = at.saturating_add(12).saturating_add(len);
        match kind {
            b"IEND" => return next != bytes.len(),
            b"IHDR" | b"PLTE" | b"tRNS" | b"iCCP" | b"sRGB" | b"gAMA" | b"cHRM" | b"IDAT" => {}
            _ => return true,
        }
        at = next;
    }
    true
}

/// `false` only for a WebP of image, alpha, extended-header and ICC chunks
/// ending exactly at the end of the file.
fn webp_carries_metadata(bytes: &[u8]) -> bool {
    let mut at = 12;
    while let Some(&[ref kind @ .., a, b, c, d]) = bytes.get(at..at + 8) {
        if !matches!(kind, b"VP8 " | b"VP8L" | b"VP8X" | b"ALPH" | b"ICCP") {
            return true;
        }
        let len = usize::try_from(u32::from_le_bytes([a, b, c, d])).unwrap_or(usize::MAX);
        at = at.saturating_add(8).saturating_add(len).saturating_add(len & 1);
    }
    at != bytes.len()
}

/// `false` only for an AVIF of `ftyp`, `meta` and `mdat` boxes ending at
/// the end of the file, whose `meta` declares no `Exif` or `mime` (XMP) item.
fn avif_carries_metadata(bytes: &[u8]) -> bool {
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
        let Some(content) = bytes.get(at..at.saturating_add(size)).filter(|_| size >= 8) else {
            return true;
        };
        match kind {
            b"meta" => {
                let declares = |item: &[u8]| content.windows(4).any(|w| w == item);
                if declares(b"Exif") || declares(b"mime") {
                    return true;
                }
            }
            b"ftyp" | b"mdat" => {}
            _ => return true,
        }
        at += size;
    }
    false
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
