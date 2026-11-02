// SPDX-License-Identifier: GPL-3.0-or-later
//! Remover fundo: local BiRefNet background removal.

use super::*;

/// Output path for `remove_background` — always PNG to preserve alpha.
/// Sibling of the source, stem suffixed with `_nobg`.
pub(in crate::gui) fn nobg_output_path(src: &Path) -> PathBuf {
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
    // Increment, never Replace: the dialog promises not to overwrite, and
    // `foto.jpg` + `foto.png` in one batch both map to `foto_nobg.png`.
    let outcome = bigimage_core::save_image(
        out,
        nobg_output_path(src),
        Format::Png,
        &EncodeOptions::default(),
        OverwritePolicy::Increment,
    )
    .map_err(|e| format!("encode: {e}"))?;
    Ok(outcome.path().to_path_buf())
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
    let files = Rc::new(files);
    run_dialog_app(move |app| build_remove_bg_dialog(app, files.clone()).present())
}

pub(in crate::gui) fn build_remove_bg_dialog(
    app: &adw::Application,
    files: Rc<Vec<PathBuf>>,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(520)
        .default_height(420)
        .title(gettext("Remover fundo — Prisma"))
        .build();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        &gettext("Remover fundo"),
        &gettext("Prisma · BiRefNet-lite"),
    )));

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
            .title(gettext("Nenhuma imagem selecionada"))
            .description(gettext("Informe um ou mais arquivos ao abrir o diálogo."))
            .vexpand(true)
            .build();
        content.append(&status);

        let close = gtk::Button::with_label(&gettext("Fechar"));
        close_on_click(&window, &close);
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
            .label(gettext("Esta build não inclui IA. Recompile com `cargo build --features ai` e instale `onnxruntime` (Arch: `pacman -S onnxruntime`)."))
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

    let files_row = selection_row(&gettext("Imagens"), &files);
    let files_icon = gtk::Image::from_icon_name("image-x-generic-symbolic");
    files_row.add_prefix(&files_icon);

    let model_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Modelo"))
        .subtitle(gettext("BiRefNet-lite (MIT) · roda offline no seu computador"))
        .build();
    let model_icon = gtk::Image::from_icon_name("application-x-executable-symbolic");
    model_row.add_prefix(&model_icon);

    // Subtitle é Pango-markup em adw::ActionRow → angle-brackets ("<orig>")
    // eram interpretados como tag malformada e a linha ficava vazia.
    // Usar chevrons Unicode evita o parser sem escaping manual.
    let output_row = adw::ActionRow::builder()
        .use_markup(false)
        .title(gettext("Saída"))
        .subtitle(gettext(
            "«origem»_nobg.png · PNG com alfa, sem sobrescrever (_1, _2… se já existir)",
        ))
        .build();
    let output_icon = gtk::Image::from_icon_name("document-save-symbolic");
    output_row.add_prefix(&output_icon);

    let group = adw::PreferencesGroup::builder()
        .title(gettext("Remover fundo com IA local"))
        .description(gettext("O modelo roda inteiramente no seu computador. Nenhuma imagem sai do disco — sem upload, sem conta, sem API externa."))
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

    let status = status_label();
    content.append(&status);

    let (cancel_btn, apply_btn, actions_box) = action_buttons(&gettext("Remover fundo"));
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
    close_on_click(&window, &cancel_btn);

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
                        progress_bar.set_text(Some(&fill(
                            &gettext("Baixando modelo (uma vez só): {done} / {total}"),
                            &[("done", &format_size(done)), ("total", &format_size(total))],
                        )));
                        glib::ControlFlow::Continue
                    }
                    BgUiState::Processing { idx, total, file } => {
                        let frac = (idx as f64 / total.max(1) as f64).clamp(0.0, 1.0);
                        progress_bar.set_fraction(frac);
                        progress_bar.set_text(Some(&fill(
                            &gettext("Processando {current}/{total} — {file}"),
                            &[
                                ("current", &(idx + 1).to_string()),
                                ("total", &total.to_string()),
                                ("file", &file),
                            ],
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
                            status.set_text(&format!("{}: {ok}", gettext("Imagens gravadas")));
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
                            status.set_text(&format!(
                                "{}: {ok} · {}: {fail}\n{detail}",
                                gettext("Imagens gravadas"),
                                gettext("Falhas")
                            ));
                            progress_bar.set_text(Some(&gettext("Com erros")));
                            progress_bar.add_css_class("error");
                            apply_btn.set_sensitive(true);
                            cancel_btn.set_label(&gettext("Fechar"));
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
