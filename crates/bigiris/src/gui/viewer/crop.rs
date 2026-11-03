// SPDX-License-Identifier: GPL-3.0-or-later
//! Inline crop on the main picture.
//!
//! `Ctrl+R` (or the menu) turns the viewer into a crop mode: a selection
//! with corner and edge handles is drawn over the picture itself and a
//! floating bar offers aspect presets, Cancel and Crop. Cropping pushes an
//! [`EditOp::Crop`] onto the in-memory edit history, so the opened image
//! changes at once and Undo, Redo and `Ctrl+S` work like rotate and flip.
//! Coordinates are natural pixels of the image as currently shown, i.e.
//! after the pending edits.

use super::super::*;

/// Widget-pixel distance within which a handle or an edge is grabbed.
const HANDLE_TOLERANCE: f64 = 14.0;
/// Handle square size drawn on corners.
const HANDLE_SIZE: f64 = 10.0;

/// Crop rectangle in natural pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::gui) struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub(in crate::gui) fn full(natural: (u32, u32)) -> Self {
        Rect { x: 0.0, y: 0.0, w: f64::from(natural.0), h: f64::from(natural.1) }
    }
}

/// Which handle the user grabbed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) enum Handle {
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
}

impl Handle {
    /// Horizontal and vertical direction the handle moves: -1 left/top,
    /// 1 right/bottom, 0 not on that axis.
    fn dirs(self) -> (i8, i8) {
        match self {
            Handle::N => (0, -1),
            Handle::S => (0, 1),
            Handle::E => (1, 0),
            Handle::W => (-1, 0),
            Handle::NE => (1, -1),
            Handle::NW => (-1, -1),
            Handle::SE => (1, 1),
            Handle::SW => (-1, 1),
        }
    }

    fn cursor(self) -> &'static str {
        match self {
            Handle::N | Handle::S => "ns-resize",
            Handle::E | Handle::W => "ew-resize",
            Handle::NE | Handle::SW => "nesw-resize",
            Handle::NW | Handle::SE => "nwse-resize",
        }
    }
}

/// What a press on the picture does, decided when the drag starts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::gui) enum Drag {
    /// Outside the selection: draw a new one from this natural point.
    New { anchor: (f64, f64) },
    /// Inside the selection: move it, keeping its size.
    Move { start: Rect },
    /// On a handle: resize from it, the opposite side stays put.
    Resize { start: Rect, handle: Handle },
}

/// Where the picture is drawn inside the overlay: widget = origin + natural × scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::gui) struct Fit {
    pub scale: f64,
    pub ox: f64,
    pub oy: f64,
}

/// Layout of a `ContentFit::ScaleDown` picture in a `(bx, by, bw, bh)` box:
/// shrink to fit, never enlarge, centred.
pub(in crate::gui) fn scale_down_fit(bounds: (f64, f64, f64, f64), natural: (u32, u32)) -> Fit {
    let (bx, by, bw, bh) = bounds;
    let (iw, ih) = (f64::from(natural.0), f64::from(natural.1));
    if bw <= 0.0 || bh <= 0.0 || iw <= 0.0 || ih <= 0.0 {
        return Fit { scale: 1.0, ox: bx, oy: by };
    }
    let scale = (bw / iw).min(bh / ih).min(1.0);
    Fit { scale, ox: bx + (bw - iw * scale) / 2.0, oy: by + (bh - ih * scale) / 2.0 }
}

/// Decide what a press at `point` (widget pixels) does.
pub(in crate::gui) fn hit(point: (f64, f64), rect: Rect, fit: Fit, natural: (u32, u32)) -> Drag {
    let (px, py) = point;
    let left = fit.ox + rect.x * fit.scale;
    let top = fit.oy + rect.y * fit.scale;
    let right = left + rect.w * fit.scale;
    let bottom = top + rect.h * fit.scale;
    let near = |a: f64, b: f64| (a - b).abs() <= HANDLE_TOLERANCE;
    let within =
        |v: f64, lo: f64, hi: f64| v >= lo - HANDLE_TOLERANCE && v <= hi + HANDLE_TOLERANCE;
    let handle = match (near(px, left), near(px, right), near(py, top), near(py, bottom)) {
        (true, _, true, _) => Some(Handle::NW),
        (_, true, true, _) => Some(Handle::NE),
        (true, _, _, true) => Some(Handle::SW),
        (_, true, _, true) => Some(Handle::SE),
        (true, _, _, _) if within(py, top, bottom) => Some(Handle::W),
        (_, true, _, _) if within(py, top, bottom) => Some(Handle::E),
        (_, _, true, _) if within(px, left, right) => Some(Handle::N),
        (_, _, _, true) if within(px, left, right) => Some(Handle::S),
        _ => None,
    };
    if let Some(handle) = handle {
        return Drag::Resize { start: rect, handle };
    }
    let (w, h) = (f64::from(natural.0), f64::from(natural.1));
    // Moving a selection that already covers the whole image does nothing,
    // so a drag inside it draws a new one instead.
    let whole = rect.x <= 0.5 && rect.y <= 0.5 && rect.w >= w - 0.5 && rect.h >= h - 0.5;
    if !whole && px > left && px < right && py > top && py < bottom {
        return Drag::Move { start: rect };
    }
    let nx = ((px - fit.ox) / fit.scale).clamp(0.0, w);
    let ny = ((py - fit.oy) / fit.scale).clamp(0.0, h);
    Drag::New { anchor: (nx, ny) }
}

/// Mouse cursor for a pointer at `point`, mirroring [`hit`].
pub(in crate::gui) fn cursor_for(
    point: (f64, f64),
    rect: Rect,
    fit: Fit,
    natural: (u32, u32),
) -> &'static str {
    match hit(point, rect, fit, natural) {
        Drag::Resize { handle, .. } => handle.cursor(),
        Drag::Move { .. } => "move",
        Drag::New { .. } => "crosshair",
    }
}

/// Rectangle from a fixed `anchor` towards `(dx, dy)` directions with the
/// requested size, shrunk (keeping `ratio`, if any) to stay in the image.
fn from_anchor(
    anchor: (f64, f64),
    dirs: (f64, f64),
    size: (f64, f64),
    ratio: Option<f64>,
    natural: (f64, f64),
) -> Rect {
    let (ax, ay) = anchor;
    let (mut w, mut h) = (size.0.max(1.0), size.1.max(1.0));
    if let Some(r) = ratio {
        if w / h > r {
            h = w / r;
        } else {
            w = h * r;
        }
    }
    let max_w = if dirs.0 < 0.0 { ax } else { natural.0 - ax }.max(1.0);
    let max_h = if dirs.1 < 0.0 { ay } else { natural.1 - ay }.max(1.0);
    if ratio.is_some() {
        let k = (max_w / w).min(max_h / h).min(1.0);
        w *= k;
        h *= k;
    } else {
        w = w.min(max_w);
        h = h.min(max_h);
    }
    let x = if dirs.0 < 0.0 { ax - w } else { ax };
    let y = if dirs.1 < 0.0 { ay - h } else { ay };
    Rect { x, y, w, h }
}

/// Apply a drag of `delta` natural pixels to `drag`.
pub(in crate::gui) fn drag_to(
    drag: Drag,
    delta: (f64, f64),
    natural: (u32, u32),
    ratio: Option<f64>,
) -> Rect {
    let nat = (f64::from(natural.0), f64::from(natural.1));
    let (dx, dy) = delta;
    match drag {
        Drag::New { anchor } => {
            let tx = (anchor.0 + dx).clamp(0.0, nat.0);
            let ty = (anchor.1 + dy).clamp(0.0, nat.1);
            let dirs =
                (if tx < anchor.0 { -1.0 } else { 1.0 }, if ty < anchor.1 { -1.0 } else { 1.0 });
            from_anchor(anchor, dirs, ((tx - anchor.0).abs(), (ty - anchor.1).abs()), ratio, nat)
        }
        Drag::Move { start } => Rect {
            x: (start.x + dx).clamp(0.0, (nat.0 - start.w).max(0.0)),
            y: (start.y + dy).clamp(0.0, (nat.1 - start.h).max(0.0)),
            ..start
        },
        Drag::Resize { start, handle } => {
            let (hx, hy) = handle.dirs();
            let (l, t, r, b) = (start.x, start.y, start.x + start.w, start.y + start.h);
            match (hx, hy) {
                // Corners: the opposite corner is the anchor.
                (hx, hy) if hx != 0 && hy != 0 => {
                    let anchor = (if hx > 0 { l } else { r }, if hy > 0 { t } else { b });
                    let mx = (if hx > 0 { r } else { l } + dx).clamp(0.0, nat.0);
                    let my = (if hy > 0 { b } else { t } + dy).clamp(0.0, nat.1);
                    let dirs = (
                        if mx < anchor.0 { -1.0 } else { 1.0 },
                        if my < anchor.1 { -1.0 } else { 1.0 },
                    );
                    from_anchor(
                        anchor,
                        dirs,
                        ((mx - anchor.0).abs(), (my - anchor.1).abs()),
                        ratio,
                        nat,
                    )
                }
                // Side edges: move one side; a fixed ratio keeps the
                // selection centred on the other axis.
                (hx, 0) => {
                    let fixed = if hx > 0 { l } else { r };
                    let moved = (if hx > 0 { r } else { l } + dx).clamp(0.0, nat.0);
                    let dir = if moved < fixed { -1.0 } else { 1.0 };
                    let mut w = (moved - fixed).abs().max(1.0);
                    let max_w = if dir < 0.0 { fixed } else { nat.0 - fixed };
                    w = w.min(max_w.max(1.0));
                    let (mut y, mut h) = (t, b - t);
                    if let Some(rt) = ratio {
                        let cy = t + (b - t) / 2.0;
                        h = w / rt;
                        let max_h = 2.0 * cy.min(nat.1 - cy);
                        if h > max_h {
                            h = max_h.max(1.0);
                            w = h * rt;
                        }
                        y = cy - h / 2.0;
                    }
                    Rect { x: if dir < 0.0 { fixed - w } else { fixed }, y, w, h }
                }
                (0, hy) => {
                    let fixed = if hy > 0 { t } else { b };
                    let moved = (if hy > 0 { b } else { t } + dy).clamp(0.0, nat.1);
                    let dir = if moved < fixed { -1.0 } else { 1.0 };
                    let mut h = (moved - fixed).abs().max(1.0);
                    let max_h = if dir < 0.0 { fixed } else { nat.1 - fixed };
                    h = h.min(max_h.max(1.0));
                    let (mut x, mut w) = (l, r - l);
                    if let Some(rt) = ratio {
                        let cx = l + (r - l) / 2.0;
                        w = h * rt;
                        let max_w = 2.0 * cx.min(nat.0 - cx);
                        if w > max_w {
                            w = max_w.max(1.0);
                            h = w / rt;
                        }
                        x = cx - w / 2.0;
                    }
                    Rect { x, y: if dir < 0.0 { fixed - h } else { fixed }, w, h }
                }
                _ => start,
            }
        }
    }
}

/// Largest selection with `ratio` (width / height) centred in the image;
/// the whole image when `ratio` is `None`.
pub(in crate::gui) fn ratio_rect(natural: (u32, u32), ratio: Option<f64>) -> Rect {
    let full = Rect::full(natural);
    let Some(r) = ratio.filter(|r| r.is_finite() && *r > 0.0) else { return full };
    let (w, h) = if full.w / full.h > r { (full.h * r, full.h) } else { (full.w, full.w / r) };
    Rect { x: (full.w - w) / 2.0, y: (full.h - h) / 2.0, w, h }
}

/// Whole pixels for the edit: at least 1×1 and inside the image.
pub(in crate::gui) fn to_pixels(rect: Rect, natural: (u32, u32)) -> (u32, u32, u32, u32) {
    let (iw, ih) = natural;
    let x = (rect.x.round().max(0.0) as u32).min(iw.saturating_sub(1));
    let y = (rect.y.round().max(0.0) as u32).min(ih.saturating_sub(1));
    let w = (rect.w.round().max(1.0) as u32).clamp(1, iw - x);
    let h = (rect.h.round().max(1.0) as u32).clamp(1, ih - y);
    (x, y, w, h)
}

/// Aspect presets: label and width / height (`None` = free; the
/// "Original" entry is resolved against the image when chosen).
const PRESETS: [(&str, Option<(f64, f64)>); 9] = [
    ("Livre", None),
    ("Original", None),
    ("1:1", Some((1.0, 1.0))),
    ("4:3", Some((4.0, 3.0))),
    ("3:2", Some((3.0, 2.0))),
    ("16:9", Some((16.0, 9.0))),
    ("3:4", Some((3.0, 4.0))),
    ("2:3", Some((2.0, 3.0))),
    ("9:16", Some((9.0, 16.0))),
];

/// Widgets of the crop mode, built hidden with the viewer.
pub(in crate::gui) struct CropUi {
    pub area: gtk::DrawingArea,
    pub bar: gtk::Revealer,
    size_label: gtk::Label,
    aspect: gtk::DropDown,
}

/// An active crop on the current image.
pub(in crate::gui) struct CropSession {
    path: PathBuf,
    natural: (u32, u32),
    rect: Rect,
    drag: Option<Drag>,
    ratio: Option<f64>,
    /// Actions disabled while cropping and whether each was enabled.
    suspended: Vec<(gio::SimpleAction, bool)>,
}

/// Actions that would change, leave or zoom the image under the selection.
const SUSPENDED_ACTIONS: [&str; 21] = [
    "rotate-cw",
    "rotate-ccw",
    "rotate-180",
    "flip-h",
    "flip-v",
    "undo",
    "redo",
    "save-edits",
    "trash",
    "previous",
    "next",
    "crop",
    "remove-bg",
    "upscale",
    "zoom-in",
    "zoom-out",
    "zoom-fit",
    "zoom-to",
    "print",
    "open",
    "batch-folder",
];

pub(in crate::gui) fn build() -> CropUi {
    let area = gtk::DrawingArea::builder().hexpand(true).vexpand(true).visible(false).build();
    let labels: Vec<String> = PRESETS
        .iter()
        .map(|(label, _)| match *label {
            "Livre" => gettext("Livre"),
            "Original" => gettext("Original"),
            ratio => ratio.to_string(),
        })
        .collect();
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let aspect = gtk::DropDown::from_strings(&label_refs);
    aspect.set_tooltip_text(Some(&gettext("Proporção")));
    let size_label = gtk::Label::new(None);
    size_label.add_css_class("numeric");
    size_label.set_width_chars(13);
    let cancel =
        gtk::Button::builder().label(gettext("Cancelar")).action_name("win.crop-cancel").build();
    let apply =
        gtk::Button::builder().label(gettext("Recortar")).action_name("win.crop-apply").build();
    apply.add_css_class("suggested-action");
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(8)
        .margin_end(8)
        .build();
    row.append(&aspect);
    row.append(&size_label);
    row.append(&cancel);
    row.append(&apply);
    let pill = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    pill.add_css_class("osd");
    pill.add_css_class("toolbar");
    pill.add_css_class("crop-bar");
    pill.append(&row);
    let bar = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideUp)
        .reveal_child(false)
        .visible(false)
        .valign(gtk::Align::End)
        .halign(gtk::Align::Center)
        .margin_bottom(18)
        .child(&pill)
        .build();
    CropUi { area, bar, size_label, aspect }
}

#[cfg(test)]
impl CropUi {
    pub(in crate::gui) fn aspect_dropdown(&self) -> &gtk::DropDown {
        &self.aspect
    }
}

#[cfg(test)]
impl CropSession {
    pub(in crate::gui) fn rect(&self) -> Rect {
        self.rect
    }

    pub(in crate::gui) fn natural(&self) -> (u32, u32) {
        self.natural
    }
}

pub(in crate::gui) fn is_active(state: &Rc<RefCell<ViewerState>>) -> bool {
    state.borrow().crop.is_some()
}

/// Where the picture sits inside the crop overlay right now.
pub(in crate::gui) fn current_fit(s: &ViewerState, natural: (u32, u32)) -> Fit {
    let bounds = s
        .picture
        .compute_bounds(&s.crop_ui.area)
        .map(|b| (f64::from(b.x()), f64::from(b.y()), f64::from(b.width()), f64::from(b.height())))
        .unwrap_or((0.0, 0.0, 0.0, 0.0));
    scale_down_fit(bounds, natural)
}

fn refresh(s: &ViewerState) {
    if let Some(session) = s.crop.as_ref() {
        let (_, _, w, h) = to_pixels(session.rect, session.natural);
        s.crop_ui.size_label.set_text(&format!("{w} × {h} px"));
    }
    s.crop_ui.area.queue_draw();
}

/// Wire the crop widgets and actions to the viewer.
pub(in crate::gui) fn connect(state: &Rc<RefCell<ViewerState>>, window: &adw::ApplicationWindow) {
    let (area, aspect) = {
        let s = state.borrow();
        (s.crop_ui.area.clone(), s.crop_ui.aspect.clone())
    };
    {
        let weak = Rc::downgrade(state);
        area.set_draw_func(move |_, cr, width, height| {
            let Some(state) = weak.upgrade() else { return };
            let Ok(s) = state.try_borrow() else { return };
            let Some(session) = s.crop.as_ref() else { return };
            draw(
                cr,
                f64::from(width),
                f64::from(height),
                current_fit(&s, session.natural),
                session,
            );
        });
    }
    let drag = gtk::GestureDrag::builder().button(1).build();
    {
        let weak = Rc::downgrade(state);
        drag.connect_drag_begin(move |_, x, y| {
            let Some(state) = weak.upgrade() else { return };
            let mut s = state.borrow_mut();
            let fit = {
                let Some(session) = s.crop.as_ref() else { return };
                current_fit(&s, session.natural)
            };
            if let Some(session) = s.crop.as_mut() {
                session.drag = Some(hit((x, y), session.rect, fit, session.natural));
            }
        });
    }
    {
        let weak = Rc::downgrade(state);
        drag.connect_drag_update(move |_, dx, dy| {
            let Some(state) = weak.upgrade() else { return };
            let mut s = state.borrow_mut();
            let fit = {
                let Some(session) = s.crop.as_ref() else { return };
                current_fit(&s, session.natural)
            };
            if fit.scale <= 0.0 {
                return;
            }
            if let Some(session) = s.crop.as_mut() {
                if let Some(drag) = session.drag {
                    session.rect = drag_to(
                        drag,
                        (dx / fit.scale, dy / fit.scale),
                        session.natural,
                        session.ratio,
                    );
                }
            }
            refresh(&s);
        });
    }
    {
        let weak = Rc::downgrade(state);
        drag.connect_drag_end(move |_, _, _| {
            let Some(state) = weak.upgrade() else { return };
            let mut s = state.borrow_mut();
            if let Some(session) = s.crop.as_mut() {
                session.drag = None;
            }
            refresh(&s);
        });
    }
    area.add_controller(drag);
    // Double-click inside the selection crops, like confirming in most editors.
    let click = gtk::GestureClick::builder().button(1).build();
    {
        let weak = Rc::downgrade(state);
        click.connect_pressed(move |_, presses, _, _| {
            if presses == 2 {
                if let Some(state) = weak.upgrade() {
                    apply(&state);
                }
            }
        });
    }
    area.add_controller(click);
    let motion = gtk::EventControllerMotion::new();
    {
        let weak = Rc::downgrade(state);
        motion.connect_motion(move |controller, x, y| {
            let Some(state) = weak.upgrade() else { return };
            let Ok(s) = state.try_borrow() else { return };
            let Some(session) = s.crop.as_ref() else { return };
            let name = match session.drag {
                Some(Drag::Move { .. }) => "move",
                Some(Drag::Resize { handle, .. }) => handle.cursor(),
                Some(Drag::New { .. }) => "crosshair",
                None => cursor_for(
                    (x, y),
                    session.rect,
                    current_fit(&s, session.natural),
                    session.natural,
                ),
            };
            if let Some(widget) = controller.widget() {
                widget.set_cursor_from_name(Some(name));
            }
        });
    }
    area.add_controller(motion);
    {
        let weak = Rc::downgrade(state);
        aspect.connect_selected_notify(move |dropdown| {
            let Some(state) = weak.upgrade() else { return };
            // `enter` resets the preset; never re-enter a borrowed state.
            let Ok(mut s) = state.try_borrow_mut() else { return };
            let Some(session) = s.crop.as_mut() else { return };
            let index = dropdown.selected() as usize;
            session.ratio = match PRESETS.get(index) {
                Some(("Original", _)) => {
                    Some(f64::from(session.natural.0) / f64::from(session.natural.1.max(1)))
                }
                Some((_, Some((w, h)))) => Some(w / h),
                _ => None,
            };
            if session.ratio.is_some() {
                session.rect = ratio_rect(session.natural, session.ratio);
            }
            refresh(&s);
        });
    }
    for (name, run) in [
        ("crop-apply", apply as fn(&Rc<RefCell<ViewerState>>)),
        ("crop-cancel", cancel as fn(&Rc<RefCell<ViewerState>>)),
    ] {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(state);
        action.connect_activate(move |_, _| {
            if let Some(state) = weak.upgrade() {
                run(&state);
            }
        });
        window.add_action(&action);
    }
}

/// Start cropping the image on screen.
pub(in crate::gui) fn enter(state: &Rc<RefCell<ViewerState>>) {
    let message = {
        let s = state.borrow();
        if s.crop.is_some() || s.files.is_empty() || s.texture.is_none() {
            return;
        }
        if s.saving {
            Some(gettext("Aguarde o salvamento terminar"))
        } else if !s.current_ready {
            return;
        } else {
            None
        }
    };
    if let Some(message) = message {
        state.borrow().osd_label.set_text(&message);
        pulse_osd(state);
        return;
    }
    if let Some(id) = state.borrow_mut().slideshow_source.take() {
        id.remove();
    }
    set_zoom(state, ZoomState::Fit);
    // Back to "free" before the session exists, so the preset handler
    // (which needs the state) runs now and finds nothing to update.
    let aspect = state.borrow().crop_ui.aspect.clone();
    aspect.set_selected(0);
    let window = state.borrow().window().clone();
    let suspended = SUSPENDED_ACTIONS
        .iter()
        .filter_map(|name| window.lookup_action(name)?.downcast::<gio::SimpleAction>().ok())
        .map(|action| {
            let enabled = action.is_enabled();
            action.set_enabled(false);
            (action, enabled)
        })
        .collect();
    let (area, bar, strip, osd_label) = {
        let mut s = state.borrow_mut();
        let natural = s.natural;
        s.crop = Some(CropSession {
            path: s.files[s.idx].clone(),
            natural,
            rect: Rect::full(natural),
            drag: None,
            ratio: None,
            suspended,
        });
        refresh(&s);
        (
            s.crop_ui.area.clone(),
            s.crop_ui.bar.clone(),
            s.strip_revealer.clone(),
            s.osd_label.clone(),
        )
    };
    // Showing or hiding widgets under the pointer emits crossing events
    // synchronously, and their handlers borrow the state: never do it
    // while a borrow is held.
    strip.set_reveal_child(false);
    area.set_visible(true);
    bar.set_visible(true);
    bar.set_reveal_child(true);
    osd_label.set_text(&gettext(
        "Arraste nos cantos para redimensionar · dentro do recorte para mover · fora dele para uma nova seleção.",
    ));
    cancel_cursor_hide(state);
    state.borrow().window().set_cursor(None);
    pulse_osd(state);
}

/// Leave crop mode and give the suspended actions back.
fn leave(state: &Rc<RefCell<ViewerState>>) -> Option<CropSession> {
    let (session, area, bar) = {
        let mut s = state.borrow_mut();
        let session = s.crop.take()?;
        (session, s.crop_ui.area.clone(), s.crop_ui.bar.clone())
    };
    // Hiding the layer under the pointer re-enters the picture's motion
    // handlers synchronously (see `enter`), so no borrow is held here.
    bar.set_reveal_child(false);
    bar.set_visible(false);
    area.set_cursor(None);
    area.set_visible(false);
    for (action, enabled) in &session.suspended {
        action.set_enabled(*enabled);
    }
    sync_edit_actions(state);
    Some(session)
}

pub(in crate::gui) fn cancel(state: &Rc<RefCell<ViewerState>>) {
    leave(state);
}

/// Crop the opened image to the selection (as an undoable edit).
pub(in crate::gui) fn apply(state: &Rc<RefCell<ViewerState>>) {
    let Some(session) = leave(state) else { return };
    let still_current = {
        let s = state.borrow();
        !s.files.is_empty() && s.files[s.idx] == session.path && s.natural == session.natural
    };
    if !still_current {
        return;
    }
    let (x, y, w, h) = to_pixels(session.rect, session.natural);
    if (x, y, w, h) == (0, 0, session.natural.0, session.natural.1) {
        return;
    }
    apply_edit(state, EditOp::Crop { x, y, w, h });
}

/// Selection overlay: dim outside, outline, rule-of-thirds and handles.
fn draw(cr: &gtk::cairo::Context, width: f64, height: f64, fit: Fit, session: &CropSession) {
    let r = session.rect;
    let x = fit.ox + r.x * fit.scale;
    let y = fit.oy + r.y * fit.scale;
    let w = r.w * fit.scale;
    let h = r.h * fit.scale;
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.55);
    cr.rectangle(0.0, 0.0, width, height);
    cr.rectangle(x, y, w, h);
    cr.set_fill_rule(gtk::cairo::FillRule::EvenOdd);
    let _ = cr.fill();
    cr.set_fill_rule(gtk::cairo::FillRule::Winding);
    // Thirds while the user is shaping the selection.
    if session.drag.is_some() {
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.45);
        cr.set_line_width(1.0);
        for i in 1..3 {
            let fx = x + w * f64::from(i) / 3.0;
            let fy = y + h * f64::from(i) / 3.0;
            cr.move_to(fx.round() + 0.5, y);
            cr.line_to(fx.round() + 0.5, y + h);
            cr.move_to(x, fy.round() + 0.5);
            cr.line_to(x + w, fy.round() + 0.5);
        }
        let _ = cr.stroke();
    }
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
    cr.set_line_width(1.5);
    cr.rectangle(x, y, w, h);
    let _ = cr.stroke();
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    let k = HANDLE_SIZE;
    for (cx, cy) in [(x, y), (x + w, y), (x, y + h), (x + w, y + h)] {
        cr.rectangle(cx - k / 2.0, cy - k / 2.0, k, k);
    }
    // Edge handles: short bars in the middle of each side.
    for (cx, cy, horizontal) in [
        (x + w / 2.0, y, true),
        (x + w / 2.0, y + h, true),
        (x, y + h / 2.0, false),
        (x + w, y + h / 2.0, false),
    ] {
        if horizontal {
            cr.rectangle(cx - k, cy - 2.0, 2.0 * k, 4.0);
        } else {
            cr.rectangle(cx - 2.0, cy - k, 4.0, 2.0 * k);
        }
    }
    let _ = cr.fill();
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAT: (u32, u32) = (400, 300);
    const FIT: Fit = Fit { scale: 1.0, ox: 0.0, oy: 0.0 };

    fn close(a: Rect, b: Rect) -> bool {
        (a.x - b.x).abs() < 1e-6
            && (a.y - b.y).abs() < 1e-6
            && (a.w - b.w).abs() < 1e-6
            && (a.h - b.h).abs() < 1e-6
    }

    #[test]
    fn scale_down_fit_never_enlarges_and_centres() {
        let small = scale_down_fit((0.0, 0.0, 1000.0, 800.0), (400, 300));
        assert_eq!(small, Fit { scale: 1.0, ox: 300.0, oy: 250.0 });
        let big = scale_down_fit((10.0, 20.0, 400.0, 300.0), (800, 300));
        assert!((big.scale - 0.5).abs() < 1e-9);
        assert_eq!((big.ox, big.oy), (10.0, 20.0 + 75.0));
    }

    #[test]
    fn hit_picks_corners_edges_inside_and_outside() {
        let r = Rect { x: 100.0, y: 50.0, w: 200.0, h: 100.0 };
        assert_eq!(hit((101.0, 52.0), r, FIT, NAT), Drag::Resize { start: r, handle: Handle::NW });
        assert_eq!(hit((299.0, 149.0), r, FIT, NAT), Drag::Resize { start: r, handle: Handle::SE });
        assert_eq!(hit((200.0, 51.0), r, FIT, NAT), Drag::Resize { start: r, handle: Handle::N });
        assert_eq!(hit((300.0, 100.0), r, FIT, NAT), Drag::Resize { start: r, handle: Handle::E });
        assert_eq!(hit((200.0, 100.0), r, FIT, NAT), Drag::Move { start: r });
        assert_eq!(hit((20.0, 250.0), r, FIT, NAT), Drag::New { anchor: (20.0, 250.0) });
        // With the whole image selected, a drag inside draws a new selection.
        let whole = Rect::full(NAT);
        assert_eq!(hit((200.0, 150.0), whole, FIT, NAT), Drag::New { anchor: (200.0, 150.0) });
        assert_eq!(
            hit((399.0, 299.0), whole, FIT, NAT),
            Drag::Resize { start: whole, handle: Handle::SE }
        );
    }

    #[test]
    fn edge_resize_moves_one_side_and_clamps() {
        let r = Rect { x: 100.0, y: 50.0, w: 200.0, h: 100.0 };
        let east = drag_to(Drag::Resize { start: r, handle: Handle::E }, (500.0, 7.0), NAT, None);
        assert!(close(east, Rect { x: 100.0, y: 50.0, w: 300.0, h: 100.0 }));
        let north = drag_to(Drag::Resize { start: r, handle: Handle::N }, (3.0, -20.0), NAT, None);
        assert!(close(north, Rect { x: 100.0, y: 30.0, w: 200.0, h: 120.0 }));
        // Dragging past the opposite side flips instead of going negative.
        let west = drag_to(Drag::Resize { start: r, handle: Handle::W }, (250.0, 0.0), NAT, None);
        assert!(close(west, Rect { x: 300.0, y: 50.0, w: 50.0, h: 100.0 }));
    }

    #[test]
    fn corner_resize_keeps_the_ratio_inside_the_image() {
        let r = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        let se =
            drag_to(Drag::Resize { start: r, handle: Handle::SE }, (1000.0, 50.0), NAT, Some(1.0));
        assert!((se.w - se.h).abs() < 1e-6, "{se:?}");
        assert!(se.x >= 0.0 && se.y >= 0.0 && se.x + se.w <= 400.0 && se.y + se.h <= 300.0);
        assert!((se.w - 300.0).abs() < 1e-6);
    }

    #[test]
    fn edge_resize_with_ratio_stays_centred() {
        let r = Rect { x: 100.0, y: 100.0, w: 100.0, h: 100.0 };
        let e = drag_to(Drag::Resize { start: r, handle: Handle::E }, (50.0, 0.0), NAT, Some(1.0));
        assert!(close(e, Rect { x: 100.0, y: 75.0, w: 150.0, h: 150.0 }), "{e:?}");
    }

    #[test]
    fn new_selection_and_move_stay_in_bounds() {
        let new = drag_to(Drag::New { anchor: (350.0, 250.0) }, (-400.0, 400.0), NAT, None);
        assert!(close(new, Rect { x: 0.0, y: 250.0, w: 350.0, h: 50.0 }), "{new:?}");
        let r = Rect { x: 100.0, y: 50.0, w: 200.0, h: 100.0 };
        let moved = drag_to(Drag::Move { start: r }, (1000.0, -1000.0), NAT, None);
        assert!(close(moved, Rect { x: 200.0, y: 0.0, w: 200.0, h: 100.0 }));
    }

    #[test]
    fn ratio_rect_is_the_largest_centred_box() {
        assert!(close(ratio_rect(NAT, None), Rect::full(NAT)));
        assert!(close(ratio_rect(NAT, Some(1.0)), Rect { x: 50.0, y: 0.0, w: 300.0, h: 300.0 }));
        let wide = ratio_rect(NAT, Some(16.0 / 9.0));
        assert!((wide.w - 400.0).abs() < 1e-6 && (wide.h - 225.0).abs() < 1e-6);
    }

    #[test]
    fn pixels_are_whole_and_inside_the_image() {
        assert_eq!(to_pixels(Rect { x: 10.4, y: 9.6, w: 99.5, h: 0.2 }, NAT), (10, 10, 100, 1));
        assert_eq!(to_pixels(Rect { x: 399.9, y: 0.0, w: 50.0, h: 300.0 }, NAT), (399, 0, 1, 300));
        assert_eq!(to_pixels(Rect::full(NAT), NAT), (0, 0, 400, 300));
    }
}
