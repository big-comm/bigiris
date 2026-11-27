// SPDX-License-Identifier: GPL-3.0-or-later
//! Build animated GIFs (and, later, WebP/APNG) from a stack of still frames.
//!
//! Frames are decoded and encoded one at a time. A unique temporary sibling
//! protects both existing outputs and input frames when encoding fails.

use std::path::{Path, PathBuf};

use image::{DynamicImage, RgbaImage};

use crate::pipeline;
use crate::{BigImageError, ConvertOutcome, OverwritePolicy, Result};

/// Loop behaviour for an animated GIF.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoopMode {
    /// Play through the frames once and stop on the last one.
    Once,
    /// Play a fixed number of full cycles (0 and 1 play once).
    Finite(u16),
    /// Loop forever — what most users expect from a "GIF".
    #[default]
    Infinite,
}

impl LoopMode {
    fn to_gif_repeat(self) -> gif::Repeat {
        match self {
            LoopMode::Once => gif::Repeat::Finite(0),
            // The NETSCAPE loop count is the number of repeats after the
            // first play.
            LoopMode::Finite(n) => gif::Repeat::Finite(n.saturating_sub(1)),
            LoopMode::Infinite => gif::Repeat::Infinite,
        }
    }
}

/// Options that apply to the entire animation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimateOptions {
    /// Delay shown for each frame, in milliseconds. `100` ≈ 10fps,
    /// `40` ≈ 25fps. GIF stores delays in centiseconds and browsers replace
    /// delays below 2 cs with 10 cs (making "fast" GIFs play *slower*), so
    /// values below [`MIN_DELAY_MS`] are raised to it.
    pub delay_ms: u32,
    /// Loop behaviour (see [`LoopMode`]).
    pub loop_mode: LoopMode,
    /// Encoder speed 1..=30 (GIF palette quantisation). Higher = faster
    /// encode but worse colour quality. `10` is the balanced default.
    /// Mapped onto imagequant's 1..=10 speed (`(speed + 2) / 3`).
    pub speed: i32,
}

/// Shortest frame delay written to a GIF (2 centiseconds, 50fps): the
/// smallest value every major browser honours instead of clamping to 100 ms.
pub const MIN_DELAY_MS: u32 = 20;

impl Default for AnimateOptions {
    fn default() -> Self {
        Self { delay_ms: 100, loop_mode: LoopMode::Infinite, speed: 10 }
    }
}

/// Build an animated GIF from `frames` and write it to `output`. All
/// frames are normalised to the first frame's dimensions (later frames
/// shorter/narrower get letter-boxed transparent; larger frames get
/// resized with Lanczos3 via `resize::apply_to`).
///
/// An existing `output` is replaced; see [`make_gif_with_policy`] to skip or
/// pick a free name instead.
///
/// # Errors
///
/// As [`make_gif_with_policy`].
pub fn make_gif(frames: &[PathBuf], output: &Path, opts: AnimateOptions) -> Result<PathBuf> {
    make_gif_with_policy(frames, output, opts, OverwritePolicy::Replace)
        .map(|outcome| outcome.path().to_path_buf())
}

/// Like [`make_gif`], honouring `policy` when `output` already exists:
/// `Skip` returns [`ConvertOutcome::Skipped`] without decoding any frame,
/// `Increment` publishes to `name_1.gif`, `name_2.gif`, … atomically.
///
/// # Errors
///
/// - [`BigImageError::InvalidInput`]: `frames` is empty, or the first frame
///   exceeds the processing limits.
/// - [`BigImageError::NotStatic`]: a frame is itself animated or multi-page.
/// - [`BigImageError::Decode`] or [`BigImageError::UnsupportedFormat`]: a frame
///   cannot be read or decoded.
/// - [`BigImageError::Encode`] or [`BigImageError::Io`]: the GIF cannot be
///   encoded, synced or published.
/// - [`BigImageError::NoFreeName`]: `Increment` finds every numbered name taken.
pub fn make_gif_with_policy(
    frames: &[PathBuf],
    output: &Path,
    opts: AnimateOptions,
    policy: OverwritePolicy,
) -> Result<ConvertOutcome> {
    if frames.is_empty() {
        return Err(BigImageError::InvalidInput("no frames given".to_string()));
    }
    if matches!(policy, OverwritePolicy::Skip) && output.symlink_metadata().is_ok() {
        return Ok(ConvertOutcome::Skipped { output: output.to_path_buf() });
    }

    // Decode first frame to lock canvas dimensions.
    let (first_img, _fmt) = pipeline::decode_with_source_format(&frames[0])?;
    let (w, h) = (first_img.width(), first_img.height());
    // Every frame is widened to an RGBA8 canvas of this size.
    pipeline::check_dimensions(w, h, 4)?;
    let delay_cs = u16::try_from(opts.delay_ms.max(MIN_DELAY_MS) / 10).unwrap_or(u16::MAX);
    let speed = (opts.speed.clamp(1, 30) + 2) / 3;
    let encode_error = |source| BigImageError::Encode { path: output.to_path_buf(), source };

    let mut temp = pipeline::output_temp(output, policy)?;
    {
        let writer = std::io::BufWriter::new(temp.as_file_mut());
        let mut animation = crate::optimize::animated::GifAnimation::new(
            writer,
            w,
            h,
            opts.loop_mode.to_gif_repeat(),
            delay_cs,
            speed,
        )
        .map_err(encode_error)?;
        animation.push(&normalise_frame(first_img, w, h)?).map_err(encode_error)?;
        for path in &frames[1..] {
            let (img, _fmt) = pipeline::decode_with_source_format(path)?;
            animation.push(&normalise_frame(img, w, h)?).map_err(encode_error)?;
        }
        let mut writer = animation.finish().map_err(encode_error)?;
        std::io::Write::flush(&mut writer).map_err(BigImageError::Io)?;
    }
    pipeline::publish_temp(temp, output.to_path_buf(), policy)
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
    image::imageops::overlay(&mut canvas, &scaled_rgba, i64::from(dx), i64::from(dy));
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
    fn tiny_delays_are_raised_to_two_centiseconds() {
        use image::AnimationDecoder;
        let dir = TempDir::new().unwrap();
        let f = fixture(dir.path(), "f.png", 8, 8, [9; 3]);
        let out = dir.path().join("fast.gif");
        let opts = AnimateOptions { delay_ms: 5, ..AnimateOptions::default() };
        make_gif(&[f.clone(), f], &out, opts).unwrap();
        let decoder = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(
            std::fs::File::open(&out).unwrap(),
        ))
        .unwrap();
        for frame in decoder.into_frames().collect_frames().unwrap() {
            let (numer, denom) = frame.delay().numer_denom_ms();
            assert_eq!(numer / denom, MIN_DELAY_MS);
        }
    }

    #[test]
    fn policy_variant_skips_or_increments_existing_output() {
        let dir = TempDir::new().unwrap();
        let f = fixture(dir.path(), "f.png", 8, 8, [9; 3]);
        let out = dir.path().join("anim.gif");
        std::fs::write(&out, b"keep").unwrap();
        let frames = [f];
        let opts = AnimateOptions::default();
        let skipped = make_gif_with_policy(&frames, &out, opts, OverwritePolicy::Skip).unwrap();
        assert!(matches!(skipped, ConvertOutcome::Skipped { .. }));
        let next = make_gif_with_policy(&frames, &out, opts, OverwritePolicy::Increment).unwrap();
        assert_eq!(next.path(), dir.path().join("anim_1.gif"));
        assert_eq!(std::fs::read(&out).unwrap(), b"keep");
        let replaced = make_gif_with_policy(&frames, &out, opts, OverwritePolicy::Replace).unwrap();
        assert_eq!(replaced.path(), out);
        image::open(&out).unwrap();
    }

    #[test]
    fn empty_frames_rejected() {
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("empty.gif");
        let err = make_gif(&[], &out, AnimateOptions::default()).unwrap_err();
        assert!(matches!(err, BigImageError::InvalidInput(_)));
    }

    #[test]
    fn finite_loop_counts_full_plays() {
        let dir = TempDir::new().unwrap();
        let f = fixture(dir.path(), "f.png", 16, 16, [0; 3]);
        let out = dir.path().join("loop.gif");
        // The NETSCAPE 2.0 loop count is the repeats after the first play.
        for (mode, repeats) in [
            (LoopMode::Once, gif::Repeat::Finite(0)),
            (LoopMode::Finite(0), gif::Repeat::Finite(0)),
            (LoopMode::Finite(1), gif::Repeat::Finite(0)),
            (LoopMode::Finite(3), gif::Repeat::Finite(2)),
            (LoopMode::Infinite, gif::Repeat::Infinite),
        ] {
            let opts = AnimateOptions { loop_mode: mode, ..AnimateOptions::default() };
            make_gif(std::slice::from_ref(&f), &out, opts).unwrap();
            let mut decoder =
                gif::DecodeOptions::new().read_info(std::fs::File::open(&out).unwrap()).unwrap();
            while decoder.read_next_frame().unwrap().is_some() {}
            assert_eq!(decoder.repeat(), repeats, "{mode:?}");
        }
    }
}
