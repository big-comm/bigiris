// SPDX-License-Identifier: GPL-3.0-or-later
//! Size, SSIMULACRA2 and time per format and quality target, against the
//! previous encoders (image's baseline JPEG q75, lossless WebP, AVIF speed 8
//! q80, default PNG).
//!
//! ```text
//! encode_benchmark [IMAGE…]            # synthetic photo + graphic when empty
//! encode_benchmark score REF FILE…     # score other tools' outputs
//! ```
//!
//! Inputs are only read; outputs go to a temporary directory.
use std::path::{Path, PathBuf};
use std::time::Instant;

use bigimage_core::{EncodeOptions, Format, OverwritePolicy, QualityTarget};
use fast_ssim2::{PixelDescriptor, PixelSlice, Ssimulacra2Reference};
use image::{DynamicImage, ImageEncoder, RgbaImage};

type Error = Box<dyn std::error::Error>;

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
fn score(reference: &RgbaImage, distorted: &RgbaImage) -> Result<f64, Error> {
    let (w, h) = reference.dimensions();
    let mut worst = f64::INFINITY;
    for bg in [0, 255] {
        let a = composite(reference, bg);
        let b = composite(distorted, bg);
        let sa = PixelSlice::new(&a, w, h, w as usize * 3, PixelDescriptor::RGB8_SRGB)?;
        let sb = PixelSlice::new(&b, w, h, w as usize * 3, PixelDescriptor::RGB8_SRGB)?;
        worst = worst.min(Ssimulacra2Reference::new(&sa)?.compare(&sb)?);
    }
    Ok(worst)
}

fn flatten_white(image: &RgbaImage) -> RgbaImage {
    let rgb = composite(image, 255);
    DynamicImage::ImageRgb8(image::RgbImage::from_raw(image.width(), image.height(), rgb).unwrap())
        .into_rgba8()
}

fn synthetic() -> Vec<(String, DynamicImage)> {
    // Photo-like: smooth gradients, soft texture and noise.
    let photo = image::RgbImage::from_fn(1024, 768, |x, y| {
        let (fx, fy) = (x as f32 / 1024.0, y as f32 / 768.0);
        let n = ((x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) >> 13) % 23;
        let wave = ((fx * 18.0).sin() * (fy * 11.0).cos() * 40.0) as i32;
        let c = |base: f32, k: i32| (base * 255.0) as i32 + wave * k / 2 + n as i32 - 11;
        image::Rgb([
            c(0.3 + 0.5 * fx, 1).clamp(0, 255) as u8,
            c(0.2 + 0.6 * fy, -1).clamp(0, 255) as u8,
            c(0.7 - 0.4 * fx * fy, 1).clamp(0, 255) as u8,
        ])
    });
    // Graphic-like: flat colours, hard edges, thin lines and text-ish bars.
    let graphic = image::RgbaImage::from_fn(1024, 768, |x, y| {
        let bar = (y / 24) % 5;
        let text = x % 9 < 2 && y % 24 < 14 && (x / 9 + y / 24) % 3 != 0;
        let px = match (x / 256, bar) {
            _ if text => [20, 20, 20, 255],
            (0, _) => [240, 240, 240, 255],
            (1, 0..=1) => [33, 150, 243, 255],
            (1, _) => [255, 193, 7, 255],
            (2, _) => [76, 175, 80, 255],
            _ => [244, 67, 54, (x % 256) as u8],
        };
        image::Rgba(px)
    });
    vec![
        ("synthetic-photo".to_owned(), DynamicImage::ImageRgb8(photo)),
        ("synthetic-graphic".to_owned(), DynamicImage::ImageRgba8(graphic)),
    ]
}

fn old_encoder(image: &DynamicImage, format: Format) -> Result<Option<Vec<u8>>, Error> {
    let mut out = Vec::new();
    let rgba = image.to_rgba8();
    let (w, h) = rgba.dimensions();
    match format {
        Format::Jpeg => {
            let rgb = flatten_white(&rgba);
            let rgb = DynamicImage::ImageRgba8(rgb).into_rgb8();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 75).write_image(
                rgb.as_raw(),
                w,
                h,
                image::ExtendedColorType::Rgb8,
            )?;
        }
        Format::WebP => image::codecs::webp::WebPEncoder::new_lossless(&mut out).write_image(
            rgba.as_raw(),
            w,
            h,
            image::ExtendedColorType::Rgba8,
        )?,
        Format::Avif => image::codecs::avif::AvifEncoder::new_with_speed_quality(&mut out, 8, 80)
            .write_image(rgba.as_raw(), w, h, image::ExtendedColorType::Rgba8)?,
        Format::Png => image::codecs::png::PngEncoder::new(&mut out).write_image(
            image.as_bytes(),
            w,
            h,
            image.color().into(),
        )?,
        _ => return Ok(None),
    }
    Ok(Some(out))
}

fn reference_for(image: &DynamicImage, format: Format) -> RgbaImage {
    let rgba = image.to_rgba8();
    if format == Format::Jpeg {
        flatten_white(&rgba)
    } else {
        rgba
    }
}

fn bench(name: &str, image: &DynamicImage, dir: &Path) -> Result<(), Error> {
    println!("\n## {name} ({}×{})", image.width(), image.height());
    println!("| formato | modo | bytes | SSIMULACRA2 | tempo (s) |");
    println!("|---|---|---:|---:|---:|");
    let formats = [Format::Jpeg, Format::WebP, Format::Avif, Format::Png, Format::Gif];
    for format in formats {
        let reference = reference_for(image, format);
        if let Some(bytes) = {
            let start = Instant::now();
            old_encoder(image, format)?.map(|b| (b, start.elapsed()))
        } {
            let (bytes, took) = bytes;
            let decoded = image::load_from_memory(&bytes)?.into_rgba8();
            println!(
                "| {format:?} | antigo | {} | {:.2} | {:.2} |",
                bytes.len(),
                score(&reference, &decoded)?,
                took.as_secs_f64()
            );
        }
        for (label, target) in [
            ("máxima", QualityTarget::Maximum),
            ("equilibrado", QualityTarget::Balanced),
            ("menor", QualityTarget::Smallest),
        ] {
            let path = dir.join(format!("{name}-{label}.{}", format.extension()));
            let opts = EncodeOptions { target, ..EncodeOptions::default() };
            let start = Instant::now();
            bigimage_core::save_image(
                image.clone(),
                &path,
                format,
                &opts,
                OverwritePolicy::Replace,
            )?;
            let took = start.elapsed();
            let decoded = bigimage_core::open_image(&path)?.into_rgba8();
            println!(
                "| {format:?} | {label} | {} | {:.2} | {:.2} |",
                std::fs::metadata(&path)?.len(),
                score(&reference, &decoded)?,
                took.as_secs_f64()
            );
        }
    }
    Ok(())
}

fn main() -> Result<(), Error> {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if args.first().is_some_and(|a| a == Path::new("score")) {
        let reference = bigimage_core::open_image(args.get(1).ok_or("REF")?)?.into_rgba8();
        for file in &args[2..] {
            let decoded = bigimage_core::open_image(file)?.into_rgba8();
            let reference = if decoded.pixels().all(|p| p[3] == 255) {
                flatten_white(&reference)
            } else {
                reference.clone()
            };
            println!(
                "{}\t{}\t{:.2}",
                file.display(),
                std::fs::metadata(file)?.len(),
                score(&reference, &decoded)?
            );
        }
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let inputs: Vec<(String, DynamicImage)> = if args.is_empty() {
        synthetic()
    } else {
        args.iter()
            .map(|p| {
                let name = p.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                Ok((name, bigimage_core::open_image(p)?))
            })
            .collect::<Result<_, Error>>()?
    };
    for (name, image) in &inputs {
        bench(name, image, dir.path())?;
    }
    Ok(())
}
