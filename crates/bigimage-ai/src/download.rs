// SPDX-License-Identifier: GPL-3.0-or-later
//! Download manager for IA model files.
//!
//! Every model we ship has a pinned `url` + `sha256`. Downloads are
//! atomic: we write to a unique temporary sibling, verify the hash, and only rename
//! into place when everything matches. Re-running over an already-valid
//! file is a no-op: after the first full hash we keep a
//! `<model>.onnx.verified` stamp (hash + size + device/inode + mtime +
//! ctime) and re-hash only when the file's identity no longer matches it.
//! Any write, truncate, chmod, utime or replacement changes ctime or the
//! inode, so accidental or out-of-band modification is still caught. The
//! stamp lives in the same user-owned directory as the model; whoever can
//! forge it can already replace anything else the user runs.
//!
//! Partial downloads abandoned by a crash are deleted on the next
//! [`ensure`] once they are older than six hours.
//!
//! Hard-coded to `ureq` + blocking IO: simple, no runtime dependency,
//! fits the rare "first-time setup" moment this runs in.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;

/// Catalogue entry — immutable description of a downloadable model.
#[derive(Debug, Clone, Copy)]
pub struct ModelSource {
    /// Stable human ID (`"birefnet-lite"`). Used as file stem locally.
    pub id: &'static str,
    /// Absolute URL to the `.onnx` file. Pin a specific revision SHA
    /// or `resolve/main/…` on HuggingFace so the hash is stable.
    pub url: &'static str,
    /// SPDX license identifier for UI display. Loaders refuse to run
    /// models whose license isn't on an allowlist (see
    /// `allowed_licenses`).
    pub license_spdx: &'static str,
    /// Expected SHA-256 of the `.onnx` bytes. Verified byte-by-byte
    /// as we stream — mismatch deletes the partial.
    pub sha256: &'static str,
    /// Expected size in bytes — used for the progress bar and as a
    /// second-layer sanity check.
    pub size_bytes: u64,
    /// Short human description shown in "baixar modelo?" prompts.
    pub description: &'static str,
}

/// Errors produced by the download manager.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DownloadError {
    /// IO failure (filesystem, network transport).
    #[error("io: {0}")]
    Io(#[source] std::io::Error),
    /// HTTP request failed (non-2xx, DNS, TLS, …).
    #[error("http: {0}")]
    Http(String),
    /// Downloaded bytes didn't match the expected SHA-256.
    #[error("hash mismatch: expected {expected:.12}…, got {actual:.12}…")]
    HashMismatch {
        /// Expected hex digest from the catalogue.
        expected: String,
        /// Actual digest we computed over the downloaded bytes.
        actual: String,
    },
    /// Could not locate the user's cache directory (`$XDG_DATA_HOME` etc.).
    #[error("não foi possível localizar o diretório de modelos")]
    NoCacheDir,
    /// The model's license isn't on our FOSS allowlist — refused at load.
    #[error("licença {0} não está na allowlist FOSS")]
    LicenseNotAllowed(String),
}

/// Licenses we accept for shipped models. Keeps non-commercial /
/// proprietary weights out of the supply chain even if somebody
/// adds them to the catalogue accidentally.
pub fn allowed_licenses() -> &'static [&'static str] {
    &["MIT", "Apache-2.0", "BSD-3-Clause", "BSD-2-Clause", "MPL-2.0", "CC0-1.0"]
}

/// Absolute path where `model.id.onnx` would live on this host.
/// Doesn't create any directories — [`ensure`] handles that.
pub fn local_path(model: &ModelSource) -> Result<PathBuf, DownloadError> {
    let dirs = directories::ProjectDirs::from("com", "biglinux", "Iris")
        .ok_or(DownloadError::NoCacheDir)?;
    let models_dir = dirs.data_dir().join("models");
    Ok(models_dir.join(format!("{}.onnx", model.id)))
}

/// Ensure the model is available on disk. Returns the path; downloads
/// if needed. `progress` is called with `(current, total)` bytes during
/// streaming so callers can render a bar.
pub fn ensure(
    model: &ModelSource,
    mut progress: impl FnMut(u64, u64),
) -> Result<PathBuf, DownloadError> {
    if !allowed_licenses().contains(&model.license_spdx) {
        return Err(DownloadError::LicenseNotAllowed(model.license_spdx.to_string()));
    }

    let dest = local_path(model)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(DownloadError::Io)?;
    }

    // Happy path: already downloaded and verified (stamp or full hash).
    if dest.exists() && cached_model_is_valid(&dest, model.sha256) {
        progress(model.size_bytes, model.size_bytes);
        return Ok(dest);
    }

    let models_dir = dest.parent().ok_or(DownloadError::NoCacheDir)?;
    remove_stale_partials(models_dir, STALE_PARTIAL_AGE);

    let mut partial = tempfile::Builder::new()
        .prefix(PARTIAL_PREFIX)
        .suffix(PARTIAL_SUFFIX)
        .tempfile_in(models_dir)
        .map_err(DownloadError::Io)?;
    download_stream(model.url, partial.as_file_mut(), model.size_bytes, &mut progress)?;
    let actual = file_sha256(partial.path()).map_err(DownloadError::Io)?;
    if !actual.eq_ignore_ascii_case(model.sha256) {
        return Err(DownloadError::HashMismatch { expected: model.sha256.to_string(), actual });
    }
    partial.as_file().sync_all().map_err(DownloadError::Io)?;
    partial.persist(&dest).map_err(|e| DownloadError::Io(e.error))?;
    // Metadata is read after the rename (which updates ctime), so the stamp
    // matches the file as it now sits in the cache.
    if let Ok(meta) = std::fs::metadata(&dest) {
        write_stamp(&dest, &meta, model.sha256);
    }
    Ok(dest)
}

/// Prefix/suffix of the exclusive temporary files [`ensure`] downloads into.
const PARTIAL_PREFIX: &str = ".bigiris-model-";
const PARTIAL_SUFFIX: &str = ".part";

/// A `.part` file untouched for this long belongs to a download that died
/// (crash, power loss, killed process) and will never be finished: every
/// download writes to a fresh unique file. A live download rewrites its
/// file every 64 KiB and gives up after 60 s without data (`timeout_read`),
/// so its mtime is never anywhere near this old; a concurrent download in
/// another process is therefore never deleted, no lock needed.
const STALE_PARTIAL_AGE: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// Delete abandoned partial downloads (each up to ~224 MB) left in `dir`.
/// Best effort: errors are ignored, a later call retries.
fn remove_stale_partials(dir: &Path, max_age: std::time::Duration) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return removed;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with(PARTIAL_PREFIX) && name.ends_with(PARTIAL_SUFFIX)) {
            continue;
        }
        // DirEntry::metadata does not follow symlinks; only plain files.
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let stale = meta
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= max_age);
        if stale && std::fs::remove_file(entry.path()).is_ok() {
            tracing::info!(path = %entry.path().display(), "removed abandoned model download");
            removed.push(entry.path());
        }
    }
    removed
}

/// Path of the verification stamp kept next to a cached model.
fn stamp_path(model: &Path) -> PathBuf {
    let mut name = model.file_name().unwrap_or_default().to_os_string();
    name.push(".verified");
    model.with_file_name(name)
}

/// Identity of the exact file we hashed. `ctime` cannot be set from user
/// space and changes on every write, truncate, chmod or utime, and the
/// device/inode pair changes when the file is replaced, so any change to
/// the model after verification invalidates the stamp and forces a full
/// re-hash. Without the stamp every `BgSession::new` re-hashed 224 MB.
fn stamp_contents(meta: &std::fs::Metadata, sha256: &str) -> String {
    use std::os::unix::fs::MetadataExt;
    format!(
        "sha256={}\nsize={}\ndev={}\nino={}\nmtime={}.{:09}\nctime={}.{:09}\n",
        sha256.to_ascii_lowercase(),
        meta.size(),
        meta.dev(),
        meta.ino(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}

/// Record that `model` (described by `meta`) hashed to `sha256`. Best
/// effort: without a stamp the next call just hashes again.
fn write_stamp(model: &Path, meta: &std::fs::Metadata, sha256: &str) {
    let path = stamp_path(model);
    let Some(dir) = path.parent() else { return };
    let written =
        tempfile::Builder::new().prefix(".bigiris-stamp-").tempfile_in(dir).and_then(|mut tmp| {
            tmp.write_all(stamp_contents(meta, sha256).as_bytes())?;
            tmp.persist(&path).map_err(|e| e.error)?;
            Ok(())
        });
    if let Err(error) = written {
        tracing::debug!(%error, "could not record model verification stamp");
    }
}

/// Is the cached `model` the verified file? Trust a stamp that matches the
/// file's current identity and the expected hash; otherwise hash the whole
/// file and, if it matches, stamp it for next time.
fn cached_model_is_valid(model: &Path, sha256: &str) -> bool {
    let Ok(before) = std::fs::metadata(model) else { return false };
    if !before.is_file() {
        return false;
    }
    let expected = stamp_contents(&before, sha256);
    if std::fs::read_to_string(stamp_path(model)).is_ok_and(|stamp| stamp == expected) {
        return true;
    }
    if !verify_hash(model, sha256).unwrap_or(false) {
        return false;
    }
    // Only stamp if nothing changed while we were hashing.
    if let Ok(after) = std::fs::metadata(model) {
        if stamp_contents(&after, sha256) == expected {
            write_stamp(model, &after, sha256);
        }
    }
    true
}

/// Margin we tolerate over the catalogue's `expected_total` before
/// aborting a download. 16 MiB is enough to absorb an updated model
/// (a quantisation tweak that shifts a few MB) but small enough that a
/// hostile mirror can't fill the disk before SHA-256 verification.
const MAX_OVER_BYTES: u64 = 16 * 1024 * 1024;

fn https_url(url: &str) -> Result<url::Url, DownloadError> {
    let parsed = url::Url::parse(url).map_err(|e| DownloadError::Http(e.to_string()))?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(DownloadError::Http("download exige HTTPS sem credenciais na URL".to_string()));
    }
    Ok(parsed)
}

fn redirect_url(current: &url::Url, location: &str) -> Result<url::Url, DownloadError> {
    let joined = current.join(location).map_err(|e| DownloadError::Http(e.to_string()))?;
    https_url(joined.as_str())
}

fn download_response(url: &str) -> Result<ureq::Response, DownloadError> {
    let connector =
        ureq::native_tls::TlsConnector::new().map_err(|e| DownloadError::Http(e.to_string()))?;
    let agent = ureq::AgentBuilder::new()
        .tls_connector(std::sync::Arc::new(connector))
        .redirects(0)
        .timeout_connect(std::time::Duration::from_secs(15))
        .timeout_read(std::time::Duration::from_secs(60))
        .timeout_write(std::time::Duration::from_secs(60))
        .build();
    let mut current = https_url(url)?;
    for _ in 0..=10 {
        let response =
            agent.get(current.as_str()).call().map_err(|e| DownloadError::Http(e.to_string()))?;
        match response.status() {
            301 | 302 | 303 | 307 | 308 => {
                let location = response.header("Location").ok_or_else(|| {
                    DownloadError::Http("redirecionamento sem Location".to_string())
                })?;
                // Validate every hop before any request to its destination.
                current = redirect_url(&current, location)?;
            }
            200..=299 => return Ok(response),
            status => return Err(DownloadError::Http(format!("status HTTP inesperado: {status}"))),
        }
    }
    Err(DownloadError::Http("muitos redirecionamentos".to_string()))
}

/// Stream into an exclusively created temporary file; callers own cleanup.
fn download_stream(
    url: &str,
    file: &mut std::fs::File,
    expected_total: u64,
    progress: &mut impl FnMut(u64, u64),
) -> Result<(), DownloadError> {
    tracing::info!(%url, "baixando modelo");
    let resp = download_response(url)?;

    let total: u64 =
        resp.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(expected_total);

    // Hard cap the size budget at `expected_total + MAX_OVER_BYTES`. A hostile
    // mirror returning 50 GB of payload would otherwise fill the user's disk
    // before the SHA-256 check at the end gets to reject it.
    let budget = expected_total.saturating_add(MAX_OVER_BYTES);

    let mut reader = resp.into_reader();

    let mut buf = vec![0u8; 64 * 1024];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(DownloadError::Io)?;
        if n == 0 {
            break;
        }
        done = done.saturating_add(n as u64);
        if done > budget {
            return Err(DownloadError::Http(format!(
                "servidor enviou mais que o esperado ({} MB > limite de {} MB)",
                done / (1024 * 1024),
                budget / (1024 * 1024),
            )));
        }
        file.write_all(&buf[..n]).map_err(DownloadError::Io)?;
        progress(done, total);
    }
    file.flush().map_err(DownloadError::Io)?;
    Ok(())
}

/// Confirm `path` already hashes to `expected_hex` — used by the cache
/// short-circuit and by post-download verification.
fn verify_hash(path: &Path, expected_hex: &str) -> std::io::Result<bool> {
    let actual = file_sha256(path)?;
    Ok(actual.eq_ignore_ascii_case(expected_hex))
}

fn file_sha256(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn urls_refuse_plaintext_and_credentials_before_network_access() {
        assert!(https_url("http://example.org/model.onnx").is_err());
        assert!(https_url("https://user:password@example.org/model.onnx").is_err());
        let base = https_url("https://example.org/models/a").unwrap();
        assert!(redirect_url(&base, "http://example.org/model").is_err());
        assert_eq!(redirect_url(&base, "../model").unwrap().as_str(), "https://example.org/model");
        assert!(redirect_url(&base, "//user:password@example.org/model").is_err());
    }

    #[test]
    fn file_sha256_matches_known_sample() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.bin");
        std::fs::write(&path, b"hello, bigiris\n").unwrap();
        let digest = file_sha256(&path).unwrap();
        assert_eq!(digest, "4a30443c709da4a8901dcba8bd388efda3da7d23772b5d5d6c7c0e0c2c368744");
    }

    #[test]
    fn license_allowlist_rejects_nc() {
        assert!(!allowed_licenses().contains(&"CC-BY-NC-4.0"));
        assert!(allowed_licenses().contains(&"MIT"));
        assert!(allowed_licenses().contains(&"Apache-2.0"));
    }

    #[test]
    fn local_path_lives_under_project_data_dir() {
        let m = ModelSource {
            id: "probe",
            url: "http://invalid",
            license_spdx: "MIT",
            sha256: "00",
            size_bytes: 0,
            description: "",
        };
        let p = local_path(&m).unwrap();
        let s = p.to_string_lossy();
        assert!(s.ends_with("/models/probe.onnx"), "path={s}");
    }

    const SAMPLE_SHA: &str = "4a30443c709da4a8901dcba8bd388efda3da7d23772b5d5d6c7c0e0c2c368744";

    fn sample_model(dir: &Path) -> PathBuf {
        let path = dir.join("probe.onnx");
        std::fs::write(&path, b"hello, bigiris\n").unwrap();
        path
    }

    fn age(path: &Path, by: std::time::Duration) {
        let when = std::time::SystemTime::now() - by;
        std::fs::File::options().write(true).open(path).unwrap().set_modified(when).unwrap();
    }

    #[test]
    fn verified_model_gets_a_stamp() {
        let dir = TempDir::new().unwrap();
        let model = sample_model(dir.path());
        assert!(cached_model_is_valid(&model, SAMPLE_SHA));
        let stamp = std::fs::read_to_string(stamp_path(&model)).unwrap();
        assert_eq!(stamp, stamp_contents(&std::fs::metadata(&model).unwrap(), SAMPLE_SHA));
        assert!(cached_model_is_valid(&model, SAMPLE_SHA));
    }

    #[test]
    fn matching_stamp_skips_the_rehash() {
        // A stamp recorded for the file's exact identity is trusted without
        // reading the model: here it claims a hash the bytes don't have,
        // which only a no-rehash path can accept.
        let dir = TempDir::new().unwrap();
        let model = sample_model(dir.path());
        let meta = std::fs::metadata(&model).unwrap();
        write_stamp(&model, &meta, "deadbeef");
        assert!(cached_model_is_valid(&model, "deadbeef"));
        // A stamp for another hash is ignored and the file is hashed.
        assert!(!cached_model_is_valid(&model, "cafebabe"));
        assert!(cached_model_is_valid(&model, SAMPLE_SHA));
    }

    #[test]
    fn modified_model_invalidates_stamp_and_is_rehashed() {
        let dir = TempDir::new().unwrap();
        let model = sample_model(dir.path());
        assert!(cached_model_is_valid(&model, SAMPLE_SHA));
        // Same size, different bytes, original mtime restored: ctime still
        // moves, so the stamp no longer matches and the hash catches it.
        let mtime = std::fs::metadata(&model).unwrap().modified().unwrap();
        std::fs::write(&model, b"HELLO, bigiris\n").unwrap();
        std::fs::File::options().write(true).open(&model).unwrap().set_modified(mtime).unwrap();
        assert!(!cached_model_is_valid(&model, SAMPLE_SHA));
    }

    #[test]
    fn replaced_model_invalidates_stamp() {
        let dir = TempDir::new().unwrap();
        let model = sample_model(dir.path());
        assert!(cached_model_is_valid(&model, SAMPLE_SHA));
        let other = dir.path().join("other");
        std::fs::write(&other, b"tampered weights\n").unwrap();
        std::fs::rename(&other, &model).unwrap();
        assert!(!cached_model_is_valid(&model, SAMPLE_SHA));
    }

    #[test]
    fn stale_partials_are_removed_but_fresh_ones_and_models_are_kept() {
        let dir = TempDir::new().unwrap();
        let stale = dir.path().join(".bigiris-model-abc123.part");
        let live = dir.path().join(".bigiris-model-def456.part");
        let model = dir.path().join("birefnet-lite.onnx");
        let unrelated = dir.path().join("notes.part");
        for path in [&stale, &live, &model, &unrelated] {
            std::fs::write(path, b"x").unwrap();
        }
        let seven_hours = std::time::Duration::from_secs(7 * 60 * 60);
        for path in [&stale, &model, &unrelated] {
            age(path, seven_hours);
        }
        age(&live, std::time::Duration::from_secs(5 * 60));
        let removed = remove_stale_partials(dir.path(), STALE_PARTIAL_AGE);
        assert_eq!(removed, vec![stale.clone()]);
        assert!(!stale.exists());
        assert!(live.exists(), "an in-progress download was deleted");
        assert!(model.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn stale_partial_cleanup_ignores_symlinks_and_missing_dirs() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"x").unwrap();
        age(&target, std::time::Duration::from_secs(7 * 60 * 60));
        std::os::unix::fs::symlink(&target, dir.path().join(".bigiris-model-x.part")).unwrap();
        assert!(remove_stale_partials(dir.path(), STALE_PARTIAL_AGE).is_empty());
        assert!(target.exists());
        assert!(remove_stale_partials(&dir.path().join("missing"), STALE_PARTIAL_AGE).is_empty());
    }

    #[test]
    fn hash_mismatch_is_reported_when_expected_bytes_dont_line_up() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("x.bin");
        std::fs::write(&path, b"abc").unwrap();
        assert!(!verify_hash(&path, "deadbeef").unwrap());
    }
}
