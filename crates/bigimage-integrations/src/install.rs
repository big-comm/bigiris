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

fn run_all(paths: &ScopePaths, op: InstallOp, detected_only: bool) -> Report {
    let mut report = Report::default();
    report.outcomes.push(run_one(
        FileManager::Dolphin,
        "dolphin",
        paths,
        op,
        detected_only,
        dolphin::install,
        dolphin::uninstall,
    ));
    report.outcomes.push(run_one(
        FileManager::Nautilus,
        "nautilus",
        paths,
        op,
        detected_only,
        nautilus::install,
        nautilus::uninstall,
    ));
    report.outcomes.push(run_one(
        FileManager::Nemo,
        "nemo",
        paths,
        op,
        detected_only,
        nemo::install,
        nemo::uninstall,
    ));
    report.outcomes.push(run_one(
        FileManager::Thunar,
        "thunar",
        paths,
        op,
        detected_only,
        thunar::install,
        thunar::uninstall,
    ));
    report.outcomes.push(run_one(
        FileManager::PcmanfmQt,
        "pcmanfm-qt",
        paths,
        op,
        detected_only,
        pcmanfm_qt::install,
        pcmanfm_qt::uninstall,
    ));
    report.outcomes.push(run_one(
        FileManager::Pantheon,
        "io.elementary.files",
        paths,
        op,
        detected_only,
        pantheon::install,
        pantheon::uninstall,
    ));
    report
}

fn run_one(
    fm: FileManager,
    detection_bin: &str,
    paths: &ScopePaths,
    op: InstallOp,
    detected_only: bool,
    install_fn: fn(&ScopePaths) -> io::Result<Vec<PathBuf>>,
    uninstall_fn: fn(&ScopePaths) -> io::Result<Vec<PathBuf>>,
) -> FmOutcome {
    let detected = is_on_path(detection_bin);
    if matches!(op, InstallOp::Install) && detected_only && !detected {
        return FmOutcome { fm, detected, files: Vec::new(), error: None };
    }
    let result = match op {
        InstallOp::Install => install_fn(paths),
        InstallOp::Uninstall => uninstall_fn(paths),
    };
    match result {
        Ok(files) => FmOutcome { fm, detected, files, error: None },
        Err(e) => FmOutcome { fm, detected, files: Vec::new(), error: Some(e.to_string()) },
    }
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

/// Escreve `.desktop`, ícone SVG e metainfo para os paths XDG do usuário.
/// Falhas individuais não interrompem — a função retorna apenas os que
/// foram gravados com sucesso. Diretórios ausentes são criados.
fn install_app_files(paths: &ScopePaths) -> Vec<PathBuf> {
    let mut written = Vec::new();
    for (dir, name, content) in [
        (paths.applications_dir(), APP_DESKTOP_NAME, APP_DESKTOP),
        (paths.app_icon_dir(), APP_ICON_NAME, APP_ICON_SVG),
        (paths.metainfo_dir(), APP_METAINFO_NAME, APP_METAINFO),
    ] {
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let target = dir.join(name);
        if crate::safe_fs::safe_write(&target, content).is_ok() {
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
/// ignorados (idempotente).
fn uninstall_app_files(paths: &ScopePaths) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    for path in [
        paths.applications_dir().join(APP_DESKTOP_NAME),
        paths.app_icon_dir().join(APP_ICON_NAME),
        paths.metainfo_dir().join(APP_METAINFO_NAME),
    ] {
        if path.exists() && std::fs::remove_file(&path).is_ok() {
            removed.push(path);
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
        // Thunar's uca.xml goes under DESTDIR/etc/xdg/Thunar/.
        assert!(tmp.path().join("etc/xdg/Thunar/uca.xml").exists());
        assert!(tmp.path().join("usr/share/nautilus-python/extensions/bigiris-menu.py").exists());
    }
}
