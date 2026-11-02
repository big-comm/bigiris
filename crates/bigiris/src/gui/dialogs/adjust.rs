// SPDX-License-Identifier: GPL-3.0-or-later
//! Ajustar cores: brightness, contrast, saturation and gamma.

use super::*;

/// Adjust modal: brightness + contrast + saturation + gamma.
pub fn run_adjust_dialog(files: Vec<PathBuf>) -> i32 {
    let files = Rc::new(files);
    run_dialog_app(move |app| build_adjust_dialog(app, files.clone()).present())
}

pub(in crate::gui) fn build_adjust_dialog(
    app: &adw::Application,
    files: Rc<Vec<PathBuf>>,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(880)
        .default_height(640)
        .title(gettext("Ajustar cores — Prisma"))
        .build();
    prepare_dialog(&window, files, fill_adjust_dialog);
    window
}

fn fill_adjust_dialog(
    window: adw::ApplicationWindow,
    files: Rc<Vec<PathBuf>>,
    preview_session: Option<Rc<PreviewSession>>,
) -> adw::ApplicationWindow {
    // Preview session — loaded from the first file. Multi-select still works
    // (Aplicar opera em todos), mas o preview mostra o primeiro.

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(&gettext("Ajustar cores"), "Prisma")));

    let brightness = adw::SpinRow::with_range(-100.0, 100.0, 1.0);
    brightness.set_title(&gettext("Brilho"));
    brightness.set_subtitle(&gettext("-100 escuro · 0 neutro · +100 claro"));
    brightness.set_value(0.0);

    let contrast = adw::SpinRow::with_range(-100.0, 100.0, 1.0);
    contrast.set_title(&gettext("Contraste"));
    contrast.set_subtitle(&gettext("-100 plano · 0 neutro · +100 agressivo"));
    contrast.set_value(0.0);

    let saturation = adw::SpinRow::with_range(-100.0, 100.0, 1.0);
    saturation.set_title(&gettext("Saturação"));
    saturation.set_subtitle(&gettext("-100 cinza · 0 neutro · +100 vibrante"));
    saturation.set_value(0.0);

    let gamma = adw::SpinRow::with_range(0.1, 10.0, 0.05);
    gamma.set_title(&gettext("Gamma"));
    gamma.set_subtitle(&gettext(
        "Abaixo de 1 clareia os meios-tons · 1 mantém · acima de 1 escurece",
    ));
    gamma.set_digits(2);
    gamma.set_value(1.0);

    let target_row = target_format_row();

    let overwrite_row = overwrite_row();

    let files_row = selection_row(&gettext("Arquivos selecionados"), &files);

    let group_files = adw::PreferencesGroup::new();
    group_files.add(&files_row);

    let group_tone = adw::PreferencesGroup::builder().title(gettext("Tom")).build();
    group_tone.add(&brightness);
    group_tone.add(&contrast);
    group_tone.add(&gamma);

    let group_color = adw::PreferencesGroup::builder().title(gettext("Cor")).build();
    group_color.add(&saturation);

    let group_out = adw::PreferencesGroup::builder().title(gettext("Saída")).build();
    group_out.add(&target_row);
    group_out.add(&overwrite_row);

    let status = status_label();

    let (cancel_btn, apply_btn, actions_box) = action_buttons(&gettext("Aplicar"));

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    content.append(&group_files);
    content.append(&group_tone);
    content.append(&group_color);
    content.append(&group_out);
    content.append(&status);
    content.append(&actions_box);

    let scrolled = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&content)
        .build();
    scrolled.set_size_request(420, -1);

    // Two-column layout: scrollable form on the left, live preview column
    // on the right (when a session is available).
    let main_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(16)
        .margin_top(0)
        .margin_bottom(0)
        .build();
    main_box.append(&scrolled);

    let preview_picture: Option<gtk::Picture> = if let Some(sess) = preview_session.as_ref() {
        let (picture, column) = build_preview_column(sess);
        column.set_margin_top(16);
        column.set_margin_bottom(16);
        column.set_margin_end(16);
        main_box.append(&column);
        Some(picture)
    } else {
        None
    };

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&main_box));
    window.set_content(Some(&toolbar_view));

    // Wire live preview on every slider / gamma change.
    if let (Some(picture), Some(session)) = (preview_picture.as_ref(), preview_session.as_ref()) {
        let debounce = PreviewDebounce::new();
        let sliders: [adw::SpinRow; 4] =
            [brightness.clone(), contrast.clone(), saturation.clone(), gamma.clone()];
        for row in &sliders {
            let debounce = debounce.clone();
            let session = session.clone();
            let picture = picture.clone();
            let brightness = brightness.clone();
            let contrast = contrast.clone();
            let saturation = saturation.clone();
            let gamma = gamma.clone();
            dialog_signal!(window, row, connect_changed, move |_| {
                let ops = AdjustOps {
                    brightness: brightness.value() as i32,
                    contrast: contrast.value() as f32,
                    saturation: saturation.value() as f32,
                    gamma: gamma.value() as f32,
                };
                let session = session.clone();
                let picture = picture.clone();
                debounce.trigger(Duration::from_millis(80), move || {
                    refresh_preview(&session, &picture, &PreviewOp::Adjust(ops));
                });
            });
        }
    }

    close_on_click(&window, &cancel_btn);

    {
        let files = files.clone();
        let brightness = brightness.clone();
        let contrast = contrast.clone();
        let saturation = saturation.clone();
        let gamma = gamma.clone();
        let target_row = target_row.clone();
        let overwrite_row = overwrite_row.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let ops = AdjustOps {
                brightness: brightness.value() as i32,
                contrast: contrast.value() as f32,
                saturation: saturation.value() as f32,
                gamma: gamma.value() as f32,
            };
            let target = selected_target_format(&target_row);
            let policy = selected_overwrite(&overwrite_row);

            apply_btn.set_sensitive(false);
            cancel_btn.set_sensitive(false);
            status.set_text(&gettext("Ajustando…"));

            let work_files: Vec<PathBuf> = (*files).clone();
            run_batch_async(
                work_files,
                move |f| adjust_file(f, ops, target, policy),
                gettext("Ajustando"),
                status.clone(),
                apply_btn.clone(),
                cancel_btn.clone(),
                window.clone(),
            );
        });
    }

    window
}
