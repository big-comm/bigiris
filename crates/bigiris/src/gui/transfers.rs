// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded, cancellable file-manager transfers. Image decoding stays in the sandbox.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use gettextrs::gettext;
use gtk4::{gdk, gio, glib};
use libadwaita::{self as adw, prelude::*};

const MIME_TYPES: &[&str] = &["text/uri-list", "x-special/gnome-copied-files"];
const MAX_BYTES: usize = 64 * 1024;
const MAX_FILES: usize = 1024;

#[derive(Debug, PartialEq, Eq)]
enum TransferError {
    Invalid,
    TooLarge,
    TooManyFiles,
    Remote,
    Empty,
    Cancelled,
    Timeout,
    Launch,
}

fn parse_files(data: &[u8], mime: &str) -> Result<Vec<PathBuf>, TransferError> {
    if data.len() > MAX_BYTES {
        return Err(TransferError::TooLarge);
    }
    let text = std::str::from_utf8(data).map_err(|_| TransferError::Invalid)?;
    let mut lines = text.lines();
    match mime {
        "text/uri-list" => {}
        "x-special/gnome-copied-files" => {
            if !matches!(lines.next(), Some("copy" | "cut")) {
                return Err(TransferError::Invalid);
            }
        }
        _ => return Err(TransferError::Invalid),
    }
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for line in lines {
        let uri = line.trim();
        if uri.is_empty() || uri.starts_with('#') {
            continue;
        }
        if !uri.starts_with("file://") {
            return Err(TransferError::Remote);
        }
        // GFile may discard a file URI's hostname, so validate it before conversion.
        let authority = uri[7..].split('/').next().unwrap_or_default();
        if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
            return Err(TransferError::Remote);
        }
        if uri.contains('\0') || uri.to_ascii_lowercase().contains("%00") {
            return Err(TransferError::Invalid);
        }
        let path = gio::File::for_uri(uri).path().ok_or(TransferError::Remote)?;
        if !path.is_absolute() {
            return Err(TransferError::Invalid);
        }
        if seen.insert(path.clone()) {
            paths.push(path);
            if paths.len() > MAX_FILES {
                return Err(TransferError::TooManyFiles);
            }
        }
    }
    if paths.is_empty() {
        Err(TransferError::Empty)
    } else {
        Ok(paths)
    }
}

fn show_error(window: &adw::ApplicationWindow, error: TransferError) {
    if !window.is_visible() || error == TransferError::Cancelled {
        return;
    }
    let body = match error {
        TransferError::TooLarge | TransferError::TooManyFiles => {
            gettext("A seleção é muito grande. Abra menos arquivos por vez.")
        }
        TransferError::Remote => gettext("Arraste ou copie arquivos locais de imagem."),
        TransferError::Timeout => gettext("A transferência demorou demais. Tente novamente."),
        TransferError::Launch => {
            gettext("Não foi possível iniciar o visualizador. Tente novamente.")
        }
        _ => gettext("Copie arquivos de imagem no gerenciador e tente novamente."),
    };
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Não foi possível abrir os arquivos"))
        .body(body)
        .build();
    dialog.add_response("close", &gettext("Fechar"));
    dialog.set_close_response("close");
    dialog.present(Some(window));
}

pub(super) fn open_files(
    window: &adw::ApplicationWindow,
    paths: Vec<PathBuf>,
    close_after_open: bool,
) -> bool {
    if paths.is_empty() {
        return false;
    }
    let result = std::env::current_exe()
        .and_then(|exe| std::process::Command::new(exe).arg("--").args(paths).spawn());
    match result {
        Ok(_) => {
            if close_after_open {
                window.close();
            }
            true
        }
        Err(_) => {
            show_error(window, TransferError::Launch);
            false
        }
    }
}

struct Transfer {
    window: glib::WeakRef<adw::ApplicationWindow>,
    close_after_open: bool,
    busy: Cell<bool>,
    cancellable: RefCell<gio::Cancellable>,
    deadline: RefCell<Option<glib::SourceId>>,
}

impl Transfer {
    fn start(self: &Rc<Self>) -> bool {
        if self.busy.replace(true) {
            return false;
        }
        self.cancellable.replace(gio::Cancellable::new());
        let weak = Rc::downgrade(self);
        *self.deadline.borrow_mut() = Some(glib::timeout_add_seconds_local_once(15, move || {
            if let Some(transfer) = weak.upgrade() {
                transfer.deadline.borrow_mut().take();
                transfer.cancellable.borrow().cancel();
            }
        }));
        true
    }

    fn finish(&self, result: Result<Vec<PathBuf>, TransferError>) -> bool {
        let timed_out = self.deadline.borrow().is_none();
        if let Some(deadline) = self.deadline.borrow_mut().take() {
            deadline.remove();
        }
        self.busy.set(false);
        let Some(window) = self.window.upgrade().filter(|w| w.is_visible()) else {
            return false;
        };
        match result {
            Ok(paths) => open_files(&window, paths, self.close_after_open),
            Err(error) => {
                let error = if timed_out && error == TransferError::Cancelled {
                    TransferError::Timeout
                } else {
                    error
                };
                show_error(&window, error);
                false
            }
        }
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        if let Some(deadline) = self.deadline.get_mut().take() {
            deadline.remove();
        }
        self.cancellable.get_mut().cancel();
    }
}

type ReadDone = Box<dyn FnOnce(Result<Vec<u8>, TransferError>)>;

fn read_bounded(
    stream: gio::InputStream,
    mut data: Vec<u8>,
    cancel: gio::Cancellable,
    done: ReadDone,
) {
    let next_stream = stream.clone();
    let next_cancel = cancel.clone();
    // One extra byte distinguishes a complete maximum-size payload from overflow.
    let count = (MAX_BYTES + 1 - data.len()).min(4096);
    stream.read_bytes_async(count, glib::Priority::DEFAULT, Some(&cancel), move |result| {
        match result {
            Ok(bytes) if bytes.is_empty() => done(Ok(data)),
            Ok(bytes) => {
                data.extend_from_slice(bytes.as_ref());
                if data.len() > MAX_BYTES {
                    done(Err(TransferError::TooLarge));
                } else {
                    read_bounded(next_stream, data, next_cancel, done);
                }
            }
            Err(error) if error.matches(gio::IOErrorEnum::Cancelled) => {
                done(Err(TransferError::Cancelled));
            }
            Err(_) => done(Err(TransferError::Invalid)),
        }
    });
}

fn receive(
    transfer: Rc<Transfer>,
    result: Result<(gio::InputStream, glib::GString), glib::Error>,
    drop: Option<gdk::Drop>,
) {
    let cancel = transfer.cancellable.borrow().clone();
    let complete = move |result| {
        let accepted = transfer.finish(result);
        if let Some(drop) = drop {
            drop.finish(if accepted { gdk::DragAction::COPY } else { gdk::DragAction::empty() });
        }
    };
    match result {
        Ok((stream, mime)) => {
            // The callback owns no window; closing it cancels outstanding I/O.
            read_bounded(
                stream,
                Vec::new(),
                cancel,
                Box::new(move |result| {
                    complete(result.and_then(|data| parse_files(&data, &mime)));
                }),
            );
        }
        Err(error) if error.matches(gio::IOErrorEnum::Cancelled) => {
            complete(Err(TransferError::Cancelled));
        }
        Err(_) => complete(Err(TransferError::Invalid)),
    }
}

pub(super) fn install(
    window: &adw::ApplicationWindow,
    drop_area: &impl IsA<gtk4::Widget>,
    close_after_open: bool,
) {
    let transfer = Rc::new(Transfer {
        window: window.downgrade(),
        close_after_open,
        busy: Cell::new(false),
        cancellable: RefCell::new(gio::Cancellable::new()),
        deadline: RefCell::new(None),
    });
    let weak = Rc::downgrade(&transfer);
    window.connect_unrealize(move |_| {
        if let Some(transfer) = weak.upgrade() {
            transfer.cancellable.borrow().cancel();
        }
    });
    let paste = gio::SimpleAction::new("paste", None);
    let context = transfer.clone();
    paste.connect_activate(move |_, _| {
        let Some(window) = context.window.upgrade() else { return };
        if !context.start() {
            return;
        }
        let transfer = context.clone();
        window.clipboard().read_async(
            MIME_TYPES,
            glib::Priority::DEFAULT,
            Some(&context.cancellable.borrow().clone()),
            move |result| receive(transfer, result, None),
        );
    });
    window.add_action(&paste);
    if let Some(app) = window.application() {
        app.set_accels_for_action("win.paste", &["<Ctrl>v"]);
    }
    let target = gtk4::DropTargetAsync::new(
        Some(gdk::ContentFormats::new(MIME_TYPES)),
        gdk::DragAction::COPY,
    );
    target.connect_drop(move |_, drop, _, _| {
        if !transfer.start() {
            return false;
        }
        let context = transfer.clone();
        let drop_for_finish = drop.clone();
        drop.read_async(
            MIME_TYPES,
            glib::Priority::DEFAULT,
            Some(&transfer.cancellable.borrow().clone()),
            move |result| receive(context, result, Some(drop_for_finish)),
        );
        true
    });
    drop_area.add_controller(target);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_files_keep_selection_order_and_decode_escaped_names() {
        let paths = parse_files(
            b"# selected files\r\nfile:///tmp/second%20image.png\r\nfile:///tmp/first.png\r\nfile:///tmp/second%20image.png\r\n",
            "text/uri-list",
        ).unwrap();
        assert_eq!(
            paths,
            vec![PathBuf::from("/tmp/second image.png"), PathBuf::from("/tmp/first.png")]
        );
    }

    #[test]
    fn nautilus_copy_and_cut_only_open_files() {
        for operation in ["copy", "cut"] {
            assert_eq!(
                parse_files(
                    format!("{operation}\nfile:///tmp/image.png").as_bytes(),
                    "x-special/gnome-copied-files"
                )
                .unwrap(),
                vec![PathBuf::from("/tmp/image.png")]
            );
        }
        assert_eq!(
            parse_files(b"move\nfile:///tmp/image.png", "x-special/gnome-copied-files"),
            Err(TransferError::Invalid)
        );
    }

    #[test]
    fn remote_and_mixed_selections_are_rejected_without_partial_open() {
        for uri in
            ["https://example.org/image.png", "smb://server/image.png", "file://server/image.png"]
        {
            assert_eq!(
                parse_files(format!("file:///tmp/local.png\n{uri}").as_bytes(), "text/uri-list"),
                Err(TransferError::Remote)
            );
        }
    }

    #[test]
    fn invalid_and_empty_payloads_are_rejected() {
        for bytes in [b"file:///tmp/a%00.png".as_slice(), b"file:///tmp/a\0.png", b"\xff"] {
            assert_eq!(parse_files(bytes, "text/uri-list"), Err(TransferError::Invalid));
        }
        assert_eq!(parse_files(b"file:///tmp/a.png", "text/plain"), Err(TransferError::Invalid));
        assert_eq!(parse_files(b"# no selection\n\n", "text/uri-list"), Err(TransferError::Empty));
    }

    #[test]
    fn transfer_limits_are_enforced_before_launch() {
        assert_eq!(
            parse_files(&vec![b'a'; MAX_BYTES + 1], "text/uri-list"),
            Err(TransferError::TooLarge)
        );
        let list = (0..=MAX_FILES).map(|i| format!("file:///tmp/{i}.png\n")).collect::<String>();
        assert_eq!(parse_files(list.as_bytes(), "text/uri-list"), Err(TransferError::TooManyFiles));
    }

    #[test]
    fn file_names_are_data_including_shell_metacharacters() {
        assert_eq!(
            parse_files(b"file:///tmp/--help.png\nfile:///tmp/%24%28id%29.png", "text/uri-list")
                .unwrap(),
            vec![PathBuf::from("/tmp/--help.png"), PathBuf::from("/tmp/$(id).png")]
        );
    }

    fn read_memory(data: Vec<u8>, cancelled: bool) -> Result<Vec<u8>, TransferError> {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_owned(data));
                let cancel = gio::Cancellable::new();
                if cancelled {
                    cancel.cancel();
                }
                let result = Rc::new(RefCell::new(None));
                let output = result.clone();
                read_bounded(
                    stream.upcast(),
                    Vec::new(),
                    cancel,
                    Box::new(move |value| {
                        *output.borrow_mut() = Some(value);
                    }),
                );
                while result.borrow().is_none() {
                    context.iteration(true);
                }
                let value = result.borrow_mut().take().unwrap();
                value
            })
            .unwrap()
    }

    #[test]
    fn stream_reads_across_chunks_and_accepts_exact_limit() {
        let data = vec![b'a'; MAX_BYTES];
        assert_eq!(read_memory(data.clone(), false), Ok(data));
    }

    #[test]
    fn stream_stops_at_overflow_and_supports_cancellation() {
        assert_eq!(read_memory(vec![b'a'; MAX_BYTES + 4096], false), Err(TransferError::TooLarge));
        assert_eq!(read_memory(vec![b'a'; 4096], true), Err(TransferError::Cancelled));
    }
}
