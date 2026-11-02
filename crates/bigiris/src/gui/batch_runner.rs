// SPDX-License-Identifier: GPL-3.0-or-later
//! Async batch primitives shared across every Prisma dialog.
//!
//! Each per-file Prisma dialog (Convert, Resize, Rotate, Flip, Crop,
//! Adjust, Upscale) delegates its Apply handler to [`run_batch_async`],
//! which fans the work out to a pool of worker threads (one per CPU core
//! minus the one we leave for the GUI) and polls progress on the GTK
//! main loop at 20 Hz. The Animate and remove-bg flows do not use this
//! helper because they have one-shot or AI-specific shapes.
//!
//! Parallelism: the closure must be `Fn + Send + Sync` so multiple
//! threads can call it concurrently. The 7 dialog call sites pass
//! closures of the form `move |f| convert_file(f, fmt, &opts, policy)`
//! which are already `Fn` (target/opts/policy are `Copy`). Workers
//! coordinate via a shared `AtomicUsize` index into the input `Vec`,
//! so each file is processed exactly once with no Mutex contention on
//! the queue.
//!
//! Cancellation is wired through `connect_close_request` on the
//! window: clicking close flips an atomic flag the workers check
//! between files, so an in-flight encode finishes but the remaining
//! batch is skipped cleanly.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use bigimage_core::ConvertOutcome;
use gettextrs::gettext;
use gtk4 as gtk;
use gtk4::prelude::*;
use libadwaita as adw;

/// Render a byte count as `12 B` / `456 KB` / `1.2 MB` / `3.4 GB`.
/// Mirrors `gui::format_size` (kept private to that module to avoid a
/// circular re-export); tests sit at the bottom of this file.
pub(super) fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Render an elapsed time as `0.4 s` / `12 s` / `1 min 23 s` / `2h 5min`.
pub(super) fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        if secs < 10.0 {
            format!("{secs:.1} s")
        } else {
            format!("{secs:.0} s")
        }
    } else if secs < 3600.0 {
        let mins = (secs / 60.0) as u64;
        let rem = (secs - (mins as f64 * 60.0)).round() as u64;
        format!("{mins} min {rem} s")
    } else {
        let hours = (secs / 3600.0) as u64;
        let rem = ((secs - (hours as f64 * 3600.0)) / 60.0).round() as u64;
        format!("{hours}h {rem}min")
    }
}

/// Render a `bytes_in → bytes_out` size delta as `−83%` / `+4%` / `≈ same`.
/// Empty string when either side is zero (no comparison to make).
pub(super) fn format_size_delta(bytes_in: u64, bytes_out: u64) -> String {
    if bytes_in == 0 || bytes_out == 0 {
        return String::new();
    }
    let ratio = (bytes_out as f64) / (bytes_in as f64);
    let pct = (ratio - 1.0) * 100.0;
    if pct.abs() < 0.5 {
        gettext("≈ mesmo tamanho")
    } else if pct < 0.0 {
        format!("−{:.0}%", pct.abs())
    } else {
        format!("+{:.0}%", pct)
    }
}

/// A pending auto-close: the window (weakly), its timer and the
/// `unrealize` handler that drops the timer with the window.
type PendingClose = (
    gtk4::glib::WeakRef<adw::ApplicationWindow>,
    gtk4::glib::SourceId,
    gtk4::glib::SignalHandlerId,
);

thread_local! {
    /// Auto-close timers armed by [`arm_auto_close`], per window. A new run
    /// disarms them so a quick second Apply is not closed mid-batch.
    static PENDING_CLOSE: std::cell::RefCell<Vec<PendingClose>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Cancel a pending auto-close for `window`. Every dialog calls this when a
/// new run starts; unrealizing the window does it too.
pub(super) fn disarm_auto_close(window: &adw::ApplicationWindow) {
    let mine: Vec<_> = PENDING_CLOSE.with(|pending| {
        let mut pending = pending.borrow_mut();
        let (mine, others): (Vec<_>, Vec<_>) = std::mem::take(&mut *pending)
            .into_iter()
            .partition(|(weak, _, _)| weak.upgrade().as_ref() == Some(window));
        // Entries of finalized windows: their timers can only no-op now.
        *pending = others.into_iter().filter(|(weak, _, _)| weak.upgrade().is_some()).collect();
        mine
    });
    for (_, id, handler) in mine {
        id.remove();
        window.disconnect(handler);
    }
}

/// Whether `window` has an auto-close armed.
#[cfg(test)]
pub(super) fn auto_close_armed(window: &adw::ApplicationWindow) -> bool {
    PENDING_CLOSE.with(|pending| {
        pending.borrow().iter().any(|(weak, _, _)| weak.upgrade().as_ref() == Some(window))
    })
}

/// Close `window` after `seconds` unless [`disarm_auto_close`] runs first
/// (a new run, or the window going away). The timer holds only a weak
/// reference, so it never keeps the dialog alive. Shared by every dialog.
pub(super) fn arm_auto_close(window: &adw::ApplicationWindow, seconds: u32) {
    disarm_auto_close(window);
    let weak = window.downgrade();
    let id = gtk4::glib::timeout_add_seconds_local_once(seconds, move || {
        let Some(window) = weak.upgrade() else { return };
        // The source has fired: forget it instead of removing it again.
        let fired: Vec<_> = PENDING_CLOSE.with(|pending| {
            let mut pending = pending.borrow_mut();
            let (fired, others): (Vec<_>, Vec<_>) = std::mem::take(&mut *pending)
                .into_iter()
                .partition(|(other, _, _)| other.upgrade().as_ref() == Some(&window));
            *pending = others;
            fired
        });
        for (_, _, handler) in fired {
            window.disconnect(handler);
        }
        window.close();
    });
    let handler = window.connect_unrealize(disarm_auto_close);
    PENDING_CLOSE.with(|pending| pending.borrow_mut().push((window.downgrade(), id, handler)));
}

/// Summary counts. The catalog validator has no plural support, so a single
/// message with named placeholders keeps the counts translatable.
pub(super) fn format_counts(ok: usize, skip: usize, fail: usize) -> String {
    gettext("{written} gravado(s), {skipped} ignorado(s), {failed} falha(s)")
        .replace("{written}", &ok.to_string())
        .replace("{skipped}", &skip.to_string())
        .replace("{failed}", &fail.to_string())
}

/// Render the final state of a Prisma dialog after a batch finishes.
/// Re-enables the Apply / Cancel buttons, prints the final summary
/// (`ok/skip/fail` counts + elapsed time + before/after size delta when
/// available), and triggers a 2-second auto-close on clean success so
/// service-menu callers don't need to dismiss the window manually.
#[allow(clippy::too_many_arguments)]
pub(super) fn finish_dialog(
    status: &gtk::Label,
    apply_btn: &gtk::Button,
    cancel_btn: &gtk::Button,
    window: &adw::ApplicationWindow,
    ok: usize,
    skip: usize,
    fail: usize,
    first_err: Option<String>,
    elapsed: Duration,
    bytes_in: u64,
    bytes_out: u64,
) {
    // Headline: counts + elapsed. Size delta on the next line when we
    // have both totals (skipped files contribute to bytes_in but not
    // bytes_out, so the comparison is fair only when ok > 0).
    let counts = format_counts(ok, skip, fail);
    let timing = format_duration(elapsed);
    let mut msg = gettext("Concluído em {time} · {counts}")
        .replace("{time}", &timing)
        .replace("{counts}", &counts);
    if ok > 0 && bytes_in > 0 && bytes_out > 0 {
        let delta = format_size_delta(bytes_in, bytes_out);
        let avg_in = bytes_in / (ok as u64).max(1);
        let avg_out = bytes_out / (ok as u64).max(1);
        msg.push('\n');
        msg.push_str(
            &gettext("{before} → {after} ({delta}) · média por arquivo {average_before} → {average_after}")
                .replace("{before}", &format_size(bytes_in))
                .replace("{after}", &format_size(bytes_out))
                .replace("{delta}", &delta)
                .replace("{average_before}", &format_size(avg_in))
                .replace("{average_after}", &format_size(avg_out)),
        );
    }
    if let Some(err) = first_err {
        msg.push('\n');
        msg.push_str(&err);
    }
    status.set_text(&msg);
    apply_btn.set_sensitive(true);
    cancel_btn.set_sensitive(true);
    if fail == 0 {
        arm_auto_close(window, 2);
    }
}

/// The worker pool vanished without its summary: say so and let the user
/// retry or close instead of leaving both buttons disabled forever.
fn abort_dialog(status: &gtk::Label, apply_btn: &gtk::Button, cancel_btn: &gtk::Button) {
    status.set_text(&gettext("O processamento terminou inesperadamente. Tente novamente."));
    apply_btn.set_sensitive(true);
    cancel_btn.set_sensitive(true);
}

/// Progress message sent from the worker pool to the UI poll.
pub(super) enum AsyncBatchEvent {
    /// One file finished (in any of the workers, in any order). `done`
    /// is the cumulative count of finished files; `total` is fixed at
    /// the start. `file` is the bare file name (no directory) of the
    /// most recently completed input.
    Progress { done: usize, total: usize, file: String },
    /// All workers exited (cleanly or via cancel). Carries the merged
    /// summary for [`finish_dialog`] plus the elapsed wall-clock time
    /// and total bytes in / out so the dialog can display "12 s · 8.5
    /// MB → 1.2 MB (−85%)".
    Done {
        ok: usize,
        skip: usize,
        fail: usize,
        first_err: Option<String>,
        elapsed: Duration,
        bytes_in: u64,
        bytes_out: u64,
        done: usize,
        total: usize,
        cancelled: bool,
    },
}

/// Spawn the GTK-independent worker pool. The generic dialog and the richer
/// folder/batch window share this exact engine, so progress, cancellation,
/// parallelism and accounting cannot drift apart again.
pub(super) fn spawn_batch_workers<W>(
    files: Vec<PathBuf>,
    work: W,
    cancelled: Arc<AtomicBool>,
) -> mpsc::Receiver<AsyncBatchEvent>
where
    W: Fn(&Path) -> bigimage_core::Result<ConvertOutcome> + Send + Sync + 'static,
{
    let (tx, rx) = mpsc::channel::<AsyncBatchEvent>();
    std::thread::spawn(move || {
        let total = files.len();
        let start = Instant::now();
        let bytes_in = Arc::new(AtomicU64::new(0));
        let files = Arc::new(files);
        let next_idx = Arc::new(AtomicUsize::new(0));
        let progress_done = Arc::new(AtomicUsize::new(0));
        let bytes_out = Arc::new(AtomicU64::new(0));
        let work = Arc::new(work);
        let n_workers = bigimage_core::resources::batch_worker_count(&files);

        let workers: Vec<_> = (0..n_workers)
            .map(|_| {
                let files = files.clone();
                let next_idx = next_idx.clone();
                let progress_done = progress_done.clone();
                let bytes_out = bytes_out.clone();
                let bytes_in = bytes_in.clone();
                let cancelled = cancelled.clone();
                let work = work.clone();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let mut ok = 0usize;
                    let mut skip = 0usize;
                    let mut fail = 0usize;
                    let mut first_err: Option<String> = None;
                    loop {
                        if cancelled.load(Ordering::Relaxed) {
                            break;
                        }
                        let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                        if idx >= total {
                            break;
                        }
                        let path = &files[idx];
                        let display = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let input_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                        // Scoped cancel lets closing the window stop an
                        // isolated codec run instead of waiting up to 60 s.
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            crate::codec::with_cancel(cancelled.clone(), || work(path))
                        }))
                        .unwrap_or_else(|_| {
                            Err(bigimage_core::BigImageError::Other(gettext(
                                "falha inesperada no worker",
                            )))
                        });
                        match result {
                            Ok(ConvertOutcome::Written { output }) => {
                                ok += 1;
                                bytes_in.fetch_add(input_bytes, Ordering::Relaxed);
                                if let Ok(meta) = std::fs::metadata(&output) {
                                    bytes_out.fetch_add(meta.len(), Ordering::Relaxed);
                                }
                            }
                            Ok(ConvertOutcome::Skipped { .. }) => skip += 1,
                            Err(e) => {
                                fail += 1;
                                if first_err.is_none() {
                                    first_err = Some(format!("{}: {e}", path.display()));
                                }
                            }
                        }
                        let done = progress_done.fetch_add(1, Ordering::Relaxed) + 1;
                        if tx
                            .send(AsyncBatchEvent::Progress { done, total, file: display })
                            .is_err()
                        {
                            break;
                        }
                    }
                    (ok, skip, fail, first_err)
                })
            })
            .collect();

        let mut ok = 0usize;
        let mut skip = 0usize;
        let mut fail = 0usize;
        let mut first_err: Option<String> = None;
        for handle in workers {
            match handle.join() {
                Ok((local_ok, local_skip, local_fail, local_first_err)) => {
                    ok += local_ok;
                    skip += local_skip;
                    fail += local_fail;
                    if first_err.is_none() {
                        first_err = local_first_err;
                    }
                }
                Err(_) => {
                    fail += 1;
                    first_err.get_or_insert_with(|| gettext("um worker terminou inesperadamente"));
                }
            }
        }
        let done = progress_done.load(Ordering::Relaxed);
        let _ = tx.send(AsyncBatchEvent::Done {
            ok,
            skip,
            fail,
            first_err,
            elapsed: start.elapsed(),
            bytes_in: bytes_in.load(Ordering::Relaxed),
            bytes_out: bytes_out.load(Ordering::Relaxed),
            done,
            total,
            cancelled: cancelled.load(Ordering::Relaxed),
        });
    });
    rx
}

/// Run a per-file batch across a worker pool, polling progress on the
/// GTK main loop. The `work` closure is the per-file CPU op (e.g.
/// `bigimage_core::convert_file`); it must be `Fn + Send + Sync +
/// 'static` so multiple threads can invoke it concurrently. Widgets
/// (`status`, buttons, window) stay on the main thread — they're only
/// touched inside the timeout closure, which runs locally.
///
/// Cancellation: closing the window flips an atomic flag the workers
/// check between files and that aborts an in-flight isolated codec run.
/// An encode already in progress finishes and publishes atomically. The
/// application is held until every worker has exited, so closing the last
/// window never kills a worker mid-write and strands `.part` files.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_batch_async<W>(
    files: Vec<PathBuf>,
    work: W,
    busy_label: String,
    status: gtk::Label,
    apply_btn: gtk::Button,
    cancel_btn: gtk::Button,
    window: adw::ApplicationWindow,
) where
    W: Fn(&Path) -> bigimage_core::Result<ConvertOutcome> + Send + Sync + 'static,
{
    let cancelled = Arc::new(AtomicBool::new(false));
    disarm_auto_close(&window);

    {
        let cancelled = cancelled.clone();
        window.connect_close_request(move |_| {
            cancelled.store(true, Ordering::Release);
            gtk4::glib::Propagation::Proceed
        });
    }

    let rx = spawn_batch_workers(files, work, cancelled);
    // Released with the poll closure, i.e. only after the pool reported back.
    let hold = window.application().map(|app| app.hold());

    gtk4::glib::timeout_add_local(std::time::Duration::from_millis(50), move || loop {
        let _hold = &hold;
        match rx.try_recv() {
            Ok(AsyncBatchEvent::Progress { done, total, file }) => {
                status.set_text(&format!("{busy_label} {done}/{total} — {file}"));
            }
            Ok(AsyncBatchEvent::Done {
                ok,
                skip,
                fail,
                first_err,
                elapsed,
                bytes_in,
                bytes_out,
                cancelled,
                ..
            }) => {
                // Closed mid-run: nothing left to show or to auto-close.
                if cancelled && !window.is_visible() {
                    return gtk4::glib::ControlFlow::Break;
                }
                finish_dialog(
                    &status,
                    &apply_btn,
                    &cancel_btn,
                    &window,
                    ok,
                    skip,
                    fail,
                    first_err,
                    elapsed,
                    bytes_in,
                    bytes_out,
                );
                return gtk4::glib::ControlFlow::Break;
            }
            Err(mpsc::TryRecvError::Empty) => return gtk4::glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => {
                abort_dialog(&status, &apply_btn, &cancel_btn);
                return gtk4::glib::ControlFlow::Break;
            }
        }
    });
}

#[cfg(test)]
mod worker_tests {
    use super::*;

    fn summary(rx: mpsc::Receiver<AsyncBatchEvent>) -> AsyncBatchEvent {
        loop {
            let event = rx.recv_timeout(Duration::from_secs(5)).expect("workers failed to finish");
            if matches!(event, AsyncBatchEvent::Done { .. }) {
                return event;
            }
        }
    }

    #[test]
    fn cancellation_before_start_processes_no_files() {
        let cancelled = Arc::new(AtomicBool::new(true));
        let rx = spawn_batch_workers(
            vec![PathBuf::from("unread.png")],
            |_| panic!("work after cancellation"),
            cancelled,
        );
        assert!(matches!(summary(rx), AsyncBatchEvent::Done { done: 0, cancelled: true, .. }));
    }

    #[test]
    fn closing_mid_run_reaches_the_isolated_codec() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let trigger = cancelled.clone();
        let rx = spawn_batch_workers(
            vec![PathBuf::from("first.png")],
            move |path| {
                assert!(!crate::codec::cancelled());
                trigger.store(true, Ordering::Release);
                // The codec observes the same flag mid-file, not only between files.
                assert!(crate::codec::cancelled());
                Ok(ConvertOutcome::Skipped { output: path.to_path_buf() })
            },
            cancelled,
        );
        assert!(matches!(summary(rx), AsyncBatchEvent::Done { skip: 1, cancelled: true, .. }));
    }

    #[test]
    fn counts_fill_every_placeholder() {
        assert_eq!(format_counts(1, 0, 2), "1 gravado(s), 0 ignorado(s), 2 falha(s)");
    }

    #[test]
    fn worker_panic_is_reported_as_failure_without_losing_other_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<_> = (0..10)
            .map(|i| {
                let path = dir.path().join(format!("{i}.png"));
                image::RgbImage::new(4, 4).save(&path).unwrap();
                path
            })
            .collect();
        let rx = spawn_batch_workers(
            files,
            |path| {
                if path.file_name().unwrap() == "3.png" {
                    panic!("synthetic failure");
                }
                Ok(ConvertOutcome::Skipped { output: path.to_path_buf() })
            },
            Arc::new(AtomicBool::new(false)),
        );
        assert!(matches!(summary(rx), AsyncBatchEvent::Done { fail: 1, skip: 9, done: 10, .. }));
    }

    #[test]
    fn compression_accounting_excludes_skipped_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let written = dir.path().join("written.png");
        let skipped = dir.path().join("skipped.png");
        std::fs::write(&written, [0u8; 10]).unwrap();
        std::fs::write(&skipped, [0u8; 100]).unwrap();
        let rx = spawn_batch_workers(
            vec![written, skipped],
            |path| {
                if path.file_name().unwrap() == "written.png" {
                    Ok(ConvertOutcome::Written { output: path.to_path_buf() })
                } else {
                    Ok(ConvertOutcome::Skipped { output: path.to_path_buf() })
                }
            },
            Arc::new(AtomicBool::new(false)),
        );
        assert!(matches!(
            summary(rx),
            AsyncBatchEvent::Done { ok: 1, skip: 1, bytes_in: 10, bytes_out: 10, .. }
        ));
    }
}
