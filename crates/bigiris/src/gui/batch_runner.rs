// SPDX-License-Identifier: GPL-3.0-or-later
//! The worker pool every Prisma dialog and the batch window run files on.
//! Inputs are planned like the CLI's ([`crate::output_plan`]) and each group
//! writing one file runs in order on one of at most
//! [`bigimage_core::resources::batch_worker_count`] workers; progress wakes
//! the GTK loop, and closing the window cancels the run, a file in the
//! isolated codec included.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use bigimage_core::ConvertOutcome;
use gettextrs::gettext;
use gtk4 as gtk;
use gtk4::prelude::*;
use libadwaita as adw;

/// Render an elapsed time as `0.4 s` / `12 s` / `1 min 23 s` / `2h 5min`.
pub(super) fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        if secs < 10.0 { format!("{secs:.1} s") } else { format!("{secs:.0} s") }
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
        gettext("≈ same size")
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
    gettext("written: {written}, skipped: {skipped}, failed: {failed}")
        .replace("{written}", &ok.to_string())
        .replace("{skipped}", &skip.to_string())
        .replace("{failed}", &fail.to_string())
}

/// `12 MB → 3 MB (−75%) · average per file …` for `ok` written files of
/// `bytes_in` that became `bytes_out`; `None` when nothing was written.
pub(super) fn size_change_text(ok: usize, bytes_in: u64, bytes_out: u64) -> Option<String> {
    if ok == 0 || bytes_in == 0 || bytes_out == 0 {
        return None;
    }
    let files = ok as u64;
    Some(crate::fill(
        &gettext(
            "{before} → {after} ({delta}) · average per file {average_before} → {average_after}",
        ),
        &[
            ("before", &gtk4::glib::format_size(bytes_in)),
            ("after", &gtk4::glib::format_size(bytes_out)),
            ("delta", &format_size_delta(bytes_in, bytes_out)),
            ("average_before", &gtk4::glib::format_size(bytes_in / files)),
            ("average_after", &gtk4::glib::format_size(bytes_out / files)),
        ],
    ))
}

/// Show a finished run's summary and enable Apply again. Only a run that
/// wrote every input closes the dialog by itself (after 2 s); a skip or a
/// failure stays on screen to be read.
pub(super) fn finish_dialog(
    status: &gtk::Label,
    apply_btn: &gtk::Button,
    window: &adw::ApplicationWindow,
    summary: RunSummary,
) {
    let RunSummary { ok, skip, fail, first_err, elapsed, bytes_in, bytes_out, single, .. } =
        summary;
    // Headline: counts and elapsed time; the size delta follows when files
    // were written (both totals count written files only).
    let counts = format_counts(ok, skip, fail);
    let timing = format_duration(elapsed);
    let mut msg = gettext("Finished in {time} · {counts}")
        .replace("{time}", &timing)
        .replace("{counts}", &counts);
    if let Some(sizes) = size_change_text(ok, bytes_in, bytes_out) {
        msg.push('\n');
        msg.push_str(&sizes);
    }
    let single = match &single {
        Some(ConvertOutcome::Written { output }) => Some((gettext("Saved: {path}"), output)),
        Some(ConvertOutcome::Skipped { output }) => {
            Some((gettext("Already exists, nothing written: {path}"), output))
        }
        None => None,
    };
    if let Some((template, output)) = single {
        msg.push('\n');
        msg.push_str(&crate::fill(&template, &[("path", &crate::codec::printable_path(output))]));
    }
    if let Some(err) = first_err {
        msg.push('\n');
        msg.push_str(&err);
    }
    status.set_text(&msg);
    apply_btn.set_sensitive(true);
    if ok > 0 && skip == 0 && fail == 0 {
        arm_auto_close(window, 2);
    }
}

/// The worker pool vanished without its summary: say so and let the user
/// retry instead of leaving Apply disabled forever.
pub(super) fn abort_dialog(status: &gtk::Label, apply_btn: &gtk::Button) {
    status.set_text(&gettext("Processing ended unexpectedly. Please try again."));
    apply_btn.set_sensitive(true);
}

/// What a finished run did, merged over its workers.
pub(super) struct RunSummary {
    pub(super) ok: usize,
    pub(super) skip: usize,
    pub(super) fail: usize,
    pub(super) first_err: Option<String>,
    /// Wall-clock time of the run.
    pub(super) elapsed: Duration,
    /// Sizes of the written files and of their inputs.
    pub(super) bytes_in: u64,
    pub(super) bytes_out: u64,
    /// Inputs finished, of `total`.
    pub(super) done: usize,
    pub(super) total: usize,
    pub(super) cancelled: bool,
    /// The outcome of the only input of a one-file run.
    pub(super) single: Option<ConvertOutcome>,
}

/// Progress message sent from the worker pool to the UI poll.
pub(super) enum AsyncBatchEvent {
    /// Several inputs would write the same file and the policy cannot
    /// resolve it: nothing ran. The text names the colliding inputs.
    Refused(String),
    /// One file finished (in any of the workers, in any order). `done`
    /// is the cumulative count of finished files; `total` is fixed at
    /// the start. `file` is the bare file name (no directory) of the
    /// most recently completed input.
    Progress { done: usize, total: usize, file: String },
    /// All workers exited (cleanly or via cancel).
    Done(RunSummary),
}

/// Events of one run, awaited on the GTK loop without a timer.
pub(super) struct BatchEvents {
    rx: mpsc::Receiver<AsyncBatchEvent>,
    wake: super::viewer::Wake,
}

impl BatchEvents {
    /// The next event; `None` once the pool is gone and nothing is left.
    pub(super) async fn next(&self) -> Option<AsyncBatchEvent> {
        loop {
            match self.rx.try_recv() {
                Ok(event) => return Some(event),
                Err(mpsc::TryRecvError::Disconnected) => return None,
                Err(mpsc::TryRecvError::Empty) => {
                    if !self.wake.wait().await {
                        return self.rx.try_recv().ok();
                    }
                }
            }
        }
    }
}

/// Plan `files` under `naming` as the CLI does and run them on the shared
/// pool: a group writing one file runs in order on one worker, and a
/// collision the policy cannot resolve is refused before any work.
pub(super) fn spawn_batch_workers<W>(
    files: Vec<PathBuf>,
    naming: crate::output_plan::Naming,
    work: W,
    cancelled: Arc<AtomicBool>,
) -> BatchEvents
where
    W: Fn(&Path) -> bigimage_core::Result<ConvertOutcome> + Send + Sync + 'static,
{
    let (tx, rx) = mpsc::channel::<AsyncBatchEvent>();
    let wake = super::viewer::Wake::default();
    let events = BatchEvents { rx, wake: wake.clone() };
    std::thread::spawn(move || {
        // Ends the wait on the GTK side after `Done`, or if this panics.
        struct CloseOnDrop(super::viewer::Wake);
        impl Drop for CloseOnDrop {
            fn drop(&mut self) {
                self.0.close();
            }
        }
        let _close = CloseOnDrop(wake.clone());
        let start = Instant::now();
        // Planned here, off the GTK loop: it reads each input's header.
        let groups = match crate::output_plan::plan_outputs(&files, &naming) {
            Ok(groups) => groups,
            Err(message) => {
                let _ = tx.send(AsyncBatchEvent::Refused(message));
                return;
            }
        };
        let planned: Vec<PathBuf> = groups.iter().flatten().cloned().collect();
        let queue = Queue {
            total: planned.len(),
            groups,
            next: AtomicUsize::new(0),
            done: AtomicUsize::new(0),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            cancelled,
        };
        let n_workers =
            bigimage_core::resources::batch_worker_count(&planned).min(queue.groups.len());
        let tally = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..n_workers)
                .map(|_| {
                    let tx = tx.clone();
                    let (queue, work, wake) = (&queue, &work, &wake);
                    scope.spawn(move || run_worker(queue, work, &tx, wake))
                })
                .collect();
            let mut tally = Tally::default();
            for handle in handles {
                match handle.join() {
                    Ok(worker) => tally.add(worker),
                    Err(_) => tally.add(Tally {
                        fail: 1,
                        first_err: Some(gettext("a worker ended unexpectedly")),
                        ..Tally::default()
                    }),
                }
            }
            tally
        });
        let _ = tx.send(AsyncBatchEvent::Done(RunSummary {
            ok: tally.ok,
            skip: tally.skip,
            fail: tally.fail,
            first_err: tally.first_err,
            elapsed: start.elapsed(),
            bytes_in: queue.bytes_in.load(Ordering::Relaxed),
            bytes_out: queue.bytes_out.load(Ordering::Relaxed),
            done: queue.done.load(Ordering::Relaxed),
            total: queue.total,
            cancelled: queue.cancelled.load(Ordering::Relaxed),
            single: tally.last.filter(|_| queue.total == 1),
        }));
    });
    events
}

/// What the batch workers share: the planned groups, the next one to take,
/// and the running totals.
struct Queue {
    /// Inputs writing one destination, in input order.
    groups: Vec<Vec<PathBuf>>,
    /// Inputs in every group.
    total: usize,
    next: AtomicUsize,
    done: AtomicUsize,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    cancelled: Arc<AtomicBool>,
}

/// Outcome counts of one worker, or of the whole batch.
#[derive(Default)]
struct Tally {
    ok: usize,
    skip: usize,
    fail: usize,
    /// The first failure, shown in the summary.
    first_err: Option<String>,
    /// The outcome of the last input this tally finished.
    last: Option<ConvertOutcome>,
}

impl Tally {
    fn add(&mut self, other: Tally) {
        self.ok += other.ok;
        self.skip += other.skip;
        self.fail += other.fail;
        if self.first_err.is_none() {
            self.first_err = other.first_err;
        }
        self.last = other.last.or(self.last.take());
    }
}

/// Take groups from `queue` until it is empty or cancelled, running each
/// group's files in order and reporting each one to the GTK side.
fn run_worker<W>(
    queue: &Queue,
    work: &W,
    tx: &mpsc::Sender<AsyncBatchEvent>,
    wake: &super::viewer::Wake,
) -> Tally
where
    W: Fn(&Path) -> bigimage_core::Result<ConvertOutcome>,
{
    let mut tally = Tally::default();
    while !queue.cancelled.load(Ordering::Relaxed) {
        let idx = queue.next.fetch_add(1, Ordering::Relaxed);
        let Some(group) = queue.groups.get(idx) else { break };
        // A per-image name can still repeat inside a group.
        let mut written = Vec::new();
        for path in group {
            if queue.cancelled.load(Ordering::Relaxed) {
                break;
            }
            let display = path
                .file_name()
                .map(|name| crate::codec::printable_path(Path::new(name)))
                .unwrap_or_default();
            let input_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            // Scoped cancel lets closing the window stop an isolated codec run
            // instead of waiting up to 60 s.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::codec::with_cancel(queue.cancelled.clone(), || work(path))
            }))
            .unwrap_or_else(|_| {
                Err(bigimage_core::BigImageError::Other(gettext("unexpected worker failure")))
            });
            if let Ok(outcome) = &result {
                tally.last = Some(outcome.clone());
            }
            let failure = match result {
                Ok(ConvertOutcome::Written { output }) if written.contains(&output) => {
                    Some(crate::fill(
                        &gettext("replaced the output of another input: {path}"),
                        &[("path", &crate::codec::printable_path(&output))],
                    ))
                }
                Ok(ConvertOutcome::Written { output }) => {
                    tally.ok += 1;
                    queue.bytes_in.fetch_add(input_bytes, Ordering::Relaxed);
                    if let Ok(meta) = std::fs::metadata(&output) {
                        queue.bytes_out.fetch_add(meta.len(), Ordering::Relaxed);
                    }
                    written.push(output);
                    None
                }
                Ok(ConvertOutcome::Skipped { .. }) => {
                    tally.skip += 1;
                    None
                }
                Err(e) => Some(crate::error_text::core_error_text(&e)),
            };
            if let Some(failure) = failure {
                tally.fail += 1;
                let shown = crate::codec::printable_path(path);
                tally.first_err.get_or_insert_with(|| format!("{shown}: {failure}"));
            }
            let done = queue.done.fetch_add(1, Ordering::Relaxed) + 1;
            let progress = AsyncBatchEvent::Progress { done, total: queue.total, file: display };
            if tx.send(progress).is_err() {
                return tally;
            }
            wake.notify();
        }
    }
    tally
}

/// Run `work` on each file across the worker pool and show the progress.
/// Closing the window, its Cancel button included, cancels the run; the
/// application is held until every worker exits, so no `.part` file is
/// stranded.
pub(super) fn run_batch_async<W>(
    files: Vec<PathBuf>,
    naming: crate::output_plan::Naming,
    work: W,
    busy_label: String,
    status: gtk::Label,
    apply_btn: gtk::Button,
    window: adw::ApplicationWindow,
) where
    W: Fn(&Path) -> bigimage_core::Result<ConvertOutcome> + Send + Sync + 'static,
{
    let cancelled = Arc::new(AtomicBool::new(false));
    disarm_auto_close(&window);

    // Disconnected when this run ends: every Apply connects its own, and a
    // stale handler would keep the finished run's flag (and closure) alive.
    let mut close_handler = Some({
        let cancelled = cancelled.clone();
        window.connect_close_request(move |_| {
            cancelled.store(true, Ordering::Release);
            gtk4::glib::Propagation::Proceed
        })
    });

    let events = spawn_batch_workers(files, naming, work, cancelled);
    // Released with this task, i.e. only after the pool reported back.
    let hold = window.application().map(|app| app.hold());

    gtk4::glib::MainContext::default().spawn_local(async move {
        let _hold = hold;
        loop {
            match events.next().await {
                Some(AsyncBatchEvent::Progress { done, total, file }) => {
                    status.set_text(&format!("{busy_label} {done}/{total} — {file}"));
                }
                Some(AsyncBatchEvent::Refused(message)) => {
                    status.set_text(&message);
                    apply_btn.set_sensitive(true);
                    break;
                }
                Some(AsyncBatchEvent::Done(summary)) => {
                    // Closed mid-run: nothing left to show or to auto-close.
                    if !(summary.cancelled && !window.is_visible()) {
                        finish_dialog(&status, &apply_btn, &window, summary);
                    }
                    break;
                }
                None => {
                    abort_dialog(&status, &apply_btn);
                    break;
                }
            }
        }
        if let Some(handler) = close_handler.take() {
            window.disconnect(handler);
        }
    });
}

#[cfg(test)]
mod worker_tests {
    use super::*;
    use bigimage_core::{Format, OverwritePolicy};

    fn png_naming(policy: OverwritePolicy) -> crate::output_plan::Naming {
        crate::output_plan::Naming::image(Some(Format::Png), policy)
    }

    /// Awaits the events the way the dialogs do, through the wake-up.
    fn summary(events: BatchEvents) -> AsyncBatchEvent {
        gtk4::glib::MainContext::new().block_on(async {
            loop {
                let event = events.next().await.expect("workers ended without a summary");
                if matches!(event, AsyncBatchEvent::Done(_) | AsyncBatchEvent::Refused(_)) {
                    return event;
                }
            }
        })
    }

    #[test]
    fn cancellation_before_start_processes_no_files() {
        let cancelled = Arc::new(AtomicBool::new(true));
        let rx = spawn_batch_workers(
            vec![PathBuf::from("unread.png")],
            png_naming(OverwritePolicy::Skip),
            |_| panic!("work after cancellation"),
            cancelled,
        );
        assert!(matches!(
            summary(rx),
            AsyncBatchEvent::Done(RunSummary { done: 0, cancelled: true, .. })
        ));
    }

    #[test]
    fn closing_mid_run_reaches_the_isolated_codec() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let trigger = cancelled.clone();
        let rx = spawn_batch_workers(
            vec![PathBuf::from("first.png")],
            png_naming(OverwritePolicy::Skip),
            move |path| {
                assert!(!crate::codec::cancelled());
                trigger.store(true, Ordering::Release);
                // The codec observes the same flag mid-file, not only between files.
                assert!(crate::codec::cancelled());
                Ok(ConvertOutcome::Skipped { output: path.to_path_buf() })
            },
            cancelled,
        );
        assert!(matches!(
            summary(rx),
            AsyncBatchEvent::Done(RunSummary { skip: 1, cancelled: true, .. })
        ));
    }

    /// A run that skipped its only file says which output already exists and
    /// stays open; one that wrote it names the output and closes itself.
    #[test]
    #[ignore = "requires a graphical display; run alone with GSETTINGS_BACKEND=memory"]
    fn summary_names_output_and_auto_closes() {
        let app = super::super::dialogs::probe_app("com.biglinux.Iris.SummaryProbe");
        let window = adw::ApplicationWindow::builder().application(&app).build();
        window.present();
        let (status, apply) = (gtk::Label::new(None), gtk::Button::new());
        let summary = |ok, skip, single| RunSummary {
            ok,
            skip,
            fail: 0,
            first_err: None,
            elapsed: Duration::from_millis(10),
            bytes_in: 0,
            bytes_out: 0,
            done: 1,
            total: 1,
            cancelled: false,
            single: Some(single),
        };
        let output = PathBuf::from("/photos/a.png");
        finish_dialog(
            &status,
            &apply,
            &window,
            summary(0, 1, ConvertOutcome::Skipped { output: output.clone() }),
        );
        assert!(status.text().contains("/photos/a.png"), "{}", status.text());
        assert!(!auto_close_armed(&window), "a skipped run closed itself");
        finish_dialog(&status, &apply, &window, summary(1, 0, ConvertOutcome::Written { output }));
        assert!(status.text().contains("/photos/a.png"), "{}", status.text());
        assert!(auto_close_armed(&window));
        disarm_auto_close(&window);
        window.destroy();
    }

    #[test]
    fn counts_fill_every_placeholder() {
        assert_eq!(format_counts(1, 0, 2), "written: 1, skipped: 0, failed: 2");
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
            png_naming(OverwritePolicy::Skip),
            |path| {
                if path.file_name().unwrap() == "3.png" {
                    panic!("synthetic failure");
                }
                Ok(ConvertOutcome::Skipped { output: path.to_path_buf() })
            },
            Arc::new(AtomicBool::new(false)),
        );
        assert!(matches!(
            summary(rx),
            AsyncBatchEvent::Done(RunSummary { fail: 1, skip: 9, done: 10, .. })
        ));
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
            png_naming(OverwritePolicy::Skip),
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
            AsyncBatchEvent::Done(RunSummary { ok: 1, skip: 1, bytes_in: 10, bytes_out: 10, .. })
        ));
    }

    /// The GUI plans like the CLI: `a.png` and `a.bmp` both write `a.webp`.
    #[test]
    fn colliding_inputs_are_planned() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("a.png");
        let second = dir.path().join("a.bmp");
        image::RgbImage::new(32, 24).save(&first).unwrap();
        image::RgbImage::new(16, 12).save(&second).unwrap();
        let files = vec![first, second];
        let webp = |policy| crate::output_plan::Naming::image(Some(Format::WebP), policy);
        let convert = |policy| {
            move |path: &Path| {
                let opts = bigimage_core::EncodeOptions::default();
                bigimage_core::convert_file_to(path, None, Format::WebP, &opts, policy)
            }
        };
        for policy in [OverwritePolicy::Skip, OverwritePolicy::Replace] {
            let cancelled = Arc::new(AtomicBool::new(false));
            let events =
                spawn_batch_workers(files.clone(), webp(policy), convert(policy), cancelled);
            let refusal = summary(events);
            let AsyncBatchEvent::Refused(message) = refusal else { panic!("ran under {policy:?}") };
            assert!(message.contains("a.png") && message.contains("a.bmp"), "{message}");
        }
        assert!(!dir.path().join("a.webp").exists());
        // Increment runs them in input order on one worker, so the numbers
        // never depend on which worker gets there first.
        let policy = OverwritePolicy::Increment;
        let events = spawn_batch_workers(
            files,
            webp(policy),
            convert(policy),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(matches!(
            summary(events),
            AsyncBatchEvent::Done(RunSummary { ok: 2, fail: 0, .. })
        ));
        let size = |name: &str| image::image_dimensions(dir.path().join(name)).unwrap();
        assert_eq!(size("a.webp"), (32, 24));
        assert_eq!(size("a_1.webp"), (16, 12));
    }
}
