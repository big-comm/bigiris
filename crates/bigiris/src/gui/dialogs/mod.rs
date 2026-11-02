// SPDX-License-Identifier: GPL-3.0-or-later
//! Prisma dialogs launched by file-manager service menus via
//! `bigiris --dialog=<name>`. Shared form helpers live here; each dialog
//! has its own module.

use super::batch_runner::{format_size, run_batch_async};
use super::*;

/// Substitute `{name}` placeholders of a translated template in one pass,
/// so a value that itself contains `{…}` (a file name) is never expanded.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let value = after.find('}').and_then(|close| {
            let key = &after[..close];
            values.iter().find(|(name, _)| *name == key).map(|(_, value)| (close, *value))
        });
        match value {
            Some((close, value)) => {
                out.push_str(value);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

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
pub(in crate::gui) const CONVERT_FORMATS: &[(&str, Format)] = &[
    (n_("PNG"), Format::Png),
    (n_("JPG (JPEG)"), Format::Jpeg),
    (n_("WebP"), Format::WebP),
    (n_("AVIF"), Format::Avif),
    (n_("TIFF"), Format::Tiff),
    (n_("BMP"), Format::Bmp),
    (n_("GIF"), Format::Gif),
    (n_("ICO (até 256 × 256)"), Format::Ico),
    (n_("PNM (Netpbm)"), Format::Pnm),
    (n_("TGA"), Format::Tga),
    (n_("QOI"), Format::Qoi),
    (n_("HDR"), Format::Hdr),
    (n_("OpenEXR"), Format::OpenExr),
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
        let format = selected_format(&selected);
        quality.set_sensitive(matches!(format, Format::Jpeg | Format::Avif));
        optimize.set_sensitive(matches!(format, Format::Png | Format::Avif));
    };
    update();
    dialog_signal!(window, format_row, connect_selected_notify, move |_| update());
}

/// Overwrite choices shared by convert and resize dialogs.
const OVERWRITE_CHOICES: &[(&str, OverwritePolicy)] = &[
    (n_("Pular se já existir"), OverwritePolicy::Skip),
    (n_("Sobrescrever o arquivo"), OverwritePolicy::Replace),
    (n_("Incrementar (_1, _2, …)"), OverwritePolicy::Increment),
];

/// Run a Prisma dialog as its own GApplication instance. NON_UNIQUE: each
/// service-menu action is a separate process (see run_viewer); otherwise a
/// second dialog's activation is forwarded to the first process and lost.
fn run_dialog_app(activate: impl Fn(&adw::Application) + 'static) -> i32 {
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(activate);
    let code = app.run_with_args::<&str>(&[]);
    i32::from(u8::from(code))
}

/// "Formato de destino" combo listing [`CONVERT_FORMATS`].
fn format_row() -> adw::ComboRow {
    let names: Vec<&str> = CONVERT_FORMATS.iter().map(|(n, _)| *n).collect();
    adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Formato de destino"))
        .model(&translated_strings(&names))
        .build()
}

fn selected_format(row: &adw::ComboRow) -> Format {
    CONVERT_FORMATS[row.selected() as usize].1
}

/// "Formato de destino" combo whose first entry keeps the source format.
fn target_format_row() -> adw::ComboRow {
    let mut names = vec![n_("Manter o formato original")];
    names.extend(CONVERT_FORMATS.iter().map(|(n, _)| *n));
    adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Formato de destino"))
        .model(&translated_strings(&names))
        .build()
}

/// `None` keeps the source format; see [`target_format_row`].
fn selected_target_format(row: &adw::ComboRow) -> Option<Format> {
    match row.selected() {
        0 => None,
        n => Some(CONVERT_FORMATS[(n as usize) - 1].1),
    }
}

/// "Se o arquivo de saída existir" combo listing [`OVERWRITE_CHOICES`].
fn overwrite_row() -> adw::ComboRow {
    let names: Vec<&str> = OVERWRITE_CHOICES.iter().map(|(n, _)| *n).collect();
    adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Se o arquivo de saída existir"))
        .model(&translated_strings(&names))
        .build()
}

fn selected_overwrite(row: &adw::ComboRow) -> OverwritePolicy {
    OVERWRITE_CHOICES[row.selected() as usize].1
}

/// "<title>: N" row summarizing the selected files.
fn selection_row(title: &str, files: &[PathBuf]) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .use_markup(false)
        .title(format!("{title}: {}", files.len()))
        .build();
    row.set_subtitle(&files_subtitle(files));
    row
}

/// Dimmed, wrapping status line under a dialog form.
fn status_label() -> gtk::Label {
    let status = gtk::Label::builder().label("").wrap(true).xalign(0.0).build();
    status.add_css_class("dim-label");
    status
}

/// End-aligned Cancel + suggested `apply_label` buttons.
fn action_buttons(apply_label: &str) -> (gtk::Button, gtk::Button, gtk::Box) {
    let cancel_btn = gtk::Button::with_label(&gettext("Cancelar"));
    let apply_btn = gtk::Button::builder().label(apply_label).build();
    apply_btn.add_css_class("suggested-action");
    apply_btn.add_css_class("pill");
    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    actions.append(&cancel_btn);
    actions.append(&apply_btn);
    (cancel_btn, apply_btn, actions)
}

/// Close `window` when `button` is clicked.
fn close_on_click(window: &adw::ApplicationWindow, button: &gtk::Button) {
    let target = window.clone();
    dialog_signal!(window, button, connect_clicked, move |_| target.close());
}

fn files_subtitle(files: &[PathBuf]) -> String {
    if files.is_empty() {
        return "—".to_string();
    }
    let first = files[0].file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if files.len() == 1 {
        first
    } else {
        let others = files.len() - 1;
        // No plural forms: the catalog validator rejects msgid_plural, so
        // the wording avoids a counted noun. {first} is a file name.
        fill(
            &gettext("{first} e mais {count}"),
            &[("first", &first), ("count", &others.to_string())],
        )
    }
}

/// Abre FolderDialog; ao confirmar, atualiza `output_dir` e o subtitle
/// da row. Revela o botão "limpar" para o usuário voltar ao padrão.
fn pick_output_folder(
    window: &adw::ApplicationWindow,
    output_dir: &Rc<RefCell<Option<PathBuf>>>,
    row: &adw::ActionRow,
    clear_btn: &gtk::Button,
) {
    let dialog =
        gtk::FileDialog::builder().title(gettext("Escolher pasta de saída")).modal(true).build();
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

mod adjust;
mod animate;
mod batch;
mod compare;
mod convert;
mod crop;
mod extent;
mod flip;
mod properties;
mod remove_bg;
mod resize;
mod rotate;
mod upscale;

pub use adjust::*;
pub use animate::*;
pub use batch::*;
pub use compare::*;
pub use convert::*;
pub use crop::*;
pub use extent::*;
pub use flip::*;
pub use properties::*;
pub use remove_bg::*;
pub use resize::*;
pub use rotate::*;
pub use upscale::*;

#[cfg(test)]
mod fill_tests {
    use super::{files_subtitle, fill};
    use std::path::PathBuf;

    #[test]
    fn fills_named_placeholders_once() {
        assert_eq!(fill("{a} → {b}", &[("a", "{b}"), ("b", "x")]), "{b} → x");
        assert_eq!(fill("{unknown} {", &[("a", "1")]), "{unknown} {");
    }

    #[test]
    fn files_subtitle_counts_the_others() {
        let one = vec![PathBuf::from("/x/{count}.png")];
        assert_eq!(files_subtitle(&one), "{count}.png");
        let three = vec![one[0].clone(), PathBuf::from("b.png"), PathBuf::from("c.png")];
        assert_eq!(files_subtitle(&three), "{count}.png e mais 2");
        assert_eq!(files_subtitle(&three[..2]), "{count}.png e mais 1");
    }
}
