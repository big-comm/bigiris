// SPDX-License-Identifier: GPL-3.0-or-later
use bigimage_core::{convert_file_to, open_image, EncodeOptions, Format, OverwritePolicy};
use image::{DynamicImage, ImageEncoder};

#[test]
fn display_p3_is_normalized_and_output_srgb_does_not_round_again() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("p3.png");
    let output = dir.path().join("normalized");
    let original = DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
        8,
        8,
        image::Rgba([40000, 20000, 10000, 12345]),
    ));
    let mut encoder = image::codecs::png::PngEncoder::new(std::fs::File::create(&input).unwrap());
    encoder.set_icc_profile(moxcms::ColorProfile::new_display_p3().encode().unwrap()).unwrap();
    encoder.write_image(original.as_bytes(), 8, 8, original.color().into()).unwrap();
    let normalized = open_image(&input).unwrap();
    assert_ne!(&normalized.as_rgba16().unwrap().get_pixel(0, 0).0[..3], &[40000, 20000, 10000]);
    assert_eq!(normalized.as_rgba16().unwrap().get_pixel(0, 0).0[3], 12345);
    let written = convert_file_to(
        &input,
        Some(&output),
        Format::Png,
        &EncodeOptions::default(),
        OverwritePolicy::Replace,
    )
    .unwrap();
    assert_eq!(open_image(written.path()).unwrap().as_rgba16(), normalized.as_rgba16());
}

#[test]
fn all_eight_embedded_exif_orientations_are_applied_once() {
    let dir = tempfile::tempdir().unwrap();
    let image = DynamicImage::ImageRgb8(image::RgbImage::from_fn(3, 2, |x, y| {
        let v = (40 + x * 25 + y * 100) as u8;
        image::Rgb([v, v, v])
    }));
    let mut base = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut base, 100)
        .encode_image(&image)
        .unwrap();
    let raw = image::load_from_memory(&base).unwrap().to_rgb8();
    for orientation in 1..=8u8 {
        let mut exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
        exif.extend([orientation, 0, 0, 0, 0, 0, 0, 0]);
        let mut bytes = base[..2].to_vec();
        bytes.extend([0xff, 0xe1]);
        bytes.extend(((exif.len() + 2) as u16).to_be_bytes());
        bytes.extend(exif);
        bytes.extend(&base[2..]);
        let source = dir.path().join(format!("orientation-{orientation}.jpg"));
        std::fs::write(&source, bytes).unwrap();
        let upright = open_image(&source).unwrap().to_rgb8();
        assert_eq!(upright.dimensions(), if orientation >= 5 { (2, 3) } else { (3, 2) });
        for y in 0..2 {
            for x in 0..3 {
                let (dx, dy) = match orientation {
                    1 => (x, y),
                    2 => (2 - x, y),
                    3 => (2 - x, 1 - y),
                    4 => (x, 1 - y),
                    5 => (y, x),
                    6 => (1 - y, x),
                    7 => (1 - y, 2 - x),
                    8 => (y, 2 - x),
                    _ => unreachable!(),
                };
                assert_eq!(
                    upright.get_pixel(dx, dy),
                    raw.get_pixel(x, y),
                    "orientation {orientation}"
                );
            }
        }
        let output = dir.path().join(format!("normalized-{orientation}.png"));
        let written = convert_file_to(
            &source,
            Some(&output),
            Format::Png,
            &EncodeOptions::default(),
            OverwritePolicy::Replace,
        )
        .unwrap();
        assert_eq!(open_image(written.path()).unwrap().to_rgb8(), upright);
    }
}

#[test]
fn static_edits_refuse_animated_gif_and_preserve_destination() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("animated.gif");
    let output = dir.path().join("keep.png");
    let mut encoder = image::codecs::gif::GifEncoder::new(std::fs::File::create(&input).unwrap());
    for v in [50, 150] {
        encoder
            .encode_frame(image::Frame::new(image::RgbaImage::from_pixel(
                2,
                2,
                image::Rgba([v, 0, 0, 255]),
            )))
            .unwrap();
    }
    drop(encoder);
    assert!(open_image(&input).is_ok(), "viewer can still inspect the first frame");
    std::fs::write(&output, b"previous file").unwrap();
    assert!(convert_file_to(
        &input,
        Some(&output),
        Format::Png,
        &EncodeOptions::default(),
        OverwritePolicy::Replace
    )
    .is_err());
    assert_eq!(std::fs::read(output).unwrap(), b"previous file");
}

#[test]
fn preview_huge_resize_and_canvas_remain_bounded() {
    use bigimage_core::{ExtentOptions, ExtentSize, Filter, PreviewOp, PreviewSession, ResizeMode};
    let source = DynamicImage::new_rgb8(800, 600);
    let session =
        PreviewSession::from_cached("source.png".into(), (800, 600), Format::Png, source, false)
            .unwrap();
    for op in [
        PreviewOp::Resize {
            mode: ResizeMode::Exact { width: u32::MAX, height: u32::MAX },
            filter: Filter::Lanczos3,
        },
        PreviewOp::Extent(ExtentOptions {
            size: ExtentSize::Exact { width: u32::MAX, height: u32::MAX },
            ..ExtentOptions::new(ExtentSize::Exact { width: 800, height: 600 })
        }),
    ] {
        let result = session.apply(&op).unwrap();
        assert!(result.width() <= 800 && result.height() <= 800);
    }
}

#[test]
fn oversized_gif_frame_is_rejected_without_allocating_its_pixels() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("malformed.gif");
    let bytes = [
        71, 73, 70, 56, 57, 97, 1, 0, 1, 0, 128, 0, 0, 0, 0, 0, 255, 255, 255, 33, 249, 4, 1, 0, 0,
        0, 0, 44, 0, 0, 137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 16,
        0, 7, 0, 0, 0, 0, 0, 68, 1, 0,
    ];
    std::fs::write(&path, bytes).unwrap();
    assert!(open_image(&path).is_err());
    assert!(bigimage_core::content::ensure_static_local(&path).is_err());
}

#[test]
fn avif_declared_box_beyond_file_is_rejected_before_allocation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("malformed.avif");
    std::fs::write(&path, include_bytes!("fixtures/avif-box-overrun.avif")).unwrap();
    assert!(open_image(&path).is_err());
    assert!(bigimage_core::image_dimensions(&path).is_err());
}
