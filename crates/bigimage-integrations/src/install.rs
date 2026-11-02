// SPDX-License-Identifier: GPL-3.0-or-later
//! Top-level `install` / `uninstall` orchestrator. Runs every supported
//! file-manager generator in a fixed order, collects their results into a
//! [`Report`], and surfaces which file managers are actually present on
//! the user's system.

use std::io;
use std::path::PathBuf;

use crate::scope::{is_on_path, Scope, ScopeError, ScopePaths};
use crate::{dolphin, nautilus, nemo, pantheon, pcmanfm_qt, thunar, FileManager};

/// Outcome of installing/uninstalling one file manager.
#[derive(Debug, Clone)]
pub struct FmOutcome {
    /// Which file manager this row describes.
    pub fm: FileManager,
    /// Whether the file manager's executable was found on `$PATH`.
    pub detected: bool,
    /// Files touched (written on install, removed on uninstall).
    pub files: Vec<PathBuf>,
    /// Populated only on failure; the rest of the FMs still run.
    pub error: Option<String>,
    /// User install only: the distribution package already ships this
    /// integration system-wide, so nothing was written (a personal copy
    /// would shadow the packaged one and go stale on upgrades) and copies
    /// written by earlier user installs were removed.
    pub packaged: bool,
}

/// Aggregate report across all FMs.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// One row per attempted FM.
    pub outcomes: Vec<FmOutcome>,
    /// App-level files touched (desktop entry, icon, metainfo). Populated
    /// only for `Scope::User`; the `--system` packaging path leaves these
    /// to the PKGBUILD.
    pub app_files: Vec<PathBuf>,
}

impl Report {
    /// Has every attempted FM succeeded?
    pub fn is_success(&self) -> bool {
        self.outcomes.iter().all(|o| o.error.is_none())
    }

    /// Count of files touched across all FMs (plus app-level files).
    pub fn files_touched(&self) -> usize {
        self.outcomes.iter().map(|o| o.files.len()).sum::<usize>() + self.app_files.len()
    }
}

/// Combined error: scope resolution failed before any FM ran.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InstallError {
    /// Scope could not be resolved (usually `$HOME` missing).
    #[error(transparent)]
    Scope(#[from] ScopeError),
}

/// Install BigIris service menus for every supported file manager.
///
/// In `Scope::User` a file manager whose integration the distribution
/// package already ships under `/usr/share` is skipped and our stale
/// personal copies of it are removed, so on a packaged system a user
/// install only adds what packages cannot ship (Thunar's `uca.xml`).
/// For `Scope::User` also installs app launcher (`.desktop`), icon SVG,
/// and AppStream metainfo — so running the binary from a dev build plus
/// `install-integrations --user` gives the full "registered app"
/// experience without needing the PKGBUILD. For `Scope::System` the
/// PKGBUILD is expected to own those files, so we don't touch them.
pub fn install(scope: Scope) -> Result<Report, InstallError> {
    let paths = ScopePaths::resolve(scope)?;
    // A per-user install only writes integration files for file managers
    // actually present. Distribution packages still stage every supported
    // integration so users can install a manager later without reinstalling.
    let detected_only = matches!(scope, Scope::User);
    let mut report = run_all(&paths, InstallOp::Install, detected_only);
    if matches!(scope, Scope::User) {
        report.app_files = install_app_files(&paths);
        refresh_xdg_caches(&paths);
    }
    Ok(report)
}

/// Remove every BigIris service-menu file across every supported manager.
pub fn uninstall(scope: Scope) -> Result<Report, InstallError> {
    let paths = ScopePaths::resolve(scope)?;
    let mut report = run_all(&paths, InstallOp::Uninstall, false);
    if matches!(scope, Scope::User) {
        report.app_files = uninstall_app_files(&paths);
    }
    Ok(report)
}

/// System-wide install prefixed with `destdir` — used by distro packagers
/// so `makepkg` stages every service-menu file under `$pkgdir` rather than
/// touching the live `/usr/share` tree.
pub fn install_to_destdir(destdir: &std::path::Path) -> Report {
    let paths = ScopePaths::system_with_destdir(destdir);
    run_all(&paths, InstallOp::Install, false)
}

/// Same as [`install`] / [`uninstall`] but with pre-resolved paths —
/// used by tests to target a tempdir.
pub fn install_with_paths(paths: &ScopePaths) -> Report {
    run_all(paths, InstallOp::Install, false)
}

/// Same as [`install_with_paths`] but for the uninstall flow.
pub fn uninstall_with_paths(paths: &ScopePaths) -> Report {
    run_all(paths, InstallOp::Uninstall, false)
}

#[derive(Clone, Copy)]
enum InstallOp {
    Install,
    Uninstall,
}

/// A [`ScopePaths`] directory getter, such as [`ScopePaths::nemo_actions`].
type DirOf = fn(&ScopePaths) -> PathBuf;

/// One row of the orchestration table.
struct FmSpec {
    fm: FileManager,
    detection_bin: &'static str,
    install: fn(&ScopePaths) -> io::Result<Vec<PathBuf>>,
    uninstall: fn(&ScopePaths) -> io::Result<Vec<PathBuf>>,
    /// Directory (relative to a data root) and file suffix of the copies a
    /// distribution package installs. `None` for Thunar, which packages
    /// never ship (see `thunar` module docs).
    packaged: Option<(DirOf, &'static str)>,
}

const FILE_MANAGERS: [FmSpec; 6] = [
    FmSpec {
        fm: FileManager::Dolphin,
        detection_bin: "dolphin",
        install: dolphin::install,
        uninstall: dolphin::uninstall,
        packaged: Some((ScopePaths::kio_servicemenus, ".desktop")),
    },
    FmSpec {
        fm: FileManager::Nautilus,
        detection_bin: "nautilus",
        install: nautilus::install,
        uninstall: nautilus::uninstall,
        packaged: Some((ScopePaths::nautilus_python_extensions, ".py")),
    },
    FmSpec {
        fm: FileManager::Nemo,
        detection_bin: "nemo",
        install: nemo::install,
        uninstall: nemo::uninstall,
        packaged: Some((ScopePaths::nemo_actions, ".nemo_action")),
    },
    FmSpec {
        fm: FileManager::Thunar,
        detection_bin: "thunar",
        install: thunar::install,
        uninstall: thunar::uninstall,
        packaged: None,
    },
    FmSpec {
        fm: FileManager::PcmanfmQt,
        detection_bin: "pcmanfm-qt",
        install: pcmanfm_qt::install,
        uninstall: pcmanfm_qt::uninstall,
        packaged: Some((ScopePaths::libfm_actions, ".desktop")),
    },
    FmSpec {
        fm: FileManager::Pantheon,
        detection_bin: "io.elementary.files",
        install: pantheon::install,
        uninstall: pantheon::uninstall,
        packaged: Some((ScopePaths::contractor, ".contract")),
    },
];

fn run_all(paths: &ScopePaths, op: InstallOp, detected_only: bool) -> Report {
    let outcomes =
        FILE_MANAGERS.iter().map(|spec| run_one(spec, paths, op, detected_only)).collect();
    Report { outcomes, app_files: Vec::new() }
}

fn run_one(spec: &FmSpec, paths: &ScopePaths, op: InstallOp, detected_only: bool) -> FmOutcome {
    let detected = is_on_path(spec.detection_bin);
    let outcome = |result: io::Result<Vec<PathBuf>>, packaged: bool| match result {
        Ok(files) => FmOutcome { fm: spec.fm, detected, files, error: None, packaged },
        Err(e) => FmOutcome {
            fm: spec.fm,
            detected,
            files: Vec::new(),
            error: Some(e.to_string()),
            packaged,
        },
    };
    if matches!(op, InstallOp::Install) && is_packaged(spec, paths) {
        // Drop what earlier user installs wrote; the package's copy wins.
        return outcome((spec.uninstall)(paths).map(|_| Vec::new()), true);
    }
    if matches!(op, InstallOp::Install) && detected_only && !detected {
        return outcome(Ok(Vec::new()), false);
    }
    let result = match op {
        InstallOp::Install => (spec.install)(paths),
        InstallOp::Uninstall => (spec.uninstall)(paths),
    };
    outcome(result, false)
}

/// Does a user install find this integration already shipped by the
/// distribution package under [`ScopePaths::package_data`]?
fn is_packaged(spec: &FmSpec, paths: &ScopePaths) -> bool {
    let (Scope::User, Some(data), Some((dir_of, suffix))) =
        (paths.scope, paths.package_data.as_ref(), spec.packaged)
    else {
        return false;
    };
    let system = ScopePaths { data: data.clone(), ..paths.clone() };
    let Ok(entries) = std::fs::read_dir(dir_of(&system)) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name.starts_with("bigiris-")
            && name.ends_with(suffix)
            && entry.file_type().is_ok_and(|t| t.is_file())
    })
}

// ─── App-level assets (desktop entry, icon, metainfo) ──────────────────
// Embutidos em compile-time para que o binário saiba se instalar sozinho
// em desktops onde o usuário rodou cargo build sem PKGBUILD. Os arquivos
// vivem no diretório `data/` do repo; paths relativos a este arquivo.

const APP_DESKTOP: &[u8] = include_bytes!("../../../data/com.biglinux.Iris.desktop");
const APP_METAINFO: &[u8] = include_bytes!("../../../data/com.biglinux.Iris.metainfo.xml");
const APP_ICON_SVG: &[u8] =
    include_bytes!("../../../data/icons/hicolor/scalable/apps/com.biglinux.Iris.svg");

const APP_DESKTOP_NAME: &str = "com.biglinux.Iris.desktop";
const APP_METAINFO_NAME: &str = "com.biglinux.Iris.metainfo.xml";
const APP_ICON_NAME: &str = "com.biglinux.Iris.svg";

/// Desktop entry shipped by the distro package. When it exists, the
/// package owns the launcher, icon and metainfo system-wide.
const SYSTEM_APP_DESKTOP: &str = "/usr/share/applications/com.biglinux.Iris.desktop";

/// Marker comment placed after the XML declaration of the copies we write,
/// so cleanup can tell them apart from files the user put there.
const XML_MARKER: &str = "<!-- X-BigIris-Generated: written by bigiris install-integrations -->";

#[derive(Clone, Copy)]
enum AssetKind {
    Desktop,
    Icon,
    Metainfo,
}

/// FNV-1a 64 and length of every icon BigIris released before the marker
/// existed and that differs from the current asset (the history of
/// `data/icons/hicolor/scalable/apps/com.biglinux.Iris.svg`: b82d47a).
/// The icon carries no textual id to recognize it by, so only these exact
/// files count as ours. FNV is not a security boundary here: the file sits
/// in the user's own home under our application id.
const LEGACY_ICONS: &[(u64, usize)] = &[(0x93fc_7d95_f6c0_5abf, 4560)];

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

fn app_assets(paths: &ScopePaths) -> [(PathBuf, &'static [u8], AssetKind); 3] {
    [
        (paths.applications_dir().join(APP_DESKTOP_NAME), APP_DESKTOP, AssetKind::Desktop),
        (paths.app_icon_dir().join(APP_ICON_NAME), APP_ICON_SVG, AssetKind::Icon),
        (paths.metainfo_dir().join(APP_METAINFO_NAME), APP_METAINFO, AssetKind::Metainfo),
    ]
}

/// Insert our marker after the first line (`[Desktop Entry]` / the XML
/// declaration), where it is valid for each format.
fn with_marker(content: &[u8], kind: AssetKind) -> Vec<u8> {
    let marker = match kind {
        AssetKind::Desktop => crate::scope::GENERATED_MARKER,
        AssetKind::Icon | AssetKind::Metainfo => XML_MARKER,
    };
    let split = content.iter().position(|&b| b == b'\n').map_or(content.len(), |i| i + 1);
    let mut out = Vec::with_capacity(content.len() + marker.len() + 2);
    out.extend_from_slice(&content[..split]);
    if !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(marker.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(&content[split..]);
    out
}

/// Did we write `body`? It carries our marker, or it is a copy written
/// before the marker existed (by `install.sh` or an older release), which
/// is recognized per format because those assets change every release:
///
/// - launcher: `Exec=bigiris …` plus `Icon=com.biglinux.Iris`, unless it
///   hides or disables the entry (`NoDisplay=`/`Hidden=`), which only a
///   user's menu-editor override does;
/// - metainfo: the AppStream component id `com.biglinux.Iris`;
/// - icon: a byte copy of the current or a previously released icon.
///
/// Anything else is left alone.
fn is_our_app_file(body: &[u8], original: &[u8], kind: AssetKind) -> bool {
    if body == original {
        return true;
    }
    let text = String::from_utf8_lossy(body);
    match kind {
        AssetKind::Desktop => {
            let lines: Vec<&str> = text.lines().map(str::trim).collect();
            let has = |pred: fn(&str) -> bool| lines.iter().any(|l| pred(l));
            has(|l| l == crate::scope::GENERATED_MARKER)
                || (has(|l| l == "Exec=bigiris" || l.starts_with("Exec=bigiris "))
                    && has(|l| l == "Icon=com.biglinux.Iris")
                    && !has(|l| l.starts_with("NoDisplay=") || l.starts_with("Hidden=")))
        }
        AssetKind::Metainfo => {
            text.contains(XML_MARKER) || text.contains("<id>com.biglinux.Iris</id>")
        }
        AssetKind::Icon => {
            text.contains(XML_MARKER) || LEGACY_ICONS.contains(&(fnv1a64(body), body.len()))
        }
    }
}

/// Remove `path` only if it is a regular file we wrote.
fn remove_if_ours(path: &std::path::Path, original: &[u8], kind: AssetKind) -> bool {
    let is_file = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file());
    is_file
        && std::fs::read(path).is_ok_and(|body| is_our_app_file(&body, original, kind))
        && std::fs::remove_file(path).is_ok()
}

/// Escreve `.desktop`, ícone SVG e metainfo para os paths XDG do usuário.
/// Falhas individuais não interrompem — a função retorna apenas os que
/// foram gravados com sucesso. Diretórios ausentes são criados.
///
/// When the distro package already installed the launcher system-wide,
/// a personal copy would shadow it and go stale on the next package
/// upgrade, so nothing is written and copies we wrote earlier are removed.
fn install_app_files(paths: &ScopePaths) -> Vec<PathBuf> {
    install_app_files_with_system(paths, std::path::Path::new(SYSTEM_APP_DESKTOP))
}

fn install_app_files_with_system(
    paths: &ScopePaths,
    system_desktop: &std::path::Path,
) -> Vec<PathBuf> {
    let mut written = Vec::new();
    if paths.scope == Scope::User && system_desktop.exists() {
        for (target, content, kind) in app_assets(paths) {
            remove_if_ours(&target, content, kind);
        }
        return written;
    }
    for (target, content, kind) in app_assets(paths) {
        let Some(dir) = target.parent() else { continue };
        if std::fs::create_dir_all(dir).is_err() {
            continue;
        }
        if crate::safe_fs::safe_write(&target, with_marker(content, kind), paths.scope).is_ok() {
            written.push(target);
        }
    }
    written
}

/// Melhor-esforço: atualiza caches XDG (desktop-database e icon cache)
/// no escopo do usuário. GNOME Shell lê o nome do `.desktop` direto, mas
/// busca e MIME-handler dependem desses caches. Se os binários não
/// existirem, seguimos em frente sem avisar.
fn refresh_xdg_caches(paths: &ScopePaths) {
    use std::process::{Command, Stdio};
    let apps = paths.applications_dir();
    let icon_dir = paths.app_icon_dir();
    let hicolor = icon_dir.parent().and_then(|p| p.parent()); // .../hicolor
    let _ = Command::new("update-desktop-database")
        .arg(&apps)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if let Some(hicolor) = hicolor {
        for bin in ["gtk4-update-icon-cache", "gtk-update-icon-cache"] {
            // `--ignore-theme-index` é obrigatório no escopo user: esta
            // função só roda para `Scope::User` (ver `install`), e
            // `~/.local/share/icons/hicolor` não tem `index.theme` próprio
            // (só o tema system em `/usr/share` tem). Sem essa flag o
            // comando recusa com "No theme index file" e — como o stderr é
            // descartado — o cache nunca é gerado, deixando o ícone fora da
            // busca do GNOME até um logout/login.
            let _ = Command::new(bin)
                .arg("-q")
                .arg("-f")
                .arg("--ignore-theme-index")
                .arg(hicolor)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    // Plasma/Dolphin discovers ServiceMenus through the KService cache.
    let _ = Command::new("kbuildsycoca6")
        .arg("--noincremental")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Remove os arquivos instalados por [`install_app_files`]. Não remove
/// diretórios — deixa isso para o sistema. Arquivos ausentes são
/// ignorados (idempotente). Files the user placed there themselves (for
/// example a menu-editor override of the launcher) are kept.
fn uninstall_app_files(paths: &ScopePaths) -> Vec<PathBuf> {
    app_assets(paths)
        .into_iter()
        .filter(|(path, content, kind)| remove_if_ours(path, content, *kind))
        .map(|(path, _, _)| path)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const NO_SYSTEM: &str = "/nonexistent/bigiris-test/com.biglinux.Iris.desktop";

    fn install_app_files(paths: &ScopePaths) -> Vec<PathBuf> {
        install_app_files_with_system(paths, std::path::Path::new(NO_SYSTEM))
    }

    #[test]
    fn user_install_skips_app_files_the_package_provides() {
        let tmp = TempDir::new().unwrap();
        let system = tmp.path().join("system.desktop");
        std::fs::write(&system, "[Desktop Entry]\n").unwrap();
        let paths = ScopePaths::rooted_at(tmp.path().join("home"));
        assert!(install_app_files_with_system(&paths, &system).is_empty());
        for (target, _, _) in app_assets(&paths) {
            assert!(!target.exists(), "{} shadows the packaged copy", target.display());
        }
    }

    #[test]
    fn user_install_removes_our_stale_copies_when_package_is_present() {
        let tmp = TempDir::new().unwrap();
        let system = tmp.path().join("system.desktop");
        std::fs::write(&system, "[Desktop Entry]\n").unwrap();
        let paths = ScopePaths::rooted_at(tmp.path().join("home"));
        // A copy written by an older install-integrations run (marker) …
        install_app_files(&paths);
        // … and a verbatim copy, as install.sh writes it.
        let icon = paths.app_icon_dir().join(APP_ICON_NAME);
        std::fs::write(&icon, APP_ICON_SVG).unwrap();
        install_app_files_with_system(&paths, &system);
        for (target, _, _) in app_assets(&paths) {
            assert!(!target.exists(), "stale {} kept", target.display());
        }
    }

    #[test]
    fn pre_marker_copies_from_older_releases_are_recognized() {
        let tmp = TempDir::new().unwrap();
        let system = tmp.path().join("system.desktop");
        std::fs::write(&system, "[Desktop Entry]\n").unwrap();
        let paths = ScopePaths::rooted_at(tmp.path().join("home"));
        let [(desktop, _, _), (icon, _, _), (metainfo, _, _)] = app_assets(&paths);
        for p in [&desktop, &icon, &metainfo] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        }
        // As the first release's install.sh wrote them (no marker).
        std::fs::write(
            &desktop,
            "[Desktop Entry]\nName=BigIris\nExec=bigiris %U\nIcon=com.biglinux.Iris\n",
        )
        .unwrap();
        std::fs::write(
            &metainfo,
            "<?xml version=\"1.0\"?>\n<component>\n  <id>com.biglinux.Iris</id>\n</component>\n",
        )
        .unwrap();
        // An unknown SVG under our name is not provably ours.
        std::fs::write(&icon, "<svg>someone else's com.biglinux.Iris</svg>").unwrap();

        install_app_files_with_system(&paths, &system);
        assert!(!desktop.exists() && !metainfo.exists(), "legacy copies kept");
        assert!(icon.exists(), "unknown icon removed");
    }

    #[test]
    fn launcher_overrides_and_foreign_files_are_not_ours() {
        let marker_less = |s: &str, kind| is_our_app_file(s.as_bytes(), b"", kind);
        let hidden = "[Desktop Entry]\nExec=bigiris %F\nIcon=com.biglinux.Iris\nNoDisplay=true\n";
        assert!(!marker_less(hidden, AssetKind::Desktop));
        let other_icon = "[Desktop Entry]\nExec=bigiris %F\nIcon=my-icon\n";
        assert!(!marker_less(other_icon, AssetKind::Desktop));
        assert!(!marker_less("<component><id>org.other</id></component>", AssetKind::Metainfo));
        // Reference FNV-1a 64 vector, so the legacy icon table stays valid.
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn user_customized_launcher_is_never_removed() {
        let tmp = TempDir::new().unwrap();
        let system = tmp.path().join("system.desktop");
        std::fs::write(&system, "[Desktop Entry]\n").unwrap();
        let paths = ScopePaths::rooted_at(tmp.path().join("home"));
        let desktop = paths.applications_dir().join(APP_DESKTOP_NAME);
        std::fs::create_dir_all(desktop.parent().unwrap()).unwrap();
        let custom = "[Desktop Entry]\nName=Iris\nExec=bigiris %F\nNoDisplay=true\n";
        std::fs::write(&desktop, custom).unwrap();
        install_app_files_with_system(&paths, &system);
        assert!(uninstall_app_files(&paths).is_empty());
        assert_eq!(std::fs::read_to_string(&desktop).unwrap(), custom);
    }

    #[test]
    fn written_app_files_carry_marker_and_stay_valid() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        install_app_files(&paths);
        let desktop =
            std::fs::read_to_string(paths.applications_dir().join(APP_DESKTOP_NAME)).unwrap();
        assert!(
            desktop.starts_with(&format!("[Desktop Entry]\n{}\n", crate::scope::GENERATED_MARKER))
        );
        for path in
            [paths.app_icon_dir().join(APP_ICON_NAME), paths.metainfo_dir().join(APP_METAINFO_NAME)]
        {
            let body = std::fs::read_to_string(&path).unwrap();
            assert!(body.starts_with("<?xml"), "XML declaration must stay first");
            assert_eq!(body.lines().nth(1), Some(XML_MARKER));
        }
    }

    #[test]
    fn user_install_skips_packaged_fms_and_removes_stale_copies() {
        let tmp = TempDir::new().unwrap();
        let package = tmp.path().join("root");
        assert!(install_to_destdir(&package).is_success());
        let mut paths = ScopePaths::rooted_at(tmp.path().join("home"));
        // A user install from before the package was installed …
        assert!(install_with_paths(&paths).is_success());
        let mine = paths.kio_servicemenus().join("bigiris-mine.desktop");
        std::fs::write(&mine, "[Desktop Entry]\nExec=gimp %F\n").unwrap();

        paths.package_data = Some(package.join("usr/share"));
        let report = install_with_paths(&paths);
        assert!(report.is_success());
        for o in &report.outcomes {
            let thunar = matches!(o.fm, FileManager::Thunar);
            assert_eq!(o.packaged, !thunar, "{:?}", o.fm);
            assert_eq!(o.files.is_empty(), !thunar, "{:?} wrote {:?}", o.fm, o.files);
        }
        for dir in [
            paths.nemo_actions(),
            paths.libfm_actions(),
            paths.contractor(),
            paths.nautilus_python_extensions(),
        ] {
            let left: Vec<_> = std::fs::read_dir(&dir).into_iter().flatten().flatten().collect();
            assert!(left.is_empty(), "stale user copies kept in {}", dir.display());
        }
        assert!(mine.exists(), "user's own file removed");
        assert!(paths.thunar_uca().exists());
    }

    #[test]
    fn install_writes_all_fms() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let report = install_with_paths(&paths);
        assert_eq!(report.outcomes.len(), 6);
        assert!(report.is_success());
        assert!(report.files_touched() > 0);
    }

    #[test]
    fn install_app_files_writes_desktop_icon_metainfo() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let written = install_app_files(&paths);
        assert_eq!(written.len(), 3);
        assert!(paths.applications_dir().join(APP_DESKTOP_NAME).exists());
        assert!(paths.app_icon_dir().join(APP_ICON_NAME).exists());
        assert!(paths.metainfo_dir().join(APP_METAINFO_NAME).exists());
    }

    #[test]
    fn uninstall_app_files_cleans_everything() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        install_app_files(&paths);
        let removed = uninstall_app_files(&paths);
        assert_eq!(removed.len(), 3);
        assert!(!paths.applications_dir().join(APP_DESKTOP_NAME).exists());
        assert!(!paths.app_icon_dir().join(APP_ICON_NAME).exists());
        assert!(!paths.metainfo_dir().join(APP_METAINFO_NAME).exists());
    }

    #[test]
    fn uninstall_cleans_everything_we_wrote() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let wrote = install_with_paths(&paths).files_touched();
        let removed = uninstall_with_paths(&paths).files_touched();
        assert_eq!(wrote, removed);
    }

    #[test]
    fn every_generated_key_file_carries_the_marker() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let report = install_with_paths(&paths);
        let key_files: Vec<_> = report
            .outcomes
            .iter()
            .filter(|o| !matches!(o.fm, FileManager::Thunar | FileManager::Nautilus))
            .flat_map(|o| o.files.iter())
            .collect();
        assert!(!key_files.is_empty());
        for file in key_files {
            let body = std::fs::read_to_string(file).unwrap();
            assert!(
                body.lines().nth(1) == Some(crate::scope::GENERATED_MARKER),
                "{} lacks the generated marker in its first group",
                file.display()
            );
        }
    }

    #[test]
    fn uninstall_keeps_user_files_that_only_share_our_name_prefix() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        install_with_paths(&paths);
        let mine = [
            paths.kio_servicemenus().join("bigiris-mine.desktop"),
            paths.nemo_actions().join("bigiris-mine.nemo_action"),
            paths.libfm_actions().join("bigiris-mine.desktop"),
            paths.contractor().join("bigiris-mine.contract"),
        ];
        for file in &mine {
            std::fs::write(file, "[Desktop Entry]\nName=Mine\nExec=gimp %F\n").unwrap();
        }
        assert!(uninstall_with_paths(&paths).is_success());
        for file in &mine {
            assert!(file.exists(), "{} was deleted", file.display());
        }
    }

    #[test]
    fn uninstall_on_fresh_home_is_noop_but_ok() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let report = uninstall_with_paths(&paths);
        assert!(report.is_success());
        assert_eq!(report.files_touched(), 0);
    }

    #[test]
    fn install_to_destdir_stages_under_usr_share() {
        let tmp = TempDir::new().unwrap();
        let report = install_to_destdir(tmp.path());
        assert!(report.is_success());
        // Dolphin .desktop files should sit under DESTDIR/usr/share/kio/servicemenus/.
        assert!(tmp.path().join("usr/share/kio/servicemenus/bigiris-convert-png.desktop").exists());
        // /etc/xdg/Thunar/uca.xml belongs to the thunar package; a distro
        // package shipping it would file-conflict with thunar.
        assert!(!tmp.path().join("etc/xdg/Thunar/uca.xml").exists());
        assert!(!tmp.path().join("etc").exists(), "packages must not stage anything in /etc");
        assert!(tmp.path().join("usr/share/nautilus-python/extensions/bigiris-menu.py").exists());
    }
}
