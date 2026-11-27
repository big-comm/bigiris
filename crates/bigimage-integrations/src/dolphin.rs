// SPDX-License-Identifier: GPL-3.0-or-later
//! Dolphin / Konqueror service menus (`.desktop` files with
//! `ServiceTypes=KonqPopupMenu/Plugin`).
//!
//! KF6 nests a service menu one level deep, under the literal
//! `X-KDE-Submenu` title, and sorts the actions between two `_SEPARATOR_`
//! entries by their key. So every file uses the title `BigIris`, each
//! submenu of [`ACTIONS`] becomes a separated group whose labels carry the
//! submenu name, and keys start with the action's index to keep its order.

use std::io;
use std::path::PathBuf;

use crate::action::{ACTIONS, Action, TOP_LEVEL_LABEL};
use crate::safe_fs::FileMode;
use crate::scope::{Installed, ScopePaths, remove_generated, replace_generated};

const SUFFIX: &str = ".desktop";

/// Install one `.desktop` per MIME filter of [`ACTIONS`] into
/// `~/.local/share/kio/servicemenus/`, replacing what earlier installs wrote.
///
/// # Errors
///
/// An I/O error when the service-menu folder cannot be listed or created, or
/// a menu file cannot be written or a stale one removed.
pub fn install(paths: &ScopePaths) -> io::Result<Installed> {
    let files = menus()
        .into_iter()
        .map(|menu| (format!("bigiris-{}{SUFFIX}", menu[0].1.id), render(&menu)));
    // Dolphin treats executable permission as the trust marker for
    // user-owned ServiceMenus and silently rejects a regular `0644` desktop
    // file. Older releases wrote one file per action; left behind, they would
    // add every action again under their own submenu titles.
    replace_generated(
        &paths.kio_servicemenus(),
        SUFFIX,
        files.collect(),
        paths.scope,
        FileMode::Exact(0o755),
    )
}

/// Remove every file we own in `~/.local/share/kio/servicemenus/`.
///
/// # Errors
///
/// An I/O error when the service-menu folder cannot be listed or a file we
/// own cannot be removed.
pub fn uninstall(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    remove_generated(&paths.kio_servicemenus(), SUFFIX, &[])
}

/// [`ACTIONS`] with their index, grouped by MIME filter in declaration order:
/// Dolphin matches `MimeType=` per file, not per action.
fn menus() -> Vec<Vec<(usize, &'static Action)>> {
    let mut menus: Vec<Vec<(usize, &'static Action)>> = Vec::new();
    for (index, action) in ACTIONS.iter().enumerate() {
        match menus.iter_mut().find(|menu| menu[0].1.mime_types == action.mime_types) {
            Some(menu) => menu.push((index, action)),
            None => menus.push(vec![(index, action)]),
        }
    }
    menus
}

fn render(menu: &[(usize, &Action)]) -> String {
    let mime_line: String = menu[0].1.mime_types.iter().map(|m| format!("{m};")).collect();
    let mut keys = String::new();
    let mut entries = String::new();
    let mut previous_submenu = None;
    for (position, &(index, action)) in menu.iter().enumerate() {
        let submenu = action.submenu.map(|sub| sub.id);
        if position > 0 && submenu != previous_submenu {
            keys.push_str("_SEPARATOR_;");
        }
        previous_submenu = submenu;
        let key = action_key(index, action.id);
        let label = match action.submenu {
            Some(sub) => format!("{} ▸ {}", sub.label, action.label),
            None => action.label.to_owned(),
        };
        keys.push_str(&key);
        keys.push(';');
        entries.push_str(&format!(
            "\n[Desktop Action {key}]\n\
             Name={label}\n{localized_name}\
             Icon={icon}\n\
             Exec={exec}\n",
            localized_name =
                crate::i18n::localized("Name", |lang| crate::i18n::grouped_label(lang, action)),
            icon = action.icon,
            exec = action.command,
        ));
    }
    format!(
        "[Desktop Entry]\n\
         X-BigIris-Generated=true\n\
         Type=Service\n\
         ServiceTypes=KonqPopupMenu/Plugin\n\
         MimeType={mime_line}\n\
         X-KDE-Submenu={TOP_LEVEL_LABEL}\n\
         X-KDE-Priority=TopLevel\n\
         Name={TOP_LEVEL_LABEL}\n\
         Icon=com.biglinux.Iris\n\
         Actions={keys}\n\
         {entries}"
    )
}

/// `(2, "convert-png")` → `bigiris02ConvertPng`. KDE action keys must be
/// alphanumeric, and KF6 sorts a group by key, so the index leads.
fn action_key(index: usize, id: &str) -> String {
    let mut out = format!("bigiris{index:02}");
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

    fn menu_with(id: &str) -> String {
        let menu = menus().into_iter().find(|m| m.iter().any(|(_, a)| a.id == id)).unwrap();
        render(&menu)
    }

    fn actions_line(text: &str) -> Vec<&str> {
        let line = text.lines().find_map(|l| l.strip_prefix("Actions=")).unwrap();
        line.split(';').filter(|k| !k.is_empty()).collect()
    }

    #[test]
    fn install_writes_file_per_filter() {
        #[cfg(target_os = "linux")]
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let written = install(&paths).unwrap().files;
        let names: Vec<_> =
            written.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(
            names,
            [
                "bigiris-view.desktop",
                "bigiris-batch-folder.desktop",
                "bigiris-web-optimize-png.desktop",
                "bigiris-pdf-convert.desktop"
            ]
        );
        let png = std::fs::read_to_string(&written[2]).unwrap();
        assert!(png.contains("MimeType=image/png;\n"), "{png}");
        assert_eq!(actions_line(&png).len(), 1, "{png}");
        assert!(!menu_with("convert-png").contains("OptimizePng"));
        for path in &written {
            #[cfg(target_os = "linux")]
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o755,
                "Dolphin rejects non-executable user ServiceMenus"
            );
        }
    }

    #[test]
    fn every_action_rendered_once() {
        let keys: Vec<String> = menus()
            .iter()
            .flat_map(|menu| {
                actions_line(&render(menu))
                    .into_iter()
                    .filter(|k| *k != "_SEPARATOR_")
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(keys.len(), ACTIONS.len());
    }

    #[test]
    fn image_menu_is_one_bigiris_submenu() {
        let text = menu_with("convert-png");
        assert!(text.contains("MimeType=image/*;\n"));
        assert_eq!(text.matches("X-KDE-Submenu=").count(), 1);
        assert!(text.contains("X-KDE-Submenu=BigIris\n"));
        assert!(text.contains("[Desktop Action bigiris02ConvertPng]"));
        assert!(text.contains("Name=Convert ▸ PNG\n"));
        assert!(text.contains("Name[pt_BR]=Converter ▸ PNG\n"));
        assert!(text.contains("Exec=bigiris --notify convert --to png --overwrite skip %F"));
    }

    #[test]
    fn keys_sort_in_declaration_order() {
        // KF6 sorts each separated group by key; ascending keys keep ACTIONS order.
        for menu in menus() {
            let text = render(&menu);
            let keys = actions_line(&text);
            for group in keys.split(|k| *k == "_SEPARATOR_") {
                assert!(group.windows(2).all(|w| w[0] < w[1]), "{group:?}");
            }
        }
    }

    #[test]
    fn separators_split_submenus() {
        let menu = menus().into_iter().find(|m| m[0].1.id == "view").unwrap();
        let text = render(&menu);
        let keys = actions_line(&text);
        assert_eq!(keys[0], "bigiris00View");
        assert_eq!(keys[1], "_SEPARATOR_");
        assert!(keys[2].ends_with("ConvertPng"));
        let separators = keys.iter().filter(|k| **k == "_SEPARATOR_").count();
        let mut groups: Vec<_> = menu.iter().filter_map(|(_, a)| a.submenu.map(|s| s.id)).collect();
        groups.dedup();
        assert_eq!(separators, groups.len());
    }

    #[test]
    fn install_replaces_per_action_files() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        std::fs::create_dir_all(paths.kio_servicemenus()).unwrap();
        let old = paths.kio_servicemenus().join("bigiris-convert-png.desktop");
        std::fs::write(&old, "[Desktop Entry]\nX-BigIris-Generated=true\n").unwrap();
        install(&paths).unwrap();
        assert!(!old.exists());
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
        let written = install(&paths).unwrap().files;
        let removed = uninstall(&paths).unwrap();
        assert_eq!(removed.len(), written.len());
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
        assert!(foreign.exists(), "third-party file was removed");
    }

    #[test]
    fn action_key_conversions() {
        assert_eq!(action_key(2, "convert-png"), "bigiris02ConvertPng");
        assert_eq!(action_key(9, "resize-25pct"), "bigiris09Resize25pct");
        assert_eq!(action_key(13, "rotate-90"), "bigiris13Rotate90");
        assert_eq!(action_key(0, "view"), "bigiris00View");
    }
}
