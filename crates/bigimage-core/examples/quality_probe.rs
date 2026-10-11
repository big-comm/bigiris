// SPDX-License-Identifier: GPL-3.0-or-later
//! Reproducible, synthetic-only performance and stability probe.
//! cargo run --release -p bigimage-core --example quality_probe -- benchmark 10
//! cargo run --release -p bigimage-core --example quality_probe -- soak 300
use std::path::Path;
use std::time::{Duration, Instant};

use bigimage_core::{
    convert_file_to, open_image, EncodeOptions, Filter, Format, OverwritePolicy, PreviewOp,
    PreviewSession, ResizeMode,
};

fn metric(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix(field)
                    .and_then(|v| v.split_whitespace().next())
                    .and_then(|v| v.parse().ok())
            })
        })
        .unwrap_or(0)
}
fn descriptors() -> usize {
    std::fs::read_dir("/proc/self/fd").map(|entries| entries.count()).unwrap_or(0)
}
fn cycle(source: &Path, output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let image = open_image(source)?;
    let resized =
        bigimage_core::resize::apply_to(&image, ResizeMode::MaxEdge(1080), Filter::Lanczos3)?;
    std::hint::black_box(resized);
    let session = PreviewSession::open(source)?;
    std::hint::black_box(session.apply(&PreviewOp::Identity)?);
    convert_file_to(
        source,
        Some(output),
        Format::Jpeg,
        &EncodeOptions::default(),
        OverwritePolicy::Replace,
    )?;
    Ok(())
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("benchmark");
    let count: u64 = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(10);
    if count == 0
        || count > if mode == "soak" { 86400 } else { 3600 }
        || !["benchmark", "stress", "soak"].contains(&mode)
    {
        return Err("use benchmark <amostras>, stress <jobs> ou soak <segundos>, opcionalmente WIDTHxHEIGHT".into());
    }
    let size = args.get(3).map(String::as_str).unwrap_or("1920x1080");
    let (width, height) = size.split_once('x').ok_or("size must be WIDTHxHEIGHT")?;
    let (width, height): (u32, u32) = (width.parse()?, height.parse()?);
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return Err("probe dimensions must be within 1..4096".into());
    }
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("synthetic.png");
    let output = dir.path().join("out");
    let image = image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x.wrapping_mul(13) ^ y.wrapping_mul(7)) as u8, (x / 8) as u8, (y / 4) as u8])
    });
    image.save(&source)?;
    drop(image);
    for _ in 0..5 {
        cycle(&source, &output)?;
    }
    if mode == "benchmark" {
        let mut records = Vec::new();
        for name in [
            "decode_png",
            "resize_1080",
            "preview_800",
            "encode_png",
            "encode_jpeg",
            "encode_webp",
            "encode_avif",
        ] {
            let mut samples = Vec::new();
            for _ in 0..count {
                let start = Instant::now();
                match name {
                    "decode_png" => {
                        std::hint::black_box(open_image(&source)?);
                    }
                    "resize_1080" => {
                        let img = open_image(&source)?;
                        std::hint::black_box(bigimage_core::resize::apply_to(
                            &img,
                            ResizeMode::MaxEdge(1080),
                            Filter::Lanczos3,
                        )?);
                    }
                    "preview_800" => {
                        std::hint::black_box(PreviewSession::open(&source)?);
                    }
                    _ => {
                        let format = match name {
                            "encode_png" => Format::Png,
                            "encode_jpeg" => Format::Jpeg,
                            "encode_webp" => Format::WebP,
                            _ => Format::Avif,
                        };
                        convert_file_to(
                            &source,
                            Some(&output),
                            format,
                            &EncodeOptions::default(),
                            OverwritePolicy::Replace,
                        )?;
                    }
                }
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            samples.sort_by(f64::total_cmp);
            let p95 =
                samples[((samples.len() as f64 * 0.95).ceil() as usize - 1).min(samples.len() - 1)];
            records.push(format!("{{\"operation\":\"{name}\",\"samples\":{count},\"median_ms\":{:.3},\"p95_ms\":{p95:.3}}}", samples[samples.len()/2]));
        }
        println!("{{\"mode\":\"benchmark\",\"width\":{width},\"height\":{height},\"warm_cache\":true,\"rss_kib\":{},\"hwm_kib\":{},\"operations\":[{}]}}", metric("VmRSS:"), metric("VmHWM:"), records.join(","));
    } else if mode == "stress" {
        let inputs = vec![source.clone(); count as usize];
        let workers = bigimage_core::resources::batch_worker_count(&inputs);
        let next = std::sync::atomic::AtomicUsize::new(0);
        let start = Instant::now();
        std::thread::scope(|scope| -> Result<(), String> {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    scope.spawn(|| -> Result<(), String> {
                        loop {
                            let idx = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if idx >= count as usize {
                                break;
                            }
                            let result = convert_file_to(
                                &source,
                                Some(&output),
                                Format::Png,
                                &EncodeOptions::default(),
                                OverwritePolicy::Increment,
                            )
                            .map_err(|e| e.to_string())?;
                            let decoded = open_image(result.path()).map_err(|e| e.to_string())?;
                            if (decoded.width(), decoded.height()) != (width, height) {
                                return Err("corrupt output".into());
                            }
                        }
                        Ok(())
                    })
                })
                .collect();
            for handle in handles {
                handle.join().map_err(|_| "worker panic")??;
            }
            Ok(())
        })?;
        // Warmup produced one JPEG; stress adds exactly `count` PNG files.
        let written = std::fs::read_dir(&output)?.count();
        if written != count as usize + 1 {
            return Err("lost or partial output".into());
        }
        println!("{{\"mode\":\"stress\",\"jobs\":{count},\"workers\":{workers},\"seconds\":{:.3},\"hwm_kib\":{},\"passed\":true}}", start.elapsed().as_secs_f64(), metric("VmHWM:"));
    } else {
        let baseline = metric("VmRSS:");
        let fd_start = descriptors();
        let threads_start = metric("Threads:");
        let start = Instant::now();
        let mut cycles = 0;
        let mut samples = vec![baseline];
        while start.elapsed() < Duration::from_secs(count) {
            cycle(&source, &output)?;
            cycles += 1;
            if cycles % 25 == 0 {
                samples.push(metric("VmRSS:"));
            }
        }
        let final_rss = metric("VmRSS:");
        samples.push(final_rss);
        let fd_end = descriptors();
        let threads_end = metric("Threads:");
        let growth = final_rss.saturating_sub(baseline);
        let passed = growth <= 64 * 1024 && fd_end == fd_start && threads_end == threads_start;
        println!("{{\"mode\":\"soak\",\"seconds\":{:.3},\"cycles\":{cycles},\"baseline_rss_kib\":{baseline},\"final_rss_kib\":{final_rss},\"growth_kib\":{growth},\"hwm_kib\":{},\"fds_before\":{fd_start},\"fds_after\":{fd_end},\"threads_before\":{threads_start},\"threads_after\":{threads_end},\"rss_samples_kib\":{samples:?},\"passed\":{passed}}}", start.elapsed().as_secs_f64(), metric("VmHWM:"));
        if !passed {
            return Err(
                "crescimento de recursos acima do limite do probe; investigar com profiler".into(),
            );
        }
    }
    Ok(())
}
