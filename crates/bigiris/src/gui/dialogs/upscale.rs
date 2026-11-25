// SPDX-License-Identifier: GPL-3.0-or-later
//! Aumentar resolução: Real-ESRGAN (when `realesrgan-native` is installed)
//! or Lanczos3, by an integer factor.

use super::*;

/// Upscale factors we expose in the dialog — matches the CLI `--factor` range
/// (2..=4), for both engines.
pub(in crate::gui) const UPSCALE_FACTORS: &[(&str, u8)] =
    &[("2× (200%)", 2), ("3× (300%)", 3), ("4× (400%)", 4)];

/// Upscale modal: pick an integer factor and an engine.
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
    let ai_unavailable = model_unavailable(&bigimage_ai::upscale::MODEL);
    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new(
        &gettext("Aumentar resolução"),
        if ai_unavailable.is_none() { "Prisma · Real-ESRGAN" } else { "Prisma · Lanczos3" },
    );
    header.set_title_widget(Some(&title));

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

    // Real-ESRGAN when its package is installed; the row says what to
    // install otherwise and Lanczos3 runs instead.
    let engine_row = adw::SwitchRow::builder()
        .title(gettext("Usar IA (Real-ESRGAN)"))
        .subtitle(ai_unavailable.clone().unwrap_or_else(engine_subtitle))
        .active(ai_unavailable.is_none())
        .sensitive(ai_unavailable.is_none())
        .build();
    {
        let title = title.clone();
        dialog_signal!(window, engine_row, connect_active_notify, move |row| {
            title.set_subtitle(if row.is_active() {
                "Prisma · Real-ESRGAN"
            } else {
                "Prisma · Lanczos3"
            });
        });
    }
    // Notified by a run once Real-ESRGAN has loaded; closed with the window.
    let loaded = crate::gui::viewer::Wake::default();
    follow_device(&engine_row, &loaded);
    {
        let loaded = loaded.clone();
        window.connect_unrealize(move |_| loaded.close());
    }

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
    group.add(&engine_row);
    group.add(&files_row);
    group.add(&natural_row);
    group.add(&factor_row);
    group.add(&predicted_row);
    group.add(&target_row);
    group.add(&overwrite_row);

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
        let engine_row = engine_row.clone();
        let target_row = target_row.clone();
        let overwrite_row = overwrite_row.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let window = window.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let factor = UPSCALE_FACTORS[factor_row.selected() as usize].1;
            let target = selected_target_format(&target_row);
            let policy = selected_overwrite(&overwrite_row);

            apply_btn.set_sensitive(false);
            status.set_text(&gettext("Aumentando…"));

            let work_files: Vec<PathBuf> = (*files).clone();
            let mode = ResizeMode::Percent(f32::from(factor) * 100.0);
            let opts = EncodeOptions::default();
            let ai = engine_row.is_active() && engine_row.is_sensitive();
            // Same `<stem>_<pct>pct` name either way.
            let suffix = format!("_{}pct", u32::from(factor) * 100);
            // One file at a time through the network: it already uses
            // every core, and parallel files would only add their buffers.
            let one_at_a_time = Arc::new(Mutex::new(()));
            let loaded = loaded.clone();
            run_batch_async(
                work_files,
                move |f| {
                    if !ai {
                        return resize_file(f, mode, Filter::Lanczos3, target, &opts, policy);
                    }
                    let _turn =
                        one_at_a_time.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    bigimage_core::transform_file(f, &suffix, target, &opts, policy, |img| {
                        bigimage_ai::upscale::upscale(
                            &img,
                            factor,
                            bigimage_ai::upscale::Method::RealEsrgan,
                            |stage| {
                                if matches!(stage, bigimage_ai::Stage::Running(_)) {
                                    loaded.notify();
                                }
                                if crate::codec::cancelled() {
                                    std::ops::ControlFlow::Break(())
                                } else {
                                    std::ops::ControlFlow::Continue(())
                                }
                            },
                        )
                        .map_err(|e| bigimage_core::BigImageError::Other(ai_error_text(&e)))
                    })
                },
                gettext("Aumentando"),
                status.clone(),
                apply_btn.clone(),
                window.clone(),
            );
        });
    }

    window
}

/// Engine row subtitle: where Real-ESRGAN runs once it has loaded.
fn engine_subtitle() -> String {
    match bigimage_ai::upscale::device() {
        Some(device) => fill(&gettext("Roda em: {device}"), &[("device", &device.to_string())]),
        None => gettext("Detalhes mais nítidos que Lanczos3; roda no seu computador"),
    }
}

/// Show the device in `row` once the model has loaded in this process.
fn follow_device(row: &adw::SwitchRow, loaded: &crate::gui::viewer::Wake) {
    // Unavailable: the subtitle keeps the install hint.
    if !row.is_sensitive() || bigimage_ai::upscale::device().is_some() {
        return;
    }
    let weak = row.downgrade();
    let loaded = loaded.clone();
    glib::MainContext::default().spawn_local(async move {
        while loaded.wait().await {
            if bigimage_ai::upscale::device().is_some() {
                if let Some(row) = weak.upgrade() {
                    row.set_subtitle(&engine_subtitle());
                }
                return;
            }
        }
    });
}
