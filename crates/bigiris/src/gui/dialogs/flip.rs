// SPDX-License-Identifier: GPL-3.0-or-later
//! Espelhar: horizontal or vertical mirror.

use super::*;

/// Flip modal: horizontal or vertical mirror.
pub fn run_flip_dialog(files: Vec<PathBuf>) -> i32 {
    let files = Rc::new(files);
    run_dialog_app(move |app| build_flip_dialog(app, files.clone()).present())
}

pub(in crate::gui) fn build_flip_dialog(
    app: &adw::Application,
    files: Rc<Vec<PathBuf>>,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(440)
        .default_height(340)
        .title(gettext("Espelhar imagens — Prisma"))
        .build();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(&gettext("Espelhar"), "Prisma")));

    let axes = [n_("Horizontal (esquerda ↔ direita)"), n_("Vertical (cima ↔ baixo)")];
    let axis_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Eixo"))
        .model(&translated_strings(&axes))
        .build();

    let target_row = target_format_row();

    let overwrite_row = overwrite_row();

    let files_row = selection_row(&gettext("Arquivos selecionados"), &files);

    let group = adw::PreferencesGroup::new();
    group.add(&files_row);
    group.add(&axis_row);
    group.add(&target_row);
    group.add(&overwrite_row);

    let status = status_label();

    let (cancel_btn, apply_btn, actions) = action_buttons(&gettext("Espelhar"));

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    content.append(&group);
    content.append(&status);
    content.append(&actions);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&content));
    window.set_content(Some(&toolbar_view));

    close_on_click(&window, &cancel_btn);

    {
        let files = files.clone();
        let axis_row = axis_row.clone();
        let target_row = target_row.clone();
        let overwrite_row = overwrite_row.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let axis = match axis_row.selected() {
                0 => FlipAxis::Horizontal,
                _ => FlipAxis::Vertical,
            };
            let target = selected_target_format(&target_row);
            let policy = selected_overwrite(&overwrite_row);

            apply_btn.set_sensitive(false);
            cancel_btn.set_sensitive(false);
            status.set_text(&gettext("Espelhando…"));

            let work_files: Vec<PathBuf> = (*files).clone();
            run_batch_async(
                work_files,
                move |f| flip_file(f, axis, target, policy),
                gettext("Espelhando"),
                status.clone(),
                apply_btn.clone(),
                cancel_btn.clone(),
                window.clone(),
            );
        });
    }

    window
}
