// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded, cancellable preview preparation and rendering.
use super::*;

/// Turn a `DynamicImage` into a `gdk::Texture` suitable for `gtk::Picture`.
/// Consumes the image (the RGBA buffer is handed to a `glib::Bytes` that
/// owns the allocation from there on).
pub(super) fn dynamic_image_to_texture(img: DynamicImage) -> gdk::Texture {
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let stride = (w as usize) * 4;
    let bytes = glib::Bytes::from_owned(rgba.into_raw());
    gdk::MemoryTexture::new(w as i32, h as i32, gdk::MemoryFormat::R8g8b8a8, &bytes, stride)
        .upcast()
}

/// Build the preview-area column every dialog uses on its right-hand side.
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

    let heading =
        gtk::Label::builder().label("<b>Pré-visualização</b>").use_markup(true).xalign(0.0).build();

    let (nw, nh) = session.natural_size();
    let info = gtk::Label::builder().label(format!("Original: {nw}×{nh} px")).xalign(0.0).build();
    info.add_css_class("dim-label");

    let frame = gtk::Frame::new(None);
    frame.set_child(Some(&picture));

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .hexpand(true)
        .vexpand(true)
        .build();
    column.append(&heading);
    column.append(&frame);
    column.append(&info);

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

/// Apply `op` to `session` and push the resulting thumbnail into `picture`.
/// Errors are logged and the previous paintable is left untouched.
struct PreviewRenderQueue {
    stopped: bool,
    next: Option<(u64, PreviewOp)>,
    result: Option<(u64, std::result::Result<DynamicImage, String>)>,
}
struct PreviewRenderer {
    queue: Arc<(Mutex<PreviewRenderQueue>, Condvar)>,
    revision: std::cell::Cell<u64>,
}
impl Drop for PreviewRenderer {
    fn drop(&mut self) {
        let (lock, wake) = &*self.queue;
        let mut pending = lock.lock().unwrap();
        pending.stopped = true;
        pending.next = None;
        pending.result = None;
        wake.notify_one();
    }
}
thread_local! {
    static PREVIEW_RENDERERS: RefCell<Vec<(glib::WeakRef<gtk::Picture>, Rc<PreviewRenderer>)>> = const { RefCell::new(Vec::new()) };
}
pub(super) fn refresh_preview(session: &PreviewSession, picture: &gtk::Picture, op: &PreviewOp) {
    let renderer = PREVIEW_RENDERERS.with(|renderers| {
        let mut renderers = renderers.borrow_mut();
        renderers.retain(|(weak, _)| weak.upgrade().is_some());
        if let Some((_, renderer)) =
            renderers.iter().find(|(weak, _)| weak.upgrade().as_ref() == Some(picture))
        {
            return renderer.clone();
        }
        let queue = Arc::new((
            Mutex::new(PreviewRenderQueue { stopped: false, next: None, result: None }),
            Condvar::new(),
        ));
        let renderer =
            Rc::new(PreviewRenderer { queue: queue.clone(), revision: std::cell::Cell::new(0) });
        let session = session.clone();
        std::thread::spawn(move || loop {
            let (lock, wake) = &*queue;
            let request = {
                let mut pending = lock.lock().unwrap();
                while !pending.stopped && pending.next.is_none() {
                    pending = wake.wait(pending).unwrap();
                }
                if pending.stopped {
                    break;
                }
                pending.next.take().unwrap()
            };
            let image = session
                .apply(&request.1)
                .map(|img| DynamicImage::ImageRgba8(img.to_rgba8()))
                .map_err(|e| e.to_string());
            let mut pending = lock.lock().unwrap();
            if !pending.stopped {
                pending.result = Some((request.0, image));
            }
        });
        let weak = picture.downgrade();
        let weak_renderer = Rc::downgrade(&renderer);
        glib::timeout_add_local(Duration::from_millis(16), move || {
            let Some(picture) = weak.upgrade() else {
                PREVIEW_RENDERERS
                    .with(|entries| entries.borrow_mut().retain(|(w, _)| w.upgrade().is_some()));
                return glib::ControlFlow::Break;
            };
            let Some(renderer) = weak_renderer.upgrade() else { return glib::ControlFlow::Break };
            let result = renderer.queue.0.lock().unwrap().result.take();
            if let Some((revision, result)) = result {
                if revision == renderer.revision.get() {
                    match result {
                        Ok(image) => picture.set_paintable(Some(&dynamic_image_to_texture(image))),
                        Err(error) => tracing::warn!(%error, "preview: apply failed"),
                    }
                }
            }
            glib::ControlFlow::Continue
        });
        renderers.push((picture.downgrade(), renderer.clone()));
        renderer
    });
    let revision = renderer.revision.get().wrapping_add(1);
    renderer.revision.set(revision);
    let (lock, wake) = &*renderer.queue;
    lock.lock().unwrap().next = Some((revision, *op));
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
    let spinner = gtk::Spinner::new();
    spinner.start();
    let loading = gtk::Box::new(gtk::Orientation::Vertical, 16);
    loading.set_valign(gtk::Align::Center);
    loading.append(&spinner);
    loading.append(&gtk::Label::new(Some("Preparando pré-visualização…")));
    let cancel_button = gtk::Button::with_label("Cancelar");
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
    let input = files.first().cloned();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = crate::codec::with_cancel(cancel, || {
            input
                .ok_or_else(|| "Nenhuma imagem selecionada".to_string())
                .and_then(|path| PreviewSession::open(path).map_err(|e| e.to_string()))
        });
        let _ = tx.send(result);
    });
    let weak = window.downgrade();
    glib::timeout_add_local(Duration::from_millis(16), move || {
        let Some(window) = weak.upgrade() else { return glib::ControlFlow::Break };
        if !window.is_visible() {
            return glib::ControlFlow::Break;
        }
        match rx.try_recv() {
            Ok(Ok(session)) => {
                fill(window, files.clone(), Some(Rc::new(session)));
            }
            Ok(Err(error)) => {
                let page = adw::StatusPage::builder()
                    .title("Não foi possível abrir a imagem")
                    .description(error)
                    .icon_name("dialog-error-symbolic")
                    .build();
                window.set_content(Some(&page));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                window.set_content(Some(&gtk::Label::new(Some("A preparação foi interrompida"))));
            }
        }
        glib::ControlFlow::Break
    });
}
