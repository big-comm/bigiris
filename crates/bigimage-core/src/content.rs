// SPDX-License-Identifier: GPL-3.0-or-later
//! Prevent implicit frame/page loss during static transformations.
use crate::{BigImageError, Result};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Static transformations never silently discard animation or TIFF pages.
pub fn ensure_static(path: &Path) -> Result<()> {
    if let Some(backend) = crate::backend::configured() {
        return (backend.ensure_static)(path);
    }
    ensure_static_local(path)
}
/// Local inspection entry point for the confined worker.
pub fn ensure_static_local(path: &Path) -> Result<()> {
    // Validate size and regular-file status before any container parser.
    crate::pipeline::bounded_dimensions(path)
        .map_err(|source| BigImageError::Decode { path: path.into(), source })?;
    let reader = crate::pipeline::bounded_reader(path)
        .map_err(|source| BigImageError::Decode { path: path.into(), source })?;
    let format = reader.format();
    let mut file = reader.into_inner();
    let result: image::ImageResult<bool> = (|| {
        Ok(match format {
            Some(image::ImageFormat::Gif) => inspect_gif(&mut file)?,
            Some(image::ImageFormat::Png) => {
                image::codecs::png::PngDecoder::new(file)?.is_apng()?
            }
            Some(image::ImageFormat::WebP) => {
                image::codecs::webp::WebPDecoder::new(file)?.has_animation()
            }
            Some(image::ImageFormat::Avif) => is_avif_sequence(&mut file)?,
            Some(image::ImageFormat::Tiff) => {
                let mut header = [0; 16];
                file.read_exact(&mut header[..8])?;
                let little = &header[..2] == b"II";
                let word = |bytes: [u8; 2]| {
                    if little {
                        u16::from_le_bytes(bytes)
                    } else {
                        u16::from_be_bytes(bytes)
                    }
                };
                let long = |bytes: [u8; 4]| {
                    if little {
                        u32::from_le_bytes(bytes)
                    } else {
                        u32::from_be_bytes(bytes)
                    }
                };
                let wide = |bytes: [u8; 8]| {
                    if little {
                        u64::from_le_bytes(bytes)
                    } else {
                        u64::from_be_bytes(bytes)
                    }
                };
                let (first, big) = if word(header[2..4].try_into().unwrap()) == 43 {
                    file.read_exact(&mut header[8..16])?;
                    (wide(header[8..16].try_into().unwrap()), true)
                } else {
                    (u64::from(long(header[4..8].try_into().unwrap())), false)
                };
                file.seek(SeekFrom::Start(first))?;
                let mut bytes = [0; 8];
                let (count, entry, base) = if big {
                    file.read_exact(&mut bytes)?;
                    (wide(bytes), 20u64, 8u64)
                } else {
                    file.read_exact(&mut bytes[..2])?;
                    (u64::from(word(bytes[..2].try_into().unwrap())), 12, 2)
                };
                let next = count
                    .checked_mul(entry)
                    .and_then(|n| first.checked_add(base)?.checked_add(n))
                    .ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid TIFF IFD")
                    })?;
                file.seek(SeekFrom::Start(next))?;
                if big {
                    file.read_exact(&mut bytes)?;
                    wide(bytes) != 0
                } else {
                    file.read_exact(&mut bytes[..4])?;
                    long(bytes[..4].try_into().unwrap()) != 0
                }
            }
            _ => false,
        })
    })();
    if result.map_err(|source| BigImageError::Decode { path: path.into(), source })? {
        return Err(BigImageError::InvalidInput("imagem animada ou com múltiplas páginas: edição estática interrompida para evitar perda de conteúdo".into()));
    }
    Ok(())
}

/// AVIF image sequences (animated AVIF) carry the `avis` brand in `ftyp`
/// and their frames in a top-level `moov` track; the `image` decoder only
/// returns the primary still item, so either signal means frames would be
/// lost. Walks the top-level boxes only (bounded count, no allocation).
pub(crate) fn is_avif_sequence(reader: &mut (impl Read + Seek)) -> std::io::Result<bool> {
    let end = reader.seek(SeekFrom::End(0))?;
    let mut position = 0u64;
    for _ in 0..1024 {
        if end.saturating_sub(position) < 8 {
            break;
        }
        reader.seek(SeekFrom::Start(position))?;
        let mut header = [0; 8];
        reader.read_exact(&mut header)?;
        let (size, header_size) = match u32::from_be_bytes(header[..4].try_into().unwrap()) {
            0 => (end - position, 8),
            1 => {
                let mut large = [0; 8];
                reader.read_exact(&mut large)?;
                (u64::from_be_bytes(large), 16)
            }
            size => (u64::from(size), 8),
        };
        if size < header_size {
            break;
        }
        match &header[4..8] {
            b"moov" => return Ok(true),
            b"ftyp" => {
                // major_brand(4) minor_version(4) compatible_brands(4 each)
                let len = usize::try_from((size - header_size).min(4096)).unwrap_or(0);
                let mut body = vec![0; len];
                reader.read_exact(&mut body)?;
                let major = body.first_chunk::<4>().into_iter();
                let compatible = body.get(8..).unwrap_or_default().as_chunks::<4>().0.iter();
                if major.chain(compatible).any(|brand| brand == b"avis") {
                    return Ok(true);
                }
            }
            _ => {}
        }
        position = match position.checked_add(size) {
            Some(next) => next,
            None => break,
        };
    }
    Ok(false)
}

/// Count frames without allocating pixels, and reject frames outside the canvas.
pub(crate) fn inspect_gif(reader: &mut (impl Read + Seek)) -> image::ImageResult<bool> {
    fn malformed() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "GIF inválido: bloco ou quadro fora da tela",
        )
    }
    fn byte(reader: &mut impl Read) -> std::io::Result<u8> {
        let mut value = [0];
        reader.read_exact(&mut value)?;
        Ok(value[0])
    }
    fn skip(reader: &mut (impl Read + Seek), count: u64, end: u64) -> std::io::Result<()> {
        let next = reader
            .stream_position()?
            .checked_add(count)
            .filter(|next| *next <= end)
            .ok_or_else(malformed)?;
        reader.seek(SeekFrom::Start(next))?;
        Ok(())
    }
    fn blocks(reader: &mut (impl Read + Seek), end: u64) -> std::io::Result<()> {
        loop {
            let count = byte(reader)?;
            if count == 0 {
                return Ok(());
            }
            skip(reader, u64::from(count), end)?;
        }
    }
    let end = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(0))?;
    let mut header = [0; 13];
    reader.read_exact(&mut header)?;
    if &header[..6] != b"GIF87a" && &header[..6] != b"GIF89a" {
        return Err(malformed().into());
    }
    let width = u16::from_le_bytes(header[6..8].try_into().unwrap());
    let height = u16::from_le_bytes(header[8..10].try_into().unwrap());
    if width == 0 || height == 0 {
        return Err(malformed().into());
    }
    if header[10] & 0x80 != 0 {
        skip(reader, 3 * (1u64 << (1 + (header[10] & 7))), end)?;
    }
    let mut frames = 0;
    loop {
        let introducer = match byte(reader) {
            Ok(value) => value,
            // Many encoders omit the 0x3B trailer; once a complete image block
            // has been read, end of file is simply the end of the stream.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && frames > 0 => {
                return Ok(false);
            }
            Err(e) => return Err(e.into()),
        };
        match introducer {
            0x3b => return Ok(false),
            0x21 => {
                byte(reader)?;
                blocks(reader, end)?;
            }
            0x2c => {
                let mut descriptor = [0; 9];
                reader.read_exact(&mut descriptor)?;
                let word = |offset| {
                    u16::from_le_bytes(descriptor[offset..offset + 2].try_into().unwrap()) as u32
                };
                if word(4) == 0
                    || word(6) == 0
                    || word(0) + word(4) > u32::from(width)
                    || word(2) + word(6) > u32::from(height)
                {
                    return Err(malformed().into());
                }
                frames += 1;
                if frames > 1 {
                    return Ok(true);
                }
                if descriptor[8] & 0x80 != 0 {
                    skip(reader, 3 * (1u64 << (1 + (descriptor[8] & 7))), end)?;
                }
                byte(reader)?;
                blocks(reader, end)?;
            }
            _ => return Err(malformed().into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gif_bytes(frames: usize) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut out);
            for _ in 0..frames {
                let frame = image::Frame::new(image::RgbaImage::from_pixel(
                    4,
                    3,
                    image::Rgba([200, 10, 10, 255]),
                ));
                encoder.encode_frame(frame).unwrap();
            }
        }
        out
    }

    fn ftyp(major: &[u8; 4], compatible: &[&[u8; 4]]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(16 + 4 * compatible.len() as u32).to_be_bytes());
        out.extend_from_slice(b"ftyp");
        out.extend_from_slice(major);
        out.extend_from_slice(&[0; 4]);
        compatible.iter().for_each(|brand| out.extend_from_slice(*brand));
        out
    }

    #[test]
    fn avif_sequences_are_detected_by_brand_or_moov() {
        let detect = |bytes: &[u8]| is_avif_sequence(&mut std::io::Cursor::new(bytes)).unwrap();
        assert!(!detect(&ftyp(b"avif", &[b"avif", b"mif1", b"miaf"])));
        assert!(detect(&ftyp(b"avis", &[b"avif", b"msf1"])));
        assert!(detect(&ftyp(b"avif", &[b"mif1", b"avis"])));
        let mut moov = ftyp(b"avif", &[b"mif1"]);
        moov.extend_from_slice(&[0, 0, 0, 8, b'f', b'r', b'e', b'e']);
        moov.extend_from_slice(&[0, 0, 0, 8, b'm', b'o', b'o', b'v']);
        assert!(detect(&moov));
        // Truncated or nonsensical boxes end the walk instead of looping.
        assert!(!detect(&[0, 0, 0, 0]));
        assert!(!detect(&[0, 0, 0, 4, b'f', b'r', b'e', b'e', 0, 0]));

        // A real still AVIF is static; branding it as a sequence is refused.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("still.avif");
        crate::save_image(
            image::DynamicImage::ImageRgb8(image::RgbImage::new(4, 4)),
            &path,
            crate::Format::Avif,
            &crate::EncodeOptions { avif_speed: 10, ..crate::EncodeOptions::default() },
            crate::OverwritePolicy::Replace,
        )
        .unwrap();
        ensure_static_local(&path).unwrap();
        // A compatible `avis` brand without a track is refused too (the
        // AVIF parser itself rejects it before our walk runs).
        let mut bytes = std::fs::read(&path).unwrap();
        let at = bytes.windows(4).position(|w| w == b"mif1").unwrap();
        bytes[at..at + 4].copy_from_slice(b"avis");
        std::fs::write(&path, &bytes).unwrap();
        assert!(ensure_static_local(&path).is_err());
    }

    /// Real animated AVIF from libavif, when `avifenc` is available.
    #[test]
    fn avifenc_image_sequence_is_not_static() {
        let dir = tempfile::tempdir().unwrap();
        let mut frames = Vec::new();
        for (i, color) in [[255, 0, 0], [0, 0, 255]].into_iter().enumerate() {
            let frame = dir.path().join(format!("f{i}.png"));
            image::RgbImage::from_pixel(8, 8, image::Rgb(color)).save(&frame).unwrap();
            frames.push(frame);
        }
        let output = dir.path().join("anim.avif");
        let status = std::process::Command::new("avifenc")
            .args(["-s", "10", "-q", "50"])
            .args(&frames)
            .arg(&output)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if !status.is_ok_and(|status| status.success()) {
            eprintln!("avifenc unavailable; skipping real image-sequence check");
            return;
        }
        assert!(matches!(ensure_static_local(&output), Err(BigImageError::InvalidInput(_))));
    }

    #[test]
    fn gif_without_trailer_is_a_single_complete_image() {
        let mut bytes = gif_bytes(1);
        assert_eq!(bytes.pop(), Some(0x3b));
        assert!(!inspect_gif(&mut std::io::Cursor::new(&bytes)).unwrap());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-trailer.gif");
        std::fs::write(&path, &bytes).unwrap();
        ensure_static_local(&path).unwrap();
        assert_eq!(crate::open_image(&path).unwrap().width(), 4);
    }

    #[test]
    fn gif_truncated_before_any_image_is_still_rejected() {
        let bytes = gif_bytes(1);
        // Header + logical screen descriptor only.
        assert!(inspect_gif(&mut std::io::Cursor::new(&bytes[..13])).is_err());
        let mut animated = gif_bytes(2);
        animated.pop();
        assert!(inspect_gif(&mut std::io::Cursor::new(&animated)).unwrap());
    }
}
