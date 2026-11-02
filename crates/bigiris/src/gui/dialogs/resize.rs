// SPDX-License-Identifier: GPL-3.0-or-later
//! Redimensionar: scale by edge, percent or exact size.

use super::*;

/// Interpolation filters for the resize dialog.
const RESIZE_FILTERS: &[(&str, Filter)] = &[
    (n_("Lanczos3 (padrão)"), Filter::Lanczos3),
    (n_("Mitchell"), Filter::Mitchell),
    (n_("CatmullRom"), Filter::CatmullRom),
    (n_("Bilinear"), Filter::Bilinear),
    (n_("Nearest (sem anti-aliasing)"), Filter::Nearest),
];

/// Run the "Redimensionar" modal.
pub fn run_resize_dialog(files: Vec<PathBuf>) -> i32 {
    let files = Rc::new(files);
    run_dialog_app(move |app| build_resize_dialog(app, files.clone()).present())
}

pub(in crate::gui) fn build_resize_dialog(
    app: &adw::Application,
    files: Rc<Vec<PathBuf>>,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(900)
        .default_height(640)
        .title(gettext("Redimensionar imagens — Prisma"))
        .build();
    prepare_dialog(&window, files, fill_resize_dialog);
    window
}

fn fill_resize_dialog(
    window: adw::ApplicationWindow,
    files: Rc<Vec<PathBuf>>,
    preview_session: Option<Rc<PreviewSession>>,
) -> adw::ApplicationWindow {
    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new(&gettext("Redimensionar"), "Prisma");
    header.set_title_widget(Some(&title));

    // ---- Mode selector ------------------------------------------------------

    let mode_names = [
        n_("Aresta máxima"),
        n_("Escala (%)"),
        n_("Dimensão exata (WxH)"),
        n_("Encaixar em (WxH)"),
    ];
    let mode_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Modo"))
        .model(&translated_strings(&mode_names))
        .build();

    // Per-mode parameter pages live inside a Stack; switching the combo swaps
    // the page. Each page owns its own SpinRows so we can read values back.
    let max_edge_spin = adw::SpinRow::with_range(1.0, 16384.0, 1.0);
    max_edge_spin.set_title(&gettext("Aresta máxima"));
    max_edge_spin.set_subtitle(&gettext("pixels (lado maior)"));
    max_edge_spin.set_value(1920.0);

    let percent_spin = adw::SpinRow::with_range(1.0, 1000.0, 1.0);
    percent_spin.set_title(&gettext("Porcentagem"));
    percent_spin.set_subtitle(&gettext("100 = tamanho original"));
    percent_spin.set_value(50.0);

    let exact_w = adw::SpinRow::with_range(1.0, 16384.0, 1.0);
    exact_w.set_title(&gettext("Largura"));
    exact_w.set_value(1920.0);
    let exact_h = adw::SpinRow::with_range(1.0, 16384.0, 1.0);
    exact_h.set_title(&gettext("Altura"));
    exact_h.set_value(1080.0);

    let fit_w = adw::SpinRow::with_range(1.0, 16384.0, 1.0);
    fit_w.set_title(&gettext("Largura máxima"));
    fit_w.set_value(1920.0);
    let fit_h = adw::SpinRow::with_range(1.0, 16384.0, 1.0);
    fit_h.set_title(&gettext("Altura máxima"));
    fit_h.set_value(1080.0);

    let page_max_edge = adw::PreferencesGroup::new();
    page_max_edge.add(&max_edge_spin);
    let page_percent = adw::PreferencesGroup::new();
    page_percent.add(&percent_spin);
    let page_exact = adw::PreferencesGroup::new();
    page_exact.add(&exact_w);
    page_exact.add(&exact_h);
    let page_fit = adw::PreferencesGroup::new();
    page_fit.add(&fit_w);
    page_fit.add(&fit_h);

    let stack = gtk::Stack::new();
    stack.add_named(&page_max_edge, Some("max-edge"));
    stack.add_named(&page_percent, Some("percent"));
    stack.add_named(&page_exact, Some("exact"));
    stack.add_named(&page_fit, Some("fit"));
    stack.set_visible_child_name("max-edge");

    {
        let stack = stack.clone();
        dialog_signal!(window, mode_row, connect_selected_notify, move |combo| {
            let name = match combo.selected() {
                0 => "max-edge",
                1 => "percent",
                2 => "exact",
                _ => "fit",
            };
            stack.set_visible_child_name(name);
        });
    }

    // ---- Filter + format + overwrite ---------------------------------------

    let filter_names: Vec<&str> = RESIZE_FILTERS.iter().map(|(n, _)| *n).collect();
    let filter_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Kernel de interpolação"))
        .model(&translated_strings(&filter_names))
        .build();

    let target_row = target_format_row();

    let overwrite_row = overwrite_row();

    let files_row = selection_row(&gettext("Arquivos selecionados"), &files);

    let group_files = adw::PreferencesGroup::new();
    group_files.add(&files_row);

    let group_mode =
        adw::PreferencesGroup::builder().title(gettext("Modo de redimensionamento")).build();
    group_mode.add(&mode_row);

    let group_options = adw::PreferencesGroup::builder().title(gettext("Opções")).build();
    group_options.add(&filter_row);
    group_options.add(&target_row);
    group_options.add(&overwrite_row);

    let status = status_label();

    let (cancel_btn, apply_btn, actions) = action_buttons(&gettext("Redimensionar"));

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    content.append(&group_files);
    content.append(&group_mode);
    content.append(&stack);
    content.append(&group_options);
    content.append(&status);
    content.append(&actions);

    // Scroll the whole content in case the resize dialog overflows a small
    // screen — it's taller than the convert dialog.
    let scrolled = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&content)
        .build();
    scrolled.set_size_request(420, -1);

    let main_box =
        gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(16).build();
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

    // Live preview: any change in mode / filter / dimensions re-renders.
    if let (Some(picture), Some(session)) = (preview_picture.as_ref(), preview_session.as_ref()) {
        let debounce = PreviewDebounce::new();
        let mode_row_clone = mode_row.clone();
        let filter_row_clone = filter_row.clone();
        let max_edge_clone = max_edge_spin.clone();
        let percent_clone = percent_spin.clone();
        let exact_w_clone = exact_w.clone();
        let exact_h_clone = exact_h.clone();
        let fit_w_clone = fit_w.clone();
        let fit_h_clone = fit_h.clone();
        let schedule = {
            let debounce = debounce.clone();
            let session = session.clone();
            let picture = picture.clone();
            let mode_row = mode_row_clone.clone();
            let filter_row = filter_row_clone.clone();
            let max_edge_spin = max_edge_clone.clone();
            let percent_spin = percent_clone.clone();
            let exact_w = exact_w_clone.clone();
            let exact_h = exact_h_clone.clone();
            let fit_w = fit_w_clone.clone();
            let fit_h = fit_h_clone.clone();
            Rc::new(move || {
                let mode = match mode_row.selected() {
                    0 => ResizeMode::MaxEdge(max_edge_spin.value() as u32),
                    1 => ResizeMode::Percent(percent_spin.value() as f32),
                    2 => ResizeMode::Exact {
                        width: exact_w.value() as u32,
                        height: exact_h.value() as u32,
                    },
                    _ => ResizeMode::Fit {
                        width: fit_w.value() as u32,
                        height: fit_h.value() as u32,
                    },
                };
                let filter = RESIZE_FILTERS[filter_row.selected() as usize].1;
                let session = session.clone();
                let picture = picture.clone();
                debounce.trigger(Duration::from_millis(120), move || {
                    refresh_preview(&session, &picture, &PreviewOp::Resize { mode, filter });
                });
            })
        };

        // Mode / filter combo changes
        {
            let s = schedule.clone();
            dialog_signal!(window, mode_row_clone, connect_selected_notify, move |_| s());
        }
        {
            let s = schedule.clone();
            dialog_signal!(window, filter_row_clone, connect_selected_notify, move |_| s());
        }
        // Spin changes in each mode page
        for row in
            &[max_edge_clone, percent_clone, exact_w_clone, exact_h_clone, fit_w_clone, fit_h_clone]
        {
            let s = schedule.clone();
            dialog_signal!(window, row, connect_changed, move |_| s());
        }
    }

    close_on_click(&window, &cancel_btn);

    {
        let files = files.clone();
        let mode_row = mode_row.clone();
        let filter_row = filter_row.clone();
        let target_row = target_row.clone();
        let overwrite_row = overwrite_row.clone();
        let max_edge_spin = max_edge_spin.clone();
        let percent_spin = percent_spin.clone();
        let exact_w = exact_w.clone();
        let exact_h = exact_h.clone();
        let fit_w = fit_w.clone();
        let fit_h = fit_h.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let mode = match mode_row.selected() {
                0 => ResizeMode::MaxEdge(max_edge_spin.value() as u32),
                1 => ResizeMode::Percent(percent_spin.value() as f32),
                2 => ResizeMode::Exact {
                    width: exact_w.value() as u32,
                    height: exact_h.value() as u32,
                },
                _ => ResizeMode::Fit { width: fit_w.value() as u32, height: fit_h.value() as u32 },
            };
            let filter = RESIZE_FILTERS[filter_row.selected() as usize].1;
            let target = selected_target_format(&target_row);
            let policy = selected_overwrite(&overwrite_row);
            // Resize dialog doesn't expose quality today — uses defaults.
            // Users who need quality control go through Convert ▸ Personalizar…
            // or chain via CLI.
            let opts = EncodeOptions::default();

            apply_btn.set_sensitive(false);
            cancel_btn.set_sensitive(false);
            status.set_text(&gettext("Redimensionando…"));

            let work_files: Vec<PathBuf> = (*files).clone();
            run_batch_async(
                work_files,
                move |f| resize_file(f, mode, filter, target, &opts, policy),
                gettext("Redimensionando"),
                status.clone(),
                apply_btn.clone(),
                cancel_btn.clone(),
                window.clone(),
            );
        });
    }

    window
}

#[cfg(test)]
mod apply_tests {
    use super::*;

    fn find_apply(widget: &gtk::Widget, label: &str) -> Option<gtk::Button> {
        if let Some(button) = widget.downcast_ref::<gtk::Button>() {
            if button.has_css_class("suggested-action") && button.label().as_deref() == Some(label)
            {
                return Some(button.clone());
            }
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            if let Some(found) = find_apply(&current, label) {
                return Some(found);
            }
            child = current.next_sibling();
        }
        None
    }

    /// Apply must still publish its output once the dialog is on screen:
    /// after the first frame Mesa/GTK hold their own descriptors (DRM node,
    /// Wayland socket, eventfds) that anything spawned from Apply inherits
    /// unless the spawn path closes them.
    ///
    /// Decoding goes through the real sandboxed worker (`target/<profile>/
    /// bigiris`, so run with `--all-targets` or after building the binary),
    /// which is what catches descriptors leaking from GTK/Mesa into Apply.
    #[test]
    #[ignore = "requires a graphical display; run alone with GSETTINGS_BACKEND=memory"]
    fn resize_apply_after_first_frame_writes_output() {
        assert_eq!(
            std::env::var("GSETTINGS_BACKEND").as_deref(),
            Ok("memory"),
            "synthetic GUI tests require GSETTINGS_BACKEND=memory"
        );
        crate::codec::configure_for_tests(None);
        adw::init().expect("GTK display required");
        let app = adw::Application::builder()
            .application_id("com.biglinux.Iris.ResizeApplyProbe")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gio::Cancellable>).unwrap();
        let context = glib::MainContext::default();
        let pump = |what: &str, seconds: u64, done: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
            while !done() {
                while context.pending() {
                    context.iteration(false);
                }
                assert!(std::time::Instant::now() < deadline, "timed out: {what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("photo.png");
        image::RgbImage::from_pixel(320, 200, image::Rgb([30, 120, 200])).save(&source).unwrap();

        let window = build_resize_dialog(&app, Rc::new(vec![source.clone()]));
        window.present();
        let label = gettext("Redimensionar");
        pump("resize form", 30, &|| find_apply(window.upcast_ref(), &label).is_some());
        // Wait for a frame painted with the form, so GTK and the GL driver
        // have opened everything they open on first draw.
        pump("frame clock", 10, &|| window.frame_clock().is_some());
        let clock = window.frame_clock().unwrap();
        let painted = Rc::new(Cell::new(false));
        let handler = {
            let painted = painted.clone();
            clock.connect_after_paint(move |_| painted.set(true))
        };
        window.queue_draw();
        pump("first frame", 10, &|| painted.get());
        clock.disconnect(handler);

        let apply = find_apply(window.upcast_ref(), &label).unwrap();
        assert!(apply.is_sensitive());
        apply.emit_clicked();

        let written = || {
            std::fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.path()).find(|path| {
                *path != source
                    && path.extension().is_some_and(|ext| ext == "png")
                    && std::fs::metadata(path).is_ok_and(|m| m.len() > 0)
            })
        };
        pump("resized output", 60, &|| written().is_some());
        let output = written().unwrap();
        image::open(&output).expect("output is a readable image");
        window.destroy();
    }
}
