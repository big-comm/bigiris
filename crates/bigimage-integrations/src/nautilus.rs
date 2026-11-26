// SPDX-License-Identifier: GPL-3.0-or-later
//! Nautilus: a `nautilus-python` extension, the only entry point, at
//! `~/.local/share/nautilus-python/extensions/bigiris-menu.py`.
//!
//! A `~/.local/share/nautilus/scripts/BigIris/` tree left by a prototype
//! that predates the released history is not touched: what it held cannot
//! be told apart from the user's own scripts.

use std::io;
use std::path::{Path, PathBuf};

use crate::action::ACTIONS;
use crate::scope::{Installed, Scope, ScopePaths};

const PYTHON_EXTENSION_FILENAME: &str = "bigiris-menu.py";
/// How much of an existing `bigiris-menu.py` is read to identify it; ours
/// is a few KiB.
const MAX_EXTENSION_BYTES: u64 = 1024 * 1024;

/// System-wide install path for the Python extension. nautilus-python
/// loads from BOTH `/usr/share/nautilus-python/extensions/` AND
/// `~/.local/share/nautilus-python/extensions/` simultaneously, so a
/// user-scope install on top of a packaged system-wide one shows the
/// "BigIris ▸" submenu twice in the right-click menu.
const SYSTEM_EXTENSION_PATH: &str = "/usr/share/nautilus-python/extensions/bigiris-menu.py";

/// The Python extension body, embedded at compile time from
/// `data/nautilus/bigiris-menu.py` at the repo root; [`render`] fills its
/// action table.
const PYTHON_EXTENSION: &str = include_str!("../../../data/nautilus/bigiris-menu.py");
const ACTIONS_PLACEHOLDER: &str = "\n_ACTIONS = []\n";

/// The extension with `_ACTIONS` built from [`ACTIONS`], so Nautilus
/// offers the same labels, icons, argv and MIME filters as every other
/// file manager.
fn render() -> String {
    let py = |text: &str| format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""));
    let list = |items: &mut dyn Iterator<Item = &str>| {
        format!("[{}]", items.map(py).collect::<Vec<_>>().join(", "))
    };
    let mut table = String::from("\n_ACTIONS = [\n");
    for action in ACTIONS {
        let (sub_id, sub_label, sub_icon) = match action.submenu {
            Some(sub) => (py(sub.id), py(sub.label), py(sub.icon)),
            None => ("None".to_owned(), "None".to_owned(), "None".to_owned()),
        };
        // `bigiris <args> %F`: the extension adds the binary and the paths.
        let argv = list(&mut action.command.split_whitespace().filter(|arg| *arg != "%F").skip(1));
        let mimes = list(&mut action.mime_types.iter().copied());
        table.push_str(&format!(
            "    ({}, {sub_id}, {sub_label}, {sub_icon}, {}, {argv}, {}, {mimes}),\n",
            py(action.id),
            py(action.label),
            py(action.icon),
        ));
    }
    table.push_str("]\n");
    PYTHON_EXTENSION.replacen(ACTIONS_PLACEHOLDER, &table, 1)
}

/// Install the Python extension.
///
/// A user-scope install skips the copy when the package already ships
/// `SYSTEM_EXTENSION_PATH`, and removes any user-scope `.py` and our
/// `__pycache__` bytecode instead: `nautilus-python` loads bytecode even
/// without its `.py`, which would show "BigIris ▸" twice.
pub fn install(paths: &ScopePaths) -> io::Result<Installed> {
    install_with_system_path(paths, Path::new(SYSTEM_EXTENSION_PATH))
}

/// Same as [`install`] but takes the system-wide extension path as a
/// parameter so tests can point it at a non-existent file.
pub(crate) fn install_with_system_path(
    paths: &ScopePaths,
    system_ext: &Path,
) -> io::Result<Installed> {
    let ext_dir = paths.nautilus_python_extensions();

    // A user-scope copy beside a system-wide one duplicates the menu.
    // DESTDIR may share the same prefix as home; use the explicit scope.
    let is_user_scope = paths.scope == Scope::User;
    if is_user_scope && system_ext.exists() {
        // Errors are ignored: the next run retries.
        let stale_py = ext_dir.join(PYTHON_EXTENSION_FILENAME);
        if is_our_extension(&stale_py) {
            let _ = std::fs::remove_file(&stale_py);
        }
        remove_own_bytecode(&ext_dir);
        return Ok(Installed::default());
    }

    std::fs::create_dir_all(&ext_dir)?;
    let ext_file = ext_dir.join(PYTHON_EXTENSION_FILENAME);
    if std::fs::symlink_metadata(&ext_file).is_ok() && !is_our_extension(&ext_file) {
        return Ok(Installed { files: Vec::new(), kept: vec![ext_file] });
    }
    crate::safe_fs::safe_write(&ext_file, render(), paths.scope)?;
    Ok(Installed { files: vec![ext_file], kept: Vec::new() })
}

/// Is `path` a regular file holding a BigIris extension, ours from any
/// release? A file of the user's own under that name is left alone.
fn is_our_extension(path: &Path) -> bool {
    use std::io::Read;
    let mut body = String::new();
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
        && std::fs::File::open(path)
            .and_then(|f| f.take(MAX_EXTENSION_BYTES).read_to_string(&mut body))
            .is_ok()
        && body.contains("class BigIrisMenuProvider(")
}

/// Remove the Python extension and our bytecode
/// in the `__pycache__` beside it, which would otherwise keep the menu alive.
pub fn uninstall(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    let mut removed = Vec::new();

    let ext_dir = paths.nautilus_python_extensions();
    let ext_file = ext_dir.join(PYTHON_EXTENSION_FILENAME);
    if is_our_extension(&ext_file) {
        std::fs::remove_file(&ext_file)?;
        removed.push(ext_file);
    }
    // Best-effort: pyc deletion failure shouldn't abort the uninstall (the
    // .py is already gone, which is what matters for nautilus's next scan).
    remove_own_bytecode(&ext_dir);

    Ok(removed)
}

/// Delete only our compiled bytecode (`__pycache__/bigiris-menu.<tag>.pyc`).
/// The `__pycache__` directory is shared by every nautilus-python extension
/// in the same directory; wiping it whole would also discard other
/// extensions' caches. The directory itself is removed only when it ends up
/// empty. Errors are ignored: this is cleanup, the next run retries.
fn remove_own_bytecode(ext_dir: &Path) {
    let pycache = ext_dir.join("__pycache__");
    let Ok(entries) = std::fs::read_dir(&pycache) else {
        return;
    };
    let stem = PYTHON_EXTENSION_FILENAME.trim_end_matches(".py");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let ours = name
            .strip_prefix(stem)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|rest| rest.ends_with(".pyc"));
        if ours && entry.file_type().is_ok_and(|t| t.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    // Fails, and is ignored, while the directory still holds files.
    let _ = std::fs::remove_dir(&pycache);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn install_writes_only_python_extension() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        // Use a non-existent system path so tests don't see the dev
        // box's real `/usr/share/nautilus-python/extensions/bigiris-menu.py`
        // and skip the user-scope install.
        let written =
            install_with_system_path(&paths, Path::new("/nonexistent/system-path")).unwrap().files;
        assert_eq!(written.len(), 1);
        let ext = paths.nautilus_python_extensions().join(PYTHON_EXTENSION_FILENAME);
        assert!(ext.exists(), "python extension not installed");
    }

    #[test]
    fn packaging_keeps_extension_even_when_host_already_has_it() {
        let tmp = TempDir::new().unwrap();
        let host = tmp.path().join("host-extension.py");
        std::fs::write(&host, "host stays unchanged").unwrap();
        let paths = ScopePaths::system_with_destdir(tmp.path().join("package"));
        let written = install_with_system_path(&paths, &host).unwrap().files;
        assert_eq!(written.len(), 1);
        assert_eq!(std::fs::read_to_string(&written[0]).unwrap(), render());
        assert_eq!(std::fs::read_to_string(&host).unwrap(), "host stays unchanged");
        assert_eq!(paths.scope, Scope::System);
    }

    #[test]
    fn user_install_avoids_duplicate_and_preserves_system_extension() {
        let tmp = TempDir::new().unwrap();
        let host = tmp.path().join("host-extension.py");
        std::fs::write(&host, "host stays unchanged").unwrap();
        let paths = ScopePaths::rooted_at(tmp.path().join("user"));
        let ext = paths.nautilus_python_extensions().join(PYTHON_EXTENSION_FILENAME);
        std::fs::create_dir_all(ext.parent().unwrap()).unwrap();
        std::fs::write(&ext, "class BigIrisMenuProvider(GObject.GObject):\n    pass\n").unwrap();
        assert!(install_with_system_path(&paths, &host).unwrap().files.is_empty());
        assert!(!ext.exists());
        assert_eq!(std::fs::read_to_string(&host).unwrap(), "host stays unchanged");
    }

    #[test]
    fn user_extension_under_our_name_survives() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let ext = paths.nautilus_python_extensions().join(PYTHON_EXTENSION_FILENAME);
        std::fs::create_dir_all(ext.parent().unwrap()).unwrap();
        std::fs::write(&ext, "# my own menu\n").unwrap();
        let installed =
            install_with_system_path(&paths, Path::new("/nonexistent/system-path")).unwrap();
        assert_eq!(installed.kept, std::slice::from_ref(&ext));
        assert!(uninstall(&paths).unwrap().is_empty());
        let host = tmp.path().join("host-extension.py");
        std::fs::write(&host, "system copy").unwrap();
        install_with_system_path(&paths, &host).unwrap();
        assert_eq!(std::fs::read_to_string(&ext).unwrap(), "# my own menu\n");
    }

    #[test]
    fn scripts_tree_left_alone() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let script = paths.data.join("nautilus/scripts/BigIris/My preset");
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(&script, "#!/bin/sh\nexec bigiris --dialog=batch \"$@\"\n").unwrap();
        install_with_system_path(&paths, Path::new("/nonexistent/system-path")).unwrap();
        uninstall(&paths).unwrap();
        assert!(script.exists());
    }

    #[test]
    fn python_extension_has_expected_hooks() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        install_with_system_path(&paths, Path::new("/nonexistent/system-path")).unwrap();
        let ext = paths.nautilus_python_extensions().join(PYTHON_EXTENSION_FILENAME);
        let body = std::fs::read_to_string(&ext).unwrap();
        assert!(body.contains("class BigIrisMenuProvider"));
        assert!(body.contains("Nautilus.MenuProvider"));
        assert!(body.contains("_TOP_LABEL = \"BigIris\""));
        assert!(body.contains("--dialog=convert"));
    }

    #[test]
    fn extension_table_comes_from_actions() {
        let body = render();
        assert!(PYTHON_EXTENSION.contains(ACTIONS_PLACEHOLDER));
        assert!(!body.contains(ACTIONS_PLACEHOLDER));
        for action in ACTIONS {
            assert_eq!(
                body.matches(&format!("    (\"{}\", ", action.id)).count(),
                1,
                "{}",
                action.id
            );
        }
        assert!(body.contains(
            "(\"web-letterbox-yt\", \"web\", \"For the web\", \"applications-internet-symbolic\", \
             \"Letterbox YouTube post (1:1)\", [\"--notify\", \"extent\", \"--aspect\", \"1:1\", \"--bg\", \
             \"0d0e12\"], \"applications-internet-symbolic\", [\"image/*\"]),"
        ));
        assert!(body.contains("(\"view\", None, None, None, \"View in BigIris\", [], "));
        assert!(!body.chars().any(|c| c.is_control() && c != '\n'));
    }

    #[test]
    fn uninstall_removes_python_extension() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        install_with_system_path(&paths, Path::new("/nonexistent/system-path")).unwrap();
        let removed = uninstall(&paths).unwrap();
        assert!(!removed.is_empty());
        assert!(!paths.nautilus_python_extensions().join(PYTHON_EXTENSION_FILENAME).exists());
    }

    #[test]
    fn uninstall_removes_only_our_bytecode_from_shared_pycache() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        install_with_system_path(&paths, Path::new("/nonexistent/system-path")).unwrap();
        let pycache = paths.nautilus_python_extensions().join("__pycache__");
        std::fs::create_dir_all(&pycache).unwrap();
        let ours = pycache.join("bigiris-menu.cpython-313.pyc");
        let ours_opt = pycache.join("bigiris-menu.cpython-313.opt-1.pyc");
        let other = pycache.join("other-extension.cpython-313.pyc");
        let lookalike = pycache.join("bigiris-menu-extra.cpython-313.pyc");
        let not_bytecode = pycache.join("bigiris-menu.txt");
        for f in [&ours, &ours_opt, &other, &lookalike, &not_bytecode] {
            std::fs::write(f, b"pyc").unwrap();
        }
        let dir = pycache.join("bigiris-menu.cpython-312.pyc");
        std::fs::create_dir(&dir).unwrap();
        let link = pycache.join("bigiris-menu.cpython-314.pyc");
        std::os::unix::fs::symlink(&other, &link).unwrap();
        uninstall(&paths).unwrap();
        assert!(!ours.exists());
        assert!(!ours_opt.exists());
        assert!(other.exists(), "another extension's bytecode was deleted");
        assert!(lookalike.exists());
        assert!(not_bytecode.exists());
        assert!(dir.is_dir());
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
    }

    #[test]
    fn user_install_over_system_drops_only_our_bytecode() {
        let tmp = TempDir::new().unwrap();
        let host = tmp.path().join("host-extension.py");
        std::fs::write(&host, "system copy").unwrap();
        let paths = ScopePaths::rooted_at(tmp.path().join("user"));
        let pycache = paths.nautilus_python_extensions().join("__pycache__");
        std::fs::create_dir_all(&pycache).unwrap();
        let ours = pycache.join("bigiris-menu.cpython-313.pyc");
        let other = pycache.join("other-extension.cpython-313.pyc");
        std::fs::write(&ours, b"pyc").unwrap();
        std::fs::write(&other, b"pyc").unwrap();
        install_with_system_path(&paths, &host).unwrap();
        assert!(!ours.exists());
        assert!(other.exists());
    }

    #[test]
    fn uninstall_drops_pycache_dir_once_it_is_empty() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let pycache = paths.nautilus_python_extensions().join("__pycache__");
        std::fs::create_dir_all(&pycache).unwrap();
        std::fs::write(pycache.join("bigiris-menu.cpython-313.pyc"), b"pyc").unwrap();
        uninstall(&paths).unwrap();
        assert!(!pycache.exists());
    }

    #[test]
    fn uninstall_on_empty_dir_is_noop() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let removed = uninstall(&paths).unwrap();
        assert!(removed.is_empty());
    }
}
