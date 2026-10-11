// SPDX-License-Identifier: GPL-3.0-or-later
//! Compare AVIF threading with identical pixels, quality and speed.
use image::ImageEncoder;
use std::time::Instant;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let img = image::RgbaImage::from_fn(1920, 1080, |x, y| {
        image::Rgba([
            (x.wrapping_mul(13) ^ y.wrapping_mul(7)) as u8,
            (x / 8) as u8,
            (y / 4) as u8,
            255,
        ])
    });
    let mut records = Vec::new();
    for threads in [Some(1), Some(2), Some(4), None] {
        let start = Instant::now();
        let mut bytes = Vec::new();
        let encoder = image::codecs::avif::AvifEncoder::new_with_speed_quality(&mut bytes, 8, 80)
            .with_num_threads(threads);
        encoder.write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgba8,
        )?;
        records.push(format!(
            "{{\"threads\":{},\"ms\":{:.3},\"bytes\":{}}}",
            threads.map_or("null".to_string(), |n| n.to_string()),
            start.elapsed().as_secs_f64() * 1000.0,
            bytes.len()
        ));
    }
    println!(
        "{{\"width\":1920,\"height\":1080,\"speed\":8,\"quality\":80,\"results\":[{}]}}",
        records.join(",")
    );
    Ok(())
}
