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

    // Finding 7: external tools refuse to run on unsaved edits.
    apply_edit(&state, EditOp::FlipH);
    gtk::prelude::WidgetExt::activate_action(&window, "win.crop", None).unwrap();
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
