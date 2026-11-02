// SPDX-License-Identifier: GPL-3.0-or-later
//! Install scope — maps `User` / `System` to filesystem roots so every
//! file-manager generator agrees on where things live.

use std::path::{Path, PathBuf};

use directories::BaseDirs;
use thiserror::Error;

/// Errors surfaced by scope resolution.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ScopeError {
    /// Could not resolve the user's home directory (unusual on Linux).
    #[error("não foi possível localizar $HOME")]
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
                })
            }
            Scope::System => Ok(Self {
                scope,
                home: PathBuf::from("/"),
                data: PathBuf::from("/usr/share"),
                config: PathBuf::from("/etc/xdg"),
            }),
        }
    }

    /// Build a paths record rooted at `home` — used by tests to install into
    /// a tempdir without touching the developer's real `$HOME`.
    pub fn rooted_at(home: impl Into<PathBuf>) -> Self {
        let home: PathBuf = home.into();
        let data = home.join(".local/share");
        let config = home.join(".config");
        Self { scope: Scope::User, home, data, config }
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

    /// `~/.local/share/nautilus/scripts/` — GNOME Nautilus script tree
    /// (fallback when `nautilus-python` isn't installed).
    pub fn nautilus_scripts(&self) -> PathBuf {
        self.data.join("nautilus/scripts")
    }

    /// `~/.local/share/nautilus-python/extensions/` — home of Python
    /// extensions that add top-level right-click entries (required for
    /// "Íris ▸" to appear outside the Scripts submenu).
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

    /// `~/.local/share/icons/hicolor/scalable/apps/` — vetor de apps.
    pub fn app_icon_dir(&self) -> PathBuf {
        self.data.join("icons/hicolor/scalable/apps")
    }

    /// `~/.local/share/metainfo/` — AppStream metainfo XML.
    pub fn metainfo_dir(&self) -> PathBuf {
        self.data.join("metainfo")
    }
}

/// Key-file line every generator writes into the first group of the
/// files it creates (Dolphin, Nemo, PCManFM-Qt, Contractor). Uninstall
/// deletes only files carrying it, so a user's own `bigiris-foo.desktop`
/// survives even though it matches our naming scheme.
pub(crate) const GENERATED_MARKER: &str = "X-BigIris-Generated=true";

/// Upper bound on how much of a candidate file we read to identify it.
/// Our generated files are a few KiB even with 29 locales.
const MAX_IDENTIFY_BYTES: u64 = 256 * 1024;

/// Is `body` a file our generators wrote? Current files carry
/// [`GENERATED_MARKER`]; files from releases before the marker are still
/// recognized because every action they contained launched `bigiris`
/// directly (`Exec=bigiris …`).
pub(crate) fn is_generated_by_us(body: &str) -> bool {
    body.lines().map(str::trim).any(|line| {
        line == GENERATED_MARKER || line == "Exec=bigiris" || line.starts_with("Exec=bigiris ")
    })
}

/// Remove every regular file in `dir` whose name starts with `prefix`,
/// ends with `suffix` **and** whose content [`is_generated_by_us`].
/// Non-matching files (and symlinks) are left alone; a missing `dir`
/// returns an empty list rather than an error, so uninstall is idempotent.
pub(crate) fn remove_matching(
    dir: &Path,
    prefix: &str,
    suffix: &str,
) -> std::io::Result<Vec<PathBuf>> {
    use std::io::Read;

    let mut removed = Vec::new();
    if !dir.exists() {
        return Ok(removed);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with(prefix) && name.ends_with(suffix)) {
            continue;
        }
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let mut body = String::new();
        let readable = std::fs::File::open(&path)
            .and_then(|f| f.take(MAX_IDENTIFY_BYTES).read_to_string(&mut body))
            .is_ok();
        if readable && is_generated_by_us(&body) {
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
    fn remove_matching_requires_marker_or_legacy_bigiris_exec() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let marked = dir.join("bigiris-new.desktop");
        let legacy = dir.join("bigiris-old.desktop");
        let users = dir.join("bigiris-mine.desktop");
        let other_cmd = dir.join("bigiris-wrapper.desktop");
        let foreign = dir.join("someone-else.desktop");
        std::fs::write(&marked, format!("[Desktop Entry]\n{GENERATED_MARKER}\nExec=x\n")).unwrap();
        std::fs::write(&legacy, "[Desktop Entry]\nExec=bigiris convert --to png %F\n").unwrap();
        std::fs::write(&users, "[Desktop Entry]\nName=My own\nExec=gimp %F\n").unwrap();
        std::fs::write(&other_cmd, "[Desktop Entry]\nExec=bigiris-helper %F\n").unwrap();
        std::fs::write(&foreign, format!("{GENERATED_MARKER}\n")).unwrap();

        let mut removed = remove_matching(dir, "bigiris-", ".desktop").unwrap();
        removed.sort();
        let mut expected = vec![legacy.clone(), marked.clone()];
        expected.sort();
        assert_eq!(removed, expected);
        assert!(users.exists(), "user's own bigiris-* file was deleted");
        assert!(other_cmd.exists());
        assert!(foreign.exists(), "prefix filter ignored");
    }

    #[test]
    fn remove_matching_skips_symlinks_and_directories() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("menus");
        std::fs::create_dir_all(dir.join("bigiris-dir.desktop")).unwrap();
        let target = tmp.path().join("target.desktop");
        std::fs::write(&target, format!("{GENERATED_MARKER}\n")).unwrap();
        std::os::unix::fs::symlink(&target, dir.join("bigiris-link.desktop")).unwrap();
        assert!(remove_matching(&dir, "bigiris-", ".desktop").unwrap().is_empty());
        assert!(target.exists());
        assert!(dir.join("bigiris-dir.desktop").is_dir());
    }

    #[test]
    fn is_on_path_finds_sh() {
        assert!(is_on_path("sh"), "sh deveria estar sempre no PATH em um linux");
    }

    #[test]
    fn is_on_path_rejects_bogus_binary() {
        assert!(!is_on_path("bigiris-nonexistent-binary-probe-zzz"));
    }
}
