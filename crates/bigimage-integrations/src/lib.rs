// SPDX-License-Identifier: GPL-3.0-or-later
//! bigimage-integrations — installs/removes right-click actions in Linux file managers.
//!
//! Support matrix:
//! - Dolphin / Konqueror (`.desktop` with `X-KDE-Submenu` for nested menus)
//! - Nautilus (scripts tree under `~/.local/share/nautilus/scripts/BigIris/`)
//! - Nemo (`.nemo_action`, flattened with prefix)
//! - Thunar (merge into `~/.config/Thunar/uca.xml`)
//! - PCManFM-Qt / libfm (`.desktop` `Type=Action`)
//! - elementary Files (`.contract`)

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod action;
pub mod dolphin;
mod i18n;
pub mod install;
pub mod nautilus;
pub mod nemo;
pub mod pantheon;
pub mod pcmanfm_qt;
mod safe_fs;
pub mod scope;
pub mod thunar;

pub use action::{Action, Submenu, ACTIONS};
pub use install::{install, install_to_destdir, uninstall, FmOutcome, InstallError, Report};
pub use scope::{Scope, ScopeError, ScopePaths};

/// Crate version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// File managers we can integrate with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileManager {
    /// KDE Dolphin / Konqueror (shared ServiceMenu format).
    Dolphin,
    /// GNOME Nautilus (scripts tree).
    Nautilus,
    /// Cinnamon Nemo.
    Nemo,
    /// XFCE Thunar (uca.xml merge).
    Thunar,
    /// LXQt PCManFM-Qt (libfm).
    PcmanfmQt,
    /// elementary Files (Contractor).
    Pantheon,
}

impl FileManager {
    /// Human-readable label used in CLI reports.
    pub fn display_name(&self) -> &'static str {
        match self {
            FileManager::Dolphin => "Dolphin / Konqueror",
            FileManager::Nautilus => "Nautilus",
            FileManager::Nemo => "Nemo",
            FileManager::Thunar => "Thunar",
            FileManager::PcmanfmQt => "PCManFM-Qt",
            FileManager::Pantheon => "elementary Files",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!version().is_empty());
    }

    #[test]
    fn display_names_never_empty() {
        for fm in [
            FileManager::Dolphin,
            FileManager::Nautilus,
            FileManager::Nemo,
            FileManager::Thunar,
            FileManager::PcmanfmQt,
            FileManager::Pantheon,
        ] {
            assert!(!fm.display_name().is_empty());
        }
    }
}
