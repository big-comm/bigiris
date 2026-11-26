// SPDX-License-Identifier: GPL-3.0-or-later
//! Nemo (Cinnamon) — `.nemo_action` files. Nemo doesn't support nested
//! submenus, so each action ends up flat with a `BigIris ▸ <submenu> ▸
//! <label>` prefix to preserve some sense of hierarchy.

use std::io;
use std::path::PathBuf;

use crate::action::{ACTIONS, Action, TOP_LEVEL_LABEL};
use crate::safe_fs::FileMode;
use crate::scope::{Installed, ScopePaths, remove_generated, replace_generated};

const SUFFIX: &str = ".nemo_action";

/// Install one `.nemo_action` per [`Action`], removing those of actions a
/// release dropped.
pub fn install(paths: &ScopePaths) -> io::Result<Installed> {
    let files =
        ACTIONS.iter().map(|action| (format!("bigiris-{}{SUFFIX}", action.id), render(action)));
    replace_generated(
        &paths.nemo_actions(),
        SUFFIX,
        files.collect(),
        paths.scope,
        FileMode::Exact(0o644),
    )
}

/// Remove every file we own in `~/.local/share/nemo/actions/`.
pub fn uninstall(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    remove_generated(&paths.nemo_actions(), SUFFIX, &[])
}

fn render(action: &Action) -> String {
    let label = flat_label(action);
    let mime_line: String = action.mime_types.iter().map(|m| format!("{m};")).collect();
    format!(
        "[Nemo Action]\n\
         X-BigIris-Generated=true\n\
         Active=true\n\
         Name={label}\n{localized_name}\
         Comment=BigIris — {comment}\n{localized_comment}\
         Exec={exec}\n\
         Icon-Name={icon}\n\
         Selection=notnone\n\
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
        let written = install(&paths).unwrap().files;
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
        assert!(text.contains("Name=BigIris ▸ Convert ▸ PNG"));
        assert!(text.contains("Mimetypes=image/*;"));
    }

    #[test]
    fn render_filters_by_mime_only() {
        for action in ACTIONS {
            let text = render(action);
            assert!(
                !text.contains("Extensions="),
                "{}: Extensions= overrides Mimetypes",
                action.id
            );
            assert!(text.contains("Selection=notnone\n"), "{}", action.id);
        }
    }

    #[test]
    fn render_top_level_actions_suffix_owner() {
        let a = ACTIONS.iter().find(|a| a.id == "view").unwrap();
        let text = render(a);
        assert!(text.contains("Name=View in BigIris (BigIris)"));
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
