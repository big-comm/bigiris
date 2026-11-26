// SPDX-License-Identifier: GPL-3.0-or-later
//! Runs every model on synthetic images. Ignored by default: they need the
//! weights of the BigLinux packages, installed or pointed at with
//! `BIGIRIS_{BIREFNET,REALESRGAN,SCUNET,LAMA}_WEIGHTS`, and print the time,
//! device and peak memory of each run:
//!
//! ```bash
//! BIGIRIS_BIREFNET_WEIGHTS=… BIGIRIS_REALESRGAN_WEIGHTS=… \
//! BIGIRIS_SCUNET_WEIGHTS=… BIGIRIS_LAMA_WEIGHTS=… \
//!   cargo test --release -p bigimage-ai --features native --test models -- \
//!   --ignored --test-threads=1 --nocapture
//! ```
//!
//! A test whose weights are missing prints why and passes, so the suite
//! can run on machines with only some packages.
#![cfg(feature = "native")]

use std::ops::ControlFlow;
use std::time::Instant;

use bigimage_ai::{ModelInfo, Stage, background, denoise, inpaint, upscale};
use image::{DynamicImage, GrayImage, Rgb, RgbImage, Rgba, RgbaImage};

fn ready(model: &ModelInfo) -> bool {
    let availability = model.availability();
    if !availability.is_ready() {
        eprintln!("SKIP {}: {availability:?} (set {})", model.name, model.env);
    }
    availability.is_ready()
}

fn peak_mib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmHWM:"))
                .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        })
        .unwrap_or(0)
        / 1024
}

fn report(what: &str, started: Instant, device: Option<bigimage_ai::Device>) {
    eprintln!(
        "{what}: {:.2} s on {}, process peak {} MiB",
        started.elapsed().as_secs_f64(),
        device.map_or_else(|| "?".into(), |d| d.to_string()),
        peak_mib()
    );
}

fn go(_: Stage) -> ControlFlow<()> {
    ControlFlow::Continue(())
}

/// A smooth two-colour gradient.
fn backdrop(width: u32, height: u32) -> RgbImage {
    RgbImage::from_fn(width, height, |x, y| {
        let t = x as f32 / width as f32;
        let s = y as f32 / height as f32;
        Rgb([(90.0 + 60.0 * t) as u8, (140.0 + 40.0 * s) as u8, (200.0 - 30.0 * t) as u8])
    })
}

fn psnr(a: &RgbImage, b: &RgbImage) -> f64 {
    let mse: f64 = a
        .as_raw()
        .iter()
        .zip(b.as_raw())
        .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
        .sum::<f64>()
        / a.as_raw().len() as f64;
    10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10()
}

/// Deterministic noise in −spread..=spread.
fn noise(seed: &mut u64, spread: i32) -> i32 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    (*seed % (2 * spread as u64 + 1)) as i32 - spread
}

#[test]
#[ignore = "needs birefnet-native weights"]
fn birefnet_separates_a_subject_from_a_flat_background() {
    if !ready(&background::MODEL) {
        return;
    }
    // A dark-red ball, lit from the upper left, on a light gray wall.
    let (w, h) = (800u32, 600u32);
    let (cx, cy, r) = (400.0f32, 320.0f32, 170.0f32);
    let image = RgbImage::from_fn(w, h, |x, y| {
        let (dx, dy) = (x as f32 - cx, y as f32 - cy);
        if dx * dx + dy * dy <= r * r {
            let light = 1.0 - ((dx + 60.0).powi(2) + (dy + 60.0).powi(2)).sqrt() / (2.2 * r);
            Rgb([(80.0 + 170.0 * light) as u8, (20.0 + 40.0 * light) as u8, 25])
        } else {
            Rgb([222, 222, 218])
        }
    });
    let started = Instant::now();
    let out = background::remove_background_with_progress(&DynamicImage::ImageRgb8(image), go)
        .unwrap()
        .into_rgba8();
    report("birefnet 800x600", started, background::device());
    assert_eq!(out.dimensions(), (w, h));
    let center = out.get_pixel(cx as u32, cy as u32)[3];
    let corners =
        [(5, 5), (w - 5, 5), (5, h - 5), (w - 5, h - 5)].map(|(x, y)| out.get_pixel(x, y)[3]);
    assert!(center > 200, "subject alpha {center}");
    assert!(corners.iter().all(|&a| a < 40), "background alpha {corners:?}");
    // Colour is kept.
    assert_eq!(out.get_pixel(5, 5).0[..3], [222, 222, 218]);

    // Second run reuses the loaded model.
    let started = Instant::now();
    let again = background::remove_background(&DynamicImage::ImageRgb8(RgbImage::from_pixel(
        64,
        64,
        Rgb([1, 2, 3]),
    )))
    .unwrap();
    report("birefnet 64x64 (loaded)", started, background::device());
    assert_eq!((again.width(), again.height()), (64, 64));
}

#[test]
#[ignore = "needs realesrgan-native weights"]
fn realesrgan_enlarges_by_each_factor_and_keeps_alpha() {
    if !ready(&upscale::MODEL) {
        return;
    }
    let (w, h) = (384u32, 256u32);
    let rgb = backdrop(w, h);
    let rgba = RgbaImage::from_fn(w, h, |x, y| {
        let p = rgb.get_pixel(x, y);
        Rgba([p[0], p[1], p[2], if x < w / 2 { 255 } else { 0 }])
    });
    let started = Instant::now();
    let four =
        upscale::upscale(&DynamicImage::ImageRgb8(rgb.clone()), 4, upscale::Method::RealEsrgan, go)
            .unwrap();
    report("realesrgan 384x256 x4", started, upscale::device());
    assert!(matches!(four, DynamicImage::ImageRgb8(_)));
    assert_eq!((four.width(), four.height()), (w * 4, h * 4));
    // A smooth gradient stays close to its Lanczos enlargement.
    let lanczos = upscale::lanczos(&DynamicImage::ImageRgb8(rgb), 4).unwrap().into_rgb8();
    let quality = psnr(&four.into_rgb8(), &lanczos);
    assert!(quality > 30.0, "PSNR vs Lanczos {quality:.1} dB");

    for factor in [2u8, 3] {
        let out = upscale::upscale(
            &DynamicImage::ImageRgba8(rgba.clone()),
            factor,
            upscale::Method::RealEsrgan,
            go,
        )
        .unwrap();
        let DynamicImage::ImageRgba8(out) = out else { panic!("alpha lost") };
        let f = u32::from(factor);
        assert_eq!(out.dimensions(), (w * f, h * f));
        assert_eq!(out.get_pixel(2, 2)[3], 255);
        assert_eq!(out.get_pixel(w * f - 3, h * f - 3)[3], 0);
    }

    // Cancelling stops the run.
    let mut calls = 0;
    let cancelled = upscale::upscale(
        &DynamicImage::ImageRgb8(backdrop(w, h)),
        4,
        upscale::Method::RealEsrgan,
        |stage| {
            if matches!(stage, Stage::Running(_)) {
                calls += 1;
            }
            if calls >= 2 { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
        },
    );
    assert!(matches!(cancelled, Err(bigimage_ai::AiError::Cancelled)), "{cancelled:?}");
}

#[test]
#[ignore = "needs realesrgan-native weights"]
fn realesrgan_bands_match_whole_reduction() {
    if !ready(&upscale::MODEL) {
        return;
    }
    // 1000 rows run as three bands; the oracle is Lanczos3 over the 4× run.
    let (w, h) = (160u32, 1000u32);
    let mut seed = 0x51_7cc1_b727_220au64;
    let clean = backdrop(w, h);
    let textured = RgbImage::from_fn(w, h, |x, y| {
        let p = clean.get_pixel(x, y);
        Rgb(p.0.map(|v| (i32::from(v) + noise(&mut seed, 20)).clamp(0, 255) as u8))
    });
    let source = DynamicImage::ImageRgb8(textured);
    let started = Instant::now();
    let four = upscale::upscale(&source, 4, upscale::Method::RealEsrgan, go).unwrap();
    let two = upscale::upscale(&source, 2, upscale::Method::RealEsrgan, go).unwrap().into_rgb8();
    report("realesrgan 160x1000 x4 and x2", started, upscale::device());
    let reference = bigimage_core::resize::apply_to(
        &four,
        bigimage_core::ResizeMode::Exact { width: w * 2, height: h * 2 },
        bigimage_core::Filter::Lanczos3,
    )
    .unwrap()
    .into_rgb8();
    let worst =
        two.as_raw().iter().zip(reference.as_raw()).map(|(&a, &b)| a.abs_diff(b)).max().unwrap();
    assert!(worst <= 2, "largest difference {worst}");
}

#[test]
#[ignore = "needs scunet-native weights"]
fn scunet_removes_added_noise() {
    if !ready(&denoise::MODEL) {
        return;
    }
    let (w, h) = (768u32, 512u32);
    let clean = backdrop(w, h);
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let noisy = RgbImage::from_fn(w, h, |x, y| {
        let p = clean.get_pixel(x, y);
        Rgb(p.0.map(|v| (i32::from(v) + noise(&mut seed, 25)).clamp(0, 255) as u8))
    });
    let mut shares = Vec::new();
    let started = Instant::now();
    let out = denoise::denoise(&DynamicImage::ImageRgb8(noisy.clone()), |stage| {
        if let Stage::Running(share) = stage {
            shares.push(share);
        }
        ControlFlow::Continue(())
    })
    .unwrap();
    report("scunet 768x512", started, denoise::device());
    let out = out.into_rgb8();
    assert_eq!(out.dimensions(), (w, h));
    let before = psnr(&noisy, &clean);
    let after = psnr(&out, &clean);
    eprintln!("scunet PSNR {before:.1} dB -> {after:.1} dB");
    assert!(after > before + 6.0, "{before:.1} -> {after:.1}");
    assert!(shares.len() > 5 && shares.windows(2).all(|p| p[0] <= p[1]), "{shares:?}");

    // Alpha comes back untouched.
    let rgba = DynamicImage::ImageRgba8(RgbaImage::from_fn(64, 48, |x, y| {
        Rgba([x as u8 * 3, y as u8 * 4, 90, (x * 4) as u8])
    }));
    let out = denoise::denoise(&rgba, go).unwrap();
    let DynamicImage::ImageRgba8(out) = out else { panic!("alpha lost") };
    assert!(out.pixels().zip(rgba.to_rgba8().pixels()).all(|(a, b)| a[3] == b[3]));
}

#[test]
#[ignore = "needs lama-native weights"]
fn lama_fills_a_masked_square_from_its_surroundings() {
    if !ready(&inpaint::MODEL) {
        return;
    }
    let (w, h) = (1200u32, 900u32);
    let clean = backdrop(w, h);
    // A black box pasted on the gradient, masked for removal.
    let (bx, by, bs) = (500u32, 380u32, 120u32);
    let mut dirty = clean.clone();
    let mut mask = GrayImage::new(w, h);
    for y in by..by + bs {
        for x in bx..bx + bs {
            dirty.put_pixel(x, y, Rgb([10, 10, 10]));
            mask.put_pixel(x, y, image::Luma([255]));
        }
    }
    let plan = inpaint::plans(&mask).unwrap()[0];
    eprintln!("lama plan {plan:?}");
    let started = Instant::now();
    let out = inpaint::inpaint(&DynamicImage::ImageRgb8(dirty.clone()), &mask, go).unwrap();
    report("lama 1200x900 box", started, inpaint::device());
    let out = out.into_rgb8();
    assert_eq!(out.dimensions(), (w, h));
    let mut diff = 0.0;
    for y in by..by + bs {
        for x in bx..bx + bs {
            let (a, b) = (out.get_pixel(x, y), clean.get_pixel(x, y));
            diff += (0..3).map(|c| (f64::from(a[c]) - f64::from(b[c])).abs()).sum::<f64>() / 3.0;
        }
    }
    let mean = diff / f64::from(bs * bs);
    eprintln!("lama mean difference inside the hole: {mean:.1}");
    assert!(mean < 12.0, "fill far from the surroundings: {mean:.1}");
    // Far from the hole nothing changes.
    for (x, y) in [(0, 0), (w - 1, h - 1), (100, 800), (bx + bs + 40, by)] {
        assert_eq!(out.get_pixel(x, y), dirty.get_pixel(x, y), "({x},{y})");
    }

    // A crop larger than LaMa's run side is scaled down and back.
    let mut wide = GrayImage::new(w, h);
    for y in 100..800 {
        for x in 100..1100 {
            wide.put_pixel(x, y, image::Luma([255]));
        }
    }
    let plan = inpaint::plans(&wide).unwrap()[0];
    assert!(plan.run.0 < plan.crop.width, "{plan:?}");
    let started = Instant::now();
    let out = inpaint::inpaint(&DynamicImage::ImageRgb8(clean.clone()), &wide, go).unwrap();
    report("lama 1200x900 scaled", started, inpaint::device());
    assert_eq!((out.width(), out.height()), (w, h));

    // Two distant marks fill as two crops at full resolution, and every
    // pixel beyond the grown holes keeps its value.
    let marks = [(60u32, 50u32), (1100u32, 820u32)];
    let mut dirty = clean;
    let mut mask = GrayImage::new(w, h);
    for (mx, my) in marks {
        for y in my..my + 30 {
            for x in mx..mx + 30 {
                dirty.put_pixel(x, y, Rgb([10, 10, 10]));
                mask.put_pixel(x, y, image::Luma([255]));
            }
        }
    }
    let plans = inpaint::plans(&mask).unwrap();
    assert_eq!(plans.len(), 2, "{plans:?}");
    assert!(plans.iter().all(|p| p.run == (p.crop.width, p.crop.height)), "{plans:?}");
    let started = Instant::now();
    let out = inpaint::inpaint(&DynamicImage::ImageRgb8(dirty.clone()), &mask, go).unwrap();
    report("lama 1200x900 two marks", started, inpaint::device());
    let out = out.into_rgb8();
    let ring = 2 * plans.iter().map(|p| p.feather).max().unwrap();
    for (x, y, pixel) in out.enumerate_pixels() {
        let near = marks.iter().any(|&(mx, my)| {
            x + ring >= mx && x < mx + 30 + ring && y + ring >= my && y < my + 30 + ring
        });
        if near {
            assert_ne!(pixel.0, [10, 10, 10], "({x},{y}) not filled");
        } else {
            assert_eq!(pixel, dirty.get_pixel(x, y), "({x},{y})");
        }
    }
}
