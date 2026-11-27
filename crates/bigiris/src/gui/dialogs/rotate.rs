// SPDX-License-Identifier: GPL-3.0-or-later
//! Rotate: rotate by 90, 180 or 270 degrees.

use super::*;

// ---------------------------------------------------------------------------
// Rotate / Flip / Adjust dialogs — built on top of the same run_*_batch +
// finish_dialog primitives used by convert/resize, just with different forms.
// ---------------------------------------------------------------------------

/// Rotate modal: pick 90 / 180 / 270 and an optional target format.
pub fn run_rotate_dialog(files: Vec<PathBuf>) -> i32 {
    let files = Rc::new(files);
    run_dialog_app(move |app| build_rotate_dialog(app, files.clone()).present())
}

pub(in crate::gui) fn build_rotate_dialog(
    app: &adw::Application,
    files: Rc<Vec<PathBuf>>,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(440)
        .title(gettext("Rotate images — Prisma"))
        .build();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(&gettext("Rotate"), "Prisma")));

    let rotations = [n_("90° (clockwise)"), n_("180°"), n_("270° (counterclockwise)")];
    let rotation_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Rotation"))
        .model(&translated_strings(&rotations))
        .build();

    let target_row = target_format_row();

    let overwrite_row = overwrite_row();

    let files_row = selection_row(&gettext("Selected files"), &files);

    let group = adw::PreferencesGroup::new();
    group.add(&files_row);
    group.add(&rotation_row);
    group.add(&target_row);
    group.add(&overwrite_row);

    let status = status_label();

    let content = padded_box(gtk::Orientation::Vertical, 16);
    content.append(&group);
    let apply_btn =
        set_form_dialog_content(&window, &header, &content, &status, &gettext("Rotate"));

    {
        let window = window.clone();
        dialog_signal!(window, apply_btn, connect_clicked, move |_| {
            let rotation = match rotation_row.selected() {
                0 => Rotation::Deg90,
                1 => Rotation::Deg180,
                _ => Rotation::Deg270,
            };
            let target = selected_target_format(&target_row);
            let policy = selected_overwrite(&overwrite_row);

            apply_btn.set_sensitive(false);
            status.set_text(&gettext("Rotating…"));

            let work_files: Vec<PathBuf> = (*files).clone();
            run_batch_async(
                work_files,
                crate::output_plan::Naming::image(
                    target,
                    policy,
                    crate::output_plan::OutputSuffix::Fixed(rotation.filename_suffix().into()),
                ),
                move |f| rotate_file(f, rotation, target, policy),
                gettext("Rotating"),
                status.clone(),
                apply_btn.clone(),
                window.clone(),
            );
        });
    }

    window
}
