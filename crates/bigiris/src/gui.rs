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
use std::sync::{Arc, Condvar, Mutex};

use adw::prelude::*;
use gettextrs::gettext;
use gtk::{gdk, gio, glib, graphene};
use gtk4 as gtk;
use libadwaita as adw;

mod previews;
mod transfers;
mod viewer;
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
/// How long a save notice about dropped metadata stays on screen.
const OSD_NOTICE_MS: u64 = 6000;
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
.film-strip-bar { border-radius: 14px; background-color: rgba(0, 0, 0, 0.6); }
.film-strip { padding: 6px; }
.film-strip button {
    padding: 0; min-height: 0; min-width: 0;
    border-radius: 8px; opacity: 0.6; transition: opacity 150ms ease-out;
}
.film-strip button:hover, .film-strip button:focus-visible { opacity: 1; }
.film-strip button.active-thumb {
    opacity: 1;
    outline: 2px solid @accent_color;
    outline-offset: 2px;
}
.histogram-overlay { border-radius: 12px; }
.viewer-loading { padding: 12px; border-radius: 8px; background: @window_bg_color; color: @window_fg_color; }
.plasma headerbar button { border-radius: 6px; }
.plasma .suggested-action { border-radius: 6px; }
.plasma .viewer-osd { border-radius: 8px; }
";

/// Side of each square film-strip cell, in logical pixels.
const THUMB_EDGE: u32 = 64;

const fn n_(text: &'static str) -> &'static str {
    text
}

fn translated_strings(source: &[&str]) -> gtk::StringList {
    let translated: Vec<String> = source.iter().map(|text| gettext(*text)).collect();
    gtk::StringList::new(&translated.iter().map(String::as_str).collect::<Vec<_>>())
}

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

    // Abrir precisa parecer instantâneo: a decodificação da imagem pedida
    // começa antes do GTK iniciar e corre em paralelo com a criação da
    // janela, e a varredura da pasta (setas, film strip) acontece em
    // segundo plano. Nenhuma janela intermediária aparece antes da foto.
    let preload = files.first().map(|first| {
        let loader = ViewerLoader::new();
        loader.request(LoadRequest {
            generation: 1,
            path: first.clone(),
            ops: Vec::new(),
            max_pixels: None,
        });
        loader
    });
    let folder = (files.len() == 1).then(|| {
        let input = files.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let scan = gio::spawn_blocking(move || expand_to_folder_cancellable(input, &worker_cancel));
        (scan, cancel)
    });
    let startup = RefCell::new(Some((files, preload, folder)));
    app.connect_activate(move |app| {
        install_theater_css();
        let Some((files, preload, folder)) = startup.borrow_mut().take() else { return };
        let (window, state) = build_window_preloaded(app, Rc::new(files), 0, preload);
        window.present();
        let Some((scan, cancel)) = folder else { return };
        window.connect_unrealize(move |_| cancel.store(true, Ordering::Release));
        // Awaited instead of polled: nothing wakes the loop until the scan ends.
        glib::MainContext::default().spawn_local(async move {
            let Ok((files, index)) = scan.await else { return };
            if let Some(state) = state.upgrade() {
                adopt_folder(&state, files, index);
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
    zoom_source: Option<gtk::TickCallbackId>,
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
    /// Index the strip last highlighted and centred, so repeated refreshes
    /// for the same image (zoom steps, edge reveals) do no work.
    film_strip_active: Option<usize>,
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
    /// Last pointer motion. The cursor timer is armed once and, when it
    /// fires early, re-arms for the remaining time, instead of being
    /// destroyed and recreated on every motion event.
    last_motion: std::time::Instant,
    cursor_hidden: bool,
    /// Slideshow auto-advance tick (Space toggles). `None` = stopped.
    slideshow_source: Option<glib::SourceId>,
    /// DrawingArea com o overlay de histograma (canto inferior-direito).
    /// Visibilidade controlada pela tecla `G` — oculto por padrão.
    histogram_area: gtk::DrawingArea,
    /// Histograma RGB da imagem atual: `[0..256]=R, [256..512]=G, [512..768]=B`.
    /// Recalculado em cada `update_display`.
    histogram: [u32; 768],
    /// Current zoom level shown in the header, e.g. "23%".
    zoom_level: gtk::Label,
    /// Switches between the start page, the picture and the error page.
    stack: gtk::Stack,
    error_page: adw::StatusPage,
    /// Invisible pixels that keep the neighbours uploaded to the GPU.
    warm_slots: [gtk::Image; 2],
    /// Decode and edit rendering happen away from GTK's main loop. A
    /// generation token makes rapid navigation discard stale results.
    loader: ViewerLoader,
    load_generation: u64,
    /// Decodes the previous and next files while the current one is on
    /// screen, so arrow keys switch without waiting for a decode.
    prefetcher: ViewerLoader,
    /// File the prefetcher is decoding right now.
    prefetching: Option<PathBuf>,
    /// The current file and its neighbours, ready to show.
    frame_cache: Vec<CachedFrame>,
    /// Files too large to keep in memory ahead of time.
    prefetch_skip: Vec<PathBuf>,
    /// False while the current file is still being decoded; prefetching
    /// waits so it never competes with the image the user asked for.
    current_ready: bool,
    /// File whose unedited pixels `texture` holds. The film strip reuses
    /// the texture for the current cell only when it matches: `texture`
    /// still shows the previous image while the next one is decoding.
    shown_path: Option<PathBuf>,
    /// The current file is the one the prefetcher is already decoding, so
    /// its result is shown instead of decoding the same file twice.
    awaiting_prefetch: bool,
    /// Thumbnails wait for the first image so they never compete with it.
    first_frame_shown: bool,
    deferred_thumbnails: Option<Box<dyn FnOnce()>>,
    /// Only shown while edits are written to disk; loading shows nothing.
    loading_spinner: adw::Spinner,
    /// Dialog de propriedades atualmente aberto, se houver. Evita que
    /// cliques repetidos no botão `i` empilhem janelas.
    /// The modification time is part of the key: after an in-place save
    /// the same path describes different pixels and metadata.
    props_dialog: Option<(adw::ApplicationWindow, PathBuf, Option<std::time::SystemTime>)>,
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
    /// Skip files whose header reports more pixels than this.
    max_pixels: Option<u64>,
}

#[derive(Debug)]
struct DecodedFrame {
    generation: u64,
    path: PathBuf,
    /// Modification time read before decoding, to spot later changes.
    modified: Option<std::time::SystemTime>,
    /// Pixels include in-memory edits, so they do not match the file.
    edited: bool,
    pixels: Result<RawFrame, String>,
}

/// A decoded file kept ready on the GTK side.
#[derive(Clone)]
struct CachedFrame {
    path: PathBuf,
    modified: Option<std::time::SystemTime>,
    texture: gdk::Texture,
    natural: (u32, u32),
    histogram: Histogram,
}

/// Neighbours above this size are not decoded ahead: three of them would
/// hold more than half a gigabyte of RGBA pixels.
const PREFETCH_MAX_PIXELS: u64 = 40_000_000;

/// Pixel count from the file header, without decoding. `None` for formats
/// the `image` crate cannot probe (RAW, HEIF…); those are decoded anyway.
fn header_pixels(path: &Path) -> Option<u64> {
    let (w, h) =
        image::ImageReader::open(path).ok()?.with_guessed_format().ok()?.into_dimensions().ok()?;
    Some(u64::from(w) * u64::from(h))
}

fn file_modified(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

/// One coalescing decode worker per viewer. When the user holds an arrow
/// key, only the latest queued request survives; a decode already in flight
/// may finish, but its generation is ignored by the UI.
struct ViewerLoader {
    pending: Arc<(Mutex<Option<LoadRequest>>, Condvar)>,
    results: Arc<Mutex<Option<DecodedFrame>>>,
    stopped: Arc<AtomicBool>,
    /// Signalled for every published result; closed when the worker stops.
    wake: viewer::Wake,
}

impl ViewerLoader {
    fn new() -> Self {
        let pending = Arc::new((Mutex::new(None::<LoadRequest>), Condvar::new()));
        let results = Arc::new(Mutex::new(None));
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_pending = pending.clone();
        let worker_results = results.clone();
        let worker_stopped = stopped.clone();
        let wake = viewer::Wake::default();
        let worker_wake = wake.clone();
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
                let modified = file_modified(&request.path);
                let edited = !request.ops.is_empty();
                let oversized = request.max_pixels.is_some_and(|max| {
                    header_pixels(&request.path).is_some_and(|pixels| pixels > max)
                });
                let pixels = if oversized {
                    Err("image too large to prefetch".to_string())
                } else {
                    crate::codec::with_cancel(worker_stopped.clone(), || {
                        decode_frame(&request.path, &request.ops)
                    })
                    .map_err(|e| e.to_string())
                };
                if worker_stopped.load(Ordering::Relaxed) {
                    break;
                }
                // Replace stale results instead of retaining unbounded RGBA buffers.
                *worker_results.lock().expect("viewer result slot poisoned") = Some(DecodedFrame {
                    generation: request.generation,
                    path: request.path,
                    modified,
                    edited,
                    pixels,
                });
                worker_wake.notify();
            })
            .expect("could not start viewer decoder");
        Self { pending, results, stopped, wake }
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
        self.wake.close();
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

#[cfg(test)]
fn build_window_tracked(
    app: &adw::Application,
    files: Rc<Vec<PathBuf>>,
    start_idx: usize,
) -> (adw::ApplicationWindow, std::rc::Weak<RefCell<ViewerState>>) {
    build_window_preloaded(app, files, start_idx, None)
}

/// `preload` is a loader that already received generation 1 for
/// `files[start_idx]`, so the first frame is not requested twice.
fn build_window_preloaded(
    app: &adw::Application,
    files: Rc<Vec<PathBuf>>,
    start_idx: usize,
    preload: Option<ViewerLoader>,
) -> (adw::ApplicationWindow, std::rc::Weak<RefCell<ViewerState>>) {
    let preloaded = preload.is_some();
    let settings = app_settings();
    let has_images = !files.is_empty();
    let window_width = if has_images {
        settings.as_ref().map_or(1280, |s| s.int("window-width")).max(480)
    } else {
        // Same start-page size as GNOME's image viewer (Loupe).
        600
    };
    let window_height = if has_images {
        settings.as_ref().map_or(800, |s| s.int("window-height")).max(360)
    } else {
        498
    };
    let start_maximized =
        has_images && settings.as_ref().is_some_and(|s| s.boolean("window-maximized"));
    let autohide_ui = has_images && settings.as_ref().is_none_or(|s| s.boolean("autohide-ui"));
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

    let title = adw::WindowTitle::new(&gettext("BigIris"), "");
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
    transfers::install(&window, &status, !has_images);

    // Stack between empty-state and the viewport so we don't flash a blank
    // area when the app is launched without files.
    let stack = gtk::Stack::new();
    stack.add_named(&status, Some("empty"));
    stack.add_named(&scroller, Some("picture"));
    // Stays on screen when the current file cannot be read, so a missing or
    // broken file never leaves a silent black window.
    let error_page = adw::StatusPage::builder()
        .icon_name("image-missing-symbolic")
        .title(gettext("Não foi possível abrir a imagem"))
        .build();
    stack.add_named(&error_page, Some("error"));

    // OSD pill (mpv-style): filename · index · zoom. Hidden by default,
    // pulses on nav/zoom. Crossfade in, slide-down on hide so it feels
    // ambient rather than chrome-y.
    let osd_label = gtk::Label::builder()
        .label("")
        .css_classes(["osd", "title-4"])
        // Long notices (dropped metadata after a save) wrap instead of
        // widening the pill past the window.
        .wrap(true)
        .max_width_chars(60)
        .justify(gtk::Justification::Center)
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
    let film_strip_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    film_strip_box.add_css_class("film-strip");
    // A rounded bar that hugs the thumbnails and scrolls when they do not
    // fit, centred like the OSD controls of GNOME's media apps.
    let film_strip_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_width(true)
        .overflow(gtk::Overflow::Hidden)
        .child(&film_strip_box)
        .build();
    film_strip_scroll.add_css_class("film-strip-bar");
    // A horizontal-only ScrolledWindow ignores vertical wheel steps, and
    // most mice only have those: map them onto the horizontal axis.
    // Capture phase: the ScrolledWindow's own controller would otherwise
    // claim the event first and drop the vertical part.
    let strip_wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    strip_wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let strip = film_strip_scroll.downgrade();
        strip_wheel.connect_scroll(move |controller, _dx, dy| {
            let Some(strip) = strip.upgrade() else { return glib::Propagation::Proceed };
            if dy == 0.0 {
                return glib::Propagation::Proceed;
            }
            let adjustment = strip.hadjustment();
            // Same step GTK uses for discrete wheel clicks.
            let step = if controller.unit() == gdk::ScrollUnit::Surface {
                dy
            } else {
                dy * adjustment.page_size().powf(2.0 / 3.0)
            };
            let max = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
            adjustment.set_value((adjustment.value() + step).clamp(adjustment.lower(), max));
            glib::Propagation::Stop
        });
    }
    film_strip_scroll.add_controller(strip_wheel);

    let film_strip_buttons = Vec::new();

    let strip_revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideUp)
        .transition_duration(220)
        .reveal_child(false)
        .valign(gtk::Align::End)
        .halign(gtk::Align::Center)
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(12)
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
        .width_request(276)
        .height_request(120)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .margin_top(12)
        .margin_end(12)
        .visible(false)
        .build();
    histogram_area.add_css_class("osd");
    histogram_area.add_css_class("histogram-overlay");

    let loading_spinner = adw::Spinner::builder()
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
    // One-pixel, nearly transparent copies of the neighbours. Drawing them
    // in the normal frame makes the renderer upload them to the GPU ahead
    // of time without blocking, so an arrow key only swaps textures.
    let warm_slots = [gtk::Image::new(), gtk::Image::new()];
    for slot in &warm_slots {
        slot.set_pixel_size(1);
        slot.set_halign(gtk::Align::Start);
        slot.set_valign(gtk::Align::Start);
        slot.set_opacity(0.01);
        slot.set_can_target(false);
        slot.set_accessible_role(gtk::AccessibleRole::Presentation);
        overlay.add_overlay(slot);
    }

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&overlay));
    // The first contact exposes the controls. It auto-hides shortly after
    // the image appears, while edge reveal keeps the canvas uncluttered.
    toolbar_view.set_reveal_top_bars(true);
    window.set_content(Some(&toolbar_view));

    // Images use a quiet canvas; the start page keeps normal desktop chrome.
    if has_images {
        window.add_css_class("theater");
    }
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
        film_strip_active: None,
        film_strip_box: film_strip_box.clone(),
        thumbnail_cancel: Arc::new(AtomicBool::new(false)),
        ui_pinned: false,
        autohide_ui,
        autohide_delay_ms,
        slideshow_interval_s,
        theater_enabled: has_images,
        top_hide_source: None,
        bottom_hide_source: None,
        osd_hide_source: None,
        cursor_hide_source: None,
        last_motion: std::time::Instant::now(),
        cursor_hidden: false,
        slideshow_source: None,
        histogram_area: histogram_area.clone(),
        zoom_level: {
            let label = gtk::Label::new(None);
            label.set_width_chars(5);
            label.add_css_class("numeric");
            label
        },
        stack: stack.clone(),
        error_page,
        warm_slots,
        histogram: [0u32; 768],
        loader: preload.unwrap_or_else(ViewerLoader::new),
        load_generation: u64::from(preloaded),
        prefetcher: ViewerLoader::new(),
        prefetching: None,
        frame_cache: Vec::new(),
        prefetch_skip: Vec::new(),
        current_ready: false,
        shown_path: None,
        awaiting_prefetch: false,
        first_frame_shown: false,
        deferred_thumbnails: None,
        loading_spinner: loading_spinner.clone(),
        props_dialog: None,
        pending_ops: Vec::new(),
        redo_ops: Vec::new(),
        saving: false,
    }));

    if let Some(settings) = settings.filter(|_| has_images) {
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
                s.prefetcher.stop();
                s.frame_cache.clear();
                for source in [
                    s.top_hide_source.take(),
                    s.bottom_hide_source.take(),
                    s.osd_hide_source.take(),
                    s.cursor_hide_source.take(),
                    s.slideshow_source.take(),
                ]
                .into_iter()
                .flatten()
                {
                    source.remove();
                }
                zoom::cancel_pending(&mut s);
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
    // Header layout follows GNOME's image viewer: navigation as one linked
    // pair on the left; zoom as one control that shows the current level,
    // then fullscreen, properties and the main menu on the right. Opening
    // files lives in the menu (Ctrl+O), so no two buttons share an icon.
    let has_files = !state.borrow().files.is_empty();
    let navigation = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    navigation.add_css_class("linked");
    navigation.set_visible(has_files);
    for (icon, tooltip, action) in [
        ("go-previous-symbolic", gettext("Imagem anterior"), "win.previous"),
        ("go-next-symbolic", gettext("Próxima imagem"), "win.next"),
    ] {
        let button = gtk::Button::from_icon_name(icon);
        button.set_tooltip_text(Some(&tooltip));
        button.set_action_name(Some(action));
        navigation.append(&button);
    }
    header.pack_start(&navigation);

    header.pack_end(&build_header_menu_button(&state));

    let info_btn = gtk::Button::from_icon_name("dialog-information-symbolic");
    info_btn.set_tooltip_text(Some(&gettext("Propriedades da imagem (Ctrl+I)")));
    info_btn.set_action_name(Some("win.info"));
    info_btn.set_visible(has_files);
    header.pack_end(&info_btn);

    let fullscreen_btn = gtk::Button::from_icon_name("view-fullscreen-symbolic");
    fullscreen_btn.set_tooltip_text(Some(&gettext("Tela cheia (F11)")));
    fullscreen_btn.set_action_name(Some("win.fullscreen"));
    fullscreen_btn.set_visible(has_files);
    header.pack_end(&fullscreen_btn);

    let zoom_group = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    zoom_group.add_css_class("linked");
    zoom_group.set_visible(has_files);
    let zoom_out_btn = gtk::Button::from_icon_name("zoom-out-symbolic");
    zoom_out_btn.set_tooltip_text(Some(&gettext("Afastar")));
    zoom_out_btn.set_action_name(Some("win.zoom-out"));
    zoom_group.append(&zoom_out_btn);
    let zoom_menu = gio::Menu::new();
    zoom_menu.append(Some(&gettext("Ajustar à janela")), Some("win.zoom-fit"));
    for (label, scale) in [(gettext("Tamanho real (100%)"), 1.0), ("200%".to_string(), 2.0)] {
        let item = gio::MenuItem::new(Some(&label), None);
        item.set_action_and_target_value(Some("win.zoom-to"), Some(&scale.to_variant()));
        zoom_menu.append_item(&item);
    }
    let zoom_level = state.borrow().zoom_level.clone();
    let zoom_btn = gtk::MenuButton::builder()
        .child(&zoom_level)
        .menu_model(&zoom_menu)
        .tooltip_text(gettext("Zoom"))
        .build();
    zoom_group.append(&zoom_btn);
    let zoom_in_btn = gtk::Button::from_icon_name("zoom-in-symbolic");
    zoom_in_btn.set_tooltip_text(Some(&gettext("Aproximar")));
    zoom_in_btn.set_action_name(Some("win.zoom-in"));
    zoom_group.append(&zoom_in_btn);
    header.pack_end(&zoom_group);
    {
        // The fitted level changes with the window size.
        let scroller = state.borrow().scroller.clone();
        let state = Rc::downgrade(&state);
        scroller.hadjustment().connect_changed(move |_| {
            if let Some(Ok(s)) = state.upgrade().as_ref().map(|state| state.try_borrow()) {
                s.zoom_level.set_label(&zoom_percent_label(&s));
            }
        });
    }

    // Decoded frames are applied when the worker signals one, without
    // blocking GTK or waking it on a timer. At most the newest valid
    // generation is painted, so fast directory navigation remains smooth.
    // The tasks hold the state weakly and end when the loaders stop.
    for prefetch in [false, true] {
        let wake = {
            let s = state.borrow();
            if prefetch {
                s.prefetcher.wake.clone()
            } else {
                s.loader.wake.clone()
            }
        };
        let weak = Rc::downgrade(&state);
        glib::MainContext::default().spawn_local(async move {
            while wake.wait().await {
                let Some(state) = weak.upgrade() else { break };
                if prefetch {
                    let frame = state.borrow().prefetcher.take_result();
                    if let Some(frame) = frame {
                        apply_prefetched_frame(&state, frame);
                    }
                } else {
                    let frame = state.borrow().loader.take_result();
                    if let Some(frame) = frame {
                        apply_decoded_frame(&state, frame);
                    }
                }
            }
        });
    }

    if has_files {
        stack.set_visible_child_name("picture");
        if preloaded {
            sync_edit_actions(&state);
            refresh_title(&state);
        } else {
            update_display(&state);
        }
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
            // Without an image (start page, error page) or with an in-window
            // dialog open, the wheel must scroll that content instead.
            {
                let s = state.borrow();
                if s.texture.is_none()
                    || s.stack.visible_child_name().as_deref() != Some("picture")
                    || s.window().visible_dialog().is_some()
                {
                    return glib::Propagation::Proceed;
                }
            }
            // Over the revealed film strip the wheel scrolls the strip; the
            // strip's own controller turns vertical steps into horizontal.
            let position = controller.current_event().and_then(|e| e.position());
            if let Some((wx, wy)) = position {
                let s = state.borrow();
                let over_strip = s
                    .window()
                    .pick(wx, wy, gtk::PickFlags::DEFAULT)
                    .is_some_and(|widget| widget.is_ancestor(&s.strip_revealer));
                if over_strip {
                    return glib::Propagation::Proceed;
                }
            }
            // The scroll controller lives on the window, so event.position()
            // is in window-root coords. Convert to scroller-local coords so
            // adjust_zoom can anchor the re-scroll on the pixel under the
            // pointer: root.compute_point(&scroller, pt) — NOT the reverse.
            let cursor = position.and_then(|(wx, wy)| {
                let root = scroller.root()?;
                let pt = graphene::Point::new(wx as f32, wy as f32);
                root.compute_point(&scroller, &pt).map(|p| (f64::from(p.x()), f64::from(p.y())))
            });
            let surface = controller.unit() == gdk::ScrollUnit::Surface;
            if let Some(factor) = zoom::scroll_factor(dy, surface) {
                adjust_zoom(&state, factor, cursor);
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
        let weak = Rc::downgrade(&state);
        drag.connect_drag_begin(move |_, _, _| {
            if let Some(state) = weak.upgrade() {
                zoom::cancel_pending(&mut state.borrow_mut());
                refresh_title(&state);
            }
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

/// The picture inside a film-strip cell.
fn thumb_picture(button: &gtk::Button) -> Option<gtk::Picture> {
    button.child().and_downcast::<gtk::Overlay>()?.last_child().and_downcast::<gtk::Picture>()
}

/// Decode a small film-strip frame off the GTK thread. A valid entry of the
/// shared freedesktop thumbnail cache is used when another application
/// already made one; otherwise the source codec may still need to read the
/// full image. Both go through `open_image`, i.e. the sandboxed decoder:
/// cache files are written by other programs and are untrusted too. Only
/// the downscaled RGBA buffer is handed to GDK/GPU memory.
fn decode_thumbnail(path: &Path) -> Result<(Vec<u8>, u32, u32), Box<dyn std::error::Error>> {
    let cached = viewer::cached_thumbnail(path).and_then(|entry| {
        bigimage_core::open_image(&entry)
            .inspect_err(|error| tracing::debug!(?entry, %error, "miniatura em cache ilegível"))
            .ok()
    });
    let img = match cached {
        Some(img) => img,
        None => bigimage_core::open_image(path)?,
    };
    // Cells crop to a square, so the short side must cover the cell; twice
    // the cell keeps thumbnails sharp on HiDPI screens. Never upscale and
    // cap the long side: extreme aspect ratios must not allocate gigabytes.
    let (width, height) = viewer::thumbnail_dimensions(img.width(), img.height(), THUMB_EDGE * 2);
    let thumb = bigimage_core::into_srgb_rgba8(img.thumbnail(width, height));
    let (w, h) = thumb.dimensions();
    Ok((thumb.into_raw(), w, h))
}

/// Only [`viewer::FILM_STRIP_PAGE`] widgets and thumbnail requests exist,
/// independently of folder size; the window is centred on the current file.
fn rebuild_film_strip(state: &Rc<RefCell<ViewerState>>) {
    const PAGE: usize = viewer::FILM_STRIP_PAGE;
    let (container, files, start, cancel) = {
        let mut s = state.borrow_mut();
        s.thumbnail_cancel.store(true, Ordering::Release);
        s.thumbnail_cancel = Arc::new(AtomicBool::new(false));
        s.film_strip_start = viewer::film_strip_start(s.files.len(), s.idx, PAGE);
        s.film_strip_active = None;
        let container = s.film_strip_box.clone();
        s.film_strip_buttons.clear();
        (container, s.files.clone(), s.film_strip_start, s.thumbnail_cancel.clone())
    };
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    let mut buttons = Vec::new();
    for (offset, path) in files.iter().skip(start).take(PAGE).enumerate() {
        // The cell's size comes from a fixed square; the picture is an overlay
        // on it, so wide or tall images are cropped instead of widening it.
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Cover);
        let cell = gtk::Overlay::new();
        let square = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        square.set_size_request(THUMB_EDGE as i32, THUMB_EDGE as i32);
        cell.set_child(Some(&square));
        cell.add_overlay(&picture);
        let button = gtk::Button::builder().child(&cell).overflow(gtk::Overflow::Hidden).build();
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
    // The image on screen is already decoded: its thumbnail reuses the
    // texture instead of decoding the same file a second time.
    let current = state.borrow().idx.checked_sub(start).filter(|offset| *offset < paths.len());
    let weak_state = Rc::downgrade(state);
    let begin = move || {
        // Navigation rebuilds the strip before the new frame is decoded, so
        // `texture` may still be the previous image: reuse it only when it
        // is known to be this file, otherwise decode the cell like the rest.
        // A frame that arrives later paints the cell itself (show_frame).
        let mut skip = None;
        if let (Some(state), Some(offset)) = (weak_state.upgrade(), current) {
            let s = state.borrow();
            let button = weak_buttons.get(offset).and_then(|w| w.upgrade());
            let matches = s.current_ready && s.shown_path.as_ref() == paths.get(offset);
            if let (true, Some(texture), Some(button)) = (matches, s.texture.as_ref(), button) {
                if let Some(picture) = thumb_picture(&button) {
                    picture.set_paintable(Some(texture));
                    skip = Some(offset);
                }
            }
        }
        start_thumbnail_decoding(paths, skip, cancel, weak_buttons);
    };
    let mut s = state.borrow_mut();
    if s.first_frame_shown {
        drop(s);
        begin();
    } else {
        s.deferred_thumbnails = Some(Box::new(begin));
    }
}

/// Paint the current file's strip cell with the frame just shown, so the
/// strip never keeps a thumbnail of what the file looked like before.
fn paint_current_thumb(s: &ViewerState, texture: &gdk::Texture) {
    let button = s.idx.checked_sub(s.film_strip_start).and_then(|o| s.film_strip_buttons.get(o));
    if let Some(picture) = button.and_then(thumb_picture) {
        picture.set_paintable(Some(texture));
    }
}

fn start_thumbnail_decoding(
    paths: Vec<PathBuf>,
    skip: Option<usize>,
    cancel: Arc<AtomicBool>,
    weak_buttons: Vec<glib::WeakRef<gtk::Button>>,
) {
    // One thumbnail at a time on GIO's worker pool, awaited from the GTK
    // loop: no timer runs while waiting. `cancel` is raised when the strip
    // is rebuilt or the window goes away.
    glib::MainContext::default().spawn_local(async move {
        for (index, path) in paths.into_iter().enumerate() {
            if cancel.load(Ordering::Acquire) {
                break;
            }
            if Some(index) == skip {
                continue;
            }
            let worker_cancel = cancel.clone();
            let decoded = gio::spawn_blocking(move || {
                crate::codec::with_cancel(worker_cancel, || decode_thumbnail(&path).ok())
            })
            .await;
            if cancel.load(Ordering::Acquire) {
                break;
            }
            let Ok(Some((raw, w, h))) = decoded else { continue };
            if let Some(button) = weak_buttons.get(index).and_then(|w| w.upgrade()) {
                if let Some(picture) = thumb_picture(&button) {
                    picture.set_paintable(Some(&texture_from_rgba(raw, w, h)));
                }
            }
        }
    });
}

/// Toggle `.active-thumb` so the film strip mirrors the current selection,
/// and scroll the active thumb into the middle when the selection changed.
/// Zoom steps and pointer motion call this often; when the index is the
/// same it returns at once, so the strip keeps any manual scroll position.
fn refresh_film_strip_active(state: &Rc<RefCell<ViewerState>>) {
    let needs_rebuild = {
        let s = state.borrow();
        !s.files.is_empty()
            && (s.idx < s.film_strip_start
                || s.idx >= s.film_strip_start + s.film_strip_buttons.len())
    };
    if needs_rebuild {
        rebuild_film_strip(state);
    }
    {
        let mut s = state.borrow_mut();
        if s.film_strip_active == Some(s.idx) {
            return;
        }
        s.film_strip_active = Some(s.idx);
        let active = s.idx.checked_sub(s.film_strip_start);
        for (i, btn) in s.film_strip_buttons.iter().enumerate() {
            if Some(i) == active {
                btn.add_css_class("active-thumb");
            } else {
                btn.remove_css_class("active-thumb");
            }
        }
    }
    if !centre_film_strip(state) {
        schedule_centre_film_strip(state);
    }
}

/// Centre once the strip has a layout: a freshly built or just revealed
/// strip reports zero widths until the next frame.
fn schedule_centre_film_strip(state: &Rc<RefCell<ViewerState>>) {
    let strip = state.borrow().film_strip_scroll.clone();
    let weak = Rc::downgrade(state);
    let frames = Cell::new(0u32);
    strip.add_tick_callback(move |_, _| {
        frames.set(frames.get() + 1);
        let done = weak.upgrade().is_none_or(|state| centre_film_strip(&state));
        if done || frames.get() > 30 {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

/// Scroll the active thumbnail to the middle of the strip; uses the
/// button's computed bounds (allocation() was deprecated in GTK 4.12).
/// Returns false while the strip is not allocated yet.
fn centre_film_strip(state: &Rc<RefCell<ViewerState>>) -> bool {
    let s = state.borrow();
    let Some(btn) = s.idx.checked_sub(s.film_strip_start).and_then(|i| s.film_strip_buttons.get(i))
    else {
        return false;
    };
    let strip = &s.film_strip_scroll;
    let viewport_w = strip.width() as f64;
    // compute_bounds returns coords in the parent (the HBox inside the
    // scroll's viewport); that's what hadjustment speaks.
    let Some(rect) = btn.parent().and_then(|parent| btn.compute_bounds(&parent)) else {
        return false;
    };
    if rect.width() <= 0.0 || viewport_w <= 0.0 {
        return false;
    }
    let target = rect.x() as f64 - (viewport_w - rect.width() as f64) / 2.0;
    strip.hadjustment().set_value(target.max(0.0));
    true
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
    if state.borrow().files.len() < 2 {
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
    format!("{} · {}/{} · {}", name, s.idx + 1, s.files.len(), zoom_percent_label(&s))
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
    // Restore the cursor and push its idle deadline back. The pending
    // timer, if any, notices the newer motion and re-arms itself.
    let armed = {
        let mut s = state.borrow_mut();
        s.last_motion = std::time::Instant::now();
        if s.cursor_hidden {
            s.cursor_hidden = false;
            s.window().set_cursor(None);
        }
        s.cursor_hide_source.is_some()
    };
    if !armed {
        schedule_cursor_hide(state);
    }

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
    let weak = Rc::downgrade(state);
    let id = glib::timeout_add_local_once(std::time::Duration::from_millis(delay), move || {
        if let Some(state) = weak.upgrade() {
            // This source is the one firing: forget it instead of removing it.
            state.borrow_mut().top_hide_source = None;
            hide_top(&state);
        }
    });
    state.borrow_mut().top_hide_source = Some(id);
}

fn hide_top(state: &Rc<RefCell<ViewerState>>) {
    let mut s = state.borrow_mut();
    s.top_hide_source = None;
    let Some(window) = s.window_weak.upgrade() else { return };
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
    let first_reveal = {
        let s = state.borrow();
        if s.files.len() < 2 {
            return;
        }
        let hidden = !s.strip_revealer.reveals_child();
        s.strip_revealer.set_reveal_child(true);
        hidden
    };
    // A faixa nasce rolada no começo. Como o visualizador agora pode abrir
    // no meio da pasta (clique num arquivo qualquer), a miniatura ativa
    // ficaria fora de vista na primeira vez que o strip aparece. Recentrar
    // fica para o idle seguinte porque, enquanto o revealer não alocou a
    // faixa, `compute_bounds` devolve largura zero e a conta de centragem
    // não teria em que se apoiar. Só na revelação: cada movimento do
    // ponteiro na borda de baixo passa por aqui, e recentrar a cada evento
    // impediria rolar a faixa à mão.
    if first_reveal {
        schedule_centre_film_strip(state);
    }
    cancel_bottom_hide(state);
    if state.borrow().ui_pinned || !state.borrow().autohide_ui {
        return;
    }
    let delay = state.borrow().autohide_delay_ms;
    let weak = Rc::downgrade(state);
    let id = glib::timeout_add_local_once(std::time::Duration::from_millis(delay), move || {
        if let Some(state) = weak.upgrade() {
            // This source is the one firing: forget it instead of removing it.
            state.borrow_mut().bottom_hide_source = None;
            hide_bottom(&state);
        }
    });
    state.borrow_mut().bottom_hide_source = Some(id);
}

fn hide_bottom(state: &Rc<RefCell<ViewerState>>) {
    // The F key hides the strip directly: drop the pending timer too, or it
    // would outlive a window closed right after and touch freed state.
    cancel_bottom_hide(state);
    let s = state.borrow();
    let Some(window) = s.window_weak.upgrade() else { return };
    let keyboard_in_strip = gtk::prelude::GtkWindowExt::focus(&window)
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
    pulse_osd_for(state, OSD_PULSE_MS);
}

/// [`pulse_osd`] for `millis`, for notices that take longer to read.
fn pulse_osd_for(state: &Rc<RefCell<ViewerState>>, millis: u64) {
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
    let weak = Rc::downgrade(state);
    let id = glib::timeout_add_local_once(std::time::Duration::from_millis(millis), move || {
        if let Some(state) = weak.upgrade() {
            state.borrow_mut().osd_hide_source = None;
            hide_osd(&state);
        }
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
    let delay = std::time::Duration::from_millis(CURSOR_HIDE_MS);
    arm_cursor_hide(state, delay);
}

fn arm_cursor_hide(state: &Rc<RefCell<ViewerState>>, delay: std::time::Duration) {
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
    let weak = Rc::downgrade(state);
    let id = glib::timeout_add_local_once(delay, move || {
        let Some(state) = weak.upgrade() else { return };
        let mut s = state.borrow_mut();
        s.cursor_hide_source = None;
        // The pointer moved since this timer was armed: wait the rest.
        let idle = s.last_motion.elapsed();
        let full = std::time::Duration::from_millis(CURSOR_HIDE_MS);
        if idle < full {
            drop(s);
            arm_cursor_hide(&state, full - idle);
            return;
        }
        // Double-check na hora do disparo: user pode ter aberto um
        // diálogo nos últimos CURSOR_HIDE_MS ms.
        let window = s.window();
        if window.visible_dialog().is_some() {
            return;
        }
        let cursor = gdk::Cursor::from_name("none", None);
        window.set_cursor(cursor.as_ref());
        s.cursor_hidden = true;
    });
    if let Some(old) = state.borrow_mut().cursor_hide_source.replace(id) {
        old.remove();
    }
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
        if s.files.len() >= 2 {
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
            if s.files.len() >= 2 {
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
            // Like GNOME's image viewer: Esc first leaves fullscreen and
            // only closes the window when it is not fullscreen.
            let window = state.borrow().window();
            if window.is_fullscreen() {
                window.unfullscreen();
            } else {
                window.close();
            }
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
    s.osd_label.set_text(
        &gettext("Slideshow: a cada {interval} s").replace("{interval}", &interval.to_string()),
    );
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
        s.osd_label.set_text(&if now_visible {
            gettext("Histograma ligado")
        } else {
            gettext("Histograma desligado")
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
    // The rounded OSD background comes from CSS; bars sit inside a margin
    // so they never touch the corners.
    const INSET: f64 = 8.0;
    cr.translate(INSET, INSET);
    let w = (w as f64 - 2.0 * INSET).max(1.0);
    let h = (h as f64 - 2.0 * INSET).max(1.0);

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
    let inner_h = h;
    // Canais em ordem B, G, R — fica mais parecido com Photoshop (R mais
    // destacado no topo dos overlaps aditivos).
    let channels: [(usize, (f64, f64, f64)); 3] =
        [(2, (0.20, 0.55, 1.00)), (1, (0.30, 0.90, 0.40)), (0, (1.00, 0.30, 0.30))];
    for (idx, (r, g, b)) in channels {
        cr.set_source_rgba(r, g, b, 0.75);
        for (i, bucket) in bars.iter().enumerate() {
            let v = bucket[idx] as f64 / max_per_channel[idx] as f64;
            let bar_h = v * inner_h;
            cr.rectangle(i as f64, h - bar_h, 1.0, bar_h);
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
    // Trashing during a save would let the save's final rename bring the
    // file back from the trash.
    for name in ["rotate-cw", "rotate-ccw", "rotate-180", "flip-h", "flip-v", "trash"] {
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
    // Known before asking for confirmation: no point in a dialog that can
    // only end in a failure.
    if save_format(&path).is_none() {
        present_save_failure(state, &SaveFailure::NoEncoder);
        return;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let is_jpeg = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| matches!(s.to_ascii_lowercase().as_str(), "jpg" | "jpeg"))
        .unwrap_or(false);
    let mut body = if is_jpeg {
        format!("\"{name}\"\n\n{}", gettext("O arquivo será sobrescrito. JPEG é recodificado a cada gravação; isso pode reduzir a qualidade."))
    } else {
        format!("\"{name}\"\n\n{}", gettext("O arquivo será sobrescrito."))
    };
    let keeps_metadata = save_format(&path)
        .is_some_and(|format| bigimage_core::metadata::preserved_by(format).is_complete());
    if !keeps_metadata {
        body.push_str("\n\n");
        body.push_str(&gettext("Este formato não guarda todos os metadados da foto (data, câmera, perfil de cor); o que ele não suportar será removido ou convertido."));
    }
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
        }
        sync_edit_actions(&state);

        let worker_path = path.clone();
        let worker_ops = ops.clone();
        let job = gio::spawn_blocking(move || write_edits_to_disk(&worker_path, &worker_ops));

        // Awaited on the GTK loop instead of polled on a timer.
        let state = state.clone();
        let path = path.clone();
        glib::MainContext::default().spawn_local(async move {
            // A panicking encoder ends the save as a failure.
            let result = job
                .await
                .unwrap_or_else(|_| Err(SaveFailure::Other("save worker panicked".to_string())));
            let dropped = result.as_ref().ok().cloned().unwrap_or_default();
            {
                let mut s = state.borrow_mut();
                s.saving = false;
                s.loading_spinner.set_visible(false);
                if result.is_ok() {
                    s.pending_ops.clear();
                    s.redo_ops.clear();
                }
            }
            refresh_title(&state);
            sync_edit_actions(&state);
            match result {
                Ok(_) => {
                    adopt_saved_frame(&state, &path);
                    match metadata_loss_notice(&dropped) {
                        Some(notice) => {
                            tracing::info!(?path, ?dropped, "save: metadados descartados");
                            state.borrow().osd_label.set_text(&notice);
                            pulse_osd_for(&state, OSD_NOTICE_MS);
                        }
                        None => {
                            state.borrow().osd_label.set_text(&gettext("Salvo"));
                            pulse_osd(&state);
                        }
                    }
                }
                Err(failure) => {
                    tracing::error!(?path, ?failure, "save: falha ao gravar edições");
                    present_save_failure(&state, &failure);
                }
            }
        });
    });
    alert.present(Some(&window));
}

/// The texture on screen is now what the file holds: cache it under the
/// new modification time, show it in the film strip and resume the
/// neighbour prefetch that pending edits had paused.
fn adopt_saved_frame(state: &Rc<RefCell<ViewerState>>, path: &Path) {
    {
        let mut s = state.borrow_mut();
        let current = s.files.get(s.idx).is_some_and(|p| p == path);
        if let (true, Some(texture)) = (current, s.texture.clone()) {
            s.shown_path = Some(path.to_path_buf());
            paint_current_thumb(&s, &texture);
            let frame = CachedFrame {
                path: path.to_path_buf(),
                modified: file_modified(path),
                texture,
                natural: s.natural,
                histogram: s.histogram,
            };
            remember_frame(&mut s, frame);
        }
    }
    schedule_prefetch(state);
}

/// Why an in-place save did not happen, in terms the user can act on.
#[derive(Debug)]
enum SaveFailure {
    /// Animated or multi-page file (or one that cannot be inspected):
    /// re-encoding a single frame would destroy the rest.
    NotStatic,
    /// No encoder for this extension.
    NoEncoder,
    PermissionDenied,
    Other(String),
}

impl SaveFailure {
    fn from_core(error: bigimage_core::BigImageError) -> Self {
        match &error {
            bigimage_core::BigImageError::Io(io)
                if io.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                Self::PermissionDenied
            }
            _ => Self::Other(error.to_string()),
        }
    }

    fn from_image(error: image::ImageError) -> Self {
        match error {
            image::ImageError::IoError(io) => Self::from_io(io),
            other => Self::Other(other.to_string()),
        }
    }

    fn from_io(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            Self::PermissionDenied
        } else {
            Self::Other(error.to_string())
        }
    }

    fn message(&self) -> String {
        match self {
            Self::NotStatic => gettext("Este arquivo é animado, tem várias páginas ou não pôde ser verificado. Girar ou espelhar e salvar descartaria os outros quadros, então ele não foi alterado."),
            Self::NoEncoder => gettext("O BigIris não sabe gravar este formato. As alterações não podem ser salvas no próprio arquivo."),
            Self::PermissionDenied => gettext("Sem permissão para alterar este arquivo ou a pasta dele."),
            Self::Other(detail) => fill(
                &gettext("O arquivo não foi alterado. Detalhes: {detail}"),
                &[("detail", detail)],
            ),
        }
    }
}

/// Explain a failed save and offer a way out: navigation stays blocked
/// while edits are pending, so the user can discard them from here.
fn present_save_failure(state: &Rc<RefCell<ViewerState>>, failure: &SaveFailure) {
    let window = state.borrow().window();
    let alert = adw::AlertDialog::builder()
        .heading(gettext("Não foi possível salvar"))
        .body(failure.message())
        .build();
    alert.add_response("keep", &gettext("Manter alterações"));
    alert.add_response("discard", &gettext("Descartar alterações"));
    alert.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
    alert.set_default_response(Some("keep"));
    alert.set_close_response("keep");
    let weak = Rc::downgrade(state);
    alert.connect_response(Some("discard"), move |_, _| {
        if let Some(state) = weak.upgrade() {
            discard_edits(&state);
        }
    });
    alert.present(Some(&window));
}

/// Drop every pending edit and show the file as it is on disk.
fn discard_edits(state: &Rc<RefCell<ViewerState>>) {
    let path = {
        let mut s = state.borrow_mut();
        if s.saving || s.files.is_empty() {
            return;
        }
        s.pending_ops.clear();
        s.redo_ops.clear();
        s.files[s.idx].clone()
    };
    rebuild_edited_texture(state, &path);
    refresh_title(state);
    sync_edit_actions(state);
}

/// Grava em arquivo temporário no mesmo diretório, sincroniza e só então
/// renomeia por cima do destino. Assim, queda de energia ou falha do encoder
/// deixa o original intacto. Permissões existentes são preservadas, assim
/// como EXIF (com orientação zerada) e o perfil de cor original quando o
/// formato os suporta — as edições são só geométricas. JPEG é regravado na
/// qualidade estimada do original (não no padrão 75), para que cada Ctrl+S
/// não degrade a foto. Devolve os metadados que não puderam ser mantidos.
fn write_edits_to_disk(
    path: &Path,
    ops: &[EditOp],
) -> Result<Vec<bigimage_core::MetadataLoss>, SaveFailure> {
    let target = if path.symlink_metadata().map_err(SaveFailure::from_io)?.file_type().is_symlink()
    {
        path.canonicalize().map_err(SaveFailure::from_io)?
    } else {
        path.to_path_buf()
    };
    let format = save_format(&target).ok_or(SaveFailure::NoEncoder)?;
    // The confined decoder reports animation as a generic decode error,
    // so any failure here is presented as "not a static image".
    bigimage_core::content::ensure_static(&target).map_err(|error| {
        tracing::warn!(%error, "save: arquivo não é estático");
        SaveFailure::NotStatic
    })?;
    let (img, source, quality) =
        crate::codec::open_for_resave(&target).map_err(SaveFailure::from_image)?;
    let edited = ops.iter().fold(img, |acc, op| apply_op_to_image(acc, *op));
    let report = bigimage_core::save_image_preserving_metadata_report(
        edited,
        target,
        format,
        &resave_options(format, quality),
        OverwritePolicy::Replace,
        &source,
    )
    .map_err(SaveFailure::from_core)?;
    Ok(report.dropped)
}

/// Encoder options for an in-place save: a JPEG keeps the quality its
/// source was estimated at; everything else uses the defaults.
fn resave_options(format: Format, source_quality: Option<u8>) -> EncodeOptions {
    let default = EncodeOptions::default();
    match format {
        Format::Jpeg => EncodeOptions { quality: source_quality.or(default.quality), ..default },
        _ => default,
    }
}

/// OSD notice after a save that could not keep some source metadata, or
/// `None` when everything survived.
fn metadata_loss_notice(dropped: &[bigimage_core::MetadataLoss]) -> Option<String> {
    use bigimage_core::MetadataLoss;
    let mut reasons: Vec<String> = Vec::new();
    for loss in dropped {
        let reason = match loss {
            MetadataLoss::ExifUnsupported => gettext("o formato não guarda EXIF"),
            MetadataLoss::ExifTooLarge => gettext("EXIF grande demais para JPEG"),
            MetadataLoss::ExifInvalid => gettext("EXIF inválido"),
            MetadataLoss::IccUnsupported
            | MetadataLoss::IccTooLarge
            | MetadataLoss::IccMismatched => gettext("perfil de cor convertido para sRGB"),
            MetadataLoss::IccDiscarded => gettext("perfil de cor inválido ignorado"),
            _ => gettext("metadados não suportados"),
        };
        if !reasons.contains(&reason) {
            reasons.push(reason);
        }
    }
    if reasons.is_empty() {
        return None;
    }
    Some(fill(
        &gettext("Salvo. Alguns metadados não puderam ser mantidos: {list}"),
        &[("list", &reasons.join("; "))],
    ))
}

/// Encoder for an in-place save of `path`, chosen by its extension.
fn save_format(path: &Path) -> Option<Format> {
    image::ImageFormat::from_path(path).ok().and_then(Format::from_image_format)
}

/// Zoom level as the user sees it. Fitting shows the actual percentage,
/// like GNOME's image viewer, instead of a mode name.
fn zoom_percent_label(s: &ViewerState) -> String {
    if s.texture.is_none() {
        return "—".to_string();
    }
    let viewport = (
        s.scroller.hadjustment().page_size().max(f64::from(s.scroller.width())) as i32,
        s.scroller.vadjustment().page_size().max(f64::from(s.scroller.height())) as i32,
    );
    let scale = effective_scale(zoom::requested_zoom(s), s.natural, viewport);
    format!("{}%", (scale * 100.0).round() as i32)
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
    if !show_cached_frame(state, &path) {
        let in_flight = state.borrow().prefetching.as_ref() == Some(&path);
        if in_flight {
            let mut s = state.borrow_mut();
            zoom::cancel_pending(&mut s);
            // Drop any older decode still running for the previous file.
            s.load_generation = s.load_generation.wrapping_add(1);
            s.current_ready = false;
            s.awaiting_prefetch = true;
        } else {
            request_frame(state, path, Vec::new());
        }
    }
    refresh_title(state);
}

/// Show `path` from the prefetch cache. Returns false when it is not
/// there or the file changed on disk since it was decoded.
fn show_cached_frame(state: &Rc<RefCell<ViewerState>>, path: &Path) -> bool {
    let modified = file_modified(path);
    {
        let mut s = state.borrow_mut();
        let Some(pos) = s.frame_cache.iter().position(|frame| frame.path == path) else {
            return false;
        };
        if modified.is_none() || s.frame_cache[pos].modified != modified {
            s.frame_cache.remove(pos);
            return false;
        }
        let frame = s.frame_cache[pos].clone();
        zoom::cancel_pending(&mut s);
        s.load_generation = s.load_generation.wrapping_add(1);
        s.awaiting_prefetch = false;
        s.current_ready = true;
        show_frame(&mut s, frame.texture, frame.natural, frame.histogram, Some(path));
    }
    schedule_prefetch(state);
    true
}

/// `clean` names the file when `texture` holds its unedited pixels; edited
/// previews pass `None` so the film strip never shows unsaved edits.
fn show_frame(
    s: &mut ViewerState,
    texture: gdk::Texture,
    natural: (u32, u32),
    histogram: Histogram,
    clean: Option<&Path>,
) {
    s.natural = natural;
    s.histogram = histogram;
    s.shown_path = clean.map(Path::to_path_buf);
    if clean.is_some() {
        paint_current_thumb(s, &texture);
    }
    s.texture = Some(texture);
    s.stack.set_visible_child_name("picture");
    apply_zoom(s);
    s.zoom_level.set_label(&zoom_percent_label(s));
    s.histogram_area.queue_draw();
}

/// Keep the cached neighbours drawn in the invisible warm-up slots.
fn refresh_warm_slots(s: &ViewerState) {
    let neighbours = neighbour_paths(s);
    for (i, slot) in s.warm_slots.iter().enumerate() {
        let texture = neighbours
            .get(i)
            .and_then(|path| s.frame_cache.iter().find(|frame| &frame.path == path))
            .map(|frame| frame.texture.clone());
        slot.set_paintable(texture.as_ref());
    }
}

fn remember_frame(s: &mut ViewerState, frame: CachedFrame) {
    if u64::from(frame.natural.0) * u64::from(frame.natural.1) > PREFETCH_MAX_PIXELS {
        return;
    }
    s.frame_cache.retain(|cached| cached.path != frame.path);
    s.frame_cache.push(frame);
}

/// Previous and next files, nearest first, without the current one.
fn neighbour_paths(s: &ViewerState) -> Vec<PathBuf> {
    let total = s.files.len();
    if total < 2 {
        return Vec::new();
    }
    let next = (s.idx + 1) % total;
    let previous = (s.idx + total - 1) % total;
    let mut paths = vec![s.files[next].clone()];
    if previous != next {
        paths.push(s.files[previous].clone());
    }
    paths
}

/// Decode the next missing neighbour, one at a time, once the current
/// image is on screen. Frames that are no longer neighbours are freed.
fn schedule_prefetch(state: &Rc<RefCell<ViewerState>>) {
    let mut s = state.borrow_mut();
    let wanted = neighbour_paths(&s);
    let current = s.files.get(s.idx).cloned();
    s.frame_cache
        .retain(|frame| current.as_ref() == Some(&frame.path) || wanted.contains(&frame.path));
    refresh_warm_slots(&s);
    if !s.current_ready || s.prefetching.is_some() || !s.pending_ops.is_empty() {
        return;
    }
    let missing = wanted.into_iter().find(|path| {
        !s.frame_cache.iter().any(|frame| &frame.path == path) && !s.prefetch_skip.contains(path)
    });
    if let Some(path) = missing {
        s.prefetching = Some(path.clone());
        s.prefetcher.request(LoadRequest {
            generation: 0,
            path,
            ops: Vec::new(),
            max_pixels: Some(PREFETCH_MAX_PIXELS),
        });
    }
}

fn apply_prefetched_frame(state: &Rc<RefCell<ViewerState>>, frame: DecodedFrame) {
    let fallback = {
        let mut s = state.borrow_mut();
        s.prefetching = None;
        let is_current = s.awaiting_prefetch
            && s.pending_ops.is_empty()
            && s.files.get(s.idx).is_some_and(|path| path == &frame.path);
        match frame.pixels {
            Ok((raw, w, h, histogram)) => {
                let too_large = u64::from(w) * u64::from(h) > PREFETCH_MAX_PIXELS;
                if too_large {
                    s.prefetch_skip.push(frame.path.clone());
                }
                if is_current || (!too_large && neighbour_paths(&s).contains(&frame.path)) {
                    let texture = texture_from_rgba(raw, w, h);
                    remember_frame(
                        &mut s,
                        CachedFrame {
                            path: frame.path.clone(),
                            modified: frame.modified,
                            texture: texture.clone(),
                            natural: (w, h),
                            histogram,
                        },
                    );
                    if is_current {
                        zoom::cancel_pending(&mut s);
                        s.awaiting_prefetch = false;
                        s.current_ready = true;
                        show_frame(&mut s, texture, (w, h), histogram, Some(&frame.path));
                    }
                }
                None
            }
            // Let the regular loader report the error for the current file.
            Err(_) if is_current => {
                s.awaiting_prefetch = false;
                Some(frame.path)
            }
            Err(_) => {
                s.prefetch_skip.push(frame.path);
                None
            }
        }
    };
    match fallback {
        Some(path) => request_frame(state, path, Vec::new()),
        None => schedule_prefetch(state),
    }
}

/// Splice the sibling list found in the background into a viewer that was
/// opened with only the requested file. The visible image stays as it is.
fn adopt_folder(state: &Rc<RefCell<ViewerState>>, mut files: Vec<PathBuf>, index: usize) {
    {
        let mut s = state.borrow_mut();
        if files.len() < 2 || s.files.len() != 1 || index >= files.len() {
            return;
        }
        // Keep the exact path of the frame already requested, so a decode
        // still in flight is not discarded as stale.
        files[index] = s.files[0].clone();
        s.files = Rc::new(files);
        s.idx = index;
        if app_settings().is_none_or(|settings| settings.boolean("filmstrip-visible")) {
            s.strip_revealer.set_visible(true);
            s.strip_revealer.set_can_target(true);
        }
    }
    let window = state.borrow().window();
    for name in ["previous", "next"] {
        if let Some(action) = window.lookup_action(name).and_downcast::<gio::SimpleAction>() {
            action.set_enabled(true);
        }
    }
    rebuild_film_strip(state);
    refresh_title(state);
    refresh_film_strip_active(state);
    schedule_prefetch(state);
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
    s.current_ready = false;
    s.awaiting_prefetch = false;
    s.loader.request(LoadRequest { generation: s.load_generation, path, ops, max_pixels: None });
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
        s.loading_spinner.set_visible(false);
    }
    if !s.first_frame_shown {
        s.first_frame_shown = true;
        if let Some(begin) = s.deferred_thumbnails.take() {
            glib::idle_add_local_once(begin);
        }
    }
    zoom::cancel_pending(&mut s);
    s.current_ready = true;
    match frame.pixels {
        Ok((raw, w, h, histogram)) => {
            let texture = texture_from_rgba(raw, w, h);
            if !frame.edited {
                remember_frame(
                    &mut s,
                    CachedFrame {
                        path: frame.path.clone(),
                        modified: frame.modified,
                        texture: texture.clone(),
                        natural: (w, h),
                        histogram,
                    },
                );
            }
            let clean = (!frame.edited).then_some(frame.path.as_path());
            show_frame(&mut s, texture, (w, h), histogram, clean);
            drop(s);
        }
        Err(error) => {
            tracing::error!(path = ?frame.path, %error, "falha ao carregar imagem");
            s.texture = None;
            s.shown_path = None;
            s.picture.set_paintable(gdk::Paintable::NONE);
            let name = frame.path.file_name().map_or_else(
                || frame.path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            s.error_page.set_description(Some(&glib::markup_escape_text(&name)));
            s.stack.set_visible_child_name("error");
            s.zoom_level.set_label(&zoom_percent_label(&s));
            drop(s);
        }
    }
    schedule_prefetch(state);
}

/// Constrói um menu curto para as funções menos frequentes. A navegação e
/// abertura ficam visíveis na barra, enquanto ferramentas robustas aparecem
/// sob demanda — o equilíbrio "simples por padrão, poderoso quando preciso".
/// Ações ficam em `win.<nome>` (registradas em [`install_viewer_actions`]).
fn build_header_menu_button(state: &Rc<RefCell<ViewerState>>) -> gtk::MenuButton {
    let menu = gio::Menu::new();
    let has_images = !state.borrow().files.is_empty();
    let files = gio::Menu::new();
    files.append(Some(&gettext("Abrir imagens…")), Some("win.open"));
    files.append(Some(&gettext("Colar arquivos de imagem")), Some("win.paste"));
    if has_images {
        files.append(Some(&gettext("Copiar imagem")), Some("win.copy"));
        files.append(Some(&gettext("Imprimir…")), Some("win.print"));
    } else {
        files.append(Some(&gettext("Processar uma pasta…")), Some("win.batch-folder"));
    }
    menu.append_section(None, &files);
    if has_images {
        let edit = gio::Menu::new();
        edit.append(Some(&gettext("Desfazer")), Some("win.undo"));
        edit.append(Some(&gettext("Refazer")), Some("win.redo"));
        let transforms = gio::Menu::new();
        transforms.append(Some(&gettext("Girar à direita")), Some("win.rotate-cw"));
        transforms.append(Some(&gettext("Girar à esquerda")), Some("win.rotate-ccw"));
        transforms.append(Some(&gettext("Girar 180°")), Some("win.rotate-180"));
        transforms.append(Some(&gettext("Espelhar horizontal")), Some("win.flip-h"));
        transforms.append(Some(&gettext("Espelhar vertical")), Some("win.flip-v"));
        transforms.append(Some(&gettext("Recortar…")), Some("win.crop"));
        edit.append_section(None, &transforms);
        menu.append_submenu(Some(&gettext("Editar imagem")), &edit);
        let tools = gio::Menu::new();
        tools.append(Some(&gettext("Remover fundo (IA)…")), Some("win.remove-bg"));
        tools.append(Some(&gettext("Aumentar resolução…")), Some("win.upscale"));
        menu.append_submenu(Some(&gettext("Ferramentas")), &tools);
        let changes = gio::Menu::new();
        changes.append(Some(&gettext("Salvar alterações")), Some("win.save-edits"));
        changes.append(Some(&gettext("Mover para a lixeira")), Some("win.trash"));
        menu.append_section(None, &changes);
    }
    let help = gio::Menu::new();
    help.append(Some(&gettext("Atalhos do teclado")), Some("win.shortcuts"));
    help.append(Some(&gettext("Sobre o BigIris")), Some("app.about"));
    menu.append_section(None, &help);

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

    let batch_folder = gio::SimpleAction::new("batch-folder", None);
    {
        let window = window.downgrade();
        batch_folder.connect_activate(move |_, _| {
            if let Some(window) = window.upgrade() {
                open_batch_folder_dialog(&window);
            }
        });
    }
    window.add_action(&batch_folder);

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
    let zoom_to = gio::SimpleAction::new("zoom-to", Some(glib::VariantTy::DOUBLE));
    zoom_to.set_enabled(has_files);
    {
        let state = Rc::downgrade(state);
        zoom_to.connect_activate(move |_, scale| {
            let (Some(state), Some(scale)) = (state.upgrade(), scale.and_then(|v| v.get::<f64>()))
            else {
                return;
            };
            set_zoom(&state, ZoomState::Scale(scale));
            pulse_osd(&state);
        });
    }
    window.add_action(&zoom_to);

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
            let Some(path) = path_for_external_tool(&state) else { return };
            // `--` keeps a file named like `-x.png` from being read as a flag.
            let _ = transfers::spawn_bigiris([
                std::ffi::OsStr::new("--dialog=crop"),
                std::ffi::OsStr::new("--"),
                path.as_os_str(),
            ]);
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
            let Some(path) = path_for_external_tool(&state) else { return };
            let flag = std::ffi::OsString::from(format!("--dialog={dialog}"));
            let _ = transfers::spawn_bigiris([
                flag.as_os_str(),
                std::ffi::OsStr::new("--"),
                path.as_os_str(),
            ]);
        });
        window.add_action(&action);
    }

    // win.trash — confirma e joga o arquivo na lixeira do XDG, depois mostra
    // a imagem vizinha. Desligada enquanto um salvamento está em curso.
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

    // Keyboard help uses a bounded, scrollable in-window dialog.
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

/// File for the crop / remove-background / upscale dialogs. They run in
/// another process and read the file from disk, so unsaved in-memory edits
/// would silently be ignored, and a save in progress may replace the file
/// under them: ask the user to save or undo first, like navigation does.
fn path_for_external_tool(state: &Rc<RefCell<ViewerState>>) -> Option<PathBuf> {
    let message = {
        let s = state.borrow();
        if s.files.is_empty() {
            return None;
        }
        if s.saving {
            gettext("Aguarde o salvamento terminar")
        } else if !s.pending_ops.is_empty() {
            gettext("Salve ou desfaça as alterações antes de usar esta ferramenta")
        } else {
            return Some(s.files[s.idx].clone());
        }
    };
    state.borrow().osd_label.set_text(&message);
    pulse_osd(state);
    None
}

/// Apresenta um FileDialog de seleção múltipla com filtro de imagens.
/// Abre cada seleção numa nova janela via `std::process::Command` pra
/// preservar o histórico da janela atual (e porque a `files` no state
/// é Rc<Vec<_>> imutável).
fn open_file_chooser(window: &adw::ApplicationWindow) {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some(&gettext("Imagens")));
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
        let paths: Vec<std::ffi::OsString> = (0..list.n_items())
            .filter_map(|i| list.item(i).and_downcast::<gio::File>()?.path())
            .map(PathBuf::into_os_string)
            .collect();
        if paths.is_empty() {
            return;
        }
        // "--" keeps a file named like an option from being parsed as one.
        let _ = transfers::spawn_bigiris(std::iter::once("--".into()).chain(paths));
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
    let (window, path, edited) = {
        let s = state.borrow();
        if s.files.is_empty() {
            return;
        }
        // With unsaved rotate/flip the page must match the screen, not the
        // file on disk: print the edited texture already in memory.
        let edited = s.texture.clone().filter(|_| !s.pending_ops.is_empty());
        (s.window().clone(), s.files[s.idx].clone(), edited)
    };
    if let Some(texture) = edited {
        match cairo_surface_from_texture(&texture) {
            Some(surface) => present_print_dialog(&window, &path, surface),
            None => show_print_error(&window, &gettext("Não foi possível preparar a página")),
        }
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let closing = cancel.clone();
    let closing_handler = window.connect_unrealize(move |_| closing.store(true, Ordering::Release));
    let input = path.clone();
    window.set_cursor_from_name(Some("wait"));
    let job = gio::spawn_blocking(move || {
        crate::codec::with_cancel(cancel, || {
            bigimage_core::open_image(input)
                .map(|image| {
                    let rgba = bigimage_core::into_srgb_rgba8(image);
                    let size = rgba.dimensions();
                    (premultiply_for_cairo(rgba.into_raw()), size)
                })
                .map_err(|e| e.to_string())
        })
    });
    let weak = window.downgrade();
    glib::MainContext::default().spawn_local(async move {
        let result = job.await.unwrap_or_else(|_| Err(gettext("O preparo da impressão falhou")));
        let Some(window) = weak.upgrade() else { return };
        if !window.is_visible() {
            return;
        }
        window.disconnect(closing_handler);
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
                    show_print_error(&window, &gettext("Não foi possível preparar a página"));
                }
            }
            Err(error) => show_print_error(&window, &error),
        }
    });
}

/// GDK's default download format is premultiplied BGRA/ARGB in native
/// order, i.e. exactly cairo's ARGB32.
fn cairo_surface_from_texture(texture: &gdk::Texture) -> Option<gtk::cairo::ImageSurface> {
    let (width, height) = (texture.width(), texture.height());
    let stride = gtk::cairo::Format::ARgb32.stride_for_width(u32::try_from(width).ok()?).ok()?;
    let mut pixels = vec![0u8; usize::try_from(stride).ok()? * usize::try_from(height).ok()?];
    texture.download(&mut pixels, usize::try_from(stride).ok()?);
    gtk::cairo::ImageSurface::create_for_data(
        pixels,
        gtk::cairo::Format::ARgb32,
        width,
        height,
        stride,
    )
    .ok()
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
    let (window, path) = {
        let s = state.borrow();
        if s.files.is_empty() {
            return;
        }
        (s.window().clone(), s.files[s.idx].clone())
    };
    // Se já existe uma janela aberta para esta imagem, traz pra frente em
    // vez de empilhar. Depois de navegar, ela descreveria a imagem errada:
    // fecha e monta uma nova.
    // An in-place save keeps the path but changes the file, so the
    // modification time is compared too.
    let modified = file_modified(&path);
    let existing = state.borrow_mut().props_dialog.take();
    if let Some((dialog, shown, shown_modified)) = existing {
        if shown == path && shown_modified == modified {
            dialog.present();
            state.borrow_mut().props_dialog = Some((dialog, shown, shown_modified));
            return;
        }
        dialog.destroy();
    }
    let Some(app) = window.application().and_downcast::<adw::Application>() else { return };
    let dialog = build_properties_window(&app, &path);
    // Attached to the viewer: GTK then drops minimize/maximize, as for any
    // dialog, and the window closes with its parent.
    dialog.set_transient_for(Some(&window));
    dialog.set_destroy_with_parent(true);
    // Quando o user fecha, soltamos a referência — próximo clique abre fresco.
    {
        let state = Rc::downgrade(state);
        dialog.connect_close_request(move |closing| {
            if let Some(state) = state.upgrade() {
                let mut s = state.borrow_mut();
                if s.props_dialog.as_ref().is_some_and(|(open, ..)| open == closing) {
                    s.props_dialog = None;
                }
            }
            glib::Propagation::Proceed
        });
    }
    state.borrow_mut().props_dialog = Some((dialog.clone(), path, modified));
    dialog.present();
}

/// Confirma com AdwAlertDialog e move o arquivo corrente pra lixeira
/// via `gio::File::trash`. Depois segue para a próxima imagem da pasta
/// (ou a anterior, se era a última); só fecha quando não sobra nenhuma.
fn trash_current(state: &Rc<RefCell<ViewerState>>) {
    let (window, path) = {
        let s = state.borrow();
        if s.files.is_empty() || s.saving {
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
            "\"{name}\"\n\n{}",
            gettext(
                "O arquivo será movido para a lixeira do sistema. Você pode restaurá-lo depois."
            )
        ))
        .build();
    alert.add_response("cancel", &gettext("Cancelar"));
    alert.add_response("trash", &gettext("Mover"));
    alert.set_response_appearance("trash", adw::ResponseAppearance::Destructive);
    alert.set_default_response(Some("cancel"));
    alert.set_close_response("cancel");

    let weak = Rc::downgrade(state);
    alert.connect_response(Some("trash"), move |_, _| {
        let Some(state) = weak.upgrade() else { return };
        // A save may have started while the dialog was open.
        if state.borrow().saving {
            return;
        }
        let file = gio::File::for_path(&path);
        match file.trash(gio::Cancellable::NONE) {
            Ok(()) => {
                tracing::info!(?path, "trash ok");
                remove_current_file(&state, &path);
            }
            Err(error) => {
                tracing::warn!(?path, %error, "trash falhou");
                let window = state.borrow().window();
                let alert = adw::AlertDialog::builder()
                    .heading(gettext("Não foi possível mover para a lixeira"))
                    .body(format!("\"{name}\"\n\n{}", error.message()))
                    .build();
                alert.add_response("close", &gettext("Fechar"));
                alert.present(Some(&window));
            }
        }
    });
    alert.present(Some(&window));
}

/// Drop `path` (the file on screen) from the list after it went to the
/// trash and show its neighbour: the next one, or the previous when it was
/// the last. The window closes only when the list becomes empty.
fn remove_current_file(state: &Rc<RefCell<ViewerState>>, path: &Path) {
    let remaining = {
        let mut s = state.borrow_mut();
        let Some(position) = s.files.iter().position(|p| p == path) else { return };
        let mut files = s.files.as_ref().clone();
        files.remove(position);
        // The edits belonged to the file that is gone.
        s.pending_ops.clear();
        s.redo_ops.clear();
        s.frame_cache.retain(|frame| frame.path != path);
        s.prefetch_skip.retain(|p| p != path);
        s.idx = position.min(files.len().saturating_sub(1));
        s.zoom = ZoomState::Fit;
        s.files = Rc::new(files);
        s.files.len()
    };
    if remaining == 0 {
        let window = state.borrow().window();
        window.close();
        return;
    }
    if remaining < 2 {
        let s = state.borrow();
        s.strip_revealer.set_reveal_child(false);
        s.strip_revealer.set_visible(false);
        s.strip_revealer.set_can_target(false);
        let window = s.window();
        drop(s);
        for name in ["previous", "next"] {
            if let Some(action) = window.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                action.set_enabled(false);
            }
        }
        if let Some(id) = state.borrow_mut().slideshow_source.take() {
            id.remove();
        }
    }
    rebuild_film_strip(state);
    update_display(state);
    pulse_osd(state);
}

/// Compact keyboard reference using widgets supported by GTK 4.12 / Adwaita 1.5.
fn present_shortcuts_dialog(parent: &adw::ApplicationWindow) {
    parent.set_cursor(None);
    let shortcuts: [(String, Vec<(&str, String)>); 6] = [
        (
            gettext("Navegação"),
            vec![
                ("Left Right", gettext("Anterior / próximo")),
                ("Home End", gettext("Primeiro / último")),
                ("space", gettext("Slideshow (auto-avanço)")),
            ],
        ),
        (
            gettext("Zoom"),
            vec![
                ("plus minus", gettext("Aproximar / afastar")),
                ("0", gettext("Ajustar à janela")),
                ("1", gettext("Tamanho real (100%)")),
                ("scroll_wheel", gettext("Zoom contínuo")),
            ],
        ),
        (
            gettext("Janela"),
            vec![
                ("F11", gettext("Tela cheia")),
                ("T", gettext("Modo teatro")),
                ("F", gettext("Fita de miniaturas")),
                ("G", gettext("Histograma RGB")),
                ("H", gettext("Fixar barras visíveis")),
                ("Escape", gettext("Sair da tela cheia ou fechar")),
            ],
        ),
        (
            gettext("Edição"),
            vec![
                ("<ctrl>Z", gettext("Desfazer")),
                ("<ctrl><shift>Z", gettext("Refazer")),
                ("R", gettext("Girar à direita")),
                ("<Shift>R", gettext("Girar à esquerda")),
                ("<ctrl>R", gettext("Recortar…")),
                ("<ctrl>B", gettext("Remover fundo (IA)…")),
                ("<ctrl>U", gettext("Aumentar resolução…")),
                ("<ctrl>S", gettext("Salvar alterações")),
            ],
        ),
        (
            gettext("Arquivo"),
            vec![
                ("<ctrl>O", gettext("Abrir imagens")),
                ("<ctrl>C", gettext("Copiar imagem")),
                ("<ctrl>V", gettext("Colar arquivos de imagem")),
                ("<ctrl>P", gettext("Imprimir")),
                ("<ctrl>I", gettext("Propriedades")),
                ("Delete", gettext("Mover para a lixeira")),
            ],
        ),
        (
            gettext("Ajuda"),
            vec![("<ctrl>question", gettext("Este diálogo")), ("F1", gettext("Sobre"))],
        ),
    ];
    // One scrolling page with a group per category, like GNOME's own
    // shortcut windows: everything is visible without picking a category.
    let page = adw::PreferencesPage::new();
    for (name, entries) in shortcuts {
        let group = adw::PreferencesGroup::builder().title(name).build();
        for (accel, desc) in entries {
            let key_text = if accel == "scroll_wheel" {
                gettext("Roda do mouse")
            } else {
                accel
                    .split_whitespace()
                    .map(|key| {
                        gtk::accelerator_parse(key).map_or_else(
                            || key.to_owned(),
                            |(key, modifiers)| {
                                gtk::accelerator_get_label(key, modifiers).to_string()
                            },
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" / ")
            };
            let keys = gtk::Label::builder().label(key_text).valign(gtk::Align::Center).build();
            keys.add_css_class("dim-label");
            keys.add_css_class("numeric");
            let row = adw::ActionRow::builder().title(desc).use_markup(false).build();
            row.add_suffix(&keys);
            group.add(&row);
        }
        page.add(&group);
    }
    let layout = adw::ToolbarView::new();
    layout.add_top_bar(&adw::HeaderBar::new());
    layout.set_content(Some(&page));
    let dialog = adw::Dialog::builder()
        .title(gettext("Atalhos de teclado"))
        .content_width(440)
        .content_height(520)
        .child(&layout)
        .build();
    dialog.present(Some(parent));
}

/// About dialog with project, license and support links.
fn present_about_dialog(parent: &adw::ApplicationWindow) -> adw::AboutDialog {
    // Restaura o cursor antes de apresentar — o viewer pode tê-lo
    // escondido por idle; o diálogo precisa dele para navegar nas páginas.
    parent.set_cursor(None);

    let about = adw::AboutDialog::builder()
        .application_name("BigIris")
        .application_icon(APP_ID)
        .developer_name("BigCommunity Team")
        .copyright("© 2026 BigCommunity Team")
        .license_type(gtk::License::Gpl30)
        .website("https://github.com/big-comm/bigiris")
        .issue_url("https://github.com/big-comm/bigiris/issues")
        .developers(vec!["BigCommunity Team".to_string()])
        .designers(vec!["BigCommunity Team".to_string()])
        .translator_credits("BigCommunity Team")
        .comments(gettext(
            "Visualizador e conversor de imagens para Linux. Rápido, acessível por teclado e integrado ao gerenciador de arquivos.",
        ))
        .build();
    // About has navigable pages; its own native window keeps credits usable
    // even when the compact start page is shorter than those pages.
    about.set_content_width(360);
    about.set_content_height(540);
    about.present(None::<&gtk::Widget>);
    if let Some(window) = about.root().and_downcast::<gtk::Window>() {
        window.set_transient_for(Some(parent));
        window.set_modal(true);
        window.set_destroy_with_parent(true);
    }
    about
}

/// Compact start page with two actions and file-manager drop/paste support.
fn build_dashboard(window: &adw::ApplicationWindow) -> gtk::Widget {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_valign(gtk::Align::Center);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(20);
    content.set_margin_end(20);
    let icon = gtk::Image::from_icon_name(APP_ID);
    icon.set_pixel_size(128);
    content.append(&icon);
    // Short windows get a smaller icon so the actions stay in view.
    if let Ok(condition) = adw::BreakpointCondition::parse("max-height: 440px") {
        let compact = adw::Breakpoint::new(condition);
        compact.add_setter(&icon, "pixel-size", Some(&64.to_value()));
        window.add_breakpoint(compact);
    }
    let title = gtk::Label::new(Some(&gettext("Abra suas imagens")));
    title.add_css_class("title-1");
    title.set_wrap(true);
    content.append(&title);
    let hint =
        gtk::Label::new(Some(&gettext("Arraste imagens para cá ou cole arquivos com Ctrl+V.")));
    hint.add_css_class("dim-label");
    hint.set_wrap(true);
    hint.set_justify(gtk::Justification::Center);
    hint.set_max_width_chars(38);
    content.append(&hint);
    let actions = gtk::Box::new(gtk::Orientation::Vertical, 10);
    actions.set_margin_top(12);
    let open_btn = gtk::Button::builder()
        .label(gettext("Abrir imagens…"))
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
    let folder_btn = gtk::Button::builder().label(gettext("Processar uma pasta…")).build();
    {
        let window = window.downgrade();
        folder_btn.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                open_batch_folder_dialog(&window);
            }
        });
    }
    actions.append(&folder_btn);
    content.append(&actions);
    let clamp = adw::Clamp::builder().maximum_size(320).child(&content).build();
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&clamp)
        .build()
        .upcast()
}

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
        let args =
            [std::ffi::OsStr::new("--dialog=batch"), std::ffi::OsStr::new("--"), path.as_os_str()];
        if transfers::spawn_bigiris(args).is_ok() {
            window.close();
        }
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
    filter.set_name(Some(&gettext("Imagens")));
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
        transfers::open_files(&window, paths, true);
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
    let subtitle = gettext("{current} de {total}")
        .replace("{current}", &(s.idx + 1).to_string())
        .replace("{total}", &s.files.len().to_string());
    s.title.set_title(&name);
    s.title.set_subtitle(&subtitle);
    s.zoom_level.set_label(&zoom_percent_label(&s));
    drop(s);
    refresh_osd(state);
    refresh_film_strip_active(state);
}

type Histogram = [u32; 768];
type RawFrame = (Vec<u8>, u32, u32, Histogram);

fn decode_frame(path: &Path, ops: &[EditOp]) -> Result<RawFrame, Box<dyn std::error::Error>> {
    let img = bigimage_core::open_image(path)?;
    let edited = ops.iter().fold(img, |acc, op| apply_op_to_image(acc, *op));
    // Float (EXR/HDR) pixels are linear: encode them to sRGB for display.
    let rgba = bigimage_core::into_srgb_rgba8(edited);
    let (w, h) = rgba.dimensions();
    let raw = rgba.into_raw();
    let histogram = sampled_histogram(&raw);
    Ok((raw, w, h, histogram))
}

fn texture_from_rgba(raw: Vec<u8>, w: u32, h: u32) -> gdk::Texture {
    let stride = (w as usize) * 4;
    let bytes = glib::Bytes::from_owned(raw);
    gdk::MemoryTexture::new(w as i32, h as i32, gdk::MemoryFormat::R8g8b8a8, &bytes, stride)
        .upcast()
}

/// The overlay draws each channel relative to its own peak, so a regular
/// sample of about a million pixels gives the same shape as a full count
/// at a fraction of the cost on large photos.
fn sampled_histogram(rgba: &[u8]) -> Histogram {
    let pixels = rgba.as_chunks::<4>().0;
    let step = (pixels.len() / 1_000_000).max(1);
    let mut h = [0u32; 768];
    for px in pixels.iter().step_by(step) {
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
    adjust_file, convert_file_to, crop_file, extent_file, flip_file, metadata, resize_file,
    rotate_file, AdjustOps, AnimateOptions, CropRect, EncodeOptions, ExtentOptions, ExtentSize,
    Filter, FlipAxis, Format, Gravity, LoopMode, OverwritePolicy, PreviewOp, PreviewSession,
    ResizeMode, Rotation,
};
use image::DynamicImage;
use std::time::Duration;

mod batch_runner;
mod dialogs;
pub use dialogs::*;

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
    fn in_place_jpeg_save_keeps_the_source_quality() {
        use super::resave_options;
        use bigimage_core::{EncodeOptions, Format};
        assert_eq!(resave_options(Format::Jpeg, Some(92)).quality, Some(92));
        assert_eq!(resave_options(Format::Jpeg, None).quality, EncodeOptions::default().quality);
        // Only JPEG is lossy-by-quality here; others keep their defaults.
        assert_eq!(resave_options(Format::Png, Some(92)).quality, EncodeOptions::default().quality);
    }

    #[test]
    fn dropped_metadata_is_explained_once_per_reason() {
        use super::metadata_loss_notice;
        use bigimage_core::MetadataLoss;
        assert_eq!(metadata_loss_notice(&[]), None);
        let notice = metadata_loss_notice(&[
            MetadataLoss::IccUnsupported,
            MetadataLoss::IccMismatched,
            MetadataLoss::ExifTooLarge,
        ])
        .unwrap();
        assert_eq!(
            notice,
            "Salvo. Alguns metadados não puderam ser mantidos: \
             perfil de cor convertido para sRGB; EXIF grande demais para JPEG"
        );
    }

    #[test]
    fn histogram_counts_channels_and_ignores_alpha() {
        use super::sampled_histogram;
        // 3 pixels: puro vermelho, puro verde (com alpha zero), puro azul.
        // Alpha deve ser ignorado — só somamos R/G/B no ponto certo.
        let rgba = vec![
            255, 0, 0, 255, //
            0, 255, 0, 0, //
            0, 0, 255, 255,
        ];
        let h = sampled_histogram(&rgba);
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
    fn capture_window(window: &impl IsA<gtk::Widget>, name: &str) {
        let window = window.as_ref();
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
        let texture = window
            .native()
            .and_then(|native| native.renderer())
            .expect("window renderer")
            .render_texture(&node, None);
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
        let animations_before = window.settings().is_gtk_enable_animations();
        window.settings().set_gtk_enable_animations(true);
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
        let frame_trace = Rc::new(RefCell::new(Vec::new()));
        let clock = window.frame_clock().unwrap();
        let weak = Rc::downgrade(&state);
        let trace = frame_trace.clone();
        let sample_signal = clock.connect_after_paint(move |clock| {
            let Some(state) = weak.upgrade() else { return };
            let s = state.borrow();
            let Some(bounds) = s.picture.compute_bounds(&s.scroller) else { return };
            let Some(paintable) = s.picture.paintable() else { return };
            let iw = f64::from(paintable.intrinsic_width());
            let ih = f64::from(paintable.intrinsic_height());
            let pw = f64::from(s.picture.width());
            let ph = f64::from(s.picture.height());
            let mut scale = (pw / iw).min(ph / ih);
            if s.picture.content_fit() == gtk::ContentFit::ScaleDown {
                scale = scale.min(1.0);
            }
            let x = f64::from(bounds.x()) + (pw - iw * scale) / 2.0;
            let y = f64::from(bounds.y()) + (ph - ih * scale) / 2.0;
            trace.borrow_mut().push((
                (point.0 - x) / (iw * scale) * 1600.0,
                (point.1 - y) / (ih * scale) * 1200.0,
                iw * scale,
                ih * scale,
                clock.frame_time(),
            ));
        });
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
        let reports = RefCell::new(Vec::new());
        let anchored = |name: &str, expected: (f64, f64)| {
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
            let frames = std::mem::take(&mut *frame_trace.borrow_mut());
            assert!(!frames.is_empty(), "no painted frames for {name}");
            let area = {
                let s = state.borrow();
                (s.scroller.hadjustment().page_size(), s.scroller.vadjustment().page_size())
            };
            // When a side is smaller than the viewport, GTK centers it and
            // cannot scroll it. Compare to the constrained geometric target;
            // once scrolling is possible the original point remains anchored.
            let projected =
                |natural_point: f64, natural: f64, size: f64, cursor: f64, page: f64| {
                    let padding = ((page - size) / 2.0).max(0.0);
                    let target = (natural_point / natural * size - cursor + padding)
                        .clamp(0.0, (size - page).max(0.0));
                    (cursor + target - padding) / size * natural
                };
            assert!(
                frames.iter().all(|&(x, y, width, height, _)| {
                    let px = projected(expected.0, 1600.0, width, point.0, area.0);
                    let py = projected(expected.1, 1200.0, height, point.1, area.1);
                    (x - px).abs() * width / 1600.0 < 1.5 && (y - py).abs() * height / 1200.0 < 1.5
                }),
                "zoom drift during painted frames ({name}): before={expected:?}, frames={frames:?}"
            );
            let first = frames.first().unwrap().2;
            let last = frames.last().unwrap().2;
            let increasing = last >= first;
            assert!(
                name == "direction-change"
                    || frames.windows(2).all(|pair| if increasing {
                        pair[1].2 >= pair[0].2 - 1.0
                    } else {
                        pair[1].2 <= pair[0].2 + 1.0
                    }),
                "zoom reversed during transition ({name}): {frames:?}"
            );
            if name == "direction-change" {
                assert!(frames.windows(2).any(|p| p[1].2 < p[0].2 - 1.0));
                assert!(frames.windows(2).any(|p| p[1].2 > p[0].2 + 1.0));
            }
            let intermediate = frames
                .iter()
                .filter(|frame| frame.2 > first.min(last) + 1.0 && frame.2 < first.max(last) - 1.0)
                .count();
            let animations = window.settings().is_gtk_enable_animations();
            if animations && (last - first).abs() > 32.0 {
                assert!(intermediate >= 2, "zoom skipped intermediate scales ({name}): {frames:?}");
            }
            reports.borrow_mut().push(serde_json::json!({"case":name,
                "animations":animations,"anchor":expected,"cursor":point,"viewport":area,
                "max_screen_error":1.5,"frames":frames,"intermediate_frames":intermediate}));
        };
        handle_key(&state, gdk::Key::plus, gdk::ModifierType::empty());
        anchored("first-in", before);
        let rapid_anchor = image_point();
        for _ in 0..4 {
            handle_key(&state, gdk::Key::plus, gdk::ModifierType::empty());
        }
        anchored("rapid-in", rapid_anchor);
        let reversal_anchor = image_point();
        zoom_out(&state);
        wait(&|| frame_trace.borrow().len() >= 4);
        assert!(state.borrow().pending_zoom.is_some(), "reversal test missed the animation");
        zoom_in(&state);
        anchored("direction-change", reversal_anchor);
        let toolbar_anchor = image_point();
        zoom_out(&state); // the toolbar action uses the same selected point
        anchored("toolbar-out", toolbar_anchor);
        let full_anchor = image_point();
        handle_key(&state, gdk::Key::_1, gdk::ModifierType::empty());
        anchored("100-percent", full_anchor);
        assert_eq!(state.borrow().picture.width(), 1600);
        assert_eq!(state.borrow().picture.height(), 1200);
        assert_eq!(state.borrow().zoom_level.label(), "100%", "header zoom level is stale");
        capture_window(&window, "zoom-pointer-100pct");
        let reduced_anchor = image_point();
        window.settings().set_gtk_enable_animations(false);
        zoom_out(&state);
        anchored("reduced-motion", reduced_anchor);
        window.settings().set_gtk_enable_animations(true);
        zoom_in(&state);
        let drag = (0..controllers.n_items())
            .find_map(|index| controllers.item(index)?.downcast::<gtk::GestureDrag>().ok())
            .unwrap();
        drag.emit_by_name::<()>("drag-begin", &[&point.0, &point.1]);
        assert!(state.borrow().pending_zoom.is_none(), "animation fought manual pan");
        if let Some(folder) = std::env::var_os("BIGIRIS_TEST_ZOOM_REPORT_DIR") {
            let folder = PathBuf::from(folder);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(
                folder.join("painted-frames.json"),
                serde_json::to_vec_pretty(&reports.into_inner()).unwrap(),
            )
            .unwrap();
        }
        clock.disconnect(sample_signal);
        window.settings().set_gtk_enable_animations(animations_before);
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

    fn exercise_prefetch(app: &adw::Application, directory: &Path) {
        let files: Vec<_> = ["prefetch-a.png", "prefetch-b.png", "prefetch-c.png"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let path = directory.join(name);
                image::RgbImage::from_pixel(64, 48, image::Rgb([i as u8 * 100, 50, 50]))
                    .save(&path)
                    .unwrap();
                path
            })
            .collect();
        let (window, witness) = build_window_tracked(app, Rc::new(files.clone()), 0);
        window.present();
        let state = witness.upgrade().unwrap();
        let context = glib::MainContext::default();
        let wait = |what: &str, condition: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !condition() {
                while context.pending() {
                    context.iteration(false);
                }
                assert!(std::time::Instant::now() < deadline, "prefetch: {what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let cached = |path: &PathBuf| state.borrow().frame_cache.iter().any(|f| &f.path == path);
        wait("neighbours were not decoded", &|| {
            cached(&files[1]) && cached(&files[2]) && state.borrow().prefetching.is_none()
        });
        assert!(
            state.borrow().warm_slots.iter().all(|slot| slot.paintable().is_some()),
            "neighbours are not kept uploaded"
        );

        // The next image is on screen as soon as the key is handled.
        let expected = state
            .borrow()
            .frame_cache
            .iter()
            .find(|f| f.path == files[1])
            .map(|f| f.texture.clone())
            .unwrap();
        navigate_relative(&state, 1);
        assert_eq!(state.borrow().texture.as_ref(), Some(&expected), "cached frame not shown");
        assert!(state.borrow().current_ready);

        // A neighbour rewritten after it was decoded is read again.
        wait("new neighbours were not decoded", &|| {
            cached(&files[2]) && cached(&files[0]) && state.borrow().prefetching.is_none()
        });
        std::thread::sleep(Duration::from_millis(20));
        image::RgbImage::from_pixel(80, 60, image::Rgb([0, 200, 0])).save(&files[2]).unwrap();
        navigate_relative(&state, 1);
        wait("changed file was not reloaded", &|| state.borrow().natural == (80, 60));

        window.destroy();
        drop(state);
        drop(window);
        wait("viewer state leaked", &|| witness.upgrade().is_none());
    }

    fn find_widget(
        widget: &impl IsA<gtk::Widget>,
        predicate: &mut impl FnMut(&gtk::Widget) -> bool,
    ) -> Option<gtk::Widget> {
        let widget = widget.as_ref();
        if predicate(widget) {
            return Some(widget.clone());
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            if let Some(found) = find_widget(&current, predicate) {
                return Some(found);
            }
            child = current.next_sibling();
        }
        None
    }

    fn exercise_start_dialogs(app: &adw::Application) {
        let context = glib::MainContext::default();
        let settle = || {
            let until = std::time::Instant::now() + Duration::from_millis(300);
            while std::time::Instant::now() < until {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        if let Some(settings) = app_settings() {
            settings.set_int("window-width", 1280).unwrap();
            settings.set_int("window-height", 800).unwrap();
            settings.set_boolean("window-maximized", true).unwrap();
        }
        let (window, state) = build_window_tracked(app, Rc::new(Vec::new()), 0);
        window.present();
        settle();
        assert!(!window.is_maximized(), "start page inherited viewer maximization");
        assert!(
            window.width() <= 620 && window.height() <= 520,
            "start page grew to {}x{}",
            window.width(),
            window.height()
        );
        for text in [gettext("Abrir imagens…"), gettext("Processar uma pasta…")] {
            let button = find_widget(&window, &mut |widget| {
                widget
                    .clone()
                    .downcast::<gtk::Button>()
                    .is_ok_and(|button| button.label().as_deref() == Some(text.as_str()))
            })
            .expect("start page action");
            let bounds = button.compute_bounds(&window).unwrap();
            assert!(
                button.is_mapped()
                    && bounds.y() >= 0.0
                    && bounds.y() + bounds.height() <= window.height() as f32,
                "start page action is clipped: {text}, {bounds:?}"
            );
        }
        capture_window(&window, "welcome");
        let menu = find_widget(&window, &mut |widget| {
            widget
                .clone()
                .downcast::<gtk::MenuButton>()
                .is_ok_and(|button| button.icon_name().as_deref() == Some("open-menu-symbolic"))
        })
        .unwrap()
        .downcast::<gtk::MenuButton>()
        .unwrap();
        menu.popup();
        settle();
        let popover = menu.popover().unwrap();
        assert!(popover.height() <= 280, "start menu grew to {}px", popover.height());
        capture_window(&popover, "welcome-menu");
        menu.popdown();
        settle();

        // Credits must fit independently of the small main window.
        let about = present_about_dialog(&window);
        settle();
        let about_window = about.root().and_downcast::<gtk::Window>().unwrap();
        capture_window(&about_window, "about");
        let credits = find_widget(&about, &mut |widget| {
            widget.clone().downcast::<adw::ActionRow>().is_ok_and(|row| {
                row.action_name().as_deref() == Some("navigation.push")
                    && row
                        .action_target_value()
                        .is_some_and(|target| target.str() == Some("credits"))
            })
        })
        .expect("credits navigation row")
        .downcast::<adw::ActionRow>()
        .unwrap();
        gtk::prelude::WidgetExt::activate(&credits);
        settle();
        settle();
        let mut team_labels = Vec::new();
        find_widget(&about, &mut |widget| {
            if let Some(label) = widget
                .clone()
                .downcast::<gtk::Label>()
                .ok()
                .filter(|label| widget.is_mapped() && label.text() == "BigCommunity Team")
            {
                team_labels.push(label);
            }
            false
        });
        assert_eq!(team_labels.len(), 3, "credits did not display all three roles");
        for label in team_labels {
            let bounds = label.compute_bounds(&about_window).unwrap();
            assert!(
                bounds.y() >= 0.0 && bounds.y() + bounds.height() <= about_window.height() as f32,
                "credit label extends beyond the window: {bounds:?}"
            );
        }
        capture_window(&about_window, "credits");
        about.close();
        settle();

        present_shortcuts_dialog(&window);
        settle();
        let dialog = window.visible_dialog().unwrap();
        assert!(
            dialog.child().unwrap().height() <= window.height(),
            "shortcuts exceeded the window"
        );
        capture_window(&window, "shortcuts");
        dialog.close();
        settle();
        window.set_default_size(360, 420);
        settle();
        capture_window(&window, "welcome-narrow");
        assert!(
            state.upgrade().unwrap().borrow().toolbar_view.reveals_top_bars(),
            "start page hid its window controls while idle"
        );
        present_shortcuts_dialog(&window);
        settle();
        capture_window(&window, "shortcuts-narrow");
        window.visible_dialog().unwrap().close();
        settle();
        drop(dialog);
        drop(about_window);
        drop(about);
        drop(credits);
        drop(menu);
        drop(popover);
        window.destroy();
        drop(window);
        settle();
        assert!(state.upgrade().is_none(), "start page retained its state after closing");
        if let Some(settings) = app_settings() {
            assert_eq!(settings.int("window-width"), 1280);
            assert_eq!(settings.int("window-height"), 800);
            settings.set_boolean("window-maximized", false).unwrap();
        }
    }

    #[test]
    #[ignore = "requires a graphical display; run alone with GSETTINGS_BACKEND=memory"]
    fn viewer_lifecycle_releases_state_and_workers() {
        assert_eq!(
            std::env::var("GSETTINGS_BACKEND").as_deref(),
            Ok("memory"),
            "synthetic GUI tests require GSETTINGS_BACKEND=memory"
        );
        crate::init_i18n();
        adw::init().expect("GTK display required");
        if std::env::var_os("BIGIRIS_LOCALE_DIR").is_some() {
            let language = std::env::var("LANGUAGE").unwrap_or_default();
            let expected = match language.as_str() {
                "en" => "Open images…",
                "fr" => "Ouvrir des images…",
                "he" => "פתיחת תמונות…",
                _ => panic!("localized GUI probe expects en, fr or he"),
            };
            assert_eq!(
                gettext("Abrir imagens…"),
                expected,
                "GUI did not load the requested catalog"
            );
        }
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
        exercise_start_dialogs(&app);
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
        exercise_prefetch(&app, dir.path());

        // A file that cannot be read keeps an error page on screen.
        let missing = dir.path().join("missing <&>.jpg");
        let (window, witness) = build_window_tracked(&app, Rc::new(vec![missing]), 0);
        window.present();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while witness
            .upgrade()
            .is_some_and(|s| s.borrow().stack.visible_child_name().as_deref() != Some("error"))
        {
            while context.pending() {
                context.iteration(false);
            }
            assert!(std::time::Instant::now() < deadline, "missing file left no error page");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            witness.upgrade().unwrap().borrow().error_page.description().as_deref(),
            Some("missing &lt;&amp;&gt;.jpg")
        );
        window.destroy();
        drop(window);
        while context.pending() {
            context.iteration(false);
        }

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

    /// Unsaved edits are printed from the texture: same layout as the
    /// on-disk path above.
    #[test]
    fn edited_texture_becomes_a_premultiplied_cairo_surface() {
        let texture = super::texture_from_rgba(vec![200, 100, 50, 128, 2, 3, 4, 255], 2, 1);
        let mut surface = super::cairo_surface_from_texture(&texture).unwrap();
        assert_eq!((surface.width(), surface.height()), (2, 1));
        let data = surface.data().unwrap();
        let values: Vec<_> =
            data[..8].as_chunks::<4>().0.iter().map(|p| u32::from_ne_bytes(*p)).collect();
        assert_eq!(values, vec![0x80643219, 0xff020304]);
    }
}

#[cfg(test)]
mod float_display_tests {
    /// Linear 0.5 must reach the screen as sRGB ≈188, not as a bare 128.
    #[test]
    fn linear_float_images_display_in_srgb() {
        let dir = tempfile::tempdir().unwrap();
        let exr = dir.path().join("half.exr");
        let linear = image::Rgba32FImage::from_pixel(4, 2, image::Rgba([0.5, 0.5, 0.5, 1.0]));
        linear.save(&exr).unwrap();
        let hdr = dir.path().join("half.hdr");
        image::Rgb32FImage::from_pixel(4, 2, image::Rgb([0.5, 0.5, 0.5])).save(&hdr).unwrap();
        for path in [exr, hdr] {
            let (raw, w, h, _) = super::decode_frame(&path, &[]).unwrap();
            assert_eq!((w, h), (4, 2));
            let pixel = &raw[..4];
            assert!(pixel[..3].iter().all(|v| v.abs_diff(188) <= 1), "{path:?}: {pixel:?}");
            assert_eq!(pixel[3], 255);
        }
    }
}
