// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared primitives used by every single-file transform pipeline
//! (convert / resize / rotate / flip / crop). Extracted here so the op
//! modules stay focused on the transform itself and pick up the same
//! decode, naming, and fail-safe encoding semantics for free.

use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageReader};

use crate::convert::OverwritePolicy;
use crate::encode::EncodeOptions;
use crate::{BigImageError, Format, Result};

/// Resource bounds shared by the viewer and all transforms. Decoder scratch
/// allocations are best-effort; the decoded buffer is checked explicitly.
const MAX_INPUT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_PIXELS: u64 = 256 * 1024 * 1024;
pub(crate) const MAX_BUFFER_BYTES: u64 = 512 * 1024 * 1024;

fn limit_error() -> image::ImageError {
    image::ImageError::Limits(image::error::LimitError::from_kind(
        image::error::LimitErrorKind::InsufficientMemory,
    ))
}

/// Reject oversized transformed buffers before allocating them.
pub(crate) fn check_dimensions(w: u32, h: u32, bytes_per_pixel: u8) -> Result<()> {
    let pixels = u64::from(w) * u64::from(h);
    if w == 0
        || h == 0
        || pixels > MAX_PIXELS
        || pixels.saturating_mul(u64::from(bytes_per_pixel)) > MAX_BUFFER_BYTES
    {
        return Err(BigImageError::InvalidInput(format!(
            "dimensões {w}×{h} excedem os limites de processamento"
        )));
    }
    Ok(())
}

pub(crate) fn bounded_reader(
    input: &Path,
) -> image::ImageResult<ImageReader<std::io::BufReader<std::fs::File>>> {
    use std::io::BufReader;
    // Refuse special files before open, so accidental FIFO input does not block.
    let meta = std::fs::metadata(input)?;
    if !meta.is_file() || meta.len() > MAX_INPUT_BYTES {
        return Err(limit_error());
    }
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(input)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_INPUT_BYTES {
        return Err(limit_error());
    }
    let mut reader = ImageReader::new(BufReader::new(file)).with_guessed_format()?;
    if reader.format().is_none() {
        if let Some(format) = input.extension().and_then(image::ImageFormat::from_extension) {
            reader.set_format(format);
        }
    }
    if reader.format() == Some(image::ImageFormat::Avif) {
        use std::io::{Seek, SeekFrom};
        let mut data = reader.into_inner();
        crate::container_bounds::inspect(&mut data)?;
        data.seek(SeekFrom::Start(0))?;
        reader = ImageReader::with_format(data, image::ImageFormat::Avif);
    }
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_BUFFER_BYTES);
    reader.limits(limits);
    Ok(reader)
}

pub(crate) fn bounded_dimensions(input: &Path) -> image::ImageResult<(u32, u32)> {
    use image::ImageDecoder;
    let decoder = bounded_reader(input)?.into_decoder()?;
    let (w, h) = decoder.dimensions();
    if w == 0
        || h == 0
        || u64::from(w) * u64::from(h) > MAX_PIXELS
        || decoder.total_bytes() > MAX_BUFFER_BYTES
    {
        return Err(limit_error());
    }
    Ok((w, h))
}

/// Decode using the same open descriptor for header and pixel data.
pub(crate) fn decode_image(input: &Path) -> image::ImageResult<(DynamicImage, Option<Format>)> {
    if let Some(backend) = crate::backend::configured() {
        return (backend.decode)(input);
    }
    decode_local(input)
}

pub(crate) fn decode_local(input: &Path) -> image::ImageResult<(DynamicImage, Option<Format>)> {
    decode_local_with(input, true).map(|(image, format, _)| (image, format))
}

/// Decode upright pixels. With `normalize`, ICC-tagged pixels are converted
/// to sRGB and no metadata is returned; without it, pixels stay in the
/// source colour space and the raw EXIF/ICC blobs are returned so an
/// in-place save can re-attach them.
pub(crate) fn decode_local_with(
    input: &Path,
    normalize: bool,
) -> image::ImageResult<(DynamicImage, Option<Format>, crate::metadata::SourceMetadata)> {
    use image::ImageDecoder;
    let mut reader = bounded_reader(input)?;
    let format = reader.format().and_then(Format::from_image_format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_BUFFER_BYTES);
    if format == Some(Format::Gif) {
        use std::io::{Seek, SeekFrom};
        let mut data = reader.into_inner();
        crate::content::inspect_gif(&mut data)?;
        data.seek(SeekFrom::Start(0))?;
        reader = ImageReader::with_format(data, image::ImageFormat::Gif);
        reader.limits(limits.clone());
    }
    let mut avif_transform = crate::container_bounds::ItemTransform::default();
    if format == Some(Format::Avif) {
        use std::io::{Seek, SeekFrom};
        let mut data = reader.into_inner();
        avif_transform = crate::container_bounds::primary_item_transform(&mut data)?;
        data.seek(SeekFrom::Start(0))?;
        reader = ImageReader::with_format(data, image::ImageFormat::Avif);
        reader.limits(limits.clone());
    }
    let mut decoder = reader.into_decoder()?;
    let (w, h) = decoder.dimensions();
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(limit_error());
    }
    limits.reserve(decoder.total_bytes())?;
    decoder.set_limits(limits)?;
    let exif = if normalize { None } else { decoder.exif_metadata().ok().flatten() };
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let icc = decoder.icc_profile()?.filter(|profile| !profile.is_empty());
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    image = avif_transform.apply(image);
    if normalize {
        if let Some(icc) = icc {
            crate::color::normalize_srgb(&mut image, &icc)?;
        }
        return Ok((image, format, crate::metadata::SourceMetadata::default()));
    }
    let exif = exif.filter(|exif| !exif.is_empty());
    Ok((image, format, crate::metadata::SourceMetadata { exif, icc }))
}

/// Open and decode an upright image with shared resource bounds.
pub(crate) fn decode_with_source_format(input: &Path) -> Result<(DynamicImage, Format)> {
    crate::content::ensure_static(input)?;
    let (img, format) = decode_image(input)
        .map_err(|source| BigImageError::Decode { path: input.to_path_buf(), source })?;
    let format = format.ok_or_else(|| {
        BigImageError::UnsupportedFormat(format!(
            "entrada sem formato reconhecido: {}",
            input.display()
        ))
    })?;
    Ok((img, format))
}

/// Source format from magic bytes (extension as fallback) without running
/// any decoder — the same answer `decode_with_source_format` reports.
pub(crate) fn sniff_format(input: &Path) -> Option<Format> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(input)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut head = Vec::with_capacity(64);
    file.take(64).read_to_end(&mut head).ok()?;
    image::guess_format(&head)
        .ok()
        .or_else(|| input.extension().and_then(image::ImageFormat::from_extension))
        .and_then(Format::from_image_format)
}

/// `Skip` short-circuit before decoding: when the destination name can be
/// computed from the path and header alone and already exists, return the
/// skip outcome without paying for a full decode.
pub(crate) fn skip_before_decode(
    input: &Path,
    suffix: &str,
    target: Option<Format>,
    policy: OverwritePolicy,
) -> Option<crate::ConvertOutcome> {
    if !matches!(policy, OverwritePolicy::Skip) {
        return None;
    }
    let target = target.or_else(|| sniff_format(input))?;
    let output = resolve_output(input, Some(suffix), target, policy).ok()?;
    output.symlink_metadata().is_ok().then(|| {
        tracing::debug!(?output, "skipping existing output before decoding");
        crate::ConvertOutcome::Skipped { output }
    })
}

/// Compute the output path.
///
/// * `suffix`: optional tag appended to the stem (e.g. `"_1080"`, `"_rot90"`).
///   `None` means "same stem" (convert's behaviour).
///
/// The overwrite policy is enforced atomically at publication. Callers may
/// still short-circuit `Skip` before decoding an existing destination.
pub(crate) fn resolve_output(
    input: &Path,
    suffix: Option<&str>,
    target: Format,
    policy: OverwritePolicy,
) -> Result<PathBuf> {
    resolve_output_to(input, None, suffix, target, policy)
}

/// Like [`resolve_output`] but takes an explicit `output_dir`. If `None`,
/// grava ao lado do arquivo original (comportamento histórico). Caso
/// contrário usa o diretório fornecido mantendo o stem da fonte.
pub(crate) fn resolve_output_to(
    input: &Path,
    output_dir: Option<&Path>,
    suffix: Option<&str>,
    target: Format,
    policy: OverwritePolicy,
) -> Result<PathBuf> {
    let stem = input
        .file_stem()
        .ok_or_else(|| BigImageError::InvalidInput(format!("no file stem in {input:?}")))?;
    let parent = output_dir.or_else(|| input.parent()).unwrap_or_else(|| Path::new("."));
    let ext = target.extension();
    let suffix = suffix.unwrap_or("");

    let naive = {
        let mut name = std::ffi::OsString::from(stem);
        name.push(suffix);
        name.push(".");
        name.push(ext);
        parent.join(name)
    };

    let _ = policy;
    Ok(naive)
}

/// A unique sibling temporary file, cleaned up on every error path.
pub(crate) fn output_temp(output: &Path) -> Result<tempfile::NamedTempFile> {
    let parent = output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(BigImageError::Io)?;
    tempfile::Builder::new()
        .prefix(".bigiris-")
        .suffix(".part")
        .tempfile_in(parent)
        .map_err(BigImageError::Io)
}

/// Mode for newly created outputs: `0o666 & !umask`, like `open(2)` would.
///
/// The umask is read from `/proc/self/status` instead of `umask(2)`, which
/// can only be queried by changing it (a process-wide race with other
/// threads creating files). Falls back to the common `0o022`.
pub(crate) fn new_file_mode() -> u32 {
    let umask = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("Umask:").and_then(|v| u32::from_str_radix(v.trim(), 8).ok())
            })
        })
        .unwrap_or(0o022);
    0o666 & !umask
}

/// Make the rename itself durable. Best-effort: the file is already in
/// place, so a failure here is logged rather than reported as a failed write.
fn sync_parent(output: &Path) {
    let parent = output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    if let Err(error) = std::fs::File::open(parent).and_then(|dir| dir.sync_all()) {
        tracing::warn!(?parent, %error, "fsync do diretório de saída falhou");
    }
}

/// Publish a complete file atomically. Skip/Increment never overwrite an
/// existing destination, even when other threads or processes race us.
pub(crate) fn publish_temp(
    temp: tempfile::NamedTempFile,
    output: PathBuf,
    policy: OverwritePolicy,
) -> Result<crate::ConvertOutcome> {
    use std::os::unix::fs::PermissionsExt;
    // tempfile creates 0600 files; a published image must get the same
    // permissions as any other new file (0666 & !umask), unless it replaces
    // an existing regular file whose permissions we keep.
    let existing = matches!(policy, OverwritePolicy::Replace)
        .then(|| std::fs::symlink_metadata(&output).ok())
        .flatten()
        .filter(std::fs::Metadata::is_file);
    let permissions = match existing {
        Some(meta) => meta.permissions(),
        None => std::fs::Permissions::from_mode(new_file_mode()),
    };
    temp.as_file().set_permissions(permissions).map_err(BigImageError::Io)?;
    temp.as_file().sync_all().map_err(BigImageError::Io)?;
    if matches!(policy, OverwritePolicy::Replace) {
        temp.persist(&output).map_err(|e| BigImageError::Io(e.error))?;
        sync_parent(&output);
        return Ok(crate::ConvertOutcome::Written { output });
    }
    let mut temp = temp;
    let mut candidate = output.clone();
    for n in 0..=9_999u32 {
        if n > 0 {
            let mut name = output.file_stem().unwrap_or_default().to_os_string();
            name.push(format!("_{n}"));
            if let Some(ext) = output.extension() {
                name.push(".");
                name.push(ext);
            }
            candidate = output.with_file_name(name);
        }
        match temp.persist_noclobber(&candidate) {
            Ok(_) => {
                sync_parent(&candidate);
                return Ok(crate::ConvertOutcome::Written { output: candidate });
            }
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                if matches!(policy, OverwritePolicy::Skip) {
                    return Ok(crate::ConvertOutcome::Skipped { output });
                }
                temp = e.file;
            }
            Err(e) => return Err(BigImageError::Io(e.error)),
        }
    }
    Err(BigImageError::Other(format!("muitos arquivos com o mesmo nome: {}", output.display())))
}

/// Metadata written by the encoder in addition to the pixels.
#[derive(Debug, Default)]
pub(crate) struct Extras {
    /// Raw TIFF-structured EXIF (no `Exif\0\0` prefix).
    pub(crate) exif: Option<Vec<u8>>,
    /// ICC profile describing the pixels; `None` means sRGB, for which the
    /// generated canonical profile is embedded where supported.
    pub(crate) icc: Option<Vec<u8>>,
}

/// Encode into a unique sibling and publish only after encode, flush and sync.
pub(crate) fn encode_and_cleanup(
    img: DynamicImage,
    output: PathBuf,
    target: Format,
    opts: &EncodeOptions,
    policy: OverwritePolicy,
) -> Result<crate::ConvertOutcome> {
    encode_and_cleanup_with(img, output, target, opts, policy, &Extras::default())
}

/// [`encode_and_cleanup`] with extra metadata for the encoder.
pub(crate) fn encode_and_cleanup_with(
    img: DynamicImage,
    output: PathBuf,
    target: Format,
    opts: &EncodeOptions,
    policy: OverwritePolicy,
    extras: &Extras,
) -> Result<crate::ConvertOutcome> {
    use std::io::Write;
    let destination_bpp = match target {
        Format::OpenExr => 16,
        Format::Hdr => 12,
        _ => img.color().bytes_per_pixel().max(4),
    };
    check_dimensions(img.width(), img.height(), destination_bpp)?;
    let prepared = crate::convert::prepare_for_target(img, target);
    let mut temp = output_temp(&output)?;
    {
        let mut writer = std::io::BufWriter::new(temp.as_file_mut());
        encode_to_writer(&prepared, &mut writer, target, opts, extras)
            .map_err(|source| BigImageError::Encode { path: output.clone(), source })?;
        writer.flush().map_err(BigImageError::Io)?;
    }
    publish_temp(temp, output, policy)
}

fn encode_to_writer(
    img: &DynamicImage,
    writer: &mut (impl std::io::Write + std::io::Seek),
    target: Format,
    opts: &EncodeOptions,
    extras: &Extras,
) -> image::ImageResult<()> {
    use image::ImageEncoder;
    let icc = || match &extras.icc {
        Some(icc) => Ok(icc.clone()),
        None => crate::color::srgb_profile(),
    };
    let exif = |encoder: &mut dyn FnMut(Vec<u8>) -> image::ImageResult<()>| match &extras.exif {
        Some(exif) => encoder(exif.clone()),
        None => Ok(()),
    };
    match target {
        Format::Jpeg => {
            let mut encoder =
                image::codecs::jpeg::JpegEncoder::new_with_quality(writer, opts.jpeg_quality());
            encoder.set_icc_profile(icc()?).map_err(image::ImageError::Unsupported)?;
            exif(&mut |e| encoder.set_exif_metadata(e).map_err(image::ImageError::Unsupported))?;
            let rgb = img.to_rgb8();
            encoder.write_image(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                image::ExtendedColorType::Rgb8,
            )
        }
        Format::Avif => {
            let encoder = image::codecs::avif::AvifEncoder::new_with_speed_quality(
                writer,
                opts.effective_avif_speed(),
                opts.quality.unwrap_or(80),
            )
            .with_num_threads(Some(
                std::thread::available_parallelism()
                    .map(|n| n.get().saturating_sub(1).clamp(1, 2))
                    .unwrap_or(1),
            ));
            let mut encoder = encoder;
            exif(&mut |e| encoder.set_exif_metadata(e).map_err(image::ImageError::Unsupported))?;
            let rgba = img.to_rgba8();
            encoder.write_image(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                image::ExtendedColorType::Rgba8,
            )
        }
        Format::Png => {
            use image::codecs::png::{CompressionType, FilterType, PngEncoder};
            let mut encoder = if opts.optimize {
                PngEncoder::new_with_quality(writer, CompressionType::Best, FilterType::Adaptive)
            } else {
                PngEncoder::new(writer)
            };
            // A custom profile was matched to the pixel layout by the caller;
            // the generated sRGB one only describes RGB pixels.
            if extras.icc.is_some()
                || matches!(
                    img.color(),
                    image::ColorType::Rgb8
                        | image::ColorType::Rgba8
                        | image::ColorType::Rgb16
                        | image::ColorType::Rgba16
                )
            {
                encoder.set_icc_profile(icc()?).map_err(image::ImageError::Unsupported)?;
            }
            exif(&mut |e| encoder.set_exif_metadata(e).map_err(image::ImageError::Unsupported))?;
            encoder.write_image(img.as_bytes(), img.width(), img.height(), img.color().into())
        }
        Format::WebP if extras.icc.is_some() || extras.exif.is_some() => {
            let mut encoder = image::codecs::webp::WebPEncoder::new_lossless(writer);
            if let Some(icc) = &extras.icc {
                encoder.set_icc_profile(icc.clone()).map_err(image::ImageError::Unsupported)?;
            }
            exif(&mut |e| encoder.set_exif_metadata(e).map_err(image::ImageError::Unsupported))?;
            encoder.write_image(img.as_bytes(), img.width(), img.height(), img.color().into())
        }
        Format::Tiff if extras.icc.is_some() => {
            let mut encoder = image::codecs::tiff::TiffEncoder::new(writer);
            encoder.set_icc_profile(icc()?).map_err(image::ImageError::Unsupported)?;
            encoder.write_image(img.as_bytes(), img.width(), img.height(), img.color().into())
        }
        _ => img.write_to(writer, target.to_image_format()),
    }
}
