// SPDX-License-Identifier: GPL-3.0-or-later
//! Letterbox: extend the canvas to a fixed aspect ratio without cropping.

use super::*;

/// Aspect-ratio presets for the "Letterbox" (extent) dialog. Each preset
/// is `(label, (w, h))` with `w:h` the target ratio. Order drives the
/// default selection — YouTube community 1:1 sits first because it's the
/// case that motivated the feature (post thumbnail forces square crop on
/// 16:9 source).
const EXTENT_RATIO_PRESETS: &[(&str, (u32, u32))] = &[
    (n_("YouTube community (1:1)"), (1, 1)),
    (n_("Instagram feed (4:5)"), (4, 5)),
    (n_("Stories / Shorts (9:16)"), (9, 16)),
    (n_("Widescreen (16:9)"), (16, 9)),
    (n_("Personalizado…"), (0, 0)),
];

/// Background-colour presets for the "Letterbox" dialog. The first entry
/// is the default. `Custom` (last) reveals the hex entry.
const EXTENT_BG_PRESETS: &[(&str, [u8; 4])] = &[
    (n_("Preto suave (#0d0e12)"), [0x0d, 0x0e, 0x12, 0xFF]),
    (n_("Preto puro (#000000)"), [0, 0, 0, 0xFF]),
    (n_("Branco (#ffffff)"), [0xFF, 0xFF, 0xFF, 0xFF]),
    (n_("Transparente"), [0, 0, 0, 0]),
    (n_("Personalizada (hex)"), [0, 0, 0, 0xFF]),
];

/// Gravity choices exposed in the Prisma dialog. Limited to centre +
/// vertical pinning because horizontal letterbox use cases are rarer in
/// social-media workflows; the CLI exposes all nine for completeness.
const EXTENT_GRAVITIES: &[(&str, Gravity)] =
    &[(n_("Centro"), Gravity::Center), (n_("Topo"), Gravity::North), (n_("Base"), Gravity::South)];

/// Run the "Letterbox" modal. Returns GTK's exit code.
pub fn run_extent_dialog(files: Vec<PathBuf>) -> i32 {
    let files = Rc::new(files);
    run_dialog_app(move |app| build_extent_dialog(app, files.clone()).present())
}

pub(in crate::gui) fn build_extent_dialog(
    app: &adw::Application,
    files: Rc<Vec<PathBuf>>,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(900)
        .default_height(620)
        .title(gettext("Letterbox — Prisma"))
        .build();
    prepare_dialog(&window, files, fill_extent_dialog);
    window
}

fn fill_extent_dialog(
    window: adw::ApplicationWindow,
    files: Rc<Vec<PathBuf>>,
    preview_session: Option<Rc<PreviewSession>>,
) -> adw::ApplicationWindow {
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        &gettext("Letterbox"),
        &gettext("Prisma · sem corte"),
    )));

    let content_h = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(16)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();

    let natural = preview_session.as_ref().map(|s| s.natural_size()).unwrap_or((1, 1));

    let form_col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .width_request(380)
        .build();

    let intro = gtk::Label::builder()
        .label(gettext(
            "Estende a tela em volta da imagem para encaixá-la num formato fixo (post de YouTube, feed do Instagram, Stories…) sem cortar nenhum pixel. A imagem original é centralizada e o resto é preenchido com a cor escolhida.",
        ))
        .wrap(true)
        .xalign(0.0)
        .build();
    intro.add_css_class("dim-label");

    let files_row = selection_row(&gettext("Arquivos selecionados"), &files);

    let natural_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Tamanho original"))
        .subtitle(format!("{} × {} px ({})", natural.0, natural.1, gettext("Primeiro arquivo")))
        .build();

    // Aspect preset combo.
    let ratio_names: Vec<&str> = EXTENT_RATIO_PRESETS.iter().map(|(n, _)| *n).collect();
    let ratio_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Proporção alvo"))
        .model(&translated_strings(&ratio_names))
        .build();

    // Custom W:H entries — visible only when the "Personalizado…" preset is
    // selected. Defaults to 1:1 so switching back and forth feels stable.
    let custom_w_spin = adw::SpinRow::with_range(1.0, 99.0, 1.0);
    custom_w_spin.set_title(&gettext("Razão W"));
    custom_w_spin.set_value(1.0);
    let custom_h_spin = adw::SpinRow::with_range(1.0, 99.0, 1.0);
    custom_h_spin.set_title(&gettext("Razão H"));
    custom_h_spin.set_value(1.0);
    custom_w_spin.set_visible(false);
    custom_h_spin.set_visible(false);

    // Background colour combo + hex entry (entry shown when "Personalizada"
    // is picked).
    let bg_names: Vec<&str> = EXTENT_BG_PRESETS.iter().map(|(n, _)| *n).collect();
    let bg_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Cor de fundo"))
        .model(&translated_strings(&bg_names))
        .build();

    let bg_hex = adw::EntryRow::builder()
        .use_markup(false)
        .title(gettext("Hex (#RRGGBB)"))
        .text("#0d0e12")
        .build();
    bg_hex.set_visible(false);

    let gravity_names: Vec<&str> = EXTENT_GRAVITIES.iter().map(|(n, _)| *n).collect();
    let gravity_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Posição da imagem"))
        .model(&translated_strings(&gravity_names))
        .build();

    let predicted_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Destino previsto"))
        .subtitle("—")
        .build();

    let target_row = target_format_row();

    let overwrite_row = overwrite_row();

    let group = adw::PreferencesGroup::new();
    group.add(&files_row);
    group.add(&natural_row);
    group.add(&ratio_row);
    group.add(&custom_w_spin);
    group.add(&custom_h_spin);
    group.add(&bg_row);
    group.add(&bg_hex);
    group.add(&gravity_row);
    group.add(&predicted_row);
    group.add(&target_row);
    group.add(&overwrite_row);

    form_col.append(&intro);
    form_col.append(&group);

    let status = status_label();
    form_col.append(&status);

    let (cancel_btn, apply_btn, actions) = action_buttons(&gettext("Estender"));
    form_col.append(&actions);

    content_h.append(&form_col);

    // Live preview on the right, driven by PreviewOp::Extent.
    let picture_opt = if let Some(sess) = preview_session.as_ref() {
        let (picture, column) = build_preview_column(sess);
        content_h.append(&column);
        Some(picture)
    } else {
        None
    };

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&content_h));
    window.set_content(Some(&toolbar_view));

    // Shared closure that reads every control and produces an ExtentOptions
    // (or None if the form is in an invalid state — e.g. malformed hex).
    let build_opts = {
        let ratio_row = ratio_row.clone();
        let custom_w_spin = custom_w_spin.clone();
        let custom_h_spin = custom_h_spin.clone();
        let bg_row = bg_row.clone();
        let bg_hex = bg_hex.clone();
        let gravity_row = gravity_row.clone();
        move || -> std::result::Result<ExtentOptions, String> {
            let (rw, rh) = {
                let idx = ratio_row.selected() as usize;
                let preset = EXTENT_RATIO_PRESETS[idx].1;
                if preset == (0, 0) {
                    (custom_w_spin.value() as u32, custom_h_spin.value() as u32)
                } else {
                    preset
                }
            };
            let size = ExtentSize::Aspect { w: rw, h: rh };

            let background = {
                let idx = bg_row.selected() as usize;
                if idx == EXTENT_BG_PRESETS.len() - 1 {
                    bigimage_core::parse_hex_color(&bg_hex.text())?
                } else {
                    EXTENT_BG_PRESETS[idx].1
                }
            };

            let gravity = EXTENT_GRAVITIES[gravity_row.selected() as usize].1;

            Ok(ExtentOptions { size, background, gravity })
        }
    };

    // Refresh the predicted-size row and the preview painting on every
    // control change. Errors (bad hex) surface in the status label.
    let refresh = {
        let session_opt = preview_session.clone();
        let picture_opt = picture_opt.clone();
        let predicted_row = predicted_row.clone();
        let status = status.clone();
        let build_opts = build_opts.clone();
        move || match build_opts() {
            Ok(opts) => {
                let (nw, nh) = match opts.size.resolve(natural.0, natural.1) {
                    Ok(d) => d,
                    Err(e) => {
                        status.set_text(&e.to_string());
                        predicted_row.set_subtitle(&gettext("—"));
                        return;
                    }
                };
                predicted_row.set_subtitle(&format!("{nw} × {nh} px"));
                status.set_text("");
                if let (Some(sess), Some(picture)) = (session_opt.as_ref(), picture_opt.as_ref()) {
                    refresh_preview(sess, picture, &PreviewOp::Extent(opts));
                }
            }
            Err(e) => {
                status.set_text(&e);
            }
        }
    };

    // Reveal Custom W:H spins when the "Personalizado…" preset is picked,
    // and hide them again on every other selection.
    {
        let custom_w_spin = custom_w_spin.clone();
        let custom_h_spin = custom_h_spin.clone();
        let refresh = refresh.clone();
        dialog_signal!(window, ratio_row, connect_selected_notify, move |row| {
            let idx = row.selected() as usize;
            let is_custom = EXTENT_RATIO_PRESETS[idx].1 == (0, 0);
            custom_w_spin.set_visible(is_custom);
            custom_h_spin.set_visible(is_custom);
            refresh();
        });
    }

    // Reveal the hex entry when "Personalizada" is selected.
    {
        let bg_hex = bg_hex.clone();
        let refresh = refresh.clone();
        dialog_signal!(window, bg_row, connect_selected_notify, move |row| {
            let idx = row.selected() as usize;
            bg_hex.set_visible(idx == EXTENT_BG_PRESETS.len() - 1);
            refresh();
        });
    }

    {
        let refresh = refresh.clone();
        dialog_signal!(window, custom_w_spin, connect_value_notify, move |_| refresh());
    }
    {
        let refresh = refresh.clone();
        dialog_signal!(window, custom_h_spin, connect_value_notify, move |_| refresh());
    }
    {
        let refresh = refresh.clone();
        dialog_signal!(window, bg_hex, connect_changed, move |_| refresh());
    }
    {
        let refresh = refresh.clone();
        dialog_signal!(window, gravity_row, connect_selected_notify, move |_| refresh());
    }

    // Initial paint.
    refresh();

    close_on_click(&window, &cancel_btn);

    {
        let files = files.clone();
        let build_opts = build_opts.clone();
        let target_row = target_row.clone();
        let overwrite_row = overwrite_row.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let opts = match build_opts() {
                Ok(opts) => opts,
                Err(e) => {
                    status.set_text(&e);
                    return;
                }
            };
            let target = selected_target_format(&target_row);
            let policy = selected_overwrite(&overwrite_row);

            apply_btn.set_sensitive(false);
            cancel_btn.set_sensitive(false);
            status.set_text(&gettext("Estendendo…"));

            let work_files: Vec<PathBuf> = (*files).clone();
            run_batch_async(
                work_files,
                move |f| extent_file(f, opts, target, policy),
                gettext("Estendendo"),
                status.clone(),
                apply_btn.clone(),
                cancel_btn.clone(),
                window.clone(),
            );
        });
    }

    window
}
