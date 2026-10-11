// SPDX-License-Identifier: GPL-3.0-or-later
//! Build animated GIFs (and, later, WebP/APNG) from a stack of still frames.
//!
//! Frames are decoded and encoded one at a time. A unique temporary sibling
//! protects both existing outputs and input frames when encoding fails.

use std::path::{Path, PathBuf};

use image::codecs::gif::{GifEncoder, Repeat};
use image::{DynamicImage, Frame, RgbaImage};

use crate::pipeline;
use crate::{BigImageError, Result};

/// Loop behaviour for an animated GIF.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoopMode {
    /// Play through the frames once and stop on the last one.
    Once,
    /// Repeat a fixed number of times (full cycles; 0 becomes `Once`).
    Finite(u16),
    /// Loop forever — what most users expect from a "GIF".
    #[default]
    Infinite,
}

impl LoopMode {
    fn to_gif_repeat(self) -> Repeat {
        match self {
            LoopMode::Once => Repeat::Finite(0),
            LoopMode::Finite(n) => Repeat::Finite(n),
            LoopMode::Infinite => Repeat::Infinite,
        }
    }
}

/// Options that apply to the entire animation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimateOptions {
    /// Delay shown for each frame, in milliseconds. `100` ≈ 10fps,
    /// `40` ≈ 25fps. Capped internally to 10 ms to respect GIF's minimum
    /// practical rate.
    pub delay_ms: u32,
    /// Loop behaviour (see [`LoopMode`]).
    pub loop_mode: LoopMode,
    /// Encoder speed 1..=30 (GIF palette quantisation). Higher = faster
    /// encode but worse colour quality. `10` is the balanced default.
    pub speed: i32,
}

impl Default for AnimateOptions {
    fn default() -> Self {
        Self { delay_ms: 100, loop_mode: LoopMode::Infinite, speed: 10 }
    }
}

/// Build an animated GIF from `frames` and write it to `output`. All
/// frames are normalised to the first frame's dimensions (later frames
/// shorter/narrower get letter-boxed transparent; larger frames get
/// resized with Lanczos3 via `resize::apply_to`).
pub fn make_gif(frames: &[PathBuf], output: &Path, opts: AnimateOptions) -> Result<PathBuf> {
    if frames.is_empty() {
        return Err(BigImageError::InvalidInput("nenhum quadro informado".to_string()));
    }

    let mut temp = pipeline::output_temp(output)?;
    let mut writer = std::io::BufWriter::new(temp.as_file_mut());
    let mut encoder = GifEncoder::new_with_speed(&mut writer, opts.speed.clamp(1, 30));
    encoder
        .set_repeat(opts.loop_mode.to_gif_repeat())
        .map_err(|e| BigImageError::Encode { path: output.to_path_buf(), source: e })?;

    // Decode first frame to lock canvas dimensions.
    let (first_img, _fmt) = pipeline::decode_with_source_format(&frames[0])?;
    let (w, h) = (first_img.width(), first_img.height());
    let delay = image::Delay::from_numer_denom_ms(opts.delay_ms.max(10), 1);

    let mut push_frame = |img: DynamicImage| -> Result<()> {
        let rgba = normalise_frame(img, w, h)?;
        encoder
            .encode_frame(Frame::from_parts(rgba, 0, 0, delay))
            .map_err(|e| BigImageError::Encode { path: output.to_path_buf(), source: e })?;
        Ok(())
    };

    push_frame(first_img)?;
    for path in &frames[1..] {
        let (img, _fmt) = pipeline::decode_with_source_format(path)?;
        push_frame(img)?;
    }

    drop(encoder);
    std::io::Write::flush(&mut writer).map_err(BigImageError::Io)?;
    drop(writer);
    let outcome =
        pipeline::publish_temp(temp, output.to_path_buf(), crate::OverwritePolicy::Replace)?;
    Ok(outcome.path().to_path_buf())
}

/// Bring an arbitrarily-sized `img` to the canonical `w × h` canvas used
/// for the whole animation: larger images are Lanczos3-downscaled to
/// fit, smaller images are centred on a transparent background.
fn normalise_frame(img: DynamicImage, w: u32, h: u32) -> Result<RgbaImage> {
    if img.width() == w && img.height() == h {
        return Ok(img.to_rgba8());
    }

    // Scale down if larger, preserving aspect.
    let scaled = if img.width() > w || img.height() > h {
        use crate::{Filter, ResizeMode};
        crate::resize::apply_to(&img, ResizeMode::Fit { width: w, height: h }, Filter::Lanczos3)?
    } else {
        img
    };
    let scaled_rgba = scaled.to_rgba8();

    // Centre on transparent canvas of the target size.
    let mut canvas = RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 0, 0]));
    let dx = (w - scaled_rgba.width()) / 2;
    let dy = (h - scaled_rgba.height()) / 2;
    image::imageops::overlay(&mut canvas, &scaled_rgba, dx as i64, dy as i64);
    Ok(canvas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use tempfile::TempDir;

    fn fixture(dir: &Path, name: &str, w: u32, h: u32, rgb: [u8; 3]) -> PathBuf {
        let path = dir.join(name);
        let img = RgbImage::from_pixel(w, h, Rgb(rgb));
        img.save(&path).unwrap();
        path
    }

    #[test]
    fn gif_from_three_frames() {
        let dir = TempDir::new().unwrap();
        let f1 = fixture(dir.path(), "a.png", 60, 40, [255, 0, 0]);
        let f2 = fixture(dir.path(), "b.png", 60, 40, [0, 255, 0]);
        let f3 = fixture(dir.path(), "c.png", 60, 40, [0, 0, 255]);
        let out = dir.path().join("out.gif");
        let path = make_gif(&[f1, f2, f3], &out, AnimateOptions::default()).unwrap();
        assert!(path.exists());
        // Decodes as GIF.
        let decoded = image::open(&path).unwrap();
        assert_eq!(decoded.width(), 60);
        assert_eq!(decoded.height(), 40);
    }

    #[test]
    fn frames_of_different_sizes_get_normalised() {
        let dir = TempDir::new().unwrap();
        let big = fixture(dir.path(), "big.png", 120, 80, [128; 3]);
        let small = fixture(dir.path(), "small.png", 30, 20, [200; 3]);
        let out = dir.path().join("mixed.gif");
        make_gif(&[big, small], &out, AnimateOptions::default()).unwrap();
        let decoded = image::open(&out).unwrap();
        assert_eq!(decoded.width(), 120);
        assert_eq!(decoded.height(), 80);
    }

    #[test]
    fn empty_frames_rejected() {
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("empty.gif");
        let err = make_gif(&[], &out, AnimateOptions::default()).unwrap_err();
        assert!(matches!(err, BigImageError::InvalidInput(_)));
    }

    #[test]
    fn finite_loop_writes_without_error() {
        let dir = TempDir::new().unwrap();
        let f = fixture(dir.path(), "f.png", 16, 16, [0; 3]);
        let out = dir.path().join("loop.gif");
        let opts = AnimateOptions {
            loop_mode: LoopMode::Finite(3),
            delay_ms: 50,
            ..AnimateOptions::default()
        };
        make_gif(&[f], &out, opts).unwrap();
        assert!(out.exists());
    }
}
