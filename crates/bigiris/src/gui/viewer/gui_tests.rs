// SPDX-License-Identifier: GPL-3.0-or-later
//! Viewer behaviour that needs a display: film strip, wheel routing,
//! properties, trash, Esc and the external tools. Run alone, under a
//! nested or virtual compositor:
//! `GSETTINGS_BACKEND=memory cargo test -p bigiris --features "gui ai"
//! viewer_review_fixes -- --ignored --test-threads=1`
use super::super::*;

fn settle(context: &glib::MainContext, millis: u64) {
    let until = std::time::Instant::now() + Duration::from_millis(millis);
    while std::time::Instant::now() < until {
        while context.pending() {
            context.iteration(false);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait(context: &glib::MainContext, what: &str, condition: &dyn Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !condition() {
        while context.pending() {
            context.iteration(false);
        }
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn window_scroll_controller(window: &adw::ApplicationWindow) -> gtk::EventControllerScroll {
    let controllers = window.observe_controllers();
    (0..controllers.n_items())
        .filter_map(|i| controllers.item(i).and_downcast::<gtk::EventControllerScroll>())
        .next()
        .expect("window wheel controller")
}

/// `true` when the wheel controller swallowed the event.
fn wheel_is_consumed(window: &adw::ApplicationWindow) -> bool {
    window_scroll_controller(window).emit_by_name::<bool>("scroll", &[&0.0f64, &1.0f64])
}

fn thumb_size(state: &Rc<RefCell<ViewerState>>, offset: usize) -> Option<(i32, i32)> {
    let s = state.borrow();
    let picture = thumb_picture(s.film_strip_buttons.get(offset)?)?;
    let paintable = picture.paintable()?;
    Some((paintable.intrinsic_width(), paintable.intrinsic_height()))
}

#[test]
#[ignore = "requires a graphical display; run alone with GSETTINGS_BACKEND=memory"]
fn viewer_review_fixes() {
    assert_eq!(
        std::env::var("GSETTINGS_BACKEND").as_deref(),
        Ok("memory"),
        "synthetic GUI tests require GSETTINGS_BACKEND=memory"
    );
    adw::init().expect("GTK display required");
    let app = adw::Application::builder()
        .application_id("com.biglinux.Iris.ReviewProbe")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gio::Cancellable>).unwrap();
    let context = glib::MainContext::default();
    let dir = tempfile::tempdir().unwrap();

    // Start page: the wheel scrolls the page instead of zooming nothing.
    {
        let (window, _state) = build_window_tracked(&app, Rc::new(Vec::new()), 0);
        window.present();
        settle(&context, 100);
        assert!(!wheel_is_consumed(&window), "start page wheel was swallowed");
        window.destroy();
    }

    // A 1 × 65535 image first, then 42 ordinary ones: 43 files.
    let tall = dir.path().join("tall.png");
    image::GrayImage::from_pixel(1, 65_535, image::Luma([200])).save(&tall).unwrap();
    let mut files = vec![tall.clone()];
    for i in 0..42 {
        let path = dir.path().join(format!("img-{i:02}.png"));
        image::RgbImage::from_pixel(60, 40, image::Rgb([i as u8 * 5, 80, 120]))
            .save(&path)
            .unwrap();
        files.push(path);
    }
    let (window, witness) = build_window_tracked(&app, Rc::new(files.clone()), 1);
    window.set_default_size(800, 600);
    window.present();
    let state = witness.upgrade().unwrap();
    wait(&context, "first frame", &|| state.borrow().texture.is_some());

    // Finding 1: the tall thumbnail is bounded instead of 128 × 8 388 480.
    wait(&context, "tall thumbnail", &|| thumb_size(&state, 0).is_some());
    let (w, h) = thumb_size(&state, 0).unwrap();
    assert!(w <= 512 && h <= 512, "tall thumbnail is {w}x{h}");
    wait(&context, "ordinary thumbnail", &|| thumb_size(&state, 2).is_some());
    assert_eq!(thumb_size(&state, 2), Some((60, 40)), "small image was upscaled");

    // Finding 3: with an image the wheel zooms; with a dialog it does not.
    assert!(wheel_is_consumed(&window), "wheel no longer zooms the image");
    present_shortcuts_dialog(&window);
    settle(&context, 100);
    assert!(window.visible_dialog().is_some());
    assert!(!wheel_is_consumed(&window), "wheel zoomed behind the shortcuts dialog");
    window.visible_dialog().unwrap().close();
    settle(&context, 300);
    set_zoom(&state, ZoomState::Fit);

    // Finding 2: pointer motion at the bottom edge keeps a manual scroll.
    {
        state.borrow_mut().ui_pinned = false;
        let height = f64::from(window.height());
        on_pointer_motion(&state, height - 5.0);
        settle(&context, 400);
        let strip = state.borrow().film_strip_scroll.clone();
        let adjustment = strip.hadjustment();
        assert!(adjustment.upper() > adjustment.page_size(), "strip does not scroll");
        let manual = adjustment.upper() - adjustment.page_size();
        adjustment.set_value(manual);
        for _ in 0..20 {
            on_pointer_motion(&state, height - 5.0);
            settle(&context, 10);
        }
        assert_eq!(adjustment.value(), manual, "motion recentred the film strip");
    }

    // Finding 11: motion re-arms the cursor deadline without new sources.
    {
        let first = state.borrow().cursor_hide_source.as_ref().map(|id| id.as_raw());
        assert!(first.is_some());
        for _ in 0..10 {
            on_pointer_motion(&state, 300.0);
        }
        let after = state.borrow().cursor_hide_source.as_ref().map(|id| id.as_raw());
        assert_eq!(first, after, "motion recreated the cursor timer");
    }

    // Finding 5: the last image keeps a full strip and can reveal it.
    let last = files.len() - 1;
    handle_key(&state, gdk::Key::End, gdk::ModifierType::empty());
    wait(&context, "last image", &|| state.borrow().idx == last);
    {
        let s = state.borrow();
        assert_eq!(s.film_strip_buttons.len(), viewer::FILM_STRIP_PAGE);
        assert!(s.film_strip_start <= last && last < s.film_strip_start + 41);
    }
    hide_bottom(&state);
    toggle_film_strip(&state);
    assert!(state.borrow().strip_revealer.reveals_child(), "strip refused to show");

    // Finding 4: properties follow the image after navigation.
    show_metadata_for_current(&state);
    let first_props = state.borrow().props_dialog.clone().unwrap();
    assert_eq!(first_props.1, files[last]);
    show_metadata_for_current(&state);
    assert_eq!(state.borrow().props_dialog.as_ref().unwrap().0, first_props.0);
    navigate_relative(&state, -1);
    show_metadata_for_current(&state);
    let second_props = state.borrow().props_dialog.clone().unwrap();
    assert_eq!(second_props.1, files[last - 1], "properties kept the previous image");
    assert_ne!(second_props.0, first_props.0);
    second_props.0.close();
    settle(&context, 50);
    assert!(state.borrow().props_dialog.is_none());
    drop((first_props, second_props));

    // Finding 7: external tools refuse to run on unsaved edits. (Crop is
    // inline now and works on top of them; covered by `viewer_inline_crop`.)
    apply_edit(&state, EditOp::FlipH);
    gtk::prelude::WidgetExt::activate_action(&window, "win.remove-bg", None).unwrap();
    assert_eq!(
        state.borrow().osd_label.text(),
        gettext("Salve ou desfaça as alterações antes de usar esta ferramenta")
    );
    undo_edit(&state);

    // Finding 6: no trash while a save is running.
    let trash_enabled = || {
        window.lookup_action("trash").unwrap().downcast::<gio::SimpleAction>().unwrap().is_enabled()
    };
    assert!(trash_enabled());
    state.borrow_mut().saving = true;
    sync_edit_actions(&state);
    assert!(!trash_enabled(), "trash stays enabled while saving");
    state.borrow_mut().saving = false;
    sync_edit_actions(&state);

    // Finding 6: a failed trash is reported, not just logged.
    {
        let missing = dir.path().join("gone.png");
        let mut list = state.borrow().files.as_ref().clone();
        let idx = state.borrow().idx;
        list[idx] = missing;
        state.borrow_mut().files = Rc::new(list);
        trash_current(&state);
        settle(&context, 50);
        let confirm = window.visible_dialog().and_downcast::<adw::AlertDialog>().unwrap();
        confirm.emit_by_name_with_details::<()>(
            "response",
            glib::Quark::from_str("trash"),
            &[&"trash"],
        );
        confirm.close();
        settle(&context, 300);
        let error = window.visible_dialog().and_downcast::<adw::AlertDialog>();
        let error = error.expect("trash failure was not shown");
        assert_eq!(
            error.heading().as_deref(),
            Some(gettext("Não foi possível mover para a lixeira").as_str())
        );
        error.close();
        settle(&context, 300);
        let mut list = state.borrow().files.as_ref().clone();
        list[idx] = files[idx].clone();
        state.borrow_mut().files = Rc::new(list);
    }

    // Finding 12: after the trash the viewer moves on instead of closing.
    {
        let idx = state.borrow().idx;
        let removed = files[idx].clone();
        remove_current_file(&state, &removed);
        let s = state.borrow();
        assert_eq!(s.files.len(), files.len() - 1);
        assert_eq!(s.files[s.idx], files[idx + 1], "did not move to the next image");
        drop(s);
        // Last image: falls back to the previous one.
        handle_key(&state, gdk::Key::End, gdk::ModifierType::empty());
        let last_path = state.borrow().files.last().unwrap().clone();
        remove_current_file(&state, &last_path);
        let s = state.borrow();
        assert_eq!(s.idx, s.files.len() - 1);
        assert!(!s.files.contains(&last_path));
        assert!(window.is_visible());
    }
    wait(&context, "image after trash", &|| state.borrow().current_ready);

    // Finding 12: Esc leaves fullscreen first.
    window.fullscreen();
    wait(&context, "fullscreen", &|| window.is_fullscreen());
    handle_key(&state, gdk::Key::Escape, gdk::ModifierType::empty());
    wait(&context, "unfullscreen", &|| !window.is_fullscreen());
    assert!(window.is_visible(), "Esc closed a fullscreen window");

    // The async frame tasks release the state when the window goes.
    drop(state);
    window.destroy();
    drop(window);
    wait(&context, "viewer state released", &|| witness.upgrade().is_none());

    // Down to one image and then none: the strip hides, the window closes.
    let pair = vec![files[1].clone(), files[2].clone()];
    let (window, witness) = build_window_tracked(&app, Rc::new(pair.clone()), 0);
    window.present();
    let state = witness.upgrade().unwrap();
    wait(&context, "pair frame", &|| state.borrow().current_ready);
    remove_current_file(&state, &pair[0]);
    assert!(!state.borrow().strip_revealer.is_visible());
    assert!(!window.lookup_action("next").unwrap().is_enabled());
    remove_current_file(&state, &pair[1]);
    settle(&context, 100);
    assert!(!window.is_visible(), "window stayed open without images");
    drop(state);
    drop(window);
    wait(&context, "viewer state released after closing", &|| witness.upgrade().is_none());
}

fn answer(window: &adw::ApplicationWindow, response: &str) {
    let dialog = window.visible_dialog().and_downcast::<adw::AlertDialog>().expect("alert shown");
    dialog.emit_by_name_with_details::<()>(
        "response",
        glib::Quark::from_str(response),
        &[&response],
    );
    dialog.close();
}

fn current_thumb_width(state: &Rc<RefCell<ViewerState>>) -> Option<i32> {
    let (idx, start) = (state.borrow().idx, state.borrow().film_strip_start);
    thumb_size(state, idx.checked_sub(start)?).map(|(w, _)| w)
}

#[test]
#[ignore = "requires a graphical display; run alone with GSETTINGS_BACKEND=memory"]
fn viewer_second_review_fixes() {
    assert_eq!(
        std::env::var("GSETTINGS_BACKEND").as_deref(),
        Ok("memory"),
        "synthetic GUI tests require GSETTINGS_BACKEND=memory"
    );
    adw::init().expect("GTK display required");
    let app = adw::Application::builder()
        .application_id("com.biglinux.Iris.SecondReviewProbe")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gio::Cancellable>).unwrap();
    let context = glib::MainContext::default();
    let dir = tempfile::tempdir().unwrap();

    // 45 images, each with its own width, so a thumbnail tells which file
    // it was made from.
    let files: Vec<PathBuf> = (0..45u32)
        .map(|i| {
            let path = dir.path().join(format!("img-{i:02}.png"));
            image::RgbImage::from_pixel(40 + i, 30, image::Rgb([i as u8 * 5, 80, 120]))
                .save(&path)
                .unwrap();
            path
        })
        .collect();
    let (window, witness) = build_window_tracked(&app, Rc::new(files.clone()), 0);
    window.set_default_size(800, 600);
    window.present();
    let state = witness.upgrade().unwrap();
    wait(&context, "first frame", &|| state.borrow().current_ready);

    // Finding 1: jumping past the strip shows the new file in its cell,
    // not the texture of the image that was on screen before.
    handle_key(&state, gdk::Key::End, gdk::ModifierType::empty());
    wait(&context, "last image", &|| state.borrow().current_ready);
    settle(&context, 300);
    assert_eq!(current_thumb_width(&state), Some(40 + 44), "End: stale current cell");
    handle_key(&state, gdk::Key::Home, gdk::ModifierType::empty());
    wait(&context, "first image", &|| state.borrow().current_ready);
    settle(&context, 300);
    assert_eq!(current_thumb_width(&state), Some(40), "Home: stale current cell");
    // Trashing rebuilds the strip before the next frame is decoded.
    remove_current_file(&state, &files[0]);
    wait(&context, "image after removal", &|| state.borrow().current_ready);
    settle(&context, 300);
    assert_eq!(current_thumb_width(&state), Some(41), "removal: stale current cell");

    // Finding 2: a vertical wheel step scrolls the strip horizontally.
    {
        let strip = state.borrow().film_strip_scroll.clone();
        hide_bottom(&state);
        toggle_film_strip(&state);
        settle(&context, 400);
        let controllers = strip.observe_controllers();
        let wheel = (0..controllers.n_items())
            .filter_map(|i| controllers.item(i).and_downcast::<gtk::EventControllerScroll>())
            .find(|c| c.propagation_phase() == gtk::PropagationPhase::Capture)
            .expect("strip wheel controller");
        let adjustment = strip.hadjustment();
        adjustment.set_value(0.0);
        assert!(wheel.emit_by_name::<bool>("scroll", &[&0.0f64, &1.0f64]));
        assert!(adjustment.value() > 0.0, "vertical wheel did not scroll the strip");
    }
    drop(state);
    window.destroy();
    drop(window);
    wait(&context, "first viewer released", &|| witness.upgrade().is_none());

    // Findings 4, 7 and 9 on a pair: a static PNG and an animated GIF.
    let png = dir.path().join("a-photo.png");
    image::RgbImage::from_pixel(64, 32, image::Rgb([200, 10, 10])).save(&png).unwrap();
    let old = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000);
    std::fs::File::options().write(true).open(&png).unwrap().set_modified(old).unwrap();
    let gif = dir.path().join("b-anim.gif");
    {
        let frames = (0..2u8).map(|i| {
            image::Frame::new(image::RgbaImage::from_pixel(
                16,
                16,
                image::Rgba([i * 100, 0, 0, 255]),
            ))
        });
        image::codecs::gif::GifEncoder::new(std::fs::File::create(&gif).unwrap())
            .encode_frames(frames)
            .unwrap();
    }
    let pair = vec![png.clone(), gif.clone()];
    let (window, witness) = build_window_tracked(&app, Rc::new(pair.clone()), 0);
    window.present();
    let state = witness.upgrade().unwrap();
    wait(&context, "png frame", &|| state.borrow().current_ready);

    show_metadata_for_current(&state);
    let props_before = state.borrow().props_dialog.clone().unwrap().0;

    // Finding 9: a successful save caches the result, paints the cell and
    // resumes the prefetch.
    apply_edit(&state, EditOp::RotateCw);
    wait(&context, "edited frame", &|| state.borrow().current_ready);
    save_edits(&state);
    settle(&context, 100);
    answer(&window, "save");
    wait(&context, "save done", &|| {
        let s = state.borrow();
        !s.saving && s.pending_ops.is_empty()
    });
    {
        let s = state.borrow();
        let modified = file_modified(&png);
        assert_ne!(modified, Some(old));
        assert!(
            s.frame_cache.iter().any(|f| f.path == png && f.modified == modified),
            "saved frame not cached"
        );
        assert_eq!(s.shown_path.as_ref(), Some(&png));
        // Nothing was dropped: the plain confirmation, no metadata notice.
        assert_eq!(s.osd_label.label().as_str(), gettext("Salvo"));
    }
    assert_eq!(current_thumb_width(&state), Some(32), "cell kept the pre-save thumbnail");
    wait(&context, "prefetch after save", &|| {
        state.borrow().frame_cache.iter().any(|f| f.path == gif)
    });

    // Finding 7: the properties window is rebuilt for the saved file.
    show_metadata_for_current(&state);
    let props_after = state.borrow().props_dialog.clone().unwrap().0;
    assert_ne!(props_before, props_after, "properties kept the pre-save details");
    props_after.close();
    settle(&context, 50);

    // Finding 4: a refused save explains why and lets the edits go.
    navigate_relative(&state, 1);
    wait(&context, "gif frame", &|| {
        let s = state.borrow();
        s.current_ready && s.files[s.idx] == gif
    });
    apply_edit(&state, EditOp::FlipH);
    wait(&context, "edited gif", &|| state.borrow().current_ready);
    save_edits(&state);
    settle(&context, 100);
    answer(&window, "save");
    wait(&context, "save failure", &|| !state.borrow().saving && window.visible_dialog().is_some());
    let failure = window.visible_dialog().and_downcast::<adw::AlertDialog>().unwrap();
    assert_eq!(failure.heading().as_deref(), Some(gettext("Não foi possível salvar").as_str()));
    assert_eq!(failure.body(), SaveFailure::NotStatic.message());
    drop(failure);
    answer(&window, "discard");
    settle(&context, 100);
    assert!(state.borrow().pending_ops.is_empty(), "edits were not discarded");
    assert!(can_leave_current(&state));

    drop(state);
    window.destroy();
    drop(window);
    wait(&context, "second viewer released", &|| witness.upgrade().is_none());

    // A save that cannot keep the source EXIF says so instead of "Salvo":
    // JPEG pixels with EXIF under a `.bmp` name are written back as BMP.
    let bmp = dir.path().join("c-camera.bmp");
    {
        use image::ImageEncoder;
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
            std::fs::File::create(&bmp).unwrap(),
            90,
        );
        encoder.set_exif_metadata(b"II*\0\x08\0\0\0\0\0\0\0\0\0".to_vec()).unwrap();
        let pixels = image::RgbImage::from_pixel(24, 16, image::Rgb([20, 120, 220]));
        encoder.write_image(pixels.as_raw(), 24, 16, image::ExtendedColorType::Rgb8).unwrap();
    }
    let (window, witness) = build_window_tracked(&app, Rc::new(vec![bmp.clone()]), 0);
    window.present();
    let state = witness.upgrade().unwrap();
    wait(&context, "camera frame", &|| state.borrow().current_ready);
    apply_edit(&state, EditOp::RotateCw);
    wait(&context, "edited camera frame", &|| state.borrow().current_ready);
    save_edits(&state);
    settle(&context, 100);
    answer(&window, "save");
    wait(&context, "camera save done", &|| {
        let s = state.borrow();
        !s.saving && s.pending_ops.is_empty()
    });
    assert_eq!(
        state.borrow().osd_label.label().as_str(),
        metadata_loss_notice(&[bigimage_core::MetadataLoss::ExifUnsupported]).unwrap()
    );
    assert!(state.borrow().osd_revealer.reveals_child(), "notice not shown");
    drop(state);
    window.destroy();
    drop(window);
    wait(&context, "third viewer released", &|| witness.upgrade().is_none());
}

fn crop_drag(state: &Rc<RefCell<ViewerState>>) -> gtk::GestureDrag {
    let area = state.borrow().crop_ui.area.clone();
    let controllers = area.observe_controllers();
    (0..controllers.n_items())
        .filter_map(|i| controllers.item(i).and_downcast::<gtk::GestureDrag>())
        .next()
        .expect("crop drag gesture")
}

/// Drag on the crop overlay from natural point `from` by `by` natural pixels.
fn drag_selection(state: &Rc<RefCell<ViewerState>>, from: (f64, f64), by: (f64, f64)) {
    let fit = {
        let s = state.borrow();
        let natural = s.crop.as_ref().map(|_| s.natural).expect("crop active");
        super::crop::current_fit(&s, natural)
    };
    assert!(fit.scale > 0.0, "picture not laid out");
    let gesture = crop_drag(state);
    let start = (fit.ox + from.0 * fit.scale, fit.oy + from.1 * fit.scale);
    gesture.emit_by_name::<()>("drag-begin", &[&start.0, &start.1]);
    gesture.emit_by_name::<()>("drag-update", &[&(by.0 * fit.scale), &(by.1 * fit.scale)]);
    gesture.emit_by_name::<()>("drag-end", &[&(by.0 * fit.scale), &(by.1 * fit.scale)]);
}

fn action_enabled(window: &adw::ApplicationWindow, name: &str) -> bool {
    window.lookup_action(name).unwrap().downcast::<gio::SimpleAction>().unwrap().is_enabled()
}

/// RGBA of one pixel of the texture on screen.
fn texture_pixel(state: &Rc<RefCell<ViewerState>>, x: usize, y: usize) -> [u8; 4] {
    let texture = state.borrow().texture.clone().expect("texture");
    let (w, h) = (texture.width() as usize, texture.height() as usize);
    let mut downloader = gdk::TextureDownloader::new(&texture);
    downloader.set_format(gdk::MemoryFormat::R8g8b8a8);
    let (bytes, stride) = downloader.download_bytes();
    assert!(x < w && y < h);
    let i = y * stride + x * 4;
    [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]
}

#[test]
#[ignore = "requires a graphical display; run alone with GSETTINGS_BACKEND=memory"]
fn viewer_inline_crop() {
    assert_eq!(
        std::env::var("GSETTINGS_BACKEND").as_deref(),
        Ok("memory"),
        "synthetic GUI tests require GSETTINGS_BACKEND=memory"
    );
    adw::init().expect("GTK display required");
    let app = adw::Application::builder()
        .application_id("com.biglinux.Iris.InlineCropProbe")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gio::Cancellable>).unwrap();
    let context = glib::MainContext::default();
    let dir = tempfile::tempdir().unwrap();

    // 400 × 300 with four coloured quadrants, so a crop shows which part
    // of the image it kept.
    let photo = dir.path().join("a.png");
    image::RgbImage::from_fn(400, 300, |x, y| match (x < 200, y < 150) {
        (true, true) => image::Rgb([255, 0, 0]),
        (false, true) => image::Rgb([0, 255, 0]),
        (true, false) => image::Rgb([0, 0, 255]),
        (false, false) => image::Rgb([255, 255, 0]),
    })
    .save(&photo)
    .unwrap();
    let other = dir.path().join("b.png");
    image::RgbImage::from_pixel(64, 48, image::Rgb([9, 9, 9])).save(&other).unwrap();

    let (window, witness) = build_window_tracked(&app, Rc::new(vec![photo.clone(), other]), 0);
    window.set_default_size(1000, 760);
    window.present();
    let state = witness.upgrade().unwrap();
    wait(&context, "first frame", &|| state.borrow().current_ready);
    settle(&context, 200);
    assert_eq!(state.borrow().natural, (400, 300));

    // With a real pointer over the window, showing or hiding the crop
    // layer emits crossing events synchronously, which re-enter the
    // picture's motion handler. The virtual display has no pointer, so
    // replay that motion from the visibility change itself.
    {
        let (area, scroller) = {
            let s = state.borrow();
            (s.crop_ui.area.clone(), s.scroller.clone())
        };
        let controllers = scroller.observe_controllers();
        let motion = (0..controllers.n_items())
            .filter_map(|i| controllers.item(i).and_downcast::<gtk::EventControllerMotion>())
            .find(|c| c.name().as_deref() == Some("bigiris-zoom-pointer"))
            .expect("zoom pointer controller");
        area.connect_visible_notify(move |_| {
            motion.emit_by_name::<()>("motion", &[&10.0f64, &10.0f64]);
        });
    }

    // Ctrl+R enters the crop mode on the main picture: no extra window,
    // the whole image is selected and actions that would move or change
    // the picture are suspended.
    let windows_before = app.windows().len();
    gtk::prelude::WidgetExt::activate_action(&window, "win.crop", None).unwrap();
    settle(&context, 100);
    assert!(state.borrow().crop.is_some(), "crop mode not active");
    assert_eq!(app.windows().len(), windows_before, "crop opened another window");
    assert!(state.borrow().crop_ui.area.is_visible() && state.borrow().crop_ui.bar.is_visible());
    crate::gui::lifecycle_tests::capture_window(&window, "crop-1-enter");
    for name in ["next", "rotate-cw", "save-edits", "trash", "crop"] {
        assert!(!action_enabled(&window, name), "{name} stays enabled while cropping");
    }

    // Arrow keys and the wheel do not navigate or zoom under the selection.
    handle_key(&state, gdk::Key::Right, gdk::ModifierType::empty());
    assert_eq!(state.borrow().idx, 0, "navigated while cropping");
    assert!(wheel_is_consumed(&window));
    assert!(matches!(state.borrow().zoom, ZoomState::Fit), "zoomed while cropping");

    // Esc cancels without touching the image (and does not close the window).
    handle_key(&state, gdk::Key::Escape, gdk::ModifierType::empty());
    settle(&context, 50);
    assert!(state.borrow().crop.is_none());
    assert!(state.borrow().pending_ops.is_empty());
    assert!(window.is_visible(), "Esc closed the window instead of leaving crop");
    assert!(action_enabled(&window, "next"), "actions not restored after cancel");

    // Drag the bottom-right handle to the centre: keep the red quadrant.
    gtk::prelude::WidgetExt::activate_action(&window, "win.crop", None).unwrap();
    settle(&context, 100);
    drag_selection(&state, (400.0, 300.0), (-200.0, -150.0));
    crate::gui::lifecycle_tests::capture_window(&window, "crop-2-selection");
    assert_eq!(
        super::crop::to_pixels(state.borrow().crop.as_ref().map(|c| c.rect()).unwrap(), (400, 300)),
        (0, 0, 200, 150)
    );
    gtk::prelude::WidgetExt::activate_action(&window, "win.crop-apply", None).unwrap();
    wait(&context, "cropped frame", &|| {
        let s = state.borrow();
        s.current_ready && s.natural == (200, 150)
    });
    assert!(state.borrow().crop.is_none());
    assert_eq!(state.borrow().pending_ops, vec![EditOp::Crop { x: 0, y: 0, w: 200, h: 150 }]);
    assert_eq!(texture_pixel(&state, 10, 10), [255, 0, 0, 255]);
    crate::gui::lifecycle_tests::capture_window(&window, "crop-3-cropped");
    assert_eq!(texture_pixel(&state, 190, 140), [255, 0, 0, 255]);
    assert!(action_enabled(&window, "save-edits") && action_enabled(&window, "undo"));

    // Undo brings the whole image back, Redo crops again.
    undo_edit(&state);
    wait(&context, "uncropped", &|| {
        let s = state.borrow();
        s.current_ready && s.natural == (400, 300)
    });
    redo_edit(&state);
    wait(&context, "re-cropped", &|| {
        let s = state.borrow();
        s.current_ready && s.natural == (200, 150)
    });

    // Crops stack on the edited image: a new selection drawn inside the
    // cropped picture, confirmed with Enter.
    gtk::prelude::WidgetExt::activate_action(&window, "win.crop", None).unwrap();
    settle(&context, 100);
    assert_eq!(state.borrow().crop.as_ref().map(|c| c.natural()), Some((200, 150)));
    drag_selection(&state, (50.0, 25.0), (100.0, 100.0));
    handle_key(&state, gdk::Key::Return, gdk::ModifierType::empty());
    wait(&context, "second crop", &|| {
        let s = state.borrow();
        s.current_ready && s.natural == (100, 100)
    });

    // A fixed ratio: square, largest centred box of the current image.
    gtk::prelude::WidgetExt::activate_action(&window, "win.crop", None).unwrap();
    settle(&context, 100);
    let aspect = state.borrow().crop_ui.aspect_dropdown().clone();
    aspect.set_selected(2);
    settle(&context, 50);
    gtk::prelude::WidgetExt::activate_action(&window, "win.crop-apply", None).unwrap();
    settle(&context, 200);
    // Already square: nothing to crop, no extra history entry.
    assert_eq!(state.borrow().pending_ops.len(), 2);
    undo_edit(&state);
    wait(&context, "back to 200x150", &|| {
        let s = state.borrow();
        s.current_ready && s.natural == (200, 150)
    });
    gtk::prelude::WidgetExt::activate_action(&window, "win.crop", None).unwrap();
    settle(&context, 100);
    let aspect = state.borrow().crop_ui.aspect_dropdown().clone();
    aspect.set_selected(2);
    settle(&context, 50);
    gtk::prelude::WidgetExt::activate_action(&window, "win.crop-apply", None).unwrap();
    wait(&context, "square crop", &|| {
        let s = state.borrow();
        s.current_ready && s.natural == (150, 150)
    });

    // Ctrl+S writes the cropped pixels to the file.
    save_edits(&state);
    settle(&context, 100);
    answer(&window, "save");
    wait(&context, "save done", &|| {
        let s = state.borrow();
        !s.saving && s.pending_ops.is_empty()
    });
    let saved = image::open(&photo).unwrap().to_rgb8();
    assert_eq!(saved.dimensions(), (150, 150));
    // Centre square of the red 200×150 crop: all red.
    assert_eq!(saved.get_pixel(0, 0).0, [255, 0, 0]);
    assert_eq!(saved.get_pixel(149, 149).0, [255, 0, 0]);

    window.destroy();
    settle(&context, 100);
}
