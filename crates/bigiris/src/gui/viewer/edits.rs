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

/// Every slider movement replaces the adjustment instead of stacking on
/// it: a run of consecutive `Adjust` operations is worth only its last
/// one. Each committed slider gesture is still its own history entry,
/// so Undo goes back to the previous slider position, not to the
/// unadjusted image. Returns the operations that reach the pixels.
pub(in crate::gui) fn effective_ops(ops: &[EditOp]) -> Vec<EditOp> {
    ops.iter()
        .enumerate()
        .filter(|(i, op)| {
            !(matches!(op, EditOp::Adjust { .. })
                && matches!(ops.get(i + 1), Some(EditOp::Adjust { .. })))
        })
        .map(|(_, op)| *op)
        .collect()
}

/// What the sliders show for `ops`: the trailing adjustment, or neutral
/// when the last edit was something else.
pub(in crate::gui) fn trailing_adjust(ops: &[EditOp]) -> AdjustValues {
    ops.last().and_then(|op| AdjustValues::from_op(*op)).unwrap_or(AdjustValues::NEUTRAL)
}

/// The image the sliders work on: every effective edit except the
/// trailing adjustment they are replacing.
pub(in crate::gui) fn adjust_base_ops(ops: &[EditOp]) -> Vec<EditOp> {
    let mut effective = effective_ops(ops);
    if matches!(effective.last(), Some(EditOp::Adjust { .. })) {
        effective.pop();
    }
    effective
}

/// The operation that records a finished slider gesture, or `None` when
/// the sliders ended where they started.
pub(in crate::gui) fn adjust_commit(ops: &[EditOp], values: AdjustValues) -> Option<EditOp> {
    (trailing_adjust(ops) != values).then(|| values.op())
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
    fn base_excludes_the_adjustment_being_replaced() {
        assert_eq!(adjust_base_ops(&[EditOp::RotateCw, adj(10), adj(30)]), [EditOp::RotateCw]);
        assert_eq!(adjust_base_ops(&[adj(10), EditOp::FlipV]), [adj(10), EditOp::FlipV]);
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
