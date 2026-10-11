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
        match byte(reader)? {
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
