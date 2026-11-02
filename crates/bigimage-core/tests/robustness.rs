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

#[test]
fn gray_png_with_gray_icc_profile_opens() {
    use image::ImageEncoder;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gray.png");
    let mut encoder = image::codecs::png::PngEncoder::new(std::fs::File::create(&path).unwrap());
    encoder
        .set_icc_profile(moxcms::ColorProfile::new_gray_with_gamma(2.2).encode().unwrap())
        .unwrap();
    encoder.write_image(&[100u8; 6], 3, 2, image::ExtendedColorType::L8).unwrap();
    let image = open_image(&path).unwrap();
    assert_eq!(image.color(), image::ColorType::L8);
    assert_eq!(image.dimensions(), (3, 2));
}

fn expected_new_file_mode() -> u32 {
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

#[cfg(unix)]
#[test]
fn new_outputs_get_umask_permissions_instead_of_private_temp_mode() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let dir = tempfile::tempdir().unwrap();
    let src = fixture(dir.path(), "perm.png", 80);
    for policy in [OverwritePolicy::Replace, OverwritePolicy::Skip, OverwritePolicy::Increment] {
        let out = convert_file_to(
            &src,
            Some(&dir.path().join(format!("{policy:?}"))),
            Format::Jpeg,
            &EncodeOptions::default(),
            policy,
        )
        .unwrap();
        assert_eq!(mode(out.path()), expected_new_file_mode(), "{policy:?}");
    }
    let gif = dir.path().join("anim.gif");
    make_gif(std::slice::from_ref(&src), &gif, AnimateOptions::default()).unwrap();
    assert_eq!(mode(&gif), expected_new_file_mode());

    // Replacing an existing file keeps that file's permissions.
    std::fs::set_permissions(&gif, std::fs::Permissions::from_mode(0o640)).unwrap();
    make_gif(&[src], &gif, AnimateOptions::default()).unwrap();
    assert_eq!(mode(&gif), 0o640);
}

#[test]
fn skip_policy_returns_before_decoding_when_destination_is_known() {
    use bigimage_core::*;
    let dir = tempfile::tempdir().unwrap();
    // Valid PNG signature, truncated body: decoding it would fail.
    let src = dir.path().join("broken.png");
    let good = std::fs::read(fixture(dir.path(), "good.png", 1)).unwrap();
    std::fs::write(&src, &good[..40]).unwrap();
    assert!(open_image(&src).is_err());
    let skip = OverwritePolicy::Skip;
    let existing = |name: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, b"keep").unwrap();
        path
    };
    let outcomes = [
        (
            existing("broken_crop-1x1+0+0.png"),
            crop_file(&src, CropRect::new(0, 0, 1, 1), None, skip),
        ),
        (
            existing("broken_flipH.jpg"),
            flip_file(&src, FlipAxis::Horizontal, Some(Format::Jpeg), skip),
        ),
        (existing("broken_rot90.png"), rotate_file(&src, Rotation::Deg90, None, skip)),
        (existing("broken_auto.png"), rotate_file_auto(&src, None, skip)),
        (
            existing("broken_b10.png"),
            adjust_file(&src, AdjustOps { brightness: 10, ..AdjustOps::default() }, None, skip),
        ),
        (
            existing("broken_extent-64x64.png"),
            extent_file(
                &src,
                ExtentOptions::new(ExtentSize::Exact { width: 64, height: 64 }),
                None,
                skip,
            ),
        ),
        (
            existing("broken_10.png"),
            resize_file(
                &src,
                ResizeMode::MaxEdge(10),
                Filter::Nearest,
                None,
                &EncodeOptions::default(),
                skip,
            ),
        ),
    ];
    for (path, outcome) in outcomes {
        assert_eq!(outcome.unwrap(), ConvertOutcome::Skipped { output: path.clone() });
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
    }
}

#[test]
fn single_file_operations_honour_caller_encode_options() {
    use bigimage_core::*;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("noise.png");
    let noise = RgbImage::from_fn(64, 64, |x, y| {
        Rgb([(x * 37 + y * 11) as u8, (x * x + y * 7) as u8, (x ^ y) as u8 * 3])
    });
    noise.save(&src).unwrap();
    let size = |quality: u8| {
        let opts = EncodeOptions { quality: Some(quality), ..EncodeOptions::default() };
        let out = dir.path().join(format!("q{quality}"));
        std::fs::create_dir_all(&out).unwrap();
        let input = out.join("noise.png");
        std::fs::copy(&src, &input).unwrap();
        let jpeg = Some(Format::Jpeg);
        let replace = OverwritePolicy::Replace;
        [
            crop_file_with_options(&input, CropRect::new(0, 0, 60, 60), jpeg, &opts, replace),
            flip_file_with_options(&input, FlipAxis::Vertical, jpeg, &opts, replace),
            rotate_file_with_options(&input, Rotation::Deg180, jpeg, &opts, replace),
            rotate_file_auto_with_options(&input, jpeg, &opts, replace),
            adjust_file_with_options(
                &input,
                AdjustOps { contrast: 5.0, ..AdjustOps::default() },
                jpeg,
                &opts,
                replace,
            ),
            extent_file_with_options(
                &input,
                ExtentOptions::new(ExtentSize::Exact { width: 70, height: 70 }),
                jpeg,
                &opts,
                replace,
            ),
        ]
        .map(|outcome| std::fs::metadata(outcome.unwrap().path()).unwrap().len())
    };
    let (low, high) = (size(10), size(95));
    for (low, high) in low.iter().zip(high.iter()) {
        assert!(low < high, "quality ignored: q10={low} q95={high}");
    }
}

/// Depth/alpha policy documented on `save_image` (findings: float to
/// PNG/TIFF without added alpha, extent canvas depth, PNM depth).
#[test]
fn documented_depth_and_alpha_policy_holds() {
    let dir = tempfile::tempdir().unwrap();
    let save = |image: DynamicImage, name: &str, format: Format| {
        let path = dir.path().join(name);
        bigimage_core::save_image(
            image,
            &path,
            format,
            &EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap();
        image::open(&path).unwrap()
    };
    let float = DynamicImage::ImageRgb32F(image::ImageBuffer::from_pixel(
        2,
        2,
        image::Rgb([0.5f32, 0.25, 1.0]),
    ));
    let png = save(float.clone(), "float.png", Format::Png);
    assert_eq!(png.color(), image::ColorType::Rgb16);
    let px = png.to_rgb8().get_pixel(0, 0).0;
    assert!((187..=189).contains(&px[0]), "sRGB transfer missing: {px:?}");
    assert_eq!(save(float, "float.tiff", Format::Tiff).color(), image::ColorType::Rgb16);

    let gray16 =
        DynamicImage::ImageLuma16(image::ImageBuffer::from_pixel(3, 2, image::Luma([40000u16])));
    let rgb16 = DynamicImage::ImageRgb16(image::ImageBuffer::from_pixel(
        3,
        2,
        image::Rgb([1000u16, 2000, 3000]),
    ));
    let gray8 = DynamicImage::ImageLuma8(image::ImageBuffer::from_pixel(3, 2, image::Luma([77u8])));
    assert_eq!(save(gray16.clone(), "g16.pgm", Format::Pnm), gray16);
    assert_eq!(save(rgb16.clone(), "c16.ppm", Format::Pnm), rgb16);
    assert_eq!(save(gray8.clone(), "g8.pgm", Format::Pnm), gray8);

    let extent = ExtentOptions::new(ExtentSize::Exact { width: 5, height: 4 });
    let extended = bigimage_core::extent::apply_to(&rgb16, &extent).unwrap();
    assert_eq!(save(extended, "extent.png", Format::Png).color(), image::ColorType::Rgb16);
    let extended = bigimage_core::extent::apply_to(&gray16, &extent).unwrap();
    assert_eq!(save(extended, "extent-gray.png", Format::Png).color(), image::ColorType::L16);
}

/// Append an `irot` property to `ipco`, associate it with the single item
/// in `ipma` (as the encoder writes it), grow the enclosing box sizes and
/// shift the `iloc` offsets that point past the insertion.
fn add_primary_irot(file: &[u8], angle: u8) -> Vec<u8> {
    fn find(bytes: &[u8], kind: &[u8; 4]) -> usize {
        bytes.windows(4).position(|w| w == kind).expect("box present") - 4
    }
    fn size(bytes: &[u8], at: usize) -> usize {
        u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
    }
    fn grow(bytes: &mut [u8], at: usize, by: usize) {
        let new = (size(bytes, at) + by) as u32;
        bytes[at..at + 4].copy_from_slice(&new.to_be_bytes());
    }
    let mut out = file.to_vec();
    let (meta, iprp, ipco, ipma) =
        (find(file, b"meta"), find(file, b"iprp"), find(file, b"ipco"), find(file, b"ipma"));
    assert!(ipco < ipma, "encoder layout changed");
    // ipma v0, flags 0: entry_count(4) item_ID(2) count(1) assoc(1)...
    assert_eq!(&file[ipma + 12..ipma + 16], &[0, 0, 0, 1]);
    // The new property's 1-based index is the current child count + 1.
    let mut count = 0;
    let mut p = ipco + 8;
    while p < ipco + size(file, ipco) {
        count += 1;
        p += size(file, p);
    }
    let ipma_end = ipma + size(file, ipma);
    out.insert(ipma_end, 0x80 | (count + 1) as u8);
    out[ipma + 18] += 1;
    grow(&mut out, ipma, 1);
    let ipco_end = ipco + size(file, ipco);
    let irot = [0, 0, 0, 9, b'i', b'r', b'o', b't', angle];
    out.splice(ipco_end..ipco_end, irot);
    grow(&mut out, ipco, 9);
    grow(&mut out, iprp, 10);
    grow(&mut out, meta, 10);
    // iloc v0: offset/length sizes, base_offset size, item_count(2), then
    // per item: id(2) data_ref(2) base_offset(n) extent_count(2) extents.
    let iloc = find(&out, b"iloc");
    assert!(iloc < ipco, "iloc must precede the insertion");
    assert_eq!(out[iloc + 8], 0, "iloc version");
    let (offset_size, length_size) =
        ((out[iloc + 12] >> 4) as usize, (out[iloc + 12] & 15) as usize);
    let base_size = (out[iloc + 13] >> 4) as usize;
    let items = u16::from_be_bytes([out[iloc + 14], out[iloc + 15]]);
    let mut cursor = iloc + 16;
    let bump = |bytes: &mut [u8], at: usize, n: usize| {
        if n == 0 {
            return;
        }
        let mut value = 0u64;
        for b in &bytes[at..at + n] {
            value = value << 8 | u64::from(*b);
        }
        value += 10;
        for i in 0..n {
            bytes[at + n - 1 - i] = (value >> (8 * i)) as u8;
        }
    };
    for _ in 0..items {
        cursor += 4;
        bump(&mut out, cursor, base_size);
        cursor += base_size;
        let extents = u16::from_be_bytes([out[cursor], out[cursor + 1]]);
        cursor += 2;
        for _ in 0..extents {
            if base_size == 0 {
                bump(&mut out, cursor, offset_size);
            }
            cursor += offset_size + length_size;
        }
    }
    out
}

#[test]
fn avif_irot_property_of_primary_item_is_applied_on_decode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rotated.avif");
    // 4x2: left half red, right half blue.
    let source =
        RgbImage::from_fn(4, 2, |x, _| if x < 2 { Rgb([255, 0, 0]) } else { Rgb([0, 0, 255]) });
    bigimage_core::save_image(
        DynamicImage::ImageRgb8(source),
        &path,
        Format::Avif,
        &EncodeOptions { quality: Some(100), ..EncodeOptions::default() },
        OverwritePolicy::Replace,
    )
    .unwrap();
    let bytes = add_primary_irot(&std::fs::read(&path).unwrap(), 1);
    std::fs::write(&path, &bytes).unwrap();

    let decoded = open_image(&path).unwrap().to_rgb8();
    assert_eq!(decoded.dimensions(), (2, 4));
    // Anticlockwise: the right (blue) half ends up on top.
    let top = decoded.get_pixel(0, 0).0;
    let bottom = decoded.get_pixel(0, 3).0;
    assert!(top[2] > 200 && top[0] < 60, "top {top:?}");
    assert!(bottom[0] > 200 && bottom[2] < 60, "bottom {bottom:?}");
}
