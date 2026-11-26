// SPDX-License-Identifier: GPL-3.0-or-later
//! Animated GIF writer: imagequant palettes per frame and inter-frame
//! optimisation.
//!
//! Each frame gets its own palette (exact when it has few colours,
//! imagequant with dithering otherwise). When a frame and the one before it
//! are both fully opaque, only the rectangle that changed is stored, and
//! pixels inside it that did not change become transparent — the previous
//! frame is kept on screen underneath (disposal "keep"). Frames with
//! transparency are written whole with disposal "background", and so is a
//! difference frame followed by one: clearing only its rectangle would leave
//! older frames visible through the transparent pixels.
//!
//! Frames are written one behind: a frame's disposal depends on whether
//! the *next* frame can be stored as a difference.

use std::borrow::Cow;
use std::io::Write;

use image::{ImageResult, RgbaImage};

use super::encode_error;
use super::palette::{self, Paletted};

fn gif_error(error: gif::EncodingError) -> image::ImageError {
    encode_error(image::ImageFormat::Gif, error.to_string())
}

struct Pending {
    frame: gif::Frame<'static>,
    /// What the viewer shows after this frame is drawn.
    displayed: RgbaImage,
    opaque: bool,
    /// Stored as the changed rectangle over the frame before it.
    difference: bool,
}

/// Streaming animated GIF encoder.
pub(crate) struct GifAnimation<W: Write> {
    encoder: gif::Encoder<W>,
    width: u16,
    height: u16,
    delay_cs: u16,
    speed: i32,
    pending: Option<Pending>,
}

impl<W: Write> GifAnimation<W> {
    /// `speed` is imagequant's `1..=10` (1 = best palette, slowest).
    pub(crate) fn new(
        writer: W,
        width: u32,
        height: u32,
        repeat: gif::Repeat,
        delay_cs: u16,
        speed: i32,
    ) -> ImageResult<Self> {
        let too_big = || {
            encode_error(
                image::ImageFormat::Gif,
                format!("dimensions {width}×{height} too large for GIF"),
            )
        };
        let width = u16::try_from(width).map_err(|_| too_big())?;
        let height = u16::try_from(height).map_err(|_| too_big())?;
        let mut encoder = gif::Encoder::new(writer, width, height, &[]).map_err(gif_error)?;
        encoder.set_repeat(repeat).map_err(gif_error)?;
        Ok(Self { encoder, width, height, delay_cs, speed, pending: None })
    }

    fn palette_for(&self, image: &RgbaImage, max: usize) -> ImageResult<Paletted> {
        if let Some(exact) = palette::exact(image, max) {
            return Ok(exact);
        }
        let mut quantized = palette::quantize_with_speed(image, max as u32, &[1.0], self.speed)?;
        Ok(quantized.pop().expect("one dithering level"))
    }

    /// Add the next frame (`width × height` RGBA).
    pub(crate) fn push(&mut self, rgba: &RgbaImage) -> ImageResult<()> {
        let opaque = rgba.pixels().all(|px| px[3] == 255);
        let diff_base =
            self.pending.as_ref().filter(|p| p.opaque && opaque).map(|p| p.displayed.clone());
        let difference = diff_base.is_some();
        let next = match diff_base {
            Some(previous) => self.difference_frame(rgba, &previous)?,
            None => self.full_frame(rgba, opaque)?,
        };
        if let Some(mut pending) = self.pending.take() {
            // A difference frame draws over this one; anything else replaces
            // it. Background disposal clears only the frame's own rectangle,
            // so a pending difference frame is first rewritten whole: the
            // frames under it must not show through the next one's
            // transparency.
            if difference {
                pending.frame.dispose = gif::DisposalMethod::Keep;
            } else {
                if pending.difference {
                    pending = self.full_frame(&pending.displayed, true)?;
                }
                pending.frame.dispose = gif::DisposalMethod::Background;
            }
            self.encoder.write_frame(&pending.frame).map_err(gif_error)?;
        }
        self.pending = Some(next);
        Ok(())
    }

    fn full_frame(&self, rgba: &RgbaImage, opaque: bool) -> ImageResult<Pending> {
        let binary = palette::binary_alpha(rgba);
        let paletted = self.palette_for(&binary, 256)?;
        let displayed = paletted.to_rgba();
        let transparent = paletted.palette.iter().position(|c| c[3] < 128).map(|t| t as u8);
        let indices = match transparent {
            Some(t) => paletted
                .indices
                .iter()
                .map(|&i| if paletted.palette[usize::from(i)][3] < 128 { t } else { i })
                .collect(),
            None => paletted.indices.clone(),
        };
        let frame = gif::Frame {
            width: self.width,
            height: self.height,
            buffer: Cow::Owned(indices),
            palette: Some(paletted.palette.iter().flat_map(|c| [c[0], c[1], c[2]]).collect()),
            transparent,
            delay: self.delay_cs,
            dispose: gif::DisposalMethod::Background,
            ..gif::Frame::default()
        };
        Ok(Pending { frame, displayed, opaque, difference: false })
    }

    /// Store only what changed since `previous` (both opaque).
    fn difference_frame(&self, rgba: &RgbaImage, previous: &RgbaImage) -> ImageResult<Pending> {
        // One palette slot is reserved for "unchanged" (transparent).
        let paletted = self.palette_for(rgba, 255)?;
        let displayed = paletted.to_rgba();
        let (w, h) = displayed.dimensions();
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
        for (x, y, px) in displayed.enumerate_pixels() {
            if px != previous.get_pixel(x, y) {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
        if x0 >= x1 {
            // Nothing changed: one transparent pixel keeps the timing.
            (x0, y0, x1, y1) = (0, 0, 1, 1);
        }
        let clear = paletted.palette.len() as u8;
        let mut indices = Vec::with_capacity(((x1 - x0) * (y1 - y0)) as usize);
        for y in y0..y1 {
            for x in x0..x1 {
                let unchanged = displayed.get_pixel(x, y) == previous.get_pixel(x, y);
                indices.push(if unchanged {
                    clear
                } else {
                    paletted.indices[(y * w + x) as usize]
                });
            }
        }
        let mut colours: Vec<u8> =
            paletted.palette.iter().flat_map(|c| [c[0], c[1], c[2]]).collect();
        colours.extend_from_slice(&[0, 0, 0]);
        let frame = gif::Frame {
            left: x0 as u16,
            top: y0 as u16,
            width: (x1 - x0) as u16,
            height: (y1 - y0) as u16,
            buffer: Cow::Owned(indices),
            palette: Some(colours),
            transparent: Some(clear),
            delay: self.delay_cs,
            dispose: gif::DisposalMethod::Keep,
            ..gif::Frame::default()
        };
        Ok(Pending { frame, displayed, opaque: true, difference: true })
    }

    /// Write the last frame and the trailer.
    pub(crate) fn finish(mut self) -> ImageResult<W> {
        if let Some(pending) = self.pending.take() {
            self.encoder.write_frame(&pending.frame).map_err(gif_error)?;
        }
        self.encoder.into_inner().map_err(gif_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::AnimationDecoder;

    #[test]
    fn opaque_frames_store_only_the_changed_rectangle_and_decode_exactly() {
        // 16 × 12 blocks: 192 colours, so palettes are exact.
        let base = RgbaImage::from_fn(64, 48, |x, y| {
            image::Rgba([(x / 4 * 16) as u8, (y / 4 * 20) as u8, 90, 255])
        });
        let mut moved = base.clone();
        for y in 10..14 {
            for x in 20..26 {
                moved.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
            }
        }
        let frames = [base.clone(), moved.clone(), moved.clone(), base.clone()];
        let encode = |frames: &[RgbaImage]| {
            let mut animation =
                GifAnimation::new(Vec::new(), 64, 48, gif::Repeat::Infinite, 10, 4).unwrap();
            for frame in frames {
                animation.push(frame).unwrap();
            }
            animation.finish().unwrap()
        };
        let bytes = encode(&frames);
        let decoded = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&bytes))
            .unwrap()
            .into_frames()
            .collect_frames()
            .unwrap();
        assert_eq!(decoded.len(), 4);
        for (frame, expected) in decoded.iter().zip(&frames) {
            assert_eq!(frame.buffer(), expected, "≤ 256 colours must round-trip exactly");
        }
        // Four full frames would repeat the whole picture each time.
        let full = encode(std::slice::from_ref(&base)).len();
        assert!(bytes.len() < full * 2, "{} vs one frame {full}", bytes.len());
    }

    #[test]
    fn transparent_frames_are_written_whole() {
        let a = RgbaImage::from_fn(16, 16, |x, _| {
            image::Rgba([200, 0, 0, if x < 8 { 255 } else { 0 }])
        });
        let b = RgbaImage::from_fn(16, 16, |x, _| {
            image::Rgba([0, 0, 200, if x < 8 { 0 } else { 255 }])
        });
        let mut animation =
            GifAnimation::new(Vec::new(), 16, 16, gif::Repeat::Finite(2), 5, 4).unwrap();
        animation.push(&a).unwrap();
        animation.push(&b).unwrap();
        let bytes = animation.finish().unwrap();
        let decoded = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&bytes))
            .unwrap()
            .into_frames()
            .collect_frames()
            .unwrap();
        // Transparent pixels decode as transparent black.
        let clear = |image: &RgbaImage| {
            let mut out = image.clone();
            out.pixels_mut().filter(|px| px[3] == 0).for_each(|px| *px = image::Rgba([0; 4]));
            out
        };
        assert_eq!(decoded[0].buffer(), &clear(&a));
        assert_eq!(decoded[1].buffer(), &clear(&b), "the first frame must not show through");
    }

    #[test]
    fn transparent_frame_after_a_difference_frame_shows_nothing_older() {
        let red = RgbaImage::from_pixel(16, 16, image::Rgba([255, 0, 0, 255]));
        let mut changed = red.clone();
        changed.put_pixel(3, 3, image::Rgba([0, 255, 0, 255]));
        let half = RgbaImage::from_fn(16, 16, |x, _| {
            image::Rgba([0, 0, 255, if x < 8 { 255 } else { 0 }])
        });
        let mut animation =
            GifAnimation::new(Vec::new(), 16, 16, gif::Repeat::Infinite, 5, 4).unwrap();
        for frame in [&red, &changed, &half] {
            animation.push(frame).unwrap();
        }
        let bytes = animation.finish().unwrap();
        let decoded = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&bytes))
            .unwrap()
            .into_frames()
            .collect_frames()
            .unwrap();
        assert_eq!(decoded[1].buffer(), &changed);
        for (x, y, px) in decoded[2].buffer().enumerate_pixels() {
            let expected = if x < 8 { [0, 0, 255, 255] } else { [0; 4] };
            assert_eq!(px.0, expected, "pixel {x},{y}");
        }
    }
}
