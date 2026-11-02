// SPDX-License-Identifier: GPL-3.0-or-later
//! Recortar: rectangular crop with an interactive preview.

use super::*;

/// Default crop: a centred 50% rectangle — a safe non-empty starting
/// point for the UI that's unlikely to match the full image.
pub(in crate::gui) fn default_crop(img_w: u32, img_h: u32) -> CropRect {
    let w = (img_w / 2).max(1);
    let h = (img_h / 2).max(1);
    let x = (img_w.saturating_sub(w)) / 2;
    let y = (img_h.saturating_sub(h)) / 2;
    CropRect::new(x, y, w, h)
}

/// What the user is doing while the GestureDrag is active on the crop
/// preview. Decided at `drag_begin` from the hit-test against the
/// current rect; stays constant until drag ends.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::gui) enum CropDragMode {
    /// Click+drag outside the rect (or on an empty canvas) grows a new
    /// rect starting at the click point.
    NewSelection {
        /// Click position in natural-image pixels.
        anchor_nat: (f64, f64),
    },
    /// Click strictly inside the current rect → translate it; its size
    /// is preserved. `rect0` is the rect at drag start (x, y, w, h),
    /// `click_nat` the click point in natural pixels.
    Move { rect0: (f64, f64, f64, f64), click_nat: (f64, f64) },
    /// Click on a corner handle → resize anchored at the opposite
    /// corner. `anchor_nat` is the fixed corner, `click_nat` the moving
    /// one at drag start.
    ResizeCorner { anchor_nat: (f64, f64), click_nat: (f64, f64) },
}

/// Widget-pixel tolerance for corner-handle hit-testing.
const CROP_HANDLE_TOLERANCE_WIDGET_PX: f64 = 12.0;

/// Decide the drag mode from the click's position relative to the
/// current rect. `fit` is `(scale, ox, oy)` from [`contain_fit`];
/// `rect` is the current crop in natural-image pixels.
pub(in crate::gui) fn decide_drag_mode(
    click_widget: (f64, f64),
    rect: (f64, f64, f64, f64),
    fit: (f64, f64, f64),
    natural: (u32, u32),
) -> CropDragMode {
    let (sx, sy) = click_widget;
    let (rx, ry, rw, rh) = rect;
    let (scale, ox, oy) = fit;
    let nat_w = natural.0 as f64;
    let nat_h = natural.1 as f64;
    let click_nat = (((sx - ox) / scale).clamp(0.0, nat_w), ((sy - oy) / scale).clamp(0.0, nat_h));

    // Widget-space corners of the current rect.
    let corners_nat = [
        (rx, ry),           // 0: top-left      ↔ 3 bottom-right
        (rx + rw, ry),      // 1: top-right     ↔ 2 bottom-left
        (rx, ry + rh),      // 2: bottom-left   ↔ 1 top-right
        (rx + rw, ry + rh), // 3: bottom-right  ↔ 0 top-left
    ];
    let corners_widget: [(f64, f64); 4] = core::array::from_fn(|i| {
        let (nx, ny) = corners_nat[i];
        (ox + nx * scale, oy + ny * scale)
    });
    let tol2 = CROP_HANDLE_TOLERANCE_WIDGET_PX * CROP_HANDLE_TOLERANCE_WIDGET_PX;
    for (i, (cx, cy)) in corners_widget.iter().enumerate() {
        let d2 = (sx - cx).powi(2) + (sy - cy).powi(2);
        if d2 <= tol2 {
            let anchor_nat = corners_nat[3 - i];
            return CropDragMode::ResizeCorner { anchor_nat, click_nat: corners_nat[i] };
        }
    }

    // Inside the rect (strict) → move.
    let inside =
        click_nat.0 > rx && click_nat.0 < rx + rw && click_nat.1 > ry && click_nat.1 < ry + rh;
    if inside {
        return CropDragMode::Move { rect0: rect, click_nat };
    }

    CropDragMode::NewSelection { anchor_nat: click_nat }
}

/// Apply a drag delta (already in *natural-pixel* units via `(dx/scale,
/// dy/scale)`) to `mode` and return the resulting `(x, y, w, h)` in
/// natural pixels, clamped to image bounds. Width/height never collapse
/// below 1 so the rect stays visible.
pub(in crate::gui) fn apply_drag(
    mode: CropDragMode,
    delta_nat: (f64, f64),
    natural: (u32, u32),
) -> (f64, f64, f64, f64) {
    let nat_w = natural.0 as f64;
    let nat_h = natural.1 as f64;
    let (dx, dy) = delta_nat;
    match mode {
        CropDragMode::NewSelection { anchor_nat: (ax, ay) } => {
            let cx = (ax + dx).clamp(0.0, nat_w);
            let cy = (ay + dy).clamp(0.0, nat_h);
            let x0 = ax.min(cx);
            let y0 = ay.min(cy);
            let w = (ax - cx).abs().max(1.0);
            let h = (ay - cy).abs().max(1.0);
            (x0, y0, w, h)
        }
        CropDragMode::Move { rect0: (rx, ry, rw, rh), click_nat: _ } => {
            // Preserve size; translate within bounds.
            let max_x = (nat_w - rw).max(0.0);
            let max_y = (nat_h - rh).max(0.0);
            let new_x = (rx + dx).clamp(0.0, max_x);
            let new_y = (ry + dy).clamp(0.0, max_y);
            (new_x, new_y, rw, rh)
        }
        CropDragMode::ResizeCorner { anchor_nat: (ax, ay), click_nat: (cx0, cy0) } => {
            let cx = (cx0 + dx).clamp(0.0, nat_w);
            let cy = (cy0 + dy).clamp(0.0, nat_h);
            let x0 = ax.min(cx);
            let y0 = ay.min(cy);
            let w = (ax - cx).abs().max(1.0);
            let h = (ay - cy).abs().max(1.0);
            (x0, y0, w, h)
        }
    }
}

/// Layout math for a `gtk::Picture` with `ContentFit::Contain`: given
/// the widget's allocated `(ww, wh)` and the displayed image's natural
/// `(iw, ih)`, returns `(scale, ox, oy)` — the uniform scale factor and
/// top-left offset of the rendered image inside the widget box.
///
/// Returns a safe identity transform when any input is non-positive so the
/// caller can unconditionally multiply without a division guard.
pub(in crate::gui) fn contain_fit(ww: f64, wh: f64, iw: f64, ih: f64) -> (f64, f64, f64) {
    if ww <= 0.0 || wh <= 0.0 || iw <= 0.0 || ih <= 0.0 {
        return (1.0, 0.0, 0.0);
    }
    let s = (ww / iw).min(wh / ih);
    let disp_w = iw * s;
    let disp_h = ih * s;
    ((s), (ww - disp_w) / 2.0, (wh - disp_h) / 2.0)
}

/// Build the interactive preview column for the crop dialog: full
/// thumbnail in a `gtk::Picture`, plus a `gtk::DrawingArea` overlay that
/// strokes the selection rect. A `GestureDrag` on the overlay updates the
/// spin rows (which remain authoritative); spin changes queue a redraw.
fn build_crop_preview_column(
    session: &Rc<PreviewSession>,
    natural: (u32, u32),
    x_spin: &adw::SpinRow,
    y_spin: &adw::SpinRow,
    w_spin: &adw::SpinRow,
    h_spin: &adw::SpinRow,
) -> gtk::Box {
    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    picture.set_size_request(360, 360);
    picture.set_paintable(Some(&dynamic_image_to_texture(session.thumbnail().clone())));

    let selection_area = gtk::DrawingArea::builder().hexpand(true).vexpand(true).build();

    // Draw: two-pass — shade the outside of the selection rect, then
    // stroke the rect itself with a bright outline + corner handles.
    {
        let x_spin = x_spin.clone();
        let y_spin = y_spin.clone();
        let w_spin = w_spin.clone();
        let h_spin = h_spin.clone();
        selection_area.set_draw_func(move |_area, cr, ww, wh| {
            let ww = ww as f64;
            let wh = wh as f64;
            let (scale, ox, oy) = contain_fit(ww, wh, natural.0 as f64, natural.1 as f64);
            let rx = ox + x_spin.value() * scale;
            let ry = oy + y_spin.value() * scale;
            let rw = w_spin.value() * scale;
            let rh = h_spin.value() * scale;

            // Shade outside the rect (four bands) for clear selection focus.
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.45);
            cr.rectangle(0.0, 0.0, ww, ry);
            cr.rectangle(0.0, ry + rh, ww, wh - (ry + rh));
            cr.rectangle(0.0, ry, rx, rh);
            cr.rectangle(rx + rw, ry, ww - (rx + rw), rh);
            let _ = cr.fill();

            // Selection outline.
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
            cr.set_line_width(1.5);
            cr.rectangle(rx + 0.5, ry + 0.5, rw - 1.0, rh - 1.0);
            let _ = cr.stroke();

            // Corner handles — small squares to hint "drag me".
            cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
            let k = 5.0;
            for (cx, cy) in [(rx, ry), (rx + rw, ry), (rx, ry + rh), (rx + rw, ry + rh)] {
                cr.rectangle(cx - k / 2.0, cy - k / 2.0, k, k);
            }
            let _ = cr.fill();
        });
    }

    // Spin changes → queue redraw of the overlay.
    for spin in [x_spin, y_spin, w_spin, h_spin] {
        let area = selection_area.clone();
        spin.connect_changed(move |_| area.queue_draw());
    }

    // Drag semantics — three modes, decided at drag_begin from the click
    // point's position relative to the current rect:
    //   - on a corner handle  → ResizeCorner (opposite corner anchors)
    //   - strictly inside     → Move (rect translates, size preserved)
    //   - outside             → NewSelection (classic draw-a-box)
    let drag = gtk::GestureDrag::builder().button(1).build();
    let drag_mode: Rc<Cell<CropDragMode>> =
        Rc::new(Cell::new(CropDragMode::NewSelection { anchor_nat: (0.0, 0.0) }));
    {
        let area = selection_area.clone();
        let mode = drag_mode.clone();
        let x_spin = x_spin.clone();
        let y_spin = y_spin.clone();
        let w_spin = w_spin.clone();
        let h_spin = h_spin.clone();
        drag.connect_drag_begin(move |_, sx, sy| {
            let (scale, ox, oy) = contain_fit(
                area.width() as f64,
                area.height() as f64,
                natural.0 as f64,
                natural.1 as f64,
            );
            if scale <= 0.0 {
                return;
            }
            let rect = (x_spin.value(), y_spin.value(), w_spin.value(), h_spin.value());
            let decided = decide_drag_mode((sx, sy), rect, (scale, ox, oy), natural);
            mode.set(decided);
            if let CropDragMode::NewSelection { anchor_nat } = decided {
                // Seed a 1px rect anchored at the click so the overlay is
                // visible immediately — update handler will expand it.
                x_spin.set_value(anchor_nat.0);
                y_spin.set_value(anchor_nat.1);
                w_spin.set_value(1.0);
                h_spin.set_value(1.0);
            }
        });
    }
    {
        let area = selection_area.clone();
        let mode = drag_mode.clone();
        let x_spin = x_spin.clone();
        let y_spin = y_spin.clone();
        let w_spin = w_spin.clone();
        let h_spin = h_spin.clone();
        drag.connect_drag_update(move |_, dx, dy| {
            let (scale, _ox, _oy) = contain_fit(
                area.width() as f64,
                area.height() as f64,
                natural.0 as f64,
                natural.1 as f64,
            );
            if scale <= 0.0 {
                return;
            }
            let (x0, y0, w, h) = apply_drag(mode.get(), (dx / scale, dy / scale), natural);
            x_spin.set_value(x0);
            y_spin.set_value(y0);
            w_spin.set_value(w);
            h_spin.set_value(h);
        });
    }
    selection_area.add_controller(drag);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&picture));
    overlay.add_overlay(&selection_area);

    let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    column.append(&overlay);

    let hint = gtk::Label::builder()
        .label(gettext("Arraste nos cantos para redimensionar · dentro do recorte para mover · fora dele para uma nova seleção."))
        .wrap(true)
        .xalign(0.5)
        .margin_top(6)
        .build();
    hint.add_css_class("dim-label");
    column.append(&hint);

    column
}

/// Crop modal: rectangular window by (x, y, w, h) with live preview.
pub fn run_crop_dialog(files: Vec<PathBuf>) -> i32 {
    let files = Rc::new(files);
    run_dialog_app(move |app| build_crop_dialog(app, files.clone()).present())
}

pub(in crate::gui) fn build_crop_dialog(
    app: &adw::Application,
    files: Rc<Vec<PathBuf>>,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(900)
        .default_height(560)
        .title(gettext("Recortar — Prisma"))
        .build();
    prepare_dialog(&window, files, fill_crop_dialog);
    window
}

fn fill_crop_dialog(
    window: adw::ApplicationWindow,
    files: Rc<Vec<PathBuf>>,
    preview_session: Option<Rc<PreviewSession>>,
) -> adw::ApplicationWindow {
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(&gettext("Recortar"), "Prisma")));

    let content_h = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(16)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();

    // Preview is keyed off the first file — multi-file dialogs share the
    // rect and per-file validation catches out-of-bounds at apply time.
    let natural = preview_session.as_ref().map(|s| s.natural_size()).unwrap_or((1, 1));
    let initial = default_crop(natural.0, natural.1);

    // Form column — native-width controls for coordinates and size.
    let form_col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .width_request(360)
        .build();

    let files_row = selection_row(&gettext("Arquivos selecionados"), &files);

    let natural_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Tamanho original"))
        .subtitle(format!("{} × {} px ({})", natural.0, natural.1, gettext("Primeiro arquivo")))
        .build();

    // Ranges clamp to the first file's natural dims. For multi-file sets
    // with different sizes, per-file validation handles the rest.
    let x_spin = adw::SpinRow::with_range(0.0, (natural.0.saturating_sub(1)) as f64, 1.0);
    x_spin.set_title(&gettext("X (px)"));
    x_spin.set_value(initial.x as f64);

    let y_spin = adw::SpinRow::with_range(0.0, (natural.1.saturating_sub(1)) as f64, 1.0);
    y_spin.set_title(&gettext("Y (px)"));
    y_spin.set_value(initial.y as f64);

    let w_spin = adw::SpinRow::with_range(1.0, natural.0 as f64, 1.0);
    w_spin.set_title(&gettext("Largura (px)"));
    w_spin.set_value(initial.width as f64);

    let h_spin = adw::SpinRow::with_range(1.0, natural.1 as f64, 1.0);
    h_spin.set_title(&gettext("Altura (px)"));
    h_spin.set_value(initial.height as f64);

    let target_row = target_format_row();

    let overwrite_row = overwrite_row();

    let group = adw::PreferencesGroup::new();
    group.add(&files_row);
    group.add(&natural_row);
    group.add(&x_spin);
    group.add(&y_spin);
    group.add(&w_spin);
    group.add(&h_spin);
    group.add(&target_row);
    group.add(&overwrite_row);
    form_col.append(&group);

    let status = status_label();
    form_col.append(&status);

    let reset_btn = gtk::Button::with_label(&gettext("Centro 50%"));
    let (cancel_btn, apply_btn, actions) = action_buttons(&gettext("Recortar"));
    actions.prepend(&reset_btn);
    form_col.append(&actions);

    content_h.append(&form_col);

    // Interactive preview: full thumbnail + DrawingArea overlay that strokes
    // the selection rect and shades the outside. Spin rows are the source of
    // truth; drag gestures update them and the overlay redraws.
    if let Some(sess) = preview_session.as_ref() {
        let column = build_crop_preview_column(sess, natural, &x_spin, &y_spin, &w_spin, &h_spin);
        content_h.append(&column);
    }

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&content_h));
    window.set_content(Some(&toolbar_view));

    // Reset button → re-centre to 50% of the first image.
    {
        let x_spin = x_spin.clone();
        let y_spin = y_spin.clone();
        let w_spin = w_spin.clone();
        let h_spin = h_spin.clone();
        dialog_signal!(window, reset_btn, connect_clicked, move |_| {
            let d = default_crop(natural.0, natural.1);
            w_spin.set_value(d.width as f64);
            h_spin.set_value(d.height as f64);
            x_spin.set_value(d.x as f64);
            y_spin.set_value(d.y as f64);
        });
    }

    close_on_click(&window, &cancel_btn);

    {
        let files = files.clone();
        let x_spin = x_spin.clone();
        let y_spin = y_spin.clone();
        let w_spin = w_spin.clone();
        let h_spin = h_spin.clone();
        let target_row = target_row.clone();
        let overwrite_row = overwrite_row.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let rect = CropRect::new(
                x_spin.value() as u32,
                y_spin.value() as u32,
                w_spin.value() as u32,
                h_spin.value() as u32,
            );
            let target = selected_target_format(&target_row);
            let policy = selected_overwrite(&overwrite_row);

            apply_btn.set_sensitive(false);
            cancel_btn.set_sensitive(false);
            status.set_text(&gettext("Recortando…"));

            let work_files: Vec<PathBuf> = (*files).clone();
            run_batch_async(
                work_files,
                move |f| crop_file(f, rect, target, policy),
                gettext("Recortando"),
                status.clone(),
                apply_btn.clone(),
                cancel_btn.clone(),
                window.clone(),
            );
        });
    }

    window
}
