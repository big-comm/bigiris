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
///
/// The operation is still validated first (see [`skip_existing`]), so an
/// invalid request fails the same way whether or not the output exists.
pub(crate) fn skip_before_decode(
    input: &Path,
    suffix: &str,
    target: Option<Format>,
    policy: OverwritePolicy,
    validate: impl Fn(u32, u32) -> Result<()>,
) -> Result<Option<crate::ConvertOutcome>> {
    if !matches!(policy, OverwritePolicy::Skip) {
        return Ok(None);
    }
    let Some(target) = target.or_else(|| sniff_format(input)) else {
        return Ok(None);
    };
    let Ok(output) = resolve_output(input, Some(suffix), target, policy) else {
        return Ok(None);
    };
    skip_existing(input, output, target, validate)
}

/// Skip outcome for an `output` that already exists, after the cheap checks
/// a full run would fail on: the input must be a static image (no animation
/// or extra pages) and `validate` must accept its header dimensions.
///
/// The header reports stored dimensions, before EXIF/AVIF orientation is
/// applied; the operation is rejected only when it is invalid for both the
/// stored and the transposed size, so a rotated photo is never refused
/// wrongly (the post-decode check stays authoritative).
///
/// PNM outputs are never skipped here: their extension depends on the
/// pixels (see [`content_output`]), so the decision waits for the decode.
pub(crate) fn skip_existing(
    input: &Path,
    output: PathBuf,
    target: Format,
    validate: impl Fn(u32, u32) -> Result<()>,
) -> Result<Option<crate::ConvertOutcome>> {
    if target == Format::Pnm || output.symlink_metadata().is_err() {
        return Ok(None);
    }
    crate::content::ensure_static(input)?;
    let (w, h) = crate::image_dimensions(input)
        .map_err(|source| BigImageError::Decode { path: input.to_path_buf(), source })?;
    validate(w, h).or_else(|error| validate(h, w).map_err(|_| error))?;
    tracing::debug!(?output, "skipping existing output before decoding");
    Ok(Some(crate::ConvertOutcome::Skipped { output }))
}

/// Post-decode `Skip` check for a planned `output`. Always `false` for PNM,
/// whose final name is only known once the pixels are (see
/// [`content_output`]); publication still never overwrites under `Skip`.
pub(crate) fn existing_output(output: &Path, target: Format, policy: OverwritePolicy) -> bool {
    matches!(policy, OverwritePolicy::Skip) && target != Format::Pnm && output.exists()
}

/// Final output path for `image`: the planned one, except that a PNM
/// target takes the extension of what is actually written — `.pgm` for
/// gray, `.ppm` for colour ([`Format::extension_for`]).
pub(crate) fn content_output(output: PathBuf, target: Format, image: &DynamicImage) -> PathBuf {
    if target != Format::Pnm {
        return output;
    }
    output.with_extension(target.extension_for(image.color()))
}

/// [`encode_and_cleanup`] to the [`content_output`] path, for pipelines
/// that name their own outputs.
pub(crate) fn encode_output(
    img: DynamicImage,
    output: PathBuf,
    target: Format,
    opts: &EncodeOptions,
    policy: OverwritePolicy,
) -> Result<crate::ConvertOutcome> {
    let output = content_output(output, target, &img);
    encode_and_cleanup(img, output, target, opts, policy)
}

/// Accept-everything validator for operations valid on any static image.
pub(crate) fn any_size(_: u32, _: u32) -> Result<()> {
    Ok(())
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

/// Existing regular file a `Replace` publication will overwrite.
fn replaced_file(output: &Path, policy: OverwritePolicy) -> Option<std::fs::Metadata> {
    matches!(policy, OverwritePolicy::Replace)
        .then(|| std::fs::symlink_metadata(output).ok())
        .flatten()
        .filter(std::fs::Metadata::is_file)
}

/// A unique sibling temporary file, cleaned up on every error path.
///
/// The file is created with the mode a plain `open(2)` would use (`0o666`,
/// or the permission bits of the file a `Replace` will overwrite), so the
/// kernel applies the umask and any default ACL at creation. No `chmod` is
/// needed for new outputs, which keeps writes working on FUSE/gvfs/SMB
/// mounts that refuse `chmod`.
pub(crate) fn output_temp(
    output: &Path,
    policy: OverwritePolicy,
) -> Result<tempfile::NamedTempFile> {
    use std::os::unix::fs::PermissionsExt;
    let parent = output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(BigImageError::Io)?;
    let mode =
        replaced_file(output, policy).map_or(0o666, |meta| meta.permissions().mode() & 0o777);
    tempfile::Builder::new()
        .prefix(".bigiris-")
        .suffix(".part")
        .permissions(std::fs::Permissions::from_mode(mode))
        .tempfile_in(parent)
        .map_err(BigImageError::Io)
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
    // New outputs already got `0o666 & !umask` (and default ACLs) at
    // creation, see `output_temp`. A replaced file keeps its exact
    // permission bits: the umask may have narrowed them at creation, so copy
    // them again — best-effort, since some mounts refuse chmod and the
    // creation mode can only be narrower than the original, never wider.
    if let Some(meta) = replaced_file(&output, policy) {
        let wanted = meta.permissions().mode() & 0o777;
        let current = temp.as_file().metadata().map(|m| m.permissions().mode() & 0o777).ok();
        if current != Some(wanted) {
            if let Err(error) =
                temp.as_file().set_permissions(std::fs::Permissions::from_mode(wanted))
            {
                tracing::warn!(?output, %error, "permissões do arquivo substituído não copiadas");
            }
        }
    }
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

impl Extras {
    /// Nothing to embed besides the pixels.
    pub(crate) fn is_empty(&self) -> bool {
        self.exif.is_none() && self.icc.is_none()
    }
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
    encode_keeping_smaller(img, output, target, opts, policy, extras, None)
        .map(|(outcome, _)| outcome)
}

/// [`encode_and_cleanup_with`] that, given the `original` file the pixels
/// came from (same format as `target`), publishes the original bytes
/// instead when the new encode came out larger and the original carries
/// no metadata the encode would have stripped. Returns whether it did.
pub(crate) fn encode_keeping_smaller(
    img: DynamicImage,
    output: PathBuf,
    target: Format,
    opts: &EncodeOptions,
    policy: OverwritePolicy,
    extras: &Extras,
    original: Option<&Path>,
) -> Result<(crate::ConvertOutcome, bool)> {
    use std::io::{Seek, Write};
    let destination_bpp = match target {
        Format::OpenExr => 16,
        Format::Hdr => 12,
        _ => img.color().bytes_per_pixel().max(4),
    };
    check_dimensions(img.width(), img.height(), destination_bpp)?;
    let prepared = crate::convert::prepare_for_target(img, target);
    let mut temp = output_temp(&output, policy)?;
    {
        let mut writer = std::io::BufWriter::new(temp.as_file_mut());
        encode_to_writer(&prepared, &mut writer, target, opts, extras)
            .map_err(|source| BigImageError::Encode { path: output.clone(), source })?;
        writer.flush().map_err(BigImageError::Io)?;
    }
    let mut kept = false;
    if let Some(original) = original {
        let written = temp.as_file().metadata().map_err(BigImageError::Io)?.len();
        if let Some(bytes) = smaller_original(original, written, target, opts) {
            let file = temp.as_file_mut();
            file.set_len(0).map_err(BigImageError::Io)?;
            file.rewind().map_err(BigImageError::Io)?;
            file.write_all(&bytes).map_err(BigImageError::Io)?;
            kept = true;
            tracing::debug!(
                ?original,
                written,
                original_len = bytes.len(),
                "mantido o original menor"
            );
        }
    }
    publish_temp(temp, output, policy).map(|outcome| (outcome, kept))
}

/// The bytes of `original` when they are smaller than `written` and can be
/// published as-is: the file is a regular file still in `target` format,
/// and either metadata stripping is off or it carries none.
fn smaller_original(
    original: &Path,
    written: u64,
    target: Format,
    opts: &EncodeOptions,
) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(original).ok()?;
    if !meta.is_file() || meta.len() >= written {
        return None;
    }
    let bytes = std::fs::read(original).ok()?;
    let format = image::guess_format(&bytes).ok().and_then(Format::from_image_format)?;
    if format != target
        || (opts.strip_metadata && crate::metadata::carries_metadata(&bytes, format))
    {
        return None;
    }
    Some(bytes)
}

fn encode_to_writer(
    img: &DynamicImage,
    writer: &mut (impl std::io::Write + std::io::Seek),
    target: Format,
    opts: &EncodeOptions,
    extras: &Extras,
) -> image::ImageResult<()> {
    let icc = || match &extras.icc {
        Some(icc) => Ok(Some(icc.clone())),
        None => crate::color::srgb_profile().map(Some),
    };
    // Which profile each format embeds: JPEG always (generated sRGB unless
    // the caller matched a source profile to the pixels); PNG a custom one,
    // or sRGB for RGB pixels only (the generated profile is RGB); WebP and
    // TIFF only a caller-provided one; the rest none.
    let icc = match target {
        Format::Jpeg => icc()?,
        Format::Png
            if extras.icc.is_some()
                || matches!(
                    img.color(),
                    image::ColorType::Rgb8
                        | image::ColorType::Rgba8
                        | image::ColorType::Rgb16
                        | image::ColorType::Rgba16
                ) =>
        {
            icc()?
        }
        Format::WebP | Format::Tiff => extras.icc.clone(),
        _ => None,
    };
    let exif = match target {
        Format::Jpeg | Format::Png | Format::WebP | Format::Avif => extras.exif.as_deref(),
        _ => None,
    };
    if let Some(bytes) = crate::optimize::encode(img, target, opts, icc.as_deref(), exif)? {
        writer.write_all(&bytes)?;
        return Ok(());
    }
    match target {
        Format::Pnm => write_pnm(img, writer),
        _ => img.write_to(writer, target.to_image_format()),
    }
}

/// Binary PGM (P5) or PPM (P6) at the prepared depth. The vendored PNM
/// encoder writes PAM (P7) by default — which many viewers cannot open and
/// which contradicts the `.pgm`/`.ppm` name — and its P5/P6 headers only
/// take 8-bit samples. Netpbm stores 16-bit samples big-endian.
fn write_pnm(img: &DynamicImage, writer: &mut impl std::io::Write) -> image::ImageResult<()> {
    use image::ColorType;
    let rgb8;
    let img = match img.color() {
        ColorType::L8 | ColorType::L16 | ColorType::Rgb8 | ColorType::Rgb16 => img,
        // prepare_for_target never produces these; stay total anyway.
        _ => {
            rgb8 = DynamicImage::ImageRgb8(img.to_rgb8());
            &rgb8
        }
    };
    let (magic, maxval) = match img.color() {
        ColorType::L8 => ("P5", 255),
        ColorType::L16 => ("P5", 65535),
        ColorType::Rgb16 => ("P6", 65535),
        _ => ("P6", 255),
    };
    write!(writer, "{magic}\n{} {}\n{maxval}\n", img.width(), img.height())?;
    match img {
        DynamicImage::ImageLuma16(b) => write_be16(b.as_raw(), writer)?,
        DynamicImage::ImageRgb16(b) => write_be16(b.as_raw(), writer)?,
        other => writer.write_all(other.as_bytes())?,
    }
    Ok(())
}

fn write_be16(samples: &[u16], writer: &mut impl std::io::Write) -> std::io::Result<()> {
    let mut chunk = Vec::with_capacity(64 * 1024);
    for part in samples.chunks(32 * 1024) {
        chunk.clear();
        chunk.extend(part.iter().flat_map(|v| v.to_be_bytes()));
        writer.write_all(&chunk)?;
    }
    Ok(())
}
