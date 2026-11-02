// SPDX-License-Identifier: GPL-3.0-or-later
//! Criar GIF animado: build an animated GIF from the selected frames.

use super::*;
use bigimage_core::{make_gif_with_policy, ConvertOutcome};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Animate dialog — pick a delay, a loop mode, an output path and build an
// animated GIF from the selected frames. Minimal shape: every parameter
// has a sensible default so the user can just hit "Criar GIF" and get
// something playable.
// ---------------------------------------------------------------------------

pub fn run_animate_dialog(files: Vec<PathBuf>) -> i32 {
    let files = Rc::new(files);
    run_dialog_app(move |app| build_animate_dialog(app, files.clone()).present())
}

pub(in crate::gui) fn build_animate_dialog(
    app: &adw::Application,
    files: Rc<Vec<PathBuf>>,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(520)
        .default_height(520)
        .title(gettext("Criar GIF animado — Prisma"))
        .build();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(&gettext("Criar GIF animado"), "Prisma")));

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
            .title(gettext("Selecione pelo menos 2 imagens"))
            .description(gettext("Informe vários arquivos de uma vez para montar a animação."))
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

    // Parametric rows.
    let delay_spin = adw::SpinRow::with_range(10.0, 5000.0, 10.0);
    delay_spin.set_title(&gettext("Atraso por quadro (ms)"));
    delay_spin.set_subtitle(&gettext("100 ≈ 10 fps · 40 ≈ 25 fps · 1000 = 1 s"));
    delay_spin.set_value(100.0);

    let loop_row = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Loop"))
        .model(&translated_strings(&[
            n_("Infinito (padrão)"),
            n_("Uma vez"),
            n_("2 vezes"),
            n_("5 vezes"),
            n_("10 vezes"),
        ]))
        .build();

    let speed_spin = adw::SpinRow::with_range(1.0, 30.0, 1.0);
    speed_spin.set_title(&gettext("Velocidade do encoder"));
    speed_spin.set_subtitle(&gettext("maior = mais rápido, paleta um pouco pior · padrão 10"));
    speed_spin.set_value(10.0);

    // Default output: next to the first frame, named "animated.gif".
    let default_output: PathBuf = files[0]
        .parent()
        .map(|p| p.join("animated.gif"))
        .unwrap_or_else(|| PathBuf::from("animated.gif"));
    let output_row = adw::EntryRow::builder()
        .use_markup(false)
        .title(gettext("Arquivo de saída (.gif)"))
        .build();
    output_row.set_text(&default_output.to_string_lossy());

    let files_row = selection_row(&gettext("Quadros"), &files);

    let group = adw::PreferencesGroup::new();
    group.add(&files_row);
    group.add(&delay_spin);
    group.add(&loop_row);
    group.add(&speed_spin);
    group.add(&output_row);
    content.append(&group);

    let status = status_label();
    content.append(&status);

    let (cancel_btn, apply_btn, actions_box) = action_buttons(&gettext("Criar GIF"));
    content.append(&actions_box);

    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&content));
    window.set_content(Some(&tv));

    close_on_click(&window, &cancel_btn);

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
            let output = match gif_output_path(output_row.text().as_str(), &files) {
                Ok(output) => output,
                Err(message) => {
                    status.set_text(&message);
                    return;
                }
            };
            // Show the path that will actually be written (".gif" may
            // have been appended).
            output_row.set_text(&output.to_string_lossy());
            let exists = output.exists();
            let shown = output.display().to_string();

            let start = {
                let files = files.clone();
                let status = status.clone();
                let apply_btn = apply_btn.clone();
                let cancel_btn = cancel_btn.clone();
                let window = window.clone();
                move |policy| {
                    start_animate_run(
                        (*files).clone(),
                        output.clone(),
                        opts,
                        policy,
                        &status,
                        &apply_btn,
                        &cancel_btn,
                        &window,
                    )
                }
            };
            if !exists {
                // Skip, not Replace: a file that appears meanwhile was never
                // confirmed by the user and is left alone.
                start(OverwritePolicy::Skip);
                return;
            }
            // Only replace an existing file the user explicitly agreed to lose.
            let alert = adw::AlertDialog::builder()
                .heading(gettext("Substituir arquivo existente?"))
                .body(shown)
                .build();
            alert.add_response("cancel", &gettext("Cancelar"));
            alert.add_response("replace", &gettext("Substituir"));
            alert.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
            alert.set_default_response(Some("cancel"));
            alert.set_close_response("cancel");
            alert.connect_response(Some("replace"), move |_, _| start(OverwritePolicy::Replace));
            alert.present(Some(&window));
        });
    }

    window
}

/// Validate the free-text GIF destination. The result always ends in
/// `.gif` and never names one of the frames: make_gif replaces its output,
/// so typing a frame's path would otherwise turn that photo into a GIF.
fn gif_output_path(text: &str, frames: &[PathBuf]) -> Result<PathBuf, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(gettext("Informe o arquivo de saída."));
    }
    let mut output = PathBuf::from(text);
    if !output.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("gif")) {
        let mut name = output.into_os_string();
        name.push(".gif");
        output = PathBuf::from(name);
    }
    if output.is_dir() {
        return Err(gettext("O arquivo de saída não pode ser uma pasta."));
    }
    let target = comparable_path(&output);
    if frames.iter().any(|frame| comparable_path(frame) == target) {
        return Err(gettext("O arquivo de saída não pode ser um dos quadros."));
    }
    Ok(output)
}

/// Canonical form of `path` for identity checks, also for files that do
/// not exist yet (the parent is canonicalized and the name re-attached).
fn comparable_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    match (std::fs::canonicalize(parent), path.file_name()) {
        (Ok(parent), Some(name)) => parent.join(name),
        _ => path.to_path_buf(),
    }
}

/// Encode the GIF on a worker thread and reflect the result in the dialog.
#[allow(clippy::too_many_arguments)]
fn start_animate_run(
    frames: Vec<PathBuf>,
    output: PathBuf,
    opts: AnimateOptions,
    policy: OverwritePolicy,
    status: &gtk::Label,
    apply_btn: &gtk::Button,
    cancel_btn: &gtk::Button,
    window: &adw::ApplicationWindow,
) {
    batch_runner::disarm_auto_close(window);
    apply_btn.set_sensitive(false);
    cancel_btn.set_sensitive(false);
    status.set_text(&gettext("Montando GIF…"));

    // Closing the window aborts an in-flight isolated decode; the frames
    // already decoded are dropped and nothing is published.
    let cancelled = Arc::new(AtomicBool::new(false));
    let close_handler = {
        let cancelled = cancelled.clone();
        window.connect_close_request(move |_| {
            cancelled.store(true, Ordering::Release);
            glib::Propagation::Proceed
        })
    };

    // The encode (100 frames at 4K can take several seconds) runs off the
    // GTK thread and is awaited on the loop instead of polled on a timer.
    let job = {
        let cancelled = cancelled.clone();
        gio::spawn_blocking(move || {
            crate::codec::with_cancel(cancelled, || {
                make_gif_with_policy(&frames, &output, opts, policy)
            })
        })
    };

    let status = status.clone();
    let window = window.clone();
    let apply_btn = apply_btn.clone();
    let cancel_btn = cancel_btn.clone();
    // Released only after the encoder finished: closing the window never
    // exits mid-write and strands `.part` files.
    let hold = window.application().map(|app| app.hold());
    glib::MainContext::default().spawn_local(async move {
        let _hold = hold;
        // A panicking encoder reports as an unexpected end below.
        let result = job.await.ok();
        window.disconnect(close_handler);
        // Closed mid-run: nothing left to show or to auto-close.
        if cancelled.load(Ordering::Acquire) && !window.is_visible() {
            return;
        }
        match result {
            Some(Ok(ConvertOutcome::Written { output })) => {
                status.set_text(&format!("{}: {}", gettext("Pronto"), output.display()));
                batch_runner::arm_auto_close(&window, 2);
            }
            Some(Ok(ConvertOutcome::Skipped { output })) => {
                status.set_text(&fill(
                    &gettext("O arquivo {file} passou a existir e foi mantido; nada foi gravado."),
                    &[("file", &output.display().to_string())],
                ));
                apply_btn.set_sensitive(true);
                cancel_btn.set_sensitive(true);
            }
            Some(Err(e)) => {
                status.set_text(&format!("{}: {e}", gettext("Falha")));
                apply_btn.set_sensitive(true);
                cancel_btn.set_sensitive(true);
            }
            None => {
                status.set_text(&gettext(
                    "O processamento terminou inesperadamente. Tente novamente.",
                ));
                apply_btn.set_sensitive(true);
                cancel_btn.set_sensitive(true);
            }
        }
    });
}

#[cfg(test)]
mod gif_output_tests {
    use super::gif_output_path;
    use std::path::PathBuf;

    fn frames(dir: &std::path::Path) -> Vec<PathBuf> {
        let frames = vec![dir.join("a.png"), dir.join("b.jpg")];
        for frame in &frames {
            std::fs::write(frame, b"x").unwrap();
        }
        frames
    }

    #[test]
    fn rejects_a_frame_as_output() {
        let dir = tempfile::tempdir().unwrap();
        let frames = frames(dir.path());
        // Same file through a different spelling still counts as a frame.
        let dotted = dir.path().join(".").join("a.png.gif");
        std::fs::write(&dotted, b"x").unwrap();
        let mut with_gif = frames.clone();
        with_gif.push(dotted.clone());
        assert!(gif_output_path(&dotted.to_string_lossy(), &with_gif).is_err());
        assert!(
            gif_output_path(&dir.path().join("./a.png.gif").to_string_lossy(), &with_gif).is_err()
        );
    }

    #[test]
    fn appends_gif_extension_instead_of_replacing_a_frame() {
        let dir = tempfile::tempdir().unwrap();
        let frames = frames(dir.path());
        let out = gif_output_path(&frames[0].to_string_lossy(), &frames).unwrap();
        assert_eq!(out, dir.path().join("a.png.gif"));
        let out = gif_output_path(&dir.path().join("anim.GIF").to_string_lossy(), &frames).unwrap();
        assert_eq!(out, dir.path().join("anim.GIF"));
    }

    #[test]
    fn rejects_empty_and_directory_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let frames = frames(dir.path());
        assert!(gif_output_path("   ", &frames).is_err());
        let folder = dir.path().join("pasta.gif");
        std::fs::create_dir(&folder).unwrap();
        assert!(gif_output_path(&folder.to_string_lossy(), &frames).is_err());
    }
}
