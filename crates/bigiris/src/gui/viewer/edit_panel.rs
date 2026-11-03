// SPDX-License-Identifier: GPL-3.0-or-later
//! Editing side panel.
//!
//! The "Editar" header button (or `E`) slides a translucent panel in from
//! the right with every edit in one place: rotate, flip and crop, tone
//! sliders, resize and save. Everything it does is an in-memory
//! [`EditOp`], so Undo/Redo, `Ctrl+S` and the `•` in the title keep
//! working exactly as with the menu entries.
//!
//! Sliders preview live on a downscaled working copy (at most
//! [`PREVIEW_MAX_PIXELS`]) shown in the main picture; a finished gesture
//! commits one `EditOp::Adjust` and the full-resolution result comes from
//! the regular edit pipeline.
//!
//! Showing, hiding or revealing widgets emits crossing events
//! synchronously and their handlers borrow the state: no function here
//! changes visibility while a borrow is held, and every widget handler
//! uses `try_borrow`.

use super::super::*;
use super::edits::{self, AdjustValues};

/// Panel width in logical pixels.
const PANEL_WIDTH: i32 = 300;
/// Gap between the panel and the window edges.
const PANEL_MARGIN: i32 = 12;
/// Narrower windows let the panel float over the picture instead of
/// shrinking the picture to a sliver.
const OVERLAY_BELOW_WIDTH: i32 = 760;
/// Largest working copy the sliders preview on.
const PREVIEW_MAX_PIXELS: u64 = 2_000_000;
/// Quiet time after the last keyboard or wheel change before the slider
/// value is committed as an edit. Pointer drags commit on release.
const COMMIT_IDLE_MS: u64 = 150;

/// Encoding goal offered by "Salvar como…".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::gui) enum SaveQuality {
    Max,
    Balanced,
    Smallest,
}

const SAVE_QUALITIES: [(SaveQuality, &str); 3] = [
    (SaveQuality::Max, n_("Máxima qualidade")),
    (SaveQuality::Balanced, n_("Equilibrado")),
    (SaveQuality::Smallest, n_("Menor arquivo")),
];

/// Encoder options for "Salvar como…".
///
/// TODO(coordinator): map `quality` onto the perceptual `QualityTarget`
/// of `EncodeOptions` once it lands (Máxima qualidade / Equilibrado /
/// Menor arquivo); until then every choice uses the defaults.
pub(in crate::gui) fn save_as_options(format: Format, quality: SaveQuality) -> EncodeOptions {
    let _ = (format, quality);
    EncodeOptions::default()
}

/// One tone slider and its widgets.
struct Slider {
    scale: gtk::Scale,
    value: gtk::Label,
    reset: gtk::Button,
}

/// Widgets of the panel, built hidden with the viewer.
pub(in crate::gui) struct EditPanelUi {
    pub revealer: gtk::Revealer,
    content: gtk::Box,
    original: gtk::ToggleButton,
    sliders: [Slider; 4],
    reset_all: gtk::Button,
    width: gtk::SpinButton,
    height: gtk::SpinButton,
    lock: gtk::ToggleButton,
    percent: gtk::SpinButton,
    resize_apply: gtk::Button,
    save_as: gtk::Button,
    quality: gtk::DropDown,
    /// Set while code (not the user) moves a control, so its handler
    /// does not take the change for user input.
    syncing: Rc<Cell<bool>>,
}

/// Image the sliders preview on.
struct PreviewBase {
    path: PathBuf,
    ops: Vec<EditOp>,
    image: Rc<image::RgbaImage>,
}

/// Unedited pixels of the current file, for "Ver original".
struct Original {
    path: PathBuf,
    texture: gdk::Texture,
    natural: (u32, u32),
}

/// Panel state kept in the viewer.
#[derive(Default)]
pub(in crate::gui) struct EditPanelState {
    open: bool,
    /// Hidden while the crop mode owns the picture.
    suspended: bool,
    /// Values on the sliders right now.
    adjust: Option<AdjustValues>,
    /// A pointer button is down on a slider.
    dragging: bool,
    /// A slider gesture is waiting to become an edit.
    commit_pending: bool,
    commit_source: Option<glib::SourceId>,
    preview_scheduled: bool,
    base: Option<PreviewBase>,
    /// Key of the working copy being prepared off the GTK thread.
    base_job: Option<(PathBuf, Vec<EditOp>)>,
    /// Adjusted working copy on screen instead of the full image.
    preview: Option<gdk::Texture>,
    original: Option<Original>,
    original_job: Option<PathBuf>,
    original_button: bool,
    original_key: bool,
    /// File and edits the controls were last synchronised with.
    synced: Option<(PathBuf, Vec<EditOp>)>,
    synced_natural: (u32, u32),
}

fn section(title: &str) -> gtk::Box {
    let section = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let label = gtk::Label::builder().label(title).xalign(0.0).build();
    label.add_css_class("edit-section-title");
    section.append(&label);
    section
}

fn icon_button(icon: &str, tooltip: &str, action: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.set_tooltip_text(Some(tooltip));
    button.set_action_name(Some(action));
    button.update_property(&[gtk::accessible::Property::Label(tooltip)]);
    button
}

fn slider(title: &str, min: f64, max: f64, step: f64, neutral: f64) -> (gtk::Box, Slider) {
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, step);
    scale.set_value(neutral);
    scale.set_draw_value(false);
    scale.set_hexpand(true);
    scale.add_mark(neutral, gtk::PositionType::Bottom, None);
    scale.update_property(&[gtk::accessible::Property::Label(title)]);
    let name = gtk::Label::builder().label(title).xalign(0.0).hexpand(true).build();
    let value = gtk::Label::builder().xalign(1.0).width_chars(5).build();
    value.add_css_class("numeric");
    value.add_css_class("dim-label");
    let reset = gtk::Button::from_icon_name("edit-undo-symbolic");
    reset.add_css_class("flat");
    reset.add_css_class("circular");
    reset.set_tooltip_text(Some(&gettext("Restaurar")));
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    head.append(&name);
    head.append(&value);
    head.append(&reset);
    let row = gtk::Box::new(gtk::Orientation::Vertical, 0);
    row.append(&head);
    row.append(&scale);
    (row, Slider { scale, value, reset })
}

fn size_spin(tooltip: &str) -> gtk::SpinButton {
    let spin = gtk::SpinButton::with_range(1.0, f64::from(edits::MAX_SIDE), 1.0);
    spin.set_numeric(true);
    spin.set_width_chars(5);
    spin.set_hexpand(true);
    spin.set_tooltip_text(Some(tooltip));
    spin.update_property(&[gtk::accessible::Property::Label(tooltip)]);
    spin
}

pub(in crate::gui) fn build() -> EditPanelUi {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(12)
        .margin_bottom(16)
        .margin_start(14)
        .margin_end(14)
        .build();

    let title = gtk::Label::builder().label(gettext("Editar")).xalign(0.0).hexpand(true).build();
    title.add_css_class("title-4");
    let close = gtk::Button::from_icon_name("window-close-symbolic");
    close.add_css_class("flat");
    close.add_css_class("circular");
    close.set_tooltip_text(Some(&gettext("Fechar (Esc)")));
    close.set_action_name(Some("win.edit-panel"));
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    head.append(&title);
    head.append(&close);

    let original = gtk::ToggleButton::with_label(&gettext("Ver original"));
    original.set_tooltip_text(Some(&gettext("Mostra a imagem sem as edições (segure \\)")));

    // Transformar
    let transform = section(&gettext("Transformar"));
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_homogeneous(true);
    for (icon, tooltip, action) in [
        ("object-rotate-left-symbolic", gettext("Girar à esquerda"), "win.rotate-ccw"),
        ("object-rotate-right-symbolic", gettext("Girar à direita"), "win.rotate-cw"),
        ("object-flip-horizontal-symbolic", gettext("Espelhar horizontal"), "win.flip-h"),
        ("object-flip-vertical-symbolic", gettext("Espelhar vertical"), "win.flip-v"),
    ] {
        buttons.append(&icon_button(icon, &tooltip, action));
    }
    transform.append(&buttons);
    let crop = gtk::Button::builder()
        .label(gettext("Recortar"))
        .tooltip_text(gettext("Recortar na própria imagem (Ctrl+R)"))
        .action_name("win.crop")
        .build();
    transform.append(&crop);

    // Ajustes
    let adjust = section(&gettext("Ajustes"));
    let (brightness_row, brightness) = slider(&gettext("Brilho"), -100.0, 100.0, 1.0, 0.0);
    let (contrast_row, contrast) = slider(&gettext("Contraste"), -100.0, 100.0, 1.0, 0.0);
    let (saturation_row, saturation) = slider(&gettext("Saturação"), -100.0, 100.0, 1.0, 0.0);
    let (gamma_row, gamma) = slider(&gettext("Gama"), 0.1, 3.0, 0.01, 1.0);
    gamma.scale.set_digits(2);
    for row in [&brightness_row, &contrast_row, &saturation_row, &gamma_row] {
        adjust.append(row);
    }
    let reset_all = gtk::Button::with_label(&gettext("Restaurar ajustes"));
    adjust.append(&reset_all);

    // Tamanho
    let size = section(&gettext("Tamanho"));
    let width = size_spin(&gettext("Largura (px)"));
    let height = size_spin(&gettext("Altura (px)"));
    let lock = gtk::ToggleButton::builder()
        .icon_name("changes-prevent-symbolic")
        .active(true)
        .tooltip_text(gettext("Manter proporção"))
        .build();
    lock.add_css_class("flat");
    let dims = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    dims.append(&width);
    dims.append(&lock);
    dims.append(&height);
    size.append(&dims);
    let percent = gtk::SpinButton::with_range(1.0, 1000.0, 1.0);
    percent.set_numeric(true);
    percent.set_value(100.0);
    percent.set_tooltip_text(Some(&gettext("Escala (%)")));
    percent.update_property(&[gtk::accessible::Property::Label(&gettext("Escala (%)"))]);
    let percent_label = gtk::Label::builder().label("%").build();
    let resize_apply = gtk::Button::with_label(&gettext("Aplicar"));
    resize_apply.add_css_class("suggested-action");
    resize_apply.set_hexpand(true);
    let scale_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    scale_row.append(&percent);
    scale_row.append(&percent_label);
    scale_row.append(&resize_apply);
    size.append(&scale_row);

    // Salvar
    let save = section(&gettext("Salvar"));
    let save_button = gtk::Button::builder()
        .label(gettext("Salvar"))
        .tooltip_text(gettext("Salvar alterações (Ctrl+S)"))
        .action_name("win.save-edits")
        .build();
    save_button.add_css_class("suggested-action");
    let save_as = gtk::Button::with_label(&gettext("Salvar como…"));
    let save_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    save_buttons.set_homogeneous(true);
    save_buttons.append(&save_button);
    save_buttons.append(&save_as);
    save.append(&save_buttons);
    let quality_names: Vec<&str> = SAVE_QUALITIES.iter().map(|(_, name)| *name).collect();
    let quality = gtk::DropDown::builder().model(&translated_strings(&quality_names)).build();
    quality.set_selected(1);
    quality.set_tooltip_text(Some(&gettext("Qualidade ao salvar como")));
    save.append(&quality);

    content.append(&head);
    content.append(&original);
    content.append(&transform);
    content.append(&adjust);
    content.append(&size);
    content.append(&save);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .child(&content)
        .build();
    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.add_css_class("osd");
    frame.add_css_class("edit-panel");
    frame.set_overflow(gtk::Overflow::Hidden);
    frame.set_width_request(PANEL_WIDTH);
    frame.append(&scroll);
    let revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideLeft)
        .transition_duration(200)
        .reveal_child(false)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Fill)
        .margin_top(PANEL_MARGIN)
        .margin_bottom(PANEL_MARGIN)
        .margin_end(PANEL_MARGIN)
        .child(&frame)
        .build();
    EditPanelUi {
        revealer,
        content,
        original,
        sliders: [brightness, contrast, saturation, gamma],
        reset_all,
        width,
        height,
        lock,
        percent,
        resize_apply,
        save_as,
        quality,
        syncing: Rc::new(Cell::new(false)),
    }
}

impl EditPanelUi {
    fn values(&self) -> AdjustValues {
        let [b, c, s, g] = &self.sliders;
        AdjustValues {
            brightness: b.scale.value().round() as i32,
            contrast: c.scale.value().round() as i32,
            saturation: s.scale.value().round() as i32,
            gamma_pct: (g.scale.value() * 100.0).round().max(1.0) as u32,
        }
    }

    fn show_values(&self, values: AdjustValues) {
        let [b, c, s, g] = &self.sliders;
        let signed = |v: i32| if v > 0 { format!("+{v}") } else { v.to_string() };
        b.value.set_text(&signed(values.brightness));
        c.value.set_text(&signed(values.contrast));
        s.value.set_text(&signed(values.saturation));
        g.value.set_text(&format!("{:.2}", f64::from(values.gamma_pct) / 100.0));
        let neutral = AdjustValues::NEUTRAL;
        b.reset.set_sensitive(values.brightness != neutral.brightness);
        c.reset.set_sensitive(values.contrast != neutral.contrast);
        s.reset.set_sensitive(values.saturation != neutral.saturation);
        g.reset.set_sensitive(values.gamma_pct != neutral.gamma_pct);
    }

    /// Move the sliders without treating it as user input.
    fn set_values(&self, values: AdjustValues) {
        self.syncing.set(true);
        let [b, c, s, g] = &self.sliders;
        b.scale.set_value(f64::from(values.brightness));
        c.scale.set_value(f64::from(values.contrast));
        s.scale.set_value(f64::from(values.saturation));
        g.scale.set_value(f64::from(values.gamma_pct) / 100.0);
        self.syncing.set(false);
        self.show_values(values);
    }

    fn set_size(&self, size: (u32, u32)) {
        self.syncing.set(true);
        self.width.set_value(f64::from(size.0));
        self.height.set_value(f64::from(size.1));
        self.percent.set_value(100.0);
        self.syncing.set(false);
    }

    fn quality(&self) -> SaveQuality {
        SAVE_QUALITIES.get(self.quality.selected() as usize).map_or(SaveQuality::Balanced, |q| q.0)
    }
}

/// Widgets the panel needs without keeping the state borrowed.
#[derive(Clone)]
struct Handles {
    content: gtk::Box,
    original: gtk::ToggleButton,
    scales: [gtk::Scale; 4],
    width: gtk::SpinButton,
    height: gtk::SpinButton,
    save_as: gtk::Button,
    resize_apply: gtk::Button,
}

fn handles(s: &ViewerState) -> Handles {
    let ui = &s.edit_ui;
    Handles {
        content: ui.content.clone(),
        original: ui.original.clone(),
        scales: ui.sliders.each_ref().map(|slider| slider.scale.clone()),
        width: ui.width.clone(),
        height: ui.height.clone(),
        save_as: ui.save_as.clone(),
        resize_apply: ui.resize_apply.clone(),
    }
}

pub(in crate::gui) fn is_open(state: &Rc<RefCell<ViewerState>>) -> bool {
    state.borrow().edit.open
}

/// Wire the panel widgets and the `win.edit-panel` toggle.
pub(in crate::gui) fn connect(state: &Rc<RefCell<ViewerState>>, window: &adw::ApplicationWindow) {
    let has_files = !state.borrow().files.is_empty();
    let action = gio::SimpleAction::new_stateful("edit-panel", None, &false.to_variant());
    action.set_enabled(has_files);
    {
        let weak = Rc::downgrade(state);
        action.connect_change_state(move |action, value| {
            let Some(open) = value.and_then(|v| v.get::<bool>()) else { return };
            action.set_state(&open.to_variant());
            if let Some(state) = weak.upgrade() {
                set_open(&state, open);
            }
        });
    }
    window.add_action(&action);

    let ui = handles(&state.borrow());
    let syncing = state.borrow().edit_ui.syncing.clone();

    // Sliders: preview on every change, commit when the gesture ends.
    for scale in &ui.scales {
        let weak = Rc::downgrade(state);
        let syncing = syncing.clone();
        scale.connect_value_changed(move |_| {
            if syncing.get() {
                return;
            }
            if let Some(state) = weak.upgrade() {
                sliders_moved(&state);
            }
        });
        // A legacy controller sees press and release even after the
        // slider's own drag gesture claims the pointer.
        let pointer = gtk::EventControllerLegacy::new();
        pointer.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(state);
        pointer.connect_event(move |_, event| {
            let Some(state) = weak.upgrade() else { return glib::Propagation::Proceed };
            match event.event_type() {
                gdk::EventType::ButtonPress | gdk::EventType::TouchBegin => {
                    if let Ok(mut s) = state.try_borrow_mut() {
                        s.edit.dragging = true;
                    }
                }
                gdk::EventType::ButtonRelease
                | gdk::EventType::TouchEnd
                | gdk::EventType::TouchCancel => {
                    let was =
                        state.try_borrow_mut().map(|mut s| std::mem::take(&mut s.edit.dragging));
                    if matches!(was, Ok(true)) {
                        // After GTK applied the release to the slider.
                        let weak = Rc::downgrade(&state);
                        glib::idle_add_local_once(move || {
                            if let Some(state) = weak.upgrade() {
                                flush_commit(&state);
                            }
                        });
                    }
                }
                _ => {}
            }
            glib::Propagation::Proceed
        });
        scale.add_controller(pointer);
    }
    let resets: Vec<(gtk::Button, usize)> = {
        let s = state.borrow();
        s.edit_ui.sliders.iter().enumerate().map(|(i, slider)| (slider.reset.clone(), i)).collect()
    };
    for (button, index) in resets {
        let weak = Rc::downgrade(state);
        button.connect_clicked(move |_| {
            if let Some(state) = weak.upgrade() {
                reset_slider(&state, Some(index));
            }
        });
    }
    {
        let weak = Rc::downgrade(state);
        let reset_all = state.borrow().edit_ui.reset_all.clone();
        reset_all.connect_clicked(move |_| {
            if let Some(state) = weak.upgrade() {
                reset_slider(&state, None);
            }
        });
    }

    // Size fields: the lock keeps the proportion of the current image.
    for width_changed in [true, false] {
        let spin = if width_changed { ui.width.clone() } else { ui.height.clone() };
        let weak = Rc::downgrade(state);
        let syncing = syncing.clone();
        spin.connect_value_changed(move |spin| {
            if syncing.get() {
                return;
            }
            let Some(state) = weak.upgrade() else { return };
            let Ok(s) = state.try_borrow() else { return };
            let ui = &s.edit_ui;
            if !ui.lock.is_active() {
                return;
            }
            let other = edits::locked_side(spin.value() as u32, s.natural, width_changed);
            let target = if width_changed { ui.height.clone() } else { ui.width.clone() };
            drop(s);
            syncing.set(true);
            target.set_value(f64::from(other));
            syncing.set(false);
        });
    }
    {
        let weak = Rc::downgrade(state);
        let syncing = syncing.clone();
        let percent = state.borrow().edit_ui.percent.clone();
        percent.connect_value_changed(move |percent| {
            if syncing.get() {
                return;
            }
            let Some(state) = weak.upgrade() else { return };
            let Ok(s) = state.try_borrow() else { return };
            let (w, h) = edits::percent_size(s.natural, percent.value());
            let (width, height) = (s.edit_ui.width.clone(), s.edit_ui.height.clone());
            drop(s);
            syncing.set(true);
            width.set_value(f64::from(w));
            height.set_value(f64::from(h));
            syncing.set(false);
        });
    }
    {
        let weak = Rc::downgrade(state);
        let lock = state.borrow().edit_ui.lock.clone();
        lock.connect_toggled(move |lock| {
            lock.set_icon_name(if lock.is_active() {
                "changes-prevent-symbolic"
            } else {
                "changes-allow-symbolic"
            });
            // Locking again snaps the height back to the proportion.
            let Some(state) = weak.upgrade() else { return };
            if lock.is_active() {
                let Ok(s) = state.try_borrow() else { return };
                let height = edits::locked_side(s.edit_ui.width.value() as u32, s.natural, true);
                let target = s.edit_ui.height.clone();
                let syncing = s.edit_ui.syncing.clone();
                drop(s);
                syncing.set(true);
                target.set_value(f64::from(height));
                syncing.set(false);
            }
        });
    }
    {
        let weak = Rc::downgrade(state);
        ui.resize_apply.connect_clicked(move |_| {
            if let Some(state) = weak.upgrade() {
                apply_resize(&state);
            }
        });
    }
    {
        let weak = Rc::downgrade(state);
        ui.save_as.connect_clicked(move |_| {
            if let Some(state) = weak.upgrade() {
                choose_save_as(&state);
            }
        });
    }
    {
        let weak = Rc::downgrade(state);
        ui.original.connect_toggled(move |button| {
            if let Some(state) = weak.upgrade() {
                show_original(&state, Source::Button, button.is_active());
            }
        });
    }

    // `\` shows the original only while held.
    let keys = gtk::EventControllerKey::new();
    {
        let weak = Rc::downgrade(state);
        keys.connect_key_released(move |_, key, _, _| {
            if key == gdk::Key::backslash {
                if let Some(state) = weak.upgrade() {
                    show_original(&state, Source::Key, false);
                }
            }
        });
    }
    window.add_controller(keys);

    // The reserved width depends on the window width.
    for property in ["default-width", "maximized", "fullscreened"] {
        let weak = Rc::downgrade(state);
        window.connect_notify_local(Some(property), move |_, _| {
            if let Some(state) = weak.upgrade() {
                // After GTK allocated the new size.
                glib::idle_add_local_once(move || apply_reserved_space(&state));
            }
        });
    }
}

/// Open or close the panel. Use the `win.edit-panel` action so the
/// header button follows; this only moves the widgets.
fn set_open(state: &Rc<RefCell<ViewerState>>, open: bool) {
    {
        let mut s = state.borrow_mut();
        if s.edit.open == open {
            return;
        }
        s.edit.open = open;
    }
    if !open {
        flush_commit(state);
        let original = state.borrow().edit_ui.original.clone();
        original.set_active(false);
        show_original(state, Source::Key, false);
    }
    refresh(state);
    sync_sensitivity(state);
    reveal(state);
}

/// Toggle through the action, as the header button and `E` do.
pub(in crate::gui) fn toggle(state: &Rc<RefCell<ViewerState>>) {
    let window = state.borrow().window();
    if let Some(action) = window.lookup_action("edit-panel") {
        if action.is_enabled() {
            let open = action.state().and_then(|v| v.get::<bool>()).unwrap_or(false);
            action.change_state(&(!open).to_variant());
        }
    }
}

/// Close the panel (Esc).
pub(in crate::gui) fn close(state: &Rc<RefCell<ViewerState>>) {
    let window = state.borrow().window();
    if let Some(action) = window.lookup_action("edit-panel") {
        action.change_state(&false.to_variant());
    }
}

/// Hide the panel while the crop mode owns the picture, and bring it
/// back afterwards.
pub(in crate::gui) fn suspend(state: &Rc<RefCell<ViewerState>>, suspended: bool) {
    {
        let mut s = state.borrow_mut();
        if s.edit.suspended == suspended {
            return;
        }
        s.edit.suspended = suspended;
    }
    if suspended {
        flush_commit(state);
    }
    reveal(state);
}

fn reveal(state: &Rc<RefCell<ViewerState>>) {
    let (revealer, shown) = {
        let s = state.borrow();
        (s.edit_ui.revealer.clone(), s.edit.open && !s.edit.suspended)
    };
    if shown {
        revealer.set_visible(true);
    }
    revealer.set_reveal_child(shown);
    apply_reserved_space(state);
}

/// Keep the picture, OSD, histogram and film strip clear of the panel,
/// unless the window is too narrow for both.
fn apply_reserved_space(state: &Rc<RefCell<ViewerState>>) {
    let Ok(s) = state.try_borrow() else { return };
    let Some(window) = s.window_weak.upgrade() else { return };
    let shown = s.edit.open && !s.edit.suspended;
    let reserve = if shown && window.width() >= OVERLAY_BELOW_WIDTH {
        // The panel may be wider than requested when a translation is long.
        let width = s
            .edit_ui
            .revealer
            .child()
            .map_or(PANEL_WIDTH, |panel| panel.measure(gtk::Orientation::Horizontal, -1).1);
        width.max(PANEL_WIDTH) + 2 * PANEL_MARGIN
    } else {
        0
    };
    let widgets = (
        s.stack.clone(),
        s.osd_revealer.clone(),
        s.loading_spinner.clone(),
        s.strip_revealer.clone(),
        s.histogram_area.clone(),
    );
    drop(s);
    let (stack, osd, spinner, strip, histogram) = widgets;
    if stack.margin_end() == reserve {
        return;
    }
    stack.set_margin_end(reserve);
    osd.set_margin_end(reserve);
    spinner.set_margin_end(reserve);
    strip.set_margin_end(12 + reserve);
    histogram.set_margin_end(12 + reserve);
}

/// Width the panel takes from the picture right now (0 when it floats).
#[cfg(test)]
pub(in crate::gui) fn reserved_width(s: &ViewerState) -> i32 {
    s.stack.margin_end()
}

/// Enable the controls that work right now. Transform and save buttons
/// follow their actions; the rest is driven from here.
pub(in crate::gui) fn sync_sensitivity(state: &Rc<RefCell<ViewerState>>) {
    let Ok(s) = state.try_borrow() else { return };
    let usable = !s.saving && !s.files.is_empty() && s.texture.is_some();
    let ui = handles(&s);
    drop(s);
    for scale in &ui.scales {
        scale.set_sensitive(usable);
    }
    ui.width.set_sensitive(usable);
    ui.height.set_sensitive(usable);
    ui.resize_apply.set_sensitive(usable);
    ui.save_as.set_sensitive(usable);
    ui.original.set_sensitive(usable);
    ui.content.set_sensitive(!state.try_borrow().is_ok_and(|s| s.saving));
}

/// Bring the controls in line with the current file and edits: after
/// Undo/Redo, navigation, a save or a new frame. Slider values the user
/// is still moving are left alone.
pub(in crate::gui) fn refresh(state: &Rc<RefCell<ViewerState>>) {
    let Ok(mut s) = state.try_borrow_mut() else { return };
    let Some(path) = s.files.get(s.idx).cloned() else { return };
    let key = (path.clone(), s.pending_ops.clone());
    let edits_changed = s.edit.synced.as_ref() != Some(&key);
    let gesture = s.edit.dragging || s.edit.commit_pending;
    let mut values = None;
    if edits_changed && !gesture {
        let trailing = edits::trailing_adjust(&s.pending_ops);
        s.edit.adjust = Some(trailing);
        values = Some(trailing);
        s.edit.synced = Some(key);
    }
    if s.edit.original.as_ref().is_some_and(|o| o.path != path) {
        s.edit.original = None;
    }
    if s.edit.base.as_ref().is_some_and(|b| b.path != path) {
        s.edit.base = None;
    }
    // The real frame replaces the preview once it shows the slider values.
    let mut repaint = false;
    if s.edit.preview.is_some()
        && !gesture
        && s.current_ready
        && s.edit.adjust == Some(edits::trailing_adjust(&s.pending_ops))
    {
        s.edit.preview = None;
        repaint = true;
    }
    let natural = (s.natural != s.edit.synced_natural && s.current_ready).then_some(s.natural);
    if let Some(natural) = natural {
        s.edit.synced_natural = natural;
    }
    if repaint {
        zoom::apply_zoom(&s);
    }
    let ui_values = s.edit.adjust.unwrap_or(AdjustValues::NEUTRAL);
    drop(s);
    let s = state.borrow();
    if let Some(values) = values {
        s.edit_ui.set_values(values);
    } else {
        s.edit_ui.show_values(ui_values);
    }
    if let Some(natural) = natural {
        s.edit_ui.set_size(natural);
    }
}

/// The texture to paint instead of the edited image, with the natural
/// size it stands for: the original while "Ver original" is on, or the
/// slider preview.
pub(in crate::gui) fn display_override(s: &ViewerState) -> Option<(gdk::Texture, (u32, u32))> {
    let current = s.files.get(s.idx)?;
    if s.edit.original_button || s.edit.original_key {
        if let Some(original) = s.edit.original.as_ref().filter(|o| &o.path == current) {
            return Some((original.texture.clone(), original.natural));
        }
    }
    s.edit.preview.clone().map(|texture| (texture, s.natural))
}

fn sliders_moved(state: &Rc<RefCell<ViewerState>>) {
    let values = {
        let Ok(mut s) = state.try_borrow_mut() else { return };
        if s.saving || s.files.is_empty() {
            return;
        }
        let values = s.edit_ui.values();
        s.edit.adjust = Some(values);
        values
    };
    state.borrow().edit_ui.show_values(values);
    schedule_preview(state);
    schedule_commit(state);
}

fn reset_slider(state: &Rc<RefCell<ViewerState>>, index: Option<usize>) {
    let values = {
        let s = state.borrow();
        let mut values = s.edit.adjust.unwrap_or(AdjustValues::NEUTRAL);
        let neutral = AdjustValues::NEUTRAL;
        match index {
            Some(0) => values.brightness = neutral.brightness,
            Some(1) => values.contrast = neutral.contrast,
            Some(2) => values.saturation = neutral.saturation,
            Some(_) => values.gamma_pct = neutral.gamma_pct,
            None => values = neutral,
        }
        values
    };
    state.borrow().edit_ui.set_values(values);
    {
        let mut s = state.borrow_mut();
        s.edit.adjust = Some(values);
        s.edit.commit_pending = true;
    }
    schedule_preview(state);
    flush_commit(state);
}

fn schedule_commit(state: &Rc<RefCell<ViewerState>>) {
    let dragging = {
        let mut s = state.borrow_mut();
        s.edit.commit_pending = true;
        if let Some(id) = s.edit.commit_source.take() {
            id.remove();
        }
        s.edit.dragging
    };
    // A pointer drag commits on release instead.
    if dragging {
        return;
    }
    let weak = Rc::downgrade(state);
    let id = glib::timeout_add_local_once(Duration::from_millis(COMMIT_IDLE_MS), move || {
        if let Some(state) = weak.upgrade() {
            state.borrow_mut().edit.commit_source = None;
            if !state.borrow().edit.dragging {
                flush_commit(&state);
            }
        }
    });
    state.borrow_mut().edit.commit_source = Some(id);
}

/// Commit a slider gesture still waiting for its idle time or release.
/// Called before anything else changes the edit history, so the gesture
/// is never lost.
pub(in crate::gui) fn flush_commit(state: &Rc<RefCell<ViewerState>>) {
    let pending = {
        let Ok(mut s) = state.try_borrow_mut() else { return };
        s.edit.dragging = false;
        if let Some(id) = s.edit.commit_source.take() {
            id.remove();
        }
        std::mem::take(&mut s.edit.commit_pending)
    };
    if pending {
        commit(state);
    }
}

fn commit(state: &Rc<RefCell<ViewerState>>) {
    let op = {
        let s = state.borrow();
        s.edit.adjust.and_then(|values| edits::adjust_commit(&s.pending_ops, values))
    };
    match op {
        Some(op) => apply_edit(state, op),
        // Back where it started: drop the preview.
        None => {
            let mut s = state.borrow_mut();
            if s.edit.preview.take().is_some() {
                zoom::apply_zoom(&s);
            }
        }
    }
}

fn schedule_preview(state: &Rc<RefCell<ViewerState>>) {
    {
        let mut s = state.borrow_mut();
        if s.edit.preview_scheduled {
            return;
        }
        s.edit.preview_scheduled = true;
    }
    let weak = Rc::downgrade(state);
    glib::idle_add_local_once(move || {
        if let Some(state) = weak.upgrade() {
            state.borrow_mut().edit.preview_scheduled = false;
            render_preview(&state);
        }
    });
}

fn render_preview(state: &Rc<RefCell<ViewerState>>) {
    {
        // The full-resolution frame may already show these values.
        let mut s = state.borrow_mut();
        let gesture = s.edit.dragging || s.edit.commit_pending;
        if !gesture
            && s.current_ready
            && s.edit.adjust == Some(edits::trailing_adjust(&s.pending_ops))
        {
            if s.edit.preview.take().is_some() {
                zoom::apply_zoom(&s);
            }
            return;
        }
    }
    let Some(base) = preview_base(state) else { return };
    let Some(values) = state.borrow().edit.adjust else { return };
    let adjusted = bigimage_core::adjust::apply_to(
        image::DynamicImage::ImageRgba8((*base).clone()),
        values.core(),
    )
    .into_rgba8();
    let (w, h) = adjusted.dimensions();
    let texture = texture_from_rgba(adjusted.into_raw(), w, h);
    let mut s = state.borrow_mut();
    s.edit.preview = Some(texture);
    zoom::apply_zoom(&s);
}

/// The working copy for the current edits, or `None` while it is being
/// prepared (the preview is drawn when it arrives).
fn preview_base(state: &Rc<RefCell<ViewerState>>) -> Option<Rc<image::RgbaImage>> {
    let (path, ops, from_texture) = {
        let s = state.borrow();
        let path = s.files.get(s.idx)?.clone();
        let ops = edits::adjust_base_ops(&s.pending_ops);
        if let Some(base) = s.edit.base.as_ref().filter(|b| b.path == path && b.ops == ops) {
            return Some(base.image.clone());
        }
        // The picture on screen already is the base when no adjustment
        // is being replaced and it is up to date.
        let replaces = matches!(s.pending_ops.last(), Some(EditOp::Adjust { .. }));
        let from_texture = (!replaces && s.current_ready).then(|| s.texture.clone()).flatten();
        if from_texture.is_none() && s.edit.base_job.as_ref() == Some(&(path.clone(), ops.clone()))
        {
            return None;
        }
        (path, ops, from_texture)
    };
    if let Some(texture) = from_texture {
        let image = Rc::new(downscale(texture_rgba(&texture)));
        state.borrow_mut().edit.base = Some(PreviewBase { path, ops, image: image.clone() });
        return Some(image);
    }
    state.borrow_mut().edit.base_job = Some((path.clone(), ops.clone()));
    let job_path = path.clone();
    let job_ops = ops.clone();
    let job = gio::spawn_blocking(move || {
        let img = bigimage_core::open_image(&job_path).ok()?;
        let edited = render_edits(img, &job_ops);
        Some(downscale(bigimage_core::into_srgb_rgba8(edited)))
    });
    let weak = Rc::downgrade(state);
    glib::MainContext::default().spawn_local(async move {
        let result = job.await.ok().flatten();
        let Some(state) = weak.upgrade() else { return };
        {
            let mut s = state.borrow_mut();
            if s.edit.base_job.as_ref() == Some(&(path.clone(), ops.clone())) {
                s.edit.base_job = None;
            }
            let still =
                s.files.get(s.idx) == Some(&path) && edits::adjust_base_ops(&s.pending_ops) == ops;
            match result {
                Some(image) if still => {
                    s.edit.base = Some(PreviewBase { path, ops, image: Rc::new(image) });
                }
                _ => return,
            }
        }
        schedule_preview(&state);
    });
    None
}

/// RGBA pixels of a texture.
pub(in crate::gui) fn texture_rgba(texture: &gdk::Texture) -> image::RgbaImage {
    let (w, h) = (texture.width().max(0) as u32, texture.height().max(0) as u32);
    let mut downloader = gdk::TextureDownloader::new(texture);
    downloader.set_format(gdk::MemoryFormat::R8g8b8a8);
    let (bytes, stride) = downloader.download_bytes();
    let row = w as usize * 4;
    let mut raw = Vec::with_capacity(row * h as usize);
    for y in 0..h as usize {
        raw.extend_from_slice(&bytes[y * stride..y * stride + row]);
    }
    image::RgbaImage::from_raw(w, h, raw).unwrap_or_default()
}

fn downscale(image: image::RgbaImage) -> image::RgbaImage {
    let (w, h) = edits::preview_size(image.dimensions(), PREVIEW_MAX_PIXELS);
    if (w, h) == image.dimensions() {
        return image;
    }
    let source = image::DynamicImage::ImageRgba8(image);
    match bigimage_core::resize::apply_to(
        &source,
        ResizeMode::Exact { width: w, height: h },
        Filter::Bilinear,
    ) {
        Ok(small) => small.into_rgba8(),
        Err(_) => source.into_rgba8(),
    }
}

/// Who asked for the original: the panel toggle or the held `\` key.
#[derive(Clone, Copy)]
pub(in crate::gui) enum Source {
    Button,
    Key,
}

/// Show (or stop showing) the unedited image.
pub(in crate::gui) fn show_original(state: &Rc<RefCell<ViewerState>>, source: Source, on: bool) {
    let need = {
        let Ok(mut s) = state.try_borrow_mut() else { return };
        if s.files.is_empty() {
            return;
        }
        match source {
            Source::Button => s.edit.original_button = on,
            Source::Key => s.edit.original_key = on,
        }
        let wanted = s.edit.original_button || s.edit.original_key;
        let path = s.files[s.idx].clone();
        let loaded = s.edit.original.as_ref().is_some_and(|o| o.path == path);
        if wanted && !loaded {
            // Without edits the picture on screen is the original.
            if s.pending_ops.is_empty() && s.current_ready {
                if let Some(texture) = s.texture.clone() {
                    let natural = s.natural;
                    s.edit.original = Some(Original { path: path.clone(), texture, natural });
                }
            } else if let Some(frame) = s.frame_cache.iter().find(|f| f.path == path) {
                let (texture, natural) = (frame.texture.clone(), frame.natural);
                s.edit.original = Some(Original { path: path.clone(), texture, natural });
            }
        }
        let loaded = s.edit.original.as_ref().is_some_and(|o| o.path == path);
        zoom::apply_zoom(&s);
        (wanted && !loaded && s.edit.original_job.as_ref() != Some(&path)).then_some(path)
    };
    let Some(path) = need else { return };
    state.borrow_mut().edit.original_job = Some(path.clone());
    let job_path = path.clone();
    let job = gio::spawn_blocking(move || decode_frame(&job_path, &[]).map_err(|e| e.to_string()));
    let weak = Rc::downgrade(state);
    glib::MainContext::default().spawn_local(async move {
        let result = job.await;
        let Some(state) = weak.upgrade() else { return };
        let mut s = state.borrow_mut();
        if s.edit.original_job.as_ref() == Some(&path) {
            s.edit.original_job = None;
        }
        if let Ok(Ok((raw, w, h, _))) = result {
            if s.files.get(s.idx) == Some(&path) {
                let texture = texture_from_rgba(raw, w, h);
                s.edit.original = Some(Original { path, texture, natural: (w, h) });
                zoom::apply_zoom(&s);
            }
        }
    });
}

/// Push the size fields as a resize edit.
fn apply_resize(state: &Rc<RefCell<ViewerState>>) {
    let (w, h, natural) = {
        let s = state.borrow();
        let ui = &s.edit_ui;
        (ui.width.value() as u32, ui.height.value() as u32, s.natural)
    };
    if (w, h) == natural || w == 0 || h == 0 {
        return;
    }
    apply_edit(state, EditOp::Resize { w, h });
}

/// Ask where to save a copy of the edited image.
fn choose_save_as(state: &Rc<RefCell<ViewerState>>) {
    flush_commit(state);
    let (window, path) = {
        let s = state.borrow();
        if s.files.is_empty() || s.saving {
            return;
        }
        (s.window(), s.files[s.idx].clone())
    };
    let stem = path.file_stem().map_or_else(|| "imagem".into(), |s| s.to_string_lossy());
    let extension = save_format(&path).map_or("png", |format| format.extension());
    let name = fill(&gettext("{name}-editada"), &[("name", &stem)]);
    let dialog = gtk::FileDialog::builder()
        .title(gettext("Salvar como"))
        .modal(true)
        .initial_name(format!("{name}.{extension}"))
        .build();
    if let Some(folder) = path.parent() {
        dialog.set_initial_folder(Some(&gio::File::for_path(folder)));
    }
    let weak = Rc::downgrade(state);
    dialog.save(Some(&window), None::<&gio::Cancellable>, move |result| {
        let (Some(state), Ok(file)) = (weak.upgrade(), result) else { return };
        if let Some(target) = file.path() {
            save_copy(&state, target);
        }
    });
}

/// Write the edited image to `target` without touching the open file.
pub(in crate::gui) fn save_copy(state: &Rc<RefCell<ViewerState>>, target: PathBuf) {
    flush_commit(state);
    let (source, ops, quality) = {
        let s = state.borrow();
        if s.files.is_empty() || s.saving {
            return;
        }
        (s.files[s.idx].clone(), s.pending_ops.clone(), s.edit_ui.quality())
    };
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    if same(&source, &target) {
        state.borrow().osd_label.set_text(&gettext("Para alterar o próprio arquivo, use Salvar."));
        pulse_osd_for(state, OSD_NOTICE_MS);
        return;
    }
    let Some(format) = save_format(&target) else {
        present_copy_failure(state, &SaveFailure::NoEncoder);
        return;
    };
    let options = save_as_options(format, quality);
    {
        let mut s = state.borrow_mut();
        s.saving = true;
        s.loading_spinner.set_visible(true);
    }
    sync_edit_actions(state);
    let worker_target = target.clone();
    let job =
        gio::spawn_blocking(move || write_copy(&source, &worker_target, &ops, format, &options));
    let weak = Rc::downgrade(state);
    glib::MainContext::default().spawn_local(async move {
        let result =
            job.await.unwrap_or_else(|_| Err(SaveFailure::Other("save worker panicked".into())));
        let Some(state) = weak.upgrade() else { return };
        {
            let mut s = state.borrow_mut();
            s.saving = false;
            s.loading_spinner.set_visible(false);
        }
        sync_edit_actions(&state);
        match result {
            Ok(dropped) => {
                adopt_copy(&state, &target);
                let name = target.file_name().map_or_else(
                    || target.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                let notice = metadata_loss_notice(&dropped)
                    .unwrap_or_else(|| fill(&gettext("Cópia salva: {name}"), &[("name", &name)]));
                state.borrow().osd_label.set_text(&notice);
                pulse_osd_for(&state, OSD_NOTICE_MS);
            }
            Err(failure) => {
                tracing::error!(?target, ?failure, "salvar como: falha");
                present_copy_failure(&state, &failure);
            }
        }
    });
}

/// Decode `source` for a re-save (keeping its metadata), apply `ops` and
/// publish the result atomically at `target`.
pub(in crate::gui) fn write_copy(
    source: &Path,
    target: &Path,
    ops: &[EditOp],
    format: Format,
    options: &EncodeOptions,
) -> Result<Vec<bigimage_core::MetadataLoss>, SaveFailure> {
    let (img, metadata, _) =
        crate::codec::open_for_resave(source).map_err(SaveFailure::from_image)?;
    let edited = render_edits(img, ops);
    let report = bigimage_core::save_image_preserving_metadata_report(
        edited,
        target,
        format,
        options,
        OverwritePolicy::Replace,
        &metadata,
    )
    .map_err(SaveFailure::from_core)?;
    Ok(report.dropped)
}

fn present_copy_failure(state: &Rc<RefCell<ViewerState>>, failure: &SaveFailure) {
    let window = state.borrow().window();
    let body = match failure {
        SaveFailure::NoEncoder => gettext(
            "O BigIris não sabe gravar este formato. Use, por exemplo, .png, .jpg ou .webp.",
        ),
        other => other.message(),
    };
    let alert =
        adw::AlertDialog::builder().heading(gettext("Não foi possível salvar")).body(body).build();
    alert.add_response("close", &gettext("Fechar"));
    alert.present(Some(&window));
}

/// A copy saved next to the open file joins the folder list, so the
/// arrows and the film strip reach it.
fn adopt_copy(state: &Rc<RefCell<ViewerState>>, target: &Path) {
    let changed = {
        let mut s = state.borrow_mut();
        let Some(current) = s.files.get(s.idx).cloned() else { return };
        let folder = |p: &Path| p.parent().and_then(|d| d.canonicalize().ok());
        if folder(&current).is_none() || folder(&current) != folder(target) || !is_viewable(target)
        {
            return;
        }
        // Same name as a file already listed: its cached pixels are stale.
        s.frame_cache.retain(|frame| frame.path != target);
        s.prefetch_skip.retain(|path| path != target);
        if s.files.iter().any(|path| path == target) {
            false
        } else {
            let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
            let key = name(target).unwrap_or_default();
            let mut files = (*s.files).clone();
            let at = files
                .iter()
                .position(|p| natural_cmp(&key, &name(p).unwrap_or_default()).is_lt())
                .unwrap_or(files.len());
            files.insert(at, target.to_path_buf());
            if at <= s.idx {
                s.idx += 1;
            }
            s.files = Rc::new(files);
            if app_settings().is_none_or(|settings| settings.boolean("filmstrip-visible")) {
                s.strip_revealer.set_can_target(true);
            }
            true
        }
    };
    if changed {
        let (window, strip, many) = {
            let s = state.borrow();
            (s.window(), s.strip_revealer.clone(), s.files.len() > 1)
        };
        if many && strip.can_target() {
            strip.set_visible(true);
        }
        for name in ["previous", "next"] {
            if let Some(action) = window.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                action.set_enabled(many);
            }
        }
    }
    rebuild_film_strip(state);
    refresh_title(state);
    refresh_film_strip_active(state);
    schedule_prefetch(state);
}

#[cfg(test)]
impl EditPanelState {
    pub(in crate::gui) fn is_shown(&self) -> bool {
        self.open && !self.suspended
    }

    pub(in crate::gui) fn has_preview(&self) -> bool {
        self.preview.is_some()
    }

    pub(in crate::gui) fn set_dragging(&mut self, dragging: bool) {
        self.dragging = dragging;
    }
}

#[cfg(test)]
impl EditPanelUi {
    pub(in crate::gui) fn scale(&self, index: usize) -> gtk::Scale {
        self.sliders[index].scale.clone()
    }

    pub(in crate::gui) fn slider_values(&self) -> AdjustValues {
        self.values()
    }

    pub(in crate::gui) fn size_controls(
        &self,
    ) -> (gtk::SpinButton, gtk::SpinButton, gtk::ToggleButton, gtk::SpinButton, gtk::Button) {
        (
            self.width.clone(),
            self.height.clone(),
            self.lock.clone(),
            self.percent.clone(),
            self.resize_apply.clone(),
        )
    }

    pub(in crate::gui) fn original_button(&self) -> gtk::ToggleButton {
        self.original.clone()
    }

    pub(in crate::gui) fn content(&self) -> gtk::Box {
        self.content.clone()
    }
}
