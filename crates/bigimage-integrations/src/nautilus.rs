// SPDX-License-Identifier: GPL-3.0-or-later
//! Nautilus — integração via `nautilus-python` extension.
//!
//! Historicamente instalávamos também um "scripts tree" em
//! `~/.local/share/nautilus/scripts/BigIris/` como fallback para quem
//! não tinha `nautilus-python` — mas isso gerava **duas entradas
//! distintas** no menu de clique-direito ("Scripts ▸ BigIris" e
//! "Íris ▸"), confundindo o usuário. A partir do BigLinux atual o
//! `nautilus-python` é dependência básica; dropamos o fallback e
//! mantemos apenas a extension Python como ponto único de entrada.
//!
//! O `uninstall` ainda limpa a árvore antiga caso ela esteja presente
//! de instalações anteriores — garante que usuários que rodaram
//! `install-integrations --user` numa versão velha e agora correm a
//! nova ficam com o menu limpo.
//!
//! Caminho único: `~/.local/share/nautilus-python/extensions/bigiris-menu.py`.
//! Submenu "Íris ▸" aparece top-level no clique-direito.

use std::io;
use std::path::{Path, PathBuf};

use crate::scope::{Scope, ScopePaths};

const STALE_TREE_ROOT: &str = "BigIris";
const PYTHON_EXTENSION_FILENAME: &str = "bigiris-menu.py";

/// System-wide install path for the Python extension. nautilus-python
/// loads from BOTH `/usr/share/nautilus-python/extensions/` AND
/// `~/.local/share/nautilus-python/extensions/` simultaneously, so a
/// user-scope install on top of a packaged system-wide one shows the
/// "BigIris ▸" submenu twice in the right-click menu.
const SYSTEM_EXTENSION_PATH: &str = "/usr/share/nautilus-python/extensions/bigiris-menu.py";

/// The Python extension body, embedded at compile time from
/// `data/nautilus/bigiris-menu.py` at the repo root.
const PYTHON_EXTENSION: &str = include_str!("../../../data/nautilus/bigiris-menu.py");

/// Instala a extension Python e limpa resíduos de uma eventual árvore
/// scripts legada (quem rodou `install-integrations` em versões
/// anteriores ficou com os dois, gerando menus duplicados).
///
/// **Detecção do system-wide:** se o pacote já instalou a extension em
/// [`SYSTEM_EXTENSION_PATH`] e este install é user-scope (ou seja, o
/// caminho de destino está sob o `$HOME`), pulamos a cópia user-scope
/// para evitar o "BigIris ▸" duplicado. Limpa também `__pycache__` e
/// qualquer `.py` órfão que ficou de uma instalação user antiga
/// pré-pacote — `nautilus-python` carrega bytecode mesmo sem o `.py`
/// correspondente, e isso ressuscitava a extensão após `uninstall`.
pub fn install(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    install_with_system_path(paths, Path::new(SYSTEM_EXTENSION_PATH))
}

/// Same as [`install`] but takes the system-wide extension path as a
/// parameter so tests can point it at a non-existent file (and so a
/// future flag like `BIGIRIS_NAUTILUS_SYSTEM_PATH` can override it).
pub(crate) fn install_with_system_path(
    paths: &ScopePaths,
    system_ext: &Path,
) -> io::Result<Vec<PathBuf>> {
    let mut written = Vec::new();

    // Garbage collect: scripts tree antiga fica limpa antes de instalar
    // para não aparecer no menu Scripts → BigIris.
    let stale_root = paths.nautilus_scripts().join(STALE_TREE_ROOT);
    if stale_root.exists() {
        let _ = std::fs::remove_dir_all(&stale_root);
    }

    let ext_dir = paths.nautilus_python_extensions();

    // User-scope sobre um system-wide já presente = menu duplicado.
    // DESTDIR may share the same prefix as home; use the explicit scope.
    let is_user_scope = paths.scope == Scope::User;
    if is_user_scope && system_ext.exists() {
        // Limpa qualquer resíduo user-scope (.py, .pyc/__pycache__) que
        // possa ressuscitar a duplicação. Ignora erros — a próxima
        // execução tenta de novo.
        let stale_py = ext_dir.join(PYTHON_EXTENSION_FILENAME);
        if stale_py.exists() {
            let _ = std::fs::remove_file(&stale_py);
        }
        let pycache = ext_dir.join("__pycache__");
        if pycache.exists() {
            let _ = std::fs::remove_dir_all(&pycache);
        }
        return Ok(written);
    }

    // nautilus-python extension: único ponto de entrada oficial.
    std::fs::create_dir_all(&ext_dir)?;
    let ext_file = ext_dir.join(PYTHON_EXTENSION_FILENAME);
    crate::safe_fs::safe_write(&ext_file, PYTHON_EXTENSION)?;
    written.push(ext_file);

    Ok(written)
}

/// Remove a extension Python. Também limpa a scripts tree antiga caso
/// ela tenha sobrevivido de uma instalação anterior, e o `__pycache__`
/// que `nautilus-python` mantém ao lado do `.py` (caso contrário o
/// bytecode persiste e a extensão volta a aparecer no menu).
pub fn uninstall(paths: &ScopePaths) -> io::Result<Vec<PathBuf>> {
    let mut removed = Vec::new();

    let stale_root = paths.nautilus_scripts().join(STALE_TREE_ROOT);
    if stale_root.exists() {
        removed.extend(collect_files(&stale_root)?);
        std::fs::remove_dir_all(&stale_root)?;
    }

    let ext_dir = paths.nautilus_python_extensions();
    let ext_file = ext_dir.join(PYTHON_EXTENSION_FILENAME);
    if ext_file.exists() {
        std::fs::remove_file(&ext_file)?;
        removed.push(ext_file);
    }
    let pycache = ext_dir.join("__pycache__");
    if pycache.exists() {
        // Best-effort: pyc deletion failure shouldn't abort the
        // uninstall (the .py is already gone, which is what matters
        // for nautilus's next scan).
        let _ = std::fs::remove_dir_all(&pycache);
    }

    Ok(removed)
}

fn collect_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            out.extend(collect_files(&path)?);
        } else {
            out.push(path);
        }
    }
    Ok(out)
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
            install_with_system_path(&paths, Path::new("/nonexistent/system-path")).unwrap();
        // Um único arquivo: a extension Python. Scripts tree não é mais
        // instalada (antes eram ACTIONS.len() + 1 arquivos).
        assert_eq!(written.len(), 1);
        let ext = paths.nautilus_python_extensions().join(PYTHON_EXTENSION_FILENAME);
        assert!(ext.exists(), "extensão python não instalada");
    }

    #[test]
    fn packaging_keeps_extension_even_when_host_already_has_it() {
        let tmp = TempDir::new().unwrap();
        let host = tmp.path().join("host-extension.py");
        std::fs::write(&host, "host stays unchanged").unwrap();
        let paths = ScopePaths::system_with_destdir(tmp.path().join("package"));
        let written = install_with_system_path(&paths, &host).unwrap();
        assert_eq!(written.len(), 1);
        assert_eq!(std::fs::read_to_string(&written[0]).unwrap(), PYTHON_EXTENSION);
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
        std::fs::write(&ext, "obsolete personal extension").unwrap();
        assert!(install_with_system_path(&paths, &host).unwrap().is_empty());
        assert!(!ext.exists());
        assert_eq!(std::fs::read_to_string(&host).unwrap(), "host stays unchanged");
    }

    #[test]
    fn install_cleans_stale_scripts_tree() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        // Simula um install antigo com scripts tree.
        let stale = paths.nautilus_scripts().join(STALE_TREE_ROOT);
        std::fs::create_dir_all(stale.join("Converter")).unwrap();
        std::fs::write(stale.join("Converter").join("PNG"), "#!/bin/sh\necho hi\n").unwrap();
        assert!(stale.exists());

        install_with_system_path(&paths, Path::new("/nonexistent/system-path")).unwrap();
        assert!(!stale.exists(), "scripts tree antiga deveria ter sido limpa");
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
    fn uninstall_removes_python_extension_and_legacy_tree() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        install_with_system_path(&paths, Path::new("/nonexistent/system-path")).unwrap();
        // Adiciona uma legacy tree pra simular um user com dois installs.
        let stale = paths.nautilus_scripts().join(STALE_TREE_ROOT);
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("dummy"), b"x").unwrap();

        let removed = uninstall(&paths).unwrap();
        assert!(!removed.is_empty());
        assert!(!paths.nautilus_python_extensions().join(PYTHON_EXTENSION_FILENAME).exists());
        assert!(!stale.exists());
    }

    #[test]
    fn uninstall_on_empty_dir_is_noop() {
        let tmp = TempDir::new().unwrap();
        let paths = ScopePaths::rooted_at(tmp.path());
        let removed = uninstall(&paths).unwrap();
        assert!(removed.is_empty());
    }
}
