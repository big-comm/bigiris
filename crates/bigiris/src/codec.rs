// SPDX-License-Identifier: GPL-3.0-or-later
//! Confine untrusted codecs to a short-lived, resource-limited process.
//! The parent accepts only a validated, bounded raw-pixel protocol.
use bigimage_core::{BigImageError, Format, Metadata, PreviewSession, backend};
use gettextrs::gettext;
use image::{ColorType, DynamicImage, GenericImageView, ImageBuffer, ImageError, ImageResult};
use std::cell::RefCell;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

const MAGIC: &[u8; 8] = b"BIGIRIS2";
const HEADER: usize = 36;
const MAX_BYTES: u64 = 512 * 1024 * 1024;
const MAX_INPUT: u64 = 1024 * 1024 * 1024;
const MAX_METADATA: u64 = 1024 * 1024;
/// EXIF block or ICC profile a re-save carries: what core writes back.
const MAX_RESAVE_BLOB: u64 = bigimage_core::metadata::MAX_RESAVE_BLOB as u64;
/// Re-save trailer after the pixels: EXIF and ICC lengths (u64 LE each),
/// the estimated JPEG quality (one byte, 0 = unknown, else 1..=100), then
/// the blobs the worker dropped or could not read (one byte of
/// `DROPPED_*` and `UNREADABLE_EXIF` bits).
const RESAVE_TRAILER: usize = 18;
const DROPPED_EXIF: u8 = 1;
const DROPPED_ICC: u8 = 2;
/// The source's EXIF item exists but could not be read.
const UNREADABLE_EXIF: u8 = 4;
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
    SANDBOXED.store(true, Ordering::Release);
}

/// Set once [`configure`] routed decoding through the sandboxed worker.
static SANDBOXED: AtomicBool = AtomicBool::new(false);

/// Exercise the real process boundary and raw protocol.
pub fn self_test() -> color_eyre::Result<()> {
    let input = tempfile::Builder::new().suffix(".png").tempfile()?;
    image::RgbImage::from_pixel(2, 3, image::Rgb([30, 70, 120])).save(input.path())?;
    let image = decode(input.path())?.0;
    if image.dimensions() != (2, 3) {
        return Err(color_eyre::eyre::eyre!("invalid sandbox decode"));
    }
    attest(input.path())?;
    Ok(())
}

/// Namespaces `--unshare-all` gives the worker on every host; `user` is
/// checked too when [`userns_hardening`] asks bwrap for one explicitly.
const NAMESPACES: [&str; 6] = ["cgroup", "ipc", "mnt", "net", "pid", "uts"];

/// Have a worker check, from inside, that its sandbox is the one
/// [`execute`] asks for. The worker reports its namespaces and whether it
/// could create a user namespace; only this side knows its own namespaces
/// and whether bwrap was told to forbid nested ones.
fn attest(input: &Path) -> ImageResult<()> {
    let report = run(input, "attest", |output| {
        let mut report = String::new();
        output.take(4096).read_to_string(&mut report)?;
        Ok(report)
    })?;
    let accepted: &[&str] =
        if userns_hardening() { &["userns=denied"] } else { &["userns=denied", "userns=allowed"] };
    if !report.lines().next().is_some_and(|userns| accepted.contains(&userns)) {
        return Err(invalid(format!("invalid sandbox attestation: {}", sanitize_text(&report))));
    }
    let user = userns_hardening().then_some("user");
    for kind in NAMESPACES.into_iter().chain(user) {
        let own = std::fs::read_link(format!("/proc/self/ns/{kind}"))?;
        let theirs = report.lines().find_map(|line| line.strip_prefix(kind)?.strip_prefix(' '));
        if theirs.is_none_or(|theirs| Path::new(theirs) == own) {
            return Err(invalid(format!("the sandbox shares the {kind} namespace")));
        }
    }
    Ok(())
}

/// Worker side of [`attest`]: fail unless every limit [`execute`] sets is in
/// force, then report the namespaces and whether a nested user namespace
/// could be created.
fn attest_sandbox(input: &Path) -> color_eyre::Result<()> {
    attest_host_hidden()?;
    attest_limits()?;
    attest_process()?;
    attest_mounts(input)?;
    let nested = Command::new("/usr/bin/unshare")
        .args(["--user", "/usr/bin/true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    println!("userns={}", if nested.success() { "allowed" } else { "denied" });
    for kind in NAMESPACES.into_iter().chain(["user"]) {
        println!("{kind} {}", std::fs::read_link(format!("/proc/self/ns/{kind}"))?.display());
    }
    Ok(())
}

fn attest_host_hidden() -> color_eyre::Result<()> {
    for location in ["/home", "/root", "/run", "/etc"] {
        if Path::new(location).exists() {
            color_eyre::eyre::bail!("host directory visible: {location}");
        }
    }
    let network = std::fs::read_to_string("/proc/net/dev")?;
    if network.lines().skip(2).any(|line| !line.trim_start().starts_with("lo:")) {
        color_eyre::eyre::bail!("host network visible");
    }
    Ok(())
}

/// The `prlimit` values, soft and hard.
fn attest_limits() -> color_eyre::Result<()> {
    let limits = std::fs::read_to_string("/proc/self/limits")?;
    for (name, expected) in [
        ("Max address space", WORKER_ADDRESS_SPACE),
        ("Max cpu time", WORKER_CPU_SECONDS),
        ("Max open files", WORKER_FILES),
        ("Max core file size", 0),
        ("Max file size", output_limit("attest")),
    ] {
        let expected = expected.to_string();
        let values: Option<Vec<&str>> = limits
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .map(|rest| rest.split_whitespace().take(2).collect());
        if values.as_deref() != Some(&[expected.as_str(), expected.as_str()]) {
            color_eyre::eyre::bail!("{name} is not {expected}: {values:?}");
        }
    }
    Ok(())
}

/// Capabilities, session, parent-death signal and environment.
fn attest_process() -> color_eyre::Result<()> {
    use color_eyre::eyre::bail;
    let status = std::fs::read_to_string("/proc/self/status")?;
    for set in ["CapInh:", "CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"] {
        let value = status.lines().find_map(|line| line.strip_prefix(set)).map(str::trim);
        if value != Some("0000000000000000") {
            bail!("capabilities kept: {set} {value:?}");
        }
    }
    // Session 0: the session leader is outside the sandbox's PID namespace,
    // so the codec could reach the caller's terminal.
    let stat = std::fs::read_to_string("/proc/self/stat")?;
    let session = stat.rsplit_once(')').and_then(|(_, fields)| fields.split_whitespace().nth(3));
    if session.is_none_or(|session| session == "0") {
        bail!("the worker shares the caller's session: {session:?}");
    }
    let death = rustix::process::parent_process_death_signal()?;
    if death != Some(rustix::process::Signal::KILL) {
        bail!("no parent death signal: {death:?}");
    }
    let mut environment: Vec<_> = std::env::vars_os().map(|(name, _)| name).collect();
    environment.sort();
    // bwrap sets PWD from --chdir.
    if environment != ["LC_ALL", "PWD", "RAYON_NUM_THREADS"] {
        bail!("unexpected environment: {environment:?}");
    }
    Ok(())
}

/// Every mount but the 64 MiB `/tmp` is read-only.
fn attest_mounts(input: &Path) -> color_eyre::Result<()> {
    let read_only = |path: &Path| -> rustix::io::Result<bool> {
        Ok(rustix::fs::statvfs(path)?.f_flag.contains(rustix::fs::StatVfsMountFlags::RDONLY))
    };
    for mount in
        [Path::new("/"), Path::new("/usr"), Path::new("/dev"), Path::new("/app/bigiris"), input]
    {
        if !read_only(mount)? {
            color_eyre::eyre::bail!("{} is writable", mount.display());
        }
    }
    let tmp = rustix::fs::statvfs("/tmp")?;
    if tmp.f_blocks.saturating_mul(tmp.f_frsize) != WORKER_TMP {
        color_eyre::eyre::bail!("/tmp is not limited to {WORKER_TMP} bytes");
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
/// Limits `prlimit` sets on every worker (soft and hard), checked again by
/// the worker's `attest` mode.
const WORKER_ADDRESS_SPACE: u64 = 2 * 1024 * 1024 * 1024;
const WORKER_CPU_SECONDS: u64 = 30;
const WORKER_FILES: u64 = 64;
/// Size of the worker's `/tmp`, its only writable file system.
const WORKER_TMP: u64 = 64 * 1024 * 1024;

#[cfg(test)]
thread_local! {
    /// Test-only replacement for the worker binary (never compiled in releases).
    static TEST_EXECUTABLE: RefCell<Option<(PathBuf, Duration, bool)>> = const { RefCell::new(None) };
    /// Test-only rewrite of the sandbox command: the first run of tokens
    /// equal to `.0` becomes `.1`, so a test can weaken one limit.
    static TEST_SANDBOX_EDIT: RefCell<Option<(Vec<String>, Vec<String>)>> = const { RefCell::new(None) };
}

#[cfg(test)]
fn edit_sandbox(argv: &mut Vec<std::ffi::OsString>) {
    TEST_SANDBOX_EDIT.with(|edit| {
        let Some((from, to)) = &*edit.borrow() else { return };
        let at = argv
            .windows(from.len())
            .position(|window| window.iter().zip(from).all(|(arg, token)| arg == token.as_str()))
            .unwrap_or_else(|| panic!("{from:?} is not in the sandbox command"));
        argv.splice(at..at + from.len(), to.iter().map(std::ffi::OsString::from));
    });
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
            gettext("the BigIris executable was removed; restart BigIris"),
        )
    })
}

#[cfg(test)]
/// Test-only, process-wide worker binary (see [`configure_for_tests`]);
/// unlike `TEST_EXECUTABLE` it reaches batch worker threads too.
static TEST_WORKER: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The real `bigiris` binary next to the test harness: `target/<profile>/
/// bigiris`, built by `cargo test --all-targets` for the CLI tests. The
/// harness sits in `target/<profile>/deps/` with the classic layout and in
/// `target/<profile>/build/bigiris/<hash>/out/` with the newer build-dir
/// layout, so the nearest ancestor holding a `bigiris` file wins. Panics when
/// there is none, or when it comes from another commit: the worker refuses a
/// parent of another build, which would otherwise surface as an opaque
/// failure or a GUI timeout.
#[cfg(test)]
pub(crate) fn built_worker() -> PathBuf {
    static WORKER: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    WORKER
        .get_or_init(|| {
            let exe = std::env::current_exe().expect("test harness path");
            let path = exe
                .ancestors()
                .skip(1)
                .take(5)
                .map(|dir| dir.join("bigiris"))
                .find(|path| path.is_file() && *path != exe)
                .expect("bigiris binary missing: cargo test --all-targets");
            let binary = std::fs::read(&path).expect("read the bigiris worker binary");
            assert!(
                binary.windows(WORKER_ID.len()).any(|w| w == WORKER_ID.as_bytes()),
                "{} was built from another commit than this test; rebuild it with \
                 `cargo build -p bigiris --features \"gui ai\"` (or run cargo test --all-targets)",
                path.display()
            );
            path
        })
        .clone()
}

/// Test-only: route every decode in this process through the real sandbox,
/// like `main` does, with `worker` (default: [`built_worker`]) as the codec
/// binary on every thread. Idempotent; the first worker path wins.
#[cfg(test)]
pub(crate) fn configure_for_tests(worker: Option<PathBuf>) -> PathBuf {
    static CONFIGURE: std::sync::Once = std::sync::Once::new();
    let path = TEST_WORKER.get_or_init(|| worker.unwrap_or_else(built_worker)).clone();
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
pub(crate) fn close_inherited_descriptors(command: &mut Command) -> &mut Command {
    install_descriptor_hook(command, None)
}

/// [`close_inherited_descriptors`], except that `fd` reaches the child as
/// descriptor 3, the only one above stdio that survives `exec`.
fn close_inherited_descriptors_passing(
    command: &mut Command,
    fd: std::os::fd::RawFd,
) -> &mut Command {
    install_descriptor_hook(command, Some(fd))
}

#[expect(unsafe_code, reason = "pre_exec hook; its SAFETY comment holds the invariant")]
fn install_descriptor_hook(
    command: &mut Command,
    pass_as_3: Option<std::os::fd::RawFd>,
) -> &mut Command {
    use std::os::unix::process::CommandExt;
    // SAFETY: the hook runs in the forked child before `exec`. It only issues
    // raw, async-signal-safe syscalls (close_range, getrlimit, fcntl, dup2):
    // no allocation, no locks and no access to memory shared with other
    // threads. Marking descriptors close-on-exec (rather than closing them)
    // keeps the pipe std uses to report `exec` failures working until `exec`
    // itself. A passed descriptor stays open in the parent until `spawn`
    // returns.
    unsafe { command.pre_exec(move || prepare_descriptors(pass_as_3)) }
}

/// Body of the `pre_exec` hook; see [`install_descriptor_hook`].
#[expect(unsafe_code, reason = "raw descriptor syscalls in the forked child before exec")]
fn prepare_descriptors(pass_as_3: Option<std::os::fd::RawFd>) -> std::io::Result<()> {
    // SAFETY: plain syscalls on integer descriptors. Marking failures are
    // ignored because the worker re-checks its descriptor table and fails
    // closed; a failed hand-over of descriptor 3 fails the spawn.
    unsafe {
        // Linux >= 5.11: one syscall for the whole range.
        if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, libc::CLOSE_RANGE_CLOEXEC) != 0 {
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
        // dup2 clears close-on-exec on the copy; a descriptor that already
        // is 3 has its flag cleared directly.
        let placed = match pass_as_3 {
            None => 0,
            Some(3) => libc::fcntl(3, libc::F_SETFD, 0),
            Some(fd) => libc::dup2(fd, 3),
        };
        if placed < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
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
                "bwrap unavailable to test --disable-userns; sandbox runs without that hardening"
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
                "bwrap refused --disable-userns (bwrap < 0.8 or system policy); \
                 the isolated codec runs without that hardening"
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
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let pidfd = rustix::process::pidfd_open(
        rustix::process::Pid::from_child(child),
        rustix::process::PidfdFlags::empty(),
    )
    .ok();
    loop {
        if cancellable && cancelled() {
            return Err(invalid(gettext("operation canceled")));
        }
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(invalid(gettext("the codec exceeded its time limit")));
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
            if (c.is_control() && c != '\n') || bidi { '\u{FFFD}' } else { c }
        })
        .collect()
}

/// Appended to worker failures when the executable was replaced on disk.
fn restart_hint() -> String {
    format!(" ({})", gettext("BigIris was updated; restart it"))
}

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
        if replaced && !cancelled() { invalid(format!("{error}{}", restart_hint())) } else { error }
    })
}

/// Largest output file a worker in `mode` may write (`prlimit --fsize`):
/// only pixel modes need large outputs; everything else is tiny.
fn output_limit(mode: &str) -> u64 {
    match mode {
        "pixels" | "preview" => MAX_BYTES + HEADER as u64,
        "resave" => MAX_BYTES + (HEADER + RESAVE_TRAILER) as u64 + 2 * MAX_RESAVE_BLOB,
        _ => MAX_METADATA + 4096,
    }
}

/// `replaced` reports whether failures still need the restart hint.
fn execute(path: &Path, mode: &str, replaced: &mut bool) -> ImageResult<std::fs::File> {
    if cancelled() {
        return Err(invalid(gettext("operation canceled")));
    }
    let input = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(path)?;
    let info = input.metadata()?;
    if !info.is_file() || info.len() > MAX_INPUT {
        return Err(invalid(gettext("not a regular file, or larger than 1 GiB")));
    }
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|e| Format::parse(e).ok())
        .map_or("img", |format| format.extension());
    let sandbox_input = format!("/input.{extension}");
    let (executable, is_replaced, budget) = worker_executable()?;
    *replaced = is_replaced;
    let output = tempfile::tempfile()?;
    let mut argv: Vec<std::ffi::OsString> = [
        format!("--as={WORKER_ADDRESS_SPACE}"),
        format!("--cpu={WORKER_CPU_SECONDS}"),
        format!("--nofile={WORKER_FILES}"),
        "--core=0".to_owned(),
        format!("--fsize={}", output_limit(mode)),
        "--".to_owned(),
        "/usr/bin/bwrap".to_owned(),
        "--unshare-all".to_owned(),
    ]
    .map(Into::into)
    .into();
    if userns_hardening() {
        argv.extend(["--unshare-user", "--disable-userns"].map(Into::into));
    }
    let tmp_size = WORKER_TMP.to_string();
    argv.extend(
        [
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
            &tmp_size,
            "--tmpfs",
            "/tmp",
            "--dir",
            "/app",
            "--ro-bind",
        ]
        .map(Into::into),
    );
    argv.extend([executable.into_os_string(), "/app/bigiris".into()]);
    argv.extend(["--ro-bind-fd", "3", &sandbox_input].map(Into::into));
    // The root tmpfs has no size limit: once the last mount point exists it
    // turns read-only, leaving the bounded /tmp as the only writable place.
    argv.extend(
        [
            "--remount-ro",
            "/",
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
            mode,
            &sandbox_input,
        ]
        .map(Into::into),
    );
    #[cfg(test)]
    edit_sandbox(&mut argv);
    let mut command = Command::new("/usr/bin/prlimit");
    // Not stdin: a descriptor the codec keeps open would let it reopen the
    // host file writable through /proc/self/fd, past the read-only mount.
    // bwrap closes descriptor 3 once the bind is made.
    close_inherited_descriptors_passing(&mut command, input.as_raw_fd())
        .env_clear()
        .args(&argv)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::piped());
    let child = command.spawn().map_err(|error| {
        invalid(crate::with_error(
            &gettext("sandbox unavailable: install bubblewrap and util-linux; {error}"),
            error,
        ))
    })?;
    drop(input);
    let mut child = ChildGuard(child);
    let diagnostics = child.0.stderr.take().map(drain_diagnostics);
    let status = wait_child(&mut child.0, Instant::now() + budget, true)?;
    if !status.success() {
        // Both messages below already carry the restart advice.
        let hint = if std::mem::take(replaced) { restart_hint() } else { String::new() };
        if status.code() == Some(VERSION_MISMATCH) {
            return Err(invalid(gettext("BigIris was updated while it was open; restart it")));
        }
        let details = diagnostics
            .and_then(|rx| rx.recv_timeout(Duration::from_secs(2)).ok())
            .unwrap_or_default();
        let details = sanitize_text(String::from_utf8_lossy(&details).trim());
        return Err(invalid(crate::fill(
            &gettext("the isolated codec rejected the image ({status}){hint}: {details}"),
            &[("status", &status.to_string()), ("hint", &hint), ("details", &details)],
        )));
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
/// What a re-save decode adds to the pixels: the source EXIF/ICC, for JPEG
/// sources the estimated encoder quality, and the blobs over
/// [`MAX_RESAVE_BLOB`] the worker dropped.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ResaveExtras {
    pub(crate) source: bigimage_core::SourceMetadata,
    pub(crate) quality: Option<u8>,
    exif_dropped: bool,
    icc_dropped: bool,
}
impl ResaveExtras {
    /// Source metadata lost before the save began, as a save would report it.
    #[cfg(any(feature = "gui", test))]
    pub(crate) fn losses(&self) -> Vec<bigimage_core::MetadataLoss> {
        use bigimage_core::MetadataLoss;
        let exif = self.exif_dropped.then_some(MetadataLoss::ExifTooLarge);
        let icc = self.icc_dropped.then_some(MetadataLoss::IccDiscarded);
        exif.into_iter().chain(icc).collect()
    }
}
fn receive(output: std::fs::File) -> ImageResult<Pixels> {
    receive_with(output, false).map(|(pixels, _)| pixels)
}
/// Read the raw pixel protocol. With `trailer`, the EXIF and ICC blobs of a
/// re-save decode follow the pixels and are bounded like everything else.
fn receive_with(mut output: std::fs::File, trailer: bool) -> ImageResult<(Pixels, ResaveExtras)> {
    let mut header = [0u8; HEADER];
    output.read_exact(&mut header)?;
    if &header[..8] != MAGIC || header[10] > 1 || header[11] > 1 {
        return Err(invalid("invalid codec protocol"));
    }
    let word = |offset| u32::from_le_bytes(header[offset..offset + 4].try_into().unwrap());
    let (w, h) = (word(12), word(16));
    let natural = (word(20), word(24));
    let length = u64::from_le_bytes(header[28..36].try_into().unwrap());
    let color = color_from_code(header[8]).ok_or_else(|| invalid("invalid pixel type"))?;
    if w == 0
        || h == 0
        || natural.0 == 0
        || natural.1 == 0
        || u64::from(natural.0) * u64::from(natural.1) > 256 * 1024 * 1024
        || u64::from(w) * u64::from(h) > 256 * 1024 * 1024
        || length > MAX_BYTES
        || length != u64::from(w) * u64::from(h) * u64::from(color.bytes_per_pixel())
    {
        return Err(invalid("invalid protocol dimensions or size"));
    }
    let total = output.metadata()?.len();
    let pixels_end = HEADER as u64 + length;
    let mut extras = ResaveExtras::default();
    let (exif_length, icc_length) = if trailer {
        if total < pixels_end + RESAVE_TRAILER as u64 {
            return Err(invalid("invalid protocol dimensions or size"));
        }
        let mut lengths = [0u8; RESAVE_TRAILER];
        output.seek(SeekFrom::Start(pixels_end))?;
        output.read_exact(&mut lengths)?;
        output.seek(SeekFrom::Start(HEADER as u64))?;
        let exif = u64::from_le_bytes(lengths[..8].try_into().unwrap());
        let icc = u64::from_le_bytes(lengths[8..16].try_into().unwrap());
        if exif > MAX_RESAVE_BLOB || icc > MAX_RESAVE_BLOB {
            return Err(invalid("excessive metadata"));
        }
        extras.quality = match lengths[16] {
            0 => None,
            q @ 1..=100 => Some(q),
            _ => return Err(invalid("invalid JPEG quality")),
        };
        if lengths[17] & !(DROPPED_EXIF | DROPPED_ICC | UNREADABLE_EXIF) != 0 {
            return Err(invalid("invalid codec protocol"));
        }
        extras.exif_dropped = lengths[17] & DROPPED_EXIF != 0;
        extras.icc_dropped = lengths[17] & DROPPED_ICC != 0;
        extras.source.exif_unreadable = lengths[17] & UNREADABLE_EXIF != 0;
        (exif, icc)
    } else {
        (0, 0)
    };
    let expected =
        pixels_end + if trailer { RESAVE_TRAILER as u64 + exif_length + icc_length } else { 0 };
    if total != expected {
        return Err(invalid("invalid protocol dimensions or size"));
    }
    let format = if header[9] == 255 {
        None
    } else {
        Some(*Format::all().get(header[9] as usize).ok_or_else(|| invalid("invalid format"))?)
    };
    let length = length as usize;
    // Decode wide samples straight into their typed buffer through a small
    // scratch chunk, so peak memory stays at one copy of the pixels.
    macro_rules! buffer {
        ($data:expr_2021, $variant:ident) => {
            DynamicImage::$variant(
                ImageBuffer::from_raw(w, h, $data).ok_or_else(|| invalid("invalid buffer"))?,
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
        _ => return Err(invalid("invalid pixel type")),
    };
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
        extras.source.exif = blob(exif_length)?;
        extras.source.icc = blob(icc_length)?;
    }
    if header[11] == 1 {
        bigimage_core::color::note_ignored_profile();
    }
    Ok((Pixels { image, format, natural, gps: header[10] == 1 }, extras))
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
    let (pixels, extras) = resave_with_extras(path)?;
    Ok((pixels.image, pixels.format, extras.source))
}
/// [`resave`] plus what the worker estimated and dropped.
fn resave_with_extras(path: &Path) -> ImageResult<(Pixels, ResaveExtras)> {
    run(path, "resave", |output| receive_with(output, true))
}

/// Decode `path` for an in-place save: upright pixels in the source colour
/// space, its EXIF and ICC, the quality a JPEG source was estimated at (so
/// re-encoding keeps it rather than running the perceptual search, which can
/// lower it on every save), and the metadata the worker dropped. Without the
/// sandboxed backend (unit tests) the quality is unknown and a JPEG save runs
/// the perceptual search.
#[cfg(feature = "gui")]
pub(crate) fn open_for_resave(path: &Path) -> ImageResult<(DynamicImage, ResaveExtras)> {
    if SANDBOXED.load(Ordering::Acquire) {
        let (pixels, extras) = resave_with_extras(path)?;
        return Ok((pixels.image, extras));
    }
    let (image, source) = bigimage_core::open_image_for_resave(path)?;
    Ok((image, ResaveExtras { source, ..ResaveExtras::default() }))
}
fn dimensions(path: &Path) -> ImageResult<(u32, u32)> {
    run(path, "dimensions", read_dimensions)
}
fn read_dimensions(mut output: std::fs::File) -> ImageResult<(u32, u32)> {
    if output.metadata()?.len() != 8 {
        return Err(invalid("invalid dimensions"));
    }
    let mut bytes = [0; 8];
    output.read_exact(&mut bytes)?;
    let size = (
        u32::from_le_bytes(bytes[..4].try_into().unwrap()),
        u32::from_le_bytes(bytes[4..].try_into().unwrap()),
    );
    if size.0 == 0 || size.1 == 0 || u64::from(size.0) * u64::from(size.1) > 256 * 1024 * 1024 {
        return Err(invalid("invalid dimensions"));
    }
    Ok(size)
}
fn preview(path: &Path) -> bigimage_core::Result<PreviewSession> {
    let pixels = run(path, "preview", receive)
        .map_err(|source| BigImageError::Decode { path: path.into(), source })?;
    PreviewSession::from_cached(
        pixels.natural,
        pixels
            .format
            .ok_or_else(|| BigImageError::UnsupportedFormat("preview without a format".into()))?,
        pixels.image,
        pixels.gps,
    )
}
fn ensure_static(path: &Path) -> bigimage_core::Result<()> {
    run(path, "static", |_| Ok(()))
        .map_err(|source| BigImageError::Decode { path: path.into(), source })
}
fn metadata(path: &Path) -> bigimage_core::Result<Metadata> {
    run(path, "metadata", |output| read_json(output).map(sanitize_metadata))
        .map_err(|source| BigImageError::Decode { path: path.into(), source })
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Information {
    pub dimensions: Option<(u32, u32)>,
    pub color: Option<u8>,
    pub format: Option<String>,
    pub metadata: Option<Metadata>,
}
impl Information {
    /// The pixel layout behind [`Information::color`], if it names one.
    #[cfg(feature = "gui")]
    pub fn color_type(&self) -> Option<ColorType> {
        color_from_code(self.color?)
    }
}

/// Pixel layouts the protocol carries; a layout's wire code is its index
/// plus one, so the order is part of the protocol.
const COLOR_CODES: [ColorType; 10] = [
    ColorType::L8,
    ColorType::La8,
    ColorType::Rgb8,
    ColorType::Rgba8,
    ColorType::L16,
    ColorType::La16,
    ColorType::Rgb16,
    ColorType::Rgba16,
    ColorType::Rgb32F,
    ColorType::Rgba32F,
];
fn color_code(color: ColorType) -> Option<u8> {
    let index = COLOR_CODES.iter().position(|&known| known == color)?;
    u8::try_from(index + 1).ok()
}
fn color_from_code(code: u8) -> Option<ColorType> {
    COLOR_CODES.get(usize::from(code).checked_sub(1)?).copied()
}
fn information_local(path: &Path) -> Information {
    let dimensions = backend::dimensions_in_process(path).ok();
    let reader = image::ImageReader::open(path).ok().and_then(|r| r.with_guessed_format().ok());
    let format = reader.as_ref().and_then(|r| r.format()).map(|f| format!("{f:?}"));
    let color = reader
        .and_then(|r| r.into_decoder().ok())
        .and_then(|decoder| color_code(image::ImageDecoder::color_type(&decoder)));
    let metadata = bigimage_core::metadata::read(path).ok().map(trim_metadata);
    Information { dimensions, color, format, metadata }
}
/// Bounds of a `metadata` or `information` answer, in bytes of UTF-8.
const MAX_TAGS: usize = 128;
const MAX_TAG_NAME: usize = 128;
const MAX_TAG_VALUE: usize = 1024;
// JSON escapes one byte to at most six (`\u001f`), plus quotes and commas
// per tag; the rest of an `information` answer fits in 4 KiB.
const _: () =
    assert!(MAX_TAGS * 6 * (MAX_TAG_NAME + MAX_TAG_VALUE + 8) + 4096 <= MAX_METADATA as usize);
fn trim_metadata(mut meta: Metadata) -> Metadata {
    meta.tags.truncate(MAX_TAGS);
    for (name, value) in &mut meta.tags {
        name.truncate(name.floor_char_boundary(MAX_TAG_NAME));
        value.truncate(value.floor_char_boundary(MAX_TAG_VALUE));
    }
    meta
}
/// EXIF names and values come from the file, and a hostile worker may skip
/// its own trimming: see [`sanitize_text`] and [`trim_metadata`].
fn sanitize_metadata(mut meta: Metadata) -> Metadata {
    meta.tags.truncate(MAX_TAGS);
    for (name, value) in &mut meta.tags {
        *name = sanitize_text(name);
        *value = sanitize_text(value);
    }
    trim_metadata(meta)
}
#[cfg(feature = "gui")]
pub fn information(path: &Path) -> ImageResult<Information> {
    // Unit tests do not execute main/configure; still run their I/O off GTK.
    #[cfg(test)]
    return Ok(information_local(path));
    #[cfg(not(test))]
    {
        run(path, "information", read_information)
    }
}
#[cfg(feature = "gui")]
fn read_information(output: std::fs::File) -> ImageResult<Information> {
    let mut information: Information = read_json(output)?;
    information.format = information.format.as_deref().map(sanitize_text);
    information.metadata = information.metadata.map(sanitize_metadata);
    Ok(information)
}
/// Bounded JSON answer of the `metadata` and `information` modes.
fn read_json<T: serde::de::DeserializeOwned>(output: std::fs::File) -> ImageResult<T> {
    if output.metadata()?.len() > MAX_METADATA {
        return Err(invalid("excessive metadata"));
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
    let color = color_code(image.color()).ok_or_else(|| invalid("invalid pixel type"))?;
    let mut header = Vec::with_capacity(HEADER);
    header.extend(MAGIC);
    header.extend([
        color,
        format.map_or(255, |f| Format::all().iter().position(|x| *x == f).unwrap() as u8),
        u8::from(gps),
        // The decode that produced `image` on this thread showed a malformed
        // ICC profile as sRGB; the parent relays the note to its caller.
        u8::from(bigimage_core::color::take_ignored_profile()),
    ]);
    for value in [image.width(), image.height(), natural.0, natural.1] {
        header.extend(value.to_le_bytes());
    }
    header.extend((image.as_bytes().len() as u64).to_le_bytes());
    output.write_all(&header)?;
    output.write_all(image.as_bytes())?;
    Ok(())
}
/// Pixels, then the trailer lengths and quality, then the EXIF and ICC blobs.
fn send_resave(
    output: &mut impl Write,
    image: &DynamicImage,
    format: Option<Format>,
    extras: &ResaveExtras,
) -> ImageResult<()> {
    send_to(output, image, format, image.dimensions(), false)?;
    let exif = extras.source.exif.as_deref().unwrap_or_default();
    let icc = extras.source.icc.as_deref().unwrap_or_default();
    output.write_all(&(exif.len() as u64).to_le_bytes())?;
    output.write_all(&(icc.len() as u64).to_le_bytes())?;
    let quality = extras.quality.filter(|q| (1..=100).contains(q)).unwrap_or(0);
    let mut dropped = 0;
    if extras.exif_dropped {
        dropped |= DROPPED_EXIF;
    }
    if extras.icc_dropped {
        dropped |= DROPPED_ICC;
    }
    if extras.source.exif_unreadable {
        dropped |= UNREADABLE_EXIF;
    }
    output.write_all(&[quality, dropped])?;
    output.write_all(exif)?;
    output.write_all(icc)?;
    Ok(())
}
/// Keep re-save metadata inside the protocol caps, which are core's: a blob
/// over them could not be written back, so it is dropped and reported (a
/// profile that large cannot even be parsed to convert the pixels). A kept
/// profile goes through [`vouch_for_profile`].
fn bound_resave_metadata(source: bigimage_core::SourceMetadata) -> ResaveExtras {
    let mut extras = ResaveExtras { source, ..ResaveExtras::default() };
    let too_large = |blob: &mut Vec<u8>| blob.len() as u64 > MAX_RESAVE_BLOB;
    extras.exif_dropped = extras.source.exif.take_if(too_large).is_some();
    extras.icc_dropped = extras.source.icc.take_if(too_large).is_some();
    if let Some(icc) = &extras.source.icc {
        vouch_for_profile(icc);
    }
    extras
}
/// Run every conversion to sRGB the parent may apply to `icc` (when the
/// target cannot embed it) on a probe of each pixel layout, so a profile
/// that crashes or exhausts the parser does so in this process. Parsing and
/// building a transform depend only on the profile and the layout: the
/// parent repeats work this worker finished.
fn vouch_for_profile(icc: &[u8]) {
    let ramp = image::RgbaImage::from_fn(256, 1, |x, _| image::Rgba([x as u8; 4]));
    let ramp = DynamicImage::ImageRgba8(ramp);
    let probes = [
        DynamicImage::ImageLuma8(ramp.to_luma8()),
        DynamicImage::ImageLumaA8(ramp.to_luma_alpha8()),
        DynamicImage::ImageRgb8(ramp.to_rgb8()),
        DynamicImage::ImageLuma16(ramp.to_luma16()),
        DynamicImage::ImageLumaA16(ramp.to_luma_alpha16()),
        DynamicImage::ImageRgb16(ramp.to_rgb16()),
        DynamicImage::ImageRgba16(ramp.to_rgba16()),
        DynamicImage::ImageRgb32F(ramp.to_rgb32f()),
        DynamicImage::ImageRgba32F(ramp.to_rgba32f()),
        ramp,
    ];
    for mut probe in probes {
        // A malformed profile fails the parent's parse the same way.
        if bigimage_core::color::normalize_srgb(&mut probe, icc).is_err() {
            return;
        }
    }
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
            let (image, format, source) = backend::decode_for_resave_in_process(&path)?;
            let mut extras = bound_resave_metadata(source);
            // Marker parsing only, and only of the file just decoded here.
            extras.quality = (format == Some(Format::Jpeg))
                .then(|| bigimage_core::metadata::estimate_jpeg_quality(&path))
                .flatten();
            let mut output = std::io::stdout().lock();
            send_resave(&mut output, &image, format, &extras)?;
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
        "attest" => attest_sandbox(&path)?,
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

    /// A raw-protocol header claiming `size`, `natural` and `length`.
    fn header(
        color: ColorType,
        size: (u32, u32),
        natural: (u32, u32),
        length: u64,
    ) -> [u8; HEADER] {
        let mut header = [0u8; HEADER];
        header[..8].copy_from_slice(MAGIC);
        header[8] = color_code(color).unwrap();
        header[9] = 255;
        for (offset, value) in [(12, size.0), (16, size.1), (20, natural.0), (24, natural.1)] {
            header[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        header[28..].copy_from_slice(&length.to_le_bytes());
        header
    }

    /// A worker answer: `header`, then `body` bytes left sparse.
    fn answer(header: &[u8; HEADER], body: u64) -> std::fs::File {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(header).unwrap();
        file.set_len(HEADER as u64 + body).unwrap();
        file.rewind().unwrap();
        file
    }

    /// Each row breaks one field of a valid answer, sized so that only the
    /// check under test can refuse it: without that check the row decodes.
    #[test]
    fn parent_refuses_each_malformed_pixel_answer() {
        use ColorType::{L8, Rgb8, Rgba32F};
        let valid = header(Rgb8, (2, 3), (2, 3), 18);
        let (pixels, _) = receive_with(answer(&valid, 18), false).ok().unwrap();
        assert_eq!(
            (pixels.image.dimensions(), pixels.natural, pixels.format),
            ((2, 3), (2, 3), None)
        );
        let with = |offset: usize, byte: u8| {
            let mut changed = valid;
            changed[offset] = byte;
            changed
        };
        // 16385 x 16384 pixels is one row past the 256 Mi pixel cap.
        let over = 16385 * 16384;
        let size = "invalid protocol dimensions or size";
        let rows = [
            (with(0, b'X'), 18, "invalid codec protocol"),
            (with(10, 2), 18, "invalid codec protocol"),
            (with(11, 2), 18, "invalid codec protocol"),
            (with(8, 0), 18, "invalid pixel type"),
            (with(8, COLOR_CODES.len() as u8 + 1), 18, "invalid pixel type"),
            (with(9, Format::all().len() as u8), 18, "invalid format"),
            (header(Rgb8, (0, 3), (2, 3), 0), 0, size),
            (header(Rgb8, (2, 0), (2, 3), 0), 0, size),
            (header(Rgb8, (2, 3), (0, 3), 18), 18, size),
            (header(Rgb8, (2, 3), (2, 0), 18), 18, size),
            (header(Rgb8, (2, 3), (16385, 16384), 18), 18, size),
            (header(L8, (16385, 16384), (2, 3), over), over, size),
            // 16 bytes per pixel: past MAX_BYTES while under the pixel cap.
            (header(Rgba32F, (8193, 4096), (2, 3), 16 * 8193 * 4096), 16 * 8193 * 4096, size),
            (header(Rgb8, (2, 3), (2, 3), 19), 19, size),
            (valid, 19, size),
            (valid, 17, size),
        ];
        for (index, (header, body, expected)) in rows.into_iter().enumerate() {
            let error = receive_with(answer(&header, body), false).err().unwrap().to_string();
            assert!(error.contains(expected), "row {index}: {error}");
        }
        // The re-save trailer must be whole before its lengths are trusted.
        let error = receive_with(answer(&valid, 18 + RESAVE_TRAILER as u64 - 1), true);
        assert!(error.err().unwrap().to_string().contains(size));
    }

    #[test]
    fn parent_refuses_malformed_dimensions_and_oversized_json() {
        let file = |bytes: &[u8]| {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(bytes).unwrap();
            file.rewind().unwrap();
            file
        };
        let size = |w: u32, h: u32| [w.to_le_bytes(), h.to_le_bytes()].concat();
        assert_eq!(read_dimensions(file(&size(16384, 16384))).unwrap(), (16384, 16384));
        for bytes in [size(16385, 16384), size(0, 1), size(1, 0), [size(1, 1), vec![0]].concat()] {
            assert!(read_dimensions(file(&bytes)).is_err(), "{bytes:?}");
        }
        let json =
            |length: u64| file(format!("\"{}\"", "a".repeat(length as usize - 2)).as_bytes());
        assert_eq!(read_json::<String>(json(MAX_METADATA)).unwrap().len() as u64, MAX_METADATA - 2);
        let error = read_json::<String>(json(MAX_METADATA + 1)).unwrap_err().to_string();
        assert!(error.contains("excessive metadata"), "{error}");
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
        let text = sanitize_text("ok\x1b]0;pwned\x07\r\u{202E}line\u{2066}a\u{2069}\nnext\u{9b}2J");
        assert!(!text.chars().any(|c| c.is_control() && c != '\n'), "{text:?}");
        assert!(!text.contains(['\u{202E}', '\u{2066}', '\u{2069}']), "{text:?}");
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
        assert!(message.contains("protocol") && message.ends_with(&restart_hint()), "{message}");
        // A worker that already explains the upgrade is not hinted twice.
        let Some(result) = with_worker("exit 86", DEADLINE, || {
            TEST_EXECUTABLE.with(|slot| slot.borrow_mut().as_mut().unwrap().2 = true);
            decode(file.path())
        }) else {
            return;
        };
        let message = result.unwrap_err().to_string();
        assert!(message.contains("restart") && !message.ends_with(&restart_hint()), "{message}");
    }

    #[test]
    fn hostile_worker_diagnostics_are_bounded_and_sanitized() {
        let file = input();
        let script = "i=0; while [ $i -lt 2000 ]; do printf '\\033]0;owned\\007\\033[2J%0500d' 0 >&2; \
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
        let extras = ResaveExtras { source: source.clone(), ..ResaveExtras::default() };
        send_resave(&mut payload, image, Some(Format::Jpeg), &extras).unwrap();
        payload
    }

    fn receive_bytes(bytes: &[u8], trailer: bool) -> ImageResult<bigimage_core::SourceMetadata> {
        receive_extras(bytes, trailer).map(|extras| extras.source)
    }

    fn receive_extras(bytes: &[u8], trailer: bool) -> ImageResult<ResaveExtras> {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        file.rewind().unwrap();
        receive_with(file, trailer).map(|(_, extras)| extras)
    }

    #[test]
    fn resave_protocol_round_trips_pixels_and_metadata() {
        let image = DynamicImage::ImageRgb16(ImageBuffer::from_pixel(3, 2, image::Rgb([1, 2, 3])));
        let source = bigimage_core::SourceMetadata {
            exif: Some(camera_exif()),
            icc: Some(vec![7; 300]),
            ..Default::default()
        };
        let payload = resave_payload(&image, &source);
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&payload).unwrap();
        file.rewind().unwrap();
        let (pixels, received) = receive_with(file, true).unwrap();
        assert_eq!(pixels.image, image);
        assert_eq!(pixels.format, Some(Format::Jpeg));
        assert_eq!(received, ResaveExtras { source: source.clone(), ..ResaveExtras::default() });
        // Empty metadata stays empty, and the plain protocol rejects a trailer.
        let bare = resave_payload(&image, &bigimage_core::SourceMetadata::default());
        assert_eq!(receive_bytes(&bare, true).unwrap(), Default::default());
        assert!(receive_bytes(&payload, false).is_err());
        // The estimated quality travels in the trailer; out-of-range
        // values are never sent.
        for (sent, expected) in [(Some(92), Some(92)), (Some(0), None), (Some(101), None)] {
            let mut bytes = Vec::new();
            let extras =
                ResaveExtras { source: source.clone(), quality: sent, ..Default::default() };
            send_resave(&mut bytes, &image, Some(Format::Jpeg), &extras).unwrap();
            let extras = receive_extras(&bytes, true).unwrap();
            assert_eq!(extras, ResaveExtras { quality: expected, ..extras_for(&source) });
        }
        // So do the blobs the worker dropped, as the losses a save reports.
        let dropped = ResaveExtras { exif_dropped: true, icc_dropped: true, ..Default::default() };
        let mut bytes = Vec::new();
        send_resave(&mut bytes, &image, None, &dropped).unwrap();
        let losses = receive_extras(&bytes, true).unwrap().losses();
        use bigimage_core::MetadataLoss::{ExifTooLarge, IccDiscarded};
        assert_eq!(losses, [ExifTooLarge, IccDiscarded]);
        // An EXIF item the worker could not read reaches the save.
        let unreadable =
            bigimage_core::SourceMetadata { exif_unreadable: true, ..Default::default() };
        let mut bytes = Vec::new();
        send_resave(&mut bytes, &image, None, &extras_for(&unreadable)).unwrap();
        assert_eq!(receive_extras(&bytes, true).unwrap(), extras_for(&unreadable));
    }

    fn extras_for(source: &bigimage_core::SourceMetadata) -> ResaveExtras {
        ResaveExtras { source: source.clone(), ..ResaveExtras::default() }
    }

    #[test]
    fn resave_protocol_rejects_hostile_trailers() {
        let image = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(2, 2, image::Rgb([9, 9, 9])));
        let source = bigimage_core::SourceMetadata {
            exif: Some(vec![1; 10]),
            icc: None,
            ..Default::default()
        };
        let payload = resave_payload(&image, &source);
        let trailer = HEADER + 12;
        // A forged quality byte outside 1..=100 is refused.
        let mut forged = payload.clone();
        forged[trailer + 16] = 101;
        let error = receive_bytes(&forged, true).unwrap_err().to_string();
        assert!(error.contains("quality"), "{error}");
        // So is a dropped-blob bit the protocol does not define.
        let mut forged = payload.clone();
        forged[trailer + 17] = 8;
        let error = receive_bytes(&forged, true).unwrap_err().to_string();
        assert!(error.contains("invalid codec protocol"), "{error}");
        // Truncated blob, extra bytes and a missing trailer.
        assert!(receive_bytes(&payload[..payload.len() - 1], true).is_err());
        assert!(receive_bytes(&[payload.as_slice(), &[0]].concat(), true).is_err());
        assert!(receive_bytes(&payload[..trailer], true).is_err());
        // Claimed blobs above the caps are refused before allocating them.
        for (offset, claim) in [(0, MAX_RESAVE_BLOB + 1), (8, MAX_RESAVE_BLOB + 1), (0, u64::MAX)] {
            let mut forged = payload.clone();
            forged[trailer + offset..trailer + offset + 8].copy_from_slice(&claim.to_le_bytes());
            let error = receive_bytes(&forged, true).unwrap_err().to_string();
            assert!(error.contains("excessive metadata"), "{error}");
        }
    }

    /// Core keeps EXIF and ICC blobs up to its cap: the worker hands those
    /// over whole and reports a larger one as a save would.
    #[test]
    fn resave_metadata_is_kept_up_to_cores_cap() {
        let cap = MAX_RESAVE_BLOB as usize;
        let at_cap = bigimage_core::SourceMetadata {
            exif: Some(vec![1; cap]),
            icc: Some(vec![0; 64]),
            ..Default::default()
        };
        let extras = bound_resave_metadata(at_cap.clone());
        assert_eq!(extras, extras_for(&at_cap));
        let image = DynamicImage::new_rgb8(1, 1);
        assert_eq!(receive_extras(&resave_payload(&image, &at_cap), true).unwrap(), extras);

        // A profile past the cap cannot be parsed to convert the pixels;
        // dropping it must not fail the save.
        let past_cap = bigimage_core::SourceMetadata {
            exif: Some(vec![1; cap + 1]),
            icc: Some(vec![0; cap + 1]),
            ..Default::default()
        };
        let extras = bound_resave_metadata(past_cap);
        assert_eq!(extras.source, bigimage_core::SourceMetadata::default());
        use bigimage_core::MetadataLoss::{ExifTooLarge, IccDiscarded};
        assert_eq!(extras.losses(), [ExifTooLarge, IccDiscarded]);
    }

    #[test]
    fn preview_without_a_format_is_refused() {
        let mut payload = Vec::new();
        send_to(&mut payload, &DynamicImage::new_rgb8(2, 2), None, (2, 2), false).unwrap();
        let file = tempfile::Builder::new().suffix(".png").tempfile().unwrap();
        std::fs::write(file.path(), &payload).unwrap();
        let Some(result) =
            with_worker("exec /usr/bin/cat \"$4\"", DEADLINE, || preview(file.path()))
        else {
            return;
        };
        let error = result.err().unwrap().to_string();
        assert!(error.contains("preview without a format"), "{error}");
    }

    /// The input checks run before the sandbox: a FIFO would block the
    /// worker and a file past the cap is never handed over.
    #[test]
    fn only_regular_files_up_to_the_cap_reach_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe.png");
        rustix::fs::mknodat(rustix::fs::CWD, &fifo, rustix::fs::FileType::Fifo, 0o600.into(), 0)
            .unwrap();
        let at_cap = dir.path().join("cap.png");
        std::fs::File::create(&at_cap).unwrap().set_len(MAX_INPUT).unwrap();
        let past_cap = dir.path().join("past.png");
        std::fs::File::create(&past_cap).unwrap().set_len(MAX_INPUT + 1).unwrap();
        let script = "echo started >&2; exit 0";
        let Some(results) = with_worker(script, DEADLINE, || {
            [dir.path(), &fifo, &past_cap, &at_cap].map(ensure_static)
        }) else {
            return;
        };
        let [directory, fifo, past_cap, at_cap] = results;
        for refused in [directory, fifo, past_cap] {
            let error = refused.unwrap_err().to_string();
            assert!(error.contains("not a regular file"), "{error}");
        }
        at_cap.unwrap();
    }

    /// Control bytes are the worst case: JSON writes each as six.
    #[test]
    fn largest_metadata_answer_fits_the_cap() {
        let tag = (format!("\u{1}{}", "\u{10FFFF}".repeat(100)), "\u{1f}".repeat(10_000));
        let metadata = Metadata { tags: vec![tag; 300], ..Metadata::default() };
        let answer = Information {
            dimensions: Some((u32::MAX, u32::MAX)),
            color: Some(u8::MAX),
            format: Some("OpenExr".into()),
            metadata: Some(trim_metadata(metadata)),
        };
        let mut file = tempfile::tempfile().unwrap();
        serde_json::to_writer(&mut file, &answer).unwrap();
        assert!(file.metadata().unwrap().len() <= MAX_METADATA);
        file.rewind().unwrap();
        let read: Information = read_json(file).unwrap();
        let tags = read.metadata.unwrap().tags;
        assert_eq!(tags.len(), MAX_TAGS);
        // Cut on a character boundary: 1 + 31 four-byte characters.
        assert_eq!((tags[0].0.len(), tags[0].1.len()), (125, MAX_TAG_VALUE));
    }

    /// The fake worker replays a forged answer stored as the input file.
    #[cfg(feature = "gui")]
    #[test]
    fn worker_metadata_is_sanitized_and_bounded() {
        // Each tag one GTK row: a worker that skips trim_metadata could send
        // tens of thousands within the JSON cap.
        let mut tags =
            vec![("Make\u{202E}".into(), format!("a\u{2066}b\x1b[2J{}", "x".repeat(9000)))];
        tags.extend((0..2000).map(|_| ("n".to_owned(), "v".to_owned())));
        let forged = Metadata { tags, ..Metadata::default() };
        let information = Information {
            dimensions: Some((1, 1)),
            color: None,
            format: Some("Png\u{2069}".into()),
            metadata: Some(forged.clone()),
        };
        let dir = tempfile::tempdir().unwrap();
        let (meta_file, info_file) = (dir.path().join("meta.png"), dir.path().join("info.png"));
        std::fs::write(&meta_file, serde_json::to_vec(&forged).unwrap()).unwrap();
        std::fs::write(&info_file, serde_json::to_vec(&information).unwrap()).unwrap();
        let Some((meta, info)) = with_worker("exec /usr/bin/cat \"$4\"", DEADLINE, || {
            (metadata(&meta_file), run(&info_file, "information", read_information))
        }) else {
            return;
        };
        let clean = |text: &str| !text.contains(['\u{202E}', '\u{2066}', '\u{2069}', '\x1b']);
        let info = info.ok().unwrap();
        assert!(clean(info.format.as_deref().unwrap()));
        for tags in [meta.unwrap().tags, info.metadata.unwrap().tags] {
            assert!(tags.iter().all(|(name, value)| clean(name) && clean(value)));
            assert_eq!((tags.len(), tags[0].1.len()), (MAX_TAGS, MAX_TAG_VALUE));
        }
    }

    #[test]
    fn hostile_resave_worker_is_rejected_through_the_sandbox() {
        // The fake worker replays a forged payload stored as the input file.
        let image = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(2, 2, image::Rgb([9, 9, 9])));
        let mut payload = resave_payload(&image, &bigimage_core::SourceMetadata::default());
        let trailer = HEADER + 12;
        payload[trailer..trailer + 8].copy_from_slice(&(MAX_RESAVE_BLOB + 1).to_le_bytes());
        let file = tempfile::Builder::new().suffix(".png").tempfile().unwrap();
        std::fs::write(file.path(), &payload).unwrap();
        let Some(result) =
            with_worker("exec /usr/bin/cat \"$4\"", DEADLINE, || resave(file.path()))
        else {
            return;
        };
        assert!(result.unwrap_err().to_string().contains("excessive metadata"));
    }

    #[test]
    fn sandboxed_resave_keeps_exif_and_wide_gamut_profile() {
        use bigimage_core::{EncodeOptions, OverwritePolicy};
        use image::{ImageDecoder, ImageEncoder};
        let binary = built_worker();
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
    fn sandboxed_resave_estimates_and_keeps_the_jpeg_quality() {
        use bigimage_core::{EncodeOptions, OverwritePolicy};
        let binary = built_worker();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.jpg");
        let png = dir.path().join("photo.png");
        let stored =
            image::RgbImage::from_fn(32, 24, |x, y| image::Rgb([x as u8 * 7, y as u8 * 9, 90]));
        let file = std::fs::File::create(&path).unwrap();
        image::codecs::jpeg::JpegEncoder::new_with_quality(file, 92).encode_image(&stored).unwrap();
        stored.save(&png).unwrap();

        let Some(result) = with_executable(binary.clone(), DEADLINE, || resave_with_extras(&path))
        else {
            return;
        };
        let (pixels, ResaveExtras { source, quality, .. }) = result.ok().unwrap();
        assert_eq!(pixels.format, Some(Format::Jpeg));
        let estimate = quality.expect("a JPEG source reports its quality");
        assert!(estimate.abs_diff(92) <= 2, "estimated {estimate}");
        // What the viewer does on Ctrl+S: re-encode with the estimate, so the
        // file keeps its quality instead of the perceptual target's.
        bigimage_core::save_image_preserving_metadata_report(
            pixels.image.fliph(),
            &path,
            Format::Jpeg,
            &EncodeOptions { quality, ..EncodeOptions::default() },
            OverwritePolicy::Replace,
            &source,
        )
        .unwrap();
        let resaved = bigimage_core::metadata::estimate_jpeg_quality(&path).unwrap();
        assert!(resaved.abs_diff(92) <= 2, "resaved at {resaved}");
        // Non-JPEG sources carry no estimate.
        let png = with_executable(binary, DEADLINE, || resave_with_extras(&png)).unwrap();
        assert_eq!(png.ok().unwrap().1.quality, None);
    }

    #[test]
    fn sandboxed_decode_survives_inherited_descriptors() {
        let binary = built_worker();
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

    /// A host descriptor without close-on-exec, numbered 100 or above so it
    /// never collides with the input the sandbox receives as descriptor 3.
    fn leaked_descriptor() -> std::os::fd::OwnedFd {
        let file = tempfile::tempfile().unwrap();
        let leaked = rustix::io::fcntl_dupfd_cloexec(&file, 100).unwrap();
        rustix::io::fcntl_setfd(&leaked, rustix::io::FdFlags::empty()).unwrap();
        leaked
    }

    #[test]
    fn host_descriptors_never_reach_the_sandbox() {
        let leaked = leaked_descriptor();
        let number = leaked.as_raw_fd();
        let script =
            format!("[ -e /proc/self/fd/{number} ] && {{ echo leaked >&2; exit 1; }}; exit 0");
        let file = input();
        let Some(result) = with_worker(&script, DEADLINE, || ensure_static(file.path())) else {
            return;
        };
        result.unwrap();
    }

    fn run_worker(args: &[&std::ffi::OsStr]) -> std::process::Output {
        Command::new(built_worker()).arg("--codec-worker").args(args).output().unwrap()
    }

    #[test]
    fn worker_refuses_inherited_descriptors() {
        let _leaked = leaked_descriptor();
        let file = input();
        let output = run_worker(&[WORKER_ID.as_ref(), "pixels".as_ref(), file.path().as_os_str()]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(stderr.contains("inherited file descriptors"), "{stderr}");
        assert!(output.stdout.is_empty());
    }

    #[test]
    fn worker_refuses_another_build_and_extra_arguments() {
        let file = input();
        let path = file.path().as_os_str();
        let other = run_worker(&["BIGIRIS2+other".as_ref(), "pixels".as_ref(), path]);
        assert_eq!(other.status.code(), Some(VERSION_MISMATCH));
        let short = run_worker(&[WORKER_ID.as_ref(), "pixels".as_ref()]);
        assert!(String::from_utf8_lossy(&short.stderr).contains("invalid worker arguments"));
        let long = run_worker(&[WORKER_ID.as_ref(), "pixels".as_ref(), path, path]);
        assert!(String::from_utf8_lossy(&long.stderr).contains("invalid worker arguments"));
        assert!(other.stdout.is_empty() && short.stdout.is_empty() && long.stdout.is_empty());
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

    /// `--clearenv` and `--cap-drop ALL` have no row: `env_clear` and bwrap's
    /// own capability drop for a user that is not root hide their removal.
    #[test]
    fn attest_refuses_each_weakened_limit() {
        let binary = built_worker();
        let file = input();
        let attest_with = |edit: Option<(Vec<String>, Vec<String>)>| {
            TEST_SANDBOX_EDIT.with(|slot| *slot.borrow_mut() = edit);
            let result = with_executable(binary.clone(), DEADLINE, || attest(file.path()));
            TEST_SANDBOX_EDIT.with(|slot| *slot.borrow_mut() = None);
            result
        };
        let Some(result) = attest_with(None) else { return };
        result.unwrap();
        let strings = |tokens: &[&str]| tokens.iter().map(|token| token.to_string()).collect();
        let row = |from: &[&str], to: &[&str], expected: &str| {
            (strings(from), strings(to), expected.to_owned())
        };
        let fsize = format!("--fsize={}", output_limit("attest"));
        let app = binary.to_str().unwrap();
        let mut weakened = vec![
            row(&["--as=2147483648"], &[], "Max address space"),
            row(&["--cpu=30"], &[], "Max cpu time"),
            row(&["--nofile=64"], &[], "Max open files"),
            row(&["--core=0"], &[], "Max core file size"),
            row(&[&fsize], &[], "Max file size"),
            row(&["--new-session"], &[], "session"),
            row(&["--die-with-parent"], &[], "parent death signal"),
            row(&["--remount-ro", "/dev"], &[], "/dev is writable"),
            row(&["--remount-ro", "/"], &[], "/ is writable"),
            row(&["--ro-bind", "/usr"], &["--bind", "/usr"], "/usr is writable"),
            row(&["--ro-bind", app], &["--bind", app], "/app/bigiris is writable"),
            row(&["--ro-bind-fd", "3"], &["--bind-fd", "3"], "/input.png is writable"),
            row(&["--size", "67108864"], &[], "/tmp is not limited"),
            row(&["--chdir"], &["--setenv", "HOME", "/", "--chdir"], "unexpected environment"),
            row(
                &["--dir", "/app"],
                &["--ro-bind", "/etc", "/etc", "--dir", "/app"],
                "host directory visible",
            ),
        ];
        // `--unshare-all` with one namespace left out.
        let all = ["user-try", "ipc", "pid", "net", "uts", "cgroup-try"];
        for (left_out, expected) in [
            ("ipc", "ipc namespace"),
            ("pid", "pid namespace"),
            ("net", "host network visible"),
            ("uts", "uts namespace"),
            ("cgroup-try", "cgroup namespace"),
        ] {
            let kept: Vec<String> = all
                .iter()
                .filter(|kind| **kind != left_out)
                .map(|kind| format!("--unshare-{kind}"))
                .collect();
            weakened.push((strings(&["--unshare-all"]), kept, expected.to_owned()));
        }
        if userns_hardening() {
            weakened.push(row(&["--disable-userns"], &[], "userns=allowed"));
        }
        for (from, to, expected) in weakened {
            let error = attest_with(Some((from.clone(), to))).unwrap().unwrap_err().to_string();
            assert!(error.contains(&expected), "without {from:?}: {error}");
        }
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
        assert!(result.unwrap_err().to_string().contains("time limit"));
        assert!(started.elapsed() < Duration::from_secs(10));

        let flag = Arc::new(AtomicBool::new(false));
        let trigger = flag.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            trigger.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let result = with_worker(script, DEADLINE, || with_cancel(flag, || decode(file.path())));
        assert!(result.unwrap().unwrap_err().to_string().contains("canceled"));
        assert!(started.elapsed() < Duration::from_secs(10));
        // --die-with-parent tears the sandbox down with bwrap.
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_running("30.31337") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!process_running("30.31337"), "sandboxed worker outlived its parent");
    }
}
