// SPDX-License-Identifier: GPL-3.0-or-later
//! Converter: change the format of the selected images.

use super::*;

/// Run the "Converter" modal. Returns GTK's exit code.
pub fn run_convert_dialog(files: Vec<PathBuf>) -> i32 {
    let files = Rc::new(files);
    run_dialog_app(move |app| build_convert_dialog(app, files.clone()).present())
}

pub(in crate::gui) fn build_convert_dialog(
    app: &adw::Application,
    files: Rc<Vec<PathBuf>>,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(880)
        .default_height(500)
        .title(gettext("Converter imagens — Prisma"))
        .build();
    prepare_dialog(&window, files, fill_convert_dialog);
    window
}

fn fill_convert_dialog(
    window: adw::ApplicationWindow,
    files: Rc<Vec<PathBuf>>,
    preview_session: Option<Rc<PreviewSession>>,
) -> adw::ApplicationWindow {
    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new(&gettext("Converter"), "Prisma");
    header.set_title_widget(Some(&title));

    let format_row = format_row();

    let overwrite_row = overwrite_row();

    let files_row = selection_row(&gettext("Arquivos selecionados"), &files);

    let quality_spin = adw::SpinRow::with_range(1.0, 100.0, 1.0);
    quality_spin.set_title(&gettext("Qualidade (JPEG/AVIF)"));
    quality_spin.set_subtitle(&gettext("1 menor qualidade · 85 equilíbrio · 100 maior qualidade"));
    quality_spin.set_value(85.0);

    let optimize_switch = adw::SwitchRow::builder()
        .use_markup(false)
        .title(gettext("Otimizar tamanho"))
        .subtitle(gettext("PNG com compressão máxima; AVIF mais lento; arquivo menor"))
        .active(false)
        .build();

    // Keep encoder tuning outside the primary conversion workflow.
    let advanced_expander = adw::ExpanderRow::builder()
        .title(gettext("Avançado"))
        .subtitle(gettext("Otimização do encoder"))
        .build();
    advanced_expander.add_row(&optimize_switch);
    bind_encode_controls(&window, &format_row, &quality_spin, &optimize_switch);

    // ── Conversão group ───────────────────────────────────────────────
    let conv_group = adw::PreferencesGroup::builder().title(gettext("Conversão")).build();
    conv_group.add(&files_row);
    conv_group.add(&format_row);
    conv_group.add(&quality_spin);
    conv_group.add(&advanced_expander);

    // ── Destino group: overwrite policy + optional output folder ──────
    let dest_group = adw::PreferencesGroup::builder().title(gettext("Destino")).build();
    dest_group.add(&overwrite_row);

    // Pasta de saída picker (mirror do Batch dialog) — None = mesma
    // pasta do arquivo de origem (comportamento histórico).
    let output_dir: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
    let folder_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Pasta de saída"))
        .subtitle(gettext("Mesma pasta do arquivo de origem"))
        .activatable(true)
        .build();
    let folder_icon = gtk::Image::from_icon_name("folder-open-symbolic");
    folder_icon.add_css_class("dim-label");
    folder_row.add_suffix(&folder_icon);
    let clear_folder_btn = gtk::Button::builder()
        .icon_name("edit-clear-symbolic")
        .tooltip_text(gettext("Usar a mesma pasta do arquivo de origem"))
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .visible(false)
        .build();
    folder_row.add_suffix(&clear_folder_btn);
    dest_group.add(&folder_row);

    let status = status_label();

    let (cancel_btn, apply_btn, actions) = action_buttons(&gettext("Converter"));

    // Smart prompt: if the source image has an alpha channel and the user
    // picks JPEG, warn that transparency will be flattened.
    let alpha_banner = adw::Banner::builder()
        .title(gettext(
            "Esta imagem tem transparência — JPEG perde o canal alfa. Considere PNG ou WebP.",
        ))
        .revealed(false)
        .build();
    if let Some(sess) = preview_session.as_ref() {
        if sess.has_alpha_channel() {
            let format_row_clone = format_row.clone();
            let banner = alpha_banner.clone();
            let update = move || {
                let fmt = selected_format(&format_row_clone);
                let warn = matches!(fmt, Format::Jpeg);
                banner.set_revealed(warn);
            };
            update();
            dialog_signal!(window, format_row, connect_selected_notify, move |_| update());
        }
    }

    // Smart prompt: LGPD / privacidade — se a imagem tem coordenadas GPS
    // no EXIF, avisa que o upload pode vazar a localização.
    let gps_banner = adw::Banner::builder()
        .title(gettext("Esta imagem contém GPS (localização) no EXIF. Atenção ao compartilhar publicamente — nosso pipeline remove o EXIF automaticamente no re-encode."))
        .revealed(false)
        .build();
    gps_banner.add_css_class("warning");
    if let Some(sess) = preview_session.as_ref() {
        if sess.source_has_gps() {
            gps_banner.set_revealed(true);
        }
    }

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    content.append(&gps_banner);
    content.append(&alpha_banner);
    content.append(&conv_group);
    content.append(&dest_group);
    content.append(&status);
    content.append(&actions);
    content.set_size_request(420, -1);

    // Wire up the output-folder picker + reset button.
    {
        let output_dir = output_dir.clone();
        let folder_row_for_pick = folder_row.clone();
        let clear_btn = clear_folder_btn.clone();
        let window = window.clone();
        dialog_signal!(window, folder_row.clone(), connect_activated, move |_| {
            pick_output_folder(&window, &output_dir, &folder_row_for_pick, &clear_btn);
        });
    }
    {
        let output_dir = output_dir.clone();
        let folder_row = folder_row.clone();
        let clear_btn = clear_folder_btn.clone();
        dialog_signal!(window, clear_folder_btn, connect_clicked, move |_| {
            *output_dir.borrow_mut() = None;
            folder_row.set_subtitle(&gettext("Mesma pasta do arquivo de origem"));
            clear_btn.set_visible(false);
        });
    }

    let main_box =
        gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(16).build();
    main_box.append(&content);
    if let Some(sess) = preview_session.as_ref() {
        let (_picture, column) = build_preview_column(sess);
        column.set_margin_top(16);
        column.set_margin_bottom(16);
        column.set_margin_end(16);
        main_box.append(&column);
        // Convert is format-level — visual preview is just the source
        // thumbnail (pixels don't change meaningfully between PNG/JPG/WebP
        // at default quality). No refresh wiring needed.
    }

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&main_box));
    window.set_content(Some(&toolbar_view));

    close_on_click(&window, &cancel_btn);

    {
        let files = files.clone();
        let format_row = format_row.clone();
        let overwrite_row = overwrite_row.clone();
        let quality_spin = quality_spin.clone();
        let optimize_switch = optimize_switch.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        let output_dir = output_dir.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let fmt = selected_format(&format_row);
            let policy = selected_overwrite(&overwrite_row);
            let opts = EncodeOptions {
                quality: Some(quality_spin.value() as u8),
                optimize: optimize_switch.is_active(),
                ..EncodeOptions::default()
            };
            // Snapshot the chosen folder once at submit time — the
            // picker can change while the worker is running and the
            // worker thread mustn't peek into Rc<RefCell<...>>.
            let out_dir_snapshot = output_dir.borrow().clone();

            apply_btn.set_sensitive(false);
            cancel_btn.set_sensitive(false);
            status.set_text(&gettext("Convertendo…"));

            let work_files: Vec<PathBuf> = (*files).clone();
            run_batch_async(
                work_files,
                move |f| convert_file_to(f, out_dir_snapshot.as_deref(), fmt, &opts, policy),
                gettext("Convertendo"),
                status.clone(),
                apply_btn.clone(),
                cancel_btn.clone(),
                window.clone(),
            );
        });
    }

    window
}
