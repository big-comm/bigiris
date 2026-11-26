// SPDX-License-Identifier: GPL-3.0-or-later
//! Compare: two images side by side with a draggable divider.

use super::*;

/// Open the compare window on the first two files; any further files are
/// ignored. With fewer than two, the viewer opens instead.
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
        .default_height(640)
        .title(gettext("Compare images — Íris"))
        .build();

    let left_path = &files[0];
    let right_path = &files[1];

    let header = adw::HeaderBar::new();
    let left_name = crate::codec::printable_name(left_path);
    let right_name = crate::codec::printable_name(right_path);
    let title = adw::WindowTitle::new(
        &gettext("Compare images"),
        &fill(
            &gettext("Before: {before}  ·  After: {after}"),
            &[("before", &left_name), ("after", &right_name)],
        ),
    );
    header.set_title_widget(Some(&title));

    // The title bar names both sides, so the pictures get the full height.
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
        .label(gettext("Drag the divider to compare · Esc to close"))
        .xalign(0.5)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    hint.add_css_class("dim-label");

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(0).build();
    content.append(&paned);
    content.append(&hint);

    set_dialog_content(&window, &header, &content);

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
    // Awaited on the GTK loop instead of polled on a timer.
    let job = gio::spawn_blocking(move || {
        crate::codec::with_cancel(cancel, || {
            bigimage_core::open_image(input)
                .map(|image| {
                    let rgba = bigimage_core::into_srgb_rgba8(image.thumbnail(2048, 2048));
                    let size = rgba.dimensions();
                    (rgba.into_raw(), size)
                })
                .map_err(|error| image_error_text(&error))
        })
    });
    let weak_picture = picture.downgrade();
    let weak_column = column.downgrade();
    let weak_spinner = spinner.downgrade();
    let weak_window = window.downgrade();
    glib::MainContext::default().spawn_local(async move {
        // A decoder that died without an answer must not leave the spinner
        // turning forever.
        let result = job.await.unwrap_or_else(|_| Err(gettext("Could not open this image")));
        if weak_window.upgrade().is_none_or(|w| !w.is_visible()) {
            return;
        }
        let Some(column) = weak_column.upgrade() else { return };
        if let Some(spinner) = weak_spinner.upgrade() {
            column.remove(&spinner);
        }
        match result {
            Ok((raw, (w, h))) => {
                if let Some(picture) = weak_picture.upgrade() {
                    picture.set_paintable(Some(&texture_from_rgba(raw, w, h)));
                }
            }
            Err(error) => {
                let label = gtk::Label::new(Some(&error));
                label.set_wrap(true);
                column.append(&label);
            }
        }
    });
    column
}
