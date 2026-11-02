// SPDX-License-Identifier: GPL-3.0-or-later
//! Confine untrusted codecs to a short-lived, resource-limited process.
//! The parent accepts only a validated, bounded raw-pixel protocol.
use bigimage_core::{backend, BigImageError, Format, Metadata, PreviewSession};
use image::{ColorType, DynamicImage, GenericImageView, ImageBuffer, ImageError, ImageResult};
use std::cell::RefCell;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

const MAGIC: &[u8; 8] = b"BIGIRIS2";
const HEADER: usize = 36;
const MAX_BYTES: u64 = 512 * 1024 * 1024;
const MAX_INPUT: u64 = 1024 * 1024 * 1024;
const MAX_METADATA: u64 = 1024 * 1024;
/// EXIF kept for an in-place re-save (a JPEG holds at most 64 KiB anyway).
const MAX_RESAVE_EXIF: u64 = 1024 * 1024;
/// ICC profiles kept for an in-place re-save; matches the core's cap.
const MAX_RESAVE_ICC: u64 = 4 * 1024 * 1024;
/// Re-save trailer after the pixels: EXIF and ICC lengths (u64 LE each).
const RESAVE_TRAILER: usize = 16;
thread_local! { static CANCEL: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) }; }

pub fn configure() {
    assert!(backend::configure(backend::Backend {
        decode,
        dimensions,
        preview,
        metadata,
        ensure_static
    }));
    assert!(backend::configure_resave(resave));
}

/// Exercise the real process boundary and raw protocol.
pub fn self_test() -> color_eyre::Result<()> {
    let input = tempfile::Builder::new().suffix(".png").tempfile()?;
    image::RgbImage::from_pixel(2, 3, image::Rgb([30, 70, 120])).save(input.path())?;
    let image = decode(input.path())?.0;
    if image.dimensions() != (2, 3) {
        return Err(color_eyre::eyre::eyre!("invalid sandbox decode"));
    }
    let attested = run(input.path(), "attest", |output| Ok(output.metadata()?.len() == 0))?;
    if !attested {
        return Err(color_eyre::eyre::eyre!("invalid sandbox attestation"));
    }
    Ok(())
}

/// Scoped cancellation also restores the context during panic unwinding.
#[cfg(any(feature = "gui", test))]
pub fn with_cancel<T>(cancel: Arc<AtomicBool>, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<Arc<AtomicBool>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CANCEL.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(CANCEL.with(|slot| slot.replace(Some(cancel))));
    work()
}
fn invalid(message: impl Into<String>) -> ImageError {
    ImageError::IoError(std::io::Error::new(std::io::ErrorKind::InvalidData, message.into()))
}
pub(crate) fn cancelled() -> bool {
    CANCEL.with(|slot| slot.borrow().as_ref().is_some_and(|flag| flag.load(Ordering::Acquire)))
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Exit status a worker uses when the parent expects another build. Old
/// processes keep running after a package upgrade replaces the executable.
const VERSION_MISMATCH: i32 = 86;
/// Bytes of worker diagnostics kept; the rest is drained and discarded.
const MAX_DIAGNOSTIC: usize = 4096;
/// Hard wall-clock budget for one isolated codec run.
const DEADLINE: Duration = Duration::from_secs(60);

#[cfg(test)]
thread_local! {
    /// Test-only replacement for the worker binary (never compiled in releases).
    static TEST_EXECUTABLE: RefCell<Option<(PathBuf, Duration, bool)>> = const { RefCell::new(None) };
}

/// Path that starts a fresh BigIris process. After an upgrade `/proc/self/exe`
/// reads `/usr/bin/bigiris (deleted)`; the installed replacement is used then.
/// The bool reports that the path may hold a different build.
pub(crate) fn installed_executable() -> std::io::Result<(PathBuf, bool)> {
    use std::os::unix::ffi::OsStrExt;
    let exe = std::env::current_exe()?;
    if exe.is_file() {
        return Ok((exe, false));
    }
    let replaced = exe
        .as_os_str()
        .as_bytes()
        .strip_suffix(b" (deleted)")
        .map(|bytes| PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
        .filter(|path| path.is_file());
    replaced.map(|path| (path, true)).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "o executável do BigIris foi removido; reinicie o BigIris",
        )
    })
}

#[cfg(test)]
/// Test-only, process-wide worker binary (see [`configure_for_tests`]);
/// unlike `TEST_EXECUTABLE` it reaches batch worker threads too.
static TEST_WORKER: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The real `bigiris` binary next to the test harness: `target/<profile>/
/// bigiris`, built by `cargo test --all-targets` for the CLI tests.
#[cfg(test)]
pub(crate) fn built_worker() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.parent()?.join("bigiris")).filter(|path| path.is_file())
}

/// Test-only: route every decode in this process through the real sandbox,
/// like `main` does, with `worker` (default: [`built_worker`]) as the codec
/// binary on every thread. Idempotent; the first worker path wins. Panics
/// if no worker binary exists (build it with `cargo test --all-targets`).
#[cfg(test)]
pub(crate) fn configure_for_tests(worker: Option<PathBuf>) -> PathBuf {
    static CONFIGURE: std::sync::Once = std::sync::Once::new();
    let path = TEST_WORKER
        .get_or_init(|| {
            worker.or_else(built_worker).expect("bigiris binary missing: cargo test --all-targets")
        })
        .clone();
    CONFIGURE.call_once(configure);
    path
}

fn worker_executable() -> std::io::Result<(PathBuf, bool, Duration)> {
    #[cfg(test)]
    if let Some((path, deadline, replaced)) = TEST_EXECUTABLE.with(|slot| slot.borrow().clone()) {
        return Ok((path, replaced, deadline));
    }
    #[cfg(test)]
    if let Some(path) = TEST_WORKER.get() {
        return Ok((path.clone(), false, DEADLINE));
    }
    installed_executable().map(|(path, replaced)| (path, replaced, DEADLINE))
}

/// Keep the parent's descriptors out of `command`'s process tree.
///
/// Rust opens its own files close-on-exec, but libraries do not always:
/// after GTK/Mesa start, the GUI holds descriptors such as `/dev/udmabuf` or
/// `memfd:lp_dma_buf` without `FD_CLOEXEC`, and a shell may hand the CLI a
/// stray `3<file`. bwrap does not close inherited descriptors, so they would
/// reach the sandboxed codec (which refuses to run with any) or a launched
/// BigIris window. Every descriptor above stdio is marked close-on-exec in
/// the child just before `exec`, so the parent's own state is untouched.
#[allow(unsafe_code)]
pub(crate) fn close_inherited_descriptors(command: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    // SAFETY: the hook runs in the forked child before `exec`. It only issues
    // raw, async-signal-safe syscalls (close_range, getrlimit, fcntl): no
    // allocation, no locks and no access to memory shared with other threads.
    // Marking descriptors close-on-exec (rather than closing them) keeps the
    // pipe std uses to report `exec` failures working until `exec` itself.
    unsafe {
        command.pre_exec(|| {
            mark_descriptors_cloexec();
            Ok(())
        })
    }
}

/// Body of the `pre_exec` hook; see [`close_inherited_descriptors`].
#[allow(unsafe_code)]
fn mark_descriptors_cloexec() {
    // SAFETY: plain syscalls on integer descriptors; failures are ignored
    // because the worker re-checks its descriptor table and fails closed.
    unsafe {
        // Linux >= 5.11: one syscall for the whole range.
        if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, libc::CLOSE_RANGE_CLOEXEC) == 0 {
            return;
        }
        // Older kernels: walk the descriptor table up to the soft limit.
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        let top = if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 {
            limit.rlim_cur.min(1 << 16) as libc::c_int
        } else {
            1024
        };
        for fd in 3..top {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

/// Prevent the codec from creating further user namespaces when this bwrap
/// supports it (>= 0.8). Probed once, so distro policies that reject it (for
/// example AppArmor rules or containers without user namespaces) keep the
/// established sandbox instead of failing every decode.
fn userns_hardening() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let Ok(child) = close_inherited_descriptors(&mut Command::new("/usr/bin/bwrap"))
            .env_clear()
            .args([
                "--unshare-all",
                "--unshare-user",
                "--disable-userns",
                "--assert-userns-disabled",
                "--die-with-parent",
                "--ro-bind",
                "/usr",
                "/usr",
                "--symlink",
                "usr/lib",
                "/lib",
                "--symlink",
                "usr/lib64",
                "/lib64",
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "/usr/bin/true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            tracing::warn!(
                "bwrap indisponível para testar --disable-userns; sandbox sem esse reforço"
            );
            return false;
        };
        let mut child = ChildGuard(child);
        let supported = matches!(
            wait_child(&mut child.0, Instant::now() + Duration::from_secs(10), false),
            Ok(status) if status.success()
        );
        if !supported {
            // Logged once: the answer is cached for the whole process.
            tracing::warn!(
                "bwrap recusou --disable-userns (bwrap < 0.8 ou política do sistema); \
                 o codec isolado roda sem esse reforço"
            );
        }
        supported
    })
}

/// Wait without busy polling: a pidfd wakes us on exit, while short poll
/// slices keep deadline and cancellation checks responsive.
fn wait_child(
    child: &mut Child,
    deadline: Instant,
    cancellable: bool,
) -> ImageResult<std::process::ExitStatus> {
    use rustix::event::{poll, PollFd, PollFlags, Timespec};
    let pidfd = rustix::process::pidfd_open(
        rustix::process::Pid::from_child(child),
        rustix::process::PidfdFlags::empty(),
    )
    .ok();
    loop {
        if cancellable && cancelled() {
            return Err(invalid("operação cancelada"));
        }
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(invalid("codec excedeu o limite de tempo"));
        }
        let slice = (deadline - now).min(Duration::from_millis(50));
        match &pidfd {
            Some(fd) => {
                let timeout = Timespec { tv_sec: 0, tv_nsec: i64::from(slice.subsec_nanos()) };
                let mut fds = [PollFd::new(fd, PollFlags::IN)];
                // EINTR and spurious wakeups are handled by the loop.
                let _ = poll(&mut fds, Some(&timeout));
            }
            // Kernels before 5.3 lack pidfd; fall back to a gentle poll.
            None => std::thread::sleep(slice.min(Duration::from_millis(10))),
        }
    }
}

/// Keep diagnostics bounded in memory; never let them reach a host file.
fn drain_diagnostics(stderr: std::process::ChildStderr) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut stderr = stderr;
        let mut kept = Vec::with_capacity(MAX_DIAGNOSTIC);
        let mut buffer = [0u8; 4096];
        loop {
            match stderr.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let room = MAX_DIAGNOSTIC - kept.len();
                    kept.extend_from_slice(&buffer[..n.min(room)]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = tx.send(kept);
    });
    rx
}

/// Worker output is untrusted: drop terminal control sequences and bidi
/// overrides so it cannot rewrite the user's terminal or the GUI text.
pub(crate) fn sanitize_text(text: &str) -> String {
    text.chars()
        .map(|c| {
            let bidi = matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}');
            if (c.is_control() && c != '\n') || bidi {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

/// Appended to worker failures when the executable was replaced on disk.
const RESTART_HINT: &str = " (o BigIris foi atualizado; reinicie-o)";

/// Identity a worker must match: the protocol magic, the crate version and
/// the build id from `build.rs`. The version alone never changes between
/// packaged builds, so it cannot detect an upgrade.
const WORKER_ID: &str =
    concat!("BIGIRIS2+", env!("CARGO_PKG_VERSION"), "+", env!("BIGIRIS_BUILD_ID"));

/// Run one isolated codec job and parse its output. When the installed
/// executable was replaced while we were running, a different build may have
/// served the request, so any failure (spawn, exit status or protocol) says
/// that restarting BigIris fixes it.
fn run<T>(
    path: &Path,
    mode: &str,
    parse: impl FnOnce(std::fs::File) -> ImageResult<T>,
) -> ImageResult<T> {
    let mut replaced = false;
    execute(path, mode, &mut replaced).and_then(parse).map_err(|error| {
        if replaced && !cancelled() {
            invalid(format!("{error}{RESTART_HINT}"))
        } else {
            error
        }
    })
}

/// `replaced` reports whether failures still need the restart hint.
fn execute(path: &Path, mode: &str, replaced: &mut bool) -> ImageResult<std::fs::File> {
    if cancelled() {
        return Err(invalid("operação cancelada"));
    }
    let input = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(path)?;
    let info = input.metadata()?;
    if !info.is_file() || info.len() > MAX_INPUT {
        return Err(invalid("entrada não regular ou acima de 1 GiB"));
    }
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|e| Format::parse(e).ok())
        .map_or("img", |format| format.extension());
    let sandbox_input = format!("/input.{extension}");
    let (executable, is_replaced, budget) = worker_executable()?;
    *replaced = is_replaced;
    // Only pixel modes need large outputs; everything else is tiny.
    let output_limit = match mode {
        "pixels" | "preview" => MAX_BYTES + HEADER as u64,
        "resave" => MAX_BYTES + (HEADER + RESAVE_TRAILER) as u64 + MAX_RESAVE_EXIF + MAX_RESAVE_ICC,
        _ => MAX_METADATA + 4096,
    };
    let output = tempfile::tempfile()?;
    let mut command = Command::new("/usr/bin/prlimit");
    close_inherited_descriptors(&mut command)
        .env_clear()
        .args(["--as=2147483648", "--cpu=30", "--nofile=64", "--core=0"])
        .arg(format!("--fsize={output_limit}"))
        .args(["--", "/usr/bin/bwrap", "--unshare-all"]);
    if userns_hardening() {
        command.args(["--unshare-user", "--disable-userns"]);
    }
    command
        .args([
            "--die-with-parent",
            "--new-session",
            "--clearenv",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/usr",
            "/usr",
            "--symlink",
            "usr/lib",
            "/lib",
            "--symlink",
            "usr/lib64",
            "/lib64",
            "--symlink",
            "usr/bin",
            "/bin",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--remount-ro",
            "/dev",
            "--size",
            "67108864",
            "--tmpfs",
            "/tmp",
            "--dir",
            "/app",
            "--ro-bind",
        ])
        .arg(executable)
        .arg("/app/bigiris")
        .args(["--ro-bind", "/proc/self/fd/0"])
        .arg(&sandbox_input)
        .args([
            "--chdir",
            "/",
            "--setenv",
            "LC_ALL",
            "C.UTF-8",
            "--setenv",
            "RAYON_NUM_THREADS",
            "2",
            "/app/bigiris",
            "--codec-worker",
            WORKER_ID,
        ])
        .arg(mode)
        .arg(&sandbox_input)
        .stdin(Stdio::from(input))
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::piped());
    let child = command.spawn().map_err(|error| {
        invalid(format!("sandbox indisponível: instale bubblewrap e util-linux; {error}"))
    })?;
    let mut child = ChildGuard(child);
    let diagnostics = child.0.stderr.take().map(drain_diagnostics);
    let status = wait_child(&mut child.0, Instant::now() + budget, true)?;
    if !status.success() {
        // Both messages below already carry the restart advice.
        let hint = if std::mem::take(replaced) { RESTART_HINT } else { "" };
        if status.code() == Some(VERSION_MISMATCH) {
            return Err(invalid("o BigIris foi atualizado enquanto estava aberto; reinicie-o"));
        }
        let details = diagnostics
            .and_then(|rx| rx.recv_timeout(Duration::from_secs(2)).ok())
            .unwrap_or_default();
        let details = sanitize_text(String::from_utf8_lossy(&details).trim());
        return Err(invalid(format!("codec isolado recusou a imagem ({status}){hint}: {details}")));
    }
    let mut output = output;
    output.seek(SeekFrom::Start(0))?;
    Ok(output)
}

struct Pixels {
    image: DynamicImage,
    format: Option<Format>,
    natural: (u32, u32),
    gps: bool,
}
fn receive(output: std::fs::File) -> ImageResult<Pixels> {
    receive_with(output, false).map(|(pixels, _)| pixels)
}
/// Read the raw pixel protocol. With `trailer`, the EXIF and ICC blobs of a
/// re-save decode follow the pixels and are bounded like everything else.
fn receive_with(
    mut output: std::fs::File,
    trailer: bool,
) -> ImageResult<(Pixels, bigimage_core::SourceMetadata)> {
    let mut header = [0u8; HEADER];
    output.read_exact(&mut header)?;
    if &header[..8] != MAGIC || header[10] > 1 || header[11] != 0 {
        return Err(invalid("protocolo de codec inválido"));
    }
    let word = |offset| u32::from_le_bytes(header[offset..offset + 4].try_into().unwrap());
    let (w, h) = (word(12), word(16));
    let natural = (word(20), word(24));
    let length = u64::from_le_bytes(header[28..36].try_into().unwrap());
    let color = match header[8] {
        1 => ColorType::L8,
        2 => ColorType::La8,
        3 => ColorType::Rgb8,
        4 => ColorType::Rgba8,
        5 => ColorType::L16,
        6 => ColorType::La16,
        7 => ColorType::Rgb16,
        8 => ColorType::Rgba16,
        9 => ColorType::Rgb32F,
        10 => ColorType::Rgba32F,
        _ => return Err(invalid("tipo de pixels inválido")),
    };
    if w == 0
        || h == 0
        || natural.0 == 0
        || natural.1 == 0
        || u64::from(natural.0) * u64::from(natural.1) > 256 * 1024 * 1024
        || u64::from(w) * u64::from(h) > 256 * 1024 * 1024
        || length > MAX_BYTES
        || length != u64::from(w) * u64::from(h) * u64::from(color.bytes_per_pixel())
    {
        return Err(invalid("dimensões ou tamanho do protocolo inválidos"));
    }
    let total = output.metadata()?.len();
    let pixels_end = HEADER as u64 + length;
    let (exif_length, icc_length) = if trailer {
        if total < pixels_end + RESAVE_TRAILER as u64 {
            return Err(invalid("dimensões ou tamanho do protocolo inválidos"));
        }
        let mut lengths = [0u8; RESAVE_TRAILER];
        output.seek(SeekFrom::Start(pixels_end))?;
        output.read_exact(&mut lengths)?;
        output.seek(SeekFrom::Start(HEADER as u64))?;
        let exif = u64::from_le_bytes(lengths[..8].try_into().unwrap());
        let icc = u64::from_le_bytes(lengths[8..].try_into().unwrap());
        if exif > MAX_RESAVE_EXIF || icc > MAX_RESAVE_ICC {
            return Err(invalid("metadados excessivos"));
        }
        (exif, icc)
    } else {
        (0, 0)
    };
    let expected =
        pixels_end + if trailer { RESAVE_TRAILER as u64 + exif_length + icc_length } else { 0 };
    if total != expected {
        return Err(invalid("dimensões ou tamanho do protocolo inválidos"));
    }
    let format = if header[9] == 255 {
        None
    } else {
        Some(*Format::all().get(header[9] as usize).ok_or_else(|| invalid("formato inválido"))?)
    };
    let length = length as usize;
    // Decode wide samples straight into their typed buffer through a small
    // scratch chunk, so peak memory stays at one copy of the pixels.
    macro_rules! buffer {
        ($data:expr, $variant:ident) => {
            DynamicImage::$variant(
                ImageBuffer::from_raw(w, h, $data).ok_or_else(|| invalid("buffer inválido"))?,
            )
        };
    }
    macro_rules! bytes {
        () => {{
            let mut raw = vec![0; length];
            output.read_exact(&mut raw)?;
            raw
        }};
    }
    macro_rules! shorts {
        () => {
            read_samples::<2, u16>(&mut output, length, u16::from_ne_bytes)?
        };
    }
    macro_rules! floats {
        () => {
            read_samples::<4, f32>(&mut output, length, f32::from_ne_bytes)?
        };
    }
    let image = match color {
        ColorType::L8 => buffer!(bytes!(), ImageLuma8),
        ColorType::La8 => buffer!(bytes!(), ImageLumaA8),
        ColorType::Rgb8 => buffer!(bytes!(), ImageRgb8),
        ColorType::Rgba8 => buffer!(bytes!(), ImageRgba8),
        ColorType::L16 => buffer!(shorts!(), ImageLuma16),
        ColorType::La16 => buffer!(shorts!(), ImageLumaA16),
        ColorType::Rgb16 => buffer!(shorts!(), ImageRgb16),
        ColorType::Rgba16 => buffer!(shorts!(), ImageRgba16),
        ColorType::Rgb32F => buffer!(floats!(), ImageRgb32F),
        ColorType::Rgba32F => buffer!(floats!(), ImageRgba32F),
        _ => return Err(invalid("tipo de pixels inválido")),
    };
    let mut source = bigimage_core::SourceMetadata::default();
    if trailer {
        // Skip the lengths already validated above.
        output.seek(SeekFrom::Current(RESAVE_TRAILER as i64))?;
        let mut blob = |length: u64| -> std::io::Result<Option<Vec<u8>>> {
            if length == 0 {
                return Ok(None);
            }
            let mut data = vec![0; length as usize];
            output.read_exact(&mut data)?;
            Ok(Some(data))
        };
        source.exif = blob(exif_length)?;
        source.icc = blob(icc_length)?;
    }
    Ok((Pixels { image, format, natural, gps: header[10] == 1 }, source))
}
/// Read `length` bytes of native-endian `N`-byte samples without first
/// materializing the whole byte buffer.
fn read_samples<const N: usize, T>(
    reader: &mut impl Read,
    length: usize,
    convert: fn([u8; N]) -> T,
) -> std::io::Result<Vec<T>> {
    let mut samples = Vec::with_capacity(length / N);
    let mut chunk = vec![0u8; 64 * 1024];
    let mut remaining = length;
    while remaining > 0 {
        let take = remaining.min(chunk.len());
        reader.read_exact(&mut chunk[..take])?;
        samples.extend(chunk[..take].as_chunks::<N>().0.iter().map(|bytes| convert(*bytes)));
        remaining -= take;
    }
    Ok(samples)
}
fn decode(path: &Path) -> ImageResult<backend::Decoded> {
    let pixels = run(path, "pixels", receive)?;
    Ok((pixels.image, pixels.format))
}
/// Pixels in the source colour space plus its EXIF/ICC, for in-place saves.
fn resave(path: &Path) -> ImageResult<backend::ResaveDecoded> {
    let (pixels, source) = run(path, "resave", |output| receive_with(output, true))?;
    Ok((pixels.image, pixels.format, source))
}
fn dimensions(path: &Path) -> ImageResult<(u32, u32)> {
    run(path, "dimensions", read_dimensions)
}
fn read_dimensions(mut output: std::fs::File) -> ImageResult<(u32, u32)> {
    if output.metadata()?.len() != 8 {
        return Err(invalid("dimensões inválidas"));
    }
    let mut bytes = [0; 8];
    output.read_exact(&mut bytes)?;
    let size = (
        u32::from_le_bytes(bytes[..4].try_into().unwrap()),
        u32::from_le_bytes(bytes[4..].try_into().unwrap()),
    );
    if size.0 == 0 || size.1 == 0 || u64::from(size.0) * u64::from(size.1) > 256 * 1024 * 1024 {
        return Err(invalid("dimensões inválidas"));
    }
    Ok(size)
}
fn preview(path: &Path) -> bigimage_core::Result<PreviewSession> {
    let pixels = run(path, "preview", receive)
        .map_err(|source| BigImageError::Decode { path: path.into(), source })?;
    PreviewSession::from_cached(
        path.into(),
        pixels.natural,
        pixels
            .format
            .ok_or_else(|| BigImageError::UnsupportedFormat("preview sem formato".into()))?,
        pixels.image,
        pixels.gps,
    )
}
fn ensure_static(path: &Path) -> bigimage_core::Result<()> {
    run(path, "static", |_| Ok(()))
        .map_err(|source| BigImageError::Decode { path: path.into(), source })
}
fn metadata(path: &Path) -> bigimage_core::Result<Metadata> {
    run(path, "metadata", read_json)
        .map_err(|source| BigImageError::Decode { path: path.into(), source })
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Information {
    pub dimensions: Option<(u32, u32)>,
    pub color: Option<u8>,
    pub format: Option<String>,
    pub metadata: Option<Metadata>,
}
fn information_local(path: &Path) -> Information {
    let dimensions = backend::dimensions_in_process(path).ok();
    let reader = image::ImageReader::open(path).ok().and_then(|r| r.with_guessed_format().ok());
    let format = reader.as_ref().and_then(|r| r.format()).map(|f| format!("{f:?}"));
    let color =
        reader.and_then(|r| r.into_decoder().ok()).map(
            |dec| match image::ImageDecoder::color_type(&dec) {
                ColorType::L8 => 1,
                ColorType::La8 => 2,
                ColorType::Rgb8 => 3,
                ColorType::Rgba8 => 4,
                ColorType::L16 => 5,
                ColorType::La16 => 6,
                ColorType::Rgb16 => 7,
                ColorType::Rgba16 => 8,
                ColorType::Rgb32F => 9,
                ColorType::Rgba32F => 10,
                _ => 0,
            },
        );
    let metadata = bigimage_core::metadata::read(path).ok().map(trim_metadata);
    Information { dimensions, color, format, metadata }
}
fn trim_metadata(mut meta: Metadata) -> Metadata {
    meta.tags.truncate(128);
    for (_, value) in &mut meta.tags {
        *value = value.chars().take(4096).collect();
    }
    meta
}
#[cfg(feature = "gui")]
pub fn information(path: &Path) -> ImageResult<Information> {
    // Unit tests do not execute main/configure; still run their I/O off GTK.
    #[cfg(test)]
    return Ok(information_local(path));
    #[cfg(not(test))]
    {
        run(path, "information", read_json)
    }
}
/// Bounded JSON answer of the `metadata` and `information` modes.
fn read_json<T: serde::de::DeserializeOwned>(output: std::fs::File) -> ImageResult<T> {
    if output.metadata()?.len() > MAX_METADATA {
        return Err(invalid("metadados excessivos"));
    }
    serde_json::from_reader(std::io::BufReader::new(output)).map_err(|e| invalid(e.to_string()))
}

fn send(
    image: &DynamicImage,
    format: Option<Format>,
    natural: (u32, u32),
    gps: bool,
) -> ImageResult<()> {
    let mut output = std::io::stdout().lock();
    send_to(&mut output, image, format, natural, gps)?;
    output.flush()?;
    Ok(())
}
fn send_to(
    output: &mut impl Write,
    image: &DynamicImage,
    format: Option<Format>,
    natural: (u32, u32),
    gps: bool,
) -> ImageResult<()> {
    let color = match image.color() {
        ColorType::L8 => 1,
        ColorType::La8 => 2,
        ColorType::Rgb8 => 3,
        ColorType::Rgba8 => 4,
        ColorType::L16 => 5,
        ColorType::La16 => 6,
        ColorType::Rgb16 => 7,
        ColorType::Rgba16 => 8,
        ColorType::Rgb32F => 9,
        ColorType::Rgba32F => 10,
        _ => return Err(invalid("tipo de pixels inválido")),
    };
    let mut header = Vec::with_capacity(HEADER);
    header.extend(MAGIC);
    header.extend([
        color,
        format.map_or(255, |f| Format::all().iter().position(|x| *x == f).unwrap() as u8),
        u8::from(gps),
        0,
    ]);
    for value in [image.width(), image.height(), natural.0, natural.1] {
        header.extend(value.to_le_bytes());
    }
    header.extend((image.as_bytes().len() as u64).to_le_bytes());
    output.write_all(&header)?;
    output.write_all(image.as_bytes())?;
    Ok(())
}
/// Pixels, then the trailer lengths, then the EXIF and ICC blobs.
fn send_resave(
    output: &mut impl Write,
    image: &DynamicImage,
    format: Option<Format>,
    source: &bigimage_core::SourceMetadata,
) -> ImageResult<()> {
    send_to(output, image, format, image.dimensions(), false)?;
    let exif = source.exif.as_deref().unwrap_or_default();
    let icc = source.icc.as_deref().unwrap_or_default();
    output.write_all(&(exif.len() as u64).to_le_bytes())?;
    output.write_all(&(icc.len() as u64).to_le_bytes())?;
    output.write_all(exif)?;
    output.write_all(icc)?;
    Ok(())
}
/// Keep re-save metadata inside the protocol caps. EXIF over the cap is
/// dropped (no JPEG could carry it); an oversized ICC profile is applied to
/// the pixels instead, so colours stay right without it.
fn bound_resave_metadata(
    image: &mut DynamicImage,
    source: &mut bigimage_core::SourceMetadata,
) -> ImageResult<()> {
    if source.exif.as_ref().is_some_and(|exif| exif.len() as u64 > MAX_RESAVE_EXIF) {
        source.exif = None;
    }
    if let Some(icc) = source.icc.take_if(|icc| icc.len() as u64 > MAX_RESAVE_ICC) {
        bigimage_core::color::normalize_srgb(image, &icc)?;
    }
    Ok(())
}
/// Descriptors above stdio that leaked into the sandbox. bwrap does not close
/// inherited descriptors; the parent marks them close-on-exec before
/// spawning ([`close_inherited_descriptors`]), and this is the fail-closed
/// backstop in case that ever regresses.
fn inherited_descriptors() -> std::io::Result<Vec<u32>> {
    let numbers: Vec<u32> = std::fs::read_dir("/proc/self/fd")?
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    // The directory handle used for listing is closed by now; skip it.
    Ok(numbers
        .into_iter()
        .filter(|fd| {
            *fd > 2 && Path::new(&format!("/proc/self/fd/{fd}")).symlink_metadata().is_ok()
        })
        .collect())
}

pub fn worker() -> color_eyre::Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 5 {
        return Err(color_eyre::eyre::eyre!("invalid worker arguments"));
    }
    if args[2] != WORKER_ID {
        eprintln!("bigiris worker version mismatch");
        std::process::exit(VERSION_MISMATCH);
    }
    // Fail closed before touching untrusted input.
    let leaked = inherited_descriptors()?;
    if !leaked.is_empty() {
        return Err(color_eyre::eyre::eyre!("inherited file descriptors: {leaked:?}"));
    }
    let mode = args[3].to_str().unwrap_or("");
    let path = PathBuf::from(&args[4]);
    match mode {
        "pixels" => {
            let (image, format) = backend::decode_in_process(&path)?;
            send(&image, format, image.dimensions(), false)?;
        }
        "resave" => {
            let (mut image, format, mut source) = backend::decode_for_resave_in_process(&path)?;
            bound_resave_metadata(&mut image, &mut source)?;
            let mut output = std::io::stdout().lock();
            send_resave(&mut output, &image, format, &source)?;
            output.flush()?;
        }
        "preview" => {
            let session = PreviewSession::open(&path)?;
            send(
                session.thumbnail(),
                Some(session.source_format()),
                session.natural_size(),
                session.source_has_gps(),
            )?;
        }
        "dimensions" => {
            let (w, h) = backend::dimensions_in_process(&path)?;
            let mut out = std::io::stdout().lock();
            out.write_all(&w.to_le_bytes())?;
            out.write_all(&h.to_le_bytes())?;
        }
        "metadata" => {
            let meta = trim_metadata(bigimage_core::metadata::read(&path)?);
            serde_json::to_writer(std::io::stdout().lock(), &meta)?;
        }
        "attest" => {
            for location in ["/home", "/root", "/run", "/etc"] {
                if std::path::Path::new(location).exists() {
                    return Err(color_eyre::eyre::eyre!("host directory visible: {location}"));
                }
            }
            let network = std::fs::read_to_string("/proc/net/dev")?;
            if network.lines().skip(2).any(|line| !line.trim_start().starts_with("lo:")) {
                return Err(color_eyre::eyre::eyre!("host network visible"));
            }
        }
        "static" => {
            bigimage_core::content::ensure_static_local(&path)?;
        }
        "information" => {
            serde_json::to_writer(std::io::stdout().lock(), &information_local(&path))?;
        }
        _ => return Err(color_eyre::eyre::eyre!("invalid worker mode")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_context_restores_after_panic() {
        let flag = Arc::new(AtomicBool::new(true));
        assert!(!cancelled());
        let _ = std::panic::catch_unwind(|| {
            with_cancel(flag, || {
                assert!(cancelled());
                panic!("test")
            })
        });
        assert!(!cancelled());
    }
    #[test]
    fn invalid_protocol_cannot_allocate_claimed_pixels() {
        let mut file = tempfile::tempfile().unwrap();
        let mut header = [0u8; HEADER];
        header[..8].copy_from_slice(MAGIC);
        header[8] = 4;
        header[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        header[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        file.write_all(&header).unwrap();
        file.rewind().unwrap();
        assert!(receive(file).is_err());
    }

    #[test]
    fn wide_samples_are_read_across_chunk_boundaries() {
        let values: Vec<u16> = (0..70_000u32).map(|v| v as u16).collect();
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_ne_bytes()).collect();
        let read = read_samples::<2, u16>(&mut bytes.as_slice(), bytes.len(), u16::from_ne_bytes);
        assert_eq!(read.unwrap(), values);
    }

    #[test]
    fn untrusted_text_cannot_drive_the_terminal() {
        let text = sanitize_text("ok\x1b]0;pwned\x07\r\u{202E}line\nnext\u{9b}2J");
        assert!(!text.chars().any(|c| c.is_control() && c != '\n'), "{text:?}");
        assert!(!text.contains('\u{202E}'));
        assert!(text.contains("\nnext"));
    }

    /// Run `execute` against a shell script standing in for the worker
    /// binary, inside the real sandbox. `None` if this host forbids the
    /// sandbox (packaging containers); native CI must run these.
    fn with_worker<T>(script: &str, budget: Duration, work: impl FnOnce() -> T) -> Option<T> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let worker = dir.path().join("worker");
        std::fs::write(&worker, format!("#!/usr/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755)).unwrap();
        with_executable(worker, budget, work)
    }

    /// Like [`with_worker`], with an executable bound as the worker.
    fn with_executable<T>(
        worker: PathBuf,
        budget: Duration,
        work: impl FnOnce() -> T,
    ) -> Option<T> {
        let probe = Command::new("/usr/bin/bwrap")
            .args(["--unshare-all", "--ro-bind", "/usr", "/usr", "--symlink", "usr/lib", "/lib"])
            .args(["--symlink", "usr/lib64", "/lib64", "/usr/bin/true"])
            .status();
        if !probe.is_ok_and(|status| status.success()) {
            let container =
                Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists();
            assert!(container, "codec sandbox unavailable outside a container");
            return None;
        }
        TEST_EXECUTABLE.with(|slot| *slot.borrow_mut() = Some((worker, budget, false)));
        let result = work();
        TEST_EXECUTABLE.with(|slot| *slot.borrow_mut() = None);
        Some(result)
    }

    fn input() -> tempfile::NamedTempFile {
        let file = tempfile::Builder::new().suffix(".png").tempfile().unwrap();
        image::RgbImage::new(2, 2).save(file.path()).unwrap();
        file
    }

    fn process_running(marker: &str) -> bool {
        std::fs::read_dir("/proc").unwrap().flatten().any(|entry| {
            std::fs::read(entry.path().join("cmdline"))
                .is_ok_and(|cmdline| String::from_utf8_lossy(&cmdline).contains(marker))
        })
    }

    #[test]
    fn hostile_worker_header_is_rejected() {
        let file = input();
        let result = with_worker("printf 'BIGIRIS2garbage'", DEADLINE, || decode(file.path()));
        if let Some(result) = result {
            assert!(result.is_err());
        }
    }

    #[test]
    fn replaced_executable_adds_the_restart_hint_to_protocol_errors() {
        let file = input();
        let Some(result) = with_worker("printf '%040d' 0", DEADLINE, || {
            TEST_EXECUTABLE.with(|slot| slot.borrow_mut().as_mut().unwrap().2 = true);
            decode(file.path())
        }) else {
            return;
        };
        let message = result.unwrap_err().to_string();
        assert!(message.contains("protocolo") && message.ends_with(RESTART_HINT), "{message}");
        // A worker that already explains the upgrade is not hinted twice.
        let Some(result) = with_worker("exit 86", DEADLINE, || {
            TEST_EXECUTABLE.with(|slot| slot.borrow_mut().as_mut().unwrap().2 = true);
            decode(file.path())
        }) else {
            return;
        };
        let message = result.unwrap_err().to_string();
        assert!(message.contains("reinicie") && !message.ends_with(RESTART_HINT), "{message}");
    }

    #[test]
    fn hostile_worker_diagnostics_are_bounded_and_sanitized() {
        let file = input();
        let script =
            "i=0; while [ $i -lt 2000 ]; do printf '\\033]0;owned\\007\\033[2J%0500d' 0 >&2; \
                      i=$((i+1)); done; exit 1";
        let Some(result) = with_worker(script, DEADLINE, || decode(file.path())) else { return };
        let message = result.unwrap_err().to_string();
        assert!(message.len() < MAX_DIAGNOSTIC * 4, "{} bytes", message.len());
        assert!(!message.contains('\x1b') && !message.contains('\x07'));
    }

    /// TIFF EXIF: Make "Cam", Orientation 6 and an Exif sub-IFD holding
    /// DateTimeOriginal.
    fn camera_exif() -> Vec<u8> {
        let mut exif = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 3, 0];
        exif.extend_from_slice(&[0x0f, 0x01, 2, 0, 4, 0, 0, 0, b'C', b'a', b'm', 0]);
        exif.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]);
        // ExifIFDPointer -> 50.
        exif.extend_from_slice(&[0x69, 0x87, 4, 0, 1, 0, 0, 0, 50, 0, 0, 0]);
        exif.extend_from_slice(&[0, 0, 0, 0]);
        // Exif IFD: DateTimeOriginal (0x9003), ASCII, 20 bytes at 68.
        exif.extend_from_slice(&[1, 0, 0x03, 0x90, 2, 0, 20, 0, 0, 0, 68, 0, 0, 0]);
        exif.extend_from_slice(&[0, 0, 0, 0]);
        exif.extend_from_slice(b"2024:05:06 07:08:09\0");
        exif
    }

    fn resave_payload(image: &DynamicImage, source: &bigimage_core::SourceMetadata) -> Vec<u8> {
        let mut payload = Vec::new();
        send_resave(&mut payload, image, Some(Format::Jpeg), source).unwrap();
        payload
    }

    fn receive_bytes(bytes: &[u8], trailer: bool) -> ImageResult<bigimage_core::SourceMetadata> {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        file.rewind().unwrap();
        receive_with(file, trailer).map(|(_, source)| source)
    }

    #[test]
    fn resave_protocol_round_trips_pixels_and_metadata() {
        let image = DynamicImage::ImageRgb16(ImageBuffer::from_pixel(3, 2, image::Rgb([1, 2, 3])));
        let source =
            bigimage_core::SourceMetadata { exif: Some(camera_exif()), icc: Some(vec![7; 300]) };
        let payload = resave_payload(&image, &source);
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&payload).unwrap();
        file.rewind().unwrap();
        let (pixels, received) = receive_with(file, true).unwrap();
        assert_eq!(pixels.image, image);
        assert_eq!(pixels.format, Some(Format::Jpeg));
        assert_eq!(received, source);
        // Empty metadata stays empty, and the plain protocol rejects a trailer.
        let bare = resave_payload(&image, &bigimage_core::SourceMetadata::default());
        assert_eq!(receive_bytes(&bare, true).unwrap(), Default::default());
        assert!(receive_bytes(&payload, false).is_err());
    }

    #[test]
    fn resave_protocol_rejects_hostile_trailers() {
        let image = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(2, 2, image::Rgb([9, 9, 9])));
        let source = bigimage_core::SourceMetadata { exif: Some(vec![1; 10]), icc: None };
        let payload = resave_payload(&image, &source);
        let trailer = HEADER + 12;
        // Truncated blob, extra bytes and a missing trailer.
        assert!(receive_bytes(&payload[..payload.len() - 1], true).is_err());
        assert!(receive_bytes(&[payload.as_slice(), &[0]].concat(), true).is_err());
        assert!(receive_bytes(&payload[..trailer], true).is_err());
        // Claimed blobs above the caps are refused before allocating them.
        for (offset, claim) in [(0, MAX_RESAVE_EXIF + 1), (8, MAX_RESAVE_ICC + 1), (0, u64::MAX)] {
            let mut forged = payload.clone();
            forged[trailer + offset..trailer + offset + 8].copy_from_slice(&claim.to_le_bytes());
            let error = receive_bytes(&forged, true).unwrap_err().to_string();
            assert!(error.contains("excessivos"), "{error}");
        }
    }

    #[test]
    fn oversized_resave_metadata_is_bounded_in_the_worker() {
        let mut image = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(1, 1, image::Rgb([1; 3])));
        let mut source = bigimage_core::SourceMetadata {
            exif: Some(vec![0; MAX_RESAVE_EXIF as usize + 1]),
            icc: Some(vec![0; 64]),
        };
        bound_resave_metadata(&mut image, &mut source).unwrap();
        assert_eq!(source.exif, None);
        assert_eq!(source.icc.map(|icc| icc.len()), Some(64));
    }

    #[test]
    fn hostile_resave_worker_is_rejected_through_the_sandbox() {
        // The fake worker replays a forged payload stored as the input file.
        let image = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(2, 2, image::Rgb([9, 9, 9])));
        let mut payload = resave_payload(&image, &bigimage_core::SourceMetadata::default());
        let trailer = HEADER + 12;
        payload[trailer..trailer + 8].copy_from_slice(&(MAX_RESAVE_EXIF + 1).to_le_bytes());
        let file = tempfile::Builder::new().suffix(".png").tempfile().unwrap();
        std::fs::write(file.path(), &payload).unwrap();
        let Some(result) =
            with_worker("exec /usr/bin/cat \"$4\"", DEADLINE, || resave(file.path()))
        else {
            return;
        };
        assert!(result.unwrap_err().to_string().contains("excessivos"));
    }

    #[test]
    fn sandboxed_resave_keeps_exif_and_wide_gamut_profile() {
        use bigimage_core::{EncodeOptions, OverwritePolicy};
        use image::{ImageDecoder, ImageEncoder};
        let Some(binary) = built_worker() else {
            eprintln!("skipped: build the bigiris binary (cargo test --all-targets)");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.jpg");
        let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
        // Stored sideways (Orientation 6): 4 wide x 2 high on disk.
        let stored = image::RgbImage::from_pixel(4, 2, image::Rgb([200, 60, 30]));
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
            std::fs::File::create(&path).unwrap(),
            95,
        );
        encoder.set_icc_profile(p3.clone()).unwrap();
        encoder.set_exif_metadata(camera_exif()).unwrap();
        encoder.write_image(stored.as_raw(), 4, 2, image::ExtendedColorType::Rgb8).unwrap();

        // Run the real worker binary through the sandbox, like `configure()`
        // makes `open_image_for_resave` do in the application.
        let Some(result) = with_executable(binary, DEADLINE, || resave(&path)) else { return };
        let (image, format, source) = result.unwrap();
        assert_eq!(format, Some(Format::Jpeg));
        assert_eq!((image.width(), image.height()), (2, 4), "upright");
        assert_eq!(source.icc.as_deref(), Some(&p3[..]));
        let px = image.to_rgb8().get_pixel(0, 0).0;
        assert!(px[0].abs_diff(200) <= 3 && px[1].abs_diff(60) <= 3, "kept in P3: {px:?}");

        bigimage_core::save_image_preserving_metadata(
            image.fliph(),
            &path,
            Format::Jpeg,
            &EncodeOptions { quality: Some(95), ..EncodeOptions::default() },
            OverwritePolicy::Replace,
            &source,
        )
        .unwrap();
        let meta = bigimage_core::metadata::read(&path).unwrap();
        assert!(meta.has_camera_info, "Make lost: {:?}", meta.tags);
        assert!(
            meta.tags
                .iter()
                .any(|(name, value)| name.contains("original") && value.contains("2024-05-06")),
            "DateTimeOriginal lost: {:?}",
            meta.tags
        );
        assert_eq!(meta.orientation, Some(1), "pixels are upright, tag must say so");
        let file = std::io::BufReader::new(std::fs::File::open(&path).unwrap());
        let icc = image::codecs::jpeg::JpegDecoder::new(file).unwrap().icc_profile().unwrap();
        assert_eq!(icc.as_deref(), Some(&p3[..]));
    }

    #[test]
    fn sandboxed_decode_survives_inherited_descriptors() {
        let Some(binary) = built_worker() else {
            eprintln!("skipped: build the bigiris binary (cargo test --all-targets)");
            return;
        };
        // Like the GUI after Mesa starts: a host descriptor without CLOEXEC.
        let leaked = tempfile::tempfile().unwrap();
        rustix::io::fcntl_setfd(&leaked, rustix::io::FdFlags::empty()).unwrap();
        let file = input();
        let Some(result) = with_executable(binary, DEADLINE, || decode(file.path())) else {
            return;
        };
        assert_eq!(result.unwrap().0.dimensions(), (2, 2));
        drop(leaked);
    }

    /// What the GUI does after its first frame: decode on a batch worker
    /// thread (no thread-local override) while the process holds a
    /// non-CLOEXEC descriptor. Configures the process-wide backend, so it
    /// is ignored by default; run alone.
    #[test]
    #[ignore = "configures the process-wide codec backend; run alone"]
    fn configured_backend_decodes_on_other_threads_with_leaked_descriptors() {
        let worker = configure_for_tests(None);
        assert!(worker.is_file());
        let leaked = tempfile::tempfile().unwrap();
        rustix::io::fcntl_setfd(&leaked, rustix::io::FdFlags::empty()).unwrap();
        let file = input();
        let path = file.path().to_path_buf();
        let image =
            std::thread::spawn(move || bigimage_core::open_image(&path)).join().unwrap().unwrap();
        assert_eq!(image.dimensions(), (2, 2));
        drop(leaked);
    }

    #[test]
    fn deadline_and_cancel_stop_the_sandboxed_worker() {
        let file = input();
        let started = Instant::now();
        let script = "exec /usr/bin/sleep 30.31337";
        let Some(result) = with_worker(script, Duration::from_millis(500), || decode(file.path()))
        else {
            return;
        };
        assert!(result.unwrap_err().to_string().contains("limite"));
        assert!(started.elapsed() < Duration::from_secs(10));

        let flag = Arc::new(AtomicBool::new(false));
        let trigger = flag.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            trigger.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let result = with_worker(script, DEADLINE, || with_cancel(flag, || decode(file.path())));
        assert!(result.unwrap().unwrap_err().to_string().contains("cancelada"));
        assert!(started.elapsed() < Duration::from_secs(10));
        // --die-with-parent tears the sandbox down with bwrap.
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_running("30.31337") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!process_running("30.31337"), "sandboxed worker outlived its parent");
    }
}
