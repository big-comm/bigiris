// SPDX-License-Identifier: GPL-3.0-or-later
//! Batch: batch conversion with progress, cancel and auto-close.

use super::*;

/// Prisma Batch: converts the files and folders given on the command line
/// on the shared parallel pool. Folders stay folders until Apply, which
/// lists their images as the CLI does ([`crate::output_plan::batch_files`]).
pub fn run_batch_dialog(files: Vec<PathBuf>) -> i32 {
    let mut inputs: Vec<PathBuf> = files
        .into_iter()
        .filter(|input| input.is_dir() || (input.is_file() && is_viewable(input)))
        .collect();
    inputs.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    inputs.dedup();
    let inputs = Rc::new(RefCell::new(inputs));
    run_dialog_app(move |app| build_batch_dialog(app, inputs.clone()).present())
}

pub(in crate::gui) fn build_batch_dialog(
    app: &adw::Application,
    files: Rc<RefCell<Vec<PathBuf>>>,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(640)
        .default_height(640)
        .title(gettext("Prisma — Batch"))
        .build();

    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new(&gettext("Batch conversion"), "Prisma");
    header.set_title_widget(Some(&title));

    // Files and folders can be added after the dialog opens; every change
    // to the list goes through `refresh_files_row`.
    let files_group = adw::PreferencesGroup::builder().title(gettext("Files")).build();
    let summary_row = adw::ActionRow::builder().use_markup(false).activatable(false).build();
    let add_files_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text(gettext("Add files"))
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    let add_folder_btn = gtk::Button::builder()
        .icon_name("folder-symbolic")
        .tooltip_text(gettext("Add folder (images)"))
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    summary_row.add_suffix(&add_files_btn);
    summary_row.add_suffix(&add_folder_btn);
    files_group.add(&summary_row);
    refresh_files_row(&summary_row, &files.borrow());

    let convert_group = adw::PreferencesGroup::builder().title(gettext("Conversion")).build();

    let format_row = format_row();
    convert_group.add(&format_row);

    let encode = EncodeControls::new();
    convert_group.add(&encode.target);
    convert_group.add(&encode.advanced);
    encode.bind(&window, &format_row);

    let dest_group = adw::PreferencesGroup::builder().title(gettext("Destination")).build();
    let overwrite_row = overwrite_row();
    dest_group.add(&overwrite_row);
    let (folder_row, output_dir) = output_folder_row(&window);
    dest_group.add(&folder_row);

    let progress = gtk::ProgressBar::builder().show_text(true).text(gettext("Ready")).build();
    progress.set_margin_start(18);
    progress.set_margin_end(18);
    progress.set_margin_top(8);

    let status = status_label();
    status.set_label(&gettext("Ready. Click Process to start."));
    status.set_margin_start(18);
    status.set_margin_end(18);
    status.set_margin_top(4);
    status.set_margin_bottom(4);

    let apply_btn = gtk::Button::builder()
        .label(gettext("Process"))
        .css_classes(["pill", "suggested-action"])
        .build();
    let cancel_btn = gtk::Button::builder().label(gettext("Cancel")).build();
    let btn_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    btn_bar.set_halign(gtk::Align::Center);
    btn_bar.set_margin_top(8);
    btn_bar.set_margin_bottom(18);
    btn_bar.append(&cancel_btn);
    btn_bar.append(&apply_btn);

    let options_col = gtk::Box::new(gtk::Orientation::Vertical, 14);
    options_col.set_margin_start(18);
    options_col.set_margin_end(18);
    options_col.set_margin_top(18);
    options_col.append(&files_group);
    options_col.append(&convert_group);
    options_col.append(&dest_group);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&options_col)
        .build();

    let main_col = gtk::Box::new(gtk::Orientation::Vertical, 0);
    main_col.append(&scroller);
    main_col.append(&progress);
    main_col.append(&status);
    main_col.append(&btn_bar);

    set_dialog_content(&window, &header, &main_col);

    // Shared with the worker thread: set by Cancel during a run, reset on
    // each Process.
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // True while a worker is running. The Cancel button uses this to
    // decide between "cancel the run" and "close the window".
    let running = Rc::new(Cell::new(false));
    // Closing the window stops the remaining files and any in-flight
    // isolated codec run; the app stays alive until the workers report.
    {
        let cancelled = cancelled.clone();
        window.connect_close_request(move |_| {
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            glib::Propagation::Proceed
        });
    }

    {
        let files = files.clone();
        let summary_row = summary_row.clone();
        let window = window.clone();
        dialog_signal!(window, add_files_btn, connect_clicked, move |_| {
            add_files_to_batch(&window, &files, &summary_row);
        });
    }
    {
        let files = files.clone();
        let summary_row = summary_row.clone();
        let window = window.clone();
        dialog_signal!(window, add_folder_btn, connect_clicked, move |_| {
            add_folder_to_batch(&window, &files, &summary_row);
        });
    }

    {
        let cancelled = cancelled.clone();
        let running = running.clone();
        let window = window.clone();
        dialog_signal!(window, cancel_btn, connect_clicked, move |_| {
            if running.get() {
                cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            } else {
                window.close();
            }
        });
    }

    {
        let files = files.clone();
        let format_row = format_row.clone();
        let overwrite_row = overwrite_row.clone();
        let encode = Rc::new(encode);
        let progress = progress.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        let cancelled = cancelled.clone();
        let running = running.clone();
        let output_dir = output_dir.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let fmt = selected_format(&format_row);
            let policy = selected_overwrite(&overwrite_row);
            let opts = encode.options();
            let out_dir_snapshot = output_dir.borrow().clone();
            let scanned =
                crate::output_plan::batch_files(&files.borrow(), fmt, out_dir_snapshot.as_deref());
            let crate::output_plan::BatchFiles { inputs, earlier_outputs } = match scanned {
                Ok(scanned) => scanned,
                Err(message) => {
                    status.set_text(&message);
                    return;
                }
            };
            if inputs.is_empty() && earlier_outputs.is_empty() {
                status.set_text(&gettext("Add at least one image to process."));
                return;
            }

            let naming = crate::output_plan::Naming {
                output_dir: out_dir_snapshot.clone(),
                extension: Some(fmt.extension()),
                policy,
                per_image_suffix: false,
            };
            let controls = RunControls {
                window: window.clone(),
                progress: progress.clone(),
                status: status.clone(),
                apply_btn: apply_btn.clone(),
                cancel_btn: cancel_btn.clone(),
                running: running.clone(),
            };
            let start = {
                let (cancelled, inputs, naming) =
                    (cancelled.clone(), inputs.clone(), naming.clone());
                move || {
                    // Reset cancel for a re-run, mark a worker as live. A pending
                    // auto-close from the previous run must not end this one.
                    batch_runner::disarm_auto_close(&controls.window);
                    cancelled.store(false, std::sync::atomic::Ordering::Relaxed);
                    controls.running.set(true);
                    controls.apply_btn.set_sensitive(false);
                    controls.cancel_btn.set_label(&gettext("Cancel"));
                    controls.progress.set_fraction(0.0);
                    controls.progress.set_text(Some(&gettext("Starting…")));
                    controls.status.set_text(&gettext("Processing…"));
                    let events = batch_runner::spawn_batch_workers(
                        inputs,
                        naming,
                        move |path| {
                            convert_file_to(path, out_dir_snapshot.as_deref(), fmt, &opts, policy)
                        },
                        cancelled,
                    );
                    // Released with this task, i.e. only after the pool reported
                    // back: closing the last window never kills a worker mid-write
                    // and strands `.part` files.
                    let hold = controls.window.application().map(|app| app.hold());
                    glib::MainContext::default().spawn_local(async move {
                        let _hold = hold;
                        follow_run(events, earlier_outputs, controls).await;
                    });
                }
            };
            confirm_replacing_originals(&window, &inputs, &naming, start);
        });
    }

    window
}

/// The controls of the batch window one run updates.
struct RunControls {
    window: adw::ApplicationWindow,
    progress: gtk::ProgressBar,
    status: gtk::Label,
    apply_btn: gtk::Button,
    cancel_btn: gtk::Button,
    /// True while a run is in flight: Cancel then stops it instead of
    /// closing the window.
    running: Rc<Cell<bool>>,
}

/// Show the progress and the summary of one run; `earlier_outputs` are the
/// folder entries left out of it.
async fn follow_run(
    events: batch_runner::BatchEvents,
    earlier_outputs: Vec<(PathBuf, PathBuf)>,
    controls: RunControls,
) {
    let RunControls { window, progress, status, apply_btn, cancel_btn, running } = controls;
    loop {
        match events.next().await {
            Some(batch_runner::AsyncBatchEvent::Progress { done, total, file }) => {
                progress.set_fraction(done as f64 / total.max(1) as f64);
                progress.set_text(Some(&format!("{done}/{total} · {file}")));
            }
            Some(batch_runner::AsyncBatchEvent::Refused(message)) => {
                running.set(false);
                progress.set_text(None);
                status.set_text(&message);
                apply_btn.set_sensitive(true);
                // Nothing runs, so the button closes the window.
                cancel_btn.set_label(&gettext("Close"));
                return;
            }
            Some(batch_runner::AsyncBatchEvent::Done(summary)) => {
                running.set(false);
                // Closed mid-run: nothing left to show or to close.
                if summary.cancelled && !window.is_visible() {
                    return;
                }
                status.set_text(&run_summary(&summary, &earlier_outputs));
                progress.set_text(Some(&gettext(if summary.cancelled {
                    n_("Canceled")
                } else {
                    n_("Done")
                })));
                apply_btn.set_sensitive(true);
                let finished = !summary.cancelled && summary.done == summary.total;
                cancel_btn.set_label(&gettext(if finished { n_("Close") } else { n_("Cancel") }));
                // Only a run that wrote every input closes by itself: skips
                // and failures stay on screen to be read.
                let skipped = summary.skip + earlier_outputs.len();
                if !summary.cancelled && summary.ok > 0 && skipped == 0 && summary.fail == 0 {
                    batch_runner::arm_auto_close(&window, 3);
                }
                return;
            }
            None => {
                running.set(false);
                progress.set_text(None);
                batch_runner::abort_dialog(&status, &apply_btn);
                return;
            }
        }
    }
}

/// The summary line of a finished run: counts and time, the folder entries
/// left out as earlier outputs, the size change and the first failure.
fn run_summary(
    summary: &batch_runner::RunSummary,
    earlier_outputs: &[(PathBuf, PathBuf)],
) -> String {
    let batch_runner::RunSummary {
        ok,
        skip,
        fail,
        ref first_err,
        done,
        total,
        cancelled,
        elapsed,
        bytes_in,
        bytes_out,
        ..
    } = *summary;
    let timing = batch_runner::format_duration(elapsed);
    let template = if cancelled {
        gettext(
            "Canceled after {done}/{total} in {timing}. Written: {ok}, skipped: {skip}, failed: {fail}.",
        )
    } else {
        gettext("Finished in {timing}. Written: {ok}, skipped: {skip}, failed: {fail}.")
    };
    let mut msg = fill(
        &template,
        &[
            ("done", &done.to_string()),
            ("total", &total.to_string()),
            ("timing", &timing),
            ("ok", &ok.to_string()),
            ("skip", &(skip + earlier_outputs.len()).to_string()),
            ("fail", &fail.to_string()),
        ],
    );
    if !earlier_outputs.is_empty() {
        let names: Vec<_> =
            earlier_outputs.iter().map(|(entry, _)| crate::codec::printable_path(entry)).collect();
        msg.push('\n');
        msg.push_str(&fill(
            &gettext("Skipped as outputs of earlier runs: {files}"),
            &[("files", &names.join(", "))],
        ));
    }
    if ok > 0 && bytes_in > 0 && bytes_out > 0 {
        let delta = batch_runner::format_size_delta(bytes_in, bytes_out);
        let avg_in = bytes_in / (ok as u64).max(1);
        let avg_out = bytes_out / (ok as u64).max(1);
        msg.push('\n');
        msg.push_str(&fill(
            &gettext("{before} → {after} ({delta}) · average per file {avg_before} → {avg_after}"),
            &[
                ("before", &glib::format_size(bytes_in)),
                ("after", &glib::format_size(bytes_out)),
                ("delta", &delta),
                ("avg_before", &glib::format_size(avg_in)),
                ("avg_after", &glib::format_size(avg_out)),
            ],
        ));
    }
    if let Some(err) = first_err {
        msg.push('\n');
        msg.push_str(err);
    }
    msg
}

/// `inputs` are files and folders; a folder counts as its images.
fn refresh_files_row(row: &adw::ActionRow, inputs: &[PathBuf]) {
    let files: Vec<PathBuf> = inputs
        .iter()
        .flat_map(|input| {
            if input.is_dir() {
                crate::output_plan::folder_images(input).unwrap_or_default()
            } else {
                vec![input.clone()]
            }
        })
        .collect();
    let total: u64 = files.iter().filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len())).sum();
    row.set_title(&format!("{}: {}", gettext("Selected files"), files.len()));
    row.set_subtitle(&glib::format_size(total));
}

fn add_files_to_batch(
    window: &adw::ApplicationWindow,
    files: &Rc<RefCell<Vec<PathBuf>>>,
    row: &adw::ActionRow,
) {
    let dialog = crate::gui::image_file_dialog(&gettext("Add files"));

    let parent = window.clone();
    let files = files.clone();
    let row = row.clone();
    dialog.open_multiple(Some(&parent), None::<&gio::Cancellable>, move |result| {
        let Ok(list) = result else { return };
        let paths = crate::gui::local_paths(&list);
        if paths.is_empty() {
            return;
        }
        append_unique(&files, paths);
        refresh_files_row(&row, &files.borrow());
    });
}

/// Adds the chosen folder: its images directly inside, listed at Apply;
/// subfolders are not searched.
fn add_folder_to_batch(
    window: &adw::ApplicationWindow,
    files: &Rc<RefCell<Vec<PathBuf>>>,
    row: &adw::ActionRow,
) {
    let dialog = gtk::FileDialog::builder().title(gettext("Add image folder")).modal(true).build();
    let parent = window.clone();
    let files = files.clone();
    let row = row.clone();
    dialog.select_folder(Some(&parent), None::<&gio::Cancellable>, move |result| {
        let Ok(folder) = result else { return };
        let Some(dir) = folder.path() else { return };
        if crate::output_plan::folder_images(&dir).is_ok_and(|images| images.is_empty()) {
            return;
        }
        append_unique(&files, vec![dir]);
        refresh_files_row(&row, &files.borrow());
    });
}

/// Paths are compared as given, not canonicalized.
fn append_unique(files: &Rc<RefCell<Vec<PathBuf>>>, new_paths: Vec<PathBuf>) {
    let mut list = files.borrow_mut();
    for p in new_paths {
        if !list.iter().any(|existing| existing == &p) {
            list.push(p);
        }
    }
}

#[cfg(test)]
mod auto_close_tests {
    use super::*;

    fn spin(context: &glib::MainContext, duration: Duration) {
        let deadline = std::time::Instant::now() + duration;
        while std::time::Instant::now() < deadline {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "requires a graphical display; run alone with GSETTINGS_BACKEND=memory"]
    fn auto_close_is_cancelled_by_a_new_run_and_fires_otherwise() {
        adw::init().expect("GTK display required");
        let app = adw::Application::builder()
            .application_id("com.biglinux.Iris.AutoCloseProbe")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gio::Cancellable>).unwrap();
        let context = glib::MainContext::default();
        let window = adw::ApplicationWindow::builder().application(&app).build();
        window.present();
        use batch_runner::{arm_auto_close, auto_close_armed, disarm_auto_close};

        // timeout_add_seconds is coalesced to whole seconds, so a 1 s timer
        // may fire up to ~2 s later; wait past that.
        // A new run (disarm) before the deadline keeps the window open.
        arm_auto_close(&window, 1);
        assert!(auto_close_armed(&window));
        disarm_auto_close(&window);
        assert!(!auto_close_armed(&window));
        spin(&context, Duration::from_millis(2500));
        assert!(window.is_visible(), "cancelled auto-close still closed the window");

        // Left alone, it closes the window and forgets its source id, so a
        // later disarm is a harmless no-op.
        arm_auto_close(&window, 1);
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while window.is_visible() && std::time::Instant::now() < deadline {
            spin(&context, Duration::from_millis(50));
        }
        assert!(!window.is_visible(), "auto-close did not fire");
        assert!(!auto_close_armed(&window));
        disarm_auto_close(&window);

        // Closing (unrealizing) a window drops its pending timer with it.
        let other = adw::ApplicationWindow::builder().application(&app).build();
        other.present();
        spin(&context, Duration::from_millis(200));
        arm_auto_close(&other, 1);
        other.close();
        spin(&context, Duration::from_millis(200));
        assert!(!auto_close_armed(&other), "closing kept the auto-close armed");
    }
}

#[cfg(test)]
mod folder_tests {
    use super::*;

    /// Colliding files are refused with nothing run, which leaves Close on
    /// the button. A folder converted beside itself does not take its own
    /// earlier output as an input: `a.webp` next to `a.png` in a WebP batch
    /// is listed as skipped, never refused as a collision or converted again.
    #[test]
    #[ignore = "requires a graphical display; run alone with GSETTINGS_BACKEND=memory"]
    fn folder_batch_skips_earlier_outputs() {
        let app = probe_app("com.biglinux.Iris.BatchFolderProbe");
        let dir = tempfile::tempdir().unwrap();
        image::RgbImage::new(8, 8).save(dir.path().join("a.png")).unwrap();
        std::fs::write(dir.path().join("a.webp"), b"earlier output").unwrap();
        let other = tempfile::tempdir().unwrap();
        let colliding = ["b.png", "b.jpg"].map(|name| other.path().join(name));
        for file in &colliding {
            image::RgbImage::new(8, 8).save(file).unwrap();
        }

        let inputs = Rc::new(RefCell::new(colliding.to_vec()));
        let window = build_batch_dialog(&app, inputs.clone());
        window.present();
        let root: &gtk::Widget = window.upcast_ref();
        let ready = gettext("Ready. Click Process to start.");
        let status: gtk::Label =
            find_widget(root, &|label: &gtk::Label| label.text() == ready).unwrap();
        let webp = CONVERT_FORMATS.iter().position(|(_, format)| *format == Format::WebP).unwrap();
        let title = gettext("Target format");
        let format: adw::ComboRow =
            find_widget(root, &|row: &adw::ComboRow| row.title() == title).unwrap();
        format.set_selected(u32::try_from(webp).unwrap());
        let process = gettext("Process");
        let apply: gtk::Button =
            find_widget(root, &|button: &gtk::Button| button.label().as_deref() == Some(&process))
                .unwrap();
        let cancel = apply.prev_sibling().and_downcast::<gtk::Button>().unwrap();
        let run = || {
            apply.emit_clicked();
            pump_until("batch run", &|| {
                apply.is_sensitive() && status.text() != gettext("Processing…")
            });
            status.text()
        };

        let text = run();
        assert!(text.contains("several inputs would write the same output file"), "{text}");
        assert_eq!(cancel.label().as_deref(), Some(gettext("Close").as_str()));

        *inputs.borrow_mut() = vec![dir.path().to_path_buf()];
        let text = run();
        assert!(text.contains("skipped: 2"), "{text}");
        assert!(text.contains("Skipped as outputs of earlier runs: "), "{text}");
        assert_eq!(std::fs::read(dir.path().join("a.webp")).unwrap(), b"earlier output");
        window.destroy();
    }
}
