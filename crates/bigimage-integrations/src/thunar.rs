// SPDX-License-Identifier: GPL-3.0-or-later
//! XFCE Thunar — merges our actions into `~/.config/Thunar/uca.xml`,
//! preserving any custom actions the user added via Thunar's own GUI.
//!
//! Thunar stores every user custom action in one XML file with a fixed
//! shape produced by Thunar itself, so a careful state-machine walk over
//! the string beats parsing the full XML grammar. Our actions are all
//! tagged with `<unique-id>bigiris-…</unique-id>` and live as siblings of
//! any pre-existing `<action>` blocks; uninstall just drops ours.

use std::io;
use std::path::{Path, PathBuf};

use crate::action::{Action, ACTIONS, TOP_LEVEL_LABEL};
use crate::safe_fs::safe_write;
use crate::scope::ScopePaths;

const EMPTY_XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n</actions>\n";

/// Install (or refresh) our actions in `~/.config/Thunar/uca.xml`.
pub fn install(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    let target = paths.thunar_uca();
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = read_or_default(&target)?;
    let stripped = strip_bigiris_actions(&existing);
    let merged = inject_before_close(&stripped, &render_all_actions());
    safe_write(&target, merged)?;
    Ok(vec![target])
}

/// Drop every bigiris-owned `<action>` from `uca.xml`. If the resulting file
/// contains no actions at all (we were the only source), it is removed so
/// Thunar falls back to its built-in defaults.
pub fn uninstall(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    let target = paths.thunar_uca();
    if !target.exists() {
        return Ok(Vec::new());
    }
    let existing = std::fs::read_to_string(&target)?;
    let stripped = strip_bigiris_actions(&existing);
    if !stripped.contains("<action>") {
        std::fs::remove_file(&target)?;
    } else {
        safe_write(&target, stripped)?;
    }
    Ok(vec![target])
}

fn read_or_default(path: &Path) -> io::Result<String> {
    if path.exists() {
        std::fs::read_to_string(path)
    } else {
        Ok(EMPTY_XML.to_string())
    }
}

/// Remove every `<action>…<unique-id>bigiris-…</unique-id>…</action>` block,
/// plus any leading whitespace so we don't leave orphan blank lines behind.
fn strip_bigiris_actions(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut cursor = 0;

    while let Some(rel) = xml[cursor..].find("<action>") {
        let action_start = cursor + rel;
        let Some(end_rel) = xml[action_start..].find("</action>") else {
            // Malformed input — stop stripping, preserve the rest verbatim.
            out.push_str(&xml[cursor..]);
            return out;
        };
        let action_end = action_start + end_rel + "</action>".len();
        let body = &xml[action_start..action_end];
        let is_ours = body.contains("<unique-id>bigiris-");

        if is_ours {
            // Rewind out of the leading whitespace that used to separate this
            // block from the previous line — we don't want to leave a gap.
            let rtrimmed_len = out.trim_end_matches([' ', '\t']).len();
            out.truncate(rtrimmed_len);
            if out.ends_with('\n') {
                // keep exactly one newline between surviving blocks
            }
            cursor = action_end;
            // Eat exactly one trailing newline so blocks don't accumulate gaps.
            if xml.as_bytes().get(cursor).copied() == Some(b'\n') {
                cursor += 1;
            }
        } else {
            out.push_str(&xml[cursor..action_end]);
            cursor = action_end;
        }
    }

    out.push_str(&xml[cursor..]);
    out
}

fn inject_before_close(xml: &str, new_actions: &str) -> String {
    match xml.rfind("</actions>") {
        Some(idx) => {
            let before = xml[..idx].trim_end().to_string();
            let after = &xml[idx..];
            let mut result = before;
            result.push('\n');
            result.push_str(new_actions);
            result.push('\n');
            result.push_str(after);
            result
        }
        None => {
            // No sensible root element — produce one from scratch.
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n{new_actions}\n</actions>\n"
            )
        }
    }
}

fn render_all_actions() -> String {
    ACTIONS.iter().map(render_action).collect::<Vec<_>>().join("\n")
}

fn render_action(action: &Action) -> String {
    let submenu_path = match action.submenu {
        Some(sub) => format!("{TOP_LEVEL_LABEL}/{}", sub.label),
        None => TOP_LEVEL_LABEL.to_string(),
    };
    // Thunar passes <command> through `g_shell_parse_argv`, so a malicious
    // filename containing `$()` or backticks would otherwise be interpreted
    // as shell metasyntax. Wrap in `sh -c '… "$@"' progname …` so paths
    // become positional args ($1, $2, …) and never re-traverse the parser.
    let safe_cmd = wrap_for_shell(action);
    // Filter element: `<image-files/>` restricts the action to image MIME
    // (KDE/GNOME convention). Document-MIME actions (PDF submenu) need
    // `<other-files/>` or they're invisible on .docx/.odt selections.
    let file_filter = if action.mime_types.iter().any(|m| m.starts_with("image/")) {
        "<image-files/>"
    } else {
        "<other-files/>"
    };
    format!(
        "<action>\n\
         \t<icon>{icon}</icon>\n\
         \t<name>{label}</name>\n{localized_names}\
         \t<submenu>{submenu}</submenu>\n\
         \t<unique-id>bigiris-{id}</unique-id>\n\
         \t<command>{cmd}</command>\n\
         \t<description>BigIris — {label}</description>\n{localized_descriptions}\
         \t<range>0</range>\n\
         \t<patterns>*</patterns>\n\
         \t{filter}\n\
         </action>",
        localized_names = crate::i18n::xml_labels("name", action.label),
        localized_descriptions = crate::i18n::xml_labels("description", action.label),
        icon = escape_xml(action.icon),
        label = escape_xml(action.label),
        submenu = escape_xml(&submenu_path),
        id = action.id,
        cmd = escape_xml(&safe_cmd),
        filter = file_filter,
    )
}

/// Rewrite `bigiris … %F` into `sh -c '… "$@"' bigiris-id %F` so the file
/// paths (whatever Thunar substitutes for `%F`) become positional argv
/// entries, never re-parsed by the shell. Action templates are required
/// (by `every_command_uses_file_placeholder` test) to end with `%F`.
fn wrap_for_shell(action: &Action) -> String {
    let body = action.command.trim_end_matches("%F").trim_end();
    // Action templates are static `&'static str` and currently never
    // contain a single quote, but escape anyway: `'` → `'\''`.
    let escaped = body.replace('\'', r"'\''");
    format!("sh -c '{escaped} -- \"$@\"' bigiris-{id} %F", id = action.id)
}

pub(crate) fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn install_on_fresh_home_creates_file_with_only_our_actions() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        install(&paths).unwrap();
        let body = read(&paths.thunar_uca());
        for action in ACTIONS {
            assert!(
                body.contains(&format!("<unique-id>bigiris-{}</unique-id>", action.id)),
                "missing action {}",
                action.id
            );
        }
        assert!(body.starts_with("<?xml"));
        assert!(body.contains("<actions>"));
        assert!(body.contains("</actions>"));
    }

    #[test]
    fn install_preserves_existing_foreign_actions() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        std::fs::create_dir_all(paths.thunar_uca().parent().unwrap()).unwrap();
        let existing = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                        <actions>\n\
                        <action>\n\
                        \t<icon>utilities-terminal</icon>\n\
                        \t<name>Open Terminal Here</name>\n\
                        \t<unique-id>user-custom-1700000000</unique-id>\n\
                        \t<command>xterm</command>\n\
                        \t<description>Open a terminal</description>\n\
                        </action>\n\
                        </actions>\n";
        std::fs::write(paths.thunar_uca(), existing).unwrap();

        install(&paths).unwrap();
        let body = read(&paths.thunar_uca());
        assert!(body.contains("user-custom-1700000000"), "foreign action nao preservada");
        assert!(body.contains("bigiris-convert-png"), "nossa acao nao injetada");
    }

    #[test]
    fn install_is_idempotent_on_repeated_runs() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        install(&paths).unwrap();
        let first = read(&paths.thunar_uca());
        install(&paths).unwrap();
        let second = read(&paths.thunar_uca());
        // Idempotency = exactly one <action> block per id. Count the
        // unique-id tag, not the bare action id (the shell wrapper now
        // also embeds it as `progname` for sh -c).
        let needle = "<unique-id>bigiris-convert-png</unique-id>";
        let first_count = first.matches(needle).count();
        let second_count = second.matches(needle).count();
        assert_eq!(first_count, 1, "primeira execucao duplicou? ({first_count})");
        assert_eq!(second_count, 1, "segunda execucao duplicou? ({second_count})");
    }

    #[test]
    fn uninstall_drops_our_actions_only() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        std::fs::create_dir_all(paths.thunar_uca().parent().unwrap()).unwrap();
        let existing = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                        <actions>\n\
                        <action>\n\
                        \t<name>Mine</name>\n\
                        \t<unique-id>user-keep-me</unique-id>\n\
                        \t<command>echo</command>\n\
                        </action>\n\
                        </actions>\n";
        std::fs::write(paths.thunar_uca(), existing).unwrap();

        install(&paths).unwrap();
        uninstall(&paths).unwrap();
        let body = read(&paths.thunar_uca());
        assert!(body.contains("user-keep-me"));
        assert!(!body.contains("bigiris-"), "restou acao nossa apos uninstall");
    }

    #[test]
    fn uninstall_removes_file_when_only_ours_existed() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        install(&paths).unwrap();
        uninstall(&paths).unwrap();
        assert!(!paths.thunar_uca().exists(), "uca.xml nao deveria sobreviver");
    }

    #[test]
    fn uninstall_on_missing_file_is_noop() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let out = uninstall(&paths).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn escape_xml_handles_reserved_chars() {
        assert_eq!(escape_xml("a & b < c"), "a &amp; b &lt; c");
        assert_eq!(escape_xml("\"hi\""), "&quot;hi&quot;");
    }

    #[test]
    fn wrap_for_shell_yields_argv_safe_command() {
        // Find the convert-png action so we exercise a representative template.
        let a = ACTIONS.iter().find(|a| a.id == "convert-png").unwrap();
        let wrapped = wrap_for_shell(a);
        // Body ends with "$@" -- so file paths become positional argv entries,
        // and sh -c never re-parses %F's expansion as shell.
        assert!(wrapped.starts_with("sh -c '"));
        assert!(wrapped.contains(r#"-- "$@"' bigiris-convert-png %F"#));
        // Original %F should NOT appear inside the quoted body.
        let body =
            wrapped.strip_prefix("sh -c '").and_then(|r| r.split_once("' bigiris-")).unwrap().0;
        assert!(!body.contains("%F"), "%F leaked into shell-parsed body: {body}");
    }

    #[test]
    fn document_action_renders_other_files_filter() {
        // The PDF submenu uses DOC_MIME; Thunar needs <other-files/> for those
        // to appear on .docx/.odt selections rather than only on image MIME.
        let a = ACTIONS.iter().find(|a| a.id == "pdf-convert").unwrap();
        let xml = render_action(a);
        assert!(xml.contains("<other-files/>"), "DOC_MIME action missing <other-files/>: {xml}");
        assert!(!xml.contains("<image-files/>"), "DOC_MIME action wrongly emits <image-files/>");
    }

    #[test]
    fn image_action_renders_image_files_filter() {
        let a = ACTIONS.iter().find(|a| a.id == "convert-png").unwrap();
        let xml = render_action(a);
        assert!(xml.contains("<image-files/>"));
        assert!(!xml.contains("<other-files/>"));
    }

    #[test]
    fn strip_bigiris_handles_block_at_file_start() {
        let xml = "<actions>\n\
                   <action><unique-id>bigiris-test</unique-id></action>\n\
                   <action><unique-id>other</unique-id></action>\n\
                   </actions>\n";
        let stripped = strip_bigiris_actions(xml);
        assert!(!stripped.contains("bigiris-test"));
        assert!(stripped.contains("<unique-id>other</unique-id>"));
    }
}
