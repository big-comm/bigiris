// SPDX-License-Identifier: GPL-3.0-or-later
//! Batch: batch conversion with progress, cancel and auto-close.

use super::*;

/// Prisma Batch: converts the files given on the command line, with folders
/// expanded one level, on the shared parallel pool.
pub fn run_batch_dialog(files: Vec<PathBuf>) -> i32 {
    let files = Rc::new(RefCell::new(expand_batch_inputs(files)));
    run_dialog_app(move |app| build_batch_dialog(app, files.clone()).present())
}

fn expand_batch_inputs(inputs: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for input in inputs {
        if input.is_dir() {
            files.extend(collect_images_in_dir(&input));
        } else if input.is_file() && is_viewable(&input) {
            files.push(input);
        }
    }
    files.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    files.dedup();
    files
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
    // None writes next to each source file.
    let output_dir: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
    let folder_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Output folder"))
        .subtitle(gettext("Same folder as the source file"))
        .activatable(true)
        .build();
    let folder_icon = gtk::Image::from_icon_name("folder-open-symbolic");
    folder_icon.add_css_class("dim-label");
    folder_row.add_suffix(&folder_icon);
    let clear_folder_btn = gtk::Button::builder()
        .icon_name("edit-clear-symbolic")
        .tooltip_text(gettext("Use the source file folder"))
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .visible(false)
        .build();
    folder_row.add_suffix(&clear_folder_btn);
    dest_group.add(&folder_row);

    let progress = gtk::ProgressBar::builder().show_text(true).text(gettext("Ready")).build();
    progress.set_margin_start(18);
    progress.set_margin_end(18);
    progress.set_margin_top(8);

    let status = gtk::Label::builder()
        .label(gettext("Ready. Click Process to start."))
        .wrap(true)
        .max_width_chars(64)
        .css_classes(["dim-label"])
        .margin_start(18)
        .margin_end(18)
        .margin_top(4)
        .margin_bottom(4)
        .build();

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

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&main_col));
    window.set_content(Some(&toolbar_view));

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
        let output_dir = output_dir.clone();
        let folder_row = folder_row.clone();
        let clear_btn = clear_folder_btn.clone();
        let window = window.clone();
        dialog_signal!(window, folder_row.clone(), connect_activated, move |_| {
            pick_output_folder(&window, &output_dir, &folder_row, &clear_btn);
        });
    }
    {
        let output_dir = output_dir.clone();
        let folder_row = folder_row.clone();
        let clear_btn = clear_folder_btn.clone();
        dialog_signal!(window, clear_folder_btn, connect_clicked, move |_| {
            *output_dir.borrow_mut() = None;
            folder_row.set_subtitle(&gettext("Same folder as the source file"));
            clear_btn.set_visible(false);
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
            let files_owned: Vec<PathBuf> = files.borrow().clone();
            if files_owned.is_empty() {
                status.set_text(&gettext("Add at least one image to process."));
                return;
            }

            // Reset cancel for a re-run, mark a worker as live. A pending
            // auto-close from the previous run must not end this one.
            batch_runner::disarm_auto_close(&window);
            cancelled.store(false, std::sync::atomic::Ordering::Relaxed);
            running.set(true);
            apply_btn.set_sensitive(false);
            cancel_btn.set_label(&gettext("Cancel"));
            progress.set_fraction(0.0);
            progress.set_text(Some(&gettext("Starting…")));
            status.set_text(&gettext("Processing…"));

            let events = batch_runner::spawn_batch_workers(
                files_owned,
                move |path| convert_file_to(path, out_dir_snapshot.as_deref(), fmt, &opts, policy),
                cancelled.clone(),
            );

            let progress = progress.clone();
            let status = status.clone();
            let apply_btn = apply_btn.clone();
            let cancel_btn = cancel_btn.clone();
            let window = window.clone();
            let running = running.clone();
            // Released with this task, i.e. only after the pool reported
            // back: closing the last window never kills a worker mid-write
            // and strands `.part` files.
            let hold = window.application().map(|app| app.hold());
            glib::MainContext::default().spawn_local(async move {
                let _hold = hold;
                loop {
                match events.next().await {
                    Some(batch_runner::AsyncBatchEvent::Progress { done, total, file }) => {
                        progress.set_fraction(done as f64 / total.max(1) as f64);
                        progress.set_text(Some(&format!("{done}/{total} · {file}")));
                    }
                    Some(batch_runner::AsyncBatchEvent::Done {
                        ok,
                        skip,
                        fail,
                        first_err,
                        done,
                        total,
                        cancelled,
                        elapsed,
                        bytes_in,
                        bytes_out,
                    }) => {
                        running.set(false);
                        // Closed mid-run: nothing left to show or to close.
                        if cancelled && !window.is_visible() {
                            return;
                        }
                        let timing = batch_runner::format_duration(elapsed);
                        let template = if cancelled {
                            gettext("Canceled after {done}/{total} in {timing}. Written: {ok}, skipped: {skip}, failed: {fail}.")
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
                                ("skip", &skip.to_string()),
                                ("fail", &fail.to_string()),
                            ],
                        );
                        if ok > 0 && bytes_in > 0 && bytes_out > 0 {
                            let delta = batch_runner::format_size_delta(bytes_in, bytes_out);
                            let avg_in = bytes_in / (ok as u64).max(1);
                            let avg_out = bytes_out / (ok as u64).max(1);
                            msg.push('\n');
                            msg.push_str(
                                &fill(
                                    &gettext("{before} → {after} ({delta}) · average per file {avg_before} → {avg_after}"),
                                    &[
                                        ("before", &batch_runner::format_size(bytes_in)),
                                        ("after", &batch_runner::format_size(bytes_out)),
                                        ("delta", &delta),
                                        ("avg_before", &batch_runner::format_size(avg_in)),
                                        ("avg_after", &batch_runner::format_size(avg_out)),
                                    ],
                                ),
                            );
                        }
                        status.set_text(&msg);
                        if let Some(err) = first_err {
                            status.set_text(&format!("{msg}\n{err}"));
                        }
                        progress.set_text(Some(&gettext(if cancelled {
                            n_("Canceled")
                        } else {
                            n_("Done")
                        })));
                        apply_btn.set_sensitive(true);
                        let cancel_label = if !cancelled && done == total {
                            gettext("Close")
                        } else {
                            gettext("Cancel")
                        };
                        cancel_btn.set_label(&cancel_label);
                        if !cancelled && fail == 0 {
                            batch_runner::arm_auto_close(&window, 3);
                        }
                        return;
                    }
                    None => {
                        running.set(false);
                        return;
                    }
                }
                }
            });
        });
    }

    window
}

fn refresh_files_row(row: &adw::ActionRow, files: &[PathBuf]) {
    let total: u64 = files.iter().filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len())).sum();
    row.set_title(&format!("{}: {}", gettext("Selected files"), files.len()));
    row.set_subtitle(&format_size(total));
}

fn add_files_to_batch(
    window: &adw::ApplicationWindow,
    files: &Rc<RefCell<Vec<PathBuf>>>,
    row: &adw::ActionRow,
) {
    let dialog = gtk::FileDialog::builder().title(gettext("Add files")).modal(true).build();
    let filter = gtk::FileFilter::new();
    filter.set_name(Some(&gettext("Images")));
    for mime in [
        "image/jpeg",
        "image/png",
        "image/webp",
        "image/avif",
        "image/gif",
        "image/tiff",
        "image/bmp",
    ] {
        filter.add_mime_type(mime);
    }
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    dialog.set_filters(Some(&filters));

    let parent = window.clone();
    let files = files.clone();
    let row = row.clone();
    dialog.open_multiple(Some(&parent), None::<&gio::Cancellable>, move |result| {
        let Ok(list) = result else { return };
        let paths: Vec<PathBuf> = (0..list.n_items())
            .filter_map(|i| list.item(i))
            .filter_map(|o| o.downcast::<gio::File>().ok())
            .filter_map(|f| f.path())
            .collect();
        if paths.is_empty() {
            return;
        }
        append_unique(&files, paths);
        refresh_files_row(&row, &files.borrow());
    });
}

/// Adds the images directly inside the chosen folder; subfolders are not
/// searched.
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
        let found = collect_images_in_dir(&dir);
        if found.is_empty() {
            return;
        }
        append_unique(&files, found);
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

/// Lowercase extensions a folder scan accepts, compared case-insensitively.
const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "avif", "gif", "tif", "tiff", "bmp", "ico", "pnm", "ppm", "pgm",
    "pbm", "pam", "tga", "qoi", "hdr", "exr",
];

fn collect_images_in_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext) = path.extension() else { continue };
        let ext_lower = ext.to_string_lossy().to_ascii_lowercase();
        if IMAGE_EXTENSIONS.iter().any(|e| *e == ext_lower) {
            out.push(path);
        }
    }
    out.sort();
    out
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
