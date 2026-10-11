// SPDX-License-Identifier: GPL-3.0-or-later
//! Viewer "Íris" — GTK4 + libadwaita, tuned for Plasma and other Linux
//! desktops.
//!
//! MVP+zoom/pan: `gtk::Picture` wrapped in `gtk::ScrolledWindow`. `Fit` mode
//! uses `ContentFit::Contain` and lets the picture ride the viewport; the
//! moment the user zooms, we switch to `Fill` with an explicit size request
//! so the scroll view can give actual scrollbars and pan behaviour.
//!
//! The viewing surface stays intentionally quiet; advanced editing and
//! batch tools live behind the menu and the folder workflow.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};

use adw::prelude::*;
use gettextrs::gettext;
use gtk::{gdk, gio, glib, graphene};
use gtk4 as gtk;
use libadwaita as adw;

mod previews;
mod zoom;
use previews::{
    build_preview_column, dynamic_image_to_texture, prepare_dialog, refresh_preview,
    PreviewDebounce,
};
#[cfg(test)]
use zoom::anchor_axis;
use zoom::{adjust_zoom, apply_zoom, set_zoom, zoom_in, zoom_out, PendingZoom};

const APP_ID: &str = "com.biglinux.Iris";
const ZOOM_STEP: f64 = 1.25;
const MIN_ZOOM: f64 = 0.05;
const MAX_ZOOM: f64 = 32.0;
/// Top-of-window reveal zone for the header bar, in pixels.
const TOP_EDGE_PX: f64 = 60.0;
/// Bottom-of-window reveal zone for the film strip, in pixels. Slightly
/// taller than the strip itself (96 px) so the user can "catch" it.
const BOTTOM_EDGE_PX: f64 = 110.0;
/// Hide delay for header / strip after the cursor leaves the edge zone.
const DEFAULT_EDGE_HIDE_MS: u64 = 1500;
/// OSD pill visible duration after a nav/zoom pulse, mpv-style.
const OSD_PULSE_MS: u64 = 1200;
/// Idle time before the cursor itself blinks out (mouse still, no key).
const CURSOR_HIDE_MS: u64 = 2000;
/// Slideshow step between auto-advances (Space toggles).
const DEFAULT_SLIDESHOW_INTERVAL_S: u32 = 5;

/// CSS applied app-wide. Theater is on by default (see `build_window`);
/// `.theater-off` is the escape hatch that restores Adwaita chrome when
/// the user toggles theater off.
const THEATER_CSS: &str = "
.theater, .theater scrolledwindow, .theater scrolledwindow > viewport {
    background-color: #101216;
}
.film-strip { padding: 4px 8px; }
.film-strip button { padding: 2px; min-height: 0; min-width: 0; }
.film-strip button.active-thumb {
    outline: 2px solid @accent_color;
    outline-offset: -2px;
}
.viewer-loading { padding: 12px; border-radius: 8px; background: @window_bg_color; color: @window_fg_color; }
.plasma headerbar button { border-radius: 6px; }
.plasma .suggested-action { border-radius: 6px; }
.plasma .viewer-osd { border-radius: 8px; }
";

/// Pixel size of each thumbnail in the film strip (longest edge).
const THUMB_EDGE: u32 = 80;

/// Start the viewer event loop. Returns the exit code GTK produced.
pub fn run_viewer(files: Vec<PathBuf>) -> i32 {
    // Um clique no gerenciador de arquivos entrega um caminho só. Sem isto,
    // `files.len() == 1` desliga setas, film strip e slideshow, e o
    // visualizador vira uma janela de imagem única.
    // Directory enumeration must never block the GTK thread.
    // NON_UNIQUE: sem isso o GApplication tenta single-instance via D-Bus
    // e, se uma janela do BigIris tá se fechando no momento em que outra
    // tenta abrir (ex.: dashboard spawna novo processo e fecha), a nova
    // ativação é descartada. NON_UNIQUE garante que cada `bigiris …`
    // seja um processo independente com seu próprio event loop.
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let files = Rc::new(files);
    app.connect_activate(move |app| {
        install_theater_css();
        let waiting = adw::ApplicationWindow::builder()
            .application(app)
            .title("BigIris — abrindo imagens")
            .default_width(640)
            .default_height(480)
            .build();
        let spinner = gtk::Spinner::new();
        spinner.start();
        waiting.set_content(Some(&spinner));
        waiting.present();
        let input = files.as_ref().clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let closing = cancel.clone();
        waiting.connect_unrealize(move |_| closing.store(true, Ordering::Release));
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = tx.send(expand_to_folder_cancellable(input, &cancel));
        });
        let weak = waiting.downgrade();
        let weak_app = app.downgrade();
        glib::timeout_add_local(Duration::from_millis(16), move || {
            let Some(waiting) = weak.upgrade() else { return glib::ControlFlow::Break };
            if !waiting.is_visible() {
                return glib::ControlFlow::Break;
            }
            match rx.try_recv() {
                Ok((files, index)) => {
                    if let Some(app) = weak_app.upgrade() {
                        build_window(&app, Rc::new(files), index).present();
                    }
                    waiting.close();
                    glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
            }
        });
    });

    // Empty argv: clap already consumed the interesting bits and we don't want
    // GTK to re-parse our positional file paths as GIO URLs.
    let code = app.run_with_args::<&str>(&[]);
    i32::from(u8::from(code))
}

/// Quando o visualizador recebe um arquivo só, varre a pasta dele e
/// devolve todas as imagens irmãs em ordem natural, junto do índice do
/// arquivo pedido. É o que faz o clique no gerenciador de arquivos abrir
/// a coleção inteira em vez de uma imagem sozinha: setas, `Home`/`End`,
/// film strip e slideshow já operam sobre `files`, então basta entregar a
/// lista certa.
///
/// Com dois ou mais arquivos na entrada nada muda: quem passou uma seleção
/// explícita na linha de comando quer exatamente aquela seleção.
///
/// Qualquer tropeço na varredura (pasta ilegível, arquivo pedido fora da
/// lista de formatos) cai de volta na entrada original. Abrir a imagem que
/// o usuário pediu importa mais do que oferecer navegação.
#[cfg(test)]
fn expand_to_folder(files: Vec<PathBuf>) -> (Vec<PathBuf>, usize) {
    expand_to_folder_cancellable(files, &AtomicBool::new(false))
}
fn expand_to_folder_cancellable(files: Vec<PathBuf>, cancel: &AtomicBool) -> (Vec<PathBuf>, usize) {
    if files.len() != 1 {
        return (files, 0);
    }

    let target = &files[0];
    if !target.is_file() {
        return (files, 0);
    }

    // `parent()` de um caminho sem diretório ("foto.png") devolve "", que
    // não é legível por read_dir; nesse caso a pasta é a atual.
    let dir = match target.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        Some(_) => PathBuf::from("."),
        None => return (files, 0),
    };

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return (files, 0);
    };

    let mut siblings = Vec::new();
    for entry in entries {
        if cancel.load(Ordering::Acquire) {
            return (files, 0);
        }
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if is_viewable(&path) && path.is_file() {
            siblings.push(path);
        }
        if siblings.len() > 50_000 {
            tracing::warn!("pasta excedeu 50 mil imagens; abrindo somente a imagem solicitada");
            return (files, 0);
        }
    }

    if siblings.len() < 2 {
        return (files, 0);
    }

    siblings.sort_by(|a, b| {
        let an = a.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let bn = b.file_name().unwrap_or_default().to_string_lossy().into_owned();
        // Desempate pelo nome cru para a ordem não depender da ordem de
        // entrega do read_dir quando dois nomes só diferem em maiúsculas.
        natural_cmp(&an, &bn).then_with(|| an.cmp(&bn))
    });

    // O gerenciador de arquivos pode entregar symlink ou caminho relativo
    // diferente do que o read_dir devolve, então o caminho canônico decide.
    let canonical_target = target.canonicalize().ok();
    let found = siblings.iter().position(|path| {
        path == target
            || (canonical_target.as_ref().is_some_and(|p| p.file_name() == path.file_name())
                && path.canonicalize().ok() == canonical_target)
    });

    match found {
        Some(idx) => (siblings, idx),
        None => (files, 0),
    }
}

/// `true` quando a extensão do arquivo é um formato que o BigIris abre.
/// Usa a mesma tabela do resto do programa ([`bigimage_core::Format`]),
/// para a varredura da pasta não divergir do que o visualizador aceita.
///
/// A checagem é por extensão de propósito: farejar o conteúdo de cada
/// arquivo custaria uma leitura por item da pasta só para montar a lista.
/// Um arquivo com extensão mentirosa continua abrindo normalmente quando é
/// ele o alvo do clique, porque aí quem decodifica é
/// [`bigimage_core::open_image`], que fareja o conteúdo.
fn is_viewable(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| bigimage_core::Format::parse(ext).is_ok())
}

/// Compara nomes tratando sequências de dígitos como número, e não como
/// texto. É o que põe `2.png` antes de `10.png`; a ordem lexicográfica pura
/// devolveria `1, 10, 100, 11, …`, que é justamente o padrão das coleções
/// numeradas de wallpaper e das fotos saídas de câmera (`IMG_2.jpg`).
///
/// Entre dois números de mesmo valor, vence o que tem menos zeros à
/// esquerda, então `7` vem antes de `007` e a ordem fica estável.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (mut i, mut j) = (0usize, 0usize);

    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let start_a = i;
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            let start_b = j;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }

            let num_a: String = a[start_a..i].iter().collect();
            let num_b: String = b[start_b..j].iter().collect();
            let trim_a = num_a.trim_start_matches('0');
            let trim_b = num_b.trim_start_matches('0');

            // Sem zeros à esquerda, mais dígitos significa número maior.
            let ord = trim_a.len().cmp(&trim_b.len()).then_with(|| trim_a.cmp(trim_b));
            if ord != Ordering::Equal {
                return ord;
            }
            let ord = num_a.len().cmp(&num_b.len());
            if ord != Ordering::Equal {
                return ord;
            }
        } else {
            let ca = a[i].to_lowercase().next().unwrap_or(a[i]);
            let cb = b[j].to_lowercase().next().unwrap_or(b[j]);
            let ord = ca.cmp(&cb);
            if ord != Ordering::Equal {
                return ord;
            }
            i += 1;
            j += 1;
        }
    }

    (a.len() - i).cmp(&(b.len() - j))
}

/// Mutable viewer state shared between the window and its controllers.
///
/// The UI is edge-triggered: cursor near the top reveals the header,
/// near the bottom reveals the film strip, and nav/zoom pulses the OSD.
/// Everywhere else is pure image, so timers are kept per-zone.
struct ViewerState {
    files: Rc<Vec<PathBuf>>,
    idx: usize,
    picture: gtk::Picture,
    scroller: gtk::ScrolledWindow,
    title: adw::WindowTitle,
    /// Weak gobject ref to the owning window. Stored as `WeakRef` (not
    /// a strong `adw::ApplicationWindow`) to break the strong-ref cycle
    /// `window → controllers → closures → Rc<state> → state.window →
    /// window`. Use [`ViewerState::window`] for a strong handle when a
    /// callback runs (the window is always alive at that point); use
    /// `window_weak.upgrade()` directly only when you need the
    /// `Option<...>` semantics for graceful no-ops post-close.
    window_weak: glib::WeakRef<adw::ApplicationWindow>,
    zoom: ZoomState,
    texture: Option<gdk::Texture>,
    zoom_pointer: Option<(f64, f64)>,
    pending_zoom: Option<PendingZoom>,
    zoom_source: Option<glib::SourceId>,
    /// Natural pixel dimensions of the currently-loaded image.
    natural: (u32, u32),
    /// Toolbar view holding the header bar; revealed/hidden by edge-reveal.
    toolbar_view: adw::ToolbarView,
    /// Bottom-center OSD revealer (mpv-style pill: name · idx · zoom).
    osd_revealer: gtk::Revealer,
    /// Label inside the OSD revealer.
    osd_label: gtk::Label,
    /// Film strip revealer (SlideUp). Separate from OSD so each zone has
    /// its own lifecycle.
    strip_revealer: gtk::Revealer,
    /// Film strip scrolled container (hidden when files.len() < 2).
    film_strip_scroll: gtk::ScrolledWindow,
    /// One button per file so we can flip `.active-thumb` on navigation.
    film_strip_buttons: Vec<gtk::Button>,
    film_strip_start: usize,
    film_strip_box: gtk::Box,
    thumbnail_cancel: Arc<AtomicBool>,
    /// When true (accessibility override), chrome stays pinned visible.
    ui_pinned: bool,
    autohide_ui: bool,
    autohide_delay_ms: u64,
    slideshow_interval_s: u32,
    /// Theater mode darkens the chrome around the picture, no fullscreen.
    /// Enabled by default — the photo-viewer paradigm.
    theater_enabled: bool,
    /// Pending hide timers, one per zone, so each fades on its own clock.
    top_hide_source: Option<glib::SourceId>,
    bottom_hide_source: Option<glib::SourceId>,
    osd_hide_source: Option<glib::SourceId>,
    cursor_hide_source: Option<glib::SourceId>,
    /// Slideshow auto-advance tick (Space toggles). `None` = stopped.
    slideshow_source: Option<glib::SourceId>,
    /// DrawingArea com o overlay de histograma (canto inferior-direito).
    /// Visibilidade controlada pela tecla `G` — oculto por padrão.
    histogram_area: gtk::DrawingArea,
    /// Histograma RGB da imagem atual: `[0..256]=R, [256..512]=G, [512..768]=B`.
    /// Recalculado em cada `update_display`.
    histogram: [u32; 768],
    /// Decode and edit rendering happen away from GTK's main loop. A
    /// generation token makes rapid navigation discard stale results.
    loader: ViewerLoader,
    load_generation: u64,
    loading_spinner: gtk::Spinner,
    /// Dialog de propriedades atualmente aberto, se houver. Evita que
    /// cliques repetidos no botão `i` empilhem janelas.
    props_dialog: Option<adw::ApplicationWindow>,
    /// Edições pendentes (rotate/flip) aplicadas in-memory sobre a
    /// imagem decodificada. Vazio = arquivo "limpo". `Ctrl+S` grava e
    /// limpa; a navegação é bloqueada enquanto houver alterações.
    pending_ops: Vec<EditOp>,
    /// Operations removed by Undo, available to Redo until a new edit or
    /// navigation invalidates the branch.
    redo_ops: Vec<EditOp>,
    /// True while an atomic encode is running off the GTK thread.
    saving: bool,
}

/// Edição in-memory: o usuário gira/espelha sem tocar o
/// arquivo; `•` aparece no título e só no `Ctrl+S` o disco é gravado.
/// As ops são acumuladas em `ViewerState.pending_ops` e reaplicadas por
/// cima do decode original a cada push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditOp {
    RotateCw,
    RotateCcw,
    Rotate180,
    FlipH,
    FlipV,
}

#[derive(Debug)]
struct LoadRequest {
    generation: u64,
    path: PathBuf,
    ops: Vec<EditOp>,
}

#[derive(Debug)]
struct DecodedFrame {
    generation: u64,
    path: PathBuf,
    pixels: Result<RawFrame, String>,
}

/// One coalescing decode worker per viewer. When the user holds an arrow
/// key, only the latest queued request survives; a decode already in flight
/// may finish, but its generation is ignored by the UI.
struct ViewerLoader {
    pending: Arc<(Mutex<Option<LoadRequest>>, Condvar)>,
    results: Arc<Mutex<Option<DecodedFrame>>>,
    stopped: Arc<AtomicBool>,
}

impl ViewerLoader {
    fn new() -> Self {
        let pending = Arc::new((Mutex::new(None::<LoadRequest>), Condvar::new()));
        let results = Arc::new(Mutex::new(None));
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_pending = pending.clone();
        let worker_results = results.clone();
        let worker_stopped = stopped.clone();
        std::thread::Builder::new()
            .name("bigiris-viewer-decode".to_string())
            .spawn(move || loop {
                let request = {
                    let (lock, ready) = &*worker_pending;
                    let mut slot = lock.lock().expect("viewer decode queue poisoned");
                    while slot.is_none() && !worker_stopped.load(Ordering::Relaxed) {
                        slot = ready.wait(slot).expect("viewer decode queue poisoned");
                    }
                    if worker_stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    slot.take().expect("request checked above")
                };
                let pixels = crate::codec::with_cancel(worker_stopped.clone(), || {
                    decode_frame(&request.path, &request.ops)
                })
                .map_err(|e| e.to_string());
                if worker_stopped.load(Ordering::Relaxed) {
                    break;
                }
                // Replace stale results instead of retaining unbounded RGBA buffers.
                *worker_results.lock().expect("viewer result slot poisoned") = Some(DecodedFrame {
                    generation: request.generation,
                    path: request.path,
                    pixels,
                });
            })
            .expect("could not start viewer decoder");
        Self { pending, results, stopped }
    }

    fn request(&self, request: LoadRequest) {
        let (lock, ready) = &*self.pending;
        *lock.lock().expect("viewer decode queue poisoned") = Some(request);
        ready.notify_one();
    }

    fn take_result(&self) -> Option<DecodedFrame> {
        self.results.lock().expect("viewer result slot poisoned").take()
    }

    fn stop(&self) {
        // Hold the condition-variable mutex to avoid a lost wakeup during shutdown.
        let (lock, ready) = &*self.pending;
        let mut slot = lock.lock().expect("viewer decode queue poisoned");
        self.stopped.store(true, Ordering::Relaxed);
        *slot = None;
        ready.notify_one();
    }
}

impl Drop for ViewerLoader {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Debug, Clone, Copy)]
enum ZoomState {
    /// Fit inside viewport, preserve aspect, never enlarge past 1:1.
    Fit,
    /// Explicit scale factor; `1.0` = actual pixel size.
    Scale(f64),
}

impl ViewerState {
    /// Return a strong handle to the owning window. Callbacks always run
    /// while the window is still alive (GTK only dispatches signals to
    /// live widgets), so the upgrade is guaranteed to succeed; we panic
    /// on the impossible `None` rather than thread `Option` through
    /// every single method call. Use `window_weak.upgrade()` directly
    /// when you genuinely want the `Option<...>` semantics (e.g. inside
    /// a deferred timeout that may fire post-close).
    fn window(&self) -> adw::ApplicationWindow {
        self.window_weak.upgrade().expect("window must outlive the state it owns")
    }
}

fn build_window(
    app: &adw::Application,
    files: Rc<Vec<PathBuf>>,
    start_idx: usize,
) -> adw::ApplicationWindow {
    build_window_tracked(app, files, start_idx).0
}

fn build_window_tracked(
    app: &adw::Application,
    files: Rc<Vec<PathBuf>>,
    start_idx: usize,
) -> (adw::ApplicationWindow, std::rc::Weak<RefCell<ViewerState>>) {
    let settings = app_settings();
    let window_width = settings.as_ref().map_or(1280, |s| s.int("window-width")).max(480);
    let window_height = settings.as_ref().map_or(800, |s| s.int("window-height")).max(360);
    let start_maximized = settings.as_ref().is_some_and(|s| s.boolean("window-maximized"));
    let autohide_ui = settings.as_ref().is_none_or(|s| s.boolean("autohide-ui"));
    let autohide_delay_ms =
        settings.as_ref().map_or(DEFAULT_EDGE_HIDE_MS, |s| u64::from(s.uint("autohide-delay-ms")));
    let filmstrip_visible = settings.as_ref().is_none_or(|s| s.boolean("filmstrip-visible"));
    let slideshow_interval_s =
        settings.as_ref().map_or(DEFAULT_SLIDESHOW_INTERVAL_S, |s| s.uint("slideshow-interval-s"));
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(window_width)
        .default_height(window_height)
        .build();
    if start_maximized {
        window.maximize();
    }

    let title = adw::WindowTitle::new("BigIris", "");
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));

    // As ações do menu hambúrguer precisam do `state`, então criamos o
    // botão depois que o state existe — veja mais abaixo. Header fica com
    // os quick-buttons (info/fullscreen) agora.

    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    picture.set_halign(gtk::Align::Center);
    picture.set_valign(gtk::Align::Center);

    let scroller = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&picture)
        .build();

    let status = build_dashboard(&window);

    // Stack between empty-state and the viewport so we don't flash a blank
    // area when the app is launched without files.
    let stack = gtk::Stack::new();
    stack.add_named(&status, Some("empty"));
    stack.add_named(&scroller, Some("picture"));

    // OSD pill (mpv-style): filename · index · zoom. Hidden by default,
    // pulses on nav/zoom. Crossfade in, slide-down on hide so it feels
    // ambient rather than chrome-y.
    let osd_label = gtk::Label::builder()
        .label("")
        .css_classes(["osd", "title-4"])
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    let osd_bin = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    osd_bin.add_css_class("osd");
    osd_bin.add_css_class("card");
    osd_bin.set_halign(gtk::Align::Center);
    osd_bin.append(&osd_label);
    let osd_revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::Crossfade)
        .transition_duration(220)
        .reveal_child(false)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::End)
        .margin_bottom(120)
        .can_target(false)
        .child(&osd_bin)
        .build();

    // Film strip: horizontal row of thumbnail buttons, revealed only when
    // the cursor touches the bottom edge zone. Thumbs are built up lazily
    // (one per idle tick) so huge selections don't stall startup.
    let film_strip_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    film_strip_box.add_css_class("film-strip");
    let film_strip_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .child(&film_strip_box)
        .height_request(96)
        .build();

    let film_strip_buttons = Vec::new();

    let strip_revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideUp)
        .transition_duration(220)
        .reveal_child(false)
        .valign(gtk::Align::End)
        .halign(gtk::Align::Fill)
        .child(&film_strip_scroll)
        .build();
    // If there are fewer than two files the strip is meaningless — keep
    // it collapsed regardless of edge reveals.
    if files.len() < 2 || !filmstrip_visible {
        strip_revealer.set_can_target(false);
        strip_revealer.set_visible(false);
    }

    // Histograma RGB (overlay canto superior-direito). Oculto por padrão;
    // tecla `G` alterna. Tamanho fixo porque o frame sempre renderiza
    // 256 bins — deixar o widget esticar distorceria as colunas.
    let histogram_area = gtk::DrawingArea::builder()
        .width_request(260)
        .height_request(104)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .margin_top(8)
        .margin_end(8)
        .visible(false)
        .build();
    histogram_area.add_css_class("histogram-overlay");

    let loading_spinner = gtk::Spinner::builder()
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .visible(false)
        .build();
    loading_spinner.add_css_class("viewer-loading");

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&stack));
    overlay.add_overlay(&osd_revealer);
    overlay.add_overlay(&strip_revealer);
    overlay.add_overlay(&histogram_area);
    overlay.add_overlay(&loading_spinner);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&overlay));
    // The first contact exposes the controls. It auto-hides shortly after
    // the image appears, while edge reveal keeps the canvas uncluttered.
    toolbar_view.set_reveal_top_bars(true);
    window.set_content(Some(&toolbar_view));

    // Theater mode on by default: photos look best on black, and it's what
    // Apple Photos / Lightroom / mpv converge on for viewing surfaces.
    window.add_css_class("theater");
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_lowercase();
    if desktop.contains("kde") || std::env::var_os("KDE_FULL_SESSION").is_some() {
        window.add_css_class("plasma");
    }

    let state = Rc::new(RefCell::new(ViewerState {
        files: files.clone(),
        idx: start_idx.min(files.len().saturating_sub(1)),
        picture: picture.clone(),
        scroller: scroller.clone(),
        title: title.clone(),
        window_weak: gtk::glib::object::ObjectExt::downgrade(&window),
        zoom: ZoomState::Fit,
        texture: None,
        zoom_pointer: None,
        pending_zoom: None,
        zoom_source: None,
        natural: (0, 0),
        toolbar_view: toolbar_view.clone(),
        osd_revealer: osd_revealer.clone(),
        osd_label: osd_label.clone(),
        strip_revealer: strip_revealer.clone(),
        film_strip_scroll: film_strip_scroll.clone(),
        film_strip_buttons: film_strip_buttons.clone(),
        film_strip_start: 0,
        film_strip_box: film_strip_box.clone(),
        thumbnail_cancel: Arc::new(AtomicBool::new(false)),
        ui_pinned: false,
        autohide_ui,
        autohide_delay_ms,
        slideshow_interval_s,
        theater_enabled: true,
        top_hide_source: None,
        bottom_hide_source: None,
        osd_hide_source: None,
        cursor_hide_source: None,
        slideshow_source: None,
        histogram_area: histogram_area.clone(),
        histogram: [0u32; 768],
        loader: ViewerLoader::new(),
        load_generation: 0,
        loading_spinner: loading_spinner.clone(),
        props_dialog: None,
        pending_ops: Vec::new(),
        redo_ops: Vec::new(),
        saving: false,
    }));

    if let Some(settings) = settings {
        let window = window.clone();
        window.connect_close_request(move |window| {
            let maximized = window.is_maximized();
            let _ = settings.set_boolean("window-maximized", maximized);
            if !maximized {
                let _ = settings.set_int("window-width", window.width().max(480));
                let _ = settings.set_int("window-height", window.height().max(360));
            }
            glib::Propagation::Proceed
        });
    }

    // Closing is also a navigation boundary: never lose an edit because Esc
    // or the title-bar close button was pressed accidentally.
    {
        let state = state.clone();
        window.connect_close_request(move |window| {
            if state.borrow().saving {
                state.borrow().osd_label.set_text(&gettext("Aguarde o salvamento terminar"));
                pulse_osd(&state);
                return glib::Propagation::Stop;
            }
            if state.borrow().pending_ops.is_empty() {
                return glib::Propagation::Proceed;
            }
            if window.visible_dialog().is_some() {
                return glib::Propagation::Stop;
            }
            let alert = adw::AlertDialog::builder()
                .heading(gettext("Descartar alterações?"))
                .body(gettext("A imagem tem edições que ainda não foram salvas."))
                .build();
            alert.add_response("cancel", &gettext("Continuar editando"));
            alert.add_response("discard", &gettext("Descartar"));
            alert.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
            alert.set_default_response(Some("cancel"));
            alert.set_close_response("cancel");
            let state = state.clone();
            let closing_window = window.clone();
            alert.connect_response(Some("discard"), move |_, _| {
                let mut s = state.borrow_mut();
                s.pending_ops.clear();
                s.redo_ops.clear();
                drop(s);
                closing_window.close();
            });
            alert.present(Some(window));
            glib::Propagation::Stop
        });
    }

    {
        let state = Rc::downgrade(&state);
        window.connect_destroy(move |_| {
            if let Some(state) = state.upgrade() {
                let mut s = state.borrow_mut();
                s.loader.stop();
                for source in [
                    s.top_hide_source.take(),
                    s.bottom_hide_source.take(),
                    s.osd_hide_source.take(),
                    s.cursor_hide_source.take(),
                    s.slideshow_source.take(),
                    s.zoom_source.take(),
                ]
                .into_iter()
                .flatten()
                {
                    source.remove();
                }
                s.props_dialog.take();
                s.texture.take();
                s.pending_zoom = None;
                s.picture.set_paintable(None::<&gdk::Paintable>);
            }
        });
    }

    // draw callback lê o snapshot mais recente do histograma em state.
    {
        let state = Rc::downgrade(&state);
        histogram_area.set_draw_func(move |_, cr, w, h| {
            if let Some(state) = state.upgrade() {
                draw_histogram(&state, cr, w, h);
            }
        });
    }

    // Quick-buttons do header (esquerda) e menu hamburger (direita).
    // Registrados aqui porque todos fecham sobre `state`.
    install_viewer_actions(app, &window, &state);
    let open_btn = gtk::Button::from_icon_name("document-open-symbolic");
    open_btn.set_tooltip_text(Some(&gettext("Abrir imagens (Ctrl+O)")));
    open_btn.set_action_name(Some("win.open"));
    header.pack_start(&open_btn);

    let previous_btn = gtk::Button::from_icon_name("go-previous-symbolic");
    previous_btn.set_tooltip_text(Some(&gettext("Imagem anterior")));
    previous_btn.set_action_name(Some("win.previous"));
    previous_btn.set_visible(!files.is_empty());
    header.pack_start(&previous_btn);

    let next_btn = gtk::Button::from_icon_name("go-next-symbolic");
    next_btn.set_tooltip_text(Some(&gettext("Próxima imagem")));
    next_btn.set_action_name(Some("win.next"));
    next_btn.set_visible(!files.is_empty());
    header.pack_start(&next_btn);

    let info_btn = gtk::Button::from_icon_name("dialog-information-symbolic");
    info_btn.set_tooltip_text(Some(&gettext("Propriedades da imagem (Ctrl+I)")));
    info_btn.set_action_name(Some("win.info"));
    // Só faz sentido com imagem carregada — na dashboard fica escondido.
    // Fullscreen fica como F11 e double-click, sem botão dedicado no header
    // A barra já mantém os controles de janela no lado direito.
    let has_files = !state.borrow().files.is_empty();
    info_btn.set_visible(has_files);
    header.pack_start(&info_btn);
    header.pack_end(&build_header_menu_button(&state));

    let fullscreen_btn = gtk::Button::from_icon_name("view-fullscreen-symbolic");
    fullscreen_btn.set_tooltip_text(Some(&gettext("Tela cheia (F11)")));
    fullscreen_btn.set_action_name(Some("win.fullscreen"));
    fullscreen_btn.set_visible(has_files);
    header.pack_end(&fullscreen_btn);

    let zoom_in_btn = gtk::Button::from_icon_name("zoom-in-symbolic");
    zoom_in_btn.set_tooltip_text(Some(&gettext("Aproximar")));
    zoom_in_btn.set_action_name(Some("win.zoom-in"));
    zoom_in_btn.set_visible(has_files);
    header.pack_end(&zoom_in_btn);

    let fit_btn = gtk::Button::from_icon_name("zoom-fit-best-symbolic");
    fit_btn.set_tooltip_text(Some(&gettext("Ajustar à janela")));
    fit_btn.set_action_name(Some("win.zoom-fit"));
    fit_btn.set_visible(has_files);
    header.pack_end(&fit_btn);

    let zoom_out_btn = gtk::Button::from_icon_name("zoom-out-symbolic");
    zoom_out_btn.set_tooltip_text(Some(&gettext("Afastar")));
    zoom_out_btn.set_action_name(Some("win.zoom-out"));
    zoom_out_btn.set_visible(has_files);
    header.pack_end(&zoom_out_btn);

    // Poll decoded frames without blocking GTK. At most the newest valid
    // generation is painted, so fast directory navigation remains smooth.
    {
        let state = state.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
            if state.borrow().window_weak.upgrade().is_none_or(|window| !window.is_visible()) {
                state.borrow().loader.stop();
                return glib::ControlFlow::Break;
            }
            let frame = state.borrow().loader.take_result();
            if let Some(frame) = frame {
                apply_decoded_frame(&state, frame);
            }
            glib::ControlFlow::Continue
        });
    }

    if has_files {
        stack.set_visible_child_name("picture");
        update_display(&state);
    } else {
        stack.set_visible_child_name("empty");
        title.set_title("BigIris");
    }

    // Keyboard: file nav, zoom presets, fullscreen, quit.
    let key_controller = gtk::EventControllerKey::new();
    {
        let state = Rc::downgrade(&state);
        key_controller.connect_key_pressed(move |_, key, _code, modifier| {
            state
                .upgrade()
                .map_or(glib::Propagation::Proceed, |state| handle_key(&state, key, modifier))
        });
    }
    window.add_controller(key_controller);

    // Mouse-wheel zoom — live on the window so revealed chrome (film strip,
    // header) doesn't steal wheel events. Capture phase pre-empts the
    // ScrolledWindow's own scroll-to-scroll handler on the image area.
    let scroll_controller =
        gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    scroll_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let state = Rc::downgrade(&state);
        let scroller = scroller.downgrade();
        scroll_controller.connect_scroll(move |controller, _dx, dy| {
            let Some(state) = state.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let Some(scroller) = scroller.upgrade() else {
                return glib::Propagation::Proceed;
            };
            // The scroll controller lives on the window, so event.position()
            // is in window-root coords. Convert to scroller-local coords so
            // adjust_zoom can anchor the re-scroll on the pixel under the
            // pointer: root.compute_point(&scroller, pt) — NOT the reverse.
            let cursor =
                controller.current_event().and_then(|e| e.position()).and_then(|(wx, wy)| {
                    let root = scroller.root()?;
                    let pt = graphene::Point::new(wx as f32, wy as f32);
                    root.compute_point(&scroller, &pt).map(|p| (f64::from(p.x()), f64::from(p.y())))
                });
            if dy < -0.01 {
                adjust_zoom(&state, ZOOM_STEP, cursor);
                pulse_osd(&state);
                glib::Propagation::Stop
            } else if dy > 0.01 {
                adjust_zoom(&state, 1.0 / ZOOM_STEP, cursor);
                pulse_osd(&state);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }
    window.add_controller(scroll_controller);

    let zoom_motion = gtk::EventControllerMotion::new();
    zoom_motion.set_name(Some("bigiris-zoom-pointer"));
    for entering in [true, false] {
        let weak = Rc::downgrade(&state);
        let record = move |_: &gtk::EventControllerMotion, x, y| {
            if let Some(state) = weak.upgrade() {
                state.borrow_mut().zoom_pointer = Some((x, y));
            }
        };
        if entering {
            zoom_motion.connect_enter(record);
        } else {
            zoom_motion.connect_motion(record);
        }
    }
    scroller.add_controller(zoom_motion);

    // Double-click anywhere on the image → toggle fullscreen (matches the
    // F11 shortcut). Uses GestureClick with n-press=2 so single clicks still
    // fall through to the drag-to-pan gesture.
    let double_click = gtk::GestureClick::builder().button(1).build();
    double_click.set_propagation_phase(gtk::PropagationPhase::Bubble);
    {
        let state = Rc::downgrade(&state);
        double_click.connect_pressed(move |_, n_press, _, _| {
            let Some(state) = state.upgrade() else {
                return;
            };
            if n_press == 2 {
                let win = state.borrow().window().clone();
                win.set_fullscreened(!win.is_fullscreen());
            }
        });
    }
    scroller.add_controller(double_click);

    // Drag-to-pan with any mouse button. GestureDrag gives us deltas relative
    // to the drag start, so we just save the scroller's adjustment values at
    // `drag_begin` and subtract from there on every `drag_update`.
    let drag = gtk::GestureDrag::builder().button(0).build();
    drag.set_propagation_phase(gtk::PropagationPhase::Capture);
    let drag_anchor: Rc<Cell<(f64, f64)>> = Rc::new(Cell::new((0.0, 0.0)));
    {
        let scr = scroller.downgrade();
        let anchor = drag_anchor.clone();
        drag.connect_drag_begin(move |_, _, _| {
            let Some(scr) = scr.upgrade() else {
                return;
            };
            anchor.set((scr.hadjustment().value(), scr.vadjustment().value()));
        });
    }
    {
        let scr = scroller.downgrade();
        let anchor = drag_anchor.clone();
        drag.connect_drag_update(move |_, dx, dy| {
            let Some(scr) = scr.upgrade() else {
                return;
            };
            let (hx0, vy0) = anchor.get();
            scr.hadjustment().set_value(hx0 - dx);
            scr.vadjustment().set_value(vy0 - dy);
        });
    }
    scroller.add_controller(drag);

    let weak_state = Rc::downgrade(&state);
    window.connect_unrealize(move |_| {
        if let Some(state) = weak_state.upgrade() {
            state.borrow().thumbnail_cancel.store(true, Ordering::Release);
        }
    });
    rebuild_film_strip(&state);

    // Edge-triggered reveal: cursor near the top shows the header, near
    // the bottom shows the film strip, everywhere else only restores the
    // cursor. Each zone has its own hide timer.
    let motion = gtk::EventControllerMotion::new();
    {
        let state = Rc::downgrade(&state);
        motion.connect_motion(move |_, _x, y| {
            if let Some(state) = state.upgrade() {
                on_pointer_motion(&state, y);
            }
        });
    }
    window.add_controller(motion);

    // Kick off: controls are discoverable, then get out of the way.
    refresh_film_strip_active(&state);
    refresh_osd(&state);
    pulse_osd(&state);
    reveal_top(&state);
    schedule_cursor_hide(&state);

    (window, Rc::downgrade(&state))
}

/// Decode a small film-strip frame off the GTK thread. The source codec may
/// still need to read the full image, but only the downscaled RGBA buffer is
/// handed to GDK/GPU memory.
fn decode_thumbnail(path: &Path) -> Result<(Vec<u8>, u32, u32), Box<dyn std::error::Error>> {
    let img = bigimage_core::open_image(path)?;
    let thumb = img.thumbnail(THUMB_EDGE, THUMB_EDGE).to_rgba8();
    let (w, h) = thumb.dimensions();
    Ok((thumb.into_raw(), w, h))
}

/// Only 41 widgets and thumbnail requests exist, independently of folder size.
fn rebuild_film_strip(state: &Rc<RefCell<ViewerState>>) {
    const PAGE: usize = 41;
    let (container, files, start, cancel, window) = {
        let mut s = state.borrow_mut();
        s.thumbnail_cancel.store(true, Ordering::Release);
        s.thumbnail_cancel = Arc::new(AtomicBool::new(false));
        s.film_strip_start = s.idx / PAGE * PAGE;
        let container = s.film_strip_box.clone();
        s.film_strip_buttons.clear();
        (
            container,
            s.files.clone(),
            s.film_strip_start,
            s.thumbnail_cancel.clone(),
            s.window().downgrade(),
        )
    };
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    let mut buttons = Vec::new();
    for (offset, path) in files.iter().skip(start).take(PAGE).enumerate() {
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_size_request(THUMB_EDGE as i32, THUMB_EDGE as i32);
        let button = gtk::Button::builder().child(&picture).build();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        button.set_tooltip_text(Some(&name));
        button.update_property(&[gtk::accessible::Property::Label(&name)]);
        button.add_css_class("flat");
        let weak = Rc::downgrade(state);
        button.connect_clicked(move |_| {
            let Some(state) = weak.upgrade() else { return };
            if !can_leave_current(&state) {
                return;
            }
            {
                let mut s = state.borrow_mut();
                s.idx = start + offset;
                s.zoom = ZoomState::Fit;
            }
            update_display(&state);
            pulse_osd(&state);
        });
        container.append(&button);
        buttons.push(button);
    }
    let weak_buttons: Vec<_> = buttons.iter().map(|b| b.downgrade()).collect();
    state.borrow_mut().film_strip_buttons = buttons;
    let paths: Vec<_> = files.iter().skip(start).take(PAGE).cloned().collect();
    let (tx, rx) = mpsc::sync_channel(2);
    std::thread::spawn(move || {
        crate::codec::with_cancel(cancel.clone(), || {
            for (index, path) in paths.iter().enumerate() {
                if cancel.load(Ordering::Acquire) {
                    break;
                }
                if let Ok((raw, w, h)) = decode_thumbnail(path) {
                    if tx.send((index, raw, w, h)).is_err() {
                        break;
                    }
                }
            }
        })
    });
    glib::timeout_add_local(Duration::from_millis(32), move || {
        if window.upgrade().is_none_or(|w| !w.is_visible()) {
            return glib::ControlFlow::Break;
        }
        loop {
            match rx.try_recv() {
                Ok((index, raw, w, h)) => {
                    if let Some(button) = weak_buttons.get(index).and_then(|w| w.upgrade()) {
                        if let Some(picture) = button.child().and_downcast::<gtk::Picture>() {
                            picture.set_paintable(Some(&texture_from_rgba(raw, w, h)));
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => return glib::ControlFlow::Break,
            }
        }
    });
}

/// Toggle `.active-thumb` so the film strip mirrors the current selection.
/// Also autoscrolls the strip so the active thumb stays in view; uses the
/// button's computed bounds (allocation() was deprecated in GTK 4.12).
fn refresh_film_strip_active(state: &Rc<RefCell<ViewerState>>) {
    let needs_rebuild = {
        let s = state.borrow();
        s.idx < s.film_strip_start || s.idx >= s.film_strip_start + s.film_strip_buttons.len()
    };
    if needs_rebuild {
        rebuild_film_strip(state);
    }
    let s = state.borrow();
    let strip = &s.film_strip_scroll;
    let viewport_w = strip.width() as f64;
    for (i, btn) in s.film_strip_buttons.iter().enumerate() {
        if i + s.film_strip_start == s.idx {
            btn.add_css_class("active-thumb");
            // compute_bounds returns coords in the parent (the HBox inside
            // the scroll's viewport); that's what hadjustment speaks.
            if let Some(parent) = btn.parent() {
                if let Some(rect) = btn.compute_bounds(&parent) {
                    if rect.width() > 0.0 {
                        let target = rect.x() as f64 - (viewport_w - rect.width() as f64) / 2.0;
                        strip.hadjustment().set_value(target.max(0.0));
                    }
                }
            }
        } else {
            btn.remove_css_class("active-thumb");
        }
    }
}

/// Register a CssProvider carrying [`THEATER_CSS`] on the default display.
/// Install once per thread/display so repeated activation retains no providers.
fn install_theater_css() {
    thread_local! { static INSTALLED: Cell<bool> = const { Cell::new(false) }; }
    INSTALLED.with(|installed| {
        if installed.get() {
            return;
        }
        if let Some(display) = gdk::Display::default() {
            let provider = gtk::CssProvider::new();
            provider.load_from_string(THEATER_CSS);
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            installed.set(true);
        }
    });
}

/// Return settings only when the schema is installed. Development builds
/// continue to run from the source tree without requiring a global schema.
fn app_settings() -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    let schema = source.lookup(APP_ID, true)?;
    Some(gio::Settings::new_full(&schema, None::<&gio::SettingsBackend>, None))
}

/// Manual film-strip reveal (F key). Just forwards to [`reveal_bottom`]
/// so it follows the same edge-hide timing as the hover path.
fn toggle_film_strip(state: &Rc<RefCell<ViewerState>>) {
    if state.borrow().film_strip_buttons.len() < 2 {
        return;
    }
    let currently_revealed = state.borrow().strip_revealer.reveals_child();
    if currently_revealed {
        hide_bottom(state);
    } else {
        reveal_bottom(state);
    }
}

/// Flip theater mode. Adds/removes the `.theater` CSS class on the window
/// so the scoped rules in [`THEATER_CSS`] kick in.
fn toggle_theater(state: &Rc<RefCell<ViewerState>>) {
    let (window, enable) = {
        let mut s = state.borrow_mut();
        s.theater_enabled = !s.theater_enabled;
        (s.window().clone(), s.theater_enabled)
    };
    if enable {
        window.add_css_class("theater");
    } else {
        window.remove_css_class("theater");
    }
}

/// Compose the OSD pill text for the current file/zoom, mpv-style:
/// `name.jpg · 3/12 · 85%`.
fn osd_text(state: &Rc<RefCell<ViewerState>>) -> String {
    let s = state.borrow();
    if s.files.is_empty() {
        return String::new();
    }
    let path = &s.files[s.idx];
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let zoom = match s.zoom {
        ZoomState::Fit => "Ajustar".to_string(),
        ZoomState::Scale(x) => format!("{}%", (x * 100.0).round() as i32),
    };
    format!("{} · {}/{} · {}", name, s.idx + 1, s.files.len(), zoom)
}

/// Sync the OSD label to the current file/zoom without touching visibility.
fn refresh_osd(state: &Rc<RefCell<ViewerState>>) {
    let text = osd_text(state);
    let s = state.borrow();
    s.osd_label.set_label(&text);
    s.osd_revealer.set_visible(!text.is_empty());
}

/// Pointer moved — route to the appropriate zone. Called on every motion
/// event, so keep the work cheap: it's mostly math + timer juggling.
fn on_pointer_motion(state: &Rc<RefCell<ViewerState>>, y: f64) {
    // Always restore the cursor and restart its own idle timer.
    {
        let s = state.borrow();
        s.window().set_cursor(None);
    }
    cancel_cursor_hide(state);
    schedule_cursor_hide(state);

    let (window_h, pinned) = {
        let s = state.borrow();
        (s.window().height() as f64, s.ui_pinned)
    };
    if pinned {
        reveal_top(state);
        reveal_bottom(state);
        return;
    }
    if y <= TOP_EDGE_PX {
        reveal_top(state);
    } else if y >= window_h - BOTTOM_EDGE_PX {
        reveal_bottom(state);
    }
}

/// Slide the header bar into view and (re)arm its hide timer.
fn reveal_top(state: &Rc<RefCell<ViewerState>>) {
    {
        let s = state.borrow();
        s.toolbar_view.set_reveal_top_bars(true);
    }
    cancel_top_hide(state);
    if state.borrow().ui_pinned || !state.borrow().autohide_ui {
        return;
    }
    let delay = state.borrow().autohide_delay_ms;
    let clone = state.clone();
    let id = glib::timeout_add_local_once(std::time::Duration::from_millis(delay), move || {
        hide_top(&clone)
    });
    state.borrow_mut().top_hide_source = Some(id);
}

fn hide_top(state: &Rc<RefCell<ViewerState>>) {
    let mut s = state.borrow_mut();
    s.top_hide_source = None;
    let window = s.window();
    let keyboard_in_header = gtk::prelude::GtkWindowExt::focus(&window).is_some_and(|widget| {
        !widget.is_ancestor(&s.scroller) && !widget.is_ancestor(&s.film_strip_scroll)
    });
    if !keyboard_in_header && !adw::StyleManager::default().is_high_contrast() {
        s.toolbar_view.set_reveal_top_bars(false);
    }
}

fn cancel_top_hide(state: &Rc<RefCell<ViewerState>>) {
    if let Some(id) = state.borrow_mut().top_hide_source.take() {
        id.remove();
    }
}

/// Slide the film strip into view (skips the motion when files < 2) and
/// (re)arm its hide timer.
fn reveal_bottom(state: &Rc<RefCell<ViewerState>>) {
    {
        let s = state.borrow();
        if s.film_strip_buttons.len() < 2 {
            return;
        }
        s.strip_revealer.set_reveal_child(true);
    }
    // A faixa nasce rolada no começo. Como o visualizador agora pode abrir
    // no meio da pasta (clique num arquivo qualquer), a miniatura ativa
    // ficaria fora de vista na primeira vez que o strip aparece. Recentrar
    // fica para o idle seguinte porque, enquanto o revealer não alocou a
    // faixa, `compute_bounds` devolve largura zero e a conta de centragem
    // não teria em que se apoiar.
    {
        let clone = state.clone();
        glib::idle_add_local_once(move || refresh_film_strip_active(&clone));
    }
    cancel_bottom_hide(state);
    if state.borrow().ui_pinned || !state.borrow().autohide_ui {
        return;
    }
    let delay = state.borrow().autohide_delay_ms;
    let clone = state.clone();
    let id = glib::timeout_add_local_once(std::time::Duration::from_millis(delay), move || {
        hide_bottom(&clone)
    });
    state.borrow_mut().bottom_hide_source = Some(id);
}

fn hide_bottom(state: &Rc<RefCell<ViewerState>>) {
    let mut s = state.borrow_mut();
    s.bottom_hide_source = None;
    let keyboard_in_strip = gtk::prelude::GtkWindowExt::focus(&s.window())
        .is_some_and(|widget| widget.is_ancestor(&s.film_strip_scroll));
    if !keyboard_in_strip && !adw::StyleManager::default().is_high_contrast() {
        s.strip_revealer.set_reveal_child(false);
    }
}

fn cancel_bottom_hide(state: &Rc<RefCell<ViewerState>>) {
    if let Some(id) = state.borrow_mut().bottom_hide_source.take() {
        id.remove();
    }
}

/// Show the OSD pill for `OSD_PULSE_MS` on file-change / zoom. The pill
/// is bottom-centered above the strip area so it's visible without any
/// chrome taking over.
fn pulse_osd(state: &Rc<RefCell<ViewerState>>) {
    {
        let s = state.borrow();
        if s.osd_label.label().is_empty() {
            return;
        }
        s.osd_revealer.set_reveal_child(true);
    }
    cancel_osd_hide(state);
    if state.borrow().ui_pinned {
        return;
    }
    let clone = state.clone();
    let id =
        glib::timeout_add_local_once(std::time::Duration::from_millis(OSD_PULSE_MS), move || {
            hide_osd(&clone)
        });
    state.borrow_mut().osd_hide_source = Some(id);
}

fn hide_osd(state: &Rc<RefCell<ViewerState>>) {
    let mut s = state.borrow_mut();
    s.osd_hide_source = None;
    s.osd_revealer.set_reveal_child(false);
}

fn cancel_osd_hide(state: &Rc<RefCell<ViewerState>>) {
    if let Some(id) = state.borrow_mut().osd_hide_source.take() {
        id.remove();
    }
}

fn schedule_cursor_hide(state: &Rc<RefCell<ViewerState>>) {
    let s = state.borrow();
    if s.ui_pinned || !s.autohide_ui || adw::StyleManager::default().is_high_contrast() {
        return;
    }
    // AdwDialog (incluindo AdwAboutDialog) a partir da libadwaita 1.5 é
    // um modal in-window — compartilha a surface com o viewer. Se o
    // timer disparar enquanto o diálogo estiver aberto, o cursor sumiria
    // no próprio diálogo (o user não consegue clicar em "Detalhes" etc).
    // Pular a agenda enquanto houver diálogo visível na janela.
    if s.window().visible_dialog().is_some() {
        return;
    }
    drop(s);
    let clone = state.clone();
    let id =
        glib::timeout_add_local_once(std::time::Duration::from_millis(CURSOR_HIDE_MS), move || {
            // Double-check na hora do disparo: user pode ter aberto um
            // diálogo nos últimos CURSOR_HIDE_MS ms.
            let s = clone.borrow_mut();
            if s.window().visible_dialog().is_some() {
                drop(s);
                clone.borrow_mut().cursor_hide_source = None;
                return;
            }
            let cursor = gdk::Cursor::from_name("none", None);
            s.window().set_cursor(cursor.as_ref());
            drop(s);
            clone.borrow_mut().cursor_hide_source = None;
        });
    state.borrow_mut().cursor_hide_source = Some(id);
}

fn cancel_cursor_hide(state: &Rc<RefCell<ViewerState>>) {
    if let Some(id) = state.borrow_mut().cursor_hide_source.take() {
        id.remove();
    }
}

/// Flip the accessibility pin: when pinned, every chrome element stays
/// visible and no timer fires. When unpinned, we re-enter the natural
/// edge-triggered flow.
fn toggle_ui_pin(state: &Rc<RefCell<ViewerState>>) {
    let pinned = {
        let mut s = state.borrow_mut();
        s.ui_pinned = !s.ui_pinned;
        s.ui_pinned
    };
    if pinned {
        cancel_top_hide(state);
        cancel_bottom_hide(state);
        cancel_osd_hide(state);
        cancel_cursor_hide(state);
        let s = state.borrow();
        s.toolbar_view.set_reveal_top_bars(true);
        if s.film_strip_buttons.len() >= 2 {
            s.strip_revealer.set_reveal_child(true);
        }
        if !s.osd_label.label().is_empty() {
            s.osd_revealer.set_reveal_child(true);
        }
        s.window().set_cursor(None);
    } else {
        let s = state.borrow();
        if !s.autohide_ui {
            s.toolbar_view.set_reveal_top_bars(true);
            if s.film_strip_buttons.len() >= 2 {
                s.strip_revealer.set_reveal_child(true);
            }
            s.window().set_cursor(None);
            return;
        }
        // Collapse back to the clean default. Upcoming motion events will
        // re-reveal edges on demand.
        s.toolbar_view.set_reveal_top_bars(false);
        s.strip_revealer.set_reveal_child(false);
        s.osd_revealer.set_reveal_child(false);
        drop(s);
        schedule_cursor_hide(state);
    }
}

fn handle_key(
    state: &Rc<RefCell<ViewerState>>,
    key: gdk::Key,
    _modifier: gdk::ModifierType,
) -> glib::Propagation {
    use gdk::Key;

    match key {
        Key::Escape => {
            state.borrow().window().close();
            return glib::Propagation::Stop;
        }
        Key::F11 => {
            let win = state.borrow().window().clone();
            win.set_fullscreened(!win.is_fullscreen());
            return glib::Propagation::Stop;
        }
        Key::h | Key::H => {
            toggle_ui_pin(state);
            return glib::Propagation::Stop;
        }
        Key::t | Key::T => {
            toggle_theater(state);
            return glib::Propagation::Stop;
        }
        Key::f | Key::F => {
            toggle_film_strip(state);
            return glib::Propagation::Stop;
        }
        Key::plus | Key::equal | Key::KP_Add => {
            zoom_in(state);
            pulse_osd(state);
            return glib::Propagation::Stop;
        }
        Key::minus | Key::KP_Subtract => {
            zoom_out(state);
            pulse_osd(state);
            return glib::Propagation::Stop;
        }
        Key::_0 | Key::KP_0 => {
            set_zoom(state, ZoomState::Fit);
            pulse_osd(state);
            return glib::Propagation::Stop;
        }
        Key::_1 | Key::KP_1 => {
            set_zoom(state, ZoomState::Scale(1.0));
            pulse_osd(state);
            return glib::Propagation::Stop;
        }
        Key::space => {
            toggle_slideshow(state);
            return glib::Propagation::Stop;
        }
        Key::g | Key::G => {
            toggle_histogram(state);
            return glib::Propagation::Stop;
        }
        _ => {}
    }

    let total = state.borrow().files.len();
    if total == 0 {
        return glib::Propagation::Proceed;
    }

    let new_idx = match key {
        Key::Right | Key::Page_Down => Some((state.borrow().idx + 1) % total),
        Key::Left | Key::Page_Up | Key::BackSpace => {
            let cur = state.borrow().idx;
            Some(if cur == 0 { total - 1 } else { cur - 1 })
        }
        Key::Home => Some(0),
        Key::End => Some(total - 1),
        _ => None,
    };

    match new_idx {
        Some(idx) => {
            if !can_leave_current(state) {
                return glib::Propagation::Stop;
            }
            {
                let mut s = state.borrow_mut();
                s.idx = idx;
                // Reset to Fit on every file switch so each new image
                // shows whole rather than at the previous file's zoom.
                s.zoom = ZoomState::Fit;
            }
            update_display(state);
            pulse_osd(state);
            glib::Propagation::Stop
        }
        None => glib::Propagation::Proceed,
    }
}

/// Avança o índice em +1 (com wrap). Reseta zoom pra Fit como a nav
/// normal faz, pra cada imagem aparecer inteira no slideshow.
fn advance_one(state: &Rc<RefCell<ViewerState>>) -> bool {
    let total = state.borrow().files.len();
    if total < 2 {
        return false;
    }
    if !can_leave_current(state) {
        return false;
    }
    {
        let mut s = state.borrow_mut();
        s.idx = (s.idx + 1) % total;
        s.zoom = ZoomState::Fit;
    }
    update_display(state);
    pulse_osd(state);
    true
}

fn navigate_relative(state: &Rc<RefCell<ViewerState>>, delta: isize) {
    let total = state.borrow().files.len();
    if total < 2 {
        return;
    }
    if !can_leave_current(state) {
        return;
    }
    {
        let mut s = state.borrow_mut();
        s.idx = (s.idx as isize + delta).rem_euclid(total as isize) as usize;
        s.zoom = ZoomState::Fit;
    }
    update_display(state);
    pulse_osd(state);
}

/// Alterna o slideshow (Space). Ignora quando só há um arquivo. Enquanto
/// ativo, um timer configurável avança pra próxima imagem;
/// uma nova pulsação no OSD confirma cada tick.
fn toggle_slideshow(state: &Rc<RefCell<ViewerState>>) {
    let total = state.borrow().files.len();
    if total < 2 {
        return;
    }
    let active = state.borrow().slideshow_source.is_some();
    if active {
        if let Some(id) = state.borrow_mut().slideshow_source.take() {
            id.remove();
        }
        let s = state.borrow();
        s.osd_label.set_text(&gettext("Slideshow: pausado"));
        drop(s);
        pulse_osd(state);
        return;
    }
    let interval = state.borrow().slideshow_interval_s.max(1);
    let clone = state.clone();
    let id = glib::timeout_add_seconds_local(interval, move || {
        // Se a janela fechou entre ticks, encerra — evita mexer em widgets
        // já removidos do tree.
        {
            let s = clone.borrow();
            if !s.window().is_visible() || s.files.is_empty() {
                drop(s);
                clone.borrow_mut().slideshow_source = None;
                return glib::ControlFlow::Break;
            }
        }
        if advance_one(&clone) {
            glib::ControlFlow::Continue
        } else {
            clone.borrow_mut().slideshow_source = None;
            glib::ControlFlow::Break
        }
    });
    state.borrow_mut().slideshow_source = Some(id);
    let s = state.borrow();
    s.osd_label.set_text(&format!("Slideshow: {interval}s"));
    drop(s);
    pulse_osd(state);
}

/// Alterna o overlay de histograma RGB (tecla G). Quando aparece, pulsa
/// o OSD pra confirmar visualmente — o quadrante superior-direito é
/// discreto e sem o pulso o user pode não perceber que ligou.
fn toggle_histogram(state: &Rc<RefCell<ViewerState>>) {
    let now_visible = {
        let s = state.borrow();
        let v = !s.histogram_area.is_visible();
        s.histogram_area.set_visible(v);
        if v {
            s.histogram_area.queue_draw();
        }
        v
    };
    {
        let s = state.borrow();
        s.osd_label.set_text(if now_visible {
            "Histograma: ligado"
        } else {
            "Histograma: desligado"
        });
    }
    pulse_osd(state);
}

/// Renderiza o histograma RGB sobre um fundo translúcido. Normaliza cada
/// canal pelo seu próprio máximo pra que canais muito desbalanceados
/// (ex: foto bem escura) ainda mostrem forma. Bins de 256 são quantizados
/// pra largura do widget usando acúmulo por bucket — assim larguras
/// menores que 256 não perdem picos isolados.
fn draw_histogram(state: &Rc<RefCell<ViewerState>>, cr: &gtk::cairo::Context, w: i32, h: i32) {
    let hist = state.borrow().histogram;
    let w = w as f64;
    let h = h as f64;

    // Fundo — preto translúcido + borda sutil, legível sobre qualquer foto.
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.55);
    cr.rectangle(0.0, 0.0, w, h);
    let _ = cr.fill();
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.15);
    cr.set_line_width(1.0);
    cr.rectangle(0.5, 0.5, w - 1.0, h - 1.0);
    let _ = cr.stroke();

    // Colapsa os 256 bins pra largura do widget. Se w < 256, cada coluna
    // representa um intervalo; somamos os bins pra não perder picos.
    let cols = (w as usize).max(1);
    let mut bars = vec![[0u32; 3]; cols];
    for (i, channel) in hist.chunks(256).enumerate() {
        for (bin, &v) in channel.iter().enumerate() {
            let col = (bin * cols) / 256;
            bars[col][i] = bars[col][i].saturating_add(v);
        }
    }
    let max_per_channel: [u32; 3] = [
        bars.iter().map(|b| b[0]).max().unwrap_or(1).max(1),
        bars.iter().map(|b| b[1]).max().unwrap_or(1).max(1),
        bars.iter().map(|b| b[2]).max().unwrap_or(1).max(1),
    ];
    let inner_h = h - 2.0;
    // Canais em ordem B, G, R — fica mais parecido com Photoshop (R mais
    // destacado no topo dos overlaps aditivos).
    let channels: [(usize, (f64, f64, f64)); 3] =
        [(2, (0.20, 0.55, 1.00)), (1, (0.30, 0.90, 0.40)), (0, (1.00, 0.30, 0.30))];
    for (idx, (r, g, b)) in channels {
        cr.set_source_rgba(r, g, b, 0.75);
        for (i, bucket) in bars.iter().enumerate() {
            let v = bucket[idx] as f64 / max_per_channel[idx] as f64;
            let bar_h = v * inner_h;
            cr.rectangle((i as f64) + 0.5, h - 1.0 - bar_h, 1.0, bar_h);
        }
        let _ = cr.fill();
    }
}

/// Aplica uma edição in-memory. O pipeline é: acumula op, reabre o
/// arquivo do disco, reaplica todas as ops (inclusive as anteriores —
/// barato mesmo com fotos grandes), produz a textura nova. Reaplicar
/// tudo em vez de guardar a `DynamicImage` editada entre turnos evita
/// drift cumulativo e simplifica o undo futuro (basta `pending_ops.pop`).
fn apply_edit(state: &Rc<RefCell<ViewerState>>, op: EditOp) {
    let path = {
        let s = state.borrow();
        if s.files.is_empty() || s.saving {
            return;
        }
        s.files[s.idx].clone()
    };
    {
        let mut s = state.borrow_mut();
        s.pending_ops.push(op);
        s.redo_ops.clear();
    }
    rebuild_edited_texture(state, &path);
    refresh_title(state);
    sync_edit_actions(state);
    pulse_osd(state);
}

/// Reaplica `pending_ops` sobre a imagem no disco e substitui a textura.
/// Se não houver ops, recarrega o arquivo pelo worker do viewer.
/// Falhas de decode logam e não mexem no paintable (o user vê a imagem
/// anterior — melhor do que uma tela preta).
fn rebuild_edited_texture(state: &Rc<RefCell<ViewerState>>, path: &Path) {
    let ops = state.borrow().pending_ops.clone();
    request_frame(state, path.to_path_buf(), ops);
}

fn apply_op_to_image(img: image::DynamicImage, op: EditOp) -> image::DynamicImage {
    match op {
        EditOp::RotateCw => img.rotate90(),
        EditOp::RotateCcw => img.rotate270(),
        EditOp::Rotate180 => img.rotate180(),
        EditOp::FlipH => img.fliph(),
        EditOp::FlipV => img.flipv(),
    }
}

/// Liga/desliga a action `win.save-edits` pra o item de menu e o acelerador
/// Ctrl+S ficarem grey quando não houver nada pra salvar. Complementa o
/// `•` no título como pista visual do estado "dirty".
fn sync_edit_actions(state: &Rc<RefCell<ViewerState>>) {
    let s = state.borrow();
    let has_ops = !s.pending_ops.is_empty();
    let idle = !s.saving;
    if let Some(action) = s.window().lookup_action("save-edits") {
        if let Some(simple) = action.downcast_ref::<gio::SimpleAction>() {
            simple.set_enabled(has_ops && idle);
        }
    }
    if let Some(action) = s.window().lookup_action("undo") {
        if let Some(simple) = action.downcast_ref::<gio::SimpleAction>() {
            simple.set_enabled(has_ops && idle);
        }
    }
    if let Some(action) = s.window().lookup_action("redo") {
        if let Some(simple) = action.downcast_ref::<gio::SimpleAction>() {
            simple.set_enabled(!s.redo_ops.is_empty() && idle);
        }
    }
    for name in ["rotate-cw", "rotate-ccw", "rotate-180", "flip-h", "flip-v"] {
        if let Some(action) = s.window().lookup_action(name) {
            if let Some(simple) = action.downcast_ref::<gio::SimpleAction>() {
                simple.set_enabled(idle && !s.files.is_empty());
            }
        }
    }
}

fn undo_edit(state: &Rc<RefCell<ViewerState>>) {
    let path = {
        let mut s = state.borrow_mut();
        if s.saving {
            return;
        }
        let Some(op) = s.pending_ops.pop() else { return };
        s.redo_ops.push(op);
        s.files[s.idx].clone()
    };
    rebuild_edited_texture(state, &path);
    refresh_title(state);
    sync_edit_actions(state);
    pulse_osd(state);
}

fn redo_edit(state: &Rc<RefCell<ViewerState>>) {
    let path = {
        let mut s = state.borrow_mut();
        if s.saving {
            return;
        }
        let Some(op) = s.redo_ops.pop() else { return };
        s.pending_ops.push(op);
        s.files[s.idx].clone()
    };
    rebuild_edited_texture(state, &path);
    refresh_title(state);
    sync_edit_actions(state);
    pulse_osd(state);
}

/// Salva as edições pendentes no próprio arquivo. Confirma antes — em
/// JPEG o re-encode perde qualidade e o user precisa ciência disso.
fn save_edits(state: &Rc<RefCell<ViewerState>>) {
    let (window, path, ops) = {
        let s = state.borrow();
        if s.pending_ops.is_empty() || s.files.is_empty() || s.saving {
            return;
        }
        (s.window().clone(), s.files[s.idx].clone(), s.pending_ops.clone())
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let is_jpeg = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| matches!(s.to_ascii_lowercase().as_str(), "jpg" | "jpeg"))
        .unwrap_or(false);
    let body = if is_jpeg {
        format!(
            "\"{name}\" será sobrescrito. JPEG é recodificado a cada \
             gravação — isso pode degradar a qualidade um pouco."
        )
    } else {
        format!("\"{name}\" será sobrescrito.")
    };
    let alert =
        adw::AlertDialog::builder().heading(gettext("Salvar alterações?")).body(body).build();
    alert.add_response("cancel", &gettext("Cancelar"));
    alert.add_response("save", &gettext("Salvar"));
    alert.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    alert.set_default_response(Some("save"));
    alert.set_close_response("cancel");

    let state = state.clone();
    alert.connect_response(Some("save"), move |_, _| {
        {
            let mut s = state.borrow_mut();
            s.saving = true;
            s.loading_spinner.set_visible(true);
            s.loading_spinner.start();
        }
        sync_edit_actions(&state);

        let (tx, rx) = mpsc::channel();
        let worker_path = path.clone();
        let worker_ops = ops.clone();
        std::thread::Builder::new()
            .name("bigiris-save".to_string())
            .spawn(move || {
                let result =
                    write_edits_to_disk(&worker_path, &worker_ops).map_err(|e| e.to_string());
                let _ = tx.send(result);
            })
            .expect("could not start save worker");

        let state = state.clone();
        let path = path.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(32), move || {
            match rx.try_recv() {
                Ok(result) => {
                    {
                        let mut s = state.borrow_mut();
                        s.saving = false;
                        s.loading_spinner.stop();
                        s.loading_spinner.set_visible(false);
                        if result.is_ok() {
                            s.pending_ops.clear();
                            s.redo_ops.clear();
                        }
                    }
                    refresh_title(&state);
                    sync_edit_actions(&state);
                    match result {
                        Ok(()) => state.borrow().osd_label.set_text(&gettext("Salvo")),
                        Err(error) => {
                            tracing::error!(?path, %error, "save: falha ao gravar edições");
                            state.borrow().osd_label.set_text(&gettext("Falha ao salvar"));
                        }
                    }
                    pulse_osd(&state);
                    glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    let mut s = state.borrow_mut();
                    s.saving = false;
                    s.loading_spinner.stop();
                    s.loading_spinner.set_visible(false);
                    drop(s);
                    sync_edit_actions(&state);
                    glib::ControlFlow::Break
                }
            }
        });
    });
    alert.present(Some(&window));
}

/// Grava em arquivo temporário no mesmo diretório, sincroniza e só então
/// renomeia por cima do destino. Assim, queda de energia ou falha do encoder
/// deixa o original intacto. Permissões existentes são preservadas.
fn write_edits_to_disk(path: &Path, ops: &[EditOp]) -> Result<(), Box<dyn std::error::Error>> {
    let target = if path.symlink_metadata()?.file_type().is_symlink() {
        path.canonicalize()?
    } else {
        path.to_path_buf()
    };
    bigimage_core::content::ensure_static(&target)?;
    let img = bigimage_core::open_image(&target)?;
    let edited = ops.iter().fold(img, |acc, op| apply_op_to_image(acc, *op));
    let format = Format::from_image_format(image::ImageFormat::from_path(&target)?)
        .ok_or("formato sem encoder suportado")?;
    bigimage_core::save_image(
        edited,
        target,
        format,
        &EncodeOptions::default(),
        OverwritePolicy::Replace,
    )?;
    Ok(())
}

/// Effective on-screen scale factor — used to anchor zoom-in from `Fit` at
/// the current visual size, so the first click doesn't teleport the image.
fn effective_scale(zoom: ZoomState, natural: (u32, u32), viewport: (i32, i32)) -> f64 {
    match zoom {
        ZoomState::Scale(s) => s,
        ZoomState::Fit => {
            if natural.0 == 0 || natural.1 == 0 || viewport.0 <= 0 || viewport.1 <= 0 {
                return 1.0;
            }
            let sx = viewport.0 as f64 / natural.0 as f64;
            let sy = viewport.1 as f64 / natural.1 as f64;
            sx.min(sy).min(1.0)
        }
    }
}

fn update_display(state: &Rc<RefCell<ViewerState>>) {
    let path = {
        let s = state.borrow();
        s.files[s.idx].clone()
    };
    // Reaching another file starts a new edit history. `pending_ops` is
    // guaranteed empty by the navigation guard, while a redo-only branch
    // (after undoing back to the original) must not leak into the next image.
    state.borrow_mut().redo_ops.clear();
    sync_edit_actions(state);
    request_frame(state, path, Vec::new());
    refresh_title(state);
}

/// Never discard edits as a side effect of navigation. The user can save or
/// undo them first; slideshow also stops so it cannot repeatedly nag or skip.
fn can_leave_current(state: &Rc<RefCell<ViewerState>>) -> bool {
    if state.borrow().pending_ops.is_empty() {
        return true;
    }
    state.borrow().osd_label.set_text(&gettext("Salve ou desfaça as alterações antes de navegar"));
    pulse_osd(state);
    false
}

fn request_frame(state: &Rc<RefCell<ViewerState>>, path: PathBuf, ops: Vec<EditOp>) {
    let mut s = state.borrow_mut();
    zoom::cancel_pending(&mut s);
    s.load_generation = s.load_generation.wrapping_add(1);
    s.loading_spinner.set_visible(true);
    s.loading_spinner.start();
    s.loader.request(LoadRequest { generation: s.load_generation, path, ops });
}

fn apply_decoded_frame(state: &Rc<RefCell<ViewerState>>, frame: DecodedFrame) {
    let is_current = {
        let s = state.borrow();
        frame.generation == s.load_generation
            && s.files.get(s.idx).is_some_and(|path| path == &frame.path)
    };
    if !is_current {
        return;
    }

    let mut s = state.borrow_mut();
    if !s.saving {
        s.loading_spinner.stop();
        s.loading_spinner.set_visible(false);
    }
    zoom::cancel_pending(&mut s);
    match frame.pixels {
        Ok((raw, w, h, histogram)) => {
            let texture = texture_from_rgba(raw, w, h);
            s.natural = (w, h);
            s.histogram = histogram;
            s.texture = Some(texture);
            apply_zoom(&s);
            s.histogram_area.queue_draw();
        }
        Err(error) => {
            tracing::error!(path = ?frame.path, %error, "falha ao carregar imagem");
            s.texture = None;
            s.picture.set_paintable(gdk::Paintable::NONE);
            s.osd_label.set_text(&gettext("Não foi possível abrir a imagem"));
            drop(s);
            pulse_osd(state);
        }
    }
}

/// Constrói um menu curto para as funções menos frequentes. A navegação e
/// abertura ficam visíveis na barra, enquanto ferramentas robustas aparecem
/// sob demanda — o equilíbrio "simples por padrão, poderoso quando preciso".
/// Ações ficam em `win.<nome>` (registradas em [`install_viewer_actions`]).
fn build_header_menu_button(state: &Rc<RefCell<ViewerState>>) -> gtk::MenuButton {
    let menu = gio::Menu::new();

    let s_file = gio::Menu::new();
    s_file.append(Some(&gettext("Abrir imagens…")), Some("win.open"));
    s_file.append(Some(&gettext("Copiar imagem")), Some("win.copy"));
    menu.append_section(None, &s_file);

    let s_actions = gio::Menu::new();
    s_actions.append(Some(&gettext("Imprimir…")), Some("win.print"));
    menu.append_section(None, &s_actions);

    // Edição in-memory: giros/espelhos aplicados na hora;
    // só vão pra disco quando o user pedir Salvar (Ctrl+S).
    let s_edit = gio::Menu::new();
    s_edit.append(Some(&gettext("Desfazer")), Some("win.undo"));
    s_edit.append(Some(&gettext("Refazer")), Some("win.redo"));
    s_edit.append(Some(&gettext("Girar à direita")), Some("win.rotate-cw"));
    s_edit.append(Some(&gettext("Girar à esquerda")), Some("win.rotate-ccw"));
    s_edit.append(Some(&gettext("Girar 180°")), Some("win.rotate-180"));
    s_edit.append(Some(&gettext("Espelhar horizontal")), Some("win.flip-h"));
    s_edit.append(Some(&gettext("Espelhar vertical")), Some("win.flip-v"));
    s_edit.append(Some(&gettext("Recortar…")), Some("win.crop"));
    menu.append_section(None, &s_edit);

    // IA: cada item dispara um subprocesso `bigiris --dialog=…` (mesmo
    // motivo do crop: cada diálogo monta sua própria adw::Application).
    let s_ai = gio::Menu::new();
    s_ai.append(Some(&gettext("Remover fundo (IA)…")), Some("win.remove-bg"));
    s_ai.append(Some(&gettext("Aumentar resolução…")), Some("win.upscale"));
    menu.append_section(None, &s_ai);

    let s_save = gio::Menu::new();
    s_save.append(Some(&gettext("Salvar alterações")), Some("win.save-edits"));
    menu.append_section(None, &s_save);

    let s_delete = gio::Menu::new();
    s_delete.append(Some(&gettext("Mover para a lixeira")), Some("win.trash"));
    menu.append_section(None, &s_delete);

    let s_help = gio::Menu::new();
    s_help.append(Some(&gettext("Atalhos do teclado")), Some("win.shortcuts"));
    s_help.append(Some(&gettext("Sobre o BigIris")), Some("app.about"));
    menu.append_section(None, &s_help);

    let btn = gtk::MenuButton::new();
    btn.set_icon_name("open-menu-symbolic");
    btn.set_tooltip_text(Some(&gettext("Menu principal")));
    btn.set_menu_model(Some(&menu));
    // Fixa a header enquanto o popover está aberto — senão o timer de
    // auto-hide do ToolbarView dispara com o cursor já dentro do popover
    // e fecha o menu sozinho (flicker visível: abre e some no mesmo frame).
    {
        let state = Rc::downgrade(state);
        btn.connect_active_notify(move |b| {
            let Some(state) = state.upgrade() else {
                return;
            };
            if b.is_active() {
                cancel_top_hide(&state);
                state.borrow().toolbar_view.set_reveal_top_bars(true);
            } else {
                reveal_top(&state);
            }
        });
    }
    btn
}

/// Registra as actions `win.*` que alimentam o menu hamburger e os
/// quick-buttons do header, mais os acelerators globais. Tudo fecha
/// sobre `state` pra acessar a imagem corrente.
fn install_viewer_actions(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<ViewerState>>,
) {
    // Sobre — já existia, reinstalamos aqui pra ficar tudo num lugar.
    let about = gio::SimpleAction::new("about", None);
    {
        let window = window.downgrade();
        about.connect_activate(move |_, _| {
            if let Some(window) = window.upgrade() {
                present_about_dialog(&window);
            }
        });
    }
    app.add_action(&about);

    // win.fullscreen — alterna F11; quick-button do header aciona a mesma.
    let fullscreen = gio::SimpleAction::new("fullscreen", None);
    {
        let window = window.downgrade();
        fullscreen.connect_activate(move |_, _| {
            let Some(window) = window.upgrade() else {
                return;
            };
            window.set_fullscreened(!window.is_fullscreen());
        });
    }
    window.add_action(&fullscreen);

    // win.open — FileDialog → spawna nova instância do bigiris nos arquivos
    // escolhidos. Separar em nova janela mantém o histórico da atual.
    let open = gio::SimpleAction::new("open", None);
    {
        let window = window.downgrade();
        open.connect_activate(move |_, _| {
            if let Some(window) = window.upgrade() {
                open_file_chooser(&window);
            }
        });
    }
    window.add_action(&open);

    // Actions dependentes de imagem carregada ficam grey na dashboard —
    // senão o menu mostra opção clicável que não faz nada, e itens de
    // ação inválida chegam a fechar o popover ao hover no GTK4.
    let has_files = !state.borrow().files.is_empty();
    let has_multiple = state.borrow().files.len() > 1;

    for (name, delta) in [("previous", -1isize), ("next", 1isize)] {
        let action = gio::SimpleAction::new(name, None);
        action.set_enabled(has_multiple);
        let state = Rc::downgrade(state);
        action.connect_activate(move |_, _| {
            let Some(state) = state.upgrade() else {
                return;
            };
            navigate_relative(&state, delta);
        });
        window.add_action(&action);
    }

    for (name, operation) in [
        ("zoom-in", zoom_in as fn(&Rc<RefCell<ViewerState>>)),
        ("zoom-out", zoom_out as fn(&Rc<RefCell<ViewerState>>)),
    ] {
        let action = gio::SimpleAction::new(name, None);
        action.set_enabled(has_files);
        let state = Rc::downgrade(state);
        action.connect_activate(move |_, _| {
            let Some(state) = state.upgrade() else {
                return;
            };
            operation(&state);
            pulse_osd(&state);
        });
        window.add_action(&action);
    }
    let zoom_fit = gio::SimpleAction::new("zoom-fit", None);
    zoom_fit.set_enabled(has_files);
    {
        let state = Rc::downgrade(state);
        zoom_fit.connect_activate(move |_, _| {
            let Some(state) = state.upgrade() else {
                return;
            };
            set_zoom(&state, ZoomState::Fit);
            pulse_osd(&state);
        });
    }
    window.add_action(&zoom_fit);

    // win.copy — copia a textura atual pro clipboard. Qualquer app (Slack,
    // Ctrl+V num chat) consegue colar.
    let copy = gio::SimpleAction::new("copy", None);
    copy.set_enabled(has_files);
    {
        let state = Rc::downgrade(state);
        copy.connect_activate(move |_, _| {
            let Some(state) = state.upgrade() else {
                return;
            };
            copy_current_to_clipboard(&state);
        });
    }
    window.add_action(&copy);

    let undo = gio::SimpleAction::new("undo", None);
    undo.set_enabled(false);
    {
        let state = Rc::downgrade(state);
        undo.connect_activate(move |_, _| {
            let Some(state) = state.upgrade() else {
                return;
            };
            undo_edit(&state);
        });
    }
    window.add_action(&undo);
    app.set_accels_for_action("win.undo", &["<Ctrl>z"]);

    let redo = gio::SimpleAction::new("redo", None);
    redo.set_enabled(false);
    {
        let state = Rc::downgrade(state);
        redo.connect_activate(move |_, _| {
            let Some(state) = state.upgrade() else {
                return;
            };
            redo_edit(&state);
        });
    }
    window.add_action(&redo);
    app.set_accels_for_action("win.redo", &["<Ctrl><Shift>z", "<Ctrl>y"]);

    // win.rotate-{cw,ccw,180}, win.flip-{h,v} — edições in-memory. Acumulam
    // em `pending_ops`, atualizam textura na hora e marcam o título com `•`.
    let edits: [(&str, EditOp, &[&str]); 5] = [
        ("rotate-cw", EditOp::RotateCw, &["R"]),
        ("rotate-ccw", EditOp::RotateCcw, &["<Shift>R"]),
        ("rotate-180", EditOp::Rotate180, &[]),
        ("flip-h", EditOp::FlipH, &[]),
        ("flip-v", EditOp::FlipV, &[]),
    ];
    for (name, op, accels) in edits {
        let action = gio::SimpleAction::new(name, None);
        action.set_enabled(has_files);
        {
            let state = Rc::downgrade(state);
            action.connect_activate(move |_, _| {
                let Some(state) = state.upgrade() else {
                    return;
                };
                apply_edit(&state, op);
            });
        }
        window.add_action(&action);
        if !accels.is_empty() {
            app.set_accels_for_action(&format!("win.{name}"), accels);
        }
    }

    // win.save-edits — grava as `pending_ops` no arquivo. Sai grey quando
    // não há nada pra salvar (controlado por `sync_edit_actions`).
    let save_edits_action = gio::SimpleAction::new("save-edits", None);
    save_edits_action.set_enabled(false);
    {
        let state = Rc::downgrade(state);
        save_edits_action.connect_activate(move |_, _| {
            let Some(state) = state.upgrade() else {
                return;
            };
            save_edits(&state);
        });
    }
    window.add_action(&save_edits_action);
    app.set_accels_for_action("win.save-edits", &["<Ctrl>S"]);

    // win.print — gtk::PrintOperation + cairo, escala a imagem pra página
    // preservando aspecto (Contain).
    let print = gio::SimpleAction::new("print", None);
    print.set_enabled(has_files);
    {
        let state = Rc::downgrade(state);
        print.connect_activate(move |_, _| {
            let Some(state) = state.upgrade() else {
                return;
            };
            print_current(&state);
        });
    }
    window.add_action(&print);

    // win.info — reabre o dialog de metadados no arquivo corrente.
    let info = gio::SimpleAction::new("info", None);
    info.set_enabled(has_files);
    {
        let state = Rc::downgrade(state);
        info.connect_activate(move |_, _| {
            let Some(state) = state.upgrade() else {
                return;
            };
            show_metadata_for_current(&state);
        });
    }
    window.add_action(&info);

    // win.crop — abre o diálogo de recorte na imagem atual via subprocesso.
    // Um processo separado porque `run_crop_dialog` monta sua própria
    // `adw::Application`, e rodar duas no mesmo processo é conflito certo.
    let crop = gio::SimpleAction::new("crop", None);
    crop.set_enabled(has_files);
    {
        let state = Rc::downgrade(state);
        crop.connect_activate(move |_, _| {
            let Some(state) = state.upgrade() else {
                return;
            };
            let path = {
                let s = state.borrow();
                if s.files.is_empty() {
                    return;
                }
                s.files[s.idx].clone()
            };
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::process::Command::new(exe).arg("--dialog=crop").arg(path).spawn();
            }
        });
    }
    window.add_action(&crop);

    // win.remove-bg / win.upscale — mesmo padrão do crop: subprocesso
    // dedicado para cada diálogo Prisma na imagem corrente.
    for (name, dialog) in [("remove-bg", "remove-bg"), ("upscale", "upscale")] {
        let action = gio::SimpleAction::new(name, None);
        action.set_enabled(has_files);
        let state = Rc::downgrade(state);
        let dialog = dialog.to_string();
        action.connect_activate(move |_, _| {
            let Some(state) = state.upgrade() else {
                return;
            };
            let path = {
                let s = state.borrow();
                if s.files.is_empty() {
                    return;
                }
                s.files[s.idx].clone()
            };
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::process::Command::new(exe)
                    .arg(format!("--dialog={dialog}"))
                    .arg(path)
                    .spawn();
            }
        });
        window.add_action(&action);
    }

    // win.trash — confirma e joga o arquivo na lixeira do XDG. Como o
    // state guarda files como Rc<Vec>, depois de deletar fechamos a
    // janela em vez de tentar mutar a lista (o usuário reabre o próximo).
    let trash = gio::SimpleAction::new("trash", None);
    trash.set_enabled(has_files);
    {
        let state = Rc::downgrade(state);
        trash.connect_activate(move |_, _| {
            let Some(state) = state.upgrade() else {
                return;
            };
            trash_current(&state);
        });
    }
    window.add_action(&trash);

    // Fullscreen só faz sentido com imagem; dashboard tem flow diferente.
    let fullscreen_action = window.lookup_action("fullscreen").and_downcast::<gio::SimpleAction>();
    if let Some(action) = fullscreen_action {
        action.set_enabled(has_files);
    }

    // win.shortcuts — AdwAlertDialog com cheatsheet. Mais leve e
    // legível que gtk::ShortcutsWindow, e combina com o resto do visual.
    let shortcuts = gio::SimpleAction::new("shortcuts", None);
    {
        let window = window.downgrade();
        shortcuts.connect_activate(move |_, _| {
            if let Some(window) = window.upgrade() {
                present_shortcuts_dialog(&window);
            }
        });
    }
    window.add_action(&shortcuts);

    app.set_accels_for_action("win.open", &["<Ctrl>o"]);
    app.set_accels_for_action("win.copy", &["<Ctrl>c"]);
    app.set_accels_for_action("win.print", &["<Ctrl>p"]);
    app.set_accels_for_action("win.info", &["<Ctrl>i"]);
    app.set_accels_for_action("win.crop", &["<Ctrl>r"]);
    app.set_accels_for_action("win.remove-bg", &["<Ctrl>b"]);
    app.set_accels_for_action("win.upscale", &["<Ctrl>u"]);
    app.set_accels_for_action("win.trash", &["Delete"]);
    app.set_accels_for_action("win.shortcuts", &["<Ctrl>question"]);
    app.set_accels_for_action("win.fullscreen", &["F11"]);
    app.set_accels_for_action("app.about", &["F1"]);
}

/// Apresenta um FileDialog de seleção múltipla com filtro de imagens.
/// Abre cada seleção numa nova janela via `std::process::Command` pra
/// preservar o histórico da janela atual (e porque a `files` no state
/// é Rc<Vec<_>> imutável).
fn open_file_chooser(window: &adw::ApplicationWindow) {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Imagens"));
    filter.add_mime_type("image/*");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);

    let dialog = gtk::FileDialog::builder()
        .title(gettext("Abrir imagens"))
        .modal(true)
        .filters(&filters)
        .build();

    dialog.open_multiple(Some(window), gio::Cancellable::NONE, move |result| {
        let Ok(list) = result else { return };
        let mut paths: Vec<String> = Vec::new();
        for i in 0..list.n_items() {
            if let Some(file) = list.item(i).and_downcast::<gio::File>() {
                if let Some(p) = file.path() {
                    paths.push(p.to_string_lossy().into_owned());
                }
            }
        }
        if paths.is_empty() {
            return;
        }
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("bigiris"));
        let _ = std::process::Command::new(exe).args(&paths).spawn();
    });
}

/// Copia a imagem corrente (pixels, não o path) pro clipboard do sistema.
/// Usa `gdk::MemoryTexture` — qualquer consumidor que aceite imagens
/// (paste em chat, editor de imagem) recebe a textura.
fn copy_current_to_clipboard(state: &Rc<RefCell<ViewerState>>) {
    let (window, texture) = {
        let s = state.borrow();
        if s.files.is_empty() {
            return;
        }
        (s.window().clone(), s.texture.clone())
    };
    if let Some(texture) = texture {
        window.clipboard().set_texture(&texture);
    }
}

/// Envia a imagem corrente pra impressora via gtk::PrintOperation.
/// Renderiza a imagem com cairo numa única página, escalada pra
/// caber com aspect ratio preservado (Contain).
fn print_current(state: &Rc<RefCell<ViewerState>>) {
    let (window, path) = {
        let s = state.borrow();
        if s.files.is_empty() {
            return;
        }
        (s.window().clone(), s.files[s.idx].clone())
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let closing = cancel.clone();
    let closing_handler = window.connect_unrealize(move |_| closing.store(true, Ordering::Release));
    let input = path.clone();
    let (tx, rx) = mpsc::sync_channel(1);
    window.set_cursor_from_name(Some("wait"));
    std::thread::spawn(move || {
        crate::codec::with_cancel(cancel, || {
            let result = bigimage_core::open_image(input)
                .map(|image| {
                    let rgba = image.to_rgba8();
                    let size = rgba.dimensions();
                    (premultiply_for_cairo(rgba.into_raw()), size)
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
    });
    let weak = window.downgrade();
    let mut closing_handler = Some(closing_handler);
    glib::timeout_add_local(Duration::from_millis(16), move || {
        let Some(window) = weak.upgrade() else { return glib::ControlFlow::Break };
        if !window.is_visible() {
            return glib::ControlFlow::Break;
        }
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => Err("O preparo da impressão falhou".into()),
        };
        if let Some(handler) = closing_handler.take() {
            window.disconnect(handler);
        }
        window.set_cursor(None);
        match result {
            Ok((pixels, (width, height))) => {
                if let Ok(surface) = gtk::cairo::ImageSurface::create_for_data(
                    pixels,
                    gtk::cairo::Format::ARgb32,
                    width as i32,
                    height as i32,
                    width as i32 * 4,
                ) {
                    present_print_dialog(&window, &path, surface);
                } else {
                    show_print_error(&window, "Não foi possível preparar a página");
                }
            }
            Err(error) => show_print_error(&window, &error),
        }
        glib::ControlFlow::Break
    });
}

/// Cairo ARGB32 stores premultiplied components in native byte order.
fn premultiply_for_cairo(mut rgba: Vec<u8>) -> Vec<u8> {
    for pixel in rgba.as_chunks_mut::<4>().0 {
        let alpha = u32::from(pixel[3]);
        let r = (u32::from(pixel[0]) * alpha + 127) / 255;
        let g = (u32::from(pixel[1]) * alpha + 127) / 255;
        let b = (u32::from(pixel[2]) * alpha + 127) / 255;
        *pixel = ((alpha << 24) | (r << 16) | (g << 8) | b).to_ne_bytes();
    }
    rgba
}

fn show_print_error(window: &adw::ApplicationWindow, error: &str) {
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Não foi possível imprimir"))
        .body(error)
        .build();
    dialog.add_response("close", &gettext("Fechar"));
    dialog.present(Some(window));
}

fn present_print_dialog(
    window: &adw::ApplicationWindow,
    path: &Path,
    surface: gtk::cairo::ImageSurface,
) {
    let op = gtk::PrintOperation::new();
    op.set_n_pages(1);
    op.set_job_name(
        &path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "BigIris".into()),
    );
    op.connect_draw_page(move |_, ctx, _| {
        let iw = f64::from(surface.width());
        let ih = f64::from(surface.height());
        let cr = ctx.cairo_context();
        let scale = (ctx.width() / iw).min(ctx.height() / ih);
        cr.translate((ctx.width() - iw * scale) / 2.0, (ctx.height() - ih * scale) / 2.0);
        cr.scale(scale, scale);
        if let Err(error) = cr.set_source_surface(&surface, 0.0, 0.0).and_then(|_| cr.paint()) {
            tracing::warn!(%error, "erro ao renderizar página");
        }
    });
    if let Err(error) = op.run(gtk::PrintOperationAction::PrintDialog, Some(window)) {
        tracing::warn!(%error, "erro no diálogo de impressão");
        show_print_error(window, &error.to_string());
    }
}

/// Reusa o dialog de metadados existente com os arquivos do viewer.
/// Spawna nova janela (o dialog é uma AppWindow independente).
fn show_metadata_for_current(state: &Rc<RefCell<ViewerState>>) {
    // Se já existe uma janela aberta, traz pra frente em vez de empilhar.
    if let Some(existing) = state.borrow().props_dialog.clone() {
        existing.present();
        return;
    }
    let (window, path) = {
        let s = state.borrow();
        if s.files.is_empty() {
            return;
        }
        (s.window().clone(), s.files[s.idx].clone())
    };
    let Some(app) = window.application().and_downcast::<adw::Application>() else { return };
    let dialog = build_properties_window(&app, &path);
    // Quando o user fecha, soltamos a referência — próximo clique abre fresco.
    {
        let state = Rc::downgrade(state);
        dialog.connect_close_request(move |_| {
            if let Some(state) = state.upgrade() {
                state.borrow_mut().props_dialog = None;
            }
            glib::Propagation::Proceed
        });
    }
    state.borrow_mut().props_dialog = Some(dialog.clone());
    dialog.present();
}

/// Confirma com AdwAlertDialog e move o arquivo corrente pra lixeira
/// via `gio::File::trash`. Depois fecha a janela — a `files` do state
/// é imutável (Rc<Vec>), então remover elemento exigiria refactor.
fn trash_current(state: &Rc<RefCell<ViewerState>>) {
    let (window, path) = {
        let s = state.borrow();
        if s.files.is_empty() {
            return;
        }
        (s.window().clone(), s.files[s.idx].clone())
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let alert = adw::AlertDialog::builder()
        .heading(gettext("Mover para a lixeira?"))
        .body(format!(
            "\"{name}\" será movido para a lixeira do sistema. Você pode restaurá-lo depois."
        ))
        .build();
    alert.add_response("cancel", &gettext("Cancelar"));
    alert.add_response("trash", &gettext("Mover"));
    alert.set_response_appearance("trash", adw::ResponseAppearance::Destructive);
    alert.set_default_response(Some("cancel"));
    alert.set_close_response("cancel");

    let window_for_close = window.clone();
    alert.connect_response(Some("trash"), move |_, _| {
        let file = gio::File::for_path(&path);
        match file.trash(gio::Cancellable::NONE) {
            Ok(_) => {
                tracing::info!(?path, "trash ok");
                window_for_close.close();
            }
            Err(e) => tracing::warn!(?path, error = %e, "trash falhou"),
        }
    });
    alert.present(Some(&window));
}

/// Cheatsheet dos atalhos de teclado do viewer. AdwAlertDialog em vez de
/// gtk::ShortcutsWindow pra ficar consistente com o resto dos dialogs
/// do app (mesma estética, modal-in-window).
fn present_shortcuts_dialog(parent: &adw::ApplicationWindow) {
    parent.set_cursor(None);
    // Usa o widget nativo `gtk::ShortcutsWindow` — layout denso estilo
    // Layout compacto com colunas, seções e chips de tecla renderizados pelo
    // próprio tema. Bem mais limpo do que texto em AlertDialog.
    let shortcuts: [(&str, &[(&str, &str)]); 6] = [
        (
            "Navegação",
            &[
                ("Left Right", "Anterior / próximo"),
                ("Home End", "Primeiro / último"),
                ("space", "Slideshow (auto-avanço)"),
            ],
        ),
        (
            "Zoom",
            &[
                ("plus minus", "Aproximar / afastar"),
                ("0", "Ajustar à janela"),
                ("1", "Tamanho real (100%)"),
                ("scroll_wheel", "Zoom contínuo"),
            ],
        ),
        (
            "Janela",
            &[
                ("F11", "Tela cheia"),
                ("T", "Modo teatro"),
                ("F", "Fita de miniaturas"),
                ("G", "Histograma RGB"),
                ("H", "Fixar barras visíveis"),
                ("Escape", "Fechar"),
            ],
        ),
        (
            "Edição",
            &[
                ("<ctrl>Z", "Desfazer"),
                ("<ctrl><shift>Z", "Refazer"),
                ("R", "Girar à direita"),
                ("<Shift>R", "Girar à esquerda"),
                ("<ctrl>R", "Recortar…"),
                ("<ctrl>B", "Remover fundo (IA)…"),
                ("<ctrl>U", "Aumentar resolução…"),
                ("<ctrl>S", "Salvar alterações"),
            ],
        ),
        (
            "Arquivo",
            &[
                ("<ctrl>O", "Abrir imagens"),
                ("<ctrl>C", "Copiar imagem"),
                ("<ctrl>P", "Imprimir"),
                ("<ctrl>I", "Propriedades"),
                ("Delete", "Mover para a lixeira"),
            ],
        ),
        ("Ajuda", &[("<ctrl>question", "Este diálogo"), ("F1", "Sobre")]),
    ];
    let section = gtk::ShortcutsSection::builder().section_name("viewer").build();
    for (group_title, entries) in shortcuts {
        let group = gtk::ShortcutsGroup::builder().title(group_title).build();
        for (accel, desc) in entries {
            let sc = gtk::ShortcutsShortcut::builder().accelerator(*accel).title(*desc).build();
            group.append(&sc);
        }
        section.append(&group);
    }
    let window =
        gtk::ShortcutsWindow::builder().modal(true).transient_for(parent).child(&section).build();
    window.present();
}

/// About dialog with project, license and support links.
fn present_about_dialog(parent: &adw::ApplicationWindow) {
    // Restaura o cursor antes de apresentar — o viewer pode tê-lo
    // escondido por idle; dentro do AdwAboutDialog (modal in-window)
    // precisamos que esteja visível pra user navegar nos Detalhes etc.
    parent.set_cursor(None);

    let about = adw::AboutDialog::builder()
        .application_name("BigIris")
        .application_icon(APP_ID)
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("Leonardo Athayde")
        .copyright("© 2026 Leonardo Athayde e contribuidores")
        .license_type(gtk::License::Gpl30)
        .website("https://github.com/xathay/bigiris")
        .issue_url("https://github.com/xathay/bigiris/issues")
        .developers(vec!["Leonardo Athayde".to_string()])
        .designers(vec!["Leonardo Athayde".to_string(), "Comunidade BigLinux".to_string()])
        .translator_credits("Leonardo Athayde <leoathayde@gmail.com>")
        .comments(
            "Visualizador e conversor de imagens para Linux. Rápido, \
             acessível por teclado e integrado ao gerenciador de arquivos.",
        )
        .build();
    about.present(Some(parent));
}

/// Welcome dashboard shown when `bigiris` is launched without files. Sets
/// the tone for the app: explains what it does, lists the headline
/// capabilities, and gives a single obvious next step ("Abrir imagens").
/// Inspired by the Big OCR PDF welcome — makes the first-contact moment
/// feel intentional instead of the usual empty-state-with-icon.
fn build_dashboard(window: &adw::ApplicationWindow) -> gtk::Widget {
    let status = adw::StatusPage::builder()
        .icon_name("com.biglinux.Iris")
        .title(gettext("Bem-vindo ao BigIris"))
        .description(
            "Abra uma imagem para visualizar sem distrações ou processe\n\
             uma pasta inteira com as ferramentas Prisma.",
        )
        .build();
    status.add_css_class("compact");

    // Feature cards — cada um abre um picker de imagens e relança o
    // próprio binário no modo certo (viewer, --dialog=batch, etc.).
    // Vai-e-volta evita reimplementar o pipeline em cima do dashboard.
    let features =
        adw::PreferencesGroup::builder().title(gettext("Ferramentas")).margin_top(16).build();
    for (title, subtitle, icon, action) in FEATURE_HIGHLIGHTS {
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .title(*title)
            .subtitle(*subtitle)
            .activatable(action.is_some())
            .build();
        let icon_widget = gtk::Image::from_icon_name(icon);
        icon_widget.add_css_class("dim-label");
        row.add_prefix(&icon_widget);
        if let Some(cmd) = action {
            let arrow = gtk::Image::from_icon_name("go-next-symbolic");
            arrow.add_css_class("dim-label");
            row.add_suffix(&arrow);
            let window = window.downgrade();
            let cmd = *cmd;
            row.connect_activated(move |_| {
                if let Some(window) = window.upgrade() {
                    dispatch_feature(&window, cmd);
                }
            });
        }
        features.add(&row);
    }

    // Bloco de ações: botão primário "Abrir imagens" + dica sobre o
    // clique-direito (principal caminho de uso).
    let actions = gtk::Box::new(gtk::Orientation::Vertical, 12);
    actions.set_halign(gtk::Align::Center);
    actions.set_margin_top(20);

    let open_btn = gtk::Button::builder()
        .label("Abrir imagens…")
        .css_classes(["pill", "suggested-action"])
        .build();
    {
        let window = window.downgrade();
        open_btn.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                open_files_dialog(&window);
            }
        });
    }
    actions.append(&open_btn);

    let folder_btn = gtk::Button::builder().label("Processar uma pasta…").build();
    {
        let window = window.downgrade();
        folder_btn.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                open_batch_folder_dialog(&window);
            }
        });
    }
    actions.append(&folder_btn);

    let tip = gtk::Label::builder()
        .label(
            "No gerenciador de arquivos, clique com o botão direito em imagens\n\
             para acessar as operações rápidas do BigIris.",
        )
        .justify(gtk::Justification::Center)
        .wrap(true)
        .max_width_chars(52)
        .css_classes(["dim-label"])
        .build();
    actions.append(&tip);

    // Empilha features + ações no slot de child do StatusPage
    let child = gtk::Box::new(gtk::Orientation::Vertical, 0);
    child.set_halign(gtk::Align::Center);
    child.append(&features);
    child.append(&actions);
    status.set_child(Some(&child));

    status.upcast()
}

/// Ações dos cards do dashboard. `None` = card puramente informativo
/// (ex.: "Clique direito no gerenciador" — não tem como disparar do
/// dashboard). Caso contrário é a string passada a [`dispatch_feature`].
const FEATURE_HIGHLIGHTS: &[(&str, &str, &str, Option<&str>)] = &[
    (
        "Visualizar imagens",
        "JPEG, PNG, WebP, AVIF, GIF, TIFF, BMP, QOI, HDR e OpenEXR",
        "image-x-generic-symbolic",
        Some("viewer"),
    ),
    (
        "Converter em lote",
        "Formato, qualidade e presets prontos para WhatsApp, Instagram, Facebook e Discord",
        "document-save-as-symbolic",
        Some("batch"),
    ),
    (
        "Clique direito no gerenciador",
        "Ações rápidas integradas ao Dolphin e a outros gerenciadores",
        "view-list-symbolic",
        None,
    ),
];

fn open_batch_folder_dialog(window: &adw::ApplicationWindow) {
    let dialog = gtk::FileDialog::builder()
        .title(gettext("Selecionar pasta com imagens"))
        .modal(true)
        .build();
    let parent = window.clone();
    let window = window.clone();
    dialog.select_folder(Some(&parent), gio::Cancellable::NONE, move |result| {
        let Ok(folder) = result else { return };
        let Some(path) = folder.path() else { return };
        let Ok(exe) = std::env::current_exe() else { return };
        if std::process::Command::new(exe).arg("--dialog=batch").arg(path).spawn().is_ok() {
            window.close();
        }
    });
}

/// Pega os arquivos escolhidos e relança o próprio binário no modo
/// apropriado. Tratamos "viewer" como caso-especial (executável sem
/// subcomando); o resto vira `--dialog=<nome>`, exceto `remove-bg` que
/// é um subcomando CLI dedicado.
fn dispatch_feature(window: &adw::ApplicationWindow, action: &'static str) {
    let dialog = gtk::FileDialog::builder()
        .title(match action {
            "viewer" => "Selecionar imagens para visualizar",
            "batch" => "Selecionar imagens para conversão em lote",
            "adjust" => "Selecionar imagens para ajustar cores",
            "remove-bg" => "Selecionar imagens para remover fundo",
            "animate" => "Selecionar frames na ordem do GIF",
            _ => "Selecionar imagens",
        })
        .modal(true)
        .build();
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Imagens"));
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
    let window = window.clone();
    dialog.open_multiple(Some(&parent), None::<&gio::Cancellable>, move |result| {
        let Ok(files) = result else { return };
        let paths: Vec<PathBuf> = (0..files.n_items())
            .filter_map(|i| files.item(i))
            .filter_map(|o| o.downcast::<gio::File>().ok())
            .filter_map(|f| f.path())
            .collect();
        if paths.is_empty() {
            return;
        }

        let Ok(exe) = std::env::current_exe() else { return };
        let mut cmd = std::process::Command::new(exe);
        match action {
            "viewer" => {} // sem args: abre direto no visualizador
            "remove-bg" => {
                cmd.arg("remove-bg");
            }
            other => {
                cmd.arg(format!("--dialog={other}"));
            }
        }
        cmd.args(paths.iter().map(|p| p.as_os_str()));
        let _ = cmd.spawn();
        window.close();
    });
}

/// File-chooser do dashboard: carrega uma lista de imagens e respawna o
/// processo com esses arquivos para reusar a lógica de carregamento do
/// CLI. Simples e previsível — evita duplicar o pipeline de load in-place.
fn open_files_dialog(window: &adw::ApplicationWindow) {
    let dialog =
        gtk::FileDialog::builder().title(gettext("Selecionar imagens")).modal(true).build();
    // Filtro de imagens: tudo que o nosso decoder aguenta.
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Imagens"));
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

    let window = window.clone();
    let parent = window.clone();
    dialog.open_multiple(Some(&parent), None::<&gio::Cancellable>, move |result| {
        let Ok(files) = result else { return };
        let paths: Vec<PathBuf> = (0..files.n_items())
            .filter_map(|i| files.item(i))
            .filter_map(|o| o.downcast::<gio::File>().ok())
            .filter_map(|f| f.path())
            .collect();
        if paths.is_empty() {
            return;
        }
        // Reaproveita o binário atual: relança com os caminhos, o que
        // dispara o fluxo normal de viewer com lista de arquivos.
        let args: Vec<std::ffi::OsString> =
            paths.iter().map(|p| p.as_os_str().to_os_string()).collect();
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe).args(&args).spawn();
            window.close();
        }
    });
}

fn refresh_title(state: &Rc<RefCell<ViewerState>>) {
    let s = state.borrow();
    if s.files.is_empty() {
        return;
    }
    let path = &s.files[s.idx];
    let base_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    // `•` à esquerda sinaliza edições pendentes. Convenção Loupe/TextEditor.
    let name = if s.pending_ops.is_empty() { base_name } else { format!("• {base_name}") };
    let zoom_desc = match s.zoom {
        ZoomState::Fit => "Ajustar".to_string(),
        ZoomState::Scale(x) => format!("{}%", (x * 100.0).round() as i32),
    };
    let subtitle = format!("{} de {} · {}", s.idx + 1, s.files.len(), zoom_desc);
    s.title.set_title(&name);
    s.title.set_subtitle(&subtitle);
    drop(s);
    refresh_osd(state);
    refresh_film_strip_active(state);
}

type Histogram = [u32; 768];
type RawFrame = (Vec<u8>, u32, u32, Histogram);

fn decode_frame(path: &Path, ops: &[EditOp]) -> Result<RawFrame, Box<dyn std::error::Error>> {
    let img = bigimage_core::open_image(path)?;
    let edited = ops.iter().fold(img, |acc, op| apply_op_to_image(acc, *op));
    let rgba = edited.to_rgba8();
    let (w, h) = rgba.dimensions();
    let raw = rgba.into_raw();
    let histogram = compute_histogram(&raw);
    Ok((raw, w, h, histogram))
}

fn texture_from_rgba(raw: Vec<u8>, w: u32, h: u32) -> gdk::Texture {
    let stride = (w as usize) * 4;
    let bytes = glib::Bytes::from_owned(raw);
    gdk::MemoryTexture::new(w as i32, h as i32, gdk::MemoryFormat::R8g8b8a8, &bytes, stride)
        .upcast()
}

/// Conta ocorrências de cada byte (0..256) por canal em um buffer RGBA
/// contíguo. Ignora alpha — pro histograma do viewer o que importa é a
/// distribuição de luminância por canal. Retorna `[R..256, G..256, B..256]`
/// concatenado pra o rendering ficar com `.chunks(256)`.
fn compute_histogram(rgba: &[u8]) -> [u32; 768] {
    let mut h = [0u32; 768];
    for px in rgba.as_chunks::<4>().0 {
        h[px[0] as usize] += 1;
        h[256 + px[1] as usize] += 1;
        h[512 + px[2] as usize] += 1;
    }
    h
}

// --------------------------------------------------------------------------
// Dialog modals (Prisma) — launched by service menus via `bigiris --dialog=X`.
// --------------------------------------------------------------------------

use bigimage_core::{
    adjust_file, convert_file_to, crop_file, extent_file, flip_file, make_gif, metadata,
    resize_file, rotate_file, AdjustOps, AnimateOptions, CropRect, EncodeOptions, ExtentOptions,
    ExtentSize, Filter, FlipAxis, Format, Gravity, LoopMode, OverwritePolicy, PreviewOp,
    PreviewSession, ResizeMode, Rotation,
};
use image::DynamicImage;
use std::time::Duration;

/// Dialog signals capture related widgets for the duration of the dialog.
/// Disconnect when the surface is released, before object finalization:
/// a strong callback cycle would prevent `destroy` from being emitted.
fn disconnect_with_window(
    window: &adw::ApplicationWindow,
    object: &impl IsA<glib::Object>,
    handler: glib::SignalHandlerId,
) {
    let object = object.as_ref().downgrade();
    let handler = RefCell::new(Some(handler));
    window.connect_unrealize(move |_| {
        if let (Some(object), Some(handler)) = (object.upgrade(), handler.borrow_mut().take()) {
            object.disconnect(handler);
        }
    });
}

macro_rules! dialog_signal {
    ($window:expr, $object:expr, $method:ident, $callback:expr) => {{
        let owner = $window.clone();
        let object = $object.clone();
        let handler = object.$method($callback);
        disconnect_with_window(&owner, &object, handler);
    }};
}

/// Target formats shared by Prisma dialogs; keep common formats first.
const CONVERT_FORMATS: &[(&str, Format)] = &[
    ("PNG", Format::Png),
    ("JPG (JPEG)", Format::Jpeg),
    ("WebP", Format::WebP),
    ("AVIF", Format::Avif),
    ("TIFF", Format::Tiff),
    ("BMP", Format::Bmp),
    ("GIF", Format::Gif),
    ("ICO (até 256 × 256)", Format::Ico),
    ("PNM (Netpbm)", Format::Pnm),
    ("TGA", Format::Tga),
    ("QOI", Format::Qoi),
    ("HDR", Format::Hdr),
    ("OpenEXR", Format::OpenExr),
];

/// Enable only the controls honored by the selected encoder.
fn bind_encode_controls(
    window: &adw::ApplicationWindow,
    format_row: &adw::ComboRow,
    quality: &adw::SpinRow,
    optimize: &adw::SwitchRow,
) {
    let selected = format_row.clone();
    let quality = quality.clone();
    let optimize = optimize.clone();
    let update = move || {
        let format = CONVERT_FORMATS[selected.selected() as usize].1;
        quality.set_sensitive(matches!(format, Format::Jpeg | Format::Avif));
        optimize.set_sensitive(matches!(format, Format::Png | Format::Avif));
    };
    update();
    dialog_signal!(window, format_row, connect_selected_notify, move |_| update());
}

/// Overwrite choices shared by convert and resize dialogs.
const OVERWRITE_CHOICES: &[(&str, OverwritePolicy)] = &[
    ("Pular se já existir", OverwritePolicy::Skip),
    ("Sobrescrever o arquivo", OverwritePolicy::Replace),
    ("Incrementar (_1, _2, …)", OverwritePolicy::Increment),
];

/// Interpolation filters for the resize dialog.
const RESIZE_FILTERS: &[(&str, Filter)] = &[
    ("Lanczos3 (padrão)", Filter::Lanczos3),
    ("Mitchell", Filter::Mitchell),
    ("CatmullRom", Filter::CatmullRom),
    ("Bilinear", Filter::Bilinear),
    ("Nearest (sem anti-aliasing)", Filter::Nearest),
];

/// Run the "Converter" modal. Returns GTK's exit code.
pub fn run_convert_dialog(files: Vec<PathBuf>) -> i32 {
    let app = adw::Application::builder().application_id(APP_ID).build();
    let files = Rc::new(files);
    app.connect_activate(move |app| {
        build_convert_dialog(app, files.clone()).present();
    });
    let code = app.run_with_args::<&str>(&[]);
    i32::from(u8::from(code))
}

fn build_convert_dialog(app: &adw::Application, files: Rc<Vec<PathBuf>>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(880)
        .default_height(500)
        .title(gettext("Converter imagens — Prisma"))
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
    let title = adw::WindowTitle::new("Converter", "Prisma");
    header.set_title_widget(Some(&title));

    let format_names: Vec<&str> = CONVERT_FORMATS.iter().map(|(n, _)| *n).collect();
    let format_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Formato de destino"))
        .model(&gtk::StringList::new(&format_names))
        .build();

    let overwrite_names: Vec<&str> = OVERWRITE_CHOICES.iter().map(|(n, _)| *n).collect();
    let overwrite_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Se o arquivo de saída existir"))
        .model(&gtk::StringList::new(&overwrite_names))
        .build();

    let files_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(format!("{} arquivo(s) selecionado(s)", files.len()))
        .build();
    files_row.set_subtitle(&files_subtitle(&files));

    let quality_spin = adw::SpinRow::with_range(1.0, 100.0, 1.0);
    quality_spin.set_title(&gettext("Qualidade (JPEG/AVIF)"));
    quality_spin.set_subtitle("1 menor qualidade · 85 equilíbrio · 100 maior qualidade");
    quality_spin.set_value(85.0);

    let optimize_switch = adw::SwitchRow::builder()
        .use_markup(false)
        .title(gettext("Otimizar tamanho"))
        .subtitle("PNG com compressão máxima; AVIF mais lento; arquivo menor")
        .active(false)
        .build();

    // Keep encoder tuning outside the primary conversion workflow.
    let advanced_expander = adw::ExpanderRow::builder()
        .title(gettext("Avançado"))
        .subtitle(gettext("Otimização do encoder"))
        .build();
    advanced_expander.add_row(&optimize_switch);
    bind_encode_controls(&window, &format_row, &quality_spin, &optimize_switch);

    // ── Conversão group ───────────────────────────────────────────────
    let conv_group = adw::PreferencesGroup::builder().title(gettext("Conversão")).build();
    conv_group.add(&files_row);
    conv_group.add(&format_row);
    conv_group.add(&quality_spin);
    conv_group.add(&advanced_expander);

    // ── Destino group: overwrite policy + optional output folder ──────
    let dest_group = adw::PreferencesGroup::builder().title(gettext("Destino")).build();
    dest_group.add(&overwrite_row);

    // Pasta de saída picker (mirror do Batch dialog) — None = mesma
    // pasta do arquivo de origem (comportamento histórico).
    let output_dir: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
    let folder_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Pasta de saída"))
        .subtitle(gettext("Mesma pasta do arquivo de origem"))
        .activatable(true)
        .build();
    let folder_icon = gtk::Image::from_icon_name("folder-open-symbolic");
    folder_icon.add_css_class("dim-label");
    folder_row.add_suffix(&folder_icon);
    let clear_folder_btn = gtk::Button::builder()
        .icon_name("edit-clear-symbolic")
        .tooltip_text("Usar a mesma pasta do arquivo de origem")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .visible(false)
        .build();
    folder_row.add_suffix(&clear_folder_btn);
    dest_group.add(&folder_row);

    let status = gtk::Label::builder().label("").wrap(true).xalign(0.0).build();
    status.add_css_class("dim-label");

    let cancel_btn = gtk::Button::with_label("Cancelar");
    let apply_btn = gtk::Button::builder().label("Converter").build();
    apply_btn.add_css_class("suggested-action");
    apply_btn.add_css_class("pill");

    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    actions.append(&cancel_btn);
    actions.append(&apply_btn);

    // Smart prompt: if the source image has an alpha channel and the user
    // picks JPEG, warn that transparency will be flattened.
    let alpha_banner = adw::Banner::builder()
        .title(gettext(
            "Esta imagem tem transparência — JPEG perde o canal alfa. Considere PNG ou WebP.",
        ))
        .revealed(false)
        .build();
    if let Some(sess) = preview_session.as_ref() {
        if sess.has_alpha_channel() {
            let format_row_clone = format_row.clone();
            let banner = alpha_banner.clone();
            let update = move || {
                let fmt = CONVERT_FORMATS[format_row_clone.selected() as usize].1;
                let warn = matches!(fmt, Format::Jpeg);
                banner.set_revealed(warn);
            };
            update();
            dialog_signal!(window, format_row, connect_selected_notify, move |_| update());
        }
    }

    // Smart prompt: LGPD / privacidade — se a imagem tem coordenadas GPS
    // no EXIF, avisa que o upload pode vazar a localização.
    let gps_banner = adw::Banner::builder()
        .title(
            "Esta imagem contém GPS (localização) no EXIF. Atenção ao compartilhar \
             publicamente — nosso pipeline remove o EXIF automaticamente no re-encode.",
        )
        .revealed(false)
        .build();
    gps_banner.add_css_class("warning");
    if let Some(sess) = preview_session.as_ref() {
        if sess.source_has_gps() {
            gps_banner.set_revealed(true);
        }
    }

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    content.append(&gps_banner);
    content.append(&alpha_banner);
    content.append(&conv_group);
    content.append(&dest_group);
    content.append(&status);
    content.append(&actions);
    content.set_size_request(420, -1);

    // Wire up the output-folder picker + reset button.
    {
        let output_dir = output_dir.clone();
        let folder_row_for_pick = folder_row.clone();
        let clear_btn = clear_folder_btn.clone();
        let window = window.clone();
        dialog_signal!(window, folder_row.clone(), connect_activated, move |_| {
            pick_output_folder(&window, &output_dir, &folder_row_for_pick, &clear_btn);
        });
    }
    {
        let output_dir = output_dir.clone();
        let folder_row = folder_row.clone();
        let clear_btn = clear_folder_btn.clone();
        dialog_signal!(window, clear_folder_btn, connect_clicked, move |_| {
            *output_dir.borrow_mut() = None;
            folder_row.set_subtitle(&gettext("Mesma pasta do arquivo de origem"));
            clear_btn.set_visible(false);
        });
    }

    let main_box =
        gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(16).build();
    main_box.append(&content);
    if let Some(sess) = preview_session.as_ref() {
        let (_picture, column) = build_preview_column(sess);
        column.set_margin_top(16);
        column.set_margin_bottom(16);
        column.set_margin_end(16);
        main_box.append(&column);
        // Convert is format-level — visual preview is just the source
        // thumbnail (pixels don't change meaningfully between PNG/JPG/WebP
        // at default quality). No refresh wiring needed.
    }

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&main_box));
    window.set_content(Some(&toolbar_view));

    {
        let window = window.clone();
        dialog_signal!(window, cancel_btn, connect_clicked, move |_| window.close());
    }

    {
        let files = files.clone();
        let format_row = format_row.clone();
        let overwrite_row = overwrite_row.clone();
        let quality_spin = quality_spin.clone();
        let optimize_switch = optimize_switch.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        let output_dir = output_dir.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let fmt_idx = format_row.selected() as usize;
            let policy_idx = overwrite_row.selected() as usize;
            let fmt = CONVERT_FORMATS[fmt_idx].1;
            let policy = OVERWRITE_CHOICES[policy_idx].1;
            let opts = EncodeOptions {
                quality: Some(quality_spin.value() as u8),
                optimize: optimize_switch.is_active(),
                ..EncodeOptions::default()
            };
            // Snapshot the chosen folder once at submit time — the
            // picker can change while the worker is running and the
            // worker thread mustn't peek into Rc<RefCell<...>>.
            let out_dir_snapshot = output_dir.borrow().clone();

            apply_btn.set_sensitive(false);
            cancel_btn.set_sensitive(false);
            status.set_text(&gettext("Convertendo…"));

            let work_files: Vec<PathBuf> = (*files).clone();
            run_batch_async(
                work_files,
                move |f| convert_file_to(f, out_dir_snapshot.as_deref(), fmt, &opts, policy),
                gettext("Convertendo"),
                status.clone(),
                apply_btn.clone(),
                cancel_btn.clone(),
                window.clone(),
            );
        });
    }

    window
}

/// Run the "Redimensionar" modal.
pub fn run_resize_dialog(files: Vec<PathBuf>) -> i32 {
    let app = adw::Application::builder().application_id(APP_ID).build();
    let files = Rc::new(files);
    app.connect_activate(move |app| {
        build_resize_dialog(app, files.clone()).present();
    });
    let code = app.run_with_args::<&str>(&[]);
    i32::from(u8::from(code))
}

fn build_resize_dialog(app: &adw::Application, files: Rc<Vec<PathBuf>>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(900)
        .default_height(640)
        .title(gettext("Redimensionar imagens — Prisma"))
        .build();
    prepare_dialog(&window, files, fill_resize_dialog);
    window
}

fn fill_resize_dialog(
    window: adw::ApplicationWindow,
    files: Rc<Vec<PathBuf>>,
    preview_session: Option<Rc<PreviewSession>>,
) -> adw::ApplicationWindow {
    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("Redimensionar", "Prisma");
    header.set_title_widget(Some(&title));

    // ---- Mode selector ------------------------------------------------------

    let mode_names = ["Aresta máxima", "Escala (%)", "Dimensão exata (WxH)", "Encaixar em (WxH)"];
    let mode_row = adw::ComboRow::builder()
        .use_markup(false)
        .title("Modo")
        .model(&gtk::StringList::new(&mode_names))
        .build();

    // Per-mode parameter pages live inside a Stack; switching the combo swaps
    // the page. Each page owns its own SpinRows so we can read values back.
    let max_edge_spin = adw::SpinRow::with_range(1.0, 16384.0, 1.0);
    max_edge_spin.set_title(&gettext("Aresta máxima"));
    max_edge_spin.set_subtitle("pixels (lado maior)");
    max_edge_spin.set_value(1920.0);

    let percent_spin = adw::SpinRow::with_range(1.0, 1000.0, 1.0);
    percent_spin.set_title(&gettext("Porcentagem"));
    percent_spin.set_subtitle("100 = tamanho original");
    percent_spin.set_value(50.0);

    let exact_w = adw::SpinRow::with_range(1.0, 16384.0, 1.0);
    exact_w.set_title(&gettext("Largura"));
    exact_w.set_value(1920.0);
    let exact_h = adw::SpinRow::with_range(1.0, 16384.0, 1.0);
    exact_h.set_title(&gettext("Altura"));
    exact_h.set_value(1080.0);

    let fit_w = adw::SpinRow::with_range(1.0, 16384.0, 1.0);
    fit_w.set_title(&gettext("Largura máxima"));
    fit_w.set_value(1920.0);
    let fit_h = adw::SpinRow::with_range(1.0, 16384.0, 1.0);
    fit_h.set_title(&gettext("Altura máxima"));
    fit_h.set_value(1080.0);

    let page_max_edge = adw::PreferencesGroup::new();
    page_max_edge.add(&max_edge_spin);
    let page_percent = adw::PreferencesGroup::new();
    page_percent.add(&percent_spin);
    let page_exact = adw::PreferencesGroup::new();
    page_exact.add(&exact_w);
    page_exact.add(&exact_h);
    let page_fit = adw::PreferencesGroup::new();
    page_fit.add(&fit_w);
    page_fit.add(&fit_h);

    let stack = gtk::Stack::new();
    stack.add_named(&page_max_edge, Some("max-edge"));
    stack.add_named(&page_percent, Some("percent"));
    stack.add_named(&page_exact, Some("exact"));
    stack.add_named(&page_fit, Some("fit"));
    stack.set_visible_child_name("max-edge");

    {
        let stack = stack.clone();
        dialog_signal!(window, mode_row, connect_selected_notify, move |combo| {
            let name = match combo.selected() {
                0 => "max-edge",
                1 => "percent",
                2 => "exact",
                _ => "fit",
            };
            stack.set_visible_child_name(name);
        });
    }

    // ---- Filter + format + overwrite ---------------------------------------

    let filter_names: Vec<&str> = RESIZE_FILTERS.iter().map(|(n, _)| *n).collect();
    let filter_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Kernel de interpolação"))
        .model(&gtk::StringList::new(&filter_names))
        .build();

    let mut target_names = vec!["Manter o formato original"];
    target_names.extend(CONVERT_FORMATS.iter().map(|(n, _)| *n));
    let target_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Formato de destino"))
        .model(&gtk::StringList::new(&target_names))
        .build();

    let overwrite_names: Vec<&str> = OVERWRITE_CHOICES.iter().map(|(n, _)| *n).collect();
    let overwrite_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Se o arquivo de saída existir"))
        .model(&gtk::StringList::new(&overwrite_names))
        .build();

    let files_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(format!("{} arquivo(s) selecionado(s)", files.len()))
        .build();
    files_row.set_subtitle(&files_subtitle(&files));

    let group_files = adw::PreferencesGroup::new();
    group_files.add(&files_row);

    let group_mode =
        adw::PreferencesGroup::builder().title(gettext("Modo de redimensionamento")).build();
    group_mode.add(&mode_row);

    let group_options = adw::PreferencesGroup::builder().title(gettext("Opções")).build();
    group_options.add(&filter_row);
    group_options.add(&target_row);
    group_options.add(&overwrite_row);

    let status = gtk::Label::builder().label("").wrap(true).xalign(0.0).build();
    status.add_css_class("dim-label");

    let cancel_btn = gtk::Button::with_label("Cancelar");
    let apply_btn = gtk::Button::builder().label("Redimensionar").build();
    apply_btn.add_css_class("suggested-action");
    apply_btn.add_css_class("pill");

    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    actions.append(&cancel_btn);
    actions.append(&apply_btn);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    content.append(&group_files);
    content.append(&group_mode);
    content.append(&stack);
    content.append(&group_options);
    content.append(&status);
    content.append(&actions);

    // Scroll the whole content in case the resize dialog overflows a small
    // screen — it's taller than the convert dialog.
    let scrolled = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&content)
        .build();
    scrolled.set_size_request(420, -1);

    let main_box =
        gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(16).build();
    main_box.append(&scrolled);
    let preview_picture: Option<gtk::Picture> = if let Some(sess) = preview_session.as_ref() {
        let (picture, column) = build_preview_column(sess);
        column.set_margin_top(16);
        column.set_margin_bottom(16);
        column.set_margin_end(16);
        main_box.append(&column);
        Some(picture)
    } else {
        None
    };

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&main_box));
    window.set_content(Some(&toolbar_view));

    // Live preview: any change in mode / filter / dimensions re-renders.
    if let (Some(picture), Some(session)) = (preview_picture.as_ref(), preview_session.as_ref()) {
        let debounce = PreviewDebounce::new();
        let mode_row_clone = mode_row.clone();
        let filter_row_clone = filter_row.clone();
        let max_edge_clone = max_edge_spin.clone();
        let percent_clone = percent_spin.clone();
        let exact_w_clone = exact_w.clone();
        let exact_h_clone = exact_h.clone();
        let fit_w_clone = fit_w.clone();
        let fit_h_clone = fit_h.clone();
        let schedule = {
            let debounce = debounce.clone();
            let session = session.clone();
            let picture = picture.clone();
            let mode_row = mode_row_clone.clone();
            let filter_row = filter_row_clone.clone();
            let max_edge_spin = max_edge_clone.clone();
            let percent_spin = percent_clone.clone();
            let exact_w = exact_w_clone.clone();
            let exact_h = exact_h_clone.clone();
            let fit_w = fit_w_clone.clone();
            let fit_h = fit_h_clone.clone();
            Rc::new(move || {
                let mode = match mode_row.selected() {
                    0 => ResizeMode::MaxEdge(max_edge_spin.value() as u32),
                    1 => ResizeMode::Percent(percent_spin.value() as f32),
                    2 => ResizeMode::Exact {
                        width: exact_w.value() as u32,
                        height: exact_h.value() as u32,
                    },
                    _ => ResizeMode::Fit {
                        width: fit_w.value() as u32,
                        height: fit_h.value() as u32,
                    },
                };
                let filter = RESIZE_FILTERS[filter_row.selected() as usize].1;
                let session = session.clone();
                let picture = picture.clone();
                debounce.trigger(Duration::from_millis(120), move || {
                    refresh_preview(&session, &picture, &PreviewOp::Resize { mode, filter });
                });
            })
        };

        // Mode / filter combo changes
        {
            let s = schedule.clone();
            dialog_signal!(window, mode_row_clone, connect_selected_notify, move |_| s());
        }
        {
            let s = schedule.clone();
            dialog_signal!(window, filter_row_clone, connect_selected_notify, move |_| s());
        }
        // Spin changes in each mode page
        for row in
            &[max_edge_clone, percent_clone, exact_w_clone, exact_h_clone, fit_w_clone, fit_h_clone]
        {
            let s = schedule.clone();
            dialog_signal!(window, row, connect_changed, move |_| s());
        }
    }

    {
        let window = window.clone();
        dialog_signal!(window, cancel_btn, connect_clicked, move |_| window.close());
    }

    {
        let files = files.clone();
        let mode_row = mode_row.clone();
        let filter_row = filter_row.clone();
        let target_row = target_row.clone();
        let overwrite_row = overwrite_row.clone();
        let max_edge_spin = max_edge_spin.clone();
        let percent_spin = percent_spin.clone();
        let exact_w = exact_w.clone();
        let exact_h = exact_h.clone();
        let fit_w = fit_w.clone();
        let fit_h = fit_h.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let mode = match mode_row.selected() {
                0 => ResizeMode::MaxEdge(max_edge_spin.value() as u32),
                1 => ResizeMode::Percent(percent_spin.value() as f32),
                2 => ResizeMode::Exact {
                    width: exact_w.value() as u32,
                    height: exact_h.value() as u32,
                },
                _ => ResizeMode::Fit { width: fit_w.value() as u32, height: fit_h.value() as u32 },
            };
            let filter = RESIZE_FILTERS[filter_row.selected() as usize].1;
            let target: Option<Format> = match target_row.selected() {
                0 => None,
                n => Some(CONVERT_FORMATS[(n as usize) - 1].1),
            };
            let policy = OVERWRITE_CHOICES[overwrite_row.selected() as usize].1;
            // Resize dialog doesn't expose quality today — uses defaults.
            // Users who need quality control go through Convert ▸ Personalizar…
            // or chain via CLI.
            let opts = EncodeOptions::default();

            apply_btn.set_sensitive(false);
            cancel_btn.set_sensitive(false);
            status.set_text(&gettext("Redimensionando…"));

            let work_files: Vec<PathBuf> = (*files).clone();
            run_batch_async(
                work_files,
                move |f| resize_file(f, mode, filter, target, &opts, policy),
                gettext("Redimensionando"),
                status.clone(),
                apply_btn.clone(),
                cancel_btn.clone(),
                window.clone(),
            );
        });
    }

    window
}

fn files_subtitle(files: &[PathBuf]) -> String {
    if files.is_empty() {
        return "—".to_string();
    }
    let first = files[0].file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if files.len() == 1 {
        first
    } else {
        format!("{first} +{} outros", files.len() - 1)
    }
}

mod batch_runner;
use batch_runner::run_batch_async;

// ---------------------------------------------------------------------------
// Rotate / Flip / Adjust dialogs — built on top of the same run_*_batch +
// finish_dialog primitives used by convert/resize, just with different forms.
// ---------------------------------------------------------------------------

/// Rotate modal: pick 90 / 180 / 270 and an optional target format.
pub fn run_rotate_dialog(files: Vec<PathBuf>) -> i32 {
    let app = adw::Application::builder().application_id(APP_ID).build();
    let files = Rc::new(files);
    app.connect_activate(move |app| {
        build_rotate_dialog(app, files.clone()).present();
    });
    let code = app.run_with_args::<&str>(&[]);
    i32::from(u8::from(code))
}

fn build_rotate_dialog(app: &adw::Application, files: Rc<Vec<PathBuf>>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(440)
        .default_height(340)
        .title(gettext("Girar imagens — Prisma"))
        .build();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Girar", "Prisma")));

    let rotations = ["90° (horário)", "180°", "270° (anti-horário)"];
    let rotation_row = adw::ComboRow::builder()
        .use_markup(false)
        .title("Rotação")
        .model(&gtk::StringList::new(&rotations))
        .build();

    let mut target_names = vec!["Manter o formato original"];
    target_names.extend(CONVERT_FORMATS.iter().map(|(n, _)| *n));
    let target_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Formato de destino"))
        .model(&gtk::StringList::new(&target_names))
        .build();

    let overwrite_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Se o arquivo de saída existir"))
        .model(&gtk::StringList::new(
            &OVERWRITE_CHOICES.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        ))
        .build();

    let files_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(format!("{} arquivo(s) selecionado(s)", files.len()))
        .build();
    files_row.set_subtitle(&files_subtitle(&files));

    let group = adw::PreferencesGroup::new();
    group.add(&files_row);
    group.add(&rotation_row);
    group.add(&target_row);
    group.add(&overwrite_row);

    let status = gtk::Label::builder().label("").wrap(true).xalign(0.0).build();
    status.add_css_class("dim-label");

    let cancel_btn = gtk::Button::with_label("Cancelar");
    let apply_btn = gtk::Button::builder().label("Girar").build();
    apply_btn.add_css_class("suggested-action");
    apply_btn.add_css_class("pill");

    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    actions.append(&cancel_btn);
    actions.append(&apply_btn);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    content.append(&group);
    content.append(&status);
    content.append(&actions);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&content));
    window.set_content(Some(&toolbar_view));

    {
        let window = window.clone();
        dialog_signal!(window, cancel_btn, connect_clicked, move |_| window.close());
    }

    {
        let files = files.clone();
        let rotation_row = rotation_row.clone();
        let target_row = target_row.clone();
        let overwrite_row = overwrite_row.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let rotation = match rotation_row.selected() {
                0 => Rotation::Deg90,
                1 => Rotation::Deg180,
                _ => Rotation::Deg270,
            };
            let target: Option<Format> = match target_row.selected() {
                0 => None,
                n => Some(CONVERT_FORMATS[(n as usize) - 1].1),
            };
            let policy = OVERWRITE_CHOICES[overwrite_row.selected() as usize].1;

            apply_btn.set_sensitive(false);
            cancel_btn.set_sensitive(false);
            status.set_text(&gettext("Girando…"));

            let work_files: Vec<PathBuf> = (*files).clone();
            run_batch_async(
                work_files,
                move |f| rotate_file(f, rotation, target, policy),
                gettext("Girando"),
                status.clone(),
                apply_btn.clone(),
                cancel_btn.clone(),
                window.clone(),
            );
        });
    }

    window
}

/// Default crop: a centred 50% rectangle — a safe non-empty starting
/// point for the UI that's unlikely to match the full image.
fn default_crop(img_w: u32, img_h: u32) -> CropRect {
    let w = (img_w / 2).max(1);
    let h = (img_h / 2).max(1);
    let x = (img_w.saturating_sub(w)) / 2;
    let y = (img_h.saturating_sub(h)) / 2;
    CropRect::new(x, y, w, h)
}

/// What the user is doing while the GestureDrag is active on the crop
/// preview. Decided at `drag_begin` from the hit-test against the
/// current rect; stays constant until drag ends.
#[derive(Clone, Copy, Debug, PartialEq)]
enum CropDragMode {
    /// Click+drag outside the rect (or on an empty canvas) grows a new
    /// rect starting at the click point.
    NewSelection {
        /// Click position in natural-image pixels.
        anchor_nat: (f64, f64),
    },
    /// Click strictly inside the current rect → translate it; its size
    /// is preserved. `rect0` is the rect at drag start (x, y, w, h),
    /// `click_nat` the click point in natural pixels.
    Move { rect0: (f64, f64, f64, f64), click_nat: (f64, f64) },
    /// Click on a corner handle → resize anchored at the opposite
    /// corner. `anchor_nat` is the fixed corner, `click_nat` the moving
    /// one at drag start.
    ResizeCorner { anchor_nat: (f64, f64), click_nat: (f64, f64) },
}

/// Widget-pixel tolerance for corner-handle hit-testing.
const CROP_HANDLE_TOLERANCE_WIDGET_PX: f64 = 12.0;

/// Decide the drag mode from the click's position relative to the
/// current rect. `fit` is `(scale, ox, oy)` from [`contain_fit`];
/// `rect` is the current crop in natural-image pixels.
fn decide_drag_mode(
    click_widget: (f64, f64),
    rect: (f64, f64, f64, f64),
    fit: (f64, f64, f64),
    natural: (u32, u32),
) -> CropDragMode {
    let (sx, sy) = click_widget;
    let (rx, ry, rw, rh) = rect;
    let (scale, ox, oy) = fit;
    let nat_w = natural.0 as f64;
    let nat_h = natural.1 as f64;
    let click_nat = (((sx - ox) / scale).clamp(0.0, nat_w), ((sy - oy) / scale).clamp(0.0, nat_h));

    // Widget-space corners of the current rect.
    let corners_nat = [
        (rx, ry),           // 0: top-left      ↔ 3 bottom-right
        (rx + rw, ry),      // 1: top-right     ↔ 2 bottom-left
        (rx, ry + rh),      // 2: bottom-left   ↔ 1 top-right
        (rx + rw, ry + rh), // 3: bottom-right  ↔ 0 top-left
    ];
    let corners_widget: [(f64, f64); 4] = core::array::from_fn(|i| {
        let (nx, ny) = corners_nat[i];
        (ox + nx * scale, oy + ny * scale)
    });
    let tol2 = CROP_HANDLE_TOLERANCE_WIDGET_PX * CROP_HANDLE_TOLERANCE_WIDGET_PX;
    for (i, (cx, cy)) in corners_widget.iter().enumerate() {
        let d2 = (sx - cx).powi(2) + (sy - cy).powi(2);
        if d2 <= tol2 {
            let anchor_nat = corners_nat[3 - i];
            return CropDragMode::ResizeCorner { anchor_nat, click_nat: corners_nat[i] };
        }
    }

    // Inside the rect (strict) → move.
    let inside =
        click_nat.0 > rx && click_nat.0 < rx + rw && click_nat.1 > ry && click_nat.1 < ry + rh;
    if inside {
        return CropDragMode::Move { rect0: rect, click_nat };
    }

    CropDragMode::NewSelection { anchor_nat: click_nat }
}

/// Apply a drag delta (already in *natural-pixel* units via `(dx/scale,
/// dy/scale)`) to `mode` and return the resulting `(x, y, w, h)` in
/// natural pixels, clamped to image bounds. Width/height never collapse
/// below 1 so the rect stays visible.
fn apply_drag(
    mode: CropDragMode,
    delta_nat: (f64, f64),
    natural: (u32, u32),
) -> (f64, f64, f64, f64) {
    let nat_w = natural.0 as f64;
    let nat_h = natural.1 as f64;
    let (dx, dy) = delta_nat;
    match mode {
        CropDragMode::NewSelection { anchor_nat: (ax, ay) } => {
            let cx = (ax + dx).clamp(0.0, nat_w);
            let cy = (ay + dy).clamp(0.0, nat_h);
            let x0 = ax.min(cx);
            let y0 = ay.min(cy);
            let w = (ax - cx).abs().max(1.0);
            let h = (ay - cy).abs().max(1.0);
            (x0, y0, w, h)
        }
        CropDragMode::Move { rect0: (rx, ry, rw, rh), click_nat: _ } => {
            // Preserve size; translate within bounds.
            let max_x = (nat_w - rw).max(0.0);
            let max_y = (nat_h - rh).max(0.0);
            let new_x = (rx + dx).clamp(0.0, max_x);
            let new_y = (ry + dy).clamp(0.0, max_y);
            (new_x, new_y, rw, rh)
        }
        CropDragMode::ResizeCorner { anchor_nat: (ax, ay), click_nat: (cx0, cy0) } => {
            let cx = (cx0 + dx).clamp(0.0, nat_w);
            let cy = (cy0 + dy).clamp(0.0, nat_h);
            let x0 = ax.min(cx);
            let y0 = ay.min(cy);
            let w = (ax - cx).abs().max(1.0);
            let h = (ay - cy).abs().max(1.0);
            (x0, y0, w, h)
        }
    }
}

/// Layout math for a `gtk::Picture` with `ContentFit::Contain`: given
/// the widget's allocated `(ww, wh)` and the displayed image's natural
/// `(iw, ih)`, returns `(scale, ox, oy)` — the uniform scale factor and
/// top-left offset of the rendered image inside the widget box.
///
/// Returns a safe identity transform when any input is non-positive so the
/// caller can unconditionally multiply without a division guard.
fn contain_fit(ww: f64, wh: f64, iw: f64, ih: f64) -> (f64, f64, f64) {
    if ww <= 0.0 || wh <= 0.0 || iw <= 0.0 || ih <= 0.0 {
        return (1.0, 0.0, 0.0);
    }
    let s = (ww / iw).min(wh / ih);
    let disp_w = iw * s;
    let disp_h = ih * s;
    ((s), (ww - disp_w) / 2.0, (wh - disp_h) / 2.0)
}

/// Build the interactive preview column for the crop dialog: full
/// thumbnail in a `gtk::Picture`, plus a `gtk::DrawingArea` overlay that
/// strokes the selection rect. A `GestureDrag` on the overlay updates the
/// spin rows (which remain authoritative); spin changes queue a redraw.
fn build_crop_preview_column(
    session: &Rc<PreviewSession>,
    natural: (u32, u32),
    x_spin: &adw::SpinRow,
    y_spin: &adw::SpinRow,
    w_spin: &adw::SpinRow,
    h_spin: &adw::SpinRow,
) -> gtk::Box {
    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    picture.set_size_request(360, 360);
    picture.set_paintable(Some(&dynamic_image_to_texture(session.thumbnail().clone())));

    let selection_area = gtk::DrawingArea::builder().hexpand(true).vexpand(true).build();

    // Draw: two-pass — shade the outside of the selection rect, then
    // stroke the rect itself with a bright outline + corner handles.
    {
        let x_spin = x_spin.clone();
        let y_spin = y_spin.clone();
        let w_spin = w_spin.clone();
        let h_spin = h_spin.clone();
        selection_area.set_draw_func(move |_area, cr, ww, wh| {
            let ww = ww as f64;
            let wh = wh as f64;
            let (scale, ox, oy) = contain_fit(ww, wh, natural.0 as f64, natural.1 as f64);
            let rx = ox + x_spin.value() * scale;
            let ry = oy + y_spin.value() * scale;
            let rw = w_spin.value() * scale;
            let rh = h_spin.value() * scale;

            // Shade outside the rect (four bands) for clear selection focus.
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.45);
            cr.rectangle(0.0, 0.0, ww, ry);
            cr.rectangle(0.0, ry + rh, ww, wh - (ry + rh));
            cr.rectangle(0.0, ry, rx, rh);
            cr.rectangle(rx + rw, ry, ww - (rx + rw), rh);
            let _ = cr.fill();

            // Selection outline.
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
            cr.set_line_width(1.5);
            cr.rectangle(rx + 0.5, ry + 0.5, rw - 1.0, rh - 1.0);
            let _ = cr.stroke();

            // Corner handles — small squares to hint "drag me".
            cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
            let k = 5.0;
            for (cx, cy) in [(rx, ry), (rx + rw, ry), (rx, ry + rh), (rx + rw, ry + rh)] {
                cr.rectangle(cx - k / 2.0, cy - k / 2.0, k, k);
            }
            let _ = cr.fill();
        });
    }

    // Spin changes → queue redraw of the overlay.
    for spin in [x_spin, y_spin, w_spin, h_spin] {
        let area = selection_area.clone();
        spin.connect_changed(move |_| area.queue_draw());
    }

    // Drag semantics — three modes, decided at drag_begin from the click
    // point's position relative to the current rect:
    //   - on a corner handle  → ResizeCorner (opposite corner anchors)
    //   - strictly inside     → Move (rect translates, size preserved)
    //   - outside             → NewSelection (classic draw-a-box)
    let drag = gtk::GestureDrag::builder().button(1).build();
    let drag_mode: Rc<Cell<CropDragMode>> =
        Rc::new(Cell::new(CropDragMode::NewSelection { anchor_nat: (0.0, 0.0) }));
    {
        let area = selection_area.clone();
        let mode = drag_mode.clone();
        let x_spin = x_spin.clone();
        let y_spin = y_spin.clone();
        let w_spin = w_spin.clone();
        let h_spin = h_spin.clone();
        drag.connect_drag_begin(move |_, sx, sy| {
            let (scale, ox, oy) = contain_fit(
                area.width() as f64,
                area.height() as f64,
                natural.0 as f64,
                natural.1 as f64,
            );
            if scale <= 0.0 {
                return;
            }
            let rect = (x_spin.value(), y_spin.value(), w_spin.value(), h_spin.value());
            let decided = decide_drag_mode((sx, sy), rect, (scale, ox, oy), natural);
            mode.set(decided);
            if let CropDragMode::NewSelection { anchor_nat } = decided {
                // Seed a 1px rect anchored at the click so the overlay is
                // visible immediately — update handler will expand it.
                x_spin.set_value(anchor_nat.0);
                y_spin.set_value(anchor_nat.1);
                w_spin.set_value(1.0);
                h_spin.set_value(1.0);
            }
        });
    }
    {
        let area = selection_area.clone();
        let mode = drag_mode.clone();
        let x_spin = x_spin.clone();
        let y_spin = y_spin.clone();
        let w_spin = w_spin.clone();
        let h_spin = h_spin.clone();
        drag.connect_drag_update(move |_, dx, dy| {
            let (scale, _ox, _oy) = contain_fit(
                area.width() as f64,
                area.height() as f64,
                natural.0 as f64,
                natural.1 as f64,
            );
            if scale <= 0.0 {
                return;
            }
            let (x0, y0, w, h) = apply_drag(mode.get(), (dx / scale, dy / scale), natural);
            x_spin.set_value(x0);
            y_spin.set_value(y0);
            w_spin.set_value(w);
            h_spin.set_value(h);
        });
    }
    selection_area.add_controller(drag);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&picture));
    overlay.add_overlay(&selection_area);

    let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    column.append(&overlay);

    let hint = gtk::Label::builder()
        .label(
            "Arraste nos cantos para redimensionar · dentro do recorte para mover · \
             fora dele para uma nova seleção.",
        )
        .wrap(true)
        .xalign(0.5)
        .margin_top(6)
        .build();
    hint.add_css_class("dim-label");
    column.append(&hint);

    column
}

/// Crop modal: rectangular window by (x, y, w, h) with live preview.
pub fn run_crop_dialog(files: Vec<PathBuf>) -> i32 {
    let app = adw::Application::builder().application_id(APP_ID).build();
    let files = Rc::new(files);
    app.connect_activate(move |app| {
        build_crop_dialog(app, files.clone()).present();
    });
    let code = app.run_with_args::<&str>(&[]);
    i32::from(u8::from(code))
}

fn build_crop_dialog(app: &adw::Application, files: Rc<Vec<PathBuf>>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(900)
        .default_height(560)
        .title(gettext("Recortar — Prisma"))
        .build();
    prepare_dialog(&window, files, fill_crop_dialog);
    window
}

fn fill_crop_dialog(
    window: adw::ApplicationWindow,
    files: Rc<Vec<PathBuf>>,
    preview_session: Option<Rc<PreviewSession>>,
) -> adw::ApplicationWindow {
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Recortar", "Prisma")));

    let content_h = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(16)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();

    // Preview is keyed off the first file — multi-file dialogs share the
    // rect and per-file validation catches out-of-bounds at apply time.
    let natural = preview_session.as_ref().map(|s| s.natural_size()).unwrap_or((1, 1));
    let initial = default_crop(natural.0, natural.1);

    // Form column — native-width controls for coordinates and size.
    let form_col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .width_request(360)
        .build();

    let files_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(format!("{} arquivo(s) selecionado(s)", files.len()))
        .build();
    files_row.set_subtitle(&files_subtitle(&files));

    let natural_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Tamanho original"))
        .subtitle(format!("{} × {} px (primeiro arquivo)", natural.0, natural.1))
        .build();

    // Ranges clamp to the first file's natural dims. For multi-file sets
    // with different sizes, per-file validation handles the rest.
    let x_spin = adw::SpinRow::with_range(0.0, (natural.0.saturating_sub(1)) as f64, 1.0);
    x_spin.set_title(&gettext("X (px)"));
    x_spin.set_value(initial.x as f64);

    let y_spin = adw::SpinRow::with_range(0.0, (natural.1.saturating_sub(1)) as f64, 1.0);
    y_spin.set_title(&gettext("Y (px)"));
    y_spin.set_value(initial.y as f64);

    let w_spin = adw::SpinRow::with_range(1.0, natural.0 as f64, 1.0);
    w_spin.set_title(&gettext("Largura (px)"));
    w_spin.set_value(initial.width as f64);

    let h_spin = adw::SpinRow::with_range(1.0, natural.1 as f64, 1.0);
    h_spin.set_title(&gettext("Altura (px)"));
    h_spin.set_value(initial.height as f64);

    let mut target_names = vec!["Manter o formato original"];
    target_names.extend(CONVERT_FORMATS.iter().map(|(n, _)| *n));
    let target_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Formato de destino"))
        .model(&gtk::StringList::new(&target_names))
        .build();

    let overwrite_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Se o arquivo de saída existir"))
        .model(&gtk::StringList::new(
            &OVERWRITE_CHOICES.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        ))
        .build();

    let group = adw::PreferencesGroup::new();
    group.add(&files_row);
    group.add(&natural_row);
    group.add(&x_spin);
    group.add(&y_spin);
    group.add(&w_spin);
    group.add(&h_spin);
    group.add(&target_row);
    group.add(&overwrite_row);
    form_col.append(&group);

    let status = gtk::Label::builder().label("").wrap(true).xalign(0.0).build();
    status.add_css_class("dim-label");
    form_col.append(&status);

    let cancel_btn = gtk::Button::with_label("Cancelar");
    let reset_btn = gtk::Button::with_label("Centro 50%");
    let apply_btn = gtk::Button::builder().label("Recortar").build();
    apply_btn.add_css_class("suggested-action");
    apply_btn.add_css_class("pill");

    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    actions.append(&reset_btn);
    actions.append(&cancel_btn);
    actions.append(&apply_btn);
    form_col.append(&actions);

    content_h.append(&form_col);

    // Interactive preview: full thumbnail + DrawingArea overlay that strokes
    // the selection rect and shades the outside. Spin rows are the source of
    // truth; drag gestures update them and the overlay redraws.
    if let Some(sess) = preview_session.as_ref() {
        let column = build_crop_preview_column(sess, natural, &x_spin, &y_spin, &w_spin, &h_spin);
        content_h.append(&column);
    }

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&content_h));
    window.set_content(Some(&toolbar_view));

    // Reset button → re-centre to 50% of the first image.
    {
        let x_spin = x_spin.clone();
        let y_spin = y_spin.clone();
        let w_spin = w_spin.clone();
        let h_spin = h_spin.clone();
        dialog_signal!(window, reset_btn, connect_clicked, move |_| {
            let d = default_crop(natural.0, natural.1);
            w_spin.set_value(d.width as f64);
            h_spin.set_value(d.height as f64);
            x_spin.set_value(d.x as f64);
            y_spin.set_value(d.y as f64);
        });
    }

    {
        let window = window.clone();
        dialog_signal!(window, cancel_btn, connect_clicked, move |_| window.close());
    }

    {
        let files = files.clone();
        let x_spin = x_spin.clone();
        let y_spin = y_spin.clone();
        let w_spin = w_spin.clone();
        let h_spin = h_spin.clone();
        let target_row = target_row.clone();
        let overwrite_row = overwrite_row.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let rect = CropRect::new(
                x_spin.value() as u32,
                y_spin.value() as u32,
                w_spin.value() as u32,
                h_spin.value() as u32,
            );
            let target: Option<Format> = match target_row.selected() {
                0 => None,
                n => Some(CONVERT_FORMATS[(n as usize) - 1].1),
            };
            let policy = OVERWRITE_CHOICES[overwrite_row.selected() as usize].1;

            apply_btn.set_sensitive(false);
            cancel_btn.set_sensitive(false);
            status.set_text(&gettext("Recortando…"));

            let work_files: Vec<PathBuf> = (*files).clone();
            run_batch_async(
                work_files,
                move |f| crop_file(f, rect, target, policy),
                gettext("Recortando"),
                status.clone(),
                apply_btn.clone(),
                cancel_btn.clone(),
                window.clone(),
            );
        });
    }

    window
}

/// Aspect-ratio presets for the "Letterbox" (extent) dialog. Each preset
/// is `(label, (w, h))` with `w:h` the target ratio. Order drives the
/// default selection — YouTube community 1:1 sits first because it's the
/// case that motivated the feature (post thumbnail forces square crop on
/// 16:9 source).
const EXTENT_RATIO_PRESETS: &[(&str, (u32, u32))] = &[
    ("YouTube community (1:1)", (1, 1)),
    ("Instagram feed (4:5)", (4, 5)),
    ("Stories / Shorts (9:16)", (9, 16)),
    ("Widescreen (16:9)", (16, 9)),
    ("Personalizado…", (0, 0)),
];

/// Background-colour presets for the "Letterbox" dialog. The first entry
/// is the default. `Custom` (last) reveals the hex entry.
const EXTENT_BG_PRESETS: &[(&str, [u8; 4])] = &[
    ("Preto suave (#0d0e12)", [0x0d, 0x0e, 0x12, 0xFF]),
    ("Preto puro (#000000)", [0, 0, 0, 0xFF]),
    ("Branco (#ffffff)", [0xFF, 0xFF, 0xFF, 0xFF]),
    ("Transparente", [0, 0, 0, 0]),
    ("Personalizada (hex)", [0, 0, 0, 0xFF]),
];

/// Gravity choices exposed in the Prisma dialog. Limited to centre +
/// vertical pinning because horizontal letterbox use cases are rarer in
/// social-media workflows; the CLI exposes all nine for completeness.
const EXTENT_GRAVITIES: &[(&str, Gravity)] =
    &[("Centro", Gravity::Center), ("Topo", Gravity::North), ("Base", Gravity::South)];

/// Run the "Letterbox" modal. Returns GTK's exit code.
pub fn run_extent_dialog(files: Vec<PathBuf>) -> i32 {
    let app = adw::Application::builder().application_id(APP_ID).build();
    let files = Rc::new(files);
    app.connect_activate(move |app| {
        build_extent_dialog(app, files.clone()).present();
    });
    let code = app.run_with_args::<&str>(&[]);
    i32::from(u8::from(code))
}

fn build_extent_dialog(app: &adw::Application, files: Rc<Vec<PathBuf>>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(900)
        .default_height(620)
        .title(gettext("Letterbox — Prisma"))
        .build();
    prepare_dialog(&window, files, fill_extent_dialog);
    window
}

fn fill_extent_dialog(
    window: adw::ApplicationWindow,
    files: Rc<Vec<PathBuf>>,
    preview_session: Option<Rc<PreviewSession>>,
) -> adw::ApplicationWindow {
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Letterbox", "Prisma · sem corte")));

    let content_h = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(16)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();

    let natural = preview_session.as_ref().map(|s| s.natural_size()).unwrap_or((1, 1));

    let form_col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .width_request(380)
        .build();

    let intro = gtk::Label::builder()
        .label(gettext(
            "Estende a tela em volta da imagem para encaixá-la num formato \
             fixo (post de YouTube, feed do Instagram, Stories…) sem cortar \
             nenhum pixel. A imagem original é centralizada e o resto é \
             preenchido com a cor escolhida.",
        ))
        .wrap(true)
        .xalign(0.0)
        .build();
    intro.add_css_class("dim-label");

    let files_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(format!("{} arquivo(s) selecionado(s)", files.len()))
        .build();
    files_row.set_subtitle(&files_subtitle(&files));

    let natural_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Tamanho original"))
        .subtitle(format!("{} × {} px (primeiro arquivo)", natural.0, natural.1))
        .build();

    // Aspect preset combo.
    let ratio_names: Vec<&str> = EXTENT_RATIO_PRESETS.iter().map(|(n, _)| *n).collect();
    let ratio_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Proporção alvo"))
        .model(&gtk::StringList::new(&ratio_names))
        .build();

    // Custom W:H entries — visible only when the "Personalizado…" preset is
    // selected. Defaults to 1:1 so switching back and forth feels stable.
    let custom_w_spin = adw::SpinRow::with_range(1.0, 99.0, 1.0);
    custom_w_spin.set_title(&gettext("Razão W"));
    custom_w_spin.set_value(1.0);
    let custom_h_spin = adw::SpinRow::with_range(1.0, 99.0, 1.0);
    custom_h_spin.set_title(&gettext("Razão H"));
    custom_h_spin.set_value(1.0);
    custom_w_spin.set_visible(false);
    custom_h_spin.set_visible(false);

    // Background colour combo + hex entry (entry shown when "Personalizada"
    // is picked).
    let bg_names: Vec<&str> = EXTENT_BG_PRESETS.iter().map(|(n, _)| *n).collect();
    let bg_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Cor de fundo"))
        .model(&gtk::StringList::new(&bg_names))
        .build();

    let bg_hex = adw::EntryRow::builder()
        .use_markup(false)
        .title(gettext("Hex (#RRGGBB)"))
        .text("#0d0e12")
        .build();
    bg_hex.set_visible(false);

    let gravity_names: Vec<&str> = EXTENT_GRAVITIES.iter().map(|(n, _)| *n).collect();
    let gravity_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Posição da imagem"))
        .model(&gtk::StringList::new(&gravity_names))
        .build();

    let predicted_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Destino previsto"))
        .subtitle("—")
        .build();

    let mut target_names = vec!["Manter o formato original"];
    target_names.extend(CONVERT_FORMATS.iter().map(|(n, _)| *n));
    let target_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Formato de destino"))
        .model(&gtk::StringList::new(&target_names))
        .build();

    let overwrite_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Se o arquivo de saída existir"))
        .model(&gtk::StringList::new(
            &OVERWRITE_CHOICES.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        ))
        .build();

    let group = adw::PreferencesGroup::new();
    group.add(&files_row);
    group.add(&natural_row);
    group.add(&ratio_row);
    group.add(&custom_w_spin);
    group.add(&custom_h_spin);
    group.add(&bg_row);
    group.add(&bg_hex);
    group.add(&gravity_row);
    group.add(&predicted_row);
    group.add(&target_row);
    group.add(&overwrite_row);

    form_col.append(&intro);
    form_col.append(&group);

    let status = gtk::Label::builder().label("").wrap(true).xalign(0.0).build();
    status.add_css_class("dim-label");
    form_col.append(&status);

    let cancel_btn = gtk::Button::with_label("Cancelar");
    let apply_btn = gtk::Button::builder().label(gettext("Estender")).build();
    apply_btn.add_css_class("suggested-action");
    apply_btn.add_css_class("pill");

    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    actions.append(&cancel_btn);
    actions.append(&apply_btn);
    form_col.append(&actions);

    content_h.append(&form_col);

    // Live preview on the right, driven by PreviewOp::Extent.
    let picture_opt = if let Some(sess) = preview_session.as_ref() {
        let (picture, column) = build_preview_column(sess);
        content_h.append(&column);
        Some(picture)
    } else {
        None
    };

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&content_h));
    window.set_content(Some(&toolbar_view));

    // Shared closure that reads every control and produces an ExtentOptions
    // (or None if the form is in an invalid state — e.g. malformed hex).
    let build_opts = {
        let ratio_row = ratio_row.clone();
        let custom_w_spin = custom_w_spin.clone();
        let custom_h_spin = custom_h_spin.clone();
        let bg_row = bg_row.clone();
        let bg_hex = bg_hex.clone();
        let gravity_row = gravity_row.clone();
        move || -> std::result::Result<ExtentOptions, String> {
            let (rw, rh) = {
                let idx = ratio_row.selected() as usize;
                let preset = EXTENT_RATIO_PRESETS[idx].1;
                if preset == (0, 0) {
                    (custom_w_spin.value() as u32, custom_h_spin.value() as u32)
                } else {
                    preset
                }
            };
            let size = ExtentSize::Aspect { w: rw, h: rh };

            let background = {
                let idx = bg_row.selected() as usize;
                if idx == EXTENT_BG_PRESETS.len() - 1 {
                    bigimage_core::parse_hex_color(&bg_hex.text())?
                } else {
                    EXTENT_BG_PRESETS[idx].1
                }
            };

            let gravity = EXTENT_GRAVITIES[gravity_row.selected() as usize].1;

            Ok(ExtentOptions { size, background, gravity })
        }
    };

    // Refresh the predicted-size row and the preview painting on every
    // control change. Errors (bad hex) surface in the status label.
    let refresh = {
        let session_opt = preview_session.clone();
        let picture_opt = picture_opt.clone();
        let predicted_row = predicted_row.clone();
        let status = status.clone();
        let build_opts = build_opts.clone();
        move || match build_opts() {
            Ok(opts) => {
                let (nw, nh) = match opts.size.resolve(natural.0, natural.1) {
                    Ok(d) => d,
                    Err(e) => {
                        status.set_text(&e.to_string());
                        predicted_row.set_subtitle("—");
                        return;
                    }
                };
                predicted_row.set_subtitle(&format!("{nw} × {nh} px"));
                status.set_text("");
                if let (Some(sess), Some(picture)) = (session_opt.as_ref(), picture_opt.as_ref()) {
                    refresh_preview(sess, picture, &PreviewOp::Extent(opts));
                }
            }
            Err(e) => {
                status.set_text(&e);
            }
        }
    };

    // Reveal Custom W:H spins when the "Personalizado…" preset is picked,
    // and hide them again on every other selection.
    {
        let custom_w_spin = custom_w_spin.clone();
        let custom_h_spin = custom_h_spin.clone();
        let refresh = refresh.clone();
        dialog_signal!(window, ratio_row, connect_selected_notify, move |row| {
            let idx = row.selected() as usize;
            let is_custom = EXTENT_RATIO_PRESETS[idx].1 == (0, 0);
            custom_w_spin.set_visible(is_custom);
            custom_h_spin.set_visible(is_custom);
            refresh();
        });
    }

    // Reveal the hex entry when "Personalizada" is selected.
    {
        let bg_hex = bg_hex.clone();
        let refresh = refresh.clone();
        dialog_signal!(window, bg_row, connect_selected_notify, move |row| {
            let idx = row.selected() as usize;
            bg_hex.set_visible(idx == EXTENT_BG_PRESETS.len() - 1);
            refresh();
        });
    }

    {
        let refresh = refresh.clone();
        dialog_signal!(window, custom_w_spin, connect_value_notify, move |_| refresh());
    }
    {
        let refresh = refresh.clone();
        dialog_signal!(window, custom_h_spin, connect_value_notify, move |_| refresh());
    }
    {
        let refresh = refresh.clone();
        dialog_signal!(window, bg_hex, connect_changed, move |_| refresh());
    }
    {
        let refresh = refresh.clone();
        dialog_signal!(window, gravity_row, connect_selected_notify, move |_| refresh());
    }

    // Initial paint.
    refresh();

    {
        let window = window.clone();
        dialog_signal!(window, cancel_btn, connect_clicked, move |_| window.close());
    }

    {
        let files = files.clone();
        let build_opts = build_opts.clone();
        let target_row = target_row.clone();
        let overwrite_row = overwrite_row.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let opts = match build_opts() {
                Ok(opts) => opts,
                Err(e) => {
                    status.set_text(&e);
                    return;
                }
            };
            let target: Option<Format> = match target_row.selected() {
                0 => None,
                n => Some(CONVERT_FORMATS[(n as usize) - 1].1),
            };
            let policy = OVERWRITE_CHOICES[overwrite_row.selected() as usize].1;

            apply_btn.set_sensitive(false);
            cancel_btn.set_sensitive(false);
            status.set_text(&gettext("Estendendo…"));

            let work_files: Vec<PathBuf> = (*files).clone();
            run_batch_async(
                work_files,
                move |f| extent_file(f, opts, target, policy),
                gettext("Estendendo"),
                status.clone(),
                apply_btn.clone(),
                cancel_btn.clone(),
                window.clone(),
            );
        });
    }

    window
}

/// Upscale factors we expose in the dialog — matches the CLI `--factor` range
/// (2..=4). Real-ESRGAN backend, when available, slots in without changing
/// this list.
const UPSCALE_FACTORS: &[(&str, u8)] = &[("2× (200%)", 2), ("3× (300%)", 3), ("4× (400%)", 4)];

/// Upscale modal: pick an integer factor and run Lanczos3 resize.
pub fn run_upscale_dialog(files: Vec<PathBuf>) -> i32 {
    let app = adw::Application::builder().application_id(APP_ID).build();
    let files = Rc::new(files);
    app.connect_activate(move |app| {
        build_upscale_dialog(app, files.clone()).present();
    });
    let code = app.run_with_args::<&str>(&[]);
    i32::from(u8::from(code))
}

fn build_upscale_dialog(app: &adw::Application, files: Rc<Vec<PathBuf>>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(820)
        .default_height(500)
        .title(gettext("Aumentar resolução — Prisma"))
        .build();
    prepare_dialog(&window, files, fill_upscale_dialog);
    window
}

fn fill_upscale_dialog(
    window: adw::ApplicationWindow,
    files: Rc<Vec<PathBuf>>,
    preview_session: Option<Rc<PreviewSession>>,
) -> adw::ApplicationWindow {
    let header = adw::HeaderBar::new();
    header
        .set_title_widget(Some(&adw::WindowTitle::new("Aumentar resolução", "Prisma · Lanczos3")));

    let content_h = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(16)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();

    let natural = preview_session.as_ref().map(|s| s.natural_size()).unwrap_or((0, 0));

    let form_col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .width_request(340)
        .build();

    // Hook — shown when an IA backend lands. Today just an informative note.
    let engine_note = gtk::Label::builder()
        .label(
            "Motor: Lanczos3 (CPU). O modo IA (Real-ESRGAN) será habilitado \
             automaticamente quando um modelo ONNX FOSS estiver disponível.",
        )
        .wrap(true)
        .xalign(0.0)
        .build();
    engine_note.add_css_class("dim-label");

    let files_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(format!("{} arquivo(s) selecionado(s)", files.len()))
        .build();
    files_row.set_subtitle(&files_subtitle(&files));

    let natural_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Tamanho original"))
        .subtitle(if natural == (0, 0) {
            "—".to_string()
        } else {
            format!("{} × {} px (primeiro arquivo)", natural.0, natural.1)
        })
        .build();

    let factor_names: Vec<&str> = UPSCALE_FACTORS.iter().map(|(n, _)| *n).collect();
    let factor_row = adw::ComboRow::builder()
        .use_markup(false)
        .title("Fator")
        .model(&gtk::StringList::new(&factor_names))
        .build();

    let predicted_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Destino previsto"))
        .subtitle("—")
        .build();
    {
        let factor_row_c = factor_row.clone();
        let predicted_row_c = predicted_row.clone();
        let refresh_prediction = move || {
            let f = UPSCALE_FACTORS[factor_row_c.selected() as usize].1;
            let sub = if natural == (0, 0) {
                format!("× {f} (tamanho indisponível)")
            } else {
                let w = (natural.0 as u64) * (f as u64);
                let h = (natural.1 as u64) * (f as u64);
                format!("≈ {w} × {h} px")
            };
            predicted_row_c.set_subtitle(&sub);
        };
        refresh_prediction();
        dialog_signal!(window, factor_row, connect_selected_notify, move |_| refresh_prediction());
    }

    let mut target_names = vec!["Manter o formato original"];
    target_names.extend(CONVERT_FORMATS.iter().map(|(n, _)| *n));
    let target_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Formato de destino"))
        .model(&gtk::StringList::new(&target_names))
        .build();

    let overwrite_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Se o arquivo de saída existir"))
        .model(&gtk::StringList::new(
            &OVERWRITE_CHOICES.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        ))
        .build();

    let group = adw::PreferencesGroup::new();
    group.add(&files_row);
    group.add(&natural_row);
    group.add(&factor_row);
    group.add(&predicted_row);
    group.add(&target_row);
    group.add(&overwrite_row);

    form_col.append(&engine_note);
    form_col.append(&group);

    let status = gtk::Label::builder().label("").wrap(true).xalign(0.0).build();
    status.add_css_class("dim-label");
    form_col.append(&status);

    let cancel_btn = gtk::Button::with_label("Cancelar");
    let apply_btn = gtk::Button::builder().label("Aumentar").build();
    apply_btn.add_css_class("suggested-action");
    apply_btn.add_css_class("pill");

    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    actions.append(&cancel_btn);
    actions.append(&apply_btn);
    form_col.append(&actions);

    content_h.append(&form_col);

    // Preview column shows an upscaled thumbnail of the first file as a
    // sanity check — PreviewOp::Resize respects the Lanczos3 filter.
    if let Some(sess) = preview_session.as_ref() {
        let (picture, column) = build_preview_column(sess);
        content_h.append(&column);

        let refresh = {
            let session = sess.clone();
            let picture = picture.clone();
            let factor_row = factor_row.clone();
            move || {
                let f = UPSCALE_FACTORS[factor_row.selected() as usize].1;
                refresh_preview(
                    &session,
                    &picture,
                    &PreviewOp::Resize {
                        mode: ResizeMode::Percent(f32::from(f) * 100.0),
                        filter: Filter::Lanczos3,
                    },
                );
            }
        };
        refresh();
        dialog_signal!(window, factor_row, connect_selected_notify, move |_| refresh());
    }

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&content_h));
    window.set_content(Some(&toolbar_view));

    {
        let window = window.clone();
        dialog_signal!(window, cancel_btn, connect_clicked, move |_| window.close());
    }

    {
        let files = files.clone();
        let factor_row = factor_row.clone();
        let target_row = target_row.clone();
        let overwrite_row = overwrite_row.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let factor = UPSCALE_FACTORS[factor_row.selected() as usize].1;
            let target: Option<Format> = match target_row.selected() {
                0 => None,
                n => Some(CONVERT_FORMATS[(n as usize) - 1].1),
            };
            let policy = OVERWRITE_CHOICES[overwrite_row.selected() as usize].1;

            apply_btn.set_sensitive(false);
            cancel_btn.set_sensitive(false);
            status.set_text(&gettext("Aumentando…"));

            // Upscale = Lanczos3 resize at factor * 100 %.
            let work_files: Vec<PathBuf> = (*files).clone();
            let mode = ResizeMode::Percent(f32::from(factor) * 100.0);
            let opts = EncodeOptions::default();
            run_batch_async(
                work_files,
                move |f| resize_file(f, mode, Filter::Lanczos3, target, &opts, policy),
                gettext("Aumentando"),
                status.clone(),
                apply_btn.clone(),
                cancel_btn.clone(),
                window.clone(),
            );
        });
    }

    window
}

/// Flip modal: horizontal or vertical mirror.
pub fn run_flip_dialog(files: Vec<PathBuf>) -> i32 {
    let app = adw::Application::builder().application_id(APP_ID).build();
    let files = Rc::new(files);
    app.connect_activate(move |app| {
        build_flip_dialog(app, files.clone()).present();
    });
    let code = app.run_with_args::<&str>(&[]);
    i32::from(u8::from(code))
}

fn build_flip_dialog(app: &adw::Application, files: Rc<Vec<PathBuf>>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(440)
        .default_height(340)
        .title(gettext("Espelhar imagens — Prisma"))
        .build();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Espelhar", "Prisma")));

    let axes = ["Horizontal (esquerda ↔ direita)", "Vertical (cima ↔ baixo)"];
    let axis_row = adw::ComboRow::builder()
        .use_markup(false)
        .title("Eixo")
        .model(&gtk::StringList::new(&axes))
        .build();

    let mut target_names = vec!["Manter o formato original"];
    target_names.extend(CONVERT_FORMATS.iter().map(|(n, _)| *n));
    let target_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Formato de destino"))
        .model(&gtk::StringList::new(&target_names))
        .build();

    let overwrite_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Se o arquivo de saída existir"))
        .model(&gtk::StringList::new(
            &OVERWRITE_CHOICES.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        ))
        .build();

    let files_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(format!("{} arquivo(s) selecionado(s)", files.len()))
        .build();
    files_row.set_subtitle(&files_subtitle(&files));

    let group = adw::PreferencesGroup::new();
    group.add(&files_row);
    group.add(&axis_row);
    group.add(&target_row);
    group.add(&overwrite_row);

    let status = gtk::Label::builder().label("").wrap(true).xalign(0.0).build();
    status.add_css_class("dim-label");

    let cancel_btn = gtk::Button::with_label("Cancelar");
    let apply_btn = gtk::Button::builder().label("Espelhar").build();
    apply_btn.add_css_class("suggested-action");
    apply_btn.add_css_class("pill");

    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    actions.append(&cancel_btn);
    actions.append(&apply_btn);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    content.append(&group);
    content.append(&status);
    content.append(&actions);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&content));
    window.set_content(Some(&toolbar_view));

    {
        let window = window.clone();
        dialog_signal!(window, cancel_btn, connect_clicked, move |_| window.close());
    }

    {
        let files = files.clone();
        let axis_row = axis_row.clone();
        let target_row = target_row.clone();
        let overwrite_row = overwrite_row.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let axis = match axis_row.selected() {
                0 => FlipAxis::Horizontal,
                _ => FlipAxis::Vertical,
            };
            let target: Option<Format> = match target_row.selected() {
                0 => None,
                n => Some(CONVERT_FORMATS[(n as usize) - 1].1),
            };
            let policy = OVERWRITE_CHOICES[overwrite_row.selected() as usize].1;

            apply_btn.set_sensitive(false);
            cancel_btn.set_sensitive(false);
            status.set_text(&gettext("Espelhando…"));

            let work_files: Vec<PathBuf> = (*files).clone();
            run_batch_async(
                work_files,
                move |f| flip_file(f, axis, target, policy),
                gettext("Espelhando"),
                status.clone(),
                apply_btn.clone(),
                cancel_btn.clone(),
                window.clone(),
            );
        });
    }

    window
}

/// Adjust modal: brightness + contrast + saturation + gamma.
pub fn run_adjust_dialog(files: Vec<PathBuf>) -> i32 {
    let app = adw::Application::builder().application_id(APP_ID).build();
    let files = Rc::new(files);
    app.connect_activate(move |app| {
        build_adjust_dialog(app, files.clone()).present();
    });
    let code = app.run_with_args::<&str>(&[]);
    i32::from(u8::from(code))
}

fn build_adjust_dialog(app: &adw::Application, files: Rc<Vec<PathBuf>>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(880)
        .default_height(640)
        .title(gettext("Ajustar cores — Prisma"))
        .build();
    prepare_dialog(&window, files, fill_adjust_dialog);
    window
}

fn fill_adjust_dialog(
    window: adw::ApplicationWindow,
    files: Rc<Vec<PathBuf>>,
    preview_session: Option<Rc<PreviewSession>>,
) -> adw::ApplicationWindow {
    // Preview session — loaded from the first file. Multi-select still works
    // (Aplicar opera em todos), mas o preview mostra o primeiro.

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Ajustar cores", "Prisma")));

    let brightness = adw::SpinRow::with_range(-100.0, 100.0, 1.0);
    brightness.set_title(&gettext("Brilho"));
    brightness.set_subtitle("-100 escuro · 0 neutro · +100 claro");
    brightness.set_value(0.0);

    let contrast = adw::SpinRow::with_range(-100.0, 100.0, 1.0);
    contrast.set_title(&gettext("Contraste"));
    contrast.set_subtitle("-100 plano · 0 neutro · +100 agressivo");
    contrast.set_value(0.0);

    let saturation = adw::SpinRow::with_range(-100.0, 100.0, 1.0);
    saturation.set_title(&gettext("Saturação"));
    saturation.set_subtitle("-100 cinza · 0 neutro · +100 vibrante");
    saturation.set_value(0.0);

    let gamma = adw::SpinRow::with_range(0.1, 10.0, 0.05);
    gamma.set_title(&gettext("Gamma"));
    gamma.set_subtitle("Abaixo de 1 clareia os meios-tons · 1 mantém · acima de 1 escurece");
    gamma.set_digits(2);
    gamma.set_value(1.0);

    let mut target_names = vec!["Manter o formato original"];
    target_names.extend(CONVERT_FORMATS.iter().map(|(n, _)| *n));
    let target_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Formato de destino"))
        .model(&gtk::StringList::new(&target_names))
        .build();

    let overwrite_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Se o arquivo de saída existir"))
        .model(&gtk::StringList::new(
            &OVERWRITE_CHOICES.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        ))
        .build();

    let files_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(format!("{} arquivo(s) selecionado(s)", files.len()))
        .build();
    files_row.set_subtitle(&files_subtitle(&files));

    let group_files = adw::PreferencesGroup::new();
    group_files.add(&files_row);

    let group_tone = adw::PreferencesGroup::builder().title(gettext("Tom")).build();
    group_tone.add(&brightness);
    group_tone.add(&contrast);
    group_tone.add(&gamma);

    let group_color = adw::PreferencesGroup::builder().title(gettext("Cor")).build();
    group_color.add(&saturation);

    let group_out = adw::PreferencesGroup::builder().title(gettext("Saída")).build();
    group_out.add(&target_row);
    group_out.add(&overwrite_row);

    let status = gtk::Label::builder().label("").wrap(true).xalign(0.0).build();
    status.add_css_class("dim-label");

    let cancel_btn = gtk::Button::with_label("Cancelar");
    let apply_btn = gtk::Button::builder().label("Aplicar").build();
    apply_btn.add_css_class("suggested-action");
    apply_btn.add_css_class("pill");

    let actions_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    actions_box.append(&cancel_btn);
    actions_box.append(&apply_btn);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    content.append(&group_files);
    content.append(&group_tone);
    content.append(&group_color);
    content.append(&group_out);
    content.append(&status);
    content.append(&actions_box);

    let scrolled = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&content)
        .build();
    scrolled.set_size_request(420, -1);

    // Two-column layout: scrollable form on the left, live preview column
    // on the right (when a session is available).
    let main_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(16)
        .margin_top(0)
        .margin_bottom(0)
        .build();
    main_box.append(&scrolled);

    let preview_picture: Option<gtk::Picture> = if let Some(sess) = preview_session.as_ref() {
        let (picture, column) = build_preview_column(sess);
        column.set_margin_top(16);
        column.set_margin_bottom(16);
        column.set_margin_end(16);
        main_box.append(&column);
        Some(picture)
    } else {
        None
    };

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&main_box));
    window.set_content(Some(&toolbar_view));

    // Wire live preview on every slider / gamma change.
    if let (Some(picture), Some(session)) = (preview_picture.as_ref(), preview_session.as_ref()) {
        let debounce = PreviewDebounce::new();
        let sliders: [adw::SpinRow; 4] =
            [brightness.clone(), contrast.clone(), saturation.clone(), gamma.clone()];
        for row in &sliders {
            let debounce = debounce.clone();
            let session = session.clone();
            let picture = picture.clone();
            let brightness = brightness.clone();
            let contrast = contrast.clone();
            let saturation = saturation.clone();
            let gamma = gamma.clone();
            dialog_signal!(window, row, connect_changed, move |_| {
                let ops = AdjustOps {
                    brightness: brightness.value() as i32,
                    contrast: contrast.value() as f32,
                    saturation: saturation.value() as f32,
                    gamma: gamma.value() as f32,
                };
                let session = session.clone();
                let picture = picture.clone();
                debounce.trigger(Duration::from_millis(80), move || {
                    refresh_preview(&session, &picture, &PreviewOp::Adjust(ops));
                });
            });
        }
    }

    {
        let window = window.clone();
        dialog_signal!(window, cancel_btn, connect_clicked, move |_| window.close());
    }

    {
        let files = files.clone();
        let brightness = brightness.clone();
        let contrast = contrast.clone();
        let saturation = saturation.clone();
        let gamma = gamma.clone();
        let target_row = target_row.clone();
        let overwrite_row = overwrite_row.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let ops = AdjustOps {
                brightness: brightness.value() as i32,
                contrast: contrast.value() as f32,
                saturation: saturation.value() as f32,
                gamma: gamma.value() as f32,
            };
            let target: Option<Format> = match target_row.selected() {
                0 => None,
                n => Some(CONVERT_FORMATS[(n as usize) - 1].1),
            };
            let policy = OVERWRITE_CHOICES[overwrite_row.selected() as usize].1;

            apply_btn.set_sensitive(false);
            cancel_btn.set_sensitive(false);
            status.set_text(&gettext("Ajustando…"));

            let work_files: Vec<PathBuf> = (*files).clone();
            run_batch_async(
                work_files,
                move |f| adjust_file(f, ops, target, policy),
                gettext("Ajustando"),
                status.clone(),
                apply_btn.clone(),
                cancel_btn.clone(),
                window.clone(),
            );
        });
    }

    window
}

// ---------------------------------------------------------------------------
// Metadata viewer dialog — read-only. Loads EXIF via `bigimage_core::metadata`
// and shows every tag as an AdwActionRow so users can scan + copy values.
// ---------------------------------------------------------------------------

/// Run the metadata viewer for `files[0]`. Multi-select still opens but
/// only the first file is inspected — a "próximo" navegator fica para M3.
pub fn run_metadata_dialog(files: Vec<PathBuf>) -> i32 {
    let app = adw::Application::builder().application_id(APP_ID).build();
    let files = Rc::new(files);
    app.connect_activate(move |app| {
        let Some(first) = files.first() else { return };
        build_properties_window(app, first).present();
    });
    let code = app.run_with_args::<&str>(&[]);
    i32::from(u8::from(code))
}

/// Janela de propriedades da imagem. Sempre traz as infos básicas do
/// arquivo (nome, pasta, tamanho, modificado, dimensões) mesmo sem EXIF —
/// é o que o usuário enxerga quando clica no "i". Se houver EXIF, uma
/// seção extra aparece abaixo com os campos crus.
fn build_properties_window(app: &adw::Application, path: &Path) -> adw::ApplicationWindow {
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(520)
        .default_height(640)
        .title(format!("Propriedades — {filename}"))
        .build();
    let input = path.to_path_buf();
    let cancel = Arc::new(AtomicBool::new(false));
    let closing = cancel.clone();
    window.connect_unrealize(move |_| closing.store(true, Ordering::Release));
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = crate::codec::with_cancel(cancel, || {
            let info = crate::codec::information(&input).ok();
            let fsmeta = std::fs::metadata(&input).map_err(|e| e.to_string());
            (info, fsmeta)
        });
        let _ = tx.send(result);
    });
    let spinner = gtk::Spinner::new();
    spinner.start();
    window.set_content(Some(&spinner));
    let weak = window.downgrade();
    let input = path.to_path_buf();
    glib::timeout_add_local(Duration::from_millis(16), move || {
        let Some(window) = weak.upgrade() else { return glib::ControlFlow::Break };
        if !window.is_visible() {
            return glib::ControlFlow::Break;
        }
        match rx.try_recv() {
            Ok((info, fsmeta)) => {
                fill_properties_window(window, &input, info.as_ref(), fsmeta.as_ref());
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
        glib::ControlFlow::Break
    });
    window
}
fn fill_properties_window(
    window: adw::ApplicationWindow,
    path: &Path,
    info: Option<&crate::codec::Information>,
    fsmeta: std::result::Result<&std::fs::Metadata, &String>,
) -> adw::ApplicationWindow {
    let filename = path.file_name().unwrap_or_default().to_string_lossy();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Propriedades", &filename)));

    let page = adw::PreferencesPage::new();
    page.add(&build_properties_file_group(path, info, fsmeta));
    page.add(&build_properties_image_group(info));

    if let Some(meta) = info.and_then(|i| i.metadata.as_ref()) {
        if !meta.is_empty() {
            if meta.has_gps || meta.has_camera_info {
                let warning =
                    adw::PreferencesGroup::builder().title(gettext("Privacidade")).build();
                let row = adw::ActionRow::builder()
                    .use_markup(false)
                    .title(if meta.has_gps {
                        "Coordenadas GPS presentes"
                    } else {
                        "Identificação de câmera/software presente"
                    })
                    .subtitle("Remova antes de publicar se for sensível.")
                    .build();
                warning.add(&row);
                page.add(&warning);
            }
            page.add(&build_properties_exif_group(meta));
        }
    }

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&page));
    window.set_content(Some(&toolbar_view));

    window
}

fn build_properties_file_group(
    path: &Path,
    info: Option<&crate::codec::Information>,
    fsmeta: std::result::Result<&std::fs::Metadata, &String>,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title(gettext("Arquivo")).build();

    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    group.add(&info_row("Nome", &filename));

    if let Some(parent) = path.parent() {
        let parent_str = parent.display().to_string();
        if !parent_str.is_empty() {
            group.add(&info_row("Pasta", &parent_str));
        }
    }

    // Sniff format by magic bytes — `path.extension()` lies for partial-sync
    // files (Nextcloud `.chunking-*`, browser `.part`/`.crdownload`, `.tmp`)
    // even when the bytes are a valid PNG/JPEG/etc. Falls back to extension
    // when sniffing fails (truly unknown / corrupt / non-image file).
    let ext = path.extension().map(|e| e.to_string_lossy().to_uppercase());
    let sniffed = info.and_then(|i| i.format.clone());
    match (sniffed, ext) {
        (Some(s), Some(e)) if s != e => {
            group.add(&info_row("Formato", &format!("{s} (extensão: {e})")));
        }
        (Some(s), _) => group.add(&info_row("Formato", &s)),
        (None, Some(e)) => group.add(&info_row("Formato", &e)),
        (None, None) => {}
    }

    match fsmeta {
        Ok(fsmeta) => {
            group.add(&info_row("Tamanho", &format_bytes(fsmeta.len())));
            if let Ok(mtime) = fsmeta.modified() {
                group.add(&info_row("Modificado", &format_system_time(mtime)));
            }
        }
        Err(e) => {
            group.add(&info_row("Tamanho", &format!("(indisponível: {e})")));
        }
    }

    group
}

fn build_properties_image_group(info: Option<&crate::codec::Information>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title(gettext("Imagem")).build();

    match info.and_then(|i| i.dimensions) {
        Some((w, h)) => {
            let color = info.and_then(|i| i.color).and_then(|id| match id {
                1 => Some(image::ColorType::L8),
                2 => Some(image::ColorType::La8),
                3 => Some(image::ColorType::Rgb8),
                4 => Some(image::ColorType::Rgba8),
                5 => Some(image::ColorType::L16),
                6 => Some(image::ColorType::La16),
                7 => Some(image::ColorType::Rgb16),
                8 => Some(image::ColorType::Rgba16),
                9 => Some(image::ColorType::Rgb32F),
                10 => Some(image::ColorType::Rgba32F),
                _ => None,
            });
            group.add(&info_row("Dimensões", &format!("{w} × {h} px")));

            let mp = (f64::from(w) * f64::from(h)) / 1_000_000.0;
            if mp >= 0.1 {
                group.add(&info_row("Megapixels", &format!("{mp:.1} MP")));
            }
            if w > 0 && h > 0 {
                let (rw, rh) = simplify_ratio(w, h);
                group.add(&info_row("Razão de aspecto", &format!("{rw}:{rh}")));
            }
            if let Some(color) = color {
                group.add(&info_row("Modo de cor", color_type_label(color)));
                group.add(&info_row("Bits por canal", color_type_bits(color)));
            }
        }
        None => {
            group.add(&info_row(
                "Dimensões",
                "(falha: não foi possível identificar o formato pelo conteúdo)",
            ));
        }
    }

    // Orientação EXIF — só aparece se houver tag e != 1 (1 = upright).
    if let Some(meta) = info.and_then(|i| i.metadata.as_ref()) {
        if let Some(o) = meta.orientation {
            if o != 1 {
                group.add(&info_row(
                    "Orientação EXIF",
                    &format!("{o} · use Girar ▸ Automático pra corrigir"),
                ));
            }
        }
    }
    group
}

/// Friendly label for the decoded pixel layout, matching what the Convert
/// dialog says when picking encoders.
fn color_type_label(c: image::ColorType) -> &'static str {
    use image::ColorType::*;
    match c {
        L8 | L16 => "Tons de cinza",
        La8 | La16 => "Tons de cinza + alfa",
        Rgb8 | Rgb16 | Rgb32F => "RGB",
        Rgba8 | Rgba16 | Rgba32F => "RGBA",
        _ => "—",
    }
}

/// Bit depth per channel. 32F is reported as "32 (float)" so users don't
/// confuse high-precision HDR with overkill integer depth.
fn color_type_bits(c: image::ColorType) -> &'static str {
    use image::ColorType::*;
    match c {
        L8 | La8 | Rgb8 | Rgba8 => "8",
        L16 | La16 | Rgb16 | Rgba16 => "16",
        Rgb32F | Rgba32F => "32 (float)",
        _ => "—",
    }
}

/// Reduce W:H to its smallest integer pair via gcd. Used in the Properties
/// dialog so 1920×1080 shows as 16:9 instead of opaque pixel counts.
fn simplify_ratio(w: u32, h: u32) -> (u32, u32) {
    fn gcd(a: u32, b: u32) -> u32 {
        if b == 0 {
            a
        } else {
            gcd(b, a % b)
        }
    }
    let d = gcd(w, h);
    (w / d, h / d)
}

fn build_properties_exif_group(meta: &metadata::Metadata) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("EXIF")
        .description(format!("{} campo(s) presentes", meta.tags.len()))
        .build();
    for (name, value) in &meta.tags {
        group.add(&info_row(name, value));
    }
    group
}

/// ActionRow só-leitura com título+valor. Padrão visual usado no dialog
/// de propriedades pra cada campo.
fn info_row(title: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().use_markup(false).title(title).build();
    row.set_subtitle(value);
    row
}

/// Formata bytes pra uma escala humana (KB/MB/GB em base 1024).
fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut u = 0usize;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {} ({n} bytes)", UNITS[u])
    }
}

/// Formata um SystemTime em `YYYY-MM-DD HH:MM` local. Usa apenas a API
/// padrão (sem pegar `chrono`) — basta pra mostrar a data legível.
fn format_system_time(t: std::time::SystemTime) -> String {
    let secs = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    // glib::DateTime lida com timezone local automaticamente — usamos ela
    // pra evitar reimplementar aritmética de calendário.
    match glib::DateTime::from_unix_local(secs) {
        Ok(dt) => {
            dt.format("%Y-%m-%d %H:%M").map(|g| g.to_string()).unwrap_or_else(|_| format!("{secs}"))
        }
        Err(_) => format!("{secs}"),
    }
}

// ---------------------------------------------------------------------------
// Compare dialog — two images in a `gtk::Paned`, drag the divider to sweep
// between "antes" and "depois". Requires exactly 2 files; the rest are
// ignored (first two in the selection are used).
// ---------------------------------------------------------------------------

/// Open the "Compare two" window. Needs two files — if fewer are given
/// we fall back to the viewer with whatever's there.
pub fn run_compare_dialog(files: Vec<PathBuf>) -> i32 {
    if files.len() < 2 {
        tracing::warn!(count = files.len(), "compare needs two files; falling back to viewer");
        return run_viewer(files);
    }
    let app = adw::Application::builder().application_id(APP_ID).build();
    let files = Rc::new(files);
    app.connect_activate(move |app| {
        build_compare_dialog(app, files.clone()).present();
    });
    let code = app.run_with_args::<&str>(&[]);
    i32::from(u8::from(code))
}

fn build_compare_dialog(app: &adw::Application, files: Rc<Vec<PathBuf>>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(1200)
        .default_height(720)
        .title("Comparar imagens — Íris")
        .build();

    let left_path = &files[0];
    let right_path = &files[1];

    let header = adw::HeaderBar::new();
    let left_name =
        left_path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let right_name =
        right_path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let title = adw::WindowTitle::new(
        "Comparar imagens",
        &format!("Antes: {left_name}  ·  Depois: {right_name}"),
    );
    header.set_title_widget(Some(&title));

    // Captions gone — titlebar now carries the Antes/Depois info so the
    // full vertical space goes to the pictures.
    let left_picture = make_compare_side(left_path, &window);
    let right_picture = make_compare_side(right_path, &window);

    let paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .hexpand(true)
        .vexpand(true)
        .resize_start_child(true)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .start_child(&left_picture)
        .end_child(&right_picture)
        .build();

    let hint = gtk::Label::builder()
        .label("Arraste o divisor para comparar · Esc fecha")
        .xalign(0.5)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    hint.add_css_class("dim-label");

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(0).build();
    content.append(&paned);
    content.append(&hint);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&content));
    window.set_content(Some(&toolbar_view));

    // Position the divider at 50% once the window has real dimensions.
    {
        let paned = paned.clone();
        let window = window.clone();
        glib::idle_add_local_once(move || {
            let w = window.default_width();
            if w > 0 {
                paned.set_position(w / 2);
            }
        });
    }

    // Keyboard: Esc closes.
    let key = gtk::EventControllerKey::new();
    {
        let window = window.downgrade();
        key.connect_key_pressed(move |_, k, _, _| {
            let Some(window) = window.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if matches!(k, gdk::Key::Escape) {
                window.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }
    window.add_controller(key);

    window
}

fn make_compare_side(path: &Path, window: &adw::ApplicationWindow) -> gtk::Box {
    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_hexpand(true);
    picture.set_vexpand(true);

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .hexpand(true)
        .vexpand(true)
        .build();
    column.append(&picture);
    let spinner = gtk::Spinner::new();
    spinner.start();
    column.append(&spinner);
    let cancel = Arc::new(AtomicBool::new(false));
    let closing = cancel.clone();
    window.connect_unrealize(move |_| closing.store(true, Ordering::Release));
    let input = path.to_path_buf();
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        crate::codec::with_cancel(cancel, || {
            let result = bigimage_core::open_image(input)
                .map(|image| {
                    let rgba = image.thumbnail(2048, 2048).to_rgba8();
                    let size = rgba.dimensions();
                    (rgba.into_raw(), size)
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(result);
        })
    });
    let weak_picture = picture.downgrade();
    let weak_column = column.downgrade();
    let weak_spinner = spinner.downgrade();
    let weak_window = window.downgrade();
    glib::timeout_add_local(Duration::from_millis(16), move || {
        if weak_window.upgrade().is_none_or(|w| !w.is_visible()) {
            return glib::ControlFlow::Break;
        }
        match rx.try_recv() {
            Ok(result) => {
                if let (Some(column), Some(spinner)) =
                    (weak_column.upgrade(), weak_spinner.upgrade())
                {
                    column.remove(&spinner);
                }
                match result {
                    Ok((raw, (w, h))) => {
                        if let Some(picture) = weak_picture.upgrade() {
                            picture.set_paintable(Some(&texture_from_rgba(raw, w, h)));
                        }
                    }
                    Err(error) => {
                        if let Some(column) = weak_column.upgrade() {
                            let label = gtk::Label::new(Some(&error));
                            label.set_wrap(true);
                            column.append(&label);
                        }
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => {}
        }
        glib::ControlFlow::Break
    });
    column
}

// ---------------------------------------------------------------------------
// Animate dialog — pick a delay, a loop mode, an output path and build an
// animated GIF from the selected frames. Minimal shape: every parameter
// has a sensible default so the user can just hit "Criar GIF" and get
// something playable.
// ---------------------------------------------------------------------------

/// Prisma Lote — converter em lote. Pega a lista de arquivos recebida via
/// CLI, expande pastas e aplica a conversão no pool paralelo compartilhado.
pub fn run_batch_dialog(files: Vec<PathBuf>) -> i32 {
    let app = adw::Application::builder().application_id(APP_ID).build();
    let files = Rc::new(RefCell::new(expand_batch_inputs(files)));
    app.connect_activate(move |app| {
        build_batch_dialog(app, files.clone()).present();
    });
    let code = app.run_with_args::<&str>(&[]);
    i32::from(u8::from(code))
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

fn build_batch_dialog(
    app: &adw::Application,
    files: Rc<RefCell<Vec<PathBuf>>>,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(640)
        .default_height(640)
        .title("Prisma — Lote")
        .build();

    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("Conversão em lote", "Prisma");
    header.set_title_widget(Some(&title));

    let format_names: Vec<&str> = CONVERT_FORMATS.iter().map(|(n, _)| *n).collect();
    let overwrite_names: Vec<&str> = OVERWRITE_CHOICES.iter().map(|(n, _)| *n).collect();

    // ── Grupo: arquivos ─────────────────────────────────────────────────
    // Lista mutável (compartilhada via Rc<RefCell>) — suporta adicionar
    // arquivos e pastas depois do diálogo aberto. `summary_row` é
    // re-renderizada sempre que mudar a lista, via `refresh_files_row`.
    let files_group = adw::PreferencesGroup::builder().title("Arquivos").build();
    let summary_row = adw::ActionRow::builder().use_markup(false).activatable(false).build();
    let add_files_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Adicionar arquivos")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    let add_folder_btn = gtk::Button::builder()
        .icon_name("folder-symbolic")
        .tooltip_text("Adicionar pasta (imagens)")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    summary_row.add_suffix(&add_files_btn);
    summary_row.add_suffix(&add_folder_btn);
    files_group.add(&summary_row);
    refresh_files_row(&summary_row, &files.borrow());

    // ── Grupo: conversão ────────────────────────────────────────────────
    let convert_group = adw::PreferencesGroup::builder().title("Conversão").build();

    let format_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Formato de destino"))
        .model(&gtk::StringList::new(&format_names))
        .build();
    convert_group.add(&format_row);

    let quality_spin = adw::SpinRow::with_range(1.0, 100.0, 1.0);
    quality_spin.set_title(&gettext("Qualidade (JPEG/AVIF)"));
    quality_spin.set_subtitle("1 menor qualidade · 85 equilíbrio · 100 maior qualidade");
    quality_spin.set_value(85.0);
    convert_group.add(&quality_spin);

    let optimize_switch = adw::SwitchRow::builder()
        .use_markup(false)
        .title(gettext("Otimizar tamanho"))
        .subtitle("PNG com compressão máxima; AVIF mais lento; arquivo menor")
        .build();
    convert_group.add(&optimize_switch);
    bind_encode_controls(&window, &format_row, &quality_spin, &optimize_switch);

    // ── Grupo: destino ──────────────────────────────────────────────────
    let dest_group = adw::PreferencesGroup::builder().title("Destino").build();
    let overwrite_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Se o arquivo de saída existir"))
        .model(&gtk::StringList::new(&overwrite_names))
        .build();
    dest_group.add(&overwrite_row);
    // Pasta de saída: None = mesma do original. Clicar abre FolderDialog.
    let output_dir: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
    let folder_row = adw::ActionRow::builder()
        .use_markup(false)
        .title("Pasta de saída")
        .subtitle("Mesma pasta do arquivo de origem")
        .activatable(true)
        .build();
    let folder_icon = gtk::Image::from_icon_name("folder-open-symbolic");
    folder_icon.add_css_class("dim-label");
    folder_row.add_suffix(&folder_icon);
    let clear_folder_btn = gtk::Button::builder()
        .icon_name("edit-clear-symbolic")
        .tooltip_text("Usar a mesma pasta do arquivo de origem")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .visible(false)
        .build();
    folder_row.add_suffix(&clear_folder_btn);
    dest_group.add(&folder_row);

    // ── Rodapé: barra de progresso + status + botões ────────────────────
    let progress = gtk::ProgressBar::builder().show_text(true).text("Pronto").build();
    progress.set_margin_start(18);
    progress.set_margin_end(18);
    progress.set_margin_top(8);

    let status = gtk::Label::builder()
        .label("Aguardando. Clique em Processar para iniciar.")
        .wrap(true)
        .max_width_chars(64)
        .css_classes(["dim-label"])
        .margin_start(18)
        .margin_end(18)
        .margin_top(4)
        .margin_bottom(4)
        .build();

    let apply_btn =
        gtk::Button::builder().label("Processar").css_classes(["pill", "suggested-action"]).build();
    let cancel_btn = gtk::Button::builder().label("Cancelar").build();
    let btn_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    btn_bar.set_halign(gtk::Align::Center);
    btn_bar.set_margin_top(8);
    btn_bar.set_margin_bottom(18);
    btn_bar.append(&cancel_btn);
    btn_bar.append(&apply_btn);

    // ── Scroll wrapper (para listas longas no futuro) ───────────────────
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

    // Cancel signal shared with the worker thread — flipped by the
    // Cancel button while a run is in flight, reset on each Apply.
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // True while a worker is running. The Cancel button uses this to
    // decide between "cancel the run" and "close the window".
    let running = Rc::new(Cell::new(false));

    // Handlers dos botões "Adicionar arquivos" e "Adicionar pasta".
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

    // Pasta de saída: clicar na row abre FolderDialog; o botão vassoura
    // reseta para None (mesma pasta do original).
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
            folder_row.set_subtitle("Mesma pasta do arquivo de origem");
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
        let quality_spin = quality_spin.clone();
        let optimize_switch = optimize_switch.clone();
        let progress = progress.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        let cancelled = cancelled.clone();
        let running = running.clone();
        let output_dir = output_dir.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let fmt_idx = format_row.selected() as usize;
            let policy_idx = overwrite_row.selected() as usize;
            let fmt = CONVERT_FORMATS[fmt_idx].1;
            let policy = OVERWRITE_CHOICES[policy_idx].1;
            let opts = EncodeOptions {
                quality: Some(quality_spin.value() as u8),
                optimize: optimize_switch.is_active(),
                ..EncodeOptions::default()
            };
            let out_dir_snapshot = output_dir.borrow().clone();
            let files_owned: Vec<PathBuf> = files.borrow().clone();
            if files_owned.is_empty() {
                status.set_text(&gettext("Adicione ao menos uma imagem para processar."));
                return;
            }

            // Reset cancel for a re-run, mark a worker as live.
            cancelled.store(false, std::sync::atomic::Ordering::Relaxed);
            running.set(true);
            apply_btn.set_sensitive(false);
            cancel_btn.set_label(&gettext("Cancelar"));
            progress.set_fraction(0.0);
            progress.set_text(Some(&gettext("Iniciando…")));
            status.set_text(&gettext("Processando…"));

            let rx = batch_runner::spawn_batch_workers(
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
            glib::timeout_add_local(std::time::Duration::from_millis(50), move || loop {
                match rx.try_recv() {
                    Ok(batch_runner::AsyncBatchEvent::Progress { done, total, file }) => {
                        progress.set_fraction(done as f64 / total.max(1) as f64);
                        progress.set_text(Some(&format!("{done}/{total} · {file}")));
                    }
                    Ok(batch_runner::AsyncBatchEvent::Done {
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
                        let timing = batch_runner::format_duration(elapsed);
                        let mut msg = if cancelled {
                            format!(
                                "Cancelado após {done}/{total} em {timing}. \
                                 {ok} gravado(s), {skip} ignorado(s), {fail} falha(s)."
                            )
                        } else {
                            format!(
                                "Concluído em {timing}. \
                                 {ok} gravado(s), {skip} ignorado(s), {fail} falha(s)."
                            )
                        };
                        if ok > 0 && bytes_in > 0 && bytes_out > 0 {
                            let delta = batch_runner::format_size_delta(bytes_in, bytes_out);
                            let avg_in = bytes_in / (ok as u64).max(1);
                            let avg_out = bytes_out / (ok as u64).max(1);
                            msg.push_str(&format!(
                                "\n{} → {} ({delta}) · média por arquivo {} → {}",
                                batch_runner::format_size(bytes_in),
                                batch_runner::format_size(bytes_out),
                                batch_runner::format_size(avg_in),
                                batch_runner::format_size(avg_out),
                            ));
                        }
                        status.set_text(&msg);
                        if let Some(err) = first_err {
                            status.set_text(&format!("{msg}\n{err}"));
                        }
                        progress.set_text(Some(if cancelled { "Cancelado" } else { "Concluído" }));
                        apply_btn.set_sensitive(true);
                        let cancel_label = if !cancelled && done == total {
                            "Fechar".to_string()
                        } else {
                            gettext("Cancelar")
                        };
                        cancel_btn.set_label(&cancel_label);
                        if !cancelled && fail == 0 {
                            let win = window.clone();
                            glib::timeout_add_seconds_local_once(3, move || win.close());
                        }
                        return glib::ControlFlow::Break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        return glib::ControlFlow::Continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        running.set(false);
                        return glib::ControlFlow::Break;
                    }
                }
            });
        });
    }

    window
}

/// Atualiza a ActionRow de resumo do Prisma Lote com a contagem + peso
/// total da lista atual de arquivos.
fn refresh_files_row(row: &adw::ActionRow, files: &[PathBuf]) {
    let total: u64 = files.iter().filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len())).sum();
    row.set_title(&format!("{} arquivo(s) selecionado(s)", files.len()));
    row.set_subtitle(&format_size(total));
}

/// Abre um picker multi-arquivo e anexa os escolhidos à lista do batch,
/// deduplicando pelas paths canônicas. Formatos filtrados pelos mesmos
/// MIME types que o dashboard aceita.
fn add_files_to_batch(
    window: &adw::ApplicationWindow,
    files: &Rc<RefCell<Vec<PathBuf>>>,
    row: &adw::ActionRow,
) {
    let dialog = gtk::FileDialog::builder().title("Adicionar arquivos").modal(true).build();
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Imagens"));
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

/// Abre um picker de pasta e varre (não-recursivo, um nível só) em busca
/// de arquivos com extensão de imagem conhecida; anexa à lista do batch.
fn add_folder_to_batch(
    window: &adw::ApplicationWindow,
    files: &Rc<RefCell<Vec<PathBuf>>>,
    row: &adw::ActionRow,
) {
    let dialog =
        gtk::FileDialog::builder().title("Adicionar pasta com imagens").modal(true).build();
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

/// Adiciona novos paths à lista, pulando os que já estavam lá
/// (comparação por PathBuf canônica).
fn append_unique(files: &Rc<RefCell<Vec<PathBuf>>>, new_paths: Vec<PathBuf>) {
    let mut list = files.borrow_mut();
    for p in new_paths {
        if !list.iter().any(|existing| existing == &p) {
            list.push(p);
        }
    }
}

/// Extensões que aceitamos numa varredura de pasta. Case-insensitive.
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

/// Abre FolderDialog; ao confirmar, atualiza `output_dir` e o subtitle
/// da row. Revela o botão "limpar" para o usuário voltar ao padrão.
fn pick_output_folder(
    window: &adw::ApplicationWindow,
    output_dir: &Rc<RefCell<Option<PathBuf>>>,
    row: &adw::ActionRow,
    clear_btn: &gtk::Button,
) {
    let dialog = gtk::FileDialog::builder().title("Escolher pasta de saída").modal(true).build();
    let parent = window.clone();
    let output_dir = output_dir.clone();
    let row = row.clone();
    let clear_btn = clear_btn.clone();
    dialog.select_folder(Some(&parent), None::<&gio::Cancellable>, move |result| {
        let Ok(folder) = result else { return };
        let Some(path) = folder.path() else { return };
        row.set_subtitle(&path.display().to_string());
        *output_dir.borrow_mut() = Some(path);
        clear_btn.set_visible(true);
    });
}

/// Humaniza um tamanho em bytes para algo como "2.3 GB" ou "842 KB".
use batch_runner::format_size;

pub fn run_animate_dialog(files: Vec<PathBuf>) -> i32 {
    let app = adw::Application::builder().application_id(APP_ID).build();
    let files = Rc::new(files);
    app.connect_activate(move |app| {
        build_animate_dialog(app, files.clone()).present();
    });
    let code = app.run_with_args::<&str>(&[]);
    i32::from(u8::from(code))
}

fn build_animate_dialog(app: &adw::Application, files: Rc<Vec<PathBuf>>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(520)
        .default_height(520)
        .title("Criar GIF animado — Prisma")
        .build();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Criar GIF animado", "Prisma")));

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();

    if files.len() < 2 {
        let status = adw::StatusPage::builder()
            .icon_name("dialog-warning-symbolic")
            .title("Selecione pelo menos 2 imagens")
            .description("Informe vários arquivos de uma vez para montar a animação.")
            .vexpand(true)
            .build();
        content.append(&status);

        let close = gtk::Button::with_label("Fechar");
        {
            let window = window.clone();
            dialog_signal!(window, close, connect_clicked, move |_| window.close());
        }
        content.append(&close);

        let tv = adw::ToolbarView::new();
        tv.add_top_bar(&header);
        tv.set_content(Some(&content));
        window.set_content(Some(&tv));
        return window;
    }

    // Parametric rows.
    let delay_spin = adw::SpinRow::with_range(10.0, 5000.0, 10.0);
    delay_spin.set_title(&gettext("Atraso por quadro (ms)"));
    delay_spin.set_subtitle("100 ≈ 10 fps · 40 ≈ 25 fps · 1000 = 1 s");
    delay_spin.set_value(100.0);

    let loop_row = adw::ComboRow::builder()
        .use_markup(false)
        .title("Loop")
        .model(&gtk::StringList::new(&[
            "Infinito (padrão)",
            "Uma vez",
            "2 vezes",
            "5 vezes",
            "10 vezes",
        ]))
        .build();

    let speed_spin = adw::SpinRow::with_range(1.0, 30.0, 1.0);
    speed_spin.set_title(&gettext("Velocidade do encoder"));
    speed_spin.set_subtitle("maior = mais rápido, paleta um pouco pior · padrão 10");
    speed_spin.set_value(10.0);

    // Default output: next to the first frame, named "animated.gif".
    let default_output: PathBuf = files[0]
        .parent()
        .map(|p| p.join("animated.gif"))
        .unwrap_or_else(|| PathBuf::from("animated.gif"));
    let output_row =
        adw::EntryRow::builder().use_markup(false).title("Arquivo de saída (.gif)").build();
    output_row.set_text(&default_output.to_string_lossy());

    let files_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(format!("{} quadro(s)", files.len()))
        .build();
    files_row.set_subtitle(&files_subtitle(&files));

    let group = adw::PreferencesGroup::new();
    group.add(&files_row);
    group.add(&delay_spin);
    group.add(&loop_row);
    group.add(&speed_spin);
    group.add(&output_row);
    content.append(&group);

    let status = gtk::Label::builder().label("").wrap(true).xalign(0.0).build();
    status.add_css_class("dim-label");
    content.append(&status);

    let cancel_btn = gtk::Button::with_label("Cancelar");
    let apply_btn = gtk::Button::builder().label("Criar GIF").build();
    apply_btn.add_css_class("suggested-action");
    apply_btn.add_css_class("pill");

    let actions_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    actions_box.append(&cancel_btn);
    actions_box.append(&apply_btn);
    content.append(&actions_box);

    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&content));
    window.set_content(Some(&tv));

    {
        let window = window.clone();
        dialog_signal!(window, cancel_btn, connect_clicked, move |_| window.close());
    }

    {
        let files = files.clone();
        let delay_spin = delay_spin.clone();
        let loop_row = loop_row.clone();
        let speed_spin = speed_spin.clone();
        let output_row = output_row.clone();
        let status = status.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            let loop_mode = match loop_row.selected() {
                0 => LoopMode::Infinite,
                1 => LoopMode::Once,
                2 => LoopMode::Finite(2),
                3 => LoopMode::Finite(5),
                _ => LoopMode::Finite(10),
            };
            let opts = AnimateOptions {
                delay_ms: delay_spin.value() as u32,
                loop_mode,
                speed: speed_spin.value() as i32,
            };
            let output: PathBuf = output_row.text().as_str().into();

            apply_btn.set_sensitive(false);
            cancel_btn.set_sensitive(false);
            status.set_text(&gettext("Montando GIF…"));

            // Worker thread runs make_gif (encoding 100 frames at 4K can
            // take several seconds); main loop polls a oneshot channel
            // for the result. Window stays responsive throughout.
            let (tx, rx) = std::sync::mpsc::channel();
            let files_owned: Vec<PathBuf> = (*files).clone();
            std::thread::spawn(move || {
                let _ = tx.send(make_gif(&files_owned, &output, opts));
            });

            let status = status.clone();
            let window = window.clone();
            let apply_btn = apply_btn.clone();
            let cancel_btn = cancel_btn.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(80), move || {
                match rx.try_recv() {
                    Ok(Ok(path)) => {
                        status.set_text(&format!("Pronto: {}", path.display()));
                        let w = window.clone();
                        glib::timeout_add_seconds_local_once(2, move || w.close());
                        glib::ControlFlow::Break
                    }
                    Ok(Err(e)) => {
                        status.set_text(&format!("Falha: {e}"));
                        apply_btn.set_sensitive(true);
                        cancel_btn.set_sensitive(true);
                        glib::ControlFlow::Break
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
                }
            });
        });
    }

    window
}

/// Output path for `remove_background` — always PNG to preserve alpha.
/// Sibling of the source, stem suffixed with `_nobg`.
fn nobg_output_path(src: &Path) -> PathBuf {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
    let parent = src.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{stem}_nobg.png"))
}

/// Run one remove-bg pass over `src`: decode → BiRefNet → save PNG RGBA.
/// Reuse-friendly remove-bg: takes a pre-loaded [`BgSession`] (~224 MB
/// of weights stay resident across calls) and runs inference + saves
/// the RGBA output. Used by the dialog batch loop so a 50-file batch
/// loads BiRefNet **once**, not 50 times.
fn remove_bg_one_with_session(
    session: &mut bigimage_ai::background::BgSession,
    src: &Path,
) -> Result<PathBuf, String> {
    bigimage_core::content::ensure_static(src).map_err(|e| e.to_string())?;
    let img = bigimage_core::open_image(src).map_err(|e| format!("decode: {e}"))?;
    let out = session.process(&img).map_err(|e| e.to_string())?;
    let dest = nobg_output_path(src);
    bigimage_core::save_image(
        out,
        &dest,
        Format::Png,
        &EncodeOptions::default(),
        OverwritePolicy::Replace,
    )
    .map_err(|e| format!("encode: {e}"))?;
    Ok(dest)
}

/// Worker-thread state for the remove-bg dialog. Shared via `Arc<Mutex>`
/// with a `glib::timeout_add_local` on the UI thread, which polls every
/// 100 ms and reflects the latest snapshot into the ProgressBar + label.
#[derive(Clone, Debug)]
enum BgUiState {
    /// Worker hasn't started yet (apply not clicked).
    Idle,
    /// Downloading the BiRefNet weights. `done`/`total` in bytes.
    Download { done: u64, total: u64 },
    /// Running inference on file `idx` (0-based) of `total`.
    Processing { idx: usize, total: usize, file: String },
    /// Worker finished. `outputs` are per-input written paths (or None
    /// for failed inputs — lines up with the originals by index).
    Done { outputs: Vec<Option<PathBuf>>, first_err: Option<String> },
}

pub fn run_remove_bg_dialog(files: Vec<PathBuf>) -> i32 {
    let app = adw::Application::builder().application_id(APP_ID).build();
    let files = Rc::new(files);
    app.connect_activate(move |app| {
        build_remove_bg_dialog(app, files.clone()).present();
    });
    let code = app.run_with_args::<&str>(&[]);
    i32::from(u8::from(code))
}

fn build_remove_bg_dialog(
    app: &adw::Application,
    files: Rc<Vec<PathBuf>>,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(520)
        .default_height(420)
        .title("Remover fundo — Prisma")
        .build();

    let header = adw::HeaderBar::new();
    header
        .set_title_widget(Some(&adw::WindowTitle::new("Remover fundo", "Prisma · BiRefNet-lite")));

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();

    // Empty input — fail closed with clear message.
    if files.is_empty() {
        let status = adw::StatusPage::builder()
            .icon_name("dialog-warning-symbolic")
            .title("Nenhuma imagem selecionada")
            .description("Informe um ou mais arquivos ao abrir o diálogo.")
            .vexpand(true)
            .build();
        content.append(&status);

        let close = gtk::Button::with_label("Fechar");
        {
            let window = window.clone();
            dialog_signal!(window, close, connect_clicked, move |_| window.close());
        }
        content.append(&close);

        let tv = adw::ToolbarView::new();
        tv.add_top_bar(&header);
        tv.set_content(Some(&content));
        window.set_content(Some(&tv));
        return window;
    }

    // Soft-fail build warning: if the `ai` feature is off, inference will
    // refuse anyway — but tell the user upfront so they don't click and
    // get a surprise error.
    if !bigimage_ai::onnx_available() {
        let banner = gtk::Label::builder()
            .label(
                "Esta build não inclui IA. Recompile com `cargo build --features ai` \
                 e instale `onnxruntime` (Arch: `pacman -S onnxruntime`).",
            )
            .wrap(true)
            .xalign(0.0)
            .build();
        banner.add_css_class("warning");
        content.append(&banner);
    }

    let memory_row = gtk::Label::builder()
        .label(gettext(
            "Remoção de fundo: requer 8 GiB de RAM livre para iniciar. Este recurso é opcional.",
        ))
        .wrap(true)
        .xalign(0.0)
        .build();
    memory_row.add_css_class("dim-label");
    content.append(&memory_row);

    let files_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(format!("{} imagem(ns)", files.len()))
        .build();
    files_row.set_subtitle(&files_subtitle(&files));
    let files_icon = gtk::Image::from_icon_name("image-x-generic-symbolic");
    files_row.add_prefix(&files_icon);

    let model_row = adw::ActionRow::builder()
        .use_markup(false)
        .title("Modelo")
        .subtitle("BiRefNet-lite (MIT) · roda offline no seu computador")
        .build();
    let model_icon = gtk::Image::from_icon_name("application-x-executable-symbolic");
    model_row.add_prefix(&model_icon);

    // Subtitle é Pango-markup em adw::ActionRow → angle-brackets ("<orig>")
    // eram interpretados como tag malformada e a linha ficava vazia.
    // Usar chevrons Unicode evita o parser sem escaping manual.
    let output_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Saída"))
        .subtitle("«origem»_nobg.png · PNG com alfa, sem sobrescrever")
        .build();
    let output_icon = gtk::Image::from_icon_name("document-save-symbolic");
    output_row.add_prefix(&output_icon);

    let group = adw::PreferencesGroup::builder()
        .title("Remover fundo com IA local")
        .description(
            "O modelo roda inteiramente no seu computador. \
             Nenhuma imagem sai do disco — sem upload, sem conta, sem API externa.",
        )
        .build();
    group.add(&files_row);
    group.add(&model_row);
    group.add(&output_row);
    content.append(&group);

    // Progress bar — hidden until the user clicks Remover fundo. Shows
    // download bytes on first run, then per-file inference progress.
    let progress_bar =
        gtk::ProgressBar::builder().show_text(true).visible(false).margin_top(6).build();
    content.append(&progress_bar);

    let status = gtk::Label::builder().label("").wrap(true).xalign(0.0).build();
    status.add_css_class("dim-label");
    content.append(&status);

    let cancel_btn = gtk::Button::with_label("Cancelar");
    let apply_btn = gtk::Button::builder().label("Remover fundo").build();
    apply_btn.add_css_class("suggested-action");
    apply_btn.add_css_class("pill");

    let actions_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    actions_box.append(&cancel_btn);
    actions_box.append(&apply_btn);
    content.append(&actions_box);

    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&content));
    window.set_content(Some(&tv));

    // Cancellation flag — shared between UI, worker thread, and the
    // close_request handler so fechar a janela (X, Esc, botão Cancelar)
    // efetivamente encerra a interação: o Compare pós-sucesso NÃO abre,
    // e o worker para de processar arquivos restantes.
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    {
        let cancelled = cancelled.clone();
        window.connect_close_request(move |_| {
            cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            glib::Propagation::Proceed
        });
    }
    {
        let window = window.clone();
        dialog_signal!(window, cancel_btn, connect_clicked, move |_| window.close());
    }

    {
        let files_rc = files.clone();
        let status = status.clone();
        let progress_bar = progress_bar.clone();
        let apply_btn = apply_btn.clone();
        let cancel_btn = cancel_btn.clone();
        let window = window.clone();
        let cancelled = cancelled.clone();
        dialog_signal!(window, apply_btn.clone(), connect_clicked, move |_| {
            if let Err(error) = bigimage_ai::background::check_resources() {
                status.set_text(&error.to_string());
                return;
            }
            apply_btn.set_sensitive(false);
            // Cancelar continua disponível — vira "abortar e fechar".
            cancel_btn.set_label(&gettext("Cancelar"));
            progress_bar.set_visible(true);
            progress_bar.set_fraction(0.0);
            progress_bar.set_text(Some(&gettext("Preparando…")));
            status.set_text("");

            // Rc<Vec<PathBuf>> não é Send — worker recebe um Vec próprio.
            let files_owned: Vec<PathBuf> = (*files_rc).clone();
            let shared: std::sync::Arc<std::sync::Mutex<BgUiState>> =
                std::sync::Arc::new(std::sync::Mutex::new(BgUiState::Idle));

            {
                let shared = shared.clone();
                let cancelled_w = cancelled.clone();
                std::thread::spawn(move || {
                    // Build a single BgSession for the whole batch — Download
                    // progress streams once, then the loaded weights are
                    // reused for every file. Previous code reloaded ~224 MB
                    // per file (perf review I-ORT).
                    let total = files_owned.len();
                    let session_progress_shared = shared.clone();
                    let session_result = bigimage_ai::background::BgSession::new(move |stage| {
                        if let bigimage_ai::background::BgStage::Download { done, total } = stage {
                            let mut st =
                                session_progress_shared.lock().unwrap_or_else(|p| p.into_inner());
                            *st = BgUiState::Download { done, total };
                        }
                    });
                    let mut session = match session_result {
                        Ok(s) => s,
                        Err(e) => {
                            let mut st = shared.lock().unwrap_or_else(|p| p.into_inner());
                            *st = BgUiState::Done {
                                outputs: vec![None; total],
                                first_err: Some(format!("falha ao carregar modelo: {e}")),
                            };
                            return;
                        }
                    };

                    let mut outputs: Vec<Option<PathBuf>> = Vec::with_capacity(total);
                    let mut first_err: Option<String> = None;
                    for (idx, path) in files_owned.iter().enumerate() {
                        // Bail entre arquivos — não dá para interromper o
                        // session.run() do ORT no meio, mas pelo menos o
                        // próximo arquivo do lote não é tocado.
                        if cancelled_w.load(std::sync::atomic::Ordering::Relaxed) {
                            outputs.push(None);
                            continue;
                        }
                        {
                            // A poisoned mutex would normally panic the worker
                            // and silently kill the GUI thread when the timeout
                            // poll later tried to lock; into_inner() recovers
                            // the inner state and lets the dialog finish with
                            // a clean error instead.
                            let mut st = shared.lock().unwrap_or_else(|p| p.into_inner());
                            *st = BgUiState::Processing {
                                idx,
                                total,
                                file: path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default(),
                            };
                        }
                        match remove_bg_one_with_session(&mut session, path) {
                            Ok(out) => outputs.push(Some(out)),
                            Err(e) => {
                                if first_err.is_none() {
                                    first_err = Some(format!("{}: {e}", path.display()));
                                }
                                outputs.push(None);
                            }
                        }
                    }
                    let mut st = shared.lock().unwrap_or_else(|p| p.into_inner());
                    *st = BgUiState::Done { outputs, first_err };
                });
            }

            // UI poll @ 10 Hz. Se a janela já foi fechada (cancelled),
            // cortamos o loop imediatamente — widgets podem estar mortos.
            let progress_bar = progress_bar.clone();
            let status = status.clone();
            let apply_btn = apply_btn.clone();
            let cancel_btn = cancel_btn.clone();
            let window = window.clone();
            let originals = files_rc.clone();
            let cancelled_poll = cancelled.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
                if cancelled_poll.load(std::sync::atomic::Ordering::Relaxed) {
                    return glib::ControlFlow::Break;
                }
                let snapshot = shared.lock().unwrap_or_else(|p| p.into_inner()).clone();
                match snapshot {
                    BgUiState::Idle => glib::ControlFlow::Continue,
                    BgUiState::Download { done, total } => {
                        let frac = if total > 0 {
                            (done as f64 / total as f64).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        progress_bar.set_fraction(frac);
                        progress_bar.set_text(Some(&format!(
                            "Baixando modelo (uma vez só): {} / {}",
                            format_size(done),
                            format_size(total),
                        )));
                        glib::ControlFlow::Continue
                    }
                    BgUiState::Processing { idx, total, file } => {
                        let frac = (idx as f64 / total.max(1) as f64).clamp(0.0, 1.0);
                        progress_bar.set_fraction(frac);
                        progress_bar.set_text(Some(&format!(
                            "Processando {}/{} — {}",
                            idx + 1,
                            total,
                            file
                        )));
                        glib::ControlFlow::Continue
                    }
                    BgUiState::Done { outputs, first_err } => {
                        // Checa cancelamento antes de qualquer side-effect:
                        // se o usuário fechou a janela durante a inferência,
                        // não queremos que o Compare apareça sozinho.
                        if cancelled_poll.load(std::sync::atomic::Ordering::Relaxed) {
                            return glib::ControlFlow::Break;
                        }
                        progress_bar.set_fraction(1.0);
                        let ok = outputs.iter().filter(|o| o.is_some()).count();
                        let fail = outputs.len() - ok;
                        if fail == 0 {
                            status.set_text(&format!("Pronto: {ok} imagem(ns) gravada(s)"));
                            progress_bar.set_text(Some(&gettext("Concluído")));
                            progress_bar.add_css_class("success");
                            let single = originals.len() == 1;
                            if single {
                                if let (Some(orig), Some(Some(out))) =
                                    (originals.first(), outputs.first())
                                {
                                    spawn_compare_viewer(orig, out);
                                }
                                // Single-file: fecha imediato enquanto o
                                // Compare abre — evita janela dupla visível.
                                window.close();
                            } else {
                                // Lote: segura 2 s pra o usuário conferir o
                                // sumário antes de sumir.
                                let w = window.clone();
                                glib::timeout_add_seconds_local_once(2, move || w.close());
                            }
                        } else {
                            let detail = first_err.unwrap_or_default();
                            status.set_text(&format!("{ok} ok, {fail} falha(s). Último: {detail}"));
                            progress_bar.set_text(Some(&gettext("Com erros")));
                            progress_bar.add_css_class("error");
                            apply_btn.set_sensitive(true);
                            cancel_btn.set_label("Fechar");
                        }
                        glib::ControlFlow::Break
                    }
                }
            });
        });
    }

    window
}

/// Launch the Compare dialog in a separate process so both windows can
/// coexist (each `adw::Application` instance takes over the main loop).
/// Called right after a successful single-file remove-bg so the user
/// sees original vs. result side-by-side.
fn spawn_compare_viewer(original: &Path, result: &Path) {
    let Ok(exe) = std::env::current_exe() else { return };
    let _ =
        std::process::Command::new(exe).arg("--dialog=compare").arg(original).arg(result).spawn();
}

#[cfg(test)]
mod anchor_tests {
    use super::anchor_axis;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "{a} != {b}");
    }

    #[test]
    fn panned_image_anchors_cursor_point() {
        let (adj, max, _) = anchor_axis(500.0, 500.0, 1.0, 1.2, 2000.0, 1000.0);
        approx(max, 1400.0);
        approx(adj, 700.0);
    }

    #[test]
    fn fit_to_scale_anchors_over_letterbox() {
        let (adj, max, pic) = anchor_axis(600.0, 0.0, 0.5, 1.0, 1000.0, 1000.0);
        approx(pic, 1000.0);
        approx(max, 0.0);
        approx(adj, 0.0);
    }

    #[test]
    fn fit_to_scale_anchors_x_axis_wider_than_viewport() {
        let (adj, max, _) = anchor_axis(800.0, 0.0, 0.5, 1.0, 2000.0, 1000.0);
        approx(max, 1000.0);
        approx(adj, 800.0);
    }

    #[test]
    fn content_smaller_than_viewport_has_zero_scroll() {
        let (adj, max, pic) = anchor_axis(500.0, 0.0, 1.0, 1.5, 400.0, 1000.0);
        approx(pic, 600.0);
        approx(max, 0.0);
        approx(adj, 0.0);
    }

    #[test]
    fn edit_ops_map_to_expected_transforms() {
        use super::{apply_op_to_image, EditOp};
        use image::{DynamicImage, Rgba, RgbaImage};
        // 2×1: pixel esquerdo vermelho, direito verde. Giro à direita → 1×2
        // com vermelho em cima. Cobre também CCW (inverso), 180, flip-H e -V.
        let mut src = RgbaImage::new(2, 1);
        src.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        src.put_pixel(1, 0, Rgba([0, 255, 0, 255]));
        let img = DynamicImage::ImageRgba8(src);

        let cw = apply_op_to_image(img.clone(), EditOp::RotateCw).to_rgba8();
        assert_eq!(cw.dimensions(), (1, 2));
        assert_eq!(cw.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(cw.get_pixel(0, 1).0, [0, 255, 0, 255]);

        let ccw = apply_op_to_image(img.clone(), EditOp::RotateCcw).to_rgba8();
        assert_eq!(ccw.dimensions(), (1, 2));
        assert_eq!(ccw.get_pixel(0, 0).0, [0, 255, 0, 255]);
        assert_eq!(ccw.get_pixel(0, 1).0, [255, 0, 0, 255]);

        let r180 = apply_op_to_image(img.clone(), EditOp::Rotate180).to_rgba8();
        assert_eq!(r180.dimensions(), (2, 1));
        assert_eq!(r180.get_pixel(0, 0).0, [0, 255, 0, 255]);
        assert_eq!(r180.get_pixel(1, 0).0, [255, 0, 0, 255]);

        let fh = apply_op_to_image(img.clone(), EditOp::FlipH).to_rgba8();
        assert_eq!(fh.get_pixel(0, 0).0, [0, 255, 0, 255]);
        assert_eq!(fh.get_pixel(1, 0).0, [255, 0, 0, 255]);

        let fv = apply_op_to_image(img, EditOp::FlipV).to_rgba8();
        // 1px alto → flip V é no-op visual
        assert_eq!(fv.dimensions(), (2, 1));
        assert_eq!(fv.get_pixel(0, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn edited_save_is_atomic_and_preserves_permissions() {
        use super::{write_edits_to_disk, EditOp};
        use image::{Rgb, RgbImage};
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("photo.png");
        RgbImage::from_pixel(3, 2, Rgb([10, 20, 30])).save(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        write_edits_to_disk(&path, &[EditOp::RotateCw]).unwrap();

        let saved = image::open(&path).unwrap();
        assert_eq!((saved.width(), saved.height()), (2, 3));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o640);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".bigiris-"))
            .collect();
        assert!(leftovers.is_empty(), "atomic save left a temporary file behind");
    }

    #[test]
    fn histogram_counts_channels_and_ignores_alpha() {
        use super::compute_histogram;
        // 3 pixels: puro vermelho, puro verde (com alpha zero), puro azul.
        // Alpha deve ser ignorado — só somamos R/G/B no ponto certo.
        let rgba = vec![
            255, 0, 0, 255, //
            0, 255, 0, 0, //
            0, 0, 255, 255,
        ];
        let h = compute_histogram(&rgba);
        // R: bin 255 += 1 (pixel 0); bin 0 += 2 (pixels 1, 2).
        assert_eq!(h[255], 1);
        assert_eq!(h[0], 2);
        // G: bin 255 += 1 (pixel 1); bin 0 += 2 (pixels 0, 2).
        assert_eq!(h[256 + 255], 1);
        assert_eq!(h[256], 2);
        // B: bin 255 += 1 (pixel 2); bin 0 += 2 (pixels 0, 1).
        assert_eq!(h[512 + 255], 1);
        assert_eq!(h[512], 2);
        // Soma por canal confere com número de pixels.
        let sum = |chan: &[u32]| -> u32 { chan.iter().sum() };
        assert_eq!(sum(&h[0..256]), 3);
        assert_eq!(sum(&h[256..512]), 3);
        assert_eq!(sum(&h[512..768]), 3);
    }

    #[test]
    fn zoom_out_preserves_anchor() {
        let (adj, max, _) = anchor_axis(500.0, 700.0, 1.2, 1.0, 2000.0, 1000.0);
        approx(max, 1000.0);
        approx(adj, 500.0);
    }

    #[test]
    fn default_crop_is_centered_half() {
        use super::default_crop;
        let r = default_crop(100, 60);
        assert_eq!((r.width, r.height), (50, 30));
        assert_eq!((r.x, r.y), (25, 15));
    }

    #[test]
    fn default_crop_handles_degenerate_sizes() {
        // 1×1 → rect is forced to (0,0,1,1), never zero.
        let r = super::default_crop(1, 1);
        assert_eq!((r.x, r.y, r.width, r.height), (0, 0, 1, 1));
    }

    #[test]
    fn contain_fit_letterboxes_wider_widget_than_image() {
        use super::contain_fit;
        // Widget 400×200, image 200×200 → scale 1.0 (height bound),
        // letterbox 100px left + 100px right.
        let (s, ox, oy) = contain_fit(400.0, 200.0, 200.0, 200.0);
        assert!((s - 1.0).abs() < 1e-9);
        assert!((ox - 100.0).abs() < 1e-9);
        assert!(oy.abs() < 1e-9);
    }

    #[test]
    fn contain_fit_scales_down_and_centers() {
        use super::contain_fit;
        // Widget 300×300, image 600×300 → scale 0.5, letterboxed vertically.
        let (s, ox, oy) = contain_fit(300.0, 300.0, 600.0, 300.0);
        assert!((s - 0.5).abs() < 1e-9);
        assert!(ox.abs() < 1e-9);
        assert!((oy - 75.0).abs() < 1e-9);
    }

    #[test]
    fn contain_fit_degenerate_inputs_return_identity() {
        assert_eq!(super::contain_fit(0.0, 100.0, 100.0, 100.0), (1.0, 0.0, 0.0));
        assert_eq!(super::contain_fit(100.0, 100.0, 0.0, 100.0), (1.0, 0.0, 0.0));
    }

    #[test]
    fn decide_drag_mode_picks_resize_when_near_corner() {
        use super::{decide_drag_mode, CropDragMode};
        // Widget 400×400, image 200×200 at scale 2, centered (ox=oy=0).
        // Rect at (50,50,100,100) natural → widget corners at (100,100)..(300,300).
        let mode = decide_drag_mode(
            (98.0, 102.0), // ~2px from top-left handle
            (50.0, 50.0, 100.0, 100.0),
            (2.0, 0.0, 0.0),
            (200, 200),
        );
        match mode {
            CropDragMode::ResizeCorner { anchor_nat, click_nat } => {
                assert_eq!(anchor_nat, (150.0, 150.0)); // opposite = bottom-right
                assert_eq!(click_nat, (50.0, 50.0));
            }
            other => panic!("expected ResizeCorner, got {other:?}"),
        }
    }

    #[test]
    fn decide_drag_mode_picks_move_when_strictly_inside() {
        use super::{decide_drag_mode, CropDragMode};
        let mode = decide_drag_mode(
            (200.0, 200.0), // middle of the rect in widget space (nat 100,100)
            (50.0, 50.0, 100.0, 100.0),
            (2.0, 0.0, 0.0),
            (200, 200),
        );
        match mode {
            CropDragMode::Move { rect0, .. } => {
                assert_eq!(rect0, (50.0, 50.0, 100.0, 100.0));
            }
            other => panic!("expected Move, got {other:?}"),
        }
    }

    #[test]
    fn decide_drag_mode_picks_new_selection_when_outside() {
        use super::{decide_drag_mode, CropDragMode};
        let mode = decide_drag_mode(
            (20.0, 20.0), // far from rect at widget (100,100)-(300,300)
            (50.0, 50.0, 100.0, 100.0),
            (2.0, 0.0, 0.0),
            (200, 200),
        );
        match mode {
            CropDragMode::NewSelection { anchor_nat } => {
                assert_eq!(anchor_nat, (10.0, 10.0));
            }
            other => panic!("expected NewSelection, got {other:?}"),
        }
    }

    #[test]
    fn apply_drag_move_preserves_size_and_clamps() {
        use super::{apply_drag, CropDragMode};
        let mode =
            CropDragMode::Move { rect0: (50.0, 50.0, 100.0, 100.0), click_nat: (100.0, 100.0) };
        // Small move → shift preserved.
        let r = apply_drag(mode, (10.0, -20.0), (200, 200));
        assert_eq!(r, (60.0, 30.0, 100.0, 100.0));
        // Huge move → clamped at image edge, size still preserved.
        let r = apply_drag(mode, (1_000.0, 1_000.0), (200, 200));
        assert_eq!(r, (100.0, 100.0, 100.0, 100.0));
    }

    #[test]
    fn apply_drag_resize_corner_anchors_opposite() {
        use super::{apply_drag, CropDragMode};
        // Anchor is bottom-right (150,150); click started top-left (50,50).
        let mode =
            CropDragMode::ResizeCorner { anchor_nat: (150.0, 150.0), click_nat: (50.0, 50.0) };
        // Move click corner by (+20, +10) in natural pixels.
        let r = apply_drag(mode, (20.0, 10.0), (200, 200));
        // New click = (70, 60); rect becomes (70, 60, 80, 90).
        assert_eq!(r, (70.0, 60.0, 80.0, 90.0));
    }

    #[test]
    fn upscale_doubles_dimensions_lanczos() {
        // Upscale was a thin wrapper over `resize_file` at factor*100 %.
        // The helper went away with the worker-thread refactor; inline the
        // same call here so the contract (factor 2 → 200 % Lanczos) keeps
        // its regression coverage.
        use bigimage_core::{
            resize_file, ConvertOutcome, EncodeOptions, Filter, OverwritePolicy, ResizeMode,
        };
        use image::{Rgb, RgbImage};
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("src.png");
        RgbImage::from_pixel(30, 20, Rgb([10, 20, 30])).save(&path).unwrap();

        let opts = EncodeOptions::default();
        let outcome = resize_file(
            &path,
            ResizeMode::Percent(200.0),
            Filter::Lanczos3,
            None,
            &opts,
            OverwritePolicy::Replace,
        )
        .expect("resize_file at 200 %% should succeed");
        assert!(matches!(outcome, ConvertOutcome::Written { .. }));

        // Output name follows resize's Percent suffix: "_200pct".
        let out = dir.path().join("src_200pct.png");
        assert!(out.exists(), "saída esperada não encontrada: {}", out.display());
        let decoded = image::open(&out).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (60, 40));
    }

    #[test]
    fn upscale_factors_are_restricted_to_2_3_4() {
        // Preserve the contract the CLI advertises — if a future refactor
        // opens the range, this test prompts us to revisit the UI presets.
        let factors: Vec<u8> = super::UPSCALE_FACTORS.iter().map(|(_, f)| *f).collect();
        assert_eq!(factors, vec![2, 3, 4]);
    }

    #[test]
    fn nobg_output_path_is_png_sibling_with_suffix() {
        use super::nobg_output_path;
        use std::path::PathBuf;

        let p = nobg_output_path(&PathBuf::from("/tmp/foo/bar.jpg"));
        assert_eq!(p, PathBuf::from("/tmp/foo/bar_nobg.png"));

        // Sem extensão → ainda vira .png no mesmo diretório.
        let p = nobg_output_path(&PathBuf::from("/tmp/foo/bar"));
        assert_eq!(p, PathBuf::from("/tmp/foo/bar_nobg.png"));

        // Caminho sem diretório → fica no CWD relativo.
        let p = nobg_output_path(&PathBuf::from("solo.png"));
        assert_eq!(p, PathBuf::from("solo_nobg.png"));
    }
}

#[cfg(test)]
mod folder_nav_tests {
    use super::{expand_to_folder, is_viewable, natural_cmp};
    use std::cmp::Ordering;
    use std::path::{Path, PathBuf};

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"").expect("escrever arquivo de teste");
        path
    }

    #[test]
    fn digits_compare_as_numbers_not_text() {
        assert_eq!(natural_cmp("2.png", "10.png"), Ordering::Less);
        assert_eq!(natural_cmp("10.png", "2.png"), Ordering::Greater);
        assert_eq!(natural_cmp("100.png", "99.png"), Ordering::Greater);
    }

    #[test]
    fn equal_values_order_by_leading_zeros() {
        assert_eq!(natural_cmp("7.png", "007.png"), Ordering::Less);
        assert_eq!(natural_cmp("7.png", "7.png"), Ordering::Equal);
    }

    #[test]
    fn text_compares_case_insensitively() {
        assert_eq!(natural_cmp("alpha.png", "Beta.png"), Ordering::Less);
        assert_eq!(natural_cmp("IMG_2.jpg", "img_10.jpg"), Ordering::Less);
    }

    #[test]
    fn viewable_follows_the_core_format_table() {
        assert!(is_viewable(Path::new("a.png")));
        assert!(is_viewable(Path::new("a.JPEG")));
        assert!(is_viewable(Path::new("a.avif")));
        assert!(!is_viewable(Path::new("a.txt")));
        assert!(!is_viewable(Path::new("sem-extensao")));
    }

    #[test]
    fn single_file_expands_to_its_folder_in_natural_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["10.png", "2.png", "1.png", "notas.txt"] {
            touch(dir.path(), name);
        }
        let clicked = dir.path().join("2.png");

        let (files, idx) = expand_to_folder(vec![clicked.clone()]);

        let names: Vec<String> =
            files.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["1.png", "2.png", "10.png"], "o .txt fica de fora");
        assert_eq!(idx, 1, "abre posicionado no arquivo clicado");
    }

    #[test]
    fn explicit_selection_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = touch(dir.path(), "1.png");
        let b = touch(dir.path(), "2.png");
        touch(dir.path(), "3.png");

        let (files, idx) = expand_to_folder(vec![a.clone(), b.clone()]);

        assert_eq!(files, vec![a, b], "seleção da linha de comando é respeitada");
        assert_eq!(idx, 0);
    }

    #[test]
    fn lone_image_in_folder_stays_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let only = touch(dir.path(), "unica.png");
        touch(dir.path(), "leia-me.txt");

        let (files, idx) = expand_to_folder(vec![only.clone()]);

        assert_eq!(files, vec![only]);
        assert_eq!(idx, 0);
    }

    #[test]
    fn missing_file_falls_back_to_the_input() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ghost = dir.path().join("nao-existe.png");

        let (files, idx) = expand_to_folder(vec![ghost.clone()]);

        assert_eq!(files, vec![ghost]);
        assert_eq!(idx, 0);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn prisma_formats_match_tested_core_formats() {
        let formats: std::collections::HashSet<_> =
            CONVERT_FORMATS.iter().map(|(_, format)| *format).collect();
        assert_eq!(formats.len(), CONVERT_FORMATS.len(), "duplicate UI format");
        assert_eq!(formats, Format::all().iter().copied().collect());
    }

    #[test]
    fn idle_decoder_exits_when_loader_is_dropped() {
        let loader = ViewerLoader::new();
        let witness = Arc::downgrade(&loader.pending);
        drop(loader);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while witness.upgrade().is_some() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(witness.upgrade().is_none(), "idle decoder retained its queue after shutdown");
    }

    // Capture only our synthetic windows, never the user's desktop.
    fn capture_window(window: &adw::ApplicationWindow, name: &str) {
        let Some(directory) = std::env::var_os("BIGIRIS_TEST_SCREENSHOTS_DIR") else { return };
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        let context = glib::MainContext::default();
        let deadline = std::time::Instant::now() + Duration::from_millis(150);
        while std::time::Instant::now() < deadline {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let paintable = gtk::WidgetPaintable::new(Some(window));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(&snapshot, window.width() as f64, window.height() as f64);
        let node = snapshot.to_node().expect("window must have rendered content");
        let texture = window.renderer().expect("window renderer").render_texture(&node, None);
        texture.save_to_png(directory.join(format!("{name}.png"))).unwrap();
    }

    fn exercise_pointer_zoom(app: &adw::Application, directory: &Path) {
        let source = directory.join("zoom-grid.png");
        image::RgbImage::from_fn(1600, 1200, |x, y| {
            image::Rgb([(x / 8 % 256) as u8, (y / 8 % 256) as u8, 100])
        })
        .save(&source)
        .unwrap();
        let (window, witness) = build_window_tracked(app, Rc::new(vec![source.clone(), source]), 0);
        window.set_default_size(640, 500);
        let state = witness.upgrade().unwrap();
        {
            let mut s = state.borrow_mut();
            s.autohide_ui = false;
            s.ui_pinned = true;
        }
        window.present();
        let context = glib::MainContext::default();
        let wait = |condition: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !condition() {
                while context.pending() {
                    context.iteration(false);
                }
                assert!(std::time::Instant::now() < deadline, "zoom did not settle");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait(&|| state.borrow().natural == (1600, 1200) && state.borrow().scroller.width() > 0);
        // Allow the first real allocation after decode, not just header math.
        for _ in 0..10 {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let point = {
            let s = state.borrow();
            (
                s.scroller.hadjustment().page_size() * 0.68,
                s.scroller.vadjustment().page_size() * 0.52,
            )
        };
        let image_point = || {
            let s = state.borrow();
            let bounds = s.picture.compute_bounds(&s.scroller).unwrap();
            let paintable = s.picture.paintable().unwrap();
            let (iw, ih) =
                (f64::from(paintable.intrinsic_width()), f64::from(paintable.intrinsic_height()));
            let (pw, ph) = (f64::from(s.picture.width()), f64::from(s.picture.height()));
            let mut scale = (pw / iw).min(ph / ih);
            if s.picture.content_fit() == gtk::ContentFit::ScaleDown {
                scale = scale.min(1.0);
            }
            let origin = (
                f64::from(bounds.x()) + (pw - iw * scale) / 2.0,
                f64::from(bounds.y()) + (ph - ih * scale) / 2.0,
            );
            (
                (point.0 - origin.0) / (iw * scale) * f64::from(s.natural.0),
                (point.1 - origin.1) / (ih * scale) * f64::from(s.natural.1),
            )
        };
        let rendered_pixel = || {
            let until = std::time::Instant::now() + Duration::from_millis(80);
            while std::time::Instant::now() < until {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let point_in_window = state
                .borrow()
                .scroller
                .compute_point(&window, &graphene::Point::new(point.0 as f32, point.1 as f32))
                .unwrap();
            let snapshot = gtk::Snapshot::new();
            gtk::WidgetPaintable::new(Some(&window)).snapshot(
                &snapshot,
                f64::from(window.width()),
                f64::from(window.height()),
            );
            let node = snapshot.to_node().unwrap();
            let texture = window.renderer().unwrap().render_texture(&node, None);
            let path = directory.join("zoom-render.png");
            texture.save_to_png(&path).unwrap();
            *image::open(path)
                .unwrap()
                .to_rgba8()
                .get_pixel(point_in_window.x().round() as u32, point_in_window.y().round() as u32)
        };
        let before = image_point();
        let pixel_before = rendered_pixel();
        capture_window(&window, "zoom-fit");
        let controllers = state.borrow().scroller.observe_controllers();
        let motion = (0..controllers.n_items())
            .find_map(|index| {
                controllers.item(index)?.downcast::<gtk::EventControllerMotion>().ok().filter(
                    |controller| controller.name().as_deref() == Some("bigiris-zoom-pointer"),
                )
            })
            .expect("canvas pointer controller");
        motion.emit_by_name::<()>("motion", &[&point.0, &point.1]);
        let anchored = |expected: (f64, f64)| {
            wait(&|| state.borrow().pending_zoom.is_none());
            let actual = image_point();
            assert!(
                (actual.0 - expected.0).abs() < 3.0 && (actual.1 - expected.1).abs() < 3.0,
                "displayed point drifted: {expected:?} -> {actual:?}"
            );
            let pixel = rendered_pixel();
            assert!(
                pixel.0.iter().zip(pixel_before.0).all(|(a, b)| a.abs_diff(b) <= 2),
                "rendered pixel drifted: {pixel_before:?} -> {pixel:?}"
            );
        };
        handle_key(&state, gdk::Key::plus, gdk::ModifierType::empty());
        anchored(before);
        for _ in 0..4 {
            handle_key(&state, gdk::Key::plus, gdk::ModifierType::empty());
        }
        anchored(before);
        zoom_out(&state); // the toolbar action uses the same selected point
        anchored(before);
        handle_key(&state, gdk::Key::_1, gdk::ModifierType::empty());
        anchored(before);
        assert_eq!(state.borrow().picture.width(), 1600);
        assert_eq!(state.borrow().picture.height(), 1200);
        capture_window(&window, "zoom-pointer-100pct");
        zoom_in(&state);
        handle_key(&state, gdk::Key::_0, gdk::ModifierType::empty());
        assert!(state.borrow().pending_zoom.is_none());
        zoom_in(&state);
        navigate_relative(&state, 1);
        assert!(state.borrow().pending_zoom.is_none(), "old zoom survived navigation");
        wait(&|| state.borrow().texture.is_some() && !state.borrow().loading_spinner.is_visible());
        zoom_in(&state);
        window.destroy();
        drop(state);
        drop(window);
        wait(&|| witness.upgrade().is_none());
    }

    #[test]
    #[ignore = "requires a graphical display; run alone with GSETTINGS_BACKEND=memory"]
    fn viewer_lifecycle_releases_state_and_workers() {
        assert_eq!(
            std::env::var("GSETTINGS_BACKEND").as_deref(),
            Ok("memory"),
            "synthetic GUI tests require GSETTINGS_BACKEND=memory"
        );
        adw::init().expect("GTK display required");
        if let Ok(scheme) = std::env::var("BIGIRIS_TEST_COLOR_SCHEME") {
            let scheme = match scheme.as_str() {
                "light" => adw::ColorScheme::ForceLight,
                "dark" => adw::ColorScheme::ForceDark,
                _ => panic!("test color scheme must be light or dark"),
            };
            adw::StyleManager::default().set_color_scheme(scheme);
        }
        let app = adw::Application::builder()
            .application_id("com.biglinux.Iris.QualityProbe")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gio::Cancellable>).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("fixture <&>.png");
        image::RgbImage::from_pixel(320, 240, image::Rgb([10, 20, 30])).save(&source).unwrap();
        let context = glib::MainContext::default();
        for index in 0..30 {
            let (window, state) = build_window_tracked(&app, Rc::new(vec![source.clone()]), 0);
            window.present();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while state.upgrade().is_some_and(|s| s.borrow().natural == (0, 0)) {
                while context.pending() {
                    context.iteration(false);
                }
                assert!(std::time::Instant::now() < deadline, "viewer failed to decode fixture");
                std::thread::sleep(Duration::from_millis(5));
            }
            if index == 0 {
                capture_window(&window, "viewer");
            }
            let window_witness = window.downgrade();
            window.destroy();
            drop(window);
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while state.upgrade().is_some() && std::time::Instant::now() < deadline {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                state.upgrade().is_none(),
                "viewer state leaked: references={}, window_alive={}",
                state.strong_count(),
                window_witness.upgrade().is_some()
            );
        }
        exercise_pointer_zoom(&app, dir.path());

        // A large collection creates only one page of GTK thumbnails.
        let (large, witness) =
            build_window_tracked(&app, Rc::new(vec![source.clone(); 10_000]), 9_500);
        large.present();
        {
            let state = witness.upgrade().unwrap();
            assert_eq!(state.borrow().film_strip_buttons.len(), 41);
            state.borrow_mut().idx = 9_999;
            refresh_film_strip_active(&state);
            let state = state.borrow();
            assert!(state.film_strip_buttons.len() <= 41);
            assert!(state.idx >= state.film_strip_start);
            assert!(state.idx < state.film_strip_start + state.film_strip_buttons.len());
        }
        large.destroy();
        drop(large);
        while context.pending() {
            context.iteration(false);
        }
        assert!(witness.upgrade().is_none(), "large collection retained viewer state");

        let files = Rc::new(vec![source.clone(), source.clone()]);
        type DialogBuilder = fn(&adw::Application, Rc<Vec<PathBuf>>) -> adw::ApplicationWindow;
        fn batch(app: &adw::Application, files: Rc<Vec<PathBuf>>) -> adw::ApplicationWindow {
            build_batch_dialog(app, Rc::new(RefCell::new(files.as_ref().clone())))
        }
        fn properties(app: &adw::Application, files: Rc<Vec<PathBuf>>) -> adw::ApplicationWindow {
            build_properties_window(app, &files[0])
        }
        for (index, builder) in [
            build_convert_dialog as DialogBuilder,
            build_resize_dialog,
            build_rotate_dialog,
            build_crop_dialog,
            build_extent_dialog,
            build_upscale_dialog,
            build_flip_dialog,
            build_adjust_dialog,
            build_compare_dialog,
            build_animate_dialog,
            build_remove_bg_dialog,
            batch,
            properties,
        ]
        .into_iter()
        .enumerate()
        {
            let window = builder(&app, files.clone());
            let witness = window.downgrade();
            window.present();
            while context.pending() {
                context.iteration(false);
            }
            capture_window(&window, &format!("prisma-{index:02}"));
            window.destroy();
            drop(window);
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while witness.upgrade().is_some() && std::time::Instant::now() < deadline {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(witness.upgrade().is_none(), "Prisma window {index} leaked after destroy");
        }
    }
}

#[cfg(test)]
mod print_pixel_tests {
    #[test]
    fn cairo_pixels_premultiply_alpha_in_native_order() {
        let result =
            super::premultiply_for_cairo(vec![200, 100, 50, 128, 255, 1, 240, 0, 2, 3, 4, 255]);
        let values: Vec<_> =
            result.as_chunks::<4>().0.iter().map(|p| u32::from_ne_bytes(*p)).collect();
        assert_eq!(values, vec![0x80643219, 0x00000000, 0xff020304]);
    }
}
