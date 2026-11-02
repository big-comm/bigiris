// SPDX-License-Identifier: GPL-3.0-or-later
//! Comparar: two images side by side with a draggable divider.

use super::*;

// ---------------------------------------------------------------------------
// Compare dialog — two images in a `gtk::Paned`, drag the divider to sweep
// between "antes" and "depois". Requires exactly 2 files; the rest are
// ignored (first two in the selection are used).
// ---------------------------------------------------------------------------

/// Open the "Compare two" window. Needs two files — if fewer are given
/// we fall back to the viewer with whatever's there.
pub fn run_compare_dialog(files: Vec<PathBuf>) -> i32 {
    if files.len() < 2 {
        tracing::warn!(count = files.len(), "compare needs two files; falling back to viewer");
        return run_viewer(files);
    }
    let files = Rc::new(files);
    run_dialog_app(move |app| build_compare_dialog(app, files.clone()).present())
}

pub(in crate::gui) fn build_compare_dialog(
    app: &adw::Application,
    files: Rc<Vec<PathBuf>>,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(1200)
        .default_height(720)
        .title(gettext("Comparar imagens — Íris"))
        .build();

    let left_path = &files[0];
    let right_path = &files[1];

    let header = adw::HeaderBar::new();
    let left_name =
        left_path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let right_name =
        right_path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let title = adw::WindowTitle::new(
        &gettext("Comparar imagens"),
        &fill(
            &gettext("Antes: {before}  ·  Depois: {after}"),
            &[("before", &left_name), ("after", &right_name)],
        ),
    );
    header.set_title_widget(Some(&title));

    // Captions gone — titlebar now carries the Antes/Depois info so the
    // full vertical space goes to the pictures.
    let left_picture = make_compare_side(left_path, &window);
    let right_picture = make_compare_side(right_path, &window);

    let paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .hexpand(true)
        .vexpand(true)
        .resize_start_child(true)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .start_child(&left_picture)
        .end_child(&right_picture)
        .build();

    let hint = gtk::Label::builder()
        .label(gettext("Arraste o divisor para comparar · Esc fecha"))
        .xalign(0.5)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    hint.add_css_class("dim-label");

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(0).build();
    content.append(&paned);
    content.append(&hint);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&content));
    window.set_content(Some(&toolbar_view));

    // Position the divider at 50% once the window has real dimensions.
    {
        let paned = paned.clone();
        let window = window.clone();
        glib::idle_add_local_once(move || {
            let w = window.default_width();
            if w > 0 {
                paned.set_position(w / 2);
            }
        });
    }

    // Keyboard: Esc closes.
    let key = gtk::EventControllerKey::new();
    {
        let window = window.downgrade();
        key.connect_key_pressed(move |_, k, _, _| {
            let Some(window) = window.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if matches!(k, gdk::Key::Escape) {
                window.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }
    window.add_controller(key);

    window
}

fn make_compare_side(path: &Path, window: &adw::ApplicationWindow) -> gtk::Box {
    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_hexpand(true);
    picture.set_vexpand(true);

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .hexpand(true)
        .vexpand(true)
        .build();
    column.append(&picture);
    let spinner = adw::Spinner::new();
    column.append(&spinner);
    let cancel = Arc::new(AtomicBool::new(false));
    let closing = cancel.clone();
    window.connect_unrealize(move |_| closing.store(true, Ordering::Release));
    let input = path.to_path_buf();
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        crate::codec::with_cancel(cancel, || {
            let result = bigimage_core::open_image(input)
                .map(|image| {
                    let rgba = image.thumbnail(2048, 2048).to_rgba8();
                    let size = rgba.dimensions();
                    (rgba.into_raw(), size)
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(result);
        })
    });
    let weak_picture = picture.downgrade();
    let weak_column = column.downgrade();
    let weak_spinner = spinner.downgrade();
    let weak_window = window.downgrade();
    glib::timeout_add_local(Duration::from_millis(16), move || {
        if weak_window.upgrade().is_none_or(|w| !w.is_visible()) {
            return glib::ControlFlow::Break;
        }
        match rx.try_recv() {
            Ok(result) => {
                if let (Some(column), Some(spinner)) =
                    (weak_column.upgrade(), weak_spinner.upgrade())
                {
                    column.remove(&spinner);
                }
                match result {
                    Ok((raw, (w, h))) => {
                        if let Some(picture) = weak_picture.upgrade() {
                            picture.set_paintable(Some(&texture_from_rgba(raw, w, h)));
                        }
                    }
                    Err(error) => {
                        if let Some(column) = weak_column.upgrade() {
                            let label = gtk::Label::new(Some(&error));
                            label.set_wrap(true);
                            column.append(&label);
                        }
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => {}
        }
        glib::ControlFlow::Break
    });
    column
}
