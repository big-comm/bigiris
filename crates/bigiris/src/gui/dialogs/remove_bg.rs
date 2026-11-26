// SPDX-License-Identifier: GPL-3.0-or-later
//! Remove background: local BiRefNet background removal.

use super::*;

/// Run one remove-bg pass over `src`: decode → BiRefNet → save PNG RGBA.
/// The model stays loaded across calls (see [`bigimage_ai::background`]),
/// so a 50-file batch loads BiRefNet once. `cancel` stops the network.
fn remove_bg_one_with_session(
    session: &mut bigimage_ai::background::BgSession,
    src: &Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<PathBuf, String> {
    bigimage_core::content::ensure_static(src).map_err(|error| core_error_text(&error))?;
    let img = bigimage_core::open_image(src).map_err(|e| {
        fill(&gettext("Could not read the image: {error}"), &[("error", &image_error_text(&e))])
    })?;
    let out = session
        .process_with_progress(&img, bigimage_ai::cancel_on(cancel))
        .map_err(|e| ai_error_text(&e))?;
    // Increment, never Replace: the dialog promises not to overwrite, and
    // `photo.jpg` + `photo.png` in one batch both map to `photo_nobg.png`.
    let outcome = bigimage_core::save_image(
        out,
        crate::output_plan::remove_bg_output(src),
        Format::Png,
        &EncodeOptions::default(),
        OverwritePolicy::Increment,
    )
    .map_err(|e| {
        fill(&gettext("Could not write the result: {error}"), &[("error", &core_error_text(&e))])
    })?;
    Ok(outcome.path().to_path_buf())
}

/// Worker-thread state for the remove-bg dialog. Shared via `Arc<Mutex>`;
/// the worker raises a [`viewer::Wake`] after each change and a task on
/// the GTK loop reflects the latest snapshot into the ProgressBar + label.
#[derive(Clone, Debug)]
enum BgUiState {
    /// Worker hasn't started yet (apply not clicked).
    Idle,
    /// Loading BiRefNet (the first GPU load also builds its shaders).
    Loading,
    /// Running inference on file `idx` (0-based) of `total`.
    Processing { idx: usize, total: usize, file: String },
    /// Worker finished. `outputs` pairs the index of each input processed
    /// in this run with its written path (None when it failed).
    Done { outputs: Vec<(usize, Option<PathBuf>)>, first_err: Option<String> },
}

pub fn run_remove_bg_dialog(files: Vec<PathBuf>) -> i32 {
    let files = Rc::new(files);
    run_dialog_app(move |app| build_remove_bg_dialog(app, files.clone()).present())
}

pub(in crate::gui) fn build_remove_bg_dialog(
    app: &adw::Application,
    files: Rc<Vec<PathBuf>>,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(520)
        .title(gettext("Remove background — Prisma"))
        .build();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        &gettext("Remove background"),
        &gettext("Prisma · BiRefNet-lite"),
    )));
    let unavailable = model_unavailable(&bigimage_ai::background::MODEL);

    let content = padded_box(gtk::Orientation::Vertical, 12);

    // Empty input — fail closed with clear message.
    if files.is_empty() {
        show_selection_problem(
            &window,
            &header,
            &content,
            &gettext("No image selected"),
            &gettext("Select one or more files when opening the dialog."),
        );
        return window;
    }

    // No engines in this build, or the weights package is missing: say
    // which package to install before the user clicks.
    if let Some(message) = &unavailable {
        let banner = gtk::Label::builder().label(message).wrap(true).xalign(0.0).build();
        banner.add_css_class("warning");
        content.append(&banner);
    }

    let files_row = selection_row(&gettext("Images"), &files);
    let files_icon = gtk::Image::from_icon_name("image-x-generic-symbolic");
    files_row.add_prefix(&files_icon);

    let model_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Model"))
        .subtitle(model_subtitle())
        .build();
    let model_icon = gtk::Image::from_icon_name("application-x-executable-symbolic");
    model_row.add_prefix(&model_icon);

    let output_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Output"))
        .subtitle(gettext(
            "“source”_nobg.png · PNG with alpha, never overwrites (_1, _2… if it already exists)",
        ))
        .build();
    let output_icon = gtk::Image::from_icon_name("document-save-symbolic");
    output_row.add_prefix(&output_icon);

    let group = adw::PreferencesGroup::builder()
        .title(gettext("Remove background with local AI"))
        .description(gettext("The model runs entirely on your computer. No image leaves your disk — no upload, no account, no external API."))
        .build();
    group.add(&files_row);
    group.add(&model_row);
    group.add(&output_row);
    content.append(&group);

    // Hidden until Remove background is clicked. Shows download bytes on
    // the first run, then per-file inference progress.
    let progress_bar =
        gtk::ProgressBar::builder().show_text(true).visible(false).margin_top(6).build();
    let status = status_label();
    let (cancel_btn, apply_btn, actions_box) = action_buttons(&gettext("Remove background"));
    apply_btn.set_sensitive(unavailable.is_none());
    let footer = [progress_bar.upcast_ref(), status.upcast_ref(), actions_box.upcast_ref()];
    set_dialog_content(&window, &header, &form_column(&content, &footer));

    // Shared by the UI, the worker and the close handler: closing the window
    // (X, Esc, Cancel) stops the remaining files and keeps the Compare window
    // that follows a success from opening.
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    {
        let cancelled = cancelled.clone();
        window.connect_close_request(move |_| {
            cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            glib::Propagation::Proceed
        });
    }
    close_on_click(&window, &cancel_btn);

    // Result of every input across runs: a retry only processes the ones
    // still missing, so files that already succeeded are not written again
    // as `_nobg_1.png`.
    let results: Rc<RefCell<Vec<Option<PathBuf>>>> = Rc::new(RefCell::new(vec![None; files.len()]));

    {
        let files_rc = files.clone();
        let status = status.clone();
        let progress_bar = progress_bar.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        let cancelled = cancelled.clone();
        let model_row = model_row.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            if let Some(message) = model_unavailable(&bigimage_ai::background::MODEL) {
                status.set_text(&message);
                return;
            }
            let pending: Vec<(usize, PathBuf)> = files_rc
                .iter()
                .enumerate()
                .filter(|(idx, _)| results.borrow().get(*idx).is_some_and(Option::is_none))
                .map(|(idx, path)| (idx, path.clone()))
                .collect();
            if pending.is_empty() {
                return;
            }
            batch_runner::disarm_auto_close(&window);
            apply_btn.set_sensitive(false);
            // Cancel stays available: during a run it aborts and closes.
            cancel_btn.set_label(&gettext("Cancel"));
            progress_bar.remove_css_class("error");
            progress_bar.remove_css_class("success");
            progress_bar.set_visible(true);
            progress_bar.set_fraction(0.0);
            progress_bar.set_text(Some(&gettext("Preparing…")));
            status.set_text("");

            let shared: std::sync::Arc<std::sync::Mutex<BgUiState>> =
                std::sync::Arc::new(std::sync::Mutex::new(BgUiState::Idle));
            let wake = viewer::Wake::default();

            {
                let shared = shared.clone();
                let cancelled_w = cancelled.clone();
                let wake = wake.clone();
                std::thread::spawn(move || {
                    // Closes the wake-up when the worker ends, also by panic:
                    // the UI then reads the final state instead of waiting.
                    struct CloseOnDrop(viewer::Wake);
                    impl Drop for CloseOnDrop {
                        fn drop(&mut self) {
                            self.0.close();
                        }
                    }
                    let _close = CloseOnDrop(wake.clone());
                    let publish = |state: BgUiState| {
                        *shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = state;
                        wake.notify();
                    };
                    // Load the model once for the whole batch; every file
                    // reuses it.
                    let total = pending.len();
                    let session_result = bigimage_ai::background::BgSession::new(|_| {
                        publish(BgUiState::Loading);
                        std::ops::ControlFlow::Continue(())
                    });
                    let mut session = match session_result {
                        Ok(s) => s,
                        Err(e) => {
                            publish(BgUiState::Done {
                                outputs: pending.iter().map(|(idx, _)| (*idx, None)).collect(),
                                first_err: Some(fill(
                                    &gettext("Failed to load the model: {error}"),
                                    &[("error", &ai_error_text(&e))],
                                )),
                            });
                            return;
                        }
                    };

                    let mut outputs = Vec::with_capacity(total);
                    let mut first_err: Option<String> = None;
                    for (position, (idx, path)) in pending.iter().enumerate() {
                        // Bail between files; a running file stops at the
                        // network's next step through the same flag.
                        if cancelled_w.load(std::sync::atomic::Ordering::Relaxed) {
                            outputs.push((*idx, None));
                            continue;
                        }
                        publish(BgUiState::Processing {
                            idx: position,
                            total,
                            file: path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                        });
                        // Scoped cancel lets closing the window stop the
                        // isolated decode instead of waiting for it.
                        let result = crate::codec::with_cancel(cancelled_w.clone(), || {
                            remove_bg_one_with_session(&mut session, path, &cancelled_w)
                        });
                        match result {
                            Ok(out) => outputs.push((*idx, Some(out))),
                            Err(e) => {
                                if first_err.is_none() {
                                    first_err = Some(format!(
                                        "{}: {e}",
                                        crate::codec::printable_path(path)
                                    ));
                                }
                                outputs.push((*idx, None));
                            }
                        }
                    }
                    publish(BgUiState::Done { outputs, first_err });
                });
            }

            // Awaited on the GTK loop: it wakes only when the worker
            // publishes progress. Once the window is closed it leaves the widgets
            // alone but holds the app until the worker ends: quitting mid-write
            // would strand `.part` files.
            let progress_bar = progress_bar.clone();
            let status = status.clone();
            let apply_btn = apply_btn.clone();
            let cancel_btn = cancel_btn.clone();
            let window = window.clone();
            let originals = files_rc.clone();
            let results = results.clone();
            let cancelled_ui = cancelled.clone();
            let model_row = model_row.clone();
            let hold = window.application().map(|app| app.hold());
            glib::MainContext::default().spawn_local(async move {
                let _hold = hold;
                let snapshot =
                    || shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                while wake.wait().await {
                    if !cancelled_ui.load(std::sync::atomic::Ordering::Relaxed) {
                        show_bg_progress(&progress_bar, &snapshot());
                        model_row.set_subtitle(&model_subtitle());
                    }
                }
                if cancelled_ui.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                // The worker died (panic) without its summary: let the user
                // retry or close.
                let BgUiState::Done { outputs, first_err } = snapshot() else {
                    status.set_text(&gettext("Processing ended unexpectedly. Please try again."));
                    apply_btn.set_sensitive(true);
                    cancel_btn.set_label(&gettext("Close"));
                    return;
                };
                {
                    let mut results = results.borrow_mut();
                    for (idx, output) in outputs {
                        if let Some(slot) = results.get_mut(idx) {
                            *slot = output;
                        }
                    }
                }
                progress_bar.set_fraction(1.0);
                let results = results.borrow();
                let ok = results.iter().filter(|o| o.is_some()).count();
                let fail = results.len() - ok;
                if fail == 0 {
                    status.set_text(&format!("{}: {ok}", gettext("Saved images")));
                    progress_bar.set_text(Some(&gettext("Done")));
                    progress_bar.add_css_class("success");
                    if originals.len() == 1 {
                        let compared = match (originals.first(), results.first()) {
                            (Some(orig), Some(Some(out))) => {
                                spawn_compare_viewer(&window, orig, out)
                            }
                            _ => true,
                        };
                        // A single file closes as Compare opens, so two windows
                        // never show at once; it stays open to report a Compare
                        // that failed to start.
                        if compared {
                            window.close();
                        }
                    } else {
                        // A batch stays 2 s so the summary can be read.
                        batch_runner::arm_auto_close(&window, 2);
                    }
                } else {
                    let detail = first_err.unwrap_or_default();
                    status.set_text(&format!(
                        "{}: {ok} · {}: {fail}\n{detail}",
                        gettext("Saved images"),
                        gettext("Failures")
                    ));
                    progress_bar.set_text(Some(&gettext("With errors")));
                    progress_bar.add_css_class("error");
                    apply_btn.set_sensitive(true);
                    cancel_btn.set_label(&gettext("Close"));
                }
            });
        });
    }

    window
}

/// Reflect a running worker's latest state in the progress bar.
fn show_bg_progress(progress_bar: &gtk::ProgressBar, state: &BgUiState) {
    match state {
        BgUiState::Idle | BgUiState::Done { .. } => {}
        BgUiState::Loading => {
            progress_bar.pulse();
            progress_bar.set_text(Some(&gettext("Loading the model…")));
        }
        BgUiState::Processing { idx, total, file } => {
            let frac = (*idx as f64 / (*total).max(1) as f64).clamp(0.0, 1.0);
            progress_bar.set_fraction(frac);
            progress_bar.set_text(Some(&fill(
                &gettext("Processing {current}/{total} — {file}"),
                &[
                    ("current", &(idx + 1).to_string()),
                    ("total", &total.to_string()),
                    ("file", file),
                ],
            )));
        }
    }
}

/// Model row subtitle: license, and where the model runs once loaded.
fn model_subtitle() -> String {
    match bigimage_ai::background::device() {
        Some(device) => fill(
            &gettext("BiRefNet-lite (MIT) · runs offline on {device}"),
            &[("device", &device.to_string())],
        ),
        None => gettext("BiRefNet-lite (MIT) · runs offline on your computer"),
    }
}

/// Launch the Compare dialog in a separate process so both windows can
/// coexist (each `adw::Application` instance takes over the main loop).
/// Called right after a successful single-file remove-bg so the user
/// sees original vs. result side-by-side.
fn spawn_compare_viewer(window: &adw::ApplicationWindow, original: &Path, result: &Path) -> bool {
    // `--` keeps a file named like `-x.png` from being read as a flag.
    super::super::transfers::launch(
        window,
        vec![
            "--dialog=compare".into(),
            "--".into(),
            original.as_os_str().to_owned(),
            result.as_os_str().to_owned(),
        ],
    )
}

#[cfg(test)]
mod availability_tests {
    use super::*;

    fn spin(context: &glib::MainContext, millis: u64) {
        let deadline = std::time::Instant::now() + Duration::from_millis(millis);
        while std::time::Instant::now() < deadline {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Every widget under `root`, depth first.
    fn widgets(root: &gtk::Widget) -> Vec<gtk::Widget> {
        let mut all = vec![root.clone()];
        let mut child = root.first_child();
        while let Some(current) = child {
            all.extend(widgets(&current));
            child = current.next_sibling();
        }
        all
    }

    fn button(window: &adw::ApplicationWindow, label: &str) -> gtk::Button {
        widgets(window.upcast_ref())
            .into_iter()
            .filter_map(|w| w.downcast::<gtk::Button>().ok())
            .find(|b| b.label().as_deref() == Some(label))
            .unwrap_or_else(|| panic!("no button {label}"))
    }

    fn texts(window: &adw::ApplicationWindow) -> String {
        widgets(window.upcast_ref())
            .into_iter()
            .filter_map(|w| w.downcast::<gtk::Label>().ok())
            .map(|l| l.text().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A registered application, its main context and one source image.
    fn setup() -> (adw::Application, glib::MainContext, tempfile::TempDir, Rc<Vec<PathBuf>>) {
        assert_eq!(std::env::var("GSETTINGS_BACKEND").as_deref(), Ok("memory"));
        adw::init().expect("GTK display required");
        let app = adw::Application::builder()
            .application_id("com.biglinux.Iris.AiDialogsProbe")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gio::Cancellable>).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("photo.png");
        image::RgbImage::from_pixel(320, 240, image::Rgb([90, 140, 200])).save(&source).unwrap();
        (app, glib::MainContext::default(), dir, Rc::new(vec![source]))
    }

    fn show(
        app: &adw::Application,
        context: &glib::MainContext,
        files: &Rc<Vec<PathBuf>>,
        name: &str,
        build: fn(&adw::Application, Rc<Vec<PathBuf>>) -> adw::ApplicationWindow,
    ) -> adw::ApplicationWindow {
        let window = build(app, files.clone());
        window.present();
        spin(context, 300);
        crate::gui::lifecycle_tests::capture_window(&window, name);
        window
    }

    /// Without their weights the Prisma remove-bg and upscale dialogs name the
    /// package and refuse to start the model.
    #[test]
    #[ignore = "requires a graphical display; run alone with GSETTINGS_BACKEND=memory"]
    #[expect(unsafe_code, reason = "points the weight overrides at a missing file")]
    fn ai_dialogs_name_the_missing_package() {
        let absent = std::env::temp_dir().join("bigiris-absent.weights");
        for var in [bigimage_ai::background::MODEL.env, bigimage_ai::upscale::MODEL.env] {
            // SAFETY: the first statement of an ignored test run alone, before GTK
            // starts any thread; nothing else reads the environment yet.
            unsafe { std::env::set_var(var, &absent) };
        }
        let (app, context, _dir, files) = setup();
        let window =
            show(&app, &context, &files, "prisma-ai-remove-bg-missing", build_remove_bg_dialog);
        let hint = if bigimage_ai::engines_compiled() {
            "sudo pacman -S birefnet-native"
        } else {
            "--features ai"
        };
        assert!(texts(&window).contains(hint), "no install hint: {}", texts(&window));
        let apply = button(&window, &gettext("Remove background"));
        assert!(!apply.is_sensitive(), "remove-bg can start without its model");
        window.destroy();
        let window =
            show(&app, &context, &files, "prisma-ai-upscale-missing", build_upscale_dialog);
        let row = widgets(window.upcast_ref())
            .into_iter()
            .find_map(|w| w.downcast::<adw::SwitchRow>().ok())
            .expect("engine row");
        assert!(!row.is_active() && !row.is_sensitive());
        if bigimage_ai::engines_compiled() {
            assert!(row.subtitle().unwrap_or_default().contains("realesrgan-native"));
        }
        // Lanczos3 still runs.
        assert!(button(&window, &gettext("Upscale")).is_sensitive());
        window.destroy();
        spin(&context, 100);
    }

    /// With the weights (the packages, or BIGIRIS_BIREFNET_WEIGHTS and
    /// BIGIRIS_REALESRGAN_WEIGHTS) both dialogs are ready.
    #[test]
    #[ignore = "requires a graphical display; run alone with GSETTINGS_BACKEND=memory"]
    fn ai_dialogs_are_ready_with_weights() {
        let (app, context, _dir, files) = setup();
        if bigimage_ai::background::MODEL.availability().is_ready() {
            let window =
                show(&app, &context, &files, "prisma-ai-remove-bg", build_remove_bg_dialog);
            assert!(button(&window, &gettext("Remove background")).is_sensitive());
            assert!(!texts(&window).contains("sudo pacman"));
            window.destroy();
        } else {
            eprintln!("skipping the ready remove-bg dialog: weights absent");
        }
        if bigimage_ai::upscale::MODEL.availability().is_ready() {
            let window = show(&app, &context, &files, "prisma-ai-upscale", build_upscale_dialog);
            let row = widgets(window.upcast_ref())
                .into_iter()
                .find_map(|w| w.downcast::<adw::SwitchRow>().ok())
                .expect("engine row");
            assert!(row.is_active() && row.is_sensitive());
            window.destroy();
        } else {
            eprintln!("skipping the ready upscale dialog: weights absent");
        }
        spin(&context, 100);
    }
}
