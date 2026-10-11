// SPDX-License-Identifier: GPL-3.0-or-later
//! Frame-clock zoom with atomic layout and a stable pointer anchor.
use super::*;

#[derive(Clone, Copy)]
pub(super) struct PendingZoom {
    cursor: (f64, f64),
    image_point: (f64, f64),
    old_scale: f64,
    new_scale: f64,
    natural: (u32, u32),
    started: std::time::Instant,
    frame_start: i64,
    duration_us: f64,
}
impl PendingZoom {
    fn project(self, viewport: (f64, f64)) -> ((f64, f64), (f64, f64)) {
        self.project_scale(self.new_scale, viewport)
    }
    fn project_scale(self, scale: f64, viewport: (f64, f64)) -> ((f64, f64), (f64, f64)) {
        let width = scaled_side(f64::from(self.natural.0), scale);
        let height = scaled_side(f64::from(self.natural.1), scale);
        let (x, _, _) = project_axis(self.cursor.0, self.image_point.0, width, viewport.0);
        let (y, _, _) = project_axis(self.cursor.1, self.image_point.1, height, viewport.1);
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
fn project_axis(cursor: f64, image_point: f64, size: f64, viewport: f64) -> (f64, f64, f64) {
    let padding = ((viewport - size) / 2.0).max(0.0);
    let maximum = (size - viewport).max(0.0);
    ((image_point * size - cursor + padding).clamp(0.0, maximum), maximum, size)
}
#[cfg(test)]
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
    project_axis(cursor, (cursor + adj - old_padding) / old_size, new_size, new_viewport)
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
pub(super) fn requested_zoom(state: &ViewerState) -> ZoomState {
    state.pending_zoom.map_or(state.zoom, |pending| ZoomState::Scale(pending.new_scale))
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
        s.pending_zoom.map_or_else(
            || effective_scale(s.zoom, s.natural, (area.0 as i32, area.1 as i32)),
            |pending| pending.new_scale,
        )
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
/// Preserve high-resolution surface deltas; never turn a tiny touchpad event
/// into a whole mouse-wheel notch. Bound extreme single-event deltas.
pub(super) fn scroll_factor(delta: f64, surface: bool) -> Option<f64> {
    if !delta.is_finite() || delta.abs() < 1e-6 {
        return None;
    }
    let steps = if surface { delta / 48.0 } else { delta };
    Some(ZOOM_STEP.powf(-steps.clamp(-3.0, 3.0)))
}
fn animation_duration(from: f64, to: f64) -> f64 {
    ((to / from).ln().abs() * 600_000.0).clamp(160_000.0, 700_000.0)
}
fn animated_scale(from: f64, to: f64, fraction: f64) -> f64 {
    let t = fraction.clamp(0.0, 1.0);
    let eased = 1.0 - (1.0 - t) * (1.0 - t);
    from * ((to / from).ln() * eased).exp()
}
fn prepare_frame(s: &mut ViewerState, pending: PendingZoom, scale: f64) {
    let area = viewport(&s.scroller);
    let (target, size) = pending.project_scale(scale, area);
    // GtkViewport must see the matching offset on the first allocation of
    // this scale. Configuring only after paint produced two jumping frames.
    s.scroller.hadjustment().configure(
        target.0,
        0.0,
        size.0.max(area.0),
        10.0,
        area.0 * 0.9,
        area.0,
    );
    s.scroller.vadjustment().configure(
        target.1,
        0.0,
        size.1.max(area.1),
        10.0,
        area.1 * 0.9,
        area.1,
    );
    s.zoom = ZoomState::Scale(scale);
    apply_zoom(s);
}
fn rescale(state: &Rc<RefCell<ViewerState>>, scale: f64, cursor: Option<(f64, f64)>) {
    let needs_tick = {
        let mut s = state.borrow_mut();
        if s.natural.0 == 0 || s.natural.1 == 0 || s.texture.is_none() {
            return;
        }
        let area = viewport(&s.scroller);
        if area.0 <= 0.0 || area.1 <= 0.0 {
            return;
        }
        let requested_scale = effective_scale(s.zoom, s.natural, (area.0 as i32, area.1 as i32));
        if (requested_scale - scale).abs() < 1e-6 && s.pending_zoom.is_none() {
            return;
        }
        let pointer = cursor
            .or(s.zoom_pointer)
            .filter(|&(x, y)| {
                x.is_finite() && y.is_finite() && x >= 0.0 && y >= 0.0 && x <= area.0 && y <= area.1
            })
            .unwrap_or((area.0 / 2.0, area.1 / 2.0));
        // Fit mode may render fractional content dimensions inside the
        // allocated picture. Use those real bounds when capturing the anchor.
        let Some(bounds) = s.picture.compute_bounds(&s.scroller) else { return };
        let Some(paintable) = s.picture.paintable() else { return };
        let iw = f64::from(paintable.intrinsic_width());
        let ih = f64::from(paintable.intrinsic_height());
        if iw <= 0.0 || ih <= 0.0 {
            return;
        }
        let pw = f64::from(s.picture.width());
        let ph = f64::from(s.picture.height());
        let mut fit = (pw / iw).min(ph / ih);
        if s.picture.content_fit() == gtk::ContentFit::ScaleDown {
            fit = fit.min(1.0);
        }
        if fit <= 0.0 {
            return;
        }
        let measured_point = (
            (pointer.0 - f64::from(bounds.x()) - (pw - iw * fit) / 2.0) / (iw * fit),
            (pointer.1 - f64::from(bounds.y()) - (ph - ih * fit) / 2.0) / (ih * fit),
        );
        let image_point = s
            .pending_zoom
            .filter(|pending| pending.cursor == pointer && pending.natural == s.natural)
            .map_or(measured_point, |pending| pending.image_point);
        let old_scale = iw * fit / f64::from(s.natural.0);
        let clock = s.picture.frame_clock();
        let animate = clock.is_some()
            && s.picture.is_mapped()
            && s.picture.settings().is_gtk_enable_animations();
        let pending = PendingZoom {
            cursor: pointer,
            image_point,
            old_scale,
            new_scale: scale,
            natural: s.natural,
            started: std::time::Instant::now(),
            frame_start: clock.map_or(0, |clock| clock.frame_time()),
            duration_us: if animate { animation_duration(old_scale, scale) } else { 0.0 },
        };
        s.pending_zoom = Some(pending);
        if !animate {
            prepare_frame(&mut s, pending, scale);
        }
        s.zoom_source.is_none()
    };
    refresh_title(state);
    if !needs_tick {
        return;
    }
    let weak = Rc::downgrade(state);
    let source = state.borrow().picture.add_tick_callback(move |_, clock| {
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
        let fraction = if pending.duration_us == 0.0 {
            1.0
        } else {
            (clock.frame_time() - pending.frame_start) as f64 / pending.duration_us
        };
        let area = viewport(&s.scroller);
        let (target, size) = pending.project(area);
        let translated = s.picture.compute_bounds(&s.scroller).is_some_and(|bounds| {
            let expected_x = ((area.0 - size.0) / 2.0).max(0.0) - target.0;
            let expected_y = ((area.1 - size.1) / 2.0).max(0.0) - target.1;
            (f64::from(bounds.x()) - expected_x).abs() < 1.0
                && (f64::from(bounds.y()) - expected_y).abs() < 1.0
                && (f64::from(s.picture.width()) - size.0).abs() < 1.0
                && (f64::from(s.picture.height()) - size.1).abs() < 1.0
        });
        if fraction >= 1.0
            && ready(pending, &s)
            && translated
            && (effective_scale(s.zoom, s.natural, (area.0 as i32, area.1 as i32))
                - pending.new_scale)
                .abs()
                < 1e-6
        {
            s.zoom_source = None;
            s.pending_zoom = None;
            drop(s);
            refresh_title(&state);
            return glib::ControlFlow::Break;
        }
        if pending.started.elapsed() >= Duration::from_secs(2) {
            tracing::warn!("zoom layout did not settle before deadline");
            s.zoom_source = None;
            s.pending_zoom = None;
            drop(s);
            refresh_title(&state);
            return glib::ControlFlow::Break;
        }
        let scale = animated_scale(pending.old_scale, pending.new_scale, fraction);
        if (effective_scale(s.zoom, s.natural, (area.0 as i32, area.1 as i32)) - scale).abs()
            >= 1e-6
        {
            prepare_frame(&mut s, pending, scale);
        } else {
            s.picture.queue_allocate();
        }
        glib::ControlFlow::Continue
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
    fn touchpad_events_preserve_fractional_motion() {
        let tiny = scroll_factor(-0.1, true).unwrap();
        assert!(tiny > 1.0 && tiny < 1.001);
        assert!((tiny.powi(480) - ZOOM_STEP).abs() < 1e-10);
        assert!((scroll_factor(-1.0, false).unwrap() - ZOOM_STEP).abs() < 1e-10);
        assert!(scroll_factor(f64::NAN, true).is_none());
        assert!(scroll_factor(f64::INFINITY, false).is_none());
    }
    #[test]
    fn animation_is_monotonic_in_both_directions_and_stops_at_target() {
        for (from, to) in [(0.5, 1.0), (1.0, 0.5)] {
            let values: Vec<_> =
                (0..=20).map(|n| animated_scale(from, to, n as f64 / 20.0)).collect();
            assert!((values[0] - from).abs() < 1e-10);
            assert!((values[20] - to).abs() < 1e-10);
            assert!(values.windows(2).all(|pair| if to > from {
                pair[1] >= pair[0]
            } else {
                pair[1] <= pair[0]
            }));
            assert!((animated_scale(from, to, 1.5) - to).abs() < 1e-10);
        }
    }
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
