// SPDX-License-Identifier: GPL-3.0-or-later
//! SSIMULACRA2 against the pixels being saved, and the quality search built
//! on it.
//!
//! The reference is the image exactly as the target format will store it
//! (after [`crate::convert::prepare_for_target`]), reduced to 8-bit sRGB —
//! what a viewer displays. Images with transparency are scored composited
//! over black *and* over white and the worse score counts: an error hidden
//! by one background (e.g. dark fringes on black) shows on the other, and
//! colour stored under fully transparent pixels — invisible on both — is
//! correctly ignored.
//!
//! Images up to [`FULL_RES_PIXELS`] are scored at full resolution, every
//! candidate. Larger images are searched on a few full-resolution tiles
//! (never a downscale, which would hide the very artefacts we look for);
//! each tile must pass, then the whole image is encoded once at the found
//! quality.

use std::time::{Duration, Instant};

use fast_ssim2::{PixelDescriptor, PixelSlice, Ssimulacra2Reference};
use image::{ImageError, ImageResult, RgbaImage};

/// Largest image scored as a whole (12 MP). Above it, tiles are sampled.
pub(crate) const FULL_RES_PIXELS: u64 = 12_000_000;
/// Edge of the square tiles sampled from images above [`FULL_RES_PIXELS`].
const TILE: u32 = 1024;
/// Upper bound on the encode+score rounds of one search.
const MAX_ROUNDS: u32 = 8;
/// Wall-clock budget of one search; past it the best passing quality found
/// so far is used (or the upper bound when none passed yet).
const BUDGET: Duration = Duration::from_secs(45);

fn metric_error(error: impl std::fmt::Display) -> ImageError {
    ImageError::IoError(std::io::Error::other(format!("SSIMULACRA2: {error}")))
}

/// Backgrounds an RGBA image is composited over before scoring.
const BACKGROUNDS: [u8; 2] = [0, 255];

fn composite(image: &RgbaImage, background: u8) -> Vec<u8> {
    let bg = u32::from(background);
    let mut out = Vec::with_capacity(image.as_raw().len() / 4 * 3);
    for px in image.as_raw().as_chunks::<4>().0 {
        let a = u32::from(px[3]);
        for &c in &px[..3] {
            out.push(((u32::from(c) * a + bg * (255 - a) + 127) / 255) as u8);
        }
    }
    out
}

/// Precomputed SSIMULACRA2 reference(s) for one image.
pub(crate) struct Metric {
    width: u32,
    height: u32,
    alpha: bool,
    references: Vec<Ssimulacra2Reference>,
}

impl Metric {
    pub(crate) fn new(reference: &RgbaImage) -> ImageResult<Self> {
        let (width, height) = reference.dimensions();
        let alpha = reference.pixels().any(|px| px[3] != 255);
        let planes: Vec<Vec<u8>> = if alpha {
            BACKGROUNDS.iter().map(|&bg| composite(reference, bg)).collect()
        } else {
            vec![super::rgb_of(reference)]
        };
        let references = planes
            .iter()
            .map(|rgb| {
                let slice = rgb_slice(rgb, width, height)?;
                Ssimulacra2Reference::new(&slice).map_err(metric_error)
            })
            .collect::<ImageResult<_>>()?;
        Ok(Self { width, height, alpha, references })
    }

    /// Worst score of `distorted` over the backgrounds (100 = identical).
    pub(crate) fn score(&self, distorted: &RgbaImage) -> ImageResult<f64> {
        if distorted.dimensions() != (self.width, self.height) {
            return Err(metric_error("dimensões diferentes"));
        }
        let mut worst = f64::INFINITY;
        if self.alpha {
            for (reference, &bg) in self.references.iter().zip(&BACKGROUNDS) {
                let rgb = composite(distorted, bg);
                let slice = rgb_slice(&rgb, self.width, self.height)?;
                worst = worst.min(reference.compare(&slice).map_err(metric_error)?);
            }
        } else {
            let rgb = super::rgb_of(distorted);
            let slice = rgb_slice(&rgb, self.width, self.height)?;
            worst = self.references[0].compare(&slice).map_err(metric_error)?;
        }
        Ok(worst)
    }
}

fn rgb_slice(rgb: &[u8], width: u32, height: u32) -> ImageResult<PixelSlice<'_>> {
    PixelSlice::new(rgb, width, height, width as usize * 3, PixelDescriptor::RGB8_SRGB)
        .map_err(metric_error)
}

/// One region the search encodes and scores.
pub(crate) struct Region {
    pub(crate) pixels: RgbaImage,
    metric: Metric,
}

/// The regions a search scores: the whole image, or sampled tiles.
pub(crate) struct Probe {
    regions: Vec<Region>,
    /// `true` when the single region is the whole image, so a passing
    /// candidate *is* the final file.
    whole: bool,
}

impl Probe {
    pub(crate) fn new(image: &RgbaImage) -> ImageResult<Self> {
        let (w, h) = image.dimensions();
        if u64::from(w) * u64::from(h) <= FULL_RES_PIXELS {
            let metric = Metric::new(image)?;
            return Ok(Self {
                regions: vec![Region { pixels: image.clone(), metric }],
                whole: true,
            });
        }
        // A 3×2 grid of full-resolution tiles (6 MP scored instead of the
        // whole), spread over the frame so smooth skies and busy detail
        // both get a vote.
        let tile_w = TILE.min(w);
        let tile_h = TILE.min(h);
        let mut regions = Vec::new();
        for row in 0..2u32 {
            for col in 0..3u32 {
                let x = (u64::from(w - tile_w) * u64::from(col * 2 + 1) / 6) as u32;
                let y = (u64::from(h - tile_h) * u64::from(row * 2 + 1) / 4) as u32;
                let pixels = image::imageops::crop_imm(image, x, y, tile_w, tile_h).to_image();
                let metric = Metric::new(&pixels)?;
                regions.push(Region { pixels, metric });
            }
        }
        Ok(Self { regions, whole: false })
    }

    pub(crate) fn is_whole(&self) -> bool {
        self.whole
    }

    /// Encode every region at `quality`; the worst score and, for a whole-
    /// image probe, the encoded bytes.
    fn evaluate<E>(&self, quality: u8, encode: &mut E) -> ImageResult<(f64, Vec<u8>)>
    where
        E: FnMut(&RgbaImage, u8) -> ImageResult<(Vec<u8>, RgbaImage)>,
    {
        let mut worst = f64::INFINITY;
        let mut bytes = Vec::new();
        for region in &self.regions {
            let (encoded, decoded) = encode(&region.pixels, quality)?;
            worst = worst.min(region.metric.score(&decoded)?);
            if self.whole {
                bytes = encoded;
            }
        }
        Ok((worst, bytes))
    }

    /// Score already-decoded pixels of the whole image (whole probes only).
    pub(crate) fn score_whole(&self, decoded: &RgbaImage) -> ImageResult<Option<f64>> {
        if !self.whole {
            return Ok(None);
        }
        self.regions[0].metric.score(decoded).map(Some)
    }
}

/// Outcome of [`search`].
#[derive(Debug)]
pub(crate) struct Found {
    /// Lowest quality that met the threshold, or `hi` when none did.
    pub(crate) quality: u8,
    /// Whether `quality` actually met the threshold.
    pub(crate) passed: bool,
    /// Encoded whole image at `quality` when the probe was the whole image
    /// and that quality was evaluated.
    pub(crate) bytes: Option<Vec<u8>>,
    /// Worst score measured at `quality` (when evaluated).
    pub(crate) score: Option<f64>,
    /// Encode+score rounds spent.
    pub(crate) rounds: u32,
}

/// Quality range and first guess of a [`search`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bounds {
    pub(crate) lo: u8,
    pub(crate) hi: u8,
    /// Typical answer for the format and target; a good guess saves rounds.
    pub(crate) start: u8,
}

/// Score points gained per quality step, a conservative guess used to
/// step from the first probe before a bracket exists.
const SLOPE: f64 = 0.5;

/// Find the lowest quality in `lo..=hi` whose encode scores at least
/// `threshold` on every region.
///
/// Scores rise with quality, so this is a root search: probe the guess,
/// step towards the threshold until it is bracketed, then interpolate
/// (secant) inside the bracket, falling back to bisection when the
/// interpolation stalls on one side. Typically 3–5 rounds instead of the
/// 6–7 of plain bisection. Small non-monotonic wiggles only cost a
/// slightly larger file, never a failing one: the returned quality was
/// always measured.
pub(crate) fn search<E>(
    probe: &Probe,
    bounds: Bounds,
    threshold: f64,
    mut encode: E,
) -> ImageResult<Found>
where
    E: FnMut(&RgbaImage, u8) -> ImageResult<(Vec<u8>, RgbaImage)>,
{
    let Bounds { lo, hi, start } = bounds;
    let started = Instant::now();
    // Lowest passing (quality, score, bytes) and highest failing (quality, score).
    let mut pass: Option<(u8, f64, Vec<u8>)> = None;
    let mut fail: Option<(u8, f64)> = None;
    let mut q = start.clamp(lo, hi);
    let mut rounds = 0;
    let mut same_side = 0u32;
    let mut last_passed: Option<bool> = None;
    loop {
        if rounds >= MAX_ROUNDS || (rounds > 0 && started.elapsed() > BUDGET) {
            tracing::debug!(rounds, "busca de qualidade interrompida pelo limite");
            break;
        }
        rounds += 1;
        let (score, bytes) = probe.evaluate(q, &mut encode)?;
        tracing::trace!(quality = q, score, size = bytes.len(), "candidato");
        let passed = score >= threshold;
        same_side = if last_passed == Some(passed) { same_side + 1 } else { 0 };
        last_passed = Some(passed);
        if passed {
            if pass.as_ref().is_none_or(|(p, ..)| q < *p) {
                pass = Some((q, score, bytes));
            }
        } else if fail.is_none_or(|(f, _)| q > f) {
            fail = Some((q, score));
        }
        // Unknown qualities still worth probing.
        let lower = fail.map_or(lo, |(f, _)| f.saturating_add(1));
        let upper = pass.as_ref().map_or(hi, |(p, ..)| p.saturating_sub(1));
        if lower > upper || (pass.is_none() && fail.is_some_and(|(f, _)| f >= hi)) {
            break;
        }
        let step = |gap: f64| (gap / SLOPE).ceil().clamp(2.0, 25.0) as u8;
        q = match (&pass, fail) {
            (Some((pq, ps, _)), Some((fq, fs))) => {
                let mid = lower + (upper - lower) / 2;
                if same_side >= 1 || ps - fs <= f64::EPSILON {
                    mid
                } else {
                    let t = (threshold - fs) / (ps - fs);
                    let guess = f64::from(fq) + t * f64::from(pq - fq);
                    (guess.ceil() as u8).clamp(lower, upper)
                }
            }
            (Some((pq, ps, _)), None) => {
                pq.saturating_sub(step(ps - threshold)).clamp(lower, upper)
            }
            (None, Some((fq, fs))) => fq.saturating_add(step(threshold - fs)).clamp(lower, upper),
            (None, None) => unreachable!("one probe was evaluated"),
        };
    }
    let found = match pass {
        Some((quality, score, bytes)) => Found {
            quality,
            passed: true,
            bytes: probe.is_whole().then_some(bytes),
            score: Some(score),
            rounds,
        },
        None => Found { quality: hi, passed: false, bytes: None, score: None, rounds },
    };
    tracing::debug!(
        quality = found.quality,
        passed = found.passed,
        score = found.score,
        rounds = found.rounds,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "busca de qualidade"
    );
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_images_score_100_and_noise_scores_lower() {
        let image = RgbaImage::from_fn(64, 48, |x, y| {
            image::Rgba([(x * 4) as u8, (y * 5) as u8, ((x + y) * 2) as u8, 255])
        });
        let metric = Metric::new(&image).unwrap();
        assert!(metric.score(&image).unwrap() > 99.0);
        let mut noisy = image.clone();
        for (i, px) in noisy.pixels_mut().enumerate() {
            px[0] = px[0].wrapping_add(((i * 7919) % 41) as u8);
        }
        assert!(metric.score(&noisy).unwrap() < 80.0);
    }

    #[test]
    fn colour_under_transparent_pixels_is_ignored_but_alpha_errors_count() {
        let image = RgbaImage::from_fn(32, 32, |x, _| {
            if x < 16 {
                image::Rgba([200, 30, 30, 255])
            } else {
                image::Rgba([0, 0, 0, 0])
            }
        });
        let metric = Metric::new(&image).unwrap();
        let mut hidden = image.clone();
        for px in hidden.pixels_mut().filter(|px| px[3] == 0) {
            *px = image::Rgba([12, 250, 77, 0]);
        }
        assert!(metric.score(&hidden).unwrap() > 99.0, "invisible colour must not count");
        let mut visible = image.clone();
        for px in visible.pixels_mut().filter(|px| px[3] == 0) {
            px[3] = 128;
        }
        assert!(metric.score(&visible).unwrap() < 70.0, "alpha change must count");
    }

    #[test]
    fn search_returns_the_lowest_passing_quality_without_scanning_everything() {
        // A synthetic encoder whose "error" shrinks as quality grows.
        let image = RgbaImage::from_fn(48, 48, |x, y| {
            image::Rgba([(x * 5) as u8, (y * 5) as u8, ((x * y) % 256) as u8, 255])
        });
        let probe = Probe::new(&image).unwrap();
        let mut calls = 0;
        let bounds = Bounds { lo: 1, hi: 100, start: 50 };
        let found = search(&probe, bounds, 80.0, |pixels, q| {
            calls += 1;
            let step = (101 - u32::from(q)) / 4 + 1;
            let mut out = pixels.clone();
            for px in out.pixels_mut() {
                for c in &mut px.0[..3] {
                    *c = ((u32::from(*c) / step) * step) as u8;
                }
            }
            Ok((vec![q], out))
        })
        .unwrap();
        assert!(found.passed);
        assert!(calls <= MAX_ROUNDS as usize, "{calls} rounds");
        assert_eq!(found.bytes.as_deref(), Some(&[found.quality][..]));
        assert!(found.score.unwrap() >= 80.0);
        assert_eq!(found.rounds as usize, calls);
        // The answer is the lowest passing quality: one below fails.
        let below = search(
            &probe,
            Bounds { lo: 1, hi: found.quality - 1, start: found.quality - 1 },
            80.0,
            |pixels, q| {
                let step = (101 - u32::from(q)) / 4 + 1;
                let mut out = pixels.clone();
                for px in out.pixels_mut() {
                    for c in &mut px.0[..3] {
                        *c = ((u32::from(*c) / step) * step) as u8;
                    }
                }
                Ok((vec![q], out))
            },
        )
        .unwrap();
        assert!(!below.passed || below.quality == found.quality - 1 && found.quality == 1);
    }

    #[test]
    fn large_images_are_probed_on_full_resolution_tiles() {
        let image = RgbaImage::from_pixel(4100, 3000, image::Rgba([10, 20, 30, 255]));
        let probe = Probe::new(&image).unwrap();
        assert!(!probe.is_whole());
        assert_eq!(probe.regions.len(), 6);
        assert!(probe.regions.iter().all(|r| r.pixels.dimensions() == (TILE, TILE)));
    }
}
