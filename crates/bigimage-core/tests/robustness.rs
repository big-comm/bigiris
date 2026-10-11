// SPDX-License-Identifier: GPL-3.0-or-later
//! Regression tests for untrusted inputs, concurrent output and data preservation.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

use bigimage_core::{
    convert_file_to, make_gif, open_image, AnimateOptions, ConvertOutcome, EncodeOptions,
    ExtentOptions, ExtentSize, Filter, Format, Gravity, OverwritePolicy, ResizeMode,
};
use image::{DynamicImage, GenericImageView, Rgb, RgbImage};

fn fixture(dir: &Path, name: &str, color: u8) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    RgbImage::from_pixel(32, 24, Rgb([color, 20, 30])).save(&path).unwrap();
    path
}

fn assert_no_partials(dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        assert!(!entry.unwrap().file_name().to_string_lossy().starts_with(".bigiris-"));
    }
}

#[test]
fn every_advertised_format_encodes_and_decodes() {
    let dir = tempfile::tempdir().unwrap();
    let src = fixture(dir.path(), "source.png", 80);
    for format in [
        Format::Png,
        Format::Jpeg,
        Format::WebP,
        Format::Avif,
        Format::Tiff,
        Format::Bmp,
        Format::Gif,
        Format::Ico,
        Format::Pnm,
        Format::Tga,
        Format::Qoi,
        Format::Hdr,
        Format::OpenExr,
    ] {
        let result = convert_file_to(
            &src,
            Some(&dir.path().join("out")),
            format,
            &EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap();
        assert_eq!(open_image(result.path()).unwrap().dimensions(), (32, 24), "{format:?}");
    }
}

#[test]
fn concurrent_increment_preserves_every_distinct_image() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out");
    let barrier = Arc::new(Barrier::new(24));
    let handles: Vec<_> = (0..24)
        .map(|i| {
            let src = fixture(&dir.path().join(format!("in{i}")), "same.png", i);
            let output = output.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let result = convert_file_to(
                    src,
                    Some(&output),
                    Format::Png,
                    &EncodeOptions::default(),
                    OverwritePolicy::Increment,
                )
                .unwrap();
                assert!(matches!(result, ConvertOutcome::Written { .. }));
                (result.path().to_path_buf(), i)
            })
        })
        .collect();
    let mut paths = std::collections::HashSet::new();
    for handle in handles {
        let (path, color) = handle.join().unwrap();
        assert!(paths.insert(path.clone()));
        assert_eq!(open_image(path).unwrap().to_rgb8().get_pixel(0, 0)[0], color);
    }
    assert_eq!(std::fs::read_dir(&output).unwrap().count(), 24);
    assert_no_partials(&output);
}

#[test]
fn concurrent_skip_publishes_exactly_one_complete_file() {
    let dir = tempfile::tempdir().unwrap();
    let src = fixture(dir.path(), "same.png", 80);
    let output = dir.path().join("out");
    let barrier = Arc::new(Barrier::new(16));
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let src = src.clone();
            let output = output.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                convert_file_to(
                    src,
                    Some(&output),
                    Format::Png,
                    &EncodeOptions::default(),
                    OverwritePolicy::Skip,
                )
                .unwrap()
            })
        })
        .collect();
    let written = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|out| matches!(out, ConvertOutcome::Written { .. }))
        .count();
    assert_eq!(written, 1);
    assert_eq!(open_image(output.join("same.png")).unwrap().dimensions(), (32, 24));
    assert_no_partials(&output);
}

#[test]
fn failed_gif_keeps_existing_destination_and_cleans_partial() {
    let dir = tempfile::tempdir().unwrap();
    let frame = fixture(dir.path(), "valid.png", 80);
    let output = dir.path().join("animation.gif");
    std::fs::write(&output, b"existing user data").unwrap();
    let missing = dir.path().join("missing.png");
    assert!(make_gif(&[frame, missing], &output, AnimateOptions::default()).is_err());
    assert_eq!(std::fs::read(output).unwrap(), b"existing user data");
    assert_no_partials(dir.path());
}

#[test]
fn gif_destination_can_also_be_an_input_without_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let frame = fixture(dir.path(), "valid.png", 80);
    let output = dir.path().join("animation.gif");
    make_gif(&[frame], &output, AnimateOptions::default()).unwrap();
    make_gif(&[output.clone(), output.clone()], &output, AnimateOptions::default()).unwrap();
    use image::AnimationDecoder;
    let decoder = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(
        std::fs::File::open(output).unwrap(),
    ))
    .unwrap();
    assert_eq!(decoder.into_frames().collect_frames().unwrap().len(), 2);
}

#[test]
fn oversized_file_and_pixel_headers_are_rejected_by_viewer() {
    let dir = tempfile::tempdir().unwrap();
    let sparse = dir.path().join("huge.png");
    std::fs::File::create(&sparse).unwrap().set_len(1024 * 1024 * 1024 + 1).unwrap();
    assert!(matches!(open_image(&sparse), Err(image::ImageError::Limits(_))));
    let pixels = dir.path().join("pixels.ppm");
    std::fs::write(&pixels, b"P6\n100000 100000\n255\n").unwrap();
    assert!(matches!(open_image(&pixels), Err(image::ImageError::Limits(_))));
    // Below the pixel limit, but RGB16 requires >512 MiB.
    std::fs::write(&pixels, b"P6\n16000 12000\n65535\n").unwrap();
    assert!(matches!(open_image(&pixels), Err(image::ImageError::Limits(_))));
}

#[test]
fn enormous_resize_and_extent_return_errors_before_allocation() {
    let img = DynamicImage::ImageRgb8(RgbImage::new(2, 2));
    for mode in [
        ResizeMode::Exact { width: u32::MAX, height: u32::MAX },
        ResizeMode::Fit { width: u32::MAX, height: u32::MAX },
    ] {
        assert!(bigimage_core::resize::apply_to(&img, mode, Filter::Nearest).is_err());
    }
    let opts = ExtentOptions {
        size: ExtentSize::Exact { width: u32::MAX, height: 2 },
        background: [0; 4],
        gravity: Gravity::Center,
    };
    assert!(bigimage_core::extent::apply_to(&img, &opts).is_err());
}

#[test]
fn truncated_images_fail_without_panicking_or_publishing() {
    let dir = tempfile::tempdir().unwrap();
    let src = fixture(dir.path(), "good.png", 80);
    let bytes = std::fs::read(src).unwrap();
    let bad = dir.path().join("bad.png");
    let out = dir.path().join("out");
    for len in [0, 1, 8, 16, bytes.len() / 2] {
        std::fs::write(&bad, &bytes[..len]).unwrap();
        assert!(open_image(&bad).is_err());
        assert!(convert_file_to(
            &bad,
            Some(&out),
            Format::Jpeg,
            &EncodeOptions::default(),
            OverwritePolicy::Replace
        )
        .is_err());
        assert!(!out.join("bad.jpg").exists());
    }
}

#[cfg(unix)]
#[test]
fn skip_and_increment_do_not_follow_dangling_destination_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let src = fixture(dir.path(), "same.png", 80);
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let victim = dir.path().join("victim");
    std::os::unix::fs::symlink(&victim, out.join("same.png")).unwrap();
    let skipped = convert_file_to(
        &src,
        Some(&out),
        Format::Png,
        &EncodeOptions::default(),
        OverwritePolicy::Skip,
    )
    .unwrap();
    assert!(matches!(skipped, ConvertOutcome::Skipped { .. }));
    let incremented = convert_file_to(
        &src,
        Some(&out),
        Format::Png,
        &EncodeOptions::default(),
        OverwritePolicy::Increment,
    )
    .unwrap();
    assert_eq!(incremented.path(), out.join("same_1.png"));
    assert!(!victim.exists());
    assert!(out.join("same.png").is_symlink());
}

#[test]
fn rgb16_and_float_sources_convert_to_every_advertised_format() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("rgb16.png");
    image::ImageBuffer::from_pixel(8, 8, image::Rgb([12345u16, 23456, 34567]))
        .save(&source)
        .unwrap();
    let formats = [
        Format::Png,
        Format::Jpeg,
        Format::WebP,
        Format::Avif,
        Format::Tiff,
        Format::Bmp,
        Format::Gif,
        Format::Ico,
        Format::Pnm,
        Format::Tga,
        Format::Qoi,
        Format::Hdr,
        Format::OpenExr,
    ];
    for format in formats {
        let outcome = convert_file_to(
            &source,
            Some(&dir.path().join("out")),
            format,
            &EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap();
        assert_eq!(open_image(outcome.path()).unwrap().dimensions(), (8, 8), "{format:?}");
    }
    let float = dir.path().join("out/rgb16.exr");
    for format in formats {
        let outcome = convert_file_to(
            &float,
            Some(&dir.path().join("float")),
            format,
            &EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap();
        assert_eq!(open_image(outcome.path()).unwrap().dimensions(), (8, 8), "float to {format:?}");
    }
}

#[test]
fn png_optimization_preserves_sixteen_bit_pixels() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("rgb16.png");
    let image = image::ImageBuffer::from_pixel(8, 8, image::Rgb([12345u16, 23456, 34567]));
    image.save(&source).unwrap();
    let outcome = convert_file_to(
        &source,
        Some(&dir.path().join("out")),
        Format::Png,
        &EncodeOptions { optimize: true, ..EncodeOptions::default() },
        OverwritePolicy::Replace,
    )
    .unwrap();
    let decoded = open_image(outcome.path()).unwrap();
    assert_eq!(decoded.color(), image::ColorType::Rgb16);
    assert_eq!(decoded.to_rgb16(), image);
}

#[test]
fn large_ico_is_scaled_to_supported_dimensions() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("large.png");
    RgbImage::new(512, 256).save(&src).unwrap();
    let out = convert_file_to(
        &src,
        None,
        Format::Ico,
        &EncodeOptions::default(),
        OverwritePolicy::Replace,
    )
    .unwrap();
    assert_eq!(open_image(out.path()).unwrap().dimensions(), (256, 128));
}
