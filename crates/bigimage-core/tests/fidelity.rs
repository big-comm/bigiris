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

/// Fuzzer-found AVIF whose AV1 stream never yields a picture. image 0.25.10
/// spun forever in `AvifDecoder::new`; the vendored fix must fail promptly.
/// Runs on a worker thread so a regression fails instead of hanging the suite.
#[test]
fn avif_without_decodable_picture_terminates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("no-picture.avif");
    std::fs::write(&path, include_bytes!("fixtures/avif-no-picture.avif")).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let dimensions = bigimage_core::backend::dimensions_in_process(&path).is_err();
        let decode = bigimage_core::backend::decode_in_process(&path).is_err();
        let _ = tx.send((dimensions, decode));
    });
    let (dimensions_err, decode_err) = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("AVIF decoder did not terminate within 30 s");
    assert!(dimensions_err && decode_err);
}

/// TIFF EXIF: Make = "Cam", DateTimeOriginal-like DateTime, Orientation = 6.
fn camera_exif() -> Vec<u8> {
    let mut exif = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 3, 0];
    exif.extend_from_slice(&[0x0f, 0x01, 2, 0, 4, 0, 0, 0, b'C', b'a', b'm', 0]);
    // DateTime (0x0132), ASCII, 20 bytes at offset 50.
    exif.extend_from_slice(&[0x32, 0x01, 2, 0, 20, 0, 0, 0, 50, 0, 0, 0]);
    exif.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]);
    exif.extend_from_slice(&[0, 0, 0, 0]);
    exif.extend_from_slice(b"2024:05:06 07:08:09\0");
    exif
}

fn icc_of(path: &std::path::Path) -> Option<Vec<u8>> {
    use image::ImageDecoder;
    image::ImageReader::open(path)
        .unwrap()
        .with_guessed_format()
        .unwrap()
        .into_decoder()
        .unwrap()
        .icc_profile()
        .unwrap()
}

#[test]
fn in_place_save_keeps_exif_and_wide_gamut_profile() {
    use bigimage_core::{open_image_for_resave, save_image_preserving_metadata};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("photo.jpg");
    let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
    // Stored sideways (Orientation 6): 4 wide x 2 high on disk.
    let stored = image::RgbImage::from_pixel(4, 2, image::Rgb([200, 60, 30]));
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
        std::fs::File::create(&path).unwrap(),
        95,
    );
    encoder.set_icc_profile(p3.clone()).unwrap();
    encoder.set_exif_metadata(camera_exif()).unwrap();
    encoder.write_image(stored.as_raw(), 4, 2, image::ExtendedColorType::Rgb8).unwrap();

    let (image, source) = open_image_for_resave(&path).unwrap();
    assert_eq!((image.width(), image.height()), (2, 4), "upright");
    assert_eq!(source.icc.as_deref(), Some(&p3[..]));
    // Pixels stay in the P3 space (no sRGB conversion before the edit).
    let px = image.to_rgb8().get_pixel(0, 0).0;
    assert!(px[0].abs_diff(200) <= 3 && px[1].abs_diff(60) <= 3, "{px:?}");

    let edited = image.fliph();
    save_image_preserving_metadata(
        edited,
        &path,
        Format::Jpeg,
        &EncodeOptions { quality: Some(95), ..EncodeOptions::default() },
        OverwritePolicy::Replace,
        &source,
    )
    .unwrap();

    let meta = bigimage_core::metadata::read(&path).unwrap();
    assert!(meta.has_camera_info, "Make lost: {:?}", meta.tags);
    assert!(meta.tags.iter().any(|(_, v)| v.contains("2024-05-06")), "{:?}", meta.tags);
    assert_eq!(meta.orientation, Some(1), "pixels are upright, tag must say so");
    assert_eq!(icc_of(&path).as_deref(), Some(&p3[..]));
    // Reopening does not rotate a second time.
    let reopened = open_image(&path).unwrap();
    assert_eq!((reopened.width(), reopened.height()), (2, 4));
}

#[test]
fn preserving_save_matches_profile_to_pixels_or_falls_back_to_srgb() {
    use bigimage_core::{metadata, save_image_preserving_metadata, SourceMetadata};
    let dir = tempfile::tempdir().unwrap();
    let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
    let gray = moxcms::ColorProfile::new_gray_with_gamma(1.8).encode().unwrap();
    let save = |image: DynamicImage, name: &str, format: Format, icc: &[u8]| {
        let path = dir.path().join(name);
        let source = SourceMetadata { exif: Some(camera_exif()), icc: Some(icc.to_vec()) };
        save_image_preserving_metadata(
            image,
            &path,
            format,
            &EncodeOptions::default(),
            OverwritePolicy::Replace,
            &source,
        )
        .unwrap();
        path
    };
    let rgb16 = DynamicImage::ImageRgb16(image::ImageBuffer::from_pixel(
        3,
        3,
        image::Rgb([40000u16, 20000, 10000]),
    ));
    // 16-bit PNG: profile and exact samples kept, EXIF written.
    let png = save(rgb16.clone(), "deep.png", Format::Png, &p3);
    assert_eq!(icc_of(&png).as_deref(), Some(&p3[..]));
    let (reopened, meta) = bigimage_core::open_image_for_resave(&png).unwrap();
    assert_eq!(reopened, rgb16);
    assert_eq!(meta.exif.as_deref().and_then(|e| e.get(..2)), Some(&b"II"[..]));

    // Gray profile on gray pixels is embedded as-is.
    let luma = DynamicImage::ImageLuma8(image::ImageBuffer::from_pixel(2, 2, image::Luma([90u8])));
    assert_eq!(icc_of(&save(luma, "gray.png", Format::Png, &gray)).as_deref(), Some(&gray[..]));

    // A profile that does not describe the pixels (CMYK on RGB) is not
    // embedded; the canonical sRGB one is.
    let mut cmyk = p3.clone();
    cmyk[16..20].copy_from_slice(b"CMYK");
    let out = save(rgb16.clone(), "cmyk.png", Format::Png, &cmyk);
    assert_ne!(icc_of(&out).as_deref(), Some(&cmyk[..]));
    assert_eq!(open_image(&out).unwrap().as_rgb16(), rgb16.as_rgb16());

    // WebP keeps both; formats without ICC support get sRGB pixels.
    let webp = save(rgb16.clone(), "x.webp", Format::WebP, &p3);
    assert_eq!(icc_of(&webp).as_deref(), Some(&p3[..]));
    assert!(metadata::read(&webp).unwrap().has_camera_info);
    let bmp = save(rgb16.clone(), "x.bmp", Format::Bmp, &p3);
    let srgb = open_image(&bmp).unwrap().to_rgb8().get_pixel(0, 0).0;
    let raw = rgb16.to_rgb8().get_pixel(0, 0).0;
    assert_ne!(srgb, raw, "P3 pixels must be converted when the profile cannot be kept");
    assert!(!metadata::preserved_by(Format::Bmp).exif);
    assert!(metadata::preserved_by(Format::Jpeg).is_complete());
}

#[test]
fn preserving_save_reports_what_it_dropped() {
    use bigimage_core::{save_image_preserving_metadata_report, MetadataLoss, SourceMetadata};
    let dir = tempfile::tempdir().unwrap();
    let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
    let gray = moxcms::ColorProfile::new_gray_with_gamma(1.8).encode().unwrap();
    let mut cmyk = p3.clone();
    cmyk[16..20].copy_from_slice(b"CMYK");
    let rgb = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 3, image::Rgb([90, 90, 90])));
    let report = |name: &str, format: Format, exif: Option<Vec<u8>>, icc: &[u8]| {
        let source = SourceMetadata { exif, icc: Some(icc.to_vec()) };
        let result = save_image_preserving_metadata_report(
            rgb.clone(),
            dir.path().join(name),
            format,
            &EncodeOptions::default(),
            OverwritePolicy::Replace,
            &source,
        )
        .unwrap();
        assert!(matches!(result.outcome, bigimage_core::ConvertOutcome::Written { .. }));
        result.dropped
    };
    assert_eq!(report("all.jpg", Format::Jpeg, Some(camera_exif()), &p3), []);
    assert_eq!(
        report("none.bmp", Format::Bmp, Some(camera_exif()), &p3),
        [MetadataLoss::IccUnsupported, MetadataLoss::ExifUnsupported]
    );
    // An edit made gray pixels RGB: the GRAY profile no longer fits.
    let out = dir.path().join("mismatch.png");
    assert_eq!(report("mismatch.png", Format::Png, None, &gray), [MetadataLoss::IccMismatched]);
    let px = open_image(&out).unwrap().to_rgb8().get_pixel(0, 0).0;
    assert!(px[0] > 90 && px[0] == px[1] && px[1] == px[2], "gamma 1.8 gray → sRGB: {px:?}");
    assert_eq!(report("cmyk.png", Format::Png, None, &cmyk), [MetadataLoss::IccDiscarded]);
    assert_eq!(
        report("bad.png", Format::Png, Some(b"not exif".to_vec()), &p3),
        [MetadataLoss::ExifInvalid]
    );
    let mut huge = camera_exif();
    huge.resize(70_000, 0);
    assert_eq!(
        report("huge.jpg", Format::Jpeg, Some(huge.clone()), &p3),
        [MetadataLoss::ExifTooLarge]
    );
    assert_eq!(report("huge.png", Format::Png, Some(huge), &p3), []);
}
