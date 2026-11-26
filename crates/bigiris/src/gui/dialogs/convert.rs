// SPDX-License-Identifier: GPL-3.0-or-later
//! Convert: change the format of the selected images.

use super::*;

/// Run the "Convert" modal. Returns GTK's exit code.
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
        .title(gettext("Convert images — Prisma"))
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
    let title = adw::WindowTitle::new(&gettext("Convert"), "Prisma");
    header.set_title_widget(Some(&title));

    let format_row = format_row();

    let overwrite_row = overwrite_row();

    let files_row = selection_row(&gettext("Selected files"), &files);

    let encode = EncodeControls::new();
    encode.bind(&window, &format_row);

    let conv_group = adw::PreferencesGroup::builder().title(gettext("Conversion")).build();
    conv_group.add(&files_row);
    conv_group.add(&format_row);
    conv_group.add(&encode.target);
    conv_group.add(&encode.advanced);

    let dest_group = adw::PreferencesGroup::builder().title(gettext("Destination")).build();
    dest_group.add(&overwrite_row);

    let (folder_row, output_dir) = output_folder_row(&window);
    dest_group.add(&folder_row);

    let status = status_label();

    let (cancel_btn, apply_btn, actions) = action_buttons(&gettext("Convert"));

    // Smart prompt: if the source image has an alpha channel and the user
    // picks JPEG, warn that transparency will be flattened.
    let alpha_banner = adw::Banner::builder()
        .title(gettext(
            "This image has transparency — JPEG drops the alpha channel. Consider PNG or WebP.",
        ))
        .revealed(false)
        .build();
    if let Some(sess) = preview_session.as_ref()
        && sess.has_alpha_channel()
    {
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

    // Privacy: warn that GPS coordinates in the EXIF can reveal where the
    // photo was taken once it is shared.
    let gps_banner = adw::Banner::builder()
        .title(gettext("This image contains GPS (location) data in its EXIF. Be careful when sharing it publicly — our pipeline strips EXIF automatically when re-encoding."))
        .revealed(false)
        .build();
    gps_banner.add_css_class("warning");
    if let Some(sess) = preview_session.as_ref()
        && sess.source_has_gps()
    {
        gps_banner.set_revealed(true);
    }

    let content = padded_box(gtk::Orientation::Vertical, 16);
    content.append(&gps_banner);
    content.append(&alpha_banner);
    content.append(&conv_group);
    content.append(&dest_group);
    // The preview stays the source thumbnail: a format change does not
    // change the pixels enough to redraw it.
    set_preview_dialog_content(
        &window,
        &header,
        &content,
        &[status.upcast_ref(), actions.upcast_ref()],
        preview_session.as_deref(),
    );

    close_on_click(&window, &cancel_btn);

    {
        let files = files.clone();
        let format_row = format_row.clone();
        let overwrite_row = overwrite_row.clone();
        let encode = Rc::new(encode);
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let window = window.clone();
        let output_dir = output_dir.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let fmt = selected_format(&format_row);
            let policy = selected_overwrite(&overwrite_row);
            let opts = encode.options();
            // Snapshot the chosen folder once at submit time — the
            // picker can change while the worker is running and the
            // worker thread mustn't peek into Rc<RefCell<...>>.
            let out_dir_snapshot = output_dir.borrow().clone();
            let naming = crate::output_plan::Naming {
                output_dir: out_dir_snapshot.clone(),
                extension: Some(fmt.extension()),
                policy,
                per_image_suffix: false,
                suffixed: false,
            };
            let start = {
                let (files, naming) = (files.clone(), naming.clone());
                let (status, apply_btn, window) =
                    (status.clone(), apply_btn.clone(), window.clone());
                move || {
                    apply_btn.set_sensitive(false);
                    status.set_text(&gettext("Converting…"));
                    run_batch_async(
                        (*files).clone(),
                        naming,
                        move |f| {
                            convert_file_to(f, out_dir_snapshot.as_deref(), fmt, &opts, policy)
                        },
                        gettext("Converting"),
                        status,
                        apply_btn,
                        window,
                    );
                }
            };
            confirm_replacing_originals(&window, &files, &naming, start);
        });
    }

    window
}

#[cfg(test)]
mod replace_tests {
    use super::*;

    /// JPEG to JPEG next to the source under "Overwrite the file" names the
    /// output like the source: Convert asks first, and Cancel (the default)
    /// leaves the original as it was.
    #[test]
    #[ignore = "requires a graphical display; run alone with GSETTINGS_BACKEND=memory"]
    fn replacing_an_original_asks_first() {
        let app = probe_app("com.biglinux.Iris.ReplaceProbe");
        let dir = tempfile::tempdir().unwrap();
        let photo = dir.path().join("photo.jpg");
        image::RgbImage::from_pixel(64, 48, image::Rgb([200, 30, 30])).save(&photo).unwrap();
        let original = std::fs::read(&photo).unwrap();

        let window = build_convert_dialog(&app, Rc::new(vec![photo.clone()]));
        window.present();
        let root: gtk::Widget = window.clone().upcast();
        let convert = gettext("Convert");
        let apply = || {
            find_widget(&root, &|button: &gtk::Button| {
                button.label().as_deref() == Some(convert.as_str())
            })
        };
        pump_until("convert form", &|| apply().is_some());
        let row = |title: &str| {
            let title = gettext(title);
            find_widget(&root, &|row: &adw::ComboRow| row.title() == title).unwrap()
        };
        let jpeg = CONVERT_FORMATS.iter().position(|(_, format)| *format == Format::Jpeg);
        row("Target format").set_selected(u32::try_from(jpeg.unwrap()).unwrap());
        let replace = OVERWRITE_CHOICES.iter().position(|(_, p)| *p == OverwritePolicy::Replace);
        row("If the output file exists").set_selected(u32::try_from(replace.unwrap()).unwrap());

        apply().unwrap().emit_clicked();
        pump_until("confirmation", &|| window.visible_dialog().is_some());
        let alert = window.visible_dialog().and_downcast::<adw::AlertDialog>().unwrap();
        assert_eq!(alert.default_response().as_deref(), Some("cancel"));
        alert.close();
        pump_until("confirmation closed", &|| window.visible_dialog().is_none());
        assert!(apply().unwrap().is_sensitive(), "nothing runs after Cancel");
        assert_eq!(std::fs::read(&photo).unwrap(), original);
        window.destroy();
    }
}
