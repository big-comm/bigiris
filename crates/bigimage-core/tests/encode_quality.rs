// SPDX-License-Identifier: GPL-3.0-or-later
//! "Smallest file at a guaranteed visual quality": every lossy output meets
//! its SSIMULACRA2 target, is much smaller than the previous encoders,
//! keeps metadata, and same-format conversions are never larger.
use bigimage_core::{
    convert_file_report, save_image, save_image_preserving_metadata, EncodeOptions, Format,
    OverwritePolicy, QualityTarget, SourceMetadata,
};
use fast_ssim2::{PixelDescriptor, PixelSlice, Ssimulacra2Reference};
use image::{DynamicImage, ImageEncoder, RgbaImage};

fn composite(image: &RgbaImage, bg: u32) -> Vec<u8> {
    image
        .as_raw()
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|px| {
            let a = u32::from(px[3]);
            [0, 1, 2].map(|c| ((u32::from(px[c]) * a + bg * (255 - a) + 127) / 255) as u8)
        })
        .collect()
}

/// Worst SSIMULACRA2 over black and white backgrounds.
fn score(reference: &RgbaImage, distorted: &RgbaImage) -> f64 {
    let (w, h) = reference.dimensions();
    [0, 255]
        .into_iter()
        .map(|bg| {
            let a = composite(reference, bg);
            let b = composite(distorted, bg);
            let sa = PixelSlice::new(&a, w, h, w as usize * 3, PixelDescriptor::RGB8_SRGB).unwrap();
            let sb = PixelSlice::new(&b, w, h, w as usize * 3, PixelDescriptor::RGB8_SRGB).unwrap();
            Ssimulacra2Reference::new(&sa).unwrap().compare(&sb).unwrap()
        })
        .fold(f64::INFINITY, f64::min)
}

/// Smooth gradients, soft waves and fine grain, like a photo.
fn photo() -> DynamicImage {
    DynamicImage::ImageRgb8(image::RgbImage::from_fn(320, 240, |x, y| {
        let (fx, fy) = (x as f32 / 320.0, y as f32 / 240.0);
        let grain = ((x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) >> 13) % 17;
        let wave = ((fx * 14.0).sin() * (fy * 9.0).cos() * 40.0) as i32;
        let c = |base: f32, k: i32| {
            ((base * 255.0) as i32 + wave * k / 2 + grain as i32 - 8).clamp(0, 255) as u8
        };
        image::Rgb([c(0.3 + 0.5 * fx, 1), c(0.2 + 0.6 * fy, -1), c(0.7 - 0.4 * fx * fy, 1)])
    }))
}

/// Flat colours, hard edges, thin dark strokes and a transparent ramp.
fn graphic() -> DynamicImage {
    DynamicImage::ImageRgba8(RgbaImage::from_fn(320, 240, |x, y| {
        let stroke = x % 9 < 2 && y % 24 < 14 && (x / 9 + y / 24) % 3 != 0;
        image::Rgba(match (x / 80, (y / 24) % 5) {
            _ if stroke => [20, 20, 20, 255],
            (0, _) => [240, 240, 240, 255],
            (1, 0..=1) => [33, 150, 243, 255],
            (1, _) => [255, 193, 7, 255],
            (2, _) => [76, 175, 80, 255],
            _ => [244, 67, 54, ((x % 80) * 3) as u8],
        })
    }))
}

/// The pixels the target format displays (JPEG flattens over white).
fn displayed(image: &DynamicImage, format: Format) -> RgbaImage {
    let rgba = image.to_rgba8();
    if format != Format::Jpeg {
        return rgba;
    }
    let rgb = composite(&rgba, 255);
    DynamicImage::ImageRgb8(image::RgbImage::from_raw(rgba.width(), rgba.height(), rgb).unwrap())
        .into_rgba8()
}

fn save(
    image: &DynamicImage,
    format: Format,
    opts: &EncodeOptions,
    dir: &std::path::Path,
) -> Vec<u8> {
    let path = dir.join(format!("out-{:?}-{:?}.{}", opts.target, opts.quality, format.extension()));
    save_image(image.clone(), &path, format, opts, OverwritePolicy::Replace).unwrap();
    std::fs::read(&path).unwrap()
}

#[test]
fn lossy_and_palette_outputs_meet_every_target() {
    let dir = tempfile::tempdir().unwrap();
    for (name, image) in [("photo", photo()), ("graphic", graphic())] {
        for format in [Format::Jpeg, Format::WebP, Format::Avif, Format::Png] {
            for target in [QualityTarget::Maximum, QualityTarget::Balanced, QualityTarget::Smallest]
            {
                let opts = EncodeOptions { target, ..EncodeOptions::default() };
                let bytes = save(&image, format, &opts, dir.path());
                let decoded = image::load_from_memory(&bytes).unwrap().into_rgba8();
                let got = score(&displayed(&image, format), &decoded);
                assert!(
                    got >= target.ssimulacra2(),
                    "{name} {format:?} {target:?}: SSIMULACRA2 {got:.2} < {}",
                    target.ssimulacra2()
                );
            }
        }
    }
    // GIF holds 256 colours at most; flat graphics must still pass.
    let opts = EncodeOptions::default();
    let bytes = save(&graphic(), Format::Gif, &opts, dir.path());
    let decoded = image::load_from_memory(&bytes).unwrap().into_rgba8();
    let flat = graphic().to_rgba8();
    let mut binary = flat.clone();
    for px in binary.pixels_mut() {
        px[3] = if px[3] < 128 { 0 } else { 255 };
    }
    assert!(score(&binary, &decoded) >= 90.0);
}

#[test]
fn balanced_photos_are_much_smaller_than_the_previous_encoders() {
    let dir = tempfile::tempdir().unwrap();
    let image = photo();
    let rgb = image.to_rgb8();
    let (w, h) = rgb.dimensions();
    let opts = EncodeOptions::default();

    // Previous JPEG: image's baseline encoder. Compare at equal quality:
    // the lowest of its qualities that scores at least as well as ours.
    let jpeg = save(&image, Format::Jpeg, &opts, dir.path());
    let reference = image.to_rgba8();
    let ours = score(&reference, &image::load_from_memory(&jpeg).unwrap().into_rgba8());
    let old_jpeg = (50..=100)
        .step_by(5)
        .map(|quality| {
            let mut old = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut old, quality)
                .write_image(rgb.as_raw(), w, h, image::ExtendedColorType::Rgb8)
                .unwrap();
            old
        })
        .find(|old| score(&reference, &image::load_from_memory(old).unwrap().into_rgba8()) >= ours)
        .unwrap();
    assert!(jpeg.len() * 10 < old_jpeg.len() * 8, "JPEG {} vs {}", jpeg.len(), old_jpeg.len());

    // Previous WebP: always lossless.
    let rgba = image.to_rgba8();
    let mut old_webp = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut old_webp)
        .write_image(rgba.as_raw(), w, h, image::ExtendedColorType::Rgba8)
        .unwrap();
    let webp = save(&image, Format::WebP, &opts, dir.path());
    assert!(webp.len() * 3 < old_webp.len(), "WebP {} vs {}", webp.len(), old_webp.len());

    // Previous PNG: image's default encoder; ours is lossless in Maximum.
    let mut old_png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut old_png)
        .write_image(rgb.as_raw(), w, h, image::ExtendedColorType::Rgb8)
        .unwrap();
    let max = EncodeOptions { target: QualityTarget::Maximum, ..EncodeOptions::default() };
    let png = save(&image, Format::Png, &max, dir.path());
    assert!(png.len() < old_png.len(), "PNG {} vs {}", png.len(), old_png.len());
    let back = image::load_from_memory(&png).unwrap().into_rgb8();
    assert_eq!(back, rgb, "Maximum PNG must stay lossless");
}

#[test]
fn explicit_quality_keeps_fixed_quality_behaviour() {
    let dir = tempfile::tempdir().unwrap();
    let image = photo();
    for format in [Format::Jpeg, Format::WebP, Format::Avif] {
        let size = |quality: u8, target: QualityTarget| {
            let opts = EncodeOptions {
                quality: Some(quality),
                target,
                avif_speed: 10,
                ..EncodeOptions::default()
            };
            save(&image, format, &opts, dir.path())
        };
        // The target is ignored once the quality is fixed…
        assert_eq!(
            size(40, QualityTarget::Maximum),
            size(40, QualityTarget::Smallest),
            "{format:?}: target changed a fixed-quality encode"
        );
        // …and the quality still drives the size.
        assert!(
            size(20, QualityTarget::Balanced).len() < size(95, QualityTarget::Balanced).len(),
            "{format:?}"
        );
    }
    // WebP at quality 100 is lossless.
    let opts = EncodeOptions { quality: Some(100), ..EncodeOptions::default() };
    let webp = save(&image, Format::WebP, &opts, dir.path());
    assert_eq!(image::load_from_memory(&webp).unwrap().into_rgb8(), image.to_rgb8());
}

fn tiff_exif() -> Vec<u8> {
    // IFD0 with Make = "Cam".
    let mut exif = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0];
    exif.extend_from_slice(&[0x0f, 0x01, 2, 0, 4, 0, 0, 0, b'C', b'a', b'm', 0]);
    exif.extend_from_slice(&[0, 0, 0, 0]);
    exif
}

#[test]
fn searched_outputs_still_embed_exif_and_icc() {
    use image::ImageDecoder;
    let dir = tempfile::tempdir().unwrap();
    let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
    let source = SourceMetadata { exif: Some(tiff_exif()), icc: Some(p3.clone()) };
    for format in [Format::Jpeg, Format::Png, Format::WebP] {
        let path = dir.path().join(format!("meta.{}", format.extension()));
        save_image_preserving_metadata(
            photo(),
            &path,
            format,
            &EncodeOptions::default(),
            OverwritePolicy::Replace,
            &source,
        )
        .unwrap();
        let mut decoder = image::ImageReader::open(&path)
            .unwrap()
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .unwrap();
        assert_eq!(decoder.icc_profile().unwrap().as_deref(), Some(&p3[..]), "{format:?} ICC");
        let meta = bigimage_core::metadata::read(&path).unwrap();
        assert!(meta.has_camera_info, "{format:?} EXIF lost: {:?}", meta.tags);
    }
}

#[test]
fn same_format_conversion_never_grows_the_file() {
    let dir = tempfile::tempdir().unwrap();
    // A tiny PNG without a colour profile: the re-encode embeds the sRGB
    // profile (hundreds of bytes), so it cannot be smaller.
    let rgb = image::RgbImage::from_fn(8, 8, |x, y| image::Rgb([x as u8 * 30, y as u8 * 30, 9]));
    let encode = |exif: bool| {
        let mut out = Vec::new();
        let mut encoder = image::codecs::png::PngEncoder::new(&mut out);
        if exif {
            encoder.set_exif_metadata(tiff_exif()).unwrap();
        }
        encoder.write_image(rgb.as_raw(), 8, 8, image::ExtendedColorType::Rgb8).unwrap();
        out
    };
    let plain = encode(false);
    let src = dir.path().join("tiny.png");
    std::fs::write(&src, &plain).unwrap();
    let out = dir.path().join("out");
    let opts = EncodeOptions::default();
    let report =
        convert_file_report(&src, Some(&out), Format::Png, &opts, OverwritePolicy::Replace)
            .unwrap();
    assert!(report.kept_original, "re-encode was not larger?");
    assert_eq!(std::fs::read(report.outcome.path()).unwrap(), plain);

    // The same source with EXIF: stripping wins over size.
    let tagged = dir.path().join("tagged.png");
    std::fs::write(&tagged, encode(true)).unwrap();
    let report =
        convert_file_report(&tagged, Some(&out), Format::Png, &opts, OverwritePolicy::Replace)
            .unwrap();
    assert!(!report.kept_original);
    assert!(bigimage_core::metadata::read(report.outcome.path()).unwrap().tags.is_empty());
    // Unless the caller keeps metadata on purpose.
    let keep = EncodeOptions { strip_metadata: false, ..EncodeOptions::default() };
    let report =
        convert_file_report(&tagged, Some(&out), Format::Png, &keep, OverwritePolicy::Replace)
            .unwrap();
    assert!(report.kept_original);

    // Across formats the encode is always written.
    let report =
        convert_file_report(&src, Some(&out), Format::Bmp, &opts, OverwritePolicy::Replace)
            .unwrap();
    assert!(!report.kept_original);
    assert!(image::open(report.outcome.path()).is_ok());
}
