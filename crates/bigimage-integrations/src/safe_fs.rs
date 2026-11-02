// SPDX-License-Identifier: GPL-3.0-or-later
//! Crash-safe file replacement that never writes through a planted symlink.
//!
//! Used by every per-file-manager installer that writes into
//! `/usr/share/...` (or `~/.local/share/...`, `~/.config/...`). Three
//! properties matter:
//!
//! 1. **No write-through symlinks in system scope.** If an attacker plants
//!    a symlink at one of our target names before `sudo --system` runs,
//!    plain `std::fs::write` would rewrite whatever the link points at. In
//!    [`Scope::System`] we refuse a symlink at the final component with
//!    `ELOOP`, and the replacement is a `rename(2)`, which swaps the
//!    directory entry itself and never follows a link even if one is
//!    planted after the check. In [`Scope::User`] the only party who can
//!    plant links in `~/.config` is the user, and dotfile managers (GNU
//!    stow, home-manager, chezmoi in symlink mode) link files such as
//!    `uca.xml` on purpose. There we resolve the link and atomically
//!    replace the real file in *its* directory, keeping the link intact,
//!    but only when that file is a regular file owned by the same uid that
//!    is running the install.
//! 2. **Atomic replacement.** Some targets are shared with the user (Thunar's
//!    `uca.xml` holds every custom action they created). Truncating in place
//!    and then crashing — or hitting a full disk — would lose that data. We
//!    write a sibling temporary file created with `O_EXCL`, `fsync` it, and
//!    `rename` it over the target, so readers see either the old or the new
//!    file, never a partial one.
//! 3. **Mode.** Generated files get an exact mode. User data files (again,
//!    `uca.xml`) keep the mode they already had: the rename installs a new
//!    inode, which would otherwise silently widen a private `0600` file.

use std::fs::Metadata;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::scope::Scope;

/// `ELOOP` on Linux — what `open(O_NOFOLLOW)` reports for a symlink.
const ELOOP: i32 = 40;

/// Permission policy for the replacement file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileMode {
    /// Always apply this mode, also normalizing an existing file. Used for
    /// generated metadata, which must never be group/world writable and,
    /// for user-owned Dolphin service menus, must stay executable.
    Exact(u32),
    /// Keep the permission bits of the file being replaced (setuid, setgid
    /// and sticky bits are dropped); use this mode only for a new file.
    KeepExisting(u32),
}

/// Atomically write `content` to `path` with mode `0644`.
pub(crate) fn safe_write(path: &Path, content: impl AsRef<[u8]>, scope: Scope) -> io::Result<()> {
    safe_write_with_mode(path, content, scope, FileMode::Exact(0o644))
}

/// Atomically replace `path` with `content` under the given [`FileMode`].
///
/// Most integration metadata uses `0644`. User-owned KDE ServiceMenus are
/// the exception: Dolphin deliberately ignores them unless they carry an
/// executable bit, so their installer requests `0755`.
pub(crate) fn safe_write_with_mode(
    path: &Path,
    content: impl AsRef<[u8]>,
    scope: Scope,
    mode: FileMode,
) -> io::Result<()> {
    let target = resolve_target(path, scope)?;

    let dir = match target.path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let stem =
        target.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // tempfile creates the sibling with O_CREAT|O_EXCL and a random suffix,
    // so it can never open (or follow) a pre-existing entry. The leading dot
    // keeps half-written files out of the file managers' directory scans.
    let mut tmp =
        tempfile::Builder::new().prefix(&format!(".{stem}.")).suffix(".tmp").tempfile_in(&dir)?;
    if target.via_symlink {
        // The temp file is owned by our effective uid, which makes it the
        // reference for "same user": never replace someone else's file just
        // because a link in our home points at it. Dropping `tmp` on the
        // error path deletes it.
        let ours = tmp.as_file().metadata()?.uid();
        let theirs = target.existing.as_ref().map_or(ours, MetadataExt::uid);
        if ours != theirs {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is a symlink to {}, which belongs to another user",
                    path.display(),
                    target.path.display()
                ),
            ));
        }
    }
    tmp.write_all(content.as_ref())?;
    tmp.flush()?;
    // Creation mode is filtered by the umask (and tempfile uses 0600); set
    // the resolved mode explicitly.
    let bits = match mode {
        FileMode::Exact(m) => m,
        FileMode::KeepExisting(default) => {
            target.existing.as_ref().map_or(default, |m| m.mode() & 0o777)
        }
    };
    tmp.as_file().set_permissions(std::fs::Permissions::from_mode(bits))?;
    tmp.as_file().sync_all()?;
    tmp.persist(&target.path).map_err(|e| e.error)?;
    // Make the rename itself durable. Best effort: some filesystems refuse
    // fsync on a directory handle, and the data is already consistent.
    if let Ok(d) = std::fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Where the replacement is renamed to, plus what it replaces.
struct Target {
    path: PathBuf,
    existing: Option<Metadata>,
    via_symlink: bool,
}

fn resolve_target(path: &Path, scope: Scope) -> io::Result<Target> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Target { path: path.to_path_buf(), existing: None, via_symlink: false });
        }
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        if scope == Scope::System {
            return Err(io::Error::from_raw_os_error(ELOOP));
        }
        let real = std::fs::canonicalize(path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("{} is a symlink that cannot be resolved: {e}", path.display()),
            )
        })?;
        let real_meta = std::fs::symlink_metadata(&real)?;
        if !real_meta.is_file() {
            return Err(not_regular(&real));
        }
        return Ok(Target { path: real, existing: Some(real_meta), via_symlink: true });
    }
    if !meta.is_file() {
        return Err(not_regular(path));
    }
    Ok(Target { path: path.to_path_buf(), existing: Some(meta), via_symlink: false })
}

fn not_regular(path: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a regular file", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const SYS: Scope = Scope::System;
    const USER: Scope = Scope::User;

    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn writes_to_fresh_path() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("a.txt");
        safe_write(&p, "hello", SYS).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello");
        assert_eq!(mode_of(&p), 0o644);
        assert!(leftovers(tmp.path()).is_empty());
    }

    #[test]
    fn replaces_existing_regular_file() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("a.txt");
        std::fs::write(&p, "first content here").unwrap();
        safe_write(&p, "shorter", SYS).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "shorter");
        assert!(leftovers(tmp.path()).is_empty());
    }

    #[test]
    fn replacement_is_a_new_inode_not_an_in_place_truncate() {
        // An in-place truncate keeps the inode; a crash between truncate
        // and write would leave an empty file. A rename-based replacement
        // swaps in a fully written inode, so a reader holding the old file
        // still sees the complete old content.
        use std::io::Read;
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("uca.xml");
        std::fs::write(&p, "old complete content").unwrap();
        let before = std::fs::metadata(&p).unwrap().ino();
        let mut held = std::fs::File::open(&p).unwrap();
        safe_write(&p, "new", SYS).unwrap();
        assert_ne!(std::fs::metadata(&p).unwrap().ino(), before);
        let mut old = String::new();
        held.read_to_string(&mut old).unwrap();
        assert_eq!(old, "old complete content");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
    }

    #[test]
    fn failed_write_leaves_original_untouched() {
        // In a read-only directory the sibling temp cannot be created, so
        // the write fails before touching the target (root bypasses the
        // permission check; then the write simply succeeds).
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("ro");
        std::fs::create_dir(&dir).unwrap();
        let p = dir.join("uca.xml");
        std::fs::write(&p, "precious").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = safe_write(&p, "replacement", SYS);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if result.is_err() {
            assert_eq!(std::fs::read_to_string(&p).unwrap(), "precious");
        }
        assert!(leftovers(&dir).is_empty());
    }

    #[test]
    fn explicit_mode_normalizes_existing_file() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("action.desktop");
        std::fs::write(&p, "old").unwrap();
        safe_write_with_mode(&p, "new", SYS, FileMode::Exact(0o755)).unwrap();

        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
        assert_eq!(mode_of(&p), 0o755);
    }

    #[test]
    fn keep_existing_preserves_private_mode_without_special_bits() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("uca.xml");
        std::fs::write(&p, "old").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o4600)).unwrap();
        safe_write_with_mode(&p, "new", USER, FileMode::KeepExisting(0o644)).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
        assert_eq!(mode_of(&p), 0o600, "mode widened or setuid kept");
    }

    #[test]
    fn keep_existing_uses_default_for_new_file() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("uca.xml");
        safe_write_with_mode(&p, "new", USER, FileMode::KeepExisting(0o600)).unwrap();
        assert_eq!(mode_of(&p), 0o600);
    }

    #[test]
    fn refuses_to_follow_symlink_at_target_in_system_scope() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real.txt");
        let link = tmp.path().join("link.txt");
        std::fs::write(&real, "untouched").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = safe_write(&link, "attacker payload", SYS).unwrap_err();
        // ELOOP mirrors what open(O_NOFOLLOW) reports. What matters is that
        // we did NOT clobber the file behind the link, nor the link itself.
        assert_eq!(err.raw_os_error(), Some(ELOOP), "expected ELOOP, got {err:?}");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "untouched");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(leftovers(tmp.path()).is_empty());
    }

    #[test]
    fn user_scope_replaces_real_file_behind_own_symlink() {
        // Dotfile managers (stow, home-manager) symlink config files.
        let tmp = TempDir::new().unwrap();
        let dotfiles = tmp.path().join("dotfiles");
        std::fs::create_dir(&dotfiles).unwrap();
        let real = dotfiles.join("uca.xml");
        std::fs::write(&real, "old").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = tmp.path().join("uca.xml");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        safe_write_with_mode(&link, "new", USER, FileMode::KeepExisting(0o644)).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(mode_of(&real), 0o600);
        assert!(leftovers(&dotfiles).is_empty());
        assert!(leftovers(tmp.path()).is_empty());
    }

    #[test]
    fn user_scope_refuses_dangling_or_non_file_symlink() {
        let tmp = TempDir::new().unwrap();
        let dangling = tmp.path().join("dangling.xml");
        std::os::unix::fs::symlink(tmp.path().join("missing"), &dangling).unwrap();
        assert!(safe_write(&dangling, "x", USER).is_err());
        assert!(!tmp.path().join("missing").exists());

        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let dir_link = tmp.path().join("dir.xml");
        std::os::unix::fs::symlink(&sub, &dir_link).unwrap();
        assert!(safe_write(&dir_link, "x", USER).is_err());
        assert!(leftovers(tmp.path()).is_empty());
        assert!(leftovers(&sub).is_empty());
    }
}
