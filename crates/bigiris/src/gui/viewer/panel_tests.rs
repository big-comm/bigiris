// SPDX-License-Identifier: GPL-3.0-or-later
//! Editing panel behavior that needs a display. Run each test alone,
//! under a nested or virtual compositor:
//! `GSETTINGS_BACKEND=memory cargo test -p bigiris --features "gui ai"
//! viewer_edit_panel -- --ignored --test-threads=1`
use super::super::*;
use super::gui_tests::{settle, wait};

fn action_enabled(window: &adw::ApplicationWindow, name: &str) -> bool {
    window.lookup_action(name).unwrap().is_enabled()
}

fn pixel(texture: &gdk::Texture, x: u32, y: u32) -> [u8; 4] {
    super::edit_panel::texture_rgba(texture).get_pixel(x, y).0
}

/// RGBA of one pixel of the edited image on screen.
fn texture_pixel(state: &Rc<RefCell<ViewerState>>, x: u32, y: u32) -> [u8; 4] {
    pixel(&state.borrow().texture.clone().expect("texture"), x, y)
}

/// First widget under `root` (depth first) that matches.
fn find(
    root: &impl IsA<gtk::Widget>,
    matches: &dyn Fn(&gtk::Widget) -> bool,
) -> Option<gtk::Widget> {
    let root = root.as_ref();
    if matches(root) {
        return Some(root.clone());
    }
    let mut child = root.first_child();
    while let Some(current) = child {
        if let Some(found) = find(&current, matches) {
            return Some(found);
        }
        child = current.next_sibling();
    }
    None
}

/// The button under `root` bound to `action`.
fn action_button(root: &impl IsA<gtk::Widget>, action: &str) -> gtk::Button {
    find(root, &|w| {
        w.downcast_ref::<gtk::Button>().is_some_and(|b| b.action_name().as_deref() == Some(action))
    })
    .and_downcast::<gtk::Button>()
    .unwrap_or_else(|| panic!("no button for {action}"))
}

fn labeled_button(root: &impl IsA<gtk::Widget>, label: &str) -> gtk::Button {
    find(root, &|w| {
        w.downcast_ref::<gtk::Button>().is_some_and(|b| b.label().as_deref() == Some(label))
    })
    .and_downcast::<gtk::Button>()
    .unwrap_or_else(|| panic!("no button labeled {label}"))
}

/// A synthetic 640 × 480 landscape: sky gradient, sun, hills, and a flat
/// gray patch (100, 100, 100) at 20..80 × 400..460 to measure tone edits.
pub(super) fn landscape(path: &Path) {
    image::RgbImage::from_fn(640, 480, |x, y| {
        let (fx, fy) = (x as f32, y as f32);
        if (20..80).contains(&x) && (400..460).contains(&y) {
            return image::Rgb([100, 100, 100]);
        }
        let sun = ((fx - 470.0).powi(2) + (fy - 120.0).powi(2)).sqrt();
        if sun < 45.0 {
            return image::Rgb([255, 214, 120]);
        }
        let hill = 300.0 + 40.0 * (fx / 90.0).sin() + 25.0 * (fx / 37.0).cos();
        if fy > hill {
            let shade = ((fy - hill) / 3.0).min(60.0) as u8;
            return image::Rgb([40 + shade / 2, 120 - shade, 60]);
        }
        let t = fy / 300.0;
        image::Rgb([(70.0 + 120.0 * t) as u8, (130.0 + 80.0 * t) as u8, (220.0 - 20.0 * t) as u8])
    })
    .save(path)
    .unwrap();
}

/// Motion controllers on the window (edge reveal, plain `borrow_mut`)
/// and on the picture (zoom pointer).
fn motion_controllers(state: &Rc<RefCell<ViewerState>>) -> Vec<gtk::EventControllerMotion> {
    let (window, scroller) = {
        let s = state.borrow();
        (s.window(), s.scroller.clone())
    };
    let mut found = Vec::new();
    for widget in [window.upcast::<gtk::Widget>(), scroller.upcast()] {
        let controllers = widget.observe_controllers();
        found.extend(
            (0..controllers.n_items())
                .filter_map(|i| controllers.item(i).and_downcast::<gtk::EventControllerMotion>())
                .filter(|c| c.name().is_some_and(|name| name.starts_with("bigiris-"))),
        );
    }
    assert_eq!(found.len(), 2, "pointer controllers not found");
    found
}

/// Replay pointer motion from every show/hide of the panel, like a real
/// pointer over the window would, to prove no handler re-enters a
/// borrowed state.
fn replay_motion_on_reveal(state: &Rc<RefCell<ViewerState>>) {
    let revealer = state.borrow().edit_ui.revealer.clone();
    for controller in motion_controllers(state) {
        let again = controller.clone();
        revealer.connect_reveal_child_notify(move |_| {
            controller.emit_by_name::<()>("motion", &[&20.0f64, &20.0f64]);
        });
        revealer.connect_child_revealed_notify(move |_| {
            again.emit_by_name::<()>("motion", &[&20.0f64, &20.0f64]);
        });
    }
}

#[track_caller]
fn wait_frame(context: &glib::MainContext, state: &Rc<RefCell<ViewerState>>, natural: (u32, u32)) {
    wait(context, &format!("frame {natural:?}"), &|| {
        let s = state.borrow();
        s.current_ready && s.natural == natural && !s.edit.has_preview()
    });
}

pub(super) fn open_viewer(
    id: &str,
    files: Vec<PathBuf>,
    size: (i32, i32),
) -> (adw::ApplicationWindow, Rc<RefCell<ViewerState>>, std::rc::Weak<RefCell<ViewerState>>) {
    assert_eq!(
        std::env::var("GSETTINGS_BACKEND").as_deref(),
        Ok("memory"),
        "synthetic GUI tests require GSETTINGS_BACKEND=memory"
    );
    adw::init().expect("GTK display required");
    install_theater_css();
    let app = adw::Application::builder()
        .application_id(id)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gio::Cancellable>).unwrap();
    let (window, witness) = build_window_tracked(&app, Rc::new(files), 0);
    window.set_default_size(size.0, size.1);
    window.present();
    let state = witness.upgrade().unwrap();
    let context = glib::MainContext::default();
    wait(&context, "first frame", &|| state.borrow().current_ready);
    settle(&context, 200);
    replay_motion_on_reveal(&state);
    (window, state, witness)
}

#[test]
#[ignore = "requires a graphical display; run alone with GSETTINGS_BACKEND=memory"]
fn viewer_edit_panel() {
    let context = glib::MainContext::default();
    let dir = tempfile::tempdir().unwrap();
    let photo = dir.path().join("a.png");
    landscape(&photo);
    let original_bytes = std::fs::read(&photo).unwrap();
    let other = dir.path().join("c.png");
    image::RgbImage::from_pixel(64, 48, image::Rgb([9, 9, 9])).save(&other).unwrap();
    let (window, state, witness) =
        open_viewer("com.biglinux.Iris.EditPanelProbe", vec![photo.clone(), other], (1000, 700));
    assert_eq!(state.borrow().natural, (640, 480));
    let revealer = state.borrow().edit_ui.revealer.clone();
    let panel = state.borrow().edit_ui.content();

    // The header button opens it.
    let header_button = find(&window, &|w| {
        w.downcast_ref::<gtk::ToggleButton>()
            .is_some_and(|b| b.action_name().as_deref() == Some("win.edit-panel"))
    })
    .and_downcast::<gtk::ToggleButton>()
    .expect("Edit button");
    assert_eq!(header_button.tooltip_text().as_deref(), Some(gettext("Edit (E)").as_str()));
    header_button.emit_clicked();
    settle(&context, 400);
    assert!(state.borrow().edit.is_shown(), "button did not open the panel");
    assert!(revealer.reveals_child() && header_button.is_active());
    // The picture is re-fitted beside the panel, not hidden under it.
    let reserved = super::edit_panel::reserved_width(&state.borrow());
    assert!(reserved >= 300, "no room reserved: {reserved}");
    let picture = state.borrow().picture.clone();
    let bounds = picture.compute_bounds(&window).unwrap();
    let panel_left = revealer.compute_bounds(&window).unwrap().x();
    assert!(
        bounds.x() + bounds.width() <= panel_left + 1.0,
        "picture under the panel: {bounds:?} vs {panel_left}"
    );
    // The whole panel stays inside the window.
    let panel_bounds = revealer.compute_bounds(&window).unwrap();
    assert!(panel_bounds.y() + panel_bounds.height() <= window.height() as f32);
    crate::gui::lifecycle_tests::capture_window(&window, "panel-1-open-1000x700");

    // E toggles, Esc closes it before closing the window.
    handle_key(&state, gdk::Key::e, gdk::ModifierType::empty());
    settle(&context, 300);
    assert!(!state.borrow().edit.is_shown() && !header_button.is_active());
    assert_eq!(super::edit_panel::reserved_width(&state.borrow()), 0);
    handle_key(&state, gdk::Key::E, gdk::ModifierType::empty());
    settle(&context, 300);
    assert!(state.borrow().edit.is_shown());
    handle_key(&state, gdk::Key::Escape, gdk::ModifierType::empty());
    settle(&context, 300);
    assert!(!state.borrow().edit.is_shown(), "Esc did not close the panel");
    assert!(window.is_visible(), "Esc closed the window with the panel open");
    handle_key(&state, gdk::Key::e, gdk::ModifierType::empty());
    settle(&context, 300);

    // Transform buttons are the menu actions.
    action_button(&panel, "win.rotate-cw").emit_clicked();
    wait_frame(&context, &state, (480, 640));
    action_button(&panel, "win.rotate-ccw").emit_clicked();
    wait_frame(&context, &state, (640, 480));
    action_button(&panel, "win.flip-h").emit_clicked();
    wait_frame(&context, &state, (640, 480));
    // The gray patch moved to the right edge.
    assert_eq!(texture_pixel(&state, 639 - 30, 430), [100, 100, 100, 255]);
    action_button(&panel, "win.flip-v").emit_clicked();
    wait(&context, "flip-v", &|| state.borrow().pending_ops.len() == 4);
    wait_frame(&context, &state, (640, 480));
    assert_eq!(texture_pixel(&state, 639 - 30, 479 - 430), [100, 100, 100, 255]);
    assert_eq!(
        state.borrow().pending_ops,
        vec![EditOp::RotateCw, EditOp::RotateCcw, EditOp::FlipH, EditOp::FlipV]
    );
    for _ in 0..4 {
        undo_edit(&state);
    }
    wait(&context, "all undone", &|| state.borrow().pending_ops.is_empty());
    wait_frame(&context, &state, (640, 480));
    assert_eq!(texture_pixel(&state, 30, 430), [100, 100, 100, 255]);

    // Crop enters the inline crop and hides the panel until it ends.
    action_button(&panel, "win.crop").emit_clicked();
    settle(&context, 300);
    assert!(state.borrow().crop.is_some());
    assert!(!state.borrow().edit.is_shown() && !revealer.reveals_child());
    assert_eq!(super::edit_panel::reserved_width(&state.borrow()), 0);
    assert!(!action_enabled(&window, "edit-panel"), "panel toggle live while cropping");
    handle_key(&state, gdk::Key::Escape, gdk::ModifierType::empty());
    settle(&context, 300);
    assert!(state.borrow().crop.is_none() && state.borrow().edit.is_shown());
    assert_eq!(super::edit_panel::reserved_width(&state.borrow()), reserved);

    // Sliders: a burst of changes previews live and commits ONE edit.
    let brightness = state.borrow().edit_ui.scale(0);
    for value in [10.0, 25.0, 40.0, 50.0] {
        brightness.set_value(value);
        settle(&context, 20);
    }
    assert!(state.borrow().edit.has_preview(), "no live preview while moving");
    assert!(state.borrow().pending_ops.is_empty(), "committed before the gesture ended");
    wait(&context, "adjust committed", &|| state.borrow().pending_ops.len() == 1);
    wait_frame(&context, &state, (640, 480));
    assert_eq!(
        state.borrow().pending_ops,
        vec![EditOp::Adjust { brightness: 50, contrast: 0, saturation: 0, gamma_pct: 100 }]
    );
    assert_eq!(texture_pixel(&state, 30, 430), [150, 150, 150, 255]);
    assert!(state.borrow().title.title().starts_with('•'));
    crate::gui::lifecycle_tests::capture_window(&window, "panel-2-adjusted-1000x700");

    // A held pointer never commits mid-drag; the release does, once.
    state.borrow_mut().edit.set_dragging(true);
    let contrast = state.borrow().edit_ui.scale(1);
    for value in [5.0, 15.0, 30.0] {
        contrast.set_value(value);
        settle(&context, 120);
    }
    settle(&context, 300);
    assert_eq!(state.borrow().pending_ops.len(), 1, "committed while the slider was held");
    super::edit_panel::flush_commit(&state);
    wait(&context, "drag committed", &|| state.borrow().pending_ops.len() == 2);
    wait_frame(&context, &state, (640, 480));

    // Undo reverts the whole gesture and the sliders follow the history.
    undo_edit(&state);
    wait_frame(&context, &state, (640, 480));
    let values = state.borrow().edit_ui.slider_values();
    assert_eq!((values.brightness, values.contrast), (50, 0));
    assert_eq!(texture_pixel(&state, 30, 430), [150, 150, 150, 255]);
    redo_edit(&state);
    wait_frame(&context, &state, (640, 480));
    assert_eq!(state.borrow().edit_ui.slider_values().contrast, 30);
    // "Reset adjustments" is one more undoable step back to neutral.
    labeled_button(&panel, &gettext("Reset adjustments")).emit_clicked();
    wait(&context, "reset committed", &|| state.borrow().pending_ops.len() == 3);
    wait_frame(&context, &state, (640, 480));
    assert_eq!(texture_pixel(&state, 30, 430), [100, 100, 100, 255]);
    for _ in 0..3 {
        undo_edit(&state);
    }
    wait(&context, "adjustments undone", &|| state.borrow().pending_ops.is_empty());
    wait_frame(&context, &state, (640, 480));
    assert_eq!(state.borrow().edit_ui.slider_values(), super::edits::AdjustValues::NEUTRAL);
    // A reset pressed from the keyboard turns insensitive and hands the
    // focus to its slider instead of dropping it.
    brightness.set_value(20.0);
    wait(&context, "brightness committed", &|| state.borrow().pending_ops.len() == 1);
    let reset = state.borrow().edit_ui.reset(0);
    assert!(reset.is_sensitive());
    assert!(reset.grab_focus(), "reset button not focusable");
    reset.emit_clicked();
    assert!(!reset.is_sensitive());
    assert!(brightness.has_focus(), "focus lost after the reset");
    wait(&context, "reset committed", &|| state.borrow().pending_ops.len() == 2);
    undo_edit(&state);
    undo_edit(&state);
    wait(&context, "reset undone", &|| state.borrow().pending_ops.is_empty());
    wait_frame(&context, &state, (640, 480));

    // Resize: the lock keeps the proportion; unlocked sides are free.
    let (width, height, lock, percent, apply) = state.borrow().edit_ui.size_controls();
    assert_eq!((width.value(), height.value()), (640.0, 480.0));
    width.set_value(320.0);
    assert_eq!(height.value(), 240.0, "lock did not follow");
    apply.emit_clicked();
    wait_frame(&context, &state, (320, 240));
    assert_eq!(state.borrow().pending_ops, vec![EditOp::Resize { w: 320, h: 240 }]);
    assert_eq!((width.value(), height.value()), (320.0, 240.0), "fields not refreshed");
    percent.set_value(50.0);
    assert_eq!((width.value(), height.value()), (160.0, 120.0));
    lock.set_active(false);
    height.set_value(100.0);
    assert_eq!(width.value(), 160.0);
    apply.emit_clicked();
    wait_frame(&context, &state, (160, 100));
    undo_edit(&state);
    wait_frame(&context, &state, (320, 240));
    undo_edit(&state);
    wait_frame(&context, &state, (640, 480));
    lock.set_active(true);

    // Ver original: the button and a held backslash show the unedited file.
    apply_edit(&state, EditOp::FlipH);
    wait(&context, "flipped", &|| state.borrow().pending_ops.len() == 1);
    wait_frame(&context, &state, (640, 480));
    let shown = || picture.paintable().and_downcast::<gdk::Texture>();
    let edited = state.borrow().texture.clone().unwrap();
    assert_eq!(shown().as_ref(), Some(&edited));
    let original = state.borrow().edit_ui.original_button();
    original.set_active(true);
    wait(&context, "original shown", &|| shown().is_some_and(|t| t != edited));
    assert_eq!(pixel(&shown().unwrap(), 30, 430), [100, 100, 100, 255]);
    original.set_active(false);
    settle(&context, 50);
    assert_eq!(shown().as_ref(), Some(&edited));
    handle_key(&state, gdk::Key::backslash, gdk::ModifierType::empty());
    settle(&context, 50);
    assert_eq!(pixel(&shown().unwrap(), 30, 430), [100, 100, 100, 255]);
    let window_keys = window.observe_controllers();
    for controller in (0..window_keys.n_items())
        .filter_map(|i| window_keys.item(i).and_downcast::<gtk::EventControllerKey>())
    {
        controller.emit_by_name::<()>(
            "key-released",
            &[
                &glib::translate::IntoGlib::into_glib(gdk::Key::backslash),
                &0u32,
                &gdk::ModifierType::empty(),
            ],
        );
    }
    settle(&context, 50);
    assert_eq!(shown().as_ref(), Some(&edited), "original stayed after releasing \\");

    // Save as: a new file with the edits; the original is untouched
    // and the copy joins the folder list.
    apply_edit(&state, EditOp::Resize { w: 320, h: 240 });
    wait_frame(&context, &state, (320, 240));
    super::edit_panel::save_copy(&state, photo.clone());
    settle(&context, 100);
    assert!(!state.borrow().saving, "save-as over the open file was not refused");
    let copy = dir.path().join("b-copy.png");
    super::edit_panel::save_copy(&state, copy.clone());
    assert!(!panel.is_sensitive(), "panel stays usable while saving");
    wait(&context, "save-as done", &|| !state.borrow().saving);
    assert!(panel.is_sensitive());
    assert_eq!(std::fs::read(&photo).unwrap(), original_bytes, "original changed");
    let saved = image::open(&copy).unwrap().to_rgb8();
    assert_eq!(saved.dimensions(), (320, 240));
    // Flipped: the gray patch is on the right.
    assert_eq!(saved.get_pixel(319 - 15, 215).0, [100, 100, 100]);
    {
        let s = state.borrow();
        assert_eq!(s.files.len(), 3);
        assert_eq!(s.files[s.idx], photo, "current image changed");
        assert!(s.files.contains(&copy));
        assert_eq!(s.pending_ops.len(), 2, "save-as dropped the pending edits");
    }
    crate::gui::lifecycle_tests::capture_window(&window, "panel-3-after-save-as");

    // Other window sizes: room is reserved where it fits, the panel
    // floats over the picture in a narrow window.
    for (w, h, floats) in [(1280, 800, false), (700, 600, true)] {
        window.set_default_size(w, h);
        wait(&context, "resized", &|| window.width() == w);
        settle(&context, 300);
        let reserved = super::edit_panel::reserved_width(&state.borrow());
        assert_eq!(reserved == 0, floats, "{w}x{h}: reserved {reserved}");
        crate::gui::lifecycle_tests::capture_window(&window, &format!("panel-4-{w}x{h}"));
    }

    window.destroy();
    drop(window);
    settle(&context, 100);
    drop(state);
    wait(&context, "viewer state released", &|| witness.upgrade().is_none());
}

#[test]
#[ignore = "requires a graphical display; run alone with GSETTINGS_BACKEND=memory"]
fn viewer_edit_panel_tools() {
    let context = glib::MainContext::default();
    let dir = tempfile::tempdir().unwrap();
    let photo = dir.path().join("a.png");
    landscape(&photo);
    let (window, state, witness) =
        open_viewer("com.biglinux.Iris.EditToolsProbe", vec![photo.clone()], (1000, 700));
    handle_key(&state, gdk::Key::e, gdk::ModifierType::empty());
    settle(&context, 300);
    assert!(state.borrow().edit.is_shown());
    let grid = state.borrow().edit_ui.grid.clone();

    // Straighten: live preview with an alignment grid, then one edit that
    // turns and crops to the largest rectangle inside: no empty corners.
    let straighten = state.borrow().edit_ui.straighten_scale();
    // Held like a pointer drag, so the preview stays until the release.
    state.borrow_mut().edit.set_dragging(true);
    for value in [1.0, 3.0, 5.0] {
        straighten.set_value(value);
        settle(&context, 30);
    }
    assert!(state.borrow().edit.has_preview(), "no straighten preview");
    assert!(grid.is_visible(), "no grid while straightening");
    crate::gui::lifecycle_tests::capture_window(&window, "panel-5-straighten-grid");
    assert!(state.borrow().pending_ops.is_empty(), "committed while held");
    super::edit_panel::flush_commit(&state);
    wait(&context, "straighten committed", &|| state.borrow().pending_ops.len() == 1);
    assert_eq!(state.borrow().pending_ops, vec![EditOp::Straighten { tenths: 50 }]);
    let expected = bigimage_core::rotate::straightened_size(640, 480, 5.0);
    assert!(expected.0 < 640 && expected.1 < 480);
    wait_frame(&context, &state, expected);
    assert!(!grid.is_visible(), "grid stayed after the commit");
    let (w, h) = expected;
    for (x, y) in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)] {
        let [r, g, b, a] = texture_pixel(&state, x, y);
        assert_eq!(a, 255, "transparent corner at {x},{y}");
        assert!(u32::from(r) + u32::from(g) + u32::from(b) > 100, "dark corner at {x},{y}");
    }
    crate::gui::lifecycle_tests::capture_window(&window, "panel-6-straightened");
    // A second gesture replaces the angle instead of turning further.
    straighten.set_value(-2.0);
    wait(&context, "second angle", &|| state.borrow().pending_ops.len() == 2);
    wait_frame(&context, &state, bigimage_core::rotate::straightened_size(640, 480, 2.0));
    undo_edit(&state);
    wait_frame(&context, &state, expected);
    assert_eq!(straighten.value(), 5.0, "slider did not follow Undo");
    undo_edit(&state);
    wait_frame(&context, &state, (640, 480));
    assert_eq!(straighten.value(), 0.0);

    // Upscale runs inline with progress and becomes an edit.
    let (remove_bg, upscale, factor, tool_box, status) = state.borrow().edit_ui.tool_controls();
    factor.set_selected(0);
    // Controls that would do nothing are off: neutral sliders, the
    // image's own size.
    let content = state.borrow().edit_ui.content();
    let reset_all = labeled_button(&content, &gettext("Reset adjustments"));
    let (width, _, lock, percent, resize_apply) = state.borrow().edit_ui.size_controls();
    assert!(!reset_all.is_sensitive() && !resize_apply.is_sensitive());
    width.set_value(320.0);
    assert!(resize_apply.is_sensitive(), "Apply off for another size");
    width.set_value(640.0);
    assert!(!resize_apply.is_sensitive());
    let runs = heavy_runs();
    upscale.emit_clicked();
    assert!(tool_box.is_visible(), "no progress while upscaling");
    for widget in [lock.upcast_ref::<gtk::Widget>(), percent.upcast_ref(), factor.upcast_ref()] {
        assert!(!widget.is_sensitive(), "{} usable during a tool run", widget.type_().name());
    }
    assert!(!action_enabled(&window, "rotate-cw"), "edits live during a tool run");
    // Real-ESRGAN when its weights are there, else Lanczos3; recorded.
    let ai = bigimage_ai::upscale::availability().is_ready();
    wait(&context, "upscaled", &|| {
        state.borrow().pending_ops == [EditOp::Upscale { factor: 2, ai }]
    });
    wait_frame(&context, &state, (1280, 960));
    assert!(!tool_box.is_visible());
    assert_eq!(heavy_runs(), runs + 1);
    // Later edits, Undo and Redo reuse the cached result.
    gtk::prelude::WidgetExt::activate_action(&window, "win.rotate-cw", None).unwrap();
    wait_frame(&context, &state, (960, 1280));
    let brightness = state.borrow().edit_ui.scale(0);
    brightness.set_value(20.0);
    wait(&context, "adjusted", &|| state.borrow().pending_ops.len() == 3);
    wait_frame(&context, &state, (960, 1280));
    undo_edit(&state);
    wait_frame(&context, &state, (960, 1280));
    undo_edit(&state);
    wait_frame(&context, &state, (1280, 960));
    redo_edit(&state);
    wait_frame(&context, &state, (960, 1280));
    assert_eq!(heavy_runs(), runs + 1, "the upscale ran again");

    // Cancel gives the panel back at once and drops the result.
    upscale.emit_clicked();
    assert!(tool_box.is_visible());
    let cancel = find(&tool_box, &|w| w.is::<gtk::Button>()).and_downcast::<gtk::Button>().unwrap();
    cancel.emit_clicked();
    assert!(!tool_box.is_visible() && status.is_visible());
    assert!(action_enabled(&window, "rotate-cw"));
    settle(&context, 800);
    assert_eq!(state.borrow().pending_ops.len(), 2, "a canceled tool still applied");
    // The lower sections, scrolled into view.
    let scroller = find(&state.borrow().edit_ui.revealer, &|w| w.is::<gtk::ScrolledWindow>())
        .and_downcast::<gtk::ScrolledWindow>()
        .unwrap();
    let adjustment = scroller.vadjustment();
    adjustment.set_value(adjustment.upper());
    crate::gui::lifecycle_tests::capture_window(&window, "panel-7-tools-and-save");
    adjustment.set_value(0.0);
    undo_edit(&state);
    undo_edit(&state);
    wait_frame(&context, &state, (640, 480));

    // Remove background / Remove noise: without their packages the buttons
    // are off and the panel says which package to install.
    let (denoise, note) = state.borrow().edit_ui.ai_controls();
    for (button, model, op) in [
        (&remove_bg, &bigimage_ai::background::MODEL, EditOp::RemoveBackground),
        (&denoise, &bigimage_ai::denoise::MODEL, EditOp::Denoise),
    ] {
        if model.availability().is_ready() {
            assert!(button.is_sensitive(), "{} off with its weights", model.name);
            continue;
        }
        assert!(!button.is_sensitive(), "{} on without its weights", model.name);
        let hint = if bigimage_ai::engines_compiled() {
            format!("sudo pacman -S {}", model.package)
        } else {
            "--features ai".to_owned()
        };
        assert!(note.is_visible() && note.text().contains(&hint), "note: {}", note.text());
        super::edit_panel::run_tool(&state, op);
        assert!(status.text().contains(&hint), "no guidance: {}", status.text());
        assert!(state.borrow().pending_ops.is_empty());
    }
    // The real models run in `viewer_edit_panel_ai`.

    window.destroy();
    drop(window);
    settle(&context, 100);
    drop(state);
    wait(&context, "viewer state released", &|| witness.upgrade().is_none());
}

/// A heavy edit missing from the cache is rendered again with progress,
/// and Esc or Cancel returns to the history that was on screen.
#[test]
#[ignore = "requires a graphical display; run alone with GSETTINGS_BACKEND=memory"]
fn viewer_heavy_render_cancel() {
    let context = glib::MainContext::default();
    let dir = tempfile::tempdir().unwrap();
    let photo = dir.path().join("a.png");
    landscape(&photo);
    let (window, state, witness) =
        open_viewer("com.biglinux.Iris.HeavyRenderProbe", vec![photo], (1000, 700));
    // No budget: only the newest entry and the latest heavy result stay.
    *state.borrow().loader.cache.lock().unwrap() = super::edits::EditCache::with_budget(0);
    let bar = state.borrow().heavy_render.clone();
    let up = EditOp::Upscale { factor: 2, ai: false };
    let runs = heavy_runs();
    apply_edit(&state, up);
    assert!(bar.is_visible(), "no progress while the decode thread upscales");
    wait_frame(&context, &state, (1280, 960));
    assert!(!bar.is_visible());
    apply_edit(&state, up);
    wait_frame(&context, &state, (2560, 1920));
    assert_eq!(heavy_runs(), runs + 2);
    undo_edit(&state);
    assert!(bar.is_visible(), "the evicted first upscale is not announced");
    {
        // Holding the cache keeps the decode thread from finishing.
        let cache = state.borrow().loader.cache.clone();
        let _held = cache.lock().unwrap();
        crate::gui::lifecycle_tests::capture_window(&window, "heavy-render-progress");
        assert!(bar.is_visible());
        // The texture on screen is the previous edit: copying it is refused.
        let osd = state.borrow().osd_label.clone();
        osd.set_text("");
        copy_current_to_clipboard(&state);
        assert_eq!(osd.text(), gettext("Wait for the image to finish loading"));
    }
    assert_eq!(
        handle_key(&state, gdk::Key::Escape, gdk::ModifierType::empty()),
        glib::Propagation::Stop
    );
    assert!(!bar.is_visible());
    assert_eq!(state.borrow().pending_ops, [up, up]);
    assert!(state.borrow().redo_ops.is_empty());
    wait_frame(&context, &state, (2560, 1920));
    assert!(window.is_visible(), "Esc closed the window instead of canceling");
    // The button does the same.
    undo_edit(&state);
    assert!(bar.is_visible());
    labeled_button(&bar, &gettext("Cancel")).emit_clicked();
    assert_eq!(state.borrow().pending_ops, [up, up]);
    wait_frame(&context, &state, (2560, 1920));
    // Undoing past both upscales needs only the decoded file.
    undo_edit(&state);
    undo_edit(&state);
    assert!(!bar.is_visible(), "undo to the decoded file shown as a heavy render");
    wait_frame(&context, &state, (640, 480));

    window.destroy();
    drop(window);
    settle(&context, 100);
    drop(state);
    wait(&context, "viewer state released", &|| witness.upgrade().is_none());
}

/// The panel's AI tools with the real models. Needs the BigLinux weights
/// packages, or `BIGIRIS_{BIREFNET,REALESRGAN,SCUNET}_WEIGHTS`; a tool
/// whose weights are missing is skipped with a message.
#[test]
#[ignore = "requires a graphical display and the AI weights; run alone with GSETTINGS_BACKEND=memory"]
fn viewer_edit_panel_ai() {
    let context = glib::MainContext::default();
    let dir = tempfile::tempdir().unwrap();
    let photo = dir.path().join("a.png");
    landscape(&photo);
    let (window, state, witness) =
        open_viewer("com.biglinux.Iris.EditAiProbe", vec![photo.clone()], (1000, 700));
    handle_key(&state, gdk::Key::e, gdk::ModifierType::empty());
    settle(&context, 300);
    let (remove_bg, upscale, factor, tool_box, status) = state.borrow().edit_ui.tool_controls();
    let (denoise, note) = state.borrow().edit_ui.ai_controls();
    let ready = |model: &bigimage_ai::ModelInfo| {
        let ready = model.availability().is_ready();
        if !ready {
            eprintln!("skipping {}: weights absent (set {})", model.name, model.env);
        }
        ready
    };
    let mut expected: Vec<EditOp> = Vec::new();
    let mut runs = heavy_runs();
    let run =
        |button: &gtk::Button, op: EditOp, natural: (u32, u32), expected: &mut Vec<EditOp>| {
            status.set_visible(false);
            let started = std::time::Instant::now();
            button.emit_clicked();
            assert!(tool_box.is_visible(), "no progress for {op:?}");
            let mut want = expected.clone();
            want.push(op);
            wait(&context, &format!("{op:?} applied"), &|| {
                assert!(!status.is_visible(), "{op:?} failed: {}", status.text());
                state.borrow().pending_ops == want
            });
            wait_frame(&context, &state, natural);
            eprintln!("{op:?}: {:.2} s in the panel", started.elapsed().as_secs_f64());
            *expected = want;
        };

    let mut natural = (640, 480);
    if ready(&bigimage_ai::background::MODEL) {
        run(&remove_bg, EditOp::RemoveBackground, natural, &mut expected);
        runs += 1;
        assert_eq!(heavy_runs(), runs);
        // The cut-out has transparent pixels; the device shows in the tooltip.
        let rgba = super::edit_panel::texture_rgba(&state.borrow().texture.clone().unwrap());
        assert!(rgba.pixels().any(|p| p[3] < 128), "nothing was cut out");
        let tooltip = remove_bg.tooltip_text().unwrap_or_default();
        assert!(tooltip.contains("CPU") || tooltip.contains("GPU"), "tooltip: {tooltip}");
        crate::gui::lifecycle_tests::capture_window(&window, "panel-ai-1-remove-bg");
    }
    if ready(&bigimage_ai::upscale::MODEL) {
        factor.set_selected(0);
        natural = (1280, 960);
        run(&upscale, EditOp::Upscale { factor: 2, ai: true }, natural, &mut expected);
        runs += 1;
        assert_eq!(heavy_runs(), runs);
        crate::gui::lifecycle_tests::capture_window(&window, "panel-ai-2-upscale");
    }
    if ready(&bigimage_ai::denoise::MODEL) {
        run(&denoise, EditOp::Denoise, natural, &mut expected);
        runs += 1;
        assert_eq!(heavy_runs(), runs);
        let tooltip = denoise.tooltip_text().unwrap_or_default();
        assert!(tooltip.contains("CPU") || tooltip.contains("GPU"), "tooltip: {tooltip}");
    }
    // With every panel model installed there is nothing to install.
    if expected.len() == 3 {
        assert!(!note.is_visible(), "install note with every model present: {}", note.text());
    } else {
        assert!(note.is_visible(), "no install note with a model missing");
    }
    let scroller = find(&state.borrow().edit_ui.revealer, &|w| w.is::<gtk::ScrolledWindow>())
        .and_downcast::<gtk::ScrolledWindow>()
        .unwrap();
    let adjustment = scroller.vadjustment();
    adjustment.set_value(adjustment.upper());
    settle(&context, 100);
    crate::gui::lifecycle_tests::capture_window(&window, "panel-ai-3-tools");
    if expected.is_empty() {
        eprintln!("no AI weights available: nothing ran");
    } else {
        // Later edits, Undo and Redo reuse the cached results.
        gtk::prelude::WidgetExt::activate_action(&window, "win.flip-h", None).unwrap();
        wait(&context, "flipped", &|| state.borrow().pending_ops.len() == expected.len() + 1);
        wait_frame(&context, &state, natural);
        undo_edit(&state);
        wait_frame(&context, &state, natural);
        redo_edit(&state);
        wait_frame(&context, &state, natural);
        assert_eq!(heavy_runs(), runs, "a heavy tool ran again on flip/undo/redo");
        // Save as recomputes from the file's own pixels (metadata and
        // color profile kept): every heavy tool once more, same engines.
        let copy = dir.path().join("a-copy.png");
        super::edit_panel::save_copy(&state, copy.clone());
        // Every heavy tool again: minutes for SCUNet on a slow CPU.
        let deadline = std::time::Instant::now() + Duration::from_secs(600);
        while state.borrow().saving {
            assert!(std::time::Instant::now() < deadline, "timed out: save-as");
            settle(&context, 50);
        }
        let saved = image::open(&copy).unwrap();
        assert_eq!((saved.width(), saved.height()), natural);
        assert_eq!(heavy_runs(), runs + expected.len(), "save-as skipped a tool");
        if expected.contains(&EditOp::RemoveBackground) {
            assert!(saved.to_rgba8().pixels().any(|p| p[3] < 128), "save lost the cut-out");
        }
        // Cancelling a running tool drops its result.
        if bigimage_ai::upscale::availability().is_ready() {
            let before = state.borrow().pending_ops.clone();
            factor.set_selected(2);
            upscale.emit_clicked();
            assert!(tool_box.is_visible());
            super::edit_panel::cancel_tool(&state);
            settle(&context, 1500);
            assert_eq!(state.borrow().pending_ops, before, "a canceled upscale applied");
        }
        // Closing the panel gives the models back.
        handle_key(&state, gdk::Key::e, gdk::ModifierType::empty());
        let unloaded = || {
            use bigimage_ai::{background, denoise, inpaint, upscale};
            [background::device(), upscale::device(), denoise::device(), inpaint::device()]
                .iter()
                .all(Option::is_none)
        };
        wait(&context, "models unloaded", &unloaded);
    }
    window.destroy();
    drop(window);
    settle(&context, 100);
    drop(state);
    wait(&context, "viewer state released", &|| witness.upgrade().is_none());
}
