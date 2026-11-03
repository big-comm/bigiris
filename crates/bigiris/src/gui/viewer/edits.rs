// SPDX-License-Identifier: GPL-3.0-or-later
//! Pure rules behind the editing panel: how slider adjustments become
//! undoable operations, how the size fields keep their proportion and
//! which operations actually reach the pixels.
//!
//! Nothing here touches GTK, so every rule is unit-tested.

use super::super::EditOp;

/// Tone values as the panel sliders hold them. Gamma is kept in
/// hundredths so operations stay `Eq` and compare exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::gui) struct AdjustValues {
    pub brightness: i32,
    pub contrast: i32,
    pub saturation: i32,
    pub gamma_pct: u32,
}

impl Default for AdjustValues {
    fn default() -> Self {
        Self::NEUTRAL
    }
}

impl AdjustValues {
    pub(in crate::gui) const NEUTRAL: Self =
        Self { brightness: 0, contrast: 0, saturation: 0, gamma_pct: 100 };

    pub(in crate::gui) fn op(self) -> EditOp {
        EditOp::Adjust {
            brightness: self.brightness,
            contrast: self.contrast,
            saturation: self.saturation,
            gamma_pct: self.gamma_pct,
        }
    }

    pub(in crate::gui) fn from_op(op: EditOp) -> Option<Self> {
        match op {
            EditOp::Adjust { brightness, contrast, saturation, gamma_pct } => {
                Some(Self { brightness, contrast, saturation, gamma_pct })
            }
            _ => None,
        }
    }

    /// The same values in the units `bigimage_core::adjust` expects,
    /// clamped to its accepted ranges.
    pub(in crate::gui) fn core(self) -> bigimage_core::AdjustOps {
        bigimage_core::AdjustOps {
            brightness: self.brightness.clamp(-100, 100),
            contrast: self.contrast.clamp(-100, 100) as f32,
            saturation: self.saturation.clamp(-100, 100) as f32,
            gamma: (self.gamma_pct.clamp(10, 1000) as f32) / 100.0,
        }
    }
}

/// Panel sliders whose gestures replace, rather than stack on, the
/// previous gesture of the same slider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::gui) enum Slider {
    Adjust,
    Straighten,
}

fn slider_of(op: &EditOp) -> Option<Slider> {
    match op {
        EditOp::Adjust { .. } => Some(Slider::Adjust),
        EditOp::Straighten { .. } => Some(Slider::Straighten),
        _ => None,
    }
}

/// Every slider movement replaces the previous value instead of stacking
/// on it: a run of consecutive operations of the same slider (`Adjust`
/// or `Straighten`) is worth only its last one. Each committed gesture is
/// still its own history entry, so Undo goes back to the previous slider
/// position, not to the untouched image. Returns the operations that
/// reach the pixels.
pub(in crate::gui) fn effective_ops(ops: &[EditOp]) -> Vec<EditOp> {
    ops.iter()
        .enumerate()
        .filter(|(i, op)| {
            slider_of(op).is_none() || ops.get(i + 1).and_then(slider_of) != slider_of(op)
        })
        .map(|(_, op)| *op)
        .collect()
}

/// What the sliders show for `ops`: the trailing adjustment, or neutral
/// when the last edit was something else.
pub(in crate::gui) fn trailing_adjust(ops: &[EditOp]) -> AdjustValues {
    ops.last().and_then(|op| AdjustValues::from_op(*op)).unwrap_or(AdjustValues::NEUTRAL)
}

/// The straighten angle in tenths of a degree the slider shows for `ops`.
pub(in crate::gui) fn trailing_straighten(ops: &[EditOp]) -> i32 {
    match ops.last() {
        Some(EditOp::Straighten { tenths }) => *tenths,
        _ => 0,
    }
}

/// The image a slider works on: every effective edit except the trailing
/// gesture of that slider, which it is replacing.
pub(in crate::gui) fn base_ops(ops: &[EditOp], slider: Slider) -> Vec<EditOp> {
    let mut effective = effective_ops(ops);
    if effective.last().and_then(slider_of) == Some(slider) {
        effective.pop();
    }
    effective
}

/// The operation that records a finished adjustment gesture, or `None`
/// when the sliders ended where they started.
pub(in crate::gui) fn adjust_commit(ops: &[EditOp], values: AdjustValues) -> Option<EditOp> {
    (trailing_adjust(ops) != values).then(|| values.op())
}

/// The operation that records a finished straighten gesture.
pub(in crate::gui) fn straighten_commit(ops: &[EditOp], tenths: i32) -> Option<EditOp> {
    (trailing_straighten(ops) != tenths).then_some(EditOp::Straighten { tenths })
}

/// Operations too slow to redo on every edit: their results are cached.
pub(in crate::gui) fn is_heavy(op: &EditOp) -> bool {
    matches!(op, EditOp::RemoveBackground | EditOp::Upscale { .. })
}

/// Index and length of the longest of `prefixes` that `ops` starts with.
/// Ties keep the first candidate.
pub(in crate::gui) fn longest_prefix<'a>(
    prefixes: impl IntoIterator<Item = &'a [EditOp]>,
    ops: &[EditOp],
) -> Option<(usize, usize)> {
    prefixes.into_iter().enumerate().filter(|(_, prefix)| ops.starts_with(prefix)).fold(
        None,
        |best: Option<(usize, usize)>, (i, prefix)| match best {
            Some((_, len)) if len >= prefix.len() => best,
            _ => Some((i, prefix.len())),
        },
    )
}

/// Images after already-computed prefixes of the effective edits of one
/// file, so heavy operations (background removal, upscale) are not run
/// again on every later edit, Undo or Redo. Bounded: at most
/// [`EditCache::CAPACITY`] entries, most recently used first.
#[derive(Default)]
pub(in crate::gui) struct EditCache {
    entries: Vec<CacheEntry>,
}

struct CacheEntry {
    path: std::path::PathBuf,
    modified: Option<std::time::SystemTime>,
    ops: Vec<EditOp>,
    image: std::sync::Arc<image::DynamicImage>,
}

impl EditCache {
    pub(in crate::gui) const CAPACITY: usize = 2;
    /// Larger results are recomputed instead of held in memory.
    const MAX_PIXELS: u64 = 100_000_000;

    /// The longest cached prefix of `ops` for this version of `path`.
    pub(in crate::gui) fn best(
        &mut self,
        path: &std::path::Path,
        modified: Option<std::time::SystemTime>,
        ops: &[EditOp],
    ) -> Option<(usize, std::sync::Arc<image::DynamicImage>)> {
        let candidates: Vec<usize> = (0..self.entries.len())
            .filter(|&i| self.entries[i].path == path && self.entries[i].modified == modified)
            .collect();
        let (pick, len) =
            longest_prefix(candidates.iter().map(|&i| self.entries[i].ops.as_slice()), ops)?;
        let index = candidates[pick];
        let entry = self.entries.remove(index);
        let image = entry.image.clone();
        self.entries.insert(0, entry);
        Some((len, image))
    }

    pub(in crate::gui) fn insert(
        &mut self,
        path: &std::path::Path,
        modified: Option<std::time::SystemTime>,
        ops: Vec<EditOp>,
        image: std::sync::Arc<image::DynamicImage>,
    ) {
        if u64::from(image.width()) * u64::from(image.height()) > Self::MAX_PIXELS {
            return;
        }
        // Other files and older versions of this one are never used again.
        self.entries.retain(|e| e.path == path && e.modified == modified && e.ops != ops);
        self.entries.insert(0, CacheEntry { path: path.to_path_buf(), modified, ops, image });
        self.entries.truncate(Self::CAPACITY);
    }

    pub(in crate::gui) fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    pub(in crate::gui) fn cached_ops(&self) -> Vec<Vec<EditOp>> {
        self.entries.iter().map(|e| e.ops.clone()).collect()
    }
}

/// Largest side accepted by the size fields.
pub(in crate::gui) const MAX_SIDE: u32 = 65_535;

/// The other side of a locked resize: `changed` is the new value of one
/// side of an image currently `from.0 × from.1` (`width` says which).
pub(in crate::gui) fn locked_side(changed: u32, from: (u32, u32), width: bool) -> u32 {
    let (this, other) = if width { (from.0, from.1) } else { (from.1, from.0) };
    if this == 0 || other == 0 {
        return changed.clamp(1, MAX_SIDE);
    }
    let value = (f64::from(changed) * f64::from(other) / f64::from(this)).round();
    (value as u32).clamp(1, MAX_SIDE)
}

/// Both sides scaled by `percent`, never below one pixel.
pub(in crate::gui) fn percent_size(from: (u32, u32), percent: f64) -> (u32, u32) {
    let scale = |side: u32| {
        ((f64::from(side) * percent / 100.0).round().max(1.0) as u32).clamp(1, MAX_SIDE)
    };
    (scale(from.0), scale(from.1))
}

/// Size of a downscaled working copy for live previews: at most
/// `max_pixels`, proportional, never enlarged.
pub(in crate::gui) fn preview_size(from: (u32, u32), max_pixels: u64) -> (u32, u32) {
    let pixels = u64::from(from.0) * u64::from(from.1);
    if pixels <= max_pixels || pixels == 0 {
        return (from.0.max(1), from.1.max(1));
    }
    let scale = (max_pixels as f64 / pixels as f64).sqrt();
    let side = |v: u32| ((f64::from(v) * scale).floor() as u32).max(1);
    (side(from.0), side(from.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adj(brightness: i32) -> EditOp {
        AdjustValues { brightness, ..AdjustValues::NEUTRAL }.op()
    }

    #[test]
    fn consecutive_adjustments_collapse_to_the_last() {
        let ops = [EditOp::RotateCw, adj(10), adj(20), EditOp::FlipH, adj(5), adj(-5)];
        assert_eq!(effective_ops(&ops), vec![EditOp::RotateCw, adj(20), EditOp::FlipH, adj(-5)]);
        assert_eq!(effective_ops(&[]), Vec::<EditOp>::new());
        assert_eq!(effective_ops(&[adj(1)]), vec![adj(1)]);
    }

    #[test]
    fn sliders_follow_the_trailing_adjustment() {
        assert_eq!(trailing_adjust(&[]), AdjustValues::NEUTRAL);
        assert_eq!(trailing_adjust(&[adj(10), EditOp::RotateCw]), AdjustValues::NEUTRAL);
        assert_eq!(trailing_adjust(&[adj(10), adj(30)]).brightness, 30);
        // Undo of the second gesture brings the first values back.
        assert_eq!(trailing_adjust(&[adj(10)]).brightness, 10);
    }

    #[test]
    fn base_excludes_the_gesture_being_replaced() {
        let base = |ops: &[EditOp]| base_ops(ops, Slider::Adjust);
        assert_eq!(base(&[EditOp::RotateCw, adj(10), adj(30)]), [EditOp::RotateCw]);
        assert_eq!(base(&[adj(10), EditOp::FlipV]), [adj(10), EditOp::FlipV]);
        let turn = |tenths| EditOp::Straighten { tenths };
        let ops = [adj(10), turn(15), turn(-20)];
        assert_eq!(base_ops(&ops, Slider::Straighten), [adj(10)]);
        assert_eq!(base_ops(&ops, Slider::Adjust), [adj(10), turn(-20)]);
        assert_eq!(trailing_straighten(&ops), -20);
        assert_eq!(trailing_straighten(&[turn(5), adj(1)]), 0);
        assert_eq!(straighten_commit(&ops, -20), None);
        assert_eq!(straighten_commit(&[adj(1)], 0), None);
        assert_eq!(straighten_commit(&ops, 0), Some(turn(0)));
        // Different sliders never collapse into each other.
        assert_eq!(effective_ops(&[turn(1), adj(2), turn(3)]), [turn(1), adj(2), turn(3)]);
        assert_eq!(effective_ops(&[turn(1), turn(2), adj(3)]), [turn(2), adj(3)]);
    }

    #[test]
    fn longest_cached_prefix_wins() {
        let a = [EditOp::RotateCw];
        let ab = [EditOp::RotateCw, EditOp::RemoveBackground];
        let ops = [EditOp::RotateCw, EditOp::RemoveBackground, EditOp::FlipH];
        let empty: [EditOp; 0] = [];
        assert_eq!(longest_prefix([&a[..], &ab[..], &empty[..]], &ops), Some((1, 2)));
        assert_eq!(longest_prefix([&empty[..], &a[..]], &ops), Some((1, 1)));
        assert_eq!(longest_prefix([&ab[..]], &a), None);
        assert_eq!(longest_prefix([&ab[..]], &ab), Some((0, 2)));
        assert_eq!(longest_prefix(std::iter::empty(), &ops), None);
        assert!(is_heavy(&EditOp::Upscale { factor: 2 }) && !is_heavy(&EditOp::RotateCw));
    }

    #[test]
    fn cache_is_bounded_and_keyed_by_file_version() {
        use std::sync::Arc;
        let img = |w| Arc::new(image::DynamicImage::new_rgb8(w, 1));
        let path = std::path::Path::new("/x/a.png");
        let t0 = Some(std::time::UNIX_EPOCH);
        let t1 = Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1));
        let heavy = vec![EditOp::RemoveBackground];
        let mut cache = EditCache::default();
        cache.insert(path, t0, Vec::new(), img(1));
        cache.insert(path, t0, heavy.clone(), img(2));
        let ops = [EditOp::RemoveBackground, EditOp::RotateCw];
        let (len, hit) = cache.best(path, t0, &ops).unwrap();
        assert_eq!((len, hit.width()), (1, 2));
        let (len, hit) = cache.best(path, t0, &[EditOp::FlipH]).unwrap();
        assert_eq!((len, hit.width()), (0, 1));
        // Changed on disk, or another file: no hit.
        assert!(cache.best(path, t1, &ops).is_none());
        assert!(cache.best(std::path::Path::new("/x/b.png"), t0, &ops).is_none());
        // Never more than two images; the least recently used goes.
        cache.insert(
            path,
            t0,
            vec![EditOp::RemoveBackground, EditOp::Upscale { factor: 2 }],
            img(3),
        );
        assert_eq!(cache.cached_ops().len(), EditCache::CAPACITY);
        assert_eq!(cache.cached_ops()[1], Vec::<EditOp>::new());
        // A new version of the file drops the old entries.
        cache.insert(path, t1, Vec::new(), img(4));
        assert_eq!(cache.cached_ops(), vec![Vec::<EditOp>::new()]);
        cache.clear();
        assert!(cache.best(path, t1, &[]).is_none());
    }

    #[test]
    fn a_gesture_commits_one_operation_only_when_values_changed() {
        let ops = [EditOp::RotateCw, adj(10)];
        assert_eq!(adjust_commit(&ops, trailing_adjust(&ops)), None);
        assert_eq!(adjust_commit(&ops, AdjustValues::NEUTRAL), Some(AdjustValues::NEUTRAL.op()));
        assert_eq!(adjust_commit(&[], AdjustValues::NEUTRAL), None);
        assert_eq!(adjust_commit(&[], trailing_adjust(&[adj(7)])), Some(adj(7)));
    }

    #[test]
    fn core_values_are_clamped_to_the_accepted_ranges() {
        let wild = AdjustValues { brightness: 500, contrast: -500, saturation: 0, gamma_pct: 0 };
        let core = wild.core();
        assert_eq!(core.brightness, 100);
        assert_eq!(core.contrast, -100.0);
        assert!((core.gamma - 0.1).abs() < 1e-6);
        assert_eq!(AdjustValues::NEUTRAL.core(), bigimage_core::AdjustOps::default());
    }

    #[test]
    fn locked_resize_keeps_the_proportion() {
        assert_eq!(locked_side(200, (400, 300), true), 150);
        assert_eq!(locked_side(150, (400, 300), false), 200);
        assert_eq!(locked_side(1, (4000, 10), true), 1);
        assert_eq!(locked_side(65_535, (1, 4000), true), MAX_SIDE);
        assert_eq!(locked_side(33, (3, 2), true), 22);
        assert_eq!(locked_side(10, (0, 0), true), 10);
    }

    #[test]
    fn percent_scales_both_sides() {
        assert_eq!(percent_size((400, 300), 50.0), (200, 150));
        assert_eq!(percent_size((400, 300), 200.0), (800, 600));
        assert_eq!(percent_size((3, 1), 1.0), (1, 1));
    }

    #[test]
    fn preview_copy_is_bounded_and_proportional() {
        assert_eq!(preview_size((400, 300), 2_000_000), (400, 300));
        let (w, h) = preview_size((6000, 4000), 2_000_000);
        assert!(u64::from(w) * u64::from(h) <= 2_000_000);
        assert!((f64::from(w) / f64::from(h) - 1.5).abs() < 0.01);
        assert_eq!(preview_size((1, 100_000_000), 2_000_000).0, 1);
    }
}
