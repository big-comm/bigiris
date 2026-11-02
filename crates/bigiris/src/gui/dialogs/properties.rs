// SPDX-License-Identifier: GPL-3.0-or-later
//! Propriedades: file, image and EXIF details of one image.

use super::*;

// ---------------------------------------------------------------------------
// Metadata viewer dialog — read-only. Loads EXIF via `bigimage_core::metadata`
// and shows every tag as an AdwActionRow so users can scan + copy values.
// ---------------------------------------------------------------------------

/// Run the metadata viewer for `files[0]`. Multi-select still opens but
/// only the first file is inspected — a "próximo" navegator fica para M3.
pub fn run_metadata_dialog(files: Vec<PathBuf>) -> i32 {
    run_dialog_app(move |app| {
        if let Some(first) = files.first() {
            build_properties_window(app, first).present();
        }
    })
}

/// Janela de propriedades da imagem. Sempre traz as infos básicas do
/// arquivo (nome, pasta, tamanho, modificado, dimensões) mesmo sem EXIF —
/// é o que o usuário enxerga quando clica no "i". Se houver EXIF, uma
/// seção extra aparece abaixo com os campos crus.
pub(in crate::gui) fn build_properties_window(
    app: &adw::Application,
    path: &Path,
) -> adw::ApplicationWindow {
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(520)
        .default_height(640)
        .title(format!("{} — {filename}", gettext("Propriedades")))
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
    let spinner = adw::Spinner::new();
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
    header.set_title_widget(Some(&adw::WindowTitle::new(&gettext("Propriedades"), &filename)));

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
                        gettext("Coordenadas GPS presentes")
                    } else {
                        gettext("Identificação de câmera/software presente")
                    })
                    .subtitle(gettext("Remova antes de publicar se for sensível."))
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
    group.add(&info_row(&gettext("Nome"), &filename));

    if let Some(parent) = path.parent() {
        let parent_str = parent.display().to_string();
        if !parent_str.is_empty() {
            group.add(&info_row(&gettext("Pasta"), &parent_str));
        }
    }

    // Sniff format by magic bytes — `path.extension()` lies for partial-sync
    // files (Nextcloud `.chunking-*`, browser `.part`/`.crdownload`, `.tmp`)
    // even when the bytes are a valid PNG/JPEG/etc. Falls back to extension
    // when sniffing fails (truly unknown / corrupt / non-image file).
    let canonical = |name: &str| match name.to_uppercase().as_str() {
        "JPG" => "JPEG".to_string(),
        "TIF" => "TIFF".to_string(),
        other => other.to_string(),
    };
    let ext = path.extension().map(|e| canonical(&e.to_string_lossy()));
    let sniffed = info.and_then(|i| i.format.as_deref()).map(canonical);
    match (sniffed, ext) {
        (Some(s), Some(e)) if s != e => {
            group.add(&info_row(
                &gettext("Formato"),
                &fill(
                    &gettext("{format} (extensão: {extension})"),
                    &[("format", &s), ("extension", &e)],
                ),
            ));
        }
        (Some(s), _) => group.add(&info_row(&gettext("Formato"), &s)),
        (None, Some(e)) => group.add(&info_row(&gettext("Formato"), &e)),
        (None, None) => {}
    }

    match fsmeta {
        Ok(fsmeta) => {
            group.add(&info_row(&gettext("Tamanho"), &format_bytes(fsmeta.len())));
            if let Ok(mtime) = fsmeta.modified() {
                group.add(&info_row(&gettext("Modificado"), &format_system_time(mtime)));
            }
        }
        Err(e) => {
            group.add(&info_row(
                &gettext("Tamanho"),
                &fill(&gettext("(indisponível: {error})"), &[("error", e.as_str())]),
            ));
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
            group.add(&info_row(&gettext("Dimensões"), &format!("{w} × {h} px")));

            let mp = (f64::from(w) * f64::from(h)) / 1_000_000.0;
            if mp >= 0.1 {
                group.add(&info_row(&gettext("Megapixels"), &format!("{mp:.1} MP")));
            }
            if w > 0 && h > 0 {
                let (rw, rh) = simplify_ratio(w, h);
                group.add(&info_row(&gettext("Razão de aspecto"), &format!("{rw}:{rh}")));
            }
            if let Some(color) = color {
                group.add(&info_row(&gettext("Modo de cor"), &color_type_label(color)));
                group.add(&info_row(&gettext("Bits por canal"), color_type_bits(color)));
            }
        }
        None => {
            group.add(&info_row(
                &gettext("Dimensões"),
                &gettext("(falha: não foi possível identificar o formato pelo conteúdo)"),
            ));
        }
    }

    // Orientação EXIF — só aparece se houver tag e != 1 (1 = upright).
    if let Some(meta) = info.and_then(|i| i.metadata.as_ref()) {
        if let Some(o) = meta.orientation {
            if o != 1 {
                group.add(&info_row(
                    &gettext("Orientação EXIF"),
                    &fill(
                        &gettext("{orientation} · use Girar ▸ Automático pra corrigir"),
                        &[("orientation", &o.to_string())],
                    ),
                ));
            }
        }
    }
    group
}

/// Friendly label for the decoded pixel layout, matching what the Convert
/// dialog says when picking encoders.
fn color_type_label(c: image::ColorType) -> String {
    use image::ColorType::*;
    match c {
        L8 | L16 => gettext("Tons de cinza"),
        La8 | La16 => gettext("Tons de cinza + alfa"),
        Rgb8 | Rgb16 | Rgb32F => "RGB".to_string(),
        Rgba8 | Rgba16 | Rgba32F => "RGBA".to_string(),
        _ => "—".to_string(),
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
        .description(fill(
            &gettext("Campos presentes: {count}"),
            &[("count", &meta.tags.len().to_string())],
        ))
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
