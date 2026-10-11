// SPDX-License-Identifier: GPL-3.0-or-later
//! Pointer-anchored zoom, coalesced until GTK finishes the matching layout.
use super::*;

#[derive(Clone, Copy)]
pub(super) struct PendingZoom {
    cursor: (f64, f64),
    old_adjustment: (f64, f64),
    old_scale: f64,
    new_scale: f64,
    natural: (u32, u32),
    old_viewport: (f64, f64),
    started: std::time::Instant,
}
impl PendingZoom {
    fn project(self, viewport: (f64, f64)) -> ((f64, f64), (f64, f64)) {
        let (x, _, width) = anchor_between_viewports(
            self.cursor.0,
            self.old_adjustment.0,
            self.old_scale,
            self.new_scale,
            f64::from(self.natural.0),
            self.old_viewport.0,
            viewport.0,
        );
        let (y, _, height) = anchor_between_viewports(
            self.cursor.1,
            self.old_adjustment.1,
            self.old_scale,
            self.new_scale,
            f64::from(self.natural.1),
            self.old_viewport.1,
            viewport.1,
        );
        ((x, y), (width, height))
    }
}
fn viewport(scroller: &gtk::ScrolledWindow) -> (f64, f64) {
    let width = scroller.hadjustment().page_size();
    let height = scroller.vadjustment().page_size();
    (
        if width > 0.0 { width } else { f64::from(scroller.width()) },
        if height > 0.0 { height } else { f64::from(scroller.height()) },
    )
}
fn scaled_side(natural: f64, scale: f64) -> f64 {
    (natural * scale).round().clamp(1.0, f64::from(i32::MAX))
}
fn anchor_between_viewports(
    cursor: f64,
    adj: f64,
    old_scale: f64,
    new_scale: f64,
    natural: f64,
    old_viewport: f64,
    new_viewport: f64,
) -> (f64, f64, f64) {
    let old_size = scaled_side(natural, old_scale);
    let new_size = scaled_side(natural, new_scale);
    let old_padding = ((old_viewport - old_size) / 2.0).max(0.0);
    let new_padding = ((new_viewport - new_size) / 2.0).max(0.0);
    let target = (cursor + adj - old_padding) * new_size / old_size - cursor + new_padding;
    let maximum = (new_size - new_viewport).max(0.0);
    (target.clamp(0.0, maximum), maximum, new_size)
}
#[cfg(test)]
pub(super) fn anchor_axis(
    cursor: f64,
    adj: f64,
    old_scale: f64,
    new_scale: f64,
    natural: f64,
    viewport: f64,
) -> (f64, f64, f64) {
    anchor_between_viewports(cursor, adj, old_scale, new_scale, natural, viewport, viewport)
}
pub(super) fn cancel_pending(state: &mut ViewerState) {
    if let Some(source) = state.zoom_source.take() {
        source.remove();
    }
    state.pending_zoom = None;
}
fn ready(pending: PendingZoom, s: &ViewerState) -> bool {
    let area = viewport(&s.scroller);
    let (_, size) = pending.project(area);
    (s.scroller.hadjustment().upper() - size.0.max(area.0)).abs() < 1.0
        && (s.scroller.vadjustment().upper() - size.1.max(area.1)).abs() < 1.0
}
pub(super) fn zoom_in(state: &Rc<RefCell<ViewerState>>) {
    adjust_zoom(state, ZOOM_STEP, None);
}
pub(super) fn zoom_out(state: &Rc<RefCell<ViewerState>>) {
    adjust_zoom(state, 1.0 / ZOOM_STEP, None);
}
pub(super) fn adjust_zoom(
    state: &Rc<RefCell<ViewerState>>,
    factor: f64,
    cursor: Option<(f64, f64)>,
) {
    let current = {
        let s = state.borrow();
        let area = viewport(&s.scroller);
        effective_scale(s.zoom, s.natural, (area.0 as i32, area.1 as i32))
    };
    rescale(state, (current * factor).clamp(MIN_ZOOM, MAX_ZOOM), cursor);
}
pub(super) fn set_zoom(state: &Rc<RefCell<ViewerState>>, zoom: ZoomState) {
    match zoom {
        ZoomState::Scale(scale) => rescale(state, scale.clamp(MIN_ZOOM, MAX_ZOOM), None),
        ZoomState::Fit => {
            let mut s = state.borrow_mut();
            cancel_pending(&mut s);
            s.zoom = zoom;
            apply_zoom(&s);
            drop(s);
            refresh_title(state);
        }
    }
}
fn rescale(state: &Rc<RefCell<ViewerState>>, scale: f64, cursor: Option<(f64, f64)>) {
    let needs_timer = {
        let mut s = state.borrow_mut();
        if s.natural.0 == 0 || s.natural.1 == 0 || s.texture.is_none() {
            return;
        }
        let area = viewport(&s.scroller);
        if area.0 <= 0.0 || area.1 <= 0.0 {
            return;
        }
        let old_scale = effective_scale(s.zoom, s.natural, (area.0 as i32, area.1 as i32));
        if (old_scale - scale).abs() < 1e-6 {
            return;
        }
        let pointer = cursor
            .or(s.zoom_pointer)
            .filter(|&(x, y)| {
                x.is_finite() && y.is_finite() && x >= 0.0 && y >= 0.0 && x <= area.0 && y <= area.1
            })
            .unwrap_or((area.0 / 2.0, area.1 / 2.0));
        let adjustment = s
            .pending_zoom
            .map(|pending| pending.project(area).0)
            .unwrap_or((s.scroller.hadjustment().value(), s.scroller.vadjustment().value()));
        s.pending_zoom = Some(PendingZoom {
            cursor: pointer,
            old_adjustment: adjustment,
            old_scale,
            new_scale: scale,
            natural: s.natural,
            old_viewport: area,
            started: std::time::Instant::now(),
        });
        s.zoom = ZoomState::Scale(scale);
        apply_zoom(&s);
        s.zoom_source.is_none()
    };
    refresh_title(state);
    if !needs_timer {
        return;
    }
    let weak = Rc::downgrade(state);
    let source = glib::timeout_add_local(Duration::from_millis(16), move || {
        let Some(state) = weak.upgrade() else { return glib::ControlFlow::Break };
        let mut s = state.borrow_mut();
        let Some(pending) = s.pending_zoom else {
            s.zoom_source = None;
            return glib::ControlFlow::Break;
        };
        if s.window_weak.upgrade().is_none_or(|window| !window.is_visible()) {
            s.zoom_source = None;
            s.pending_zoom = None;
            return glib::ControlFlow::Break;
        }
        let area = viewport(&s.scroller);
        let (target, size) = pending.project(area);
        let hadj = s.scroller.hadjustment();
        let vadj = s.scroller.vadjustment();
        let layout_ready = ready(pending, &s);
        if layout_ready {
            hadj.set_value(target.0);
            vadj.set_value(target.1);
            if let Some(viewport) = s.picture.parent() {
                viewport.queue_allocate();
            }
        }
        // Adjustment values change before the widget's scroll translation.
        // Keep the request pending until the actual allocation catches up.
        let translated = s.picture.compute_bounds(&s.scroller).is_some_and(|bounds| {
            let expected_x = ((area.0 - size.0) / 2.0).max(0.0) - target.0;
            let expected_y = ((area.1 - size.1) / 2.0).max(0.0) - target.1;
            (f64::from(bounds.x()) - expected_x).abs() < 1.0
                && (f64::from(bounds.y()) - expected_y).abs() < 1.0
                && (f64::from(s.picture.width()) - size.0).abs() < 1.0
                && (f64::from(s.picture.height()) - size.1).abs() < 1.0
        });
        if (!layout_ready || !translated) && pending.started.elapsed() < Duration::from_secs(1) {
            return glib::ControlFlow::Continue;
        }
        if !layout_ready || !translated {
            tracing::warn!("zoom layout did not settle before deadline");
        }
        s.zoom_source = None;
        s.pending_zoom = None;
        glib::ControlFlow::Break
    });
    state.borrow_mut().zoom_source = Some(source);
}

pub(super) fn apply_zoom(state: &ViewerState) {
    let picture = &state.picture;
    let Some(texture) = state.texture.as_ref() else { return };
    match state.zoom {
        ZoomState::Fit => {
            picture.set_hexpand(true);
            picture.set_vexpand(true);
            picture.set_content_fit(gtk::ContentFit::ScaleDown);
            picture.set_size_request(-1, -1);
            picture.set_paintable(Some(texture));
        }
        ZoomState::Scale(scale) => {
            let width = scaled_side(f64::from(state.natural.0), scale) as i32;
            let height = scaled_side(f64::from(state.natural.1), scale) as i32;
            // This paintable references the same texture; no pixel resize or
            // additional image allocation happens on GTK's thread.
            let snapshot = gtk::Snapshot::new();
            snapshot.append_texture(
                texture,
                &graphene::Rect::new(0.0, 0.0, width as f32, height as f32),
            );
            let paintable =
                snapshot.to_paintable(Some(&graphene::Size::new(width as f32, height as f32)));
            picture.set_hexpand(false);
            picture.set_vexpand(false);
            picture.set_content_fit(gtk::ContentFit::Contain);
            picture.set_size_request(width, height);
            picture.set_paintable(paintable.as_ref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn anchor_survives_scrollbar_viewport_change() {
        let (value, _, _) = anchor_between_viewports(700.0, 0.0, 0.5, 0.75, 2000.0, 1000.0, 984.0);
        assert_eq!(value, 350.0);
    }
    #[test]
    fn rounding_matches_the_displayed_picture() {
        let (value, _, size) = anchor_axis(500.0, 100.0, 0.333, 0.667, 3001.0, 900.0);
        assert_eq!(size, 2002.0);
        let source = 600.0 / 999.0;
        assert!(((500.0 + value) / size - source).abs() < 1e-9);
    }
}
