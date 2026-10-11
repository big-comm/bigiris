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
