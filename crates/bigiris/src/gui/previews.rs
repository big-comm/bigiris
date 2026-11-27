// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded, cancellable preview preparation and rendering.
use super::*;
use std::sync::PoisonError;

/// Turn a `DynamicImage` into a `gdk::Texture` suitable for `gtk::Picture`.
/// Consumes the image (the RGBA buffer is handed to a `glib::Bytes` that
/// owns the allocation from there on).
pub(super) fn dynamic_image_to_texture(img: DynamicImage) -> gdk::Texture {
    let rgba = bigimage_core::into_srgb_rgba8(img);
    let (w, h) = rgba.dimensions();
    let stride = (w as usize) * 4;
    let bytes = glib::Bytes::from_owned(rgba.into_raw());
    gdk::MemoryTexture::new(w as i32, h as i32, gdk::MemoryFormat::R8g8b8a8, &bytes, stride)
        .upcast()
}

/// Build the preview-area column every dialog uses on its right-hand side,
/// with the window margins on its outer sides.
/// Returns the `gtk::Picture` the caller drives via `set_paintable` and the
/// outer container to append into the dialog's horizontal box.
pub(super) fn build_preview_column(session: &PreviewSession) -> (gtk::Picture, gtk::Box) {
    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    picture.set_size_request(360, 360);

    // Initial paintable = unmodified thumbnail.
    let initial = dynamic_image_to_texture(session.thumbnail().clone());
    picture.set_paintable(Some(&initial));

    let heading = gtk::Label::builder()
        .label(format!("<b>{}</b>", glib::markup_escape_text(&gettext("Preview"))))
        .use_markup(true)
        .xalign(0.0)
        .build();

    let (nw, nh) = session.natural_size();
    let original = gettext("Original: {width}×{height} px")
        .replace("{width}", &nw.to_string())
        .replace("{height}", &nh.to_string());
    let original_size = gtk::Label::builder().label(original).xalign(0.0).build();
    original_size.add_css_class("dim-label");

    let frame = gtk::Frame::new(None);
    frame.set_child(Some(&picture));

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .hexpand(true)
        .vexpand(true)
        .margin_top(16)
        .margin_bottom(16)
        .margin_end(16)
        .build();
    column.append(&heading);
    column.append(&frame);
    column.append(&original_size);

    (picture, column)
}

/// Wrap the GTK idle+timeout dance into a single handle. Callers schedule
/// a preview repaint by calling [`PreviewDebounce::trigger`], and the
/// closure runs at most once per debounce window no matter how many times
/// the sliders fire in the meantime.
pub(super) struct PreviewDebounce {
    pending: std::cell::Cell<Option<glib::SourceId>>,
}

impl PreviewDebounce {
    pub(super) fn new() -> Rc<Self> {
        Rc::new(Self { pending: std::cell::Cell::new(None) })
    }

    pub(super) fn trigger<F: FnMut() + 'static>(self: &Rc<Self>, delay: Duration, mut f: F) {
        if let Some(id) = self.pending.take() {
            id.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local(delay, move || {
            f();
            if let Some(strong) = weak.upgrade() {
                strong.pending.set(None);
            }
            glib::ControlFlow::Break
        });
        self.pending.set(Some(id));
    }
}

/// The newest render request of one preview and its latest result, by
/// revision; older requests are dropped unrendered.
struct PreviewRenderQueue {
    stopped: bool,
    next: Option<(u64, PreviewOp)>,
    result: Option<(u64, std::result::Result<DynamicImage, String>)>,
}
struct PreviewRenderer {
    queue: Arc<(Mutex<PreviewRenderQueue>, Condvar)>,
    revision: std::cell::Cell<u64>,
    /// Signalled for each finished render; closed with the renderer.
    rendered: viewer::Wake,
}
impl Drop for PreviewRenderer {
    fn drop(&mut self) {
        let (lock, wake) = &*self.queue;
        let mut pending = lock.lock().unwrap_or_else(PoisonError::into_inner);
        pending.stopped = true;
        pending.next = None;
        pending.result = None;
        wake.notify_one();
        self.rendered.close();
    }
}
thread_local! {
    static PREVIEW_RENDERERS: RefCell<Vec<(glib::WeakRef<gtk::Picture>, Rc<PreviewRenderer>)>> = const { RefCell::new(Vec::new()) };
}
/// Renders the newest queued request until the queue is stopped.
fn spawn_render_thread(
    session: PreviewSession,
    queue: Arc<(Mutex<PreviewRenderQueue>, Condvar)>,
    rendered: viewer::Wake,
) {
    std::thread::spawn(move || {
        loop {
            let (lock, wake) = &*queue;
            let request = {
                let mut pending = lock.lock().unwrap_or_else(PoisonError::into_inner);
                loop {
                    if pending.stopped {
                        return;
                    }
                    if let Some(request) = pending.next.take() {
                        break request;
                    }
                    pending = wake.wait(pending).unwrap_or_else(PoisonError::into_inner);
                }
            };
            // A panicking transform must not kill the renderer and freeze
            // the preview; report it like any other failed render.
            let image = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                session
                    .apply(&request.1)
                    .map(|img| DynamicImage::ImageRgba8(bigimage_core::into_srgb_rgba8(img)))
                    .map_err(|error| crate::error_text::core_error_text(&error))
            }))
            .unwrap_or_else(|_| Err("preview renderer panicked".to_string()));
            let mut pending = lock.lock().unwrap_or_else(PoisonError::into_inner);
            if !pending.stopped {
                pending.result = Some((request.0, image));
                rendered.notify();
            }
        }
    });
}
/// A render thread for `picture` and the main-loop task that shows its results.
fn start_renderer(session: &PreviewSession, picture: &gtk::Picture) -> Rc<PreviewRenderer> {
    let queue = Arc::new((
        Mutex::new(PreviewRenderQueue { stopped: false, next: None, result: None }),
        Condvar::new(),
    ));
    let rendered = viewer::Wake::default();
    let renderer = Rc::new(PreviewRenderer {
        queue: queue.clone(),
        revision: std::cell::Cell::new(0),
        rendered: rendered.clone(),
    });
    spawn_render_thread(session.clone(), queue, rendered.clone());
    let weak = picture.downgrade();
    let weak_renderer = Rc::downgrade(&renderer);
    glib::MainContext::default().spawn_local(async move {
        while rendered.wait().await {
            let (Some(picture), Some(renderer)) = (weak.upgrade(), weak_renderer.upgrade()) else {
                return;
            };
            let result =
                renderer.queue.0.lock().unwrap_or_else(PoisonError::into_inner).result.take();
            if let Some((revision, result)) = result
                && revision == renderer.revision.get()
            {
                match result {
                    Ok(image) => {
                        picture.set_paintable(Some(&dynamic_image_to_texture(image)));
                    }
                    Err(error) => tracing::warn!(%error, "preview: apply failed"),
                }
            }
        }
    });
    // The picture going away stops its renderer thread.
    picture.add_weak_ref_notify_local(|| {
        PREVIEW_RENDERERS.with(|entries| {
            if let Ok(mut entries) = entries.try_borrow_mut() {
                entries.retain(|(w, _)| w.upgrade().is_some());
            }
        });
    });
    renderer
}
/// Apply `op` to `session` off the GTK loop and push the resulting thumbnail
/// into `picture`. Errors are logged and the previous paintable is left
/// untouched.
pub(super) fn refresh_preview(session: &PreviewSession, picture: &gtk::Picture, op: &PreviewOp) {
    let renderer = PREVIEW_RENDERERS.with(|renderers| {
        let mut renderers = renderers.borrow_mut();
        renderers.retain(|(weak, _)| weak.upgrade().is_some());
        if let Some((_, renderer)) =
            renderers.iter().find(|(weak, _)| weak.upgrade().as_ref() == Some(picture))
        {
            return renderer.clone();
        }
        let renderer = start_renderer(session, picture);
        renderers.push((picture.downgrade(), renderer.clone()));
        renderer
    });
    let revision = renderer.revision.get().wrapping_add(1);
    renderer.revision.set(revision);
    let (lock, wake) = &*renderer.queue;
    lock.lock().unwrap_or_else(PoisonError::into_inner).next = Some((revision, *op));
    wake.notify_one();
}

/// Present the window immediately; file I/O, ICC and resampling run off GTK.
/// Closing the window cancels the isolated codec and drops pending results.
type DialogInitializer = fn(
    adw::ApplicationWindow,
    Rc<Vec<PathBuf>>,
    Option<Rc<PreviewSession>>,
) -> adw::ApplicationWindow;
pub(super) fn prepare_dialog(
    window: &adw::ApplicationWindow,
    files: Rc<Vec<PathBuf>>,
    fill: DialogInitializer,
) {
    let spinner = adw::Spinner::new();
    let loading = gtk::Box::new(gtk::Orientation::Vertical, 16);
    loading.set_valign(gtk::Align::Center);
    loading.append(&spinner);
    loading.append(&gtk::Label::new(Some(&gettext("Preparing preview…"))));
    let cancel_button = gtk::Button::with_label(&gettext("Cancel"));
    cancel_button.set_halign(gtk::Align::Center);
    let weak_window = window.downgrade();
    cancel_button.connect_clicked(move |_| {
        if let Some(window) = weak_window.upgrade() {
            window.close();
        }
    });
    loading.append(&cancel_button);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&loading));
    window.set_content(Some(&toolbar));
    let cancel = Arc::new(AtomicBool::new(false));
    let closing = cancel.clone();
    window.connect_unrealize(move |_| closing.store(true, Ordering::Release));
    // The preview shows the first file that opens; the operation still runs
    // on every file, so one unreadable file does not block the others.
    let inputs = files.to_vec();
    let job = gio::spawn_blocking(move || {
        crate::codec::with_cancel(cancel, || {
            let mut first_failure = None;
            for path in inputs {
                if crate::codec::cancelled() {
                    break;
                }
                match PreviewSession::open(path) {
                    Ok(session) => return Ok((session, first_failure)),
                    Err(error) => {
                        first_failure
                            .get_or_insert_with(|| crate::error_text::core_error_text(&error));
                    }
                }
            }
            Err(first_failure.unwrap_or_else(|| gettext("No image selected")))
        })
    });
    let weak = window.downgrade();
    let weak_toolbar = toolbar.downgrade();
    glib::MainContext::default().spawn_local(async move {
        let result = job.await;
        let (Some(window), Some(toolbar)) = (weak.upgrade(), weak_toolbar.upgrade()) else {
            return;
        };
        if !window.is_visible() {
            return;
        }
        let result = result.unwrap_or_else(|_| Err(gettext("Preparation was interrupted")));
        match result {
            Ok((session, failure)) => {
                let window = fill(window, files, Some(Rc::new(session)));
                if let Some(failure) = failure
                    && let Some(toolbar) = window.content().and_downcast::<adw::ToolbarView>()
                {
                    let banner = adw::Banner::builder()
                        .title(failure)
                        .use_markup(false)
                        .revealed(true)
                        .build();
                    toolbar.add_top_bar(&banner);
                }
            }
            Err(error) => {
                let close = gtk::Button::builder()
                    .label(gettext("Close"))
                    .halign(gtk::Align::Center)
                    .css_classes(["pill"])
                    .build();
                let weak = window.downgrade();
                close.connect_clicked(move |_| {
                    if let Some(window) = weak.upgrade() {
                        window.close();
                    }
                });
                let page = adw::StatusPage::builder()
                    .title(gettext("Could not open the image"))
                    .description(glib::markup_escape_text(&error))
                    .icon_name("dialog-error-symbolic")
                    .child(&close)
                    .build();
                toolbar.set_content(Some(&page));
            }
        }
    });
}

#[cfg(test)]
mod prepare_tests {
    use super::*;

    /// An unreadable first file leaves the rest of the selection workable:
    /// the form previews the next file under a banner naming the failure;
    /// with nothing readable, the window keeps its header and a Close button.
    #[test]
    #[ignore = "requires a graphical display; run alone with GSETTINGS_BACKEND=memory"]
    fn dialog_survives_unreadable_first_file() {
        let app = dialogs::probe_app("com.biglinux.Iris.PrepareProbe");
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.png");
        image::RgbImage::new(64, 48).save(&good).unwrap();
        let broken = dir.path().join("broken.png");
        std::fs::write(&broken, &std::fs::read(&good).unwrap()[..40]).unwrap();

        let window = build_convert_dialog(&app, Rc::new(vec![broken.clone(), good]));
        window.present();
        let root: gtk::Widget = window.clone().upcast();
        let banner = || dialogs::find_widget(&root, &|banner: &adw::Banner| banner.is_revealed());
        dialogs::pump_until("banner", &|| banner().is_some());
        let title = banner().unwrap().title();
        assert!(title.contains("broken.png"), "{title}");
        let convert = gettext("Convert");
        let apply = dialogs::find_widget(&root, &|button: &gtk::Button| {
            button.label().as_deref() == Some(convert.as_str())
        });
        assert!(apply.is_some_and(|button| button.is_sensitive()));
        window.destroy();

        let window = build_convert_dialog(&app, Rc::new(vec![broken]));
        window.present();
        let root: gtk::Widget = window.clone().upcast();
        let page = || dialogs::find_widget::<adw::StatusPage>(&root, &|_| true);
        dialogs::pump_until("failure page", &|| page().is_some());
        let close = gettext("Close");
        assert!(
            dialogs::find_widget(&root, &|button: &gtk::Button| {
                button.label().as_deref() == Some(close.as_str())
            })
            .is_some()
        );
        assert!(dialogs::find_widget::<adw::HeaderBar>(&root, &|_| true).is_some());
        window.destroy();
    }
}
