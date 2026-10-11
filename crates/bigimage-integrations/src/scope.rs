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

/// Remove every file in `dir` whose name starts with `prefix` and ends with
/// `suffix`. Non-matching files are left alone; a missing `dir` returns an
/// empty list rather than an error, so uninstall is idempotent.
pub(crate) fn remove_matching(
    dir: &Path,
    prefix: &str,
    suffix: &str,
) -> std::io::Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    if !dir.exists() {
        return Ok(removed);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(prefix) && name.ends_with(suffix) {
            let path = entry.path();
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
    fn is_on_path_finds_sh() {
        assert!(is_on_path("sh"), "sh deveria estar sempre no PATH em um linux");
    }

    #[test]
    fn is_on_path_rejects_bogus_binary() {
        assert!(!is_on_path("bigiris-nonexistent-binary-probe-zzz"));
    }
}
