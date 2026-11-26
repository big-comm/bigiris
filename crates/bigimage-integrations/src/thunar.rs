// SPDX-License-Identifier: GPL-3.0-or-later
//! XFCE Thunar — merges our actions into `~/.config/Thunar/uca.xml`,
//! preserving any custom actions the user added via Thunar's own GUI.
//!
//! Thunar stores every user custom action in one XML file with a fixed
//! shape produced by Thunar itself, so a careful state-machine walk over
//! the string beats parsing the full XML grammar. Our actions are all
//! tagged with `<unique-id>bigiris-…</unique-id>` and live as siblings of
//! any pre-existing `<action>` blocks; uninstall just drops ours.
//!
//! Thunar reads only the *first* `Thunar/uca.xml` it finds, user file
//! before `$XDG_CONFIG_DIRS` (`/etc/xdg`). So when the user has no file
//! yet, we seed it from that system file — exactly what Thunar does the
//! first time it saves — otherwise the new personal file would hide the
//! distribution defaults ("Open Terminal Here", "Search for Files").
//!
//! **User scope only.** The system-wide `/etc/xdg/Thunar/uca.xml` is owned
//! by the `thunar` package. A distro package that also shipped it would
//! conflict with `thunar` on install, and forcing it would wipe those
//! defaults. So `--system` / `--destdir` installs skip Thunar entirely;
//! users get the actions with `bigiris install-integrations --user`, which
//! merges them into their personal `~/.config/Thunar/uca.xml`.

use std::io;
use std::path::{Path, PathBuf};

use crate::action::{ACTIONS, Action, TOP_LEVEL_LABEL};
use crate::safe_fs::{FileMode, safe_write_with_mode};
use crate::scope::{Installed, Scope, ScopePaths};

const XML_DECLARATION: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";
const EMPTY_XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n</actions>\n";

/// `uca.xml` may hold hosts, users or tokens in custom commands. Thunar
/// creates it `0600` (`g_mkstemp`); keep whatever mode the user has.
const UCA_MODE: FileMode = FileMode::KeepExisting(0o600);

/// Install (or refresh) our actions in `~/.config/Thunar/uca.xml`.
///
/// No-op for [`Scope::System`]: see the module docs for why we never write
/// Thunar's package-owned system file.
pub fn install(paths: &ScopePaths) -> io::Result<Installed> {
    if paths.scope == Scope::System {
        return Ok(Installed::default());
    }
    let target = paths.thunar_uca();
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let base = match read_existing(&target)? {
        Some(body) => body,
        None => seed_from_system(&paths.xdg_config_dirs),
    };
    let stripped = strip_bigiris_actions(&base);
    let merged = inject_before_close(&stripped, &render_all_actions());
    safe_write_with_mode(&target, merged, paths.scope, UCA_MODE)?;
    Ok(Installed { files: vec![target], kept: Vec::new() })
}

/// Drop every bigiris-owned `<action>` from `uca.xml`. In user scope, if
/// what remains is exactly what Thunar shows without a personal file (the
/// system defaults we seeded it with, or nothing when there are none), the
/// file is removed so Thunar falls back to the system defaults again. A
/// file that is a symbolic link (a dotfile manager's) is rewritten instead,
/// since removing the link would leave our actions in the real file. A file
/// left with no actions while the system has defaults stays: one the user
/// emptied cannot be told from one an install made before seeding existed.
///
/// The CLI only runs this in user scope. The system-scope branch (strip
/// actions an old BigIris package merged into `/etc/xdg/Thunar/uca.xml`,
/// never deleting that `thunar`-owned file) is library-only for now. A
/// file without any of our actions is left untouched.
pub fn uninstall(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    let target = paths.thunar_uca();
    let Some(existing) = read_existing(&target)? else {
        return Ok(Vec::new());
    };
    let stripped = strip_bigiris_actions(&existing);
    if stripped == existing {
        return Ok(Vec::new());
    }
    let only_defaults = same_xml(&stripped, &seed_from_system(&paths.xdg_config_dirs));
    let is_link = std::fs::symlink_metadata(&target)?.is_symlink();
    if paths.scope == Scope::User && only_defaults && !is_link {
        std::fs::remove_file(&target)?;
    } else {
        safe_write_with_mode(&target, stripped, paths.scope, UCA_MODE)?;
    }
    Ok(vec![target])
}

/// The user's current `uca.xml`, or `None` when there is none yet (an
/// empty file counts as none: Thunar cannot parse it either). A file that
/// lost its closing `</actions>` is refused rather than rebuilt, because
/// rebuilding would drop every custom action the user still has in it.
/// Anything but a regular file (or a link to one) is refused: reading a
/// FIFO would block.
fn read_existing(path: &Path) -> io::Result<Option<String>> {
    let body = match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => std::fs::read_to_string(path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", path.display()),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if body.trim().is_empty() {
        return Ok(None);
    }
    if !body.contains("</actions>") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} has no closing </actions> tag (truncated or edited by hand?); \
                 it was left unchanged so your custom actions are not lost. \
                 Fix the file or move it aside (for example to uca.xml.bak) to start over, \
                 then run the command again",
                path.display()
            ),
        ));
    }
    if !body.contains("<actions") {
        // Earlier releases dropped the lines before the first action when
        // it was ours, leaving a file Thunar cannot parse.
        return Ok(Some(format!("{XML_DECLARATION}<actions>\n{}", body.trim_start())));
    }
    Ok(Some(body))
}

/// What Thunar shows when the user has no `uca.xml`: the first system
/// `Thunar/uca.xml` along `config_dirs`, minus actions an old BigIris
/// package may have merged into it. An unreadable or broken system file
/// yields no actions, as it does in Thunar.
fn seed_from_system(config_dirs: &[PathBuf]) -> String {
    let Some(system) = config_dirs.iter().map(|d| d.join("Thunar/uca.xml")).find(|p| p.is_file())
    else {
        return EMPTY_XML.to_string();
    };
    match std::fs::read_to_string(system) {
        Ok(body) if body.contains("</actions>") => strip_bigiris_actions(&body),
        _ => EMPTY_XML.to_string(),
    }
}

/// Equal up to whitespace — our merge re-indents around the insertion.
fn same_xml(a: &str, b: &str) -> bool {
    a.split_whitespace().eq(b.split_whitespace())
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
            // Keep what precedes the block but not its indentation.
            out.push_str(xml[cursor..action_start].trim_end_matches([' ', '\t']));
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
            format!("{XML_DECLARATION}<actions>\n{new_actions}\n</actions>\n")
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
    // Thunar quotes each path it substitutes for %F and runs the command
    // with /bin/sh -c. The `sh -c '… "$@"'` wrapper also hands the paths
    // over as positional arguments, so no file name is ever part of the
    // action's own shell text, even if that quoting were ever lost.
    let safe_cmd = wrap_for_shell(action);
    let file_filter = file_filters(action.mime_types).join("\n\t");
    format!(
        "<action>\n\
         \t<icon>{icon}</icon>\n\
         \t<name>{label}</name>\n{localized_names}\
         \t<submenu>{submenu}</submenu>\n\
         \t<unique-id>bigiris-{id}</unique-id>\n\
         \t<command>{cmd}</command>\n\
         \t<description>BigIris — {label}</description>\n{localized_descriptions}\
         \t<range>0</range>\n\
         \t<patterns>{patterns}</patterns>\n\
         \t{filter}\n\
         </action>",
        localized_names = crate::i18n::xml_labels("name", action.label),
        localized_descriptions = crate::i18n::xml_labels("description", action.label),
        icon = escape_xml(action.icon),
        label = escape_xml(action.label),
        submenu = escape_xml(&submenu_path),
        id = action.id,
        cmd = escape_xml(&safe_cmd),
        patterns = escape_xml(&patterns(action.mime_types)),
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

/// File-name extensions for the concrete MIME types we list; `None` for
/// wildcards (`image/*`), directories and anything not mapped here.
fn extensions_for(mime: &str) -> Option<&'static [&'static str]> {
    Some(match mime {
        "image/png" => &["png"],
        "application/vnd.oasis.opendocument.text" => &["odt", "ott", "fodt"],
        "application/vnd.oasis.opendocument.spreadsheet" => &["ods", "ots", "fods"],
        "application/vnd.oasis.opendocument.presentation" => &["odp", "otp", "fodp"],
        "application/vnd.oasis.opendocument.graphics" => &["odg", "otg", "fodg"],
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => &["docx"],
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => &["xlsx"],
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => &["pptx"],
        "application/msword" => &["doc"],
        "application/vnd.ms-excel" => &["xls"],
        "application/vnd.ms-powerpoint" => &["ppt", "pps"],
        "application/rtf" => &["rtf"],
        "text/plain" => &["txt"],
        "text/markdown" => &["md", "markdown"],
        "text/csv" => &["csv"],
        "text/html" => &["html", "htm"],
        "application/xhtml+xml" => &["xhtml"],
        _ => return None,
    })
}

/// The `<patterns>` value. Thunar ANDs it with the file-class elements
/// from [`file_filters`], and those classes are coarse: `<other-files/>`
/// alone would offer "Convert to PDF" on every `.zip`, `.iso` or `.sh`.
/// So when every MIME type maps to known extensions we list them; Thunar
/// matches with `g_pattern_match_simple`, which is case-sensitive, hence
/// the lower- and upper-case variant of each (mixed case like `.Docx` is
/// not matched). Wildcard MIME lists keep `*`.
fn patterns(mime_types: &[&str]) -> String {
    let mut out: Vec<String> = Vec::new();
    for mime in mime_types {
        let Some(exts) = extensions_for(mime) else {
            return "*".to_string();
        };
        for ext in exts {
            out.push(format!("*.{ext}"));
            out.push(format!("*.{}", ext.to_ascii_uppercase()));
        }
    }
    if out.is_empty() { "*".to_string() } else { out.join(";") }
}

/// Map MIME patterns onto Thunar's appearance-condition elements. Thunar
/// has no MIME list, only coarse classes (`<image-files/>`,
/// `<directories/>`, …); an action shows up when the selection matches any
/// listed class, so a mixed MIME list (the PDF submenu covers ODF/Office
/// *and* `text/*`) needs several elements.
fn file_filters(mime_types: &[&str]) -> Vec<&'static str> {
    let mut filters: Vec<&'static str> = Vec::new();
    for mime in mime_types {
        let filter = match *mime {
            "inode/directory" => "<directories/>",
            m if m.starts_with("image/") => "<image-files/>",
            m if m.starts_with("text/") => "<text-files/>",
            m if m.starts_with("audio/") => "<audio-files/>",
            m if m.starts_with("video/") => "<video-files/>",
            _ => "<other-files/>",
        };
        if !filters.contains(&filter) {
            filters.push(filter);
        }
    }
    if filters.is_empty() {
        filters.push("<other-files/>");
    }
    filters
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
        assert!(body.contains("user-custom-1700000000"), "foreign action not preserved");
        assert!(body.contains("bigiris-convert-png"), "our action not inserted");
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
        assert_eq!(first_count, 1, "first run duplicated the action ({first_count})");
        assert_eq!(second_count, 1, "second run duplicated the action ({second_count})");
        assert_eq!(first, second, "reinstall changed the file");
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
        assert!(!body.contains("bigiris-"), "one of our actions survived uninstall");
    }

    #[test]
    fn uninstall_removes_file_when_only_ours_existed() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        install(&paths).unwrap();
        uninstall(&paths).unwrap();
        assert!(!paths.thunar_uca().exists(), "uca.xml should have been removed");
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
    fn document_action_also_matches_text_files() {
        // DOC_MIME includes text/plain, text/markdown, … which Thunar
        // classifies as text files, not as "other" files.
        let a = ACTIONS.iter().find(|a| a.id == "pdf-convert").unwrap();
        let xml = render_action(a);
        assert!(xml.contains("<text-files/>"), "text/* MIME missing <text-files/>: {xml}");
    }

    #[test]
    fn directory_action_renders_directories_filter() {
        let a = ACTIONS.iter().find(|a| a.mime_types == ["inode/directory"]).unwrap();
        let xml = render_action(a);
        assert!(xml.contains("<directories/>"), "folder action missing <directories/>: {xml}");
        assert!(!xml.contains("<other-files/>"), "folder action shown on regular files");
    }

    #[test]
    fn file_filters_map_each_class_once() {
        assert_eq!(file_filters(&["inode/directory"]), ["<directories/>"]);
        assert_eq!(file_filters(&["image/*", "image/png"]), ["<image-files/>"]);
        assert_eq!(
            file_filters(&["application/pdf", "text/plain", "text/csv", "application/rtf"]),
            ["<other-files/>", "<text-files/>"]
        );
        assert_eq!(file_filters(&["audio/ogg", "video/mp4"]), ["<audio-files/>", "<video-files/>"]);
        assert_eq!(file_filters(&[]), ["<other-files/>"]);
    }

    #[test]
    fn system_scope_never_writes_thunar_package_file() {
        // /etc/xdg/Thunar/uca.xml is owned by the thunar package.
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::system_with_destdir(tmp.path());
        assert!(install(&paths).unwrap().files.is_empty());
        assert!(!paths.thunar_uca().exists());
    }

    #[test]
    fn system_uninstall_strips_legacy_actions_but_keeps_package_file() {
        let tmp = TempDir::new().unwrap();
        let seed = ScopePaths::rooted_at(tmp.path().join("seed"));
        install(&seed).unwrap();
        let paths = ScopePaths::system_with_destdir(tmp.path().join("root"));
        std::fs::create_dir_all(paths.thunar_uca().parent().unwrap()).unwrap();
        std::fs::copy(seed.thunar_uca(), paths.thunar_uca()).unwrap();
        assert_eq!(uninstall(&paths).unwrap(), vec![paths.thunar_uca()]);
        let body = read(&paths.thunar_uca());
        assert!(!body.contains("bigiris-"));
        assert!(body.contains("</actions>"));
    }

    #[test]
    fn uninstall_without_our_actions_leaves_file_untouched() {
        use std::os::unix::fs::MetadataExt;
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        std::fs::create_dir_all(paths.thunar_uca().parent().unwrap()).unwrap();
        std::fs::write(paths.thunar_uca(), EMPTY_XML).unwrap();
        let inode = std::fs::metadata(paths.thunar_uca()).unwrap().ino();
        assert!(uninstall(&paths).unwrap().is_empty());
        assert_eq!(std::fs::metadata(paths.thunar_uca()).unwrap().ino(), inode);
    }

    #[test]
    fn install_replaces_uca_atomically_and_keeps_user_actions() {
        use std::os::unix::fs::MetadataExt;
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let dir = paths.thunar_uca().parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let existing = "<actions>\n<action><unique-id>user-1</unique-id></action>\n</actions>\n";
        std::fs::write(paths.thunar_uca(), existing).unwrap();
        let inode = std::fs::metadata(paths.thunar_uca()).unwrap().ino();
        install(&paths).unwrap();
        // New inode = rename-over, not truncate-in-place.
        assert_ne!(std::fs::metadata(paths.thunar_uca()).unwrap().ino(), inode);
        assert!(read(&paths.thunar_uca()).contains("user-1"));
        let stray: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "uca.xml")
            .collect();
        assert!(stray.is_empty(), "temp files left behind: {stray:?}");
    }

    #[test]
    fn install_follows_users_own_dotfile_symlink() {
        // stow / home-manager link ~/.config/Thunar/uca.xml into a repo.
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        std::fs::create_dir_all(paths.thunar_uca().parent().unwrap()).unwrap();
        let real = tmp.path().join("dotfiles-uca.xml");
        std::fs::write(&real, EMPTY_XML).unwrap();
        std::os::unix::fs::symlink(&real, paths.thunar_uca()).unwrap();
        install(&paths).unwrap();
        assert!(std::fs::symlink_metadata(paths.thunar_uca()).unwrap().file_type().is_symlink());
        assert!(read(&real).contains("<unique-id>bigiris-view</unique-id>"));
    }

    #[test]
    fn uninstall_keeps_users_dotfile_symlink() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        std::fs::create_dir_all(paths.thunar_uca().parent().unwrap()).unwrap();
        let real = tmp.path().join("dotfiles-uca.xml");
        std::fs::write(&real, EMPTY_XML).unwrap();
        std::os::unix::fs::symlink(&real, paths.thunar_uca()).unwrap();
        install(&paths).unwrap();
        assert_eq!(uninstall(&paths).unwrap(), [paths.thunar_uca()]);
        assert!(std::fs::symlink_metadata(paths.thunar_uca()).unwrap().is_symlink());
        let body = read(&real);
        assert!(!body.contains("bigiris-") && body.contains("</actions>"), "{body}");
    }

    #[test]
    fn uninstall_keeps_uca_emptied_by_user() {
        let tmp = TempDir::new().unwrap();
        let paths = home_with_system_defaults(&tmp);
        // The user deleted Thunar's default actions before installing.
        std::fs::create_dir_all(paths.thunar_uca().parent().unwrap()).unwrap();
        std::fs::write(paths.thunar_uca(), EMPTY_XML).unwrap();
        install(&paths).unwrap();
        uninstall(&paths).unwrap();
        let body = read(&paths.thunar_uca());
        assert!(!body.contains("bigiris-") && !body.contains("1-default-terminal"), "{body}");
    }

    #[test]
    fn user_action_mentioning_bigiris_survives() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        std::fs::create_dir_all(paths.thunar_uca().parent().unwrap()).unwrap();
        let mine = "<action>\n\t<name>bigiris-convert to GIF</name>\n\
                    \t<unique-id>1700000000-1</unique-id>\n\
                    \t<command>bigiris-convert-helper %F</command>\n</action>";
        std::fs::write(paths.thunar_uca(), format!("<actions>\n{mine}\n</actions>\n")).unwrap();
        install(&paths).unwrap();
        assert!(read(&paths.thunar_uca()).contains(mine));
        uninstall(&paths).unwrap();
        assert_eq!(read(&paths.thunar_uca()), format!("<actions>\n{mine}\n</actions>\n"));
    }

    #[test]
    fn install_repairs_uca_missing_its_opening() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        std::fs::create_dir_all(paths.thunar_uca().parent().unwrap()).unwrap();
        let mine = "<action><unique-id>user-1</unique-id></action>";
        let broken = format!(
            "\n<action>\n\t<unique-id>bigiris-view</unique-id>\n</action>\n{mine}\n</actions>\n"
        );
        std::fs::write(paths.thunar_uca(), broken).unwrap();
        install(&paths).unwrap();
        let body = read(&paths.thunar_uca());
        assert!(body.starts_with(&format!("{XML_DECLARATION}<actions>\n{mine}\n")), "{body}");
        assert_eq!(body.matches("<unique-id>bigiris-view</unique-id>").count(), 1);
    }

    #[test]
    fn non_regular_uca_refused() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let uca = paths.thunar_uca();
        std::fs::create_dir_all(uca.parent().unwrap()).unwrap();
        let fifo = tmp.path().join("fifo");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let dir = tmp.path().join("dir");
        std::fs::create_dir(&dir).unwrap();
        for (target, is_link) in [(&fifo, false), (&fifo, true), (&dir, false), (&dir, true)] {
            if is_link {
                std::os::unix::fs::symlink(target, &uca).unwrap();
            } else {
                std::fs::rename(target, &uca).unwrap();
            }
            assert!(install(&paths).is_err(), "{target:?} link={is_link}");
            assert!(uninstall(&paths).is_err(), "{target:?} link={is_link}");
            if is_link {
                std::fs::remove_file(&uca).unwrap();
            } else {
                std::fs::rename(&uca, target).unwrap();
            }
            let kind = std::fs::symlink_metadata(target).unwrap().file_type();
            assert!(!kind.is_file(), "{target:?} replaced by a file");
        }
    }

    #[test]
    fn wrap_for_shell_escapes_single_quotes() {
        let action = Action { command: "bigiris --bg 'dark gray' %F", ..ACTIONS[0] };
        assert_eq!(
            wrap_for_shell(&action),
            r#"sh -c 'bigiris --bg '\''dark gray'\'' -- "$@"' bigiris-view %F"#
        );
    }

    /// A user home plus one system config dir holding Thunar's defaults
    /// (and a stray action an old BigIris package merged into it).
    fn home_with_system_defaults(tmp: &TempDir) -> ScopePaths {
        let empty = tmp.path().join("xdg-empty");
        let system = tmp.path().join("xdg-system");
        let later = tmp.path().join("xdg-later");
        for dir in [&system, &later] {
            std::fs::create_dir_all(dir.join("Thunar")).unwrap();
        }
        std::fs::write(
            system.join("Thunar/uca.xml"),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n\
             <action>\n\t<name>Open Terminal Here</name>\n\
             \t<unique-id>1-default-terminal</unique-id>\n</action>\n\
             <action>\n\t<name>Old</name>\n\t<unique-id>bigiris-view</unique-id>\n</action>\n\
             </actions>\n",
        )
        .unwrap();
        // Thunar uses only the first file found, so this one never counts.
        std::fs::write(
            later.join("Thunar/uca.xml"),
            "<actions>\n<action><unique-id>2-shadowed</unique-id></action>\n</actions>\n",
        )
        .unwrap();
        let mut paths = ScopePaths::rooted_at(tmp.path().join("home"));
        paths.xdg_config_dirs = vec![empty, system, later];
        paths
    }

    #[test]
    fn fresh_install_seeds_from_first_system_uca_like_thunar() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let paths = home_with_system_defaults(&tmp);
        install(&paths).unwrap();
        let body = read(&paths.thunar_uca());
        assert!(body.contains("1-default-terminal"), "system defaults hidden: {body}");
        assert!(!body.contains("2-shadowed"), "only the first system file counts");
        assert_eq!(body.matches("<unique-id>bigiris-view</unique-id>").count(), 1);
        assert!(!body.contains("<name>Old</name>"), "stale packaged action copied");
        let mode = std::fs::metadata(paths.thunar_uca()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "new uca.xml must be private like Thunar's");
    }

    #[test]
    fn uninstall_drops_seeded_copy_but_keeps_later_user_actions() {
        let tmp = TempDir::new().unwrap();
        let paths = home_with_system_defaults(&tmp);
        install(&paths).unwrap();
        uninstall(&paths).unwrap();
        assert!(!paths.thunar_uca().exists(), "copy of the system defaults left behind");

        install(&paths).unwrap();
        let body = read(&paths.thunar_uca()).replace(
            "</actions>",
            "<action><unique-id>user-added-later</unique-id></action>\n</actions>",
        );
        std::fs::write(paths.thunar_uca(), body).unwrap();
        uninstall(&paths).unwrap();
        let body = read(&paths.thunar_uca());
        assert!(body.contains("user-added-later") && body.contains("1-default-terminal"));
        assert!(!body.contains("bigiris-"));
    }

    #[test]
    fn install_keeps_existing_uca_mode_and_creates_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        install(&paths).unwrap();
        let mode = std::fs::metadata(paths.thunar_uca()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "new uca.xml must be private like Thunar's");
        let mode = std::fs::Permissions::from_mode(0o640);
        std::fs::set_permissions(paths.thunar_uca(), mode).unwrap();
        install(&paths).unwrap();
        let mode = std::fs::metadata(paths.thunar_uca()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
    }

    #[test]
    fn install_refuses_uca_without_closing_tag_and_leaves_it_alone() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        std::fs::create_dir_all(paths.thunar_uca().parent().unwrap()).unwrap();
        let broken = "<actions>\n<action><unique-id>precious</unique-id></action>\n";
        std::fs::write(paths.thunar_uca(), broken).unwrap();
        let err = install(&paths).unwrap_err();
        assert!(err.to_string().contains("uca.xml.bak"), "unhelpful error: {err}");
        assert_eq!(read(&paths.thunar_uca()), broken);
    }

    #[test]
    fn install_treats_empty_uca_as_missing() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        std::fs::create_dir_all(paths.thunar_uca().parent().unwrap()).unwrap();
        std::fs::write(paths.thunar_uca(), "\n").unwrap();
        install(&paths).unwrap();
        assert!(read(&paths.thunar_uca()).contains("bigiris-view"));
    }

    #[test]
    fn document_action_lists_extensions_in_both_cases() {
        let a = ACTIONS.iter().find(|a| a.id == "pdf-convert").unwrap();
        let xml = render_action(a);
        let pat = xml.split("<patterns>").nth(1).unwrap().split("</patterns>").next().unwrap();
        let pats: Vec<&str> = pat.split(';').collect();
        for p in ["*.docx", "*.DOCX", "*.odt", "*.ODT", "*.txt", "*.md", "*.csv", "*.html"] {
            assert!(pats.contains(&p), "{p} missing from {pat}");
        }
        assert!(!pats.contains(&"*"), "PDF action shown on every file: {pat}");
        assert!(!pats.iter().any(|p| p.ends_with(".zip") || p.ends_with(".iso")));
    }

    #[test]
    fn wildcard_actions_keep_star_pattern() {
        assert_eq!(patterns(&["image/*"]), "*");
        assert_eq!(patterns(&["inode/directory"]), "*");
        assert_eq!(patterns(&["application/pdf", "text/plain"]), "*", "unmapped MIME → *");
        assert_eq!(patterns(&["text/csv"]), "*.csv;*.CSV");
    }

    #[test]
    fn png_optimize_matches_png_only() {
        let a = ACTIONS.iter().find(|a| a.id == "web-optimize-png").unwrap();
        let xml = render_action(a);
        assert!(xml.contains("<patterns>*.png;*.PNG</patterns>"), "{xml}");
        assert!(xml.contains("<image-files/>"));
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
        assert_eq!(
            strip_bigiris_actions(xml),
            "<actions>\n<action><unique-id>other</unique-id></action>\n</actions>\n"
        );
        let indented = "<?xml version=\"1.0\"?>\n<actions>\n  <action><unique-id>bigiris-a</unique-id>\
                        </action>\n  <!-- mine -->\n</actions>\n";
        assert_eq!(
            strip_bigiris_actions(indented),
            "<?xml version=\"1.0\"?>\n<actions>\n  <!-- mine -->\n</actions>\n"
        );
    }
}
