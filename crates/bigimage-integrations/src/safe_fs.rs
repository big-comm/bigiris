// SPDX-License-Identifier: GPL-3.0-or-later
//! Crash-safe file replacement that never writes through a symlink.
//!
//! Used by every per-file-manager installer that writes into
//! `/usr/share/...` (or `~/.local/share/...`, `~/.config/...`). Two
//! properties matter:
//!
//! 1. **No write-through symlinks.** If an attacker plants a symlink at one
//!    of our target names before `sudo --system` runs, plain
//!    `std::fs::write` would rewrite whatever the link points at. We refuse
//!    a symlink at the final component with `ELOOP`, and the replacement is
//!    a `rename(2)`, which swaps the directory entry itself and never
//!    follows a link even if one is planted after the check.
//! 2. **Atomic replacement.** Some targets are shared with the user (Thunar's
//!    `uca.xml` holds every custom action they created). Truncating in place
//!    and then crashing — or hitting a full disk — would lose that data. We
//!    write a sibling temporary file created with `O_EXCL`, `fsync` it, and
//!    `rename` it over the target, so readers see either the old or the new
//!    file, never a partial one.

use std::io::{self, Write};
use std::path::Path;

/// `ELOOP` on Linux — what `open(O_NOFOLLOW)` reports for a symlink.
const ELOOP: i32 = 40;

pub(crate) fn safe_write(path: &Path, content: impl AsRef<[u8]>) -> io::Result<()> {
    safe_write_with_mode(path, content, 0o644)
}

/// Atomically replace `path` with `content` and an explicit Unix mode.
///
/// Most integration metadata uses `0644`. User-owned KDE ServiceMenus are
/// the exception: Dolphin deliberately ignores them unless they carry an
/// executable bit, so their installer requests `0755`.
pub(crate) fn safe_write_with_mode(
    path: &Path,
    content: impl AsRef<[u8]>,
    mode: u32,
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(io::Error::from_raw_os_error(ELOOP));
        }
        Ok(meta) if !meta.is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", path.display()),
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let stem = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // tempfile creates the sibling with O_CREAT|O_EXCL and a random suffix,
    // so it can never open (or follow) a pre-existing entry. The leading dot
    // keeps half-written files out of the file managers' directory scans.
    let mut tmp =
        tempfile::Builder::new().prefix(&format!(".{stem}.")).suffix(".tmp").tempfile_in(dir)?;
    tmp.write_all(content.as_ref())?;
    tmp.flush()?;
    // Creation mode is filtered by the umask (and tempfile uses 0600); set
    // the requested mode explicitly so metadata is never group/world
    // writable and Dolphin service menus keep their executable bit.
    tmp.as_file().set_permissions(std::fs::Permissions::from_mode(mode))?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    // Make the rename itself durable. Best effort: some filesystems refuse
    // fsync on a directory handle, and the data is already consistent.
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn writes_to_fresh_path() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("a.txt");
        safe_write(&p, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o644);
        assert!(leftovers(tmp.path()).is_empty());
    }

    #[test]
    fn replaces_existing_regular_file() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("a.txt");
        std::fs::write(&p, "first content here").unwrap();
        safe_write(&p, "shorter").unwrap();
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
        use std::os::unix::fs::MetadataExt;
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("uca.xml");
        std::fs::write(&p, "old complete content").unwrap();
        let before = std::fs::metadata(&p).unwrap().ino();
        let mut held = std::fs::File::open(&p).unwrap();
        safe_write(&p, "new").unwrap();
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
        let result = safe_write(&p, "replacement");
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
        safe_write_with_mode(&p, "new", 0o755).unwrap();

        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o755);
    }

    #[test]
    fn refuses_to_follow_symlink_at_target() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real.txt");
        let link = tmp.path().join("link.txt");
        std::fs::write(&real, "untouched").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = safe_write(&link, "attacker payload").unwrap_err();
        // ELOOP mirrors what open(O_NOFOLLOW) reports. What matters is that
        // we did NOT clobber the file behind the link, nor the link itself.
        assert_eq!(err.raw_os_error(), Some(ELOOP), "expected ELOOP, got {err:?}");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "untouched");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(leftovers(tmp.path()).is_empty());
    }
}
