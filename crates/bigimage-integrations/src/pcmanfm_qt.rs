// SPDX-License-Identifier: GPL-3.0-or-later
//! PCManFM-Qt / libfm-qt: Desktop Entry Extensions for Menus and Actions
//! in `~/.local/share/file-manager/actions/`. One `Type=Action` file per
//! action, one `Type=Menu` file per submenu and a top `BigIris` menu whose
//! `ItemsList` names them in [`ACTIONS`] order. libfm-qt keeps that order
//! inside a menu and drops a menu none of whose actions match the selection.

use std::io;
use std::path::PathBuf;

use crate::action::{ACTIONS, Action, Submenu, TOP_LEVEL_LABEL};
use crate::i18n::{localized, lookup};
use crate::safe_fs::FileMode;
use crate::scope::{ScopePaths, remove_generated, replace_generated};

const SUFFIX: &str = ".desktop";

/// Install the action and menu files, removing those of actions a release
/// dropped.
pub fn install(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    let files = entries().into_iter().map(|(id, body)| (format!("{id}{SUFFIX}"), body));
    replace_generated(
        &paths.libfm_actions(),
        SUFFIX,
        files.collect(),
        paths.scope,
        FileMode::Exact(0o644),
    )
}

/// Remove every file we own in `~/.local/share/file-manager/actions/`.
pub fn uninstall(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    remove_generated(&paths.libfm_actions(), SUFFIX, &[])
}

/// `(id, file body)` of every action and menu; libfm-qt names an entry by
/// its file name without the suffix.
fn entries() -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> =
        ACTIONS.iter().map(|action| (action_id(action), render_action(action))).collect();
    let mut top_items: Vec<String> = Vec::new();
    let mut submenus: Vec<(Submenu, Vec<String>)> = Vec::new();
    for action in ACTIONS {
        let Some(sub) = action.submenu else {
            top_items.push(action_id(action));
            continue;
        };
        match submenus.iter_mut().find(|(known, _)| known.id == sub.id) {
            Some((_, items)) => items.push(action_id(action)),
            None => {
                top_items.push(menu_id(&sub));
                submenus.push((sub, vec![action_id(action)]));
            }
        }
    }
    for (sub, items) in &submenus {
        let name = localized("Name", |lang| lookup(lang, sub.label).to_owned());
        entries.push((menu_id(sub), render_menu(sub.label, &name, sub.icon, items)));
    }
    let top = render_menu(TOP_LEVEL_LABEL, "", "com.biglinux.Iris", &top_items);
    entries.push(("bigiris-menu".to_owned(), top));
    entries
}

fn action_id(action: &Action) -> String {
    format!("bigiris-{}", action.id)
}

fn menu_id(sub: &Submenu) -> String {
    format!("bigiris-menu-{}", sub.id)
}

/// `localized_name` holds the `Name[lang]=` lines.
fn render_menu(name: &str, localized_name: &str, icon: &str, items: &[String]) -> String {
    let items_list: String = items.iter().map(|id| format!("{id};")).collect();
    format!(
        "[Desktop Entry]\n\
         X-BigIris-Generated=true\n\
         Type=Menu\n\
         Name={name}\n{localized_name}\
         Icon={icon}\n\
         ItemsList={items_list}\n"
    )
}

fn render_action(action: &Action) -> String {
    let mime_line: String = action.mime_types.iter().map(|m| format!("{m};")).collect();
    let profile_id = action_id(action);

    format!(
        "[Desktop Entry]\n\
         X-BigIris-Generated=true\n\
         Type=Action\n\
         Name={label}\n{localized_name}\
         Icon={icon}\n\
         Profiles={profile_id};\n\
         \n\
         [X-Action-Profile {profile_id}]\n\
         Exec={exec}\n\
         MimeTypes={mime_line}\n",
        label = action.label,
        localized_name = localized("Name", |lang| lookup(lang, action.label).to_owned()),
        icon = action.icon,
        exec = action.command,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn items_list(body: &str) -> Vec<&str> {
        let line = body.lines().find_map(|l| l.strip_prefix("ItemsList=")).unwrap();
        line.split(';').filter(|id| !id.is_empty()).collect()
    }

    #[test]
    fn install_and_uninstall_round_trip() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let written = install(&paths).unwrap();
        // Every action, every submenu and the top menu.
        assert_eq!(written.len(), ACTIONS.len() + crate::action::submenus().len() + 1);
        let removed = uninstall(&paths).unwrap();
        assert_eq!(removed.len(), written.len());
    }

    #[test]
    fn rendered_file_has_required_sections() {
        let a = ACTIONS.iter().find(|a| a.id == "rotate-90").unwrap();
        let text = render_action(a);
        assert!(text.contains("Type=Action"));
        assert!(text.contains("\nName=90°\n"), "{text}");
        assert!(text.contains("[X-Action-Profile bigiris-rotate-90]"));
        assert!(text.contains("Exec=bigiris --notify rotate --degrees 90"));
        assert!(text.contains("MimeTypes=image/*;"));
    }

    #[test]
    fn menus_nest_every_action_once_in_order() {
        let entries = entries();
        let body_of =
            |id: &str| entries.iter().find(|(entry, _)| entry == id).map(|(_, body)| body.as_str());
        let top = body_of("bigiris-menu").unwrap();
        assert!(top.contains("Type=Menu\nName=BigIris\n"), "{top}");
        let mut nested = Vec::new();
        for item in items_list(top) {
            let body = body_of(item).unwrap_or_else(|| panic!("{item} names no file"));
            if body.contains("Type=Menu") {
                for child in items_list(body) {
                    assert!(body_of(child).unwrap().contains("Type=Action"), "{child}");
                    nested.push(child.to_owned());
                }
            } else {
                nested.push(item.to_owned());
            }
        }
        let expected: Vec<String> = ACTIONS.iter().map(action_id).collect();
        assert_eq!(nested, expected);
        let convert = body_of("bigiris-menu-convert").unwrap();
        assert!(
            convert.contains("\nName=Convert\n") && convert.contains("Name[pt_BR]=Converter\n")
        );
    }
}
