// SPDX-License-Identifier: GPL-3.0-or-later
//! elementary Files — Contractor `.contract` files in
//! `~/.local/share/contractor/`. Flat list, same "Íris ▸ …" convention as
//! Nemo/PCManFM-Qt.

use std::io;
use std::path::PathBuf;

use crate::action::{Action, ACTIONS};
use crate::nemo::flat_label;
use crate::safe_fs::safe_write;
use crate::scope::{remove_matching, ScopePaths};

/// Install one `.contract` per [`Action`].
pub fn install(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    let dir = paths.contractor();
    std::fs::create_dir_all(&dir)?;
    let mut written = Vec::new();
    for action in ACTIONS {
        let file = dir.join(format!("bigiris-{}.contract", action.id));
        safe_write(&file, render(action))?;
        written.push(file);
    }
    Ok(written)
}

/// Remove every file we own in `~/.local/share/contractor/`.
pub fn uninstall(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    remove_matching(&paths.contractor(), "bigiris-", ".contract")
}

fn render(action: &Action) -> String {
    let label = flat_label(action);
    let mime_line: String = action.mime_types.iter().map(|m| format!("{m};")).collect();
    format!(
        "[Contractor Entry]\n\
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
        assert!(text.contains("Name=BigIris ▸ Espelhar ▸ Horizontal"));
        assert!(text.contains("MimeType=image/*;"));
        assert!(text.contains("Exec=bigiris flip --axis horizontal"));
    }
}
