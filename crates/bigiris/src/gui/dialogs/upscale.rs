// SPDX-License-Identifier: GPL-3.0-or-later
//! Aumentar resolução: Lanczos3 upscale by an integer factor.

use super::*;

/// Upscale factors we expose in the dialog — matches the CLI `--factor` range
/// (2..=4). Real-ESRGAN backend, when available, slots in without changing
/// this list.
pub(in crate::gui) const UPSCALE_FACTORS: &[(&str, u8)] =
    &[("2× (200%)", 2), ("3× (300%)", 3), ("4× (400%)", 4)];

/// Upscale modal: pick an integer factor and run Lanczos3 resize.
pub fn run_upscale_dialog(files: Vec<PathBuf>) -> i32 {
    let files = Rc::new(files);
    run_dialog_app(move |app| build_upscale_dialog(app, files.clone()).present())
}

pub(in crate::gui) fn build_upscale_dialog(
    app: &adw::Application,
    files: Rc<Vec<PathBuf>>,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(820)
        .default_height(500)
        .title(gettext("Aumentar resolução — Prisma"))
        .build();
    prepare_dialog(&window, files, fill_upscale_dialog);
    window
}

fn fill_upscale_dialog(
    window: adw::ApplicationWindow,
    files: Rc<Vec<PathBuf>>,
    preview_session: Option<Rc<PreviewSession>>,
) -> adw::ApplicationWindow {
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        &gettext("Aumentar resolução"),
        "Prisma · Lanczos3",
    )));

    let content_h = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(16)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();

    let natural = preview_session.as_ref().map(|s| s.natural_size()).unwrap_or((0, 0));

    let form_col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .width_request(340)
        .build();

    // Hook — shown when an IA backend lands. Today just an informative note.
    let engine_note = gtk::Label::builder()
        .label(gettext("Motor: Lanczos3 (CPU). O modo IA (Real-ESRGAN) será habilitado automaticamente quando um modelo ONNX FOSS estiver disponível."))
        .wrap(true)
        .xalign(0.0)
        .build();
    engine_note.add_css_class("dim-label");

    let files_row = selection_row(&gettext("Arquivos selecionados"), &files);

    let natural_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Tamanho original"))
        .subtitle(if natural == (0, 0) {
            "—".to_string()
        } else {
            format!("{} × {} px ({})", natural.0, natural.1, gettext("Primeiro arquivo"))
        })
        .build();

    let factor_names: Vec<&str> = UPSCALE_FACTORS.iter().map(|(n, _)| *n).collect();
    let factor_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Fator"))
        .model(&translated_strings(&factor_names))
        .build();

    let predicted_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Destino previsto"))
        .subtitle("—")
        .build();
    {
        let factor_row_c = factor_row.clone();
        let predicted_row_c = predicted_row.clone();
        let refresh_prediction = move || {
            let f = UPSCALE_FACTORS[factor_row_c.selected() as usize].1;
            let sub = if natural == (0, 0) {
                fill(&gettext("× {factor} (tamanho indisponível)"), &[("factor", &f.to_string())])
            } else {
                let w = (natural.0 as u64) * (f as u64);
                let h = (natural.1 as u64) * (f as u64);
                format!("≈ {w} × {h} px")
            };
            predicted_row_c.set_subtitle(&sub);
        };
        refresh_prediction();
        dialog_signal!(window, factor_row, connect_selected_notify, move |_| refresh_prediction());
    }

    let target_row = target_format_row();

    let overwrite_row = overwrite_row();

    let group = adw::PreferencesGroup::new();
    group.add(&files_row);
    group.add(&natural_row);
    group.add(&factor_row);
    group.add(&predicted_row);
    group.add(&target_row);
    group.add(&overwrite_row);

    form_col.append(&engine_note);
    form_col.append(&group);

    let status = status_label();
    form_col.append(&status);

    let (cancel_btn, apply_btn, actions) = action_buttons(&gettext("Aumentar"));
    form_col.append(&actions);

    content_h.append(&form_col);

    // Preview column shows an upscaled thumbnail of the first file as a
    // sanity check — PreviewOp::Resize respects the Lanczos3 filter.
    if let Some(sess) = preview_session.as_ref() {
        let (picture, column) = build_preview_column(sess);
        content_h.append(&column);

        let refresh = {
            let session = sess.clone();
            let picture = picture.clone();
            let factor_row = factor_row.clone();
            move || {
                let f = UPSCALE_FACTORS[factor_row.selected() as usize].1;
                refresh_preview(
                    &session,
                    &picture,
                    &PreviewOp::Resize {
                        mode: ResizeMode::Percent(f32::from(f) * 100.0),
                        filter: Filter::Lanczos3,
                    },
                );
            }
        };
        refresh();
        dialog_signal!(window, factor_row, connect_selected_notify, move |_| refresh());
    }

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&content_h));
    window.set_content(Some(&toolbar_view));

    close_on_click(&window, &cancel_btn);

    {
        let files = files.clone();
        let factor_row = factor_row.clone();
        let target_row = target_row.clone();
        let overwrite_row = overwrite_row.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let factor = UPSCALE_FACTORS[factor_row.selected() as usize].1;
            let target = selected_target_format(&target_row);
            let policy = selected_overwrite(&overwrite_row);

            apply_btn.set_sensitive(false);
            cancel_btn.set_sensitive(false);
            status.set_text(&gettext("Aumentando…"));

            // Upscale = Lanczos3 resize at factor * 100 %.
            let work_files: Vec<PathBuf> = (*files).clone();
            let mode = ResizeMode::Percent(f32::from(factor) * 100.0);
            let opts = EncodeOptions::default();
            run_batch_async(
                work_files,
                move |f| resize_file(f, mode, Filter::Lanczos3, target, &opts, policy),
                gettext("Aumentando"),
                status.clone(),
                apply_btn.clone(),
                cancel_btn.clone(),
                window.clone(),
            );
        });
    }

    window
}
