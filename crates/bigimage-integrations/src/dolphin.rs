// SPDX-License-Identifier: GPL-3.0-or-later
//! Dolphin / Konqueror service menus (`.desktop` files with
//! `ServiceTypes=KonqPopupMenu/Plugin`). Native nested submenus via
//! `X-KDE-Submenu=Íris/<submenu>`.

use std::io;
use std::path::PathBuf;

use crate::action::{Action, ACTIONS, TOP_LEVEL_LABEL};
use crate::safe_fs::safe_write_with_mode;
use crate::scope::{remove_matching, ScopePaths};

/// Install one `.desktop` per [`Action`] into `~/.local/share/kio/servicemenus/`.
pub fn install(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    let dir = paths.kio_servicemenus();
    std::fs::create_dir_all(&dir)?;
    let mut written = Vec::new();
    for action in ACTIONS {
        let file = dir.join(file_name(action));
        // Dolphin treats executable permission as the trust marker for
        // user-owned ServiceMenus and silently rejects a regular `0644`
        // desktop file. Re-applying the mode also repairs older installs.
        safe_write_with_mode(&file, render(action), 0o755)?;
        written.push(file);
    }
    Ok(written)
}

/// Remove every file we own in `~/.local/share/kio/servicemenus/`.
pub fn uninstall(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    remove_matching(&paths.kio_servicemenus(), "bigiris-", ".desktop")
}

fn file_name(action: &Action) -> String {
    format!("bigiris-{}.desktop", action.id)
}

fn render(action: &Action) -> String {
    let submenu_path = match action.submenu {
        Some(sub) => format!("{TOP_LEVEL_LABEL}/{}", sub.label),
        None => TOP_LEVEL_LABEL.to_string(),
    };
    let action_id = camel_action_id(action.id);
    let mime_line: String = action.mime_types.iter().map(|m| format!("{m};")).collect();

    format!(
        "[Desktop Entry]\n\
         Type=Service\n\
         ServiceTypes=KonqPopupMenu/Plugin\n\
         MimeType={mime_line}\n\
         X-KDE-Submenu={submenu_path}\n\
         X-KDE-Priority=TopLevel\n\
         Icon={icon}\n\
         Name={label}\n\
         Actions={action_id}\n\
         \n\
         [Desktop Action {action_id}]\n\
         Name={label}\n\
         Icon={icon}\n\
         Exec={exec}\n",
        icon = action.icon,
        label = action.label,
        exec = action.command,
    )
}

/// `convert-png` → `bigirisConvertPng`. KDE action IDs must be alphanumeric,
/// and `bigiris-` could clash with filesystem stems; the camelCase form
/// keeps the two namespaces visually separate.
fn camel_action_id(id: &str) -> String {
    let mut out = String::from("bigiris");
    let mut upcase_next = true;
    for c in id.chars() {
        if c == '-' {
            upcase_next = true;
        } else if upcase_next {
            out.push(c.to_ascii_uppercase());
            upcase_next = false;
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn install_writes_one_file_per_action() {
        #[cfg(target_os = "linux")]
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let written = install(&paths).unwrap();
        assert_eq!(written.len(), ACTIONS.len());
        for path in &written {
            assert!(path.exists());
            let name = path.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("bigiris-"));
            assert!(name.ends_with(".desktop"));
            #[cfg(target_os = "linux")]
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o755,
                "Dolphin rejects non-executable user ServiceMenus"
            );
        }
    }

    #[test]
    fn rendered_file_has_required_keys() {
        let action = ACTIONS.iter().find(|a| a.id == "convert-png").unwrap();
        let text = render(action);
        assert!(text.contains("Type=Service"));
        assert!(text.contains("ServiceTypes=KonqPopupMenu/Plugin"));
        assert!(text.contains("MimeType=image/*;"));
        assert!(text.contains("X-KDE-Submenu=BigIris/Converter"));
        assert!(text.contains("Exec=bigiris convert --to png"));
        assert!(text.contains("[Desktop Action bigirisConvertPng]"));
    }

    #[test]
    fn top_level_action_has_no_sub_path() {
        let action = ACTIONS.iter().find(|a| a.id == "view").unwrap();
        let text = render(action);
        assert!(text.contains("X-KDE-Submenu=BigIris\n"));
    }

    #[test]
    fn uninstall_is_idempotent_on_missing_dir() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let removed = uninstall(&paths).unwrap();
        assert!(removed.is_empty());
    }

    #[test]
    fn install_then_uninstall_leaves_clean_dir() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        install(&paths).unwrap();
        let removed = uninstall(&paths).unwrap();
        assert_eq!(removed.len(), ACTIONS.len());
        let remaining: Vec<_> = std::fs::read_dir(paths.kio_servicemenus()).unwrap().collect();
        assert!(remaining.is_empty());
    }

    #[test]
    fn uninstall_ignores_unrelated_files() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        std::fs::create_dir_all(paths.kio_servicemenus()).unwrap();
        let foreign = paths.kio_servicemenus().join("somebody-elses.desktop");
        std::fs::write(&foreign, "dont touch").unwrap();
        install(&paths).unwrap();
        uninstall(&paths).unwrap();
        assert!(foreign.exists(), "arquivo de terceiros foi removido");
    }

    #[test]
    fn camel_action_id_conversions() {
        assert_eq!(camel_action_id("convert-png"), "bigirisConvertPng");
        assert_eq!(camel_action_id("resize-25pct"), "bigirisResize25pct");
        assert_eq!(camel_action_id("rotate-90"), "bigirisRotate90");
        assert_eq!(camel_action_id("view"), "bigirisView");
    }
}
