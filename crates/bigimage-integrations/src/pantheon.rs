// SPDX-License-Identifier: GPL-3.0-or-later
//! elementary Files — Contractor `.contract` files in
//! `~/.local/share/contractor/`. Flat list, same "BigIris ▸ …" convention
//! as Nemo.

use std::io;
use std::path::PathBuf;

use crate::action::{ACTIONS, Action};
use crate::nemo::flat_label;
use crate::safe_fs::FileMode;
use crate::scope::{Installed, ScopePaths, remove_generated, replace_generated};

const SUFFIX: &str = ".contract";

/// Install one `.contract` per [`Action`], removing those of actions a
/// release dropped.
///
/// # Errors
///
/// An I/O error when the contractor folder cannot be listed or created, or a
/// contract file cannot be written or a stale one removed.
pub fn install(paths: &ScopePaths) -> io::Result<Installed> {
    let files =
        ACTIONS.iter().map(|action| (format!("bigiris-{}{SUFFIX}", action.id), render(action)));
    replace_generated(
        &paths.contractor(),
        SUFFIX,
        files.collect(),
        paths.scope,
        FileMode::Exact(0o644),
    )
}

/// Remove every file we own in `~/.local/share/contractor/`.
///
/// # Errors
///
/// An I/O error when the contractor folder cannot be listed or a file we own
/// cannot be removed.
pub fn uninstall(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    remove_generated(&paths.contractor(), SUFFIX, &[])
}

fn render(action: &Action) -> String {
    let label = flat_label(action);
    let mime_line: String = action.mime_types.iter().map(|m| format!("{m};")).collect();
    format!(
        "[Contractor Entry]\n\
         X-BigIris-Generated=true\n\
         Name={label}\n{localized_name}\
         Icon={icon}\n\
         Description=BigIris — {comment}\n{localized_comment}\
         MimeType={mime_line}\n\
         Exec={exec}\n",
        localized_name =
            crate::i18n::localized("Name", |lang| crate::i18n::flat_label(lang, action)),
        icon = action.icon,
        comment = action.label,
        localized_comment = crate::i18n::localized("Description", |lang| format!(
            "BigIris — {}",
            crate::i18n::lookup(lang, action.label)
        )),
        exec = action.command,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn install_and_uninstall_round_trip() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        install(&paths).unwrap();
        let removed = uninstall(&paths).unwrap();
        assert_eq!(removed.len(), ACTIONS.len());
    }

    #[test]
    fn rendered_contract_shape() {
        let a = ACTIONS.iter().find(|a| a.id == "flip-horizontal").unwrap();
        let text = render(a);
        assert!(text.starts_with("[Contractor Entry]\n"));
        assert!(text.contains("Name=BigIris ▸ Flip ▸ Horizontal"));
        assert!(text.contains("MimeType=image/*;"));
        assert!(text.contains("Exec=bigiris --notify flip --axis horizontal"));
    }
}
