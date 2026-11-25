// SPDX-License-Identifier: GPL-3.0-or-later
//! Object removal ("Apagar objeto") with Big LaMa (`lama-native`): fill a
//! masked area of a photo from its surroundings.
//!
//! LaMa sees the whole image it is given at once (its Fourier units span
//! all of it), so its memory grows with the area: 0.25 GB at 512², 0.54
//! GB at 1024². A photo is therefore never run whole:
//!
//! 1. the mask (pixels ≥ [`MASK_THRESHOLD`] are filled) is grown by a few
//!    pixels so the fill overlaps the object's edge,
//! 2. a box with context around it is cropped, one per group of marked
//!    regions whose boxes would overlap, so distant marks each run at
//!    their own resolution ([`plans`]),
//! 3. a crop longer than [`MAX_RUN_SIDE`] is scaled down (the grown mask
//!    with it, conservatively: any covered pixel stays a hole),
//! 4. LaMa fills each crop in turn, the fill is scaled back up, and
//! 5. it is pasted back with a feathered edge: fully inside the original
//!    mask, fading out over the grown ring, untouched elsewhere.
//!
//! Pixels outside the grown mask keep their exact values. The alpha
//! channel is kept as it was (LaMa fills colour only).
//!
//! LaMa works on 8-bit sRGB: 16-bit and float sources come out as 8-bit
//! (`Rgb8`, or `Rgba8` with alpha).
//!
//! The weights come from the BigLinux package `lama-native` (Apache-2.0);
//! `BIGIRIS_LAMA_WEIGHTS` points at another file.
//!
//! # API for a brush tool
//!
//! [`inpaint`] takes the image and a same-sized [`image::GrayImage`] mask
//! (paint 255 where the object is) and returns the filled image; its
//! progress callback reports [`Stage`]s and cancels with
//! `ControlFlow::Break`. [`plans`] tells beforehand which boxes will run
//! and at what size, e.g. to warn about a very large stroke.

use std::ops::ControlFlow;

use image::{DynamicImage, GrayImage};

use crate::engine::{self, Budget, Slot};
use crate::memory::MIB;
use crate::{AiError, Availability, Device, ModelInfo, Stage};

/// Big LaMa, half-float weights.
pub const MODEL: ModelInfo = ModelInfo {
    name: "LaMa",
    package: "lama-native",
    weights: "/usr/share/lama-native/big-lama-f16.weights",
    env: "BIGIRIS_LAMA_WEIGHTS",
    license: "Apache-2.0",
};

/// Mask values from this up are filled.
pub const MASK_THRESHOLD: u8 = 128;
/// Smallest side LaMa accepts.
pub const MIN_SIDE: u32 = 9;
/// Longest side LaMa runs at; larger crops are scaled down to it.
pub const MAX_RUN_SIDE: u32 = 1024;
/// Least context kept around the hole on each side.
const MIN_CONTEXT: u32 = 64;
/// Most runs of marked pixels traced into regions; a mask with more (a
/// scattered selection) is filled as one crop around all of it.
const MAX_MASK_RUNS: usize = 1 << 20;
/// Most regions grouped by their crops; beyond it, one crop as well.
const MAX_REGIONS: usize = 256;

/// Measured: 102 MB of half-float weights, 0.25 GB peak for the whole
/// process at 512², 0.54 GB at 1024².
const LOAD_BYTES: u64 = 160 * MIB;
/// Fixed part of a run, with margin.
const RUN_FIXED_BYTES: u64 = 96 * MIB;
/// Network memory per pixel of the run size (0.54 GB − 0.25 GB over
/// 786k pixels ≈ 390 B), with margin.
const RUN_BYTES_PER_PIXEL: u64 = 512;
/// Largest image, as RGBA8 (the core buffer limit).
const MAX_BYTES: u64 = 512 * MIB;

#[cfg(feature = "native")]
static SLOT: Slot<lama_native::Model> = Slot::new();
#[cfg(not(feature = "native"))]
static SLOT: Slot<()> = Slot::new();

#[cfg(feature = "native")]
engine::native_model!(lama_native::Model);

/// Whether object removal can run now.
pub fn availability() -> Availability {
    MODEL.availability()
}

/// Where the loaded model runs; `None` before the first run.
pub fn device() -> Option<Device> {
    SLOT.device()
}

/// Drop the loaded model.
pub fn unload() {
    SLOT.unload();
}

/// A rectangle of pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Left column.
    pub x: u32,
    /// Top row.
    pub y: u32,
    /// Columns.
    pub width: u32,
    /// Rows.
    pub height: u32,
}

impl Rect {
    fn right(&self) -> u32 {
        self.x + self.width
    }
    fn bottom(&self) -> u32 {
        self.y + self.height
    }
    fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

/// How a fill runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// Box of the image given to LaMa (hole plus context).
    pub crop: Rect,
    /// Size the crop runs at (smaller than the crop when scaled down).
    pub run: (u32, u32),
    /// Half-width of the feathered ring: the hole is the mask grown by
    /// `2 × feather` pixels, and the fill fades out over it.
    pub feather: u32,
}

/// The crops that fill `mask` (same size as the image), top to bottom: one
/// per group of marked regions whose crops would overlap. Empty when the
/// mask selects nothing.
///
/// # Errors
///
/// [`AiError::Processing`] when a side of the mask is under [`MIN_SIDE`].
pub fn plans(mask: &GrayImage) -> Result<Vec<Plan>, AiError> {
    let (width, height) = mask.dimensions();
    if width < MIN_SIDE || height < MIN_SIDE {
        return Err(AiError::Processing(format!(
            "image is {width}×{height}: inpainting needs at least {MIN_SIDE} px per side"
        )));
    }
    let mut groups = match mask_regions(mask) {
        Some(regions) if regions.len() <= MAX_REGIONS => regions,
        _ => mask_bounds(mask).into_iter().collect(),
    };
    let crop = |bounds: Rect| plan_for(bounds, width, height).crop;
    'merge: loop {
        for i in 0..groups.len() {
            for j in i + 1..groups.len() {
                if overlap(crop(groups[i]), crop(groups[j])) {
                    groups[i] = union(groups[i], groups[j]);
                    groups.swap_remove(j);
                    continue 'merge;
                }
            }
        }
        break;
    }
    groups.sort_by_key(|bounds| (bounds.y, bounds.x));
    Ok(groups.into_iter().map(|bounds| plan_for(bounds, width, height)).collect())
}

/// The plan around marked pixels whose bounding box is `bounds`.
fn plan_for(bounds: Rect, width: u32, height: u32) -> Plan {
    let feather = feather_for(bounds);
    let hole = grow(bounds, 2 * feather, width, height);
    let crop = crop_around(hole, width, height);
    Plan { crop, run: run_size(crop), feather }
}

fn overlap(a: Rect, b: Rect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

fn union(a: Rect, b: Rect) -> Rect {
    let (x, y) = (a.x.min(b.x), a.y.min(b.y));
    Rect { x, y, width: a.right().max(b.right()) - x, height: a.bottom().max(b.bottom()) - y }
}

/// Bounding boxes of the 8-connected regions of pixels ≥
/// [`MASK_THRESHOLD`], traced as runs along the rows; `None` past
/// [`MAX_MASK_RUNS`] runs.
fn mask_regions(mask: &GrayImage) -> Option<Vec<Rect>> {
    fn root(parent: &mut [u32], mut run: u32) -> u32 {
        while parent[run as usize] != run {
            let up = parent[parent[run as usize] as usize];
            parent[run as usize] = up;
            run = up;
        }
        run
    }
    let width = mask.width() as usize;
    // Per run: its union-find parent and its box, kept whole at each root.
    let mut parent: Vec<u32> = Vec::new();
    let mut boxes: Vec<Rect> = Vec::new();
    // Runs of the row above and of this row: start, end (exclusive), run.
    let (mut above, mut current): (Vec<(usize, usize, u32)>, Vec<_>) = (Vec::new(), Vec::new());
    for (y, line) in mask.as_raw().chunks_exact(width).enumerate() {
        current.clear();
        let (mut x, mut first) = (0, 0);
        while x < width {
            if line[x] < MASK_THRESHOLD {
                x += 1;
                continue;
            }
            let start = x;
            while x < width && line[x] >= MASK_THRESHOLD {
                x += 1;
            }
            if boxes.len() == MAX_MASK_RUNS {
                return None;
            }
            let run = boxes.len() as u32;
            parent.push(run);
            boxes.push(Rect { x: start as u32, y: y as u32, width: (x - start) as u32, height: 1 });
            current.push((start, x, run));
            // Runs above that touch this one, diagonals included.
            while first < above.len() && above[first].1 < start {
                first += 1;
            }
            for &(_, _, other) in above[first..].iter().take_while(|r| r.0 <= x) {
                let (a, b) = (root(&mut parent, other), root(&mut parent, run));
                if a != b {
                    parent[b as usize] = a;
                    boxes[a as usize] = union(boxes[a as usize], boxes[b as usize]);
                }
            }
        }
        std::mem::swap(&mut above, &mut current);
    }
    let mut regions = Vec::new();
    for run in 0..parent.len() {
        if parent[run] == run as u32 {
            regions.push(boxes[run]);
        }
    }
    Some(regions)
}

/// Bounding box of the pixels ≥ [`MASK_THRESHOLD`].
pub(crate) fn mask_bounds(mask: &GrayImage) -> Option<Rect> {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for (x, y, pixel) in mask.enumerate_pixels() {
        if pixel[0] >= MASK_THRESHOLD {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    (x0 != u32::MAX).then(|| Rect { x: x0, y: y0, width: x1 - x0 + 1, height: y1 - y0 + 1 })
}

/// Feather half-width for a hole: a little wider for larger objects.
fn feather_for(bounds: Rect) -> u32 {
    (bounds.width.max(bounds.height) / 48).clamp(2, 12)
}

/// `rect` grown by `by` on every side, inside `width × height`.
fn grow(rect: Rect, by: u32, width: u32, height: u32) -> Rect {
    let x = rect.x.saturating_sub(by);
    let y = rect.y.saturating_sub(by);
    let right = rect.right().saturating_add(by).min(width);
    let bottom = rect.bottom().saturating_add(by).min(height);
    Rect { x, y, width: right - x, height: bottom - y }
}

/// The box LaMa sees: the hole with context on each side (a quarter of
/// the hole's width plus height, at least [`MIN_CONTEXT`]), inside the
/// image, at least [`MIN_SIDE`] on each side.
pub(crate) fn crop_around(hole: Rect, width: u32, height: u32) -> Rect {
    let context = ((hole.width + hole.height) / 4).max(MIN_CONTEXT);
    let mut crop = grow(hole, context, width, height);
    for (start, len, limit) in
        [(&mut crop.x, &mut crop.width, width), (&mut crop.y, &mut crop.height, height)]
    {
        if *len < MIN_SIDE {
            *start = (*start).min(limit - MIN_SIDE);
            *len = MIN_SIDE;
        }
    }
    crop
}

/// Size a crop runs at: scaled so its longer side is at most
/// [`MAX_RUN_SIDE`], each side at least [`MIN_SIDE`].
pub(crate) fn run_size(crop: Rect) -> (u32, u32) {
    let long = crop.width.max(crop.height);
    if long <= MAX_RUN_SIDE {
        return (crop.width, crop.height);
    }
    let scale = f64::from(MAX_RUN_SIDE) / f64::from(long);
    let side = |len: u32| ((f64::from(len) * scale).round() as u32).clamp(MIN_SIDE, MAX_RUN_SIDE);
    (side(crop.width), side(crop.height))
}

/// Memory a fill needs at its largest crop: the buffers alive together at
/// the peak, per run, crop and image pixel.
fn budget(width: u32, height: u32, plans: &[Plan], alpha: bool) -> Budget {
    let image = u64::from(width) * u64::from(height);
    let crop = |plan: &Plan| {
        let (rw, rh) = plan.run;
        let run = u64::from(rw) * u64::from(rh);
        // Run: RGB8 in, mask, RGB8 out. Crop: f32 weight, grown hole, RGB8
        // crop, resampler pass (the hole filters peak lower, at 8 B).
        run * (RUN_BYTES_PER_PIXEL + 3 + 1 + 3) + plan.crop.pixels() * (4 + 1 + 3 + 3)
    };
    // RGBA8 copy and RGB8 result; with alpha also its plane and RGBA8 result.
    let per_image_pixel = if alpha { 4 + 3 + 1 + 4 } else { 4 + 3 };
    Budget {
        load: LOAD_BYTES,
        run: RUN_FIXED_BYTES + plans.iter().map(crop).max().unwrap_or(0) + image * per_image_pixel,
    }
}

/// A grayscale mask from an image: luma times alpha, so white opaque
/// pixels are filled and transparent ones kept.
pub fn mask_from_image(image: &DynamicImage) -> GrayImage {
    let la = image.to_luma_alpha8();
    let (w, h) = la.dimensions();
    GrayImage::from_fn(w, h, |x, y| {
        let [l, a] = la.get_pixel(x, y).0;
        image::Luma([((u16::from(l) * u16::from(a) + 127) / 255) as u8])
    })
}

/// Fill the pixels of `input` where `mask` (same size, one byte per pixel)
/// is at least [`MASK_THRESHOLD`]. An empty mask returns the image as is.
pub fn inpaint(
    input: &DynamicImage,
    mask: &GrayImage,
    mut progress: impl FnMut(Stage) -> ControlFlow<()>,
) -> Result<DynamicImage, AiError> {
    let (width, height) = (input.width(), input.height());
    if mask.dimensions() != (width, height) {
        return Err(AiError::Processing(format!(
            "the mask is {}×{} px; the image is {width}×{height}",
            mask.width(),
            mask.height()
        )));
    }
    if u64::from(width) * u64::from(height) * 4 > MAX_BYTES {
        return Err(AiError::Processing("image exceeds the 512 MiB RGBA limit".into()));
    }
    let plans = plans(mask)?;
    if plans.is_empty() {
        return Ok(input.clone());
    }
    let mut budget = budget(width, height, &plans, input.color().has_alpha());
    budget.run = budget.run.saturating_add(engine::float_copy_bytes(input));
    #[cfg(feature = "native")]
    {
        SLOT.run(&MODEL, budget, &mut progress, |model, progress| {
            let mut source = engine::display_rgba8(input);
            let count = plans.len() as f32;
            for (index, plan) in plans.iter().enumerate() {
                let Hole { grown, weight } = Hole::new(mask, plan);
                let crop = crop_rgb(&source, plan.crop);
                let (run_rgb, run_mask) = to_run_size(crop, grown, plan)?;
                let (rw, rh) = engine::dims(plan.run.0, plan.run.1);
                let filled =
                    model.run_with_progress(&run_rgb, &run_mask, rw, rh, |part: f32| {
                        progress(Stage::Running(((index as f32 + part) / count).clamp(0.0, 1.0)))
                    })?;
                let filled = from_run_size(filled, plan)?;
                paste(&mut source, &filled, &weight, plan.crop);
            }
            let alpha = engine::alpha_of(input, &source);
            engine::assemble(engine::rgb_bytes(&source), alpha.as_deref(), width, height)
        })
    }
    #[cfg(not(feature = "native"))]
    {
        let _ = (budget, &mut progress);
        Err(AiError::Disabled)
    }
}

/// The hole inside the crop: the mask grown by `2 × feather` (what LaMa
/// fills) and the blend weight (1 on the mask, 0 outside the hole).
pub(crate) struct Hole {
    /// 255 where LaMa fills, crop-sized.
    pub grown: Vec<u8>,
    /// Blend weight 0..=1 of the fill, crop-sized.
    pub weight: Vec<f32>,
}

impl Hole {
    pub(crate) fn new(mask: &GrayImage, plan: &Plan) -> Self {
        let Rect { x, y, width, height } = plan.crop;
        let (w, h) = (width as usize, height as usize);
        let mut binary = Vec::with_capacity(w * h);
        for line in mask.as_raw().chunks_exact(mask.width() as usize).skip(y as usize).take(h) {
            let line = &line[x as usize..][..w];
            binary.extend(line.iter().map(|&v| if v >= MASK_THRESHOLD { 255 } else { 0 }));
        }
        let k = plan.feather as usize;
        let half = dilate(&binary, w, h, k);
        drop(binary);
        let grown = dilate(&half, w, h, k);
        let weight = box_blur(&half, w, h, k);
        Self { grown, weight }
    }
}

/// Largest filter radius: a row window of 255s plus the entering pixel
/// must fit a `u16`.
const MAX_FILTER_RADIUS: usize = 127;

/// `finish` of the sum of `values` over the `(2 radius + 1)²` square around
/// each pixel, as running sums: rows, then columns a whole row at a time.
/// Beyond the image an edge pixel repeats when `repeat_edges`, else counts
/// as zero.
fn box_filter<T: Copy + Default>(
    values: &[u8],
    width: usize,
    height: usize,
    radius: usize,
    repeat_edges: bool,
    finish: impl Fn(u32) -> T,
) -> Vec<T> {
    assert!(radius <= MAX_FILTER_RADIUS, "filter radius {radius}");
    let r = radius as isize;
    let mut rows = vec![0u16; values.len()];
    for (line, out) in values.chunks_exact(width).zip(rows.chunks_exact_mut(width)) {
        let at = |i: isize| -> u16 {
            match usize::try_from(i) {
                Ok(i) if i < width => u16::from(line[i]),
                _ if !repeat_edges => 0,
                Ok(_) => u16::from(line[width - 1]),
                Err(_) => u16::from(line[0]),
            }
        };
        let mut sum: u16 = (-r..=r).map(at).sum();
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = sum;
            let i = i as isize;
            sum = sum + at(i + r + 1) - at(i - r);
        }
    }
    let row = |y: isize| -> Option<&[u16]> {
        let y = match usize::try_from(y) {
            Ok(y) if y < height => y,
            _ if !repeat_edges => return None,
            Ok(_) => height - 1,
            Err(_) => 0,
        };
        Some(&rows[y * width..][..width])
    };
    let mut sums = vec![0u32; width];
    for y in -r..=r {
        if let Some(line) = row(y) {
            sums.iter_mut().zip(line).for_each(|(s, &v)| *s += u32::from(v));
        }
    }
    let mut out = vec![T::default(); values.len()];
    for (y, line) in out.chunks_exact_mut(width).enumerate() {
        line.iter_mut().zip(&sums).for_each(|(o, &s)| *o = finish(s));
        let y = y as isize;
        if let Some(entering) = row(y + r + 1) {
            sums.iter_mut().zip(entering).for_each(|(s, &v)| *s += u32::from(v));
        }
        if let Some(leaving) = row(y - r) {
            sums.iter_mut().zip(leaving).for_each(|(s, &v)| *s -= u32::from(v));
        }
    }
    out
}

/// Square (Chebyshev) dilation of a 0/255 mask by `radius`: 255 where a
/// nonzero pixel lies within `radius` rows and columns.
pub(crate) fn dilate(mask: &[u8], width: usize, height: usize, radius: usize) -> Vec<u8> {
    box_filter(mask, width, height, radius, false, |count| if count > 0 { 255 } else { 0 })
}

/// Mean over the `(2 radius + 1)²` square around each pixel (edges
/// repeated), scaled to 0..=1.
pub(crate) fn box_blur(mask: &[u8], width: usize, height: usize, radius: usize) -> Vec<f32> {
    let full = 255 * (2 * radius as u32 + 1).pow(2);
    box_filter(mask, width, height, radius, true, |sum| sum as f32 / full as f32)
}

/// RGB8 bytes of `rect` of `source`.
fn crop_rgb(source: &image::RgbaImage, rect: Rect) -> image::RgbImage {
    image::RgbImage::from_fn(rect.width, rect.height, |col, row| {
        let p = source.get_pixel(rect.x + col, rect.y + row);
        image::Rgb([p[0], p[1], p[2]])
    })
}

fn resize(image: DynamicImage, (width, height): (u32, u32)) -> Result<DynamicImage, AiError> {
    if (image.width(), image.height()) == (width, height) {
        return Ok(image);
    }
    bigimage_core::resize::apply_to(
        &image,
        bigimage_core::ResizeMode::Exact { width, height },
        bigimage_core::Filter::Lanczos3,
    )
    .map_err(|e| AiError::Processing(e.to_string()))
}

/// The crop and its hole at the run size. A scaled-down hole keeps every
/// run pixel the full-size hole touches.
pub(crate) fn to_run_size(
    crop: image::RgbImage,
    grown: Vec<u8>,
    plan: &Plan,
) -> Result<(Vec<u8>, Vec<u8>), AiError> {
    let rgb = resize(DynamicImage::ImageRgb8(crop), plan.run)?.into_rgb8().into_raw();
    let (cw, ch) = (plan.crop.width, plan.crop.height);
    if plan.run == (cw, ch) {
        return Ok((rgb, grown));
    }
    let (rw, rh) = plan.run;
    let mut mask = vec![0u8; (rw * rh) as usize];
    // Every full-size hole pixel marks the run pixel it falls in.
    for row in 0..ch {
        let ry = ((u64::from(row) * u64::from(rh)) / u64::from(ch)) as u32;
        for col in 0..cw {
            if grown[(row * cw + col) as usize] != 0 {
                let rx = ((u64::from(col) * u64::from(rw)) / u64::from(cw)) as u32;
                mask[(ry * rw + rx) as usize] = 255;
            }
        }
    }
    Ok((rgb, mask))
}

/// LaMa's run-size RGB back at the crop's size.
pub(crate) fn from_run_size(filled: Vec<u8>, plan: &Plan) -> Result<image::RgbImage, AiError> {
    let (rw, rh) = plan.run;
    let filled = image::RgbImage::from_raw(rw, rh, filled)
        .ok_or_else(|| AiError::Processing("LaMa output has an unexpected size".into()))?;
    Ok(resize(DynamicImage::ImageRgb8(filled), (plan.crop.width, plan.crop.height))?.into_rgb8())
}

/// Blend `filled` (crop-sized) into `target` at `rect` by `weight`;
/// alpha untouched.
pub(crate) fn paste(
    target: &mut image::RgbaImage,
    filled: &image::RgbImage,
    weight: &[f32],
    rect: Rect,
) {
    for (index, (col, row, fill)) in filled.enumerate_pixels().enumerate() {
        let w = weight[index];
        if w <= 0.0 {
            continue;
        }
        let pixel = target.get_pixel_mut(rect.x + col, rect.y + row);
        for c in 0..3 {
            let blended = f32::from(pixel[c]) * (1.0 - w) + f32::from(fill[c]) * w;
            pixel[c] = blended.round().clamp(0.0, 255.0) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square_mask(width: u32, height: u32, rect: Rect) -> GrayImage {
        GrayImage::from_fn(width, height, |x, y| {
            let inside = x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom();
            image::Luma([if inside { 255 } else { 0 }])
        })
    }

    fn only_plan(mask: &GrayImage) -> Plan {
        let plans = plans(mask).unwrap();
        assert_eq!(plans.len(), 1, "{plans:?}");
        plans[0]
    }

    fn paint(mask: &mut GrayImage, rect: Rect) {
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                mask.put_pixel(x, y, image::Luma([255]));
            }
        }
    }

    #[test]
    fn distant_marks_run_as_separate_crops() {
        // Two 40 px marks at opposite corners of a 6000×4000 photo.
        let mut mask = GrayImage::new(6000, 4000);
        let first = Rect { x: 200, y: 300, width: 40, height: 40 };
        let second = Rect { x: 5600, y: 3500, width: 40, height: 40 };
        paint(&mut mask, second);
        paint(&mut mask, first);
        let plans = plans(&mask).unwrap();
        assert_eq!(plans.len(), 2, "{plans:?}");
        for (plan, mark) in plans.iter().zip([first, second]) {
            assert_eq!(*plan, plan_for(mark, 6000, 4000));
            assert_eq!(plan.run, (plan.crop.width, plan.crop.height));
        }
        // A third mark whose crop reaches the first joins its group.
        paint(&mut mask, Rect { x: 300, y: 300, width: 10, height: 10 });
        let plans = super::plans(&mask).unwrap();
        assert_eq!(plans.len(), 2, "{plans:?}");
        assert_eq!(plans[0], plan_for(Rect { x: 200, y: 300, width: 110, height: 40 }, 6000, 4000));
    }

    #[test]
    fn regions_join_diagonal_runs() {
        let mut mask = GrayImage::new(12, 6);
        // A diagonal line, a U shape whose arms meet only below, and a dot.
        for i in 0..4 {
            mask.put_pixel(i, i, image::Luma([255]));
        }
        for (x, y) in [(6, 0), (6, 1), (6, 2), (7, 2), (8, 2), (8, 1), (8, 0), (11, 5)] {
            mask.put_pixel(x, y, image::Luma([255]));
        }
        let mut regions = mask_regions(&mask).unwrap();
        regions.sort_by_key(|r| (r.x, r.y));
        assert_eq!(
            regions,
            [
                Rect { x: 0, y: 0, width: 4, height: 4 },
                Rect { x: 6, y: 0, width: 3, height: 3 },
                Rect { x: 11, y: 5, width: 1, height: 1 },
            ]
        );
    }

    #[test]
    fn bounds_follow_the_threshold() {
        let mut mask = GrayImage::new(20, 10);
        assert_eq!(mask_bounds(&mask), None);
        mask.put_pixel(3, 2, image::Luma([MASK_THRESHOLD - 1]));
        assert_eq!(mask_bounds(&mask), None);
        mask.put_pixel(4, 5, image::Luma([MASK_THRESHOLD]));
        mask.put_pixel(9, 7, image::Luma([255]));
        assert_eq!(mask_bounds(&mask), Some(Rect { x: 4, y: 5, width: 6, height: 3 }));
    }

    #[test]
    fn small_holes_keep_context_and_run_at_full_size() {
        let hole = Rect { x: 500, y: 400, width: 40, height: 30 };
        let mask = square_mask(2000, 1500, hole);
        let plan = only_plan(&mask);
        assert_eq!(plan.feather, 2);
        // Hole grown by 4, then at least 64 px of context.
        assert_eq!(plan.crop, Rect { x: 432, y: 332, width: 176, height: 166 });
        assert_eq!(plan.run, (176, 166));
    }

    #[test]
    fn crops_stay_inside_the_image_and_at_least_nine_pixels() {
        let hole = Rect { x: 0, y: 0, width: 1, height: 1 };
        let crop = crop_around(hole, 9, 9);
        assert_eq!(crop, Rect { x: 0, y: 0, width: 9, height: 9 });
        let crop = crop_around(Rect { x: 95, y: 3, width: 5, height: 2 }, 100, 9);
        assert_eq!((crop.right(), crop.height), (100, 9));
        assert!(plans(&GrayImage::new(8, 20)).is_err());
        assert!(plans(&GrayImage::new(9, 9)).unwrap().is_empty());
    }

    #[test]
    fn large_crops_are_scaled_down_to_the_run_side() {
        assert_eq!(run_size(Rect { x: 0, y: 0, width: 1024, height: 700 }), (1024, 700));
        assert_eq!(run_size(Rect { x: 0, y: 0, width: 4096, height: 2048 }), (1024, 512));
        // A thin strip keeps LaMa's minimum side.
        assert_eq!(run_size(Rect { x: 0, y: 0, width: 8000, height: 20 }), (1024, MIN_SIDE));
        let mask = square_mask(6000, 4000, Rect { x: 1000, y: 1000, width: 3000, height: 2000 });
        let plan = only_plan(&mask);
        assert!(plan.run.0 <= MAX_RUN_SIDE && plan.run.1 <= MAX_RUN_SIDE);
        // Hole grown by 24 (feather 12), then (3048 + 2048) / 4 of context.
        assert_eq!(plan.crop, Rect { x: 0, y: 0, width: 5298, height: 4000 });
        assert_eq!(plan.run, (1024, 773));
        // The budget follows the run size, not the crop.
        let b = budget(6000, 4000, std::slice::from_ref(&plan), true);
        assert!(b.run < 1024 * MIB, "{b:?}");
    }

    #[test]
    fn dilation_and_blur_make_a_ring_that_is_full_inside_the_mask() {
        let (w, h) = (15usize, 15usize);
        let mut mask = vec![0u8; w * h];
        mask[7 * w + 7] = 255;
        let grown = dilate(&mask, w, h, 2);
        let count = grown.iter().filter(|&&v| v == 255).count();
        assert_eq!(count, 25);
        let weight = box_blur(&grown, w, h, 2);
        assert!((weight[7 * w + 7] - 1.0).abs() < 1e-6);
        // Fades out over the ring and is zero beyond it.
        assert!(weight[7 * w + 10] > 0.0 && weight[7 * w + 10] < 1.0);
        assert_eq!(weight[7 * w + 12], 0.0);
        assert_eq!(weight[0], 0.0);
    }

    /// Reference dilation: the maximum over each clipped window.
    fn naive_dilate(mask: &[u8], width: usize, height: usize, radius: usize) -> Vec<u8> {
        let mut out = vec![0u8; mask.len()];
        for y in 0..height {
            for x in 0..width {
                let rows = y.saturating_sub(radius)..=(y + radius).min(height - 1);
                let cols = x.saturating_sub(radius)..=(x + radius).min(width - 1);
                out[y * width + x] = rows
                    .flat_map(|j| cols.clone().map(move |i| mask[j * width + i]))
                    .max()
                    .unwrap_or(0);
            }
        }
        out
    }

    /// Reference blur: the mean over each window with edge pixels repeated.
    fn naive_box_blur(mask: &[u8], width: usize, height: usize, radius: usize) -> Vec<f32> {
        let clamp = |i: usize, len: usize| i.saturating_sub(radius).min(len - 1);
        let span = (2 * radius + 1) as f64;
        let mut out = vec![0f32; mask.len()];
        for y in 0..height {
            for x in 0..width {
                let mut sum = 0f64;
                for dy in 0..=2 * radius {
                    for dx in 0..=2 * radius {
                        sum +=
                            f64::from(mask[clamp(y + dy, height) * width + clamp(x + dx, width)]);
                    }
                }
                out[y * width + x] = (sum / 255.0 / span / span) as f32;
            }
        }
        out
    }

    #[test]
    fn filters_match_the_naive_windows() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for (width, height) in [(1, 1), (1, 7), (9, 1), (5, 9), (23, 17), (40, 3)] {
            for density in [8u64, 64, 200] {
                let mask: Vec<u8> = (0..width * height)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        if state % 256 < density {
                            255
                        } else {
                            0
                        }
                    })
                    .collect();
                for radius in [0, 1, 2, 5, 12] {
                    let case = format!("{width}×{height}, density {density}, radius {radius}");
                    assert_eq!(
                        dilate(&mask, width, height, radius),
                        naive_dilate(&mask, width, height, radius),
                        "{case}"
                    );
                    let fast = box_blur(&mask, width, height, radius);
                    let slow = naive_box_blur(&mask, width, height, radius);
                    for (a, b) in fast.iter().zip(&slow) {
                        assert!((a - b).abs() < 1e-6, "{case}: {a} vs {b}");
                    }
                }
            }
        }
    }

    #[test]
    fn hole_weight_is_one_on_the_mask_and_zero_outside_the_hole() {
        let rect = Rect { x: 40, y: 40, width: 20, height: 20 };
        let mask = square_mask(200, 200, rect);
        let plan = only_plan(&mask);
        let hole = Hole::new(&mask, &plan);
        let cw = plan.crop.width as usize;
        let at = |x: u32, y: u32| ((y - plan.crop.y) as usize) * cw + (x - plan.crop.x) as usize;
        for (x, y) in [(40, 40), (59, 59), (50, 45)] {
            assert_eq!(hole.weight[at(x, y)], 1.0, "({x},{y})");
            assert_eq!(hole.grown[at(x, y)], 255);
        }
        let ring = 2 * plan.feather;
        // Just beyond the grown hole: not filled, not blended.
        let outside = at(60 + ring, 50);
        assert_eq!(hole.grown[outside], 0);
        assert_eq!(hole.weight[outside], 0.0);
        // In the ring: filled, partly blended.
        let inside_ring = at(60 + ring - 1, 50);
        assert_eq!(hole.grown[inside_ring], 255);
        assert!(hole.weight[inside_ring] > 0.0 && hole.weight[inside_ring] < 1.0);
    }

    #[test]
    fn scaled_down_hole_keeps_every_touched_run_pixel() {
        let plan =
            Plan { crop: Rect { x: 0, y: 0, width: 40, height: 20 }, run: (10, 5), feather: 2 };
        let crop = image::RgbImage::from_pixel(40, 20, image::Rgb([5, 6, 7]));
        let mut grown = vec![0u8; 40 * 20];
        grown[7 * 40 + 13] = 255; // one full-size pixel
        let (rgb, mask) = to_run_size(crop, grown, &plan).unwrap();
        assert_eq!(rgb.len(), 10 * 5 * 3);
        assert_eq!(mask.iter().filter(|&&v| v != 0).count(), 1);
        assert_eq!(mask[10 + 3], 255); // row 7·5/20 = 1, col 13·10/40 = 3
        let back = from_run_size(rgb, &plan).unwrap();
        assert_eq!(back.dimensions(), (40, 20));
        assert!(back.pixels().all(|p| p.0 == [5, 6, 7]));
        assert!(from_run_size(vec![0; 7], &plan).is_err());
    }

    #[test]
    fn paste_blends_by_weight_and_keeps_alpha_and_outside_pixels() {
        let mut target = image::RgbaImage::from_pixel(4, 3, image::Rgba([100, 100, 100, 77]));
        let rect = Rect { x: 1, y: 1, width: 2, height: 2 };
        let filled = image::RgbImage::from_pixel(2, 2, image::Rgb([200, 0, 50]));
        paste(&mut target, &filled, &[1.0, 0.5, 0.0, 0.25], rect);
        assert_eq!(target.get_pixel(1, 1).0, [200, 0, 50, 77]);
        assert_eq!(target.get_pixel(2, 1).0, [150, 50, 75, 77]);
        assert_eq!(target.get_pixel(1, 2).0, [100, 100, 100, 77]);
        assert_eq!(target.get_pixel(2, 2).0, [125, 75, 88, 77]);
        assert_eq!(target.get_pixel(0, 0).0, [100, 100, 100, 77]);
    }

    #[test]
    fn masks_from_images_use_luma_times_alpha() {
        let image = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(3, 1, |x, _| match x {
            0 => image::Rgba([255, 255, 255, 255]),
            1 => image::Rgba([255, 255, 255, 0]),
            _ => image::Rgba([0, 0, 0, 255]),
        }));
        assert_eq!(mask_from_image(&image).into_raw(), vec![255, 0, 0]);
    }

    #[test]
    fn mismatched_masks_are_refused_and_empty_masks_change_nothing() {
        let image = DynamicImage::new_rgb8(20, 20);
        let none = |_| ControlFlow::Continue(());
        assert!(matches!(
            inpaint(&image, &GrayImage::new(20, 19), none),
            Err(AiError::Processing(_))
        ));
        let same = inpaint(&image, &GrayImage::new(20, 20), none).unwrap();
        assert_eq!(same.as_bytes(), image.as_bytes());
    }
}
