// SPDX-License-Identifier: GPL-3.0-or-later
//! Nemo (Cinnamon) — `.nemo_action` files. Nemo doesn't support nested
//! submenus, so each action ends up flat with a `Íris ▸ <submenu> ▸ <label>`
//! prefix to preserve some sense of hierarchy.

use std::io;
use std::path::PathBuf;

use crate::action::{Action, ACTIONS, TOP_LEVEL_LABEL};
use crate::safe_fs::safe_write;
use crate::scope::{remove_matching, ScopePaths};

/// Install one `.nemo_action` per [`Action`].
pub fn install(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    let dir = paths.nemo_actions();
    std::fs::create_dir_all(&dir)?;
    let mut written = Vec::new();
    for action in ACTIONS {
        let file = dir.join(format!("bigiris-{}.nemo_action", action.id));
        safe_write(&file, render(action))?;
        written.push(file);
    }
    Ok(written)
}

/// Remove every file we own in `~/.local/share/nemo/actions/`.
pub fn uninstall(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    remove_matching(&paths.nemo_actions(), "bigiris-", ".nemo_action")
}

fn render(action: &Action) -> String {
    let label = flat_label(action);
    let mime_line: String = action.mime_types.iter().map(|m| format!("{m};")).collect();
    format!(
        "[Nemo Action]\n\
         Active=true\n\
         Name={label}\n{localized_name}\
         Comment=BigIris — {comment}\n{localized_comment}\
         Exec={exec}\n\
         Icon-Name={icon}\n\
         Selection=any\n\
         Extensions=any;\n\
         Mimetypes={mime_line}\n",
        label = label,
        comment = action.label,
        localized_comment = crate::i18n::localized("Comment", |lang| format!(
            "BigIris — {}",
            crate::i18n::lookup(lang, action.label)
        )),
        exec = action.command,
        localized_name =
            crate::i18n::localized("Name", |lang| crate::i18n::flat_label(lang, action)),
        icon = action.icon,
    )
}

pub(crate) fn flat_label(action: &Action) -> String {
    match action.submenu {
        Some(sub) => format!("{TOP_LEVEL_LABEL} ▸ {} ▸ {}", sub.label, action.label),
        None => format!("{} ({TOP_LEVEL_LABEL})", action.label),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn install_writes_expected_files() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let written = install(&paths).unwrap();
        assert_eq!(written.len(), ACTIONS.len());
        for path in &written {
            assert!(path.exists());
            assert!(path.file_name().unwrap().to_string_lossy().ends_with(".nemo_action"));
        }
    }

    #[test]
    fn render_sub_actions_carry_hierarchy_prefix() {
        let a = ACTIONS.iter().find(|a| a.id == "convert-png").unwrap();
        let text = render(a);
        assert!(text.contains("Name=BigIris ▸ Converter ▸ PNG"));
        assert!(text.contains("Mimetypes=image/*;"));
    }

    #[test]
    fn render_top_level_actions_suffix_owner() {
        let a = ACTIONS.iter().find(|a| a.id == "view").unwrap();
        let text = render(a);
        assert!(text.contains("Name=Visualizar em BigIris (BigIris)"));
    }

    #[test]
    fn install_then_uninstall_round_trip() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        install(&paths).unwrap();
        let removed = uninstall(&paths).unwrap();
        assert_eq!(removed.len(), ACTIONS.len());
        assert!(std::fs::read_dir(paths.nemo_actions()).unwrap().next().is_none());
    }
}
