// SPDX-License-Identifier: GPL-3.0-or-later
//! PCManFM-Qt / libfm — `.desktop` files with `Type=Action` in
//! `~/.local/share/file-manager/actions/`. No runtime submenu support, so
//! actions land flat, same convention as Nemo.

use std::io;
use std::path::PathBuf;

use crate::action::{Action, ACTIONS};
use crate::nemo::flat_label;
use crate::safe_fs::safe_write;
use crate::scope::{remove_matching, ScopePaths};

/// Install one `.desktop` per [`Action`].
pub fn install(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    let dir = paths.libfm_actions();
    std::fs::create_dir_all(&dir)?;
    let mut written = Vec::new();
    for action in ACTIONS {
        let file = dir.join(format!("bigiris-{}.desktop", action.id));
        safe_write(&file, render(action))?;
        written.push(file);
    }
    Ok(written)
}

/// Remove every file we own in `~/.local/share/file-manager/actions/`.
pub fn uninstall(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    remove_matching(&paths.libfm_actions(), "bigiris-", ".desktop")
}

fn render(action: &Action) -> String {
    let label = flat_label(action);
    let mime_line: String = action.mime_types.iter().map(|m| format!("{m};")).collect();
    let profile_id = format!("bigiris-{}", action.id);

    format!(
        "[Desktop Entry]\n\
         Type=Action\n\
         Name={label}\n{localized_name}\
         Icon={icon}\n\
         Profiles={profile_id};\n\
         \n\
         [X-Action-Profile {profile_id}]\n\
         Exec={exec}\n\
         MimeTypes={mime_line}\n",
        localized_name =
            crate::i18n::localized("Name", |lang| crate::i18n::flat_label(lang, action)),
        icon = action.icon,
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
        let written = install(&paths).unwrap();
        assert_eq!(written.len(), ACTIONS.len());
        let removed = uninstall(&paths).unwrap();
        assert_eq!(removed.len(), ACTIONS.len());
    }

    #[test]
    fn rendered_file_has_required_sections() {
        let a = ACTIONS.iter().find(|a| a.id == "rotate-90").unwrap();
        let text = render(a);
        assert!(text.contains("Type=Action"));
        assert!(text.contains("[X-Action-Profile bigiris-rotate-90]"));
        assert!(text.contains("Exec=bigiris rotate --degrees 90"));
        assert!(text.contains("MimeTypes=image/*;"));
    }
}
