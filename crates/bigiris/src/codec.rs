// SPDX-License-Identifier: GPL-3.0-or-later
//! Confine untrusted codecs to a short-lived, resource-limited process.
//! The parent accepts only a validated, bounded raw-pixel protocol.
use bigimage_core::{backend, BigImageError, Format, Metadata, PreviewSession};
use image::{ColorType, DynamicImage, GenericImageView, ImageBuffer, ImageError, ImageResult};
use std::cell::RefCell;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

const MAGIC: &[u8; 8] = b"BIGIRIS2";
const HEADER: usize = 36;
const MAX_BYTES: u64 = 512 * 1024 * 1024;
const MAX_INPUT: u64 = 1024 * 1024 * 1024;
const MAX_METADATA: u64 = 1024 * 1024;
thread_local! { static CANCEL: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) }; }

pub fn configure() {
    assert!(backend::configure(backend::Backend {
        decode,
        dimensions,
        preview,
        metadata,
        ensure_static
    }));
}

/// Exercise the real process boundary and raw protocol.
pub fn self_test() -> color_eyre::Result<()> {
    let input = tempfile::Builder::new().suffix(".png").tempfile()?;
    image::RgbImage::from_pixel(2, 3, image::Rgb([30, 70, 120])).save(input.path())?;
    let image = decode(input.path())?.0;
    if image.dimensions() != (2, 3) {
        return Err(color_eyre::eyre::eyre!("invalid sandbox decode"));
    }
    let output = execute(input.path(), "attest")?;
    if output.metadata()?.len() != 0 {
        return Err(color_eyre::eyre::eyre!("invalid sandbox attestation"));
    }
    Ok(())
}

/// Scoped cancellation also restores the context during panic unwinding.
#[cfg(any(feature = "gui", test))]
pub fn with_cancel<T>(cancel: Arc<AtomicBool>, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<Arc<AtomicBool>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CANCEL.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(CANCEL.with(|slot| slot.replace(Some(cancel))));
    work()
}
fn invalid(message: impl Into<String>) -> ImageError {
    ImageError::IoError(std::io::Error::new(std::io::ErrorKind::InvalidData, message.into()))
}
fn cancelled() -> bool {
    CANCEL.with(|slot| slot.borrow().as_ref().is_some_and(|flag| flag.load(Ordering::Acquire)))
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn execute(path: &Path, mode: &str) -> ImageResult<std::fs::File> {
    if cancelled() {
        return Err(invalid("operação cancelada"));
    }
    let input = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(path)?;
    let info = input.metadata()?;
    if !info.is_file() || info.len() > MAX_INPUT {
        return Err(invalid("entrada não regular ou acima de 1 GiB"));
    }
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|e| Format::parse(e).ok())
        .map_or("img", |format| format.extension());
    let sandbox_input = format!("/input.{extension}");
    let executable = std::env::current_exe()?;
    let output = tempfile::tempfile()?;
    let diagnostic = tempfile::tempfile()?;
    let mut command = Command::new("/usr/bin/prlimit");
    command
        .env_clear()
        .args(["--as=2147483648", "--cpu=30", "--nofile=64", "--core=0"])
        .arg(format!("--fsize={}", MAX_BYTES + HEADER as u64))
        .args([
            "--",
            "/usr/bin/bwrap",
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/usr",
            "/usr",
            "--symlink",
            "usr/lib",
            "/lib",
            "--symlink",
            "usr/lib64",
            "/lib64",
            "--symlink",
            "usr/bin",
            "/bin",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--remount-ro",
            "/dev",
            "--size",
            "67108864",
            "--tmpfs",
            "/tmp",
            "--dir",
            "/app",
            "--ro-bind",
        ])
        .arg(executable)
        .arg("/app/bigiris")
        .args(["--ro-bind", "/proc/self/fd/0"])
        .arg(&sandbox_input)
        .args([
            "--chdir",
            "/",
            "--setenv",
            "LC_ALL",
            "C.UTF-8",
            "--setenv",
            "RAYON_NUM_THREADS",
            "2",
            "/app/bigiris",
            "--codec-worker",
        ])
        .arg(mode)
        .arg(&sandbox_input)
        .stdin(Stdio::from(input))
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(diagnostic.try_clone()?));
    let child = command.spawn().map_err(|error| {
        invalid(format!("sandbox indisponível: instale bubblewrap e util-linux; {error}"))
    })?;
    let mut child = ChildGuard(child);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if cancelled() {
            return Err(invalid("operação cancelada"));
        }
        if Instant::now() >= deadline {
            return Err(invalid("codec excedeu o limite de 60 segundos"));
        }
        if let Some(status) = child.0.try_wait()? {
            if !status.success() {
                let mut details = String::new();
                let mut diagnostic = diagnostic;
                diagnostic.seek(SeekFrom::Start(0))?;
                diagnostic.take(4096).read_to_string(&mut details).ok();
                return Err(invalid(format!(
                    "codec isolado recusou a imagem ({status}): {}",
                    details.trim()
                )));
            }
            let mut output = output;
            output.seek(SeekFrom::Start(0))?;
            return Ok(output);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct Pixels {
    image: DynamicImage,
    format: Option<Format>,
    natural: (u32, u32),
    gps: bool,
}
fn receive(mut output: std::fs::File) -> ImageResult<Pixels> {
    let mut header = [0u8; HEADER];
    output.read_exact(&mut header)?;
    if &header[..8] != MAGIC || header[10] > 1 || header[11] != 0 {
        return Err(invalid("protocolo de codec inválido"));
    }
    let word = |offset| u32::from_le_bytes(header[offset..offset + 4].try_into().unwrap());
    let (w, h) = (word(12), word(16));
    let natural = (word(20), word(24));
    let length = u64::from_le_bytes(header[28..36].try_into().unwrap());
    let color = match header[8] {
        1 => ColorType::L8,
        2 => ColorType::La8,
        3 => ColorType::Rgb8,
        4 => ColorType::Rgba8,
        5 => ColorType::L16,
        6 => ColorType::La16,
        7 => ColorType::Rgb16,
        8 => ColorType::Rgba16,
        9 => ColorType::Rgb32F,
        10 => ColorType::Rgba32F,
        _ => return Err(invalid("tipo de pixels inválido")),
    };
    if w == 0
        || h == 0
        || natural.0 == 0
        || natural.1 == 0
        || u64::from(natural.0) * u64::from(natural.1) > 256 * 1024 * 1024
        || u64::from(w) * u64::from(h) > 256 * 1024 * 1024
        || length > MAX_BYTES
        || length != u64::from(w) * u64::from(h) * u64::from(color.bytes_per_pixel())
        || output.metadata()?.len() != HEADER as u64 + length
    {
        return Err(invalid("dimensões ou tamanho do protocolo inválidos"));
    }
    let format = if header[9] == 255 {
        None
    } else {
        Some(*Format::all().get(header[9] as usize).ok_or_else(|| invalid("formato inválido"))?)
    };
    let mut raw = vec![0; length as usize];
    output.read_exact(&mut raw)?;
    let shorts = || {
        raw.as_chunks::<2>().0.iter().map(|bytes| u16::from_ne_bytes(*bytes)).collect::<Vec<_>>()
    };
    let floats = || {
        raw.as_chunks::<4>().0.iter().map(|bytes| f32::from_ne_bytes(*bytes)).collect::<Vec<_>>()
    };
    macro_rules! buffer {
        ($data:expr, $variant:ident) => {
            DynamicImage::$variant(
                ImageBuffer::from_raw(w, h, $data).ok_or_else(|| invalid("buffer inválido"))?,
            )
        };
    }
    let image = match color {
        ColorType::L8 => buffer!(raw, ImageLuma8),
        ColorType::La8 => buffer!(raw, ImageLumaA8),
        ColorType::Rgb8 => buffer!(raw, ImageRgb8),
        ColorType::Rgba8 => buffer!(raw, ImageRgba8),
        ColorType::L16 => buffer!(shorts(), ImageLuma16),
        ColorType::La16 => buffer!(shorts(), ImageLumaA16),
        ColorType::Rgb16 => buffer!(shorts(), ImageRgb16),
        ColorType::Rgba16 => buffer!(shorts(), ImageRgba16),
        ColorType::Rgb32F => buffer!(floats(), ImageRgb32F),
        ColorType::Rgba32F => buffer!(floats(), ImageRgba32F),
        _ => return Err(invalid("tipo de pixels inválido")),
    };
    Ok(Pixels { image, format, natural, gps: header[10] == 1 })
}
fn decode(path: &Path) -> ImageResult<backend::Decoded> {
    let pixels = receive(execute(path, "pixels")?)?;
    Ok((pixels.image, pixels.format))
}
fn dimensions(path: &Path) -> ImageResult<(u32, u32)> {
    let mut output = execute(path, "dimensions")?;
    if output.metadata()?.len() != 8 {
        return Err(invalid("dimensões inválidas"));
    }
    let mut bytes = [0; 8];
    output.read_exact(&mut bytes)?;
    let size = (
        u32::from_le_bytes(bytes[..4].try_into().unwrap()),
        u32::from_le_bytes(bytes[4..].try_into().unwrap()),
    );
    if size.0 == 0 || size.1 == 0 || u64::from(size.0) * u64::from(size.1) > 256 * 1024 * 1024 {
        return Err(invalid("dimensões inválidas"));
    }
    Ok(size)
}
fn preview(path: &Path) -> bigimage_core::Result<PreviewSession> {
    let pixels = receive(
        execute(path, "preview")
            .map_err(|source| BigImageError::Decode { path: path.into(), source })?,
    )
    .map_err(|source| BigImageError::Decode { path: path.into(), source })?;
    PreviewSession::from_cached(
        path.into(),
        pixels.natural,
        pixels
            .format
            .ok_or_else(|| BigImageError::UnsupportedFormat("preview sem formato".into()))?,
        pixels.image,
        pixels.gps,
    )
}
fn ensure_static(path: &Path) -> bigimage_core::Result<()> {
    execute(path, "static")
        .map(|_| ())
        .map_err(|source| BigImageError::Decode { path: path.into(), source })
}
fn metadata(path: &Path) -> bigimage_core::Result<Metadata> {
    let output = execute(path, "metadata")
        .map_err(|source| BigImageError::Decode { path: path.into(), source })?;
    if output.metadata().map_err(BigImageError::Io)?.len() > MAX_METADATA {
        return Err(BigImageError::Other("metadados excessivos".into()));
    }
    serde_json::from_reader(output).map_err(|error| BigImageError::Other(error.to_string()))
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Information {
    pub dimensions: Option<(u32, u32)>,
    pub color: Option<u8>,
    pub format: Option<String>,
    pub metadata: Option<Metadata>,
}
fn information_local(path: &Path) -> Information {
    let dimensions = backend::dimensions_in_process(path).ok();
    let reader = image::ImageReader::open(path).ok().and_then(|r| r.with_guessed_format().ok());
    let format = reader.as_ref().and_then(|r| r.format()).map(|f| format!("{f:?}"));
    let color =
        reader.and_then(|r| r.into_decoder().ok()).map(
            |dec| match image::ImageDecoder::color_type(&dec) {
                ColorType::L8 => 1,
                ColorType::La8 => 2,
                ColorType::Rgb8 => 3,
                ColorType::Rgba8 => 4,
                ColorType::L16 => 5,
                ColorType::La16 => 6,
                ColorType::Rgb16 => 7,
                ColorType::Rgba16 => 8,
                ColorType::Rgb32F => 9,
                ColorType::Rgba32F => 10,
                _ => 0,
            },
        );
    let metadata = bigimage_core::metadata::read(path).ok().map(trim_metadata);
    Information { dimensions, color, format, metadata }
}
fn trim_metadata(mut meta: Metadata) -> Metadata {
    meta.tags.truncate(128);
    for (_, value) in &mut meta.tags {
        *value = value.chars().take(4096).collect();
    }
    meta
}
#[cfg(feature = "gui")]
pub fn information(path: &Path) -> ImageResult<Information> {
    // Unit tests do not execute main/configure; still run their I/O off GTK.
    #[cfg(test)]
    return Ok(information_local(path));
    #[cfg(not(test))]
    {
        let output = execute(path, "information")?;
        if output.metadata()?.len() > MAX_METADATA {
            return Err(invalid("metadados excessivos"));
        }
        serde_json::from_reader(output).map_err(|e| invalid(e.to_string()))
    }
}

fn send(
    image: &DynamicImage,
    format: Option<Format>,
    natural: (u32, u32),
    gps: bool,
) -> ImageResult<()> {
    let color = match image.color() {
        ColorType::L8 => 1,
        ColorType::La8 => 2,
        ColorType::Rgb8 => 3,
        ColorType::Rgba8 => 4,
        ColorType::L16 => 5,
        ColorType::La16 => 6,
        ColorType::Rgb16 => 7,
        ColorType::Rgba16 => 8,
        ColorType::Rgb32F => 9,
        ColorType::Rgba32F => 10,
        _ => return Err(invalid("tipo de pixels inválido")),
    };
    let mut header = Vec::with_capacity(HEADER);
    header.extend(MAGIC);
    header.extend([
        color,
        format.map_or(255, |f| Format::all().iter().position(|x| *x == f).unwrap() as u8),
        u8::from(gps),
        0,
    ]);
    for value in [image.width(), image.height(), natural.0, natural.1] {
        header.extend(value.to_le_bytes());
    }
    header.extend((image.as_bytes().len() as u64).to_le_bytes());
    let mut output = std::io::stdout().lock();
    output.write_all(&header)?;
    output.write_all(image.as_bytes())?;
    output.flush()?;
    Ok(())
}
pub fn worker() -> color_eyre::Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 4 {
        return Err(color_eyre::eyre::eyre!("invalid worker arguments"));
    }
    let mode = args[2].to_str().unwrap_or("");
    let path = PathBuf::from(&args[3]);
    match mode {
        "pixels" => {
            let (image, format) = backend::decode_in_process(&path)?;
            send(&image, format, image.dimensions(), false)?;
        }
        "preview" => {
            let session = PreviewSession::open(&path)?;
            send(
                session.thumbnail(),
                Some(session.source_format()),
                session.natural_size(),
                session.source_has_gps(),
            )?;
        }
        "dimensions" => {
            let (w, h) = backend::dimensions_in_process(&path)?;
            let mut out = std::io::stdout().lock();
            out.write_all(&w.to_le_bytes())?;
            out.write_all(&h.to_le_bytes())?;
        }
        "metadata" => {
            let meta = trim_metadata(bigimage_core::metadata::read(&path)?);
            serde_json::to_writer(std::io::stdout().lock(), &meta)?;
        }
        "attest" => {
            for location in ["/home", "/root", "/run", "/etc"] {
                if std::path::Path::new(location).exists() {
                    return Err(color_eyre::eyre::eyre!("host directory visible: {location}"));
                }
            }
            let network = std::fs::read_to_string("/proc/net/dev")?;
            if network.lines().skip(2).any(|line| !line.trim_start().starts_with("lo:")) {
                return Err(color_eyre::eyre::eyre!("host network visible"));
            }
        }
        "static" => {
            bigimage_core::content::ensure_static_local(&path)?;
        }
        "information" => {
            serde_json::to_writer(std::io::stdout().lock(), &information_local(&path))?;
        }
        _ => return Err(color_eyre::eyre::eyre!("invalid worker mode")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_context_restores_after_panic() {
        let flag = Arc::new(AtomicBool::new(true));
        assert!(!cancelled());
        let _ = std::panic::catch_unwind(|| {
            with_cancel(flag, || {
                assert!(cancelled());
                panic!("test")
            })
        });
        assert!(!cancelled());
    }
    #[test]
    fn invalid_protocol_cannot_allocate_claimed_pixels() {
        let mut file = tempfile::tempfile().unwrap();
        let mut header = [0u8; HEADER];
        header[..8].copy_from_slice(MAGIC);
        header[8] = 4;
        header[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        header[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        file.write_all(&header).unwrap();
        file.rewind().unwrap();
        assert!(receive(file).is_err());
    }
}
