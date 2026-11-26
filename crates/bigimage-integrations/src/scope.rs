// SPDX-License-Identifier: GPL-3.0-or-later
//! Install scope — maps `User` / `System` to filesystem roots so every
//! file-manager generator agrees on where things live.

use std::io;
use std::path::{Path, PathBuf};

use directories::BaseDirs;
use thiserror::Error;

use crate::safe_fs::{FileMode, safe_write_with_mode};

/// Errors surfaced by scope resolution.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ScopeError {
    /// Could not resolve the user's home directory (unusual on Linux).
    #[error("could not locate $HOME")]
    NoHome,
}

/// Install scope: per-user (default) or system-wide (packaging).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `~/.local/share/...` and `~/.config/...`. No root required.
    User,
    /// `/usr/share/...` and `/etc/...` — requires root and is normally done
    /// by the distro package, not by the user.
    System,
}

/// Concrete, pre-resolved paths each generator writes to.
///
/// Built once at install time so the generators don't each re-query the
/// XDG dirs (which can vary between tests, CI, and a real desktop session).
#[derive(Debug, Clone)]
pub struct ScopePaths {
    /// Explicit scope; DESTDIR retains System even when paths share a prefix.
    pub scope: Scope,
    /// Base `$HOME` (or pseudo-home in tests). System installations use `/` or the staging root.
    pub home: PathBuf,
    /// `~/.local/share` / `/usr/share`.
    pub data: PathBuf,
    /// `~/.config` / `/etc/xdg`.
    pub config: PathBuf,
    /// System configuration dirs (`$XDG_CONFIG_DIRS`, default `/etc/xdg`)
    /// that Thunar falls back to when the user has no `uca.xml`. Only set
    /// for a real user install; empty means "none" (tests, system scope).
    pub xdg_config_dirs: Vec<PathBuf>,
    /// Where the distro package installs system-wide integrations
    /// (`/usr/share`). A user install skips every file manager the package
    /// already covers there, since a personal copy would shadow the packaged
    /// one and go stale on upgrade. `None` for tests and system scope.
    pub package_data: Option<PathBuf>,
}

impl ScopePaths {
    /// Resolve real paths for the given scope.
    pub fn resolve(scope: Scope) -> Result<Self, ScopeError> {
        match scope {
            Scope::User => {
                let base = BaseDirs::new().ok_or(ScopeError::NoHome)?;
                Ok(Self {
                    scope,
                    home: base.home_dir().to_path_buf(),
                    data: base.data_dir().to_path_buf(),
                    config: base.config_dir().to_path_buf(),
                    xdg_config_dirs: config_dirs_from(std::env::var_os("XDG_CONFIG_DIRS")),
                    package_data: Some(PathBuf::from("/usr/share")),
                })
            }
            Scope::System => Ok(Self {
                scope,
                home: PathBuf::from("/"),
                data: PathBuf::from("/usr/share"),
                config: PathBuf::from("/etc/xdg"),
                xdg_config_dirs: Vec::new(),
                package_data: None,
            }),
        }
    }

    /// Build a paths record rooted at `home` — used by tests to install into
    /// a tempdir without touching the developer's real `$HOME`.
    pub fn rooted_at(home: impl Into<PathBuf>) -> Self {
        let home: PathBuf = home.into();
        let data = home.join(".local/share");
        let config = home.join(".config");
        Self {
            scope: Scope::User,
            home,
            data,
            config,
            xdg_config_dirs: Vec::new(),
            package_data: None,
        }
    }

    /// System-wide scope prefixed with a `DESTDIR` for packaging. Ensures
    /// `makepkg` / `dpkg-buildpackage` stage files under `$pkgdir` instead
    /// of touching the live system.
    pub fn system_with_destdir(destdir: impl Into<PathBuf>) -> Self {
        let destdir: PathBuf = destdir.into();
        Self {
            scope: Scope::System,
            home: destdir.clone(),
            data: destdir.join("usr/share"),
            config: destdir.join("etc/xdg"),
            xdg_config_dirs: Vec::new(),
            package_data: None,
        }
    }

    /// `~/.local/share/kio/servicemenus/` — Dolphin & Konqueror.
    pub fn kio_servicemenus(&self) -> PathBuf {
        self.data.join("kio/servicemenus")
    }

    /// `~/.local/share/nemo/actions/` — Cinnamon's Nemo.
    pub fn nemo_actions(&self) -> PathBuf {
        self.data.join("nemo/actions")
    }

    /// `~/.local/share/file-manager/actions/` — libfm (PCManFM-Qt).
    pub fn libfm_actions(&self) -> PathBuf {
        self.data.join("file-manager/actions")
    }

    /// `~/.local/share/contractor/` — elementary Files.
    pub fn contractor(&self) -> PathBuf {
        self.data.join("contractor")
    }

    /// `~/.local/share/nautilus/scripts/`: GNOME Nautilus script tree, where
    /// releases before the extension wrote; never written, only cleaned.
    pub fn nautilus_scripts(&self) -> PathBuf {
        self.data.join("nautilus/scripts")
    }

    /// `~/.local/share/nautilus-python/extensions/` — home of Python
    /// extensions that add top-level right-click entries (required for
    /// "BigIris ▸" to appear outside the Scripts submenu).
    pub fn nautilus_python_extensions(&self) -> PathBuf {
        self.data.join("nautilus-python/extensions")
    }

    /// `~/.config/Thunar/uca.xml` — XFCE Thunar custom actions. Unlike the
    /// other managers, Thunar stores everything in a single file that we
    /// must merge into rather than overwrite.
    pub fn thunar_uca(&self) -> PathBuf {
        self.config.join("Thunar/uca.xml")
    }

    /// `~/.local/share/applications/` — XDG desktop entries.
    pub fn applications_dir(&self) -> PathBuf {
        self.data.join("applications")
    }

    /// `~/.local/share/icons/hicolor/scalable/apps/`: scalable app icons.
    pub fn app_icon_dir(&self) -> PathBuf {
        self.data.join("icons/hicolor/scalable/apps")
    }

    /// `~/.local/share/metainfo/` — AppStream metainfo XML.
    pub fn metainfo_dir(&self) -> PathBuf {
        self.data.join("metainfo")
    }
}

/// Parse `$XDG_CONFIG_DIRS`. The spec ignores relative entries, and an
/// unset or empty value means the default `/etc/xdg`.
fn config_dirs_from(value: Option<std::ffi::OsString>) -> Vec<PathBuf> {
    let dirs: Vec<PathBuf> = value
        .map(|v| std::env::split_paths(&v).filter(|p| p.is_absolute()).collect())
        .unwrap_or_default();
    if dirs.is_empty() { vec![PathBuf::from("/etc/xdg")] } else { dirs }
}

/// Key-file line every generator writes into the first group of the
/// files it creates (Dolphin, Nemo, PCManFM-Qt, Contractor). Uninstall
/// deletes only files carrying it, so a user's own `bigiris-foo.desktop`
/// survives even though it matches our naming scheme.
pub(crate) const GENERATED_MARKER: &str = "X-BigIris-Generated=true";

/// Start of every file name the key-file generators write.
const FILE_PREFIX: &str = "bigiris-";

/// Ids of every action released before [`GENERATED_MARKER`] existed. Those
/// releases wrote one unmarked `bigiris-<id><suffix>` file per id; the list
/// is frozen because every later file carries the marker.
const PRE_MARKER_ACTION_IDS: &[&str] = &[
    "adjust-brightness-minus",
    "adjust-brightness-plus",
    "adjust-bw",
    "adjust-contrast-plus",
    "adjust-custom",
    "adjust-vivid",
    "ai-remove-bg",
    "batch-folder",
    "convert-avif",
    "convert-custom",
    "convert-jpg",
    "convert-png",
    "convert-tiff",
    "convert-webp",
    "flip-custom",
    "flip-horizontal",
    "flip-vertical",
    "metadata-strip-all",
    "metadata-view",
    "pdf-convert",
    "resize-200pct",
    "resize-25pct",
    "resize-4k-3840",
    "resize-50pct",
    "resize-custom",
    "resize-hd-1920",
    "rotate-180",
    "rotate-270",
    "rotate-90",
    "rotate-auto-exif",
    "rotate-custom",
    "utils-animate-gif",
    "utils-batch",
    "utils-compare",
    "view",
    "web-discord",
    "web-facebook",
    "web-instagram",
    "web-letterbox-custom",
    "web-letterbox-ig",
    "web-letterbox-stories",
    "web-letterbox-yt",
    "web-optimize-png",
    "web-telegram",
    "web-twitter",
    "web-whatsapp",
];

/// Upper bound on how much of a candidate file we read to identify it.
/// Our generated files are a few KiB even with 30 locales.
const MAX_IDENTIFY_BYTES: u64 = 256 * 1024;

/// Did our generators write the file `name` holding `body`? It carries
/// [`GENERATED_MARKER`], or it has the exact name a release before the
/// marker gave it and launches `bigiris` (`Exec=bigiris …`) as all of
/// those did. A user's own `bigiris-*` file is never ours by content alone.
fn is_generated_by_us(name: &str, suffix: &str, body: &str) -> bool {
    let mut lines = body.lines().map(str::trim);
    if lines.clone().any(|line| line == GENERATED_MARKER) {
        return true;
    }
    let pre_marker_name = name
        .strip_prefix(FILE_PREFIX)
        .and_then(|rest| rest.strip_suffix(suffix))
        .is_some_and(|id| PRE_MARKER_ACTION_IDS.contains(&id));
    pre_marker_name && lines.any(|line| line == "Exec=bigiris" || line.starts_with("Exec=bigiris "))
}

/// Write each `(file name, content)` of `files` into `dir`, then remove the
/// generated files there that `files` does not name, such as the menu entry
/// of a dropped or renamed action.
pub(crate) fn replace_generated(
    dir: &Path,
    suffix: &str,
    files: Vec<(String, String)>,
    scope: Scope,
    mode: FileMode,
) -> io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let mut written = Vec::with_capacity(files.len());
    for (name, content) in files {
        let path = dir.join(name);
        safe_write_with_mode(&path, content, scope, mode)?;
        written.push(path);
    }
    remove_generated(dir, suffix, &written)?;
    Ok(written)
}

/// Remove every regular `bigiris-*<suffix>` file in `dir` that
/// [`is_generated_by_us`], except those in `keep`. Other files and symlinks
/// are left alone; a missing `dir` removes nothing, so uninstall is
/// idempotent.
pub(crate) fn remove_generated(
    dir: &Path,
    suffix: &str,
    keep: &[PathBuf],
) -> io::Result<Vec<PathBuf>> {
    use std::io::Read;

    let mut removed = Vec::new();
    if !dir.exists() {
        return Ok(removed);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with(FILE_PREFIX) && name.ends_with(suffix)) {
            continue;
        }
        let path = entry.path();
        if !entry.file_type()?.is_file() || keep.contains(&path) {
            continue;
        }
        let mut body = String::new();
        let readable = std::fs::File::open(&path)
            .and_then(|f| f.take(MAX_IDENTIFY_BYTES).read_to_string(&mut body))
            .is_ok();
        if readable && is_generated_by_us(&name, suffix, &body) {
            std::fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    Ok(removed)
}

/// Is this binary available on the user's PATH?
pub fn is_on_path(bin: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    path.split(':').any(|dir| {
        let candidate = Path::new(dir).join(bin);
        candidate.is_file()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn rooted_at_follows_xdg_layout() {
        let tmp = TempDir::new().unwrap();
        let p = ScopePaths::rooted_at(tmp.path());
        assert_eq!(p.kio_servicemenus(), tmp.path().join(".local/share/kio/servicemenus"));
        assert_eq!(p.nemo_actions(), tmp.path().join(".local/share/nemo/actions"));
        assert_eq!(p.libfm_actions(), tmp.path().join(".local/share/file-manager/actions"));
        assert_eq!(p.contractor(), tmp.path().join(".local/share/contractor"));
        assert_eq!(p.nautilus_scripts(), tmp.path().join(".local/share/nautilus/scripts"));
        assert_eq!(
            p.nautilus_python_extensions(),
            tmp.path().join(".local/share/nautilus-python/extensions")
        );
        assert_eq!(p.thunar_uca(), tmp.path().join(".config/Thunar/uca.xml"));
    }

    #[test]
    fn removal_needs_marker_or_legacy_name() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let write = |name: &str, body: &str| {
            let path = dir.join(name);
            std::fs::write(&path, body).unwrap();
            path
        };
        let marked =
            write("bigiris-new.desktop", &format!("[Desktop Entry]\n{GENERATED_MARKER}\n"));
        let pre_marker =
            write("bigiris-convert-png.desktop", "[Desktop Entry]\nExec=bigiris convert %F\n");
        let kept = [
            // A user's own menu calling bigiris, under a name we never used.
            write("bigiris-my-preset.desktop", "[Desktop Entry]\nExec=bigiris --dialog=batch %F\n"),
            write("bigiris-view.desktop", "[Desktop Entry]\nName=My own\nExec=gimp %F\n"),
            write("bigiris-rotate-90.desktop", "[Desktop Entry]\nExec=bigiris-helper %F\n"),
            write("bigiris-view.nemo_action", "[Nemo Action]\nExec=bigiris %F\n"),
            write("someone-else.desktop", &format!("{GENERATED_MARKER}\n")),
        ];

        let mut removed = remove_generated(dir, ".desktop", &[]).unwrap();
        removed.sort();
        assert_eq!(removed, [pre_marker, marked]);
        for path in kept {
            assert!(path.exists(), "{} was deleted", path.display());
        }
    }

    #[test]
    fn remove_generated_skips_symlinks_and_directories() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("menus");
        std::fs::create_dir_all(dir.join("bigiris-dir.desktop")).unwrap();
        let target = tmp.path().join("target.desktop");
        std::fs::write(&target, format!("{GENERATED_MARKER}\n")).unwrap();
        std::os::unix::fs::symlink(&target, dir.join("bigiris-link.desktop")).unwrap();
        assert!(remove_generated(&dir, ".desktop", &[]).unwrap().is_empty());
        assert!(target.exists());
        assert!(dir.join("bigiris-dir.desktop").is_dir());
    }

    #[test]
    fn replace_drops_removed_actions() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let stale = dir.join("bigiris-old.nemo_action");
        std::fs::write(&stale, format!("[Nemo Action]\n{GENERATED_MARKER}\n")).unwrap();
        let body = format!("[Nemo Action]\n{GENERATED_MARKER}\n");
        let files = vec![("bigiris-new.nemo_action".to_owned(), body)];
        let written =
            replace_generated(dir, ".nemo_action", files, Scope::User, FileMode::Exact(0o644))
                .unwrap();
        assert_eq!(written, [dir.join("bigiris-new.nemo_action")]);
        assert!(written[0].exists());
        assert!(!stale.exists(), "stale action kept");
    }

    #[test]
    fn config_dirs_follow_the_xdg_spec() {
        let etc = vec![PathBuf::from("/etc/xdg")];
        assert_eq!(config_dirs_from(None), etc);
        assert_eq!(config_dirs_from(Some("".into())), etc);
        assert_eq!(config_dirs_from(Some("relative:also/rel".into())), etc);
        assert_eq!(
            config_dirs_from(Some("/opt/xdg::rel:/etc/xdg".into())),
            vec![PathBuf::from("/opt/xdg"), PathBuf::from("/etc/xdg")]
        );
    }

    #[test]
    fn is_on_path_finds_sh() {
        assert!(is_on_path("sh"), "sh should always be on PATH on Linux");
    }

    #[test]
    fn is_on_path_rejects_bogus_binary() {
        assert!(!is_on_path("bigiris-nonexistent-binary-probe-zzz"));
    }
}
