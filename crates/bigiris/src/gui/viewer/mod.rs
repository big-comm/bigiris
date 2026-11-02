// SPDX-License-Identifier: GPL-3.0-or-later
//! Pure helpers behind the viewer window: film-strip geometry, the
//! freedesktop thumbnail cache lookup and a cross-thread wake-up used to
//! hand decoded frames back to the GTK loop without polling.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

pub(in crate::gui) mod crop;
#[cfg(test)]
mod gui_tests;

/// Thumbnails built at once in the film strip, independently of folder size.
pub(super) const FILM_STRIP_PAGE: usize = 41;

/// First index of the film-strip window that shows `idx`. The window is
/// centred on the current image and slides back near the end of the list,
/// so it is always full when the folder has enough images: a page that
/// held only the last file would make the strip look like a single image.
pub(super) fn film_strip_start(len: usize, idx: usize, page: usize) -> usize {
    if len <= page {
        return 0;
    }
    idx.saturating_sub(page / 2).min(len - page)
}

/// Size handed to `DynamicImage::thumbnail` for a film-strip cell whose
/// square side is `cover`. The short side covers the cell (the cell crops
/// to a square), the scale never exceeds 1:1 and the long side is capped,
/// so a 1 × 65535 image cannot become a multi-gigabyte buffer.
pub(super) fn thumbnail_dimensions(width: u32, height: u32, cover: u32) -> (u32, u32) {
    let max_long = cover.saturating_mul(4).max(1);
    let short = f64::from(width.min(height).max(1));
    let scale = (f64::from(cover) / short).min(1.0);
    let side = |value: u32| ((f64::from(value) * scale).ceil() as u32).clamp(1, max_long);
    (side(width), side(height))
}

/// Largest thumbnail file accepted from the shared cache. Spec sizes go up
/// to 1024 px; anything bigger is not a cache entry we want to read.
const CACHE_MAX_BYTES: u64 = 4 * 1024 * 1024;
const CACHE_MAX_EDGE: u32 = 1024;

/// A valid entry of the freedesktop thumbnail cache for `path`, if another
/// application already created one. Only the PNG chunk headers and the
/// `Thumb::URI` / `Thumb::MTime` text chunks are read here; the pixels are
/// decoded by the caller through the same sandboxed path as any other file,
/// because the cache is written by other programs and is untrusted input.
pub(super) fn cached_thumbnail(path: &Path) -> Option<PathBuf> {
    cached_thumbnail_in(&gtk4::glib::user_cache_dir().join("thumbnails"), path)
}

pub(super) fn cached_thumbnail_in(cache: &Path, path: &Path) -> Option<PathBuf> {
    let absolute = std::path::absolute(path).ok()?;
    let modified = std::fs::metadata(&absolute)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let uri = gtk4::glib::filename_to_uri(&absolute, None).ok()?;
    let name = format!(
        "{}.png",
        gtk4::glib::compute_checksum_for_string(gtk4::glib::ChecksumType::Md5, &uri)?
    );
    // Sizes that cover a 128 px cell first; "normal" (128 px long side) is
    // still better than a full decode.
    ["large", "x-large", "normal"].iter().map(|size| cache.join(size).join(&name)).find(|entry| {
        read_bounded(entry)
            .is_some_and(|bytes| png_thumbnail_matches(&bytes, uri.as_str(), &modified.to_string()))
    })
}

fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    // Never follow a symlink or read a device out of the cache directory.
    // The checks run on the opened handle, so a file swapped between a
    // check and the open cannot slip through; O_NONBLOCK keeps the open
    // itself from hanging on a FIFO (std adds O_CLOEXEC).
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
    let file =
        std::fs::OpenOptions::new().read(true).custom_flags(flags.bits() as i32).open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > CACHE_MAX_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(CACHE_MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= CACHE_MAX_BYTES).then_some(bytes)
}

/// Walk the PNG chunk list without decompressing anything. Accepts the
/// entry only when its header is sane and both text keys match the file.
fn png_thumbnail_matches(bytes: &[u8], uri: &str, mtime: &str) -> bool {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    let Some(mut rest) = bytes.strip_prefix(SIGNATURE) else { return false };
    let (mut uri_ok, mut mtime_ok, mut first) = (false, false, true);
    while rest.len() >= 12 {
        let length = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        let kind = &rest[4..8];
        let Some(data) = rest.get(8..8 + length) else { return false };
        if first {
            if kind != b"IHDR" || length != 13 {
                return false;
            }
            let width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            let height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
            if width == 0 || height == 0 || width > CACHE_MAX_EDGE || height > CACHE_MAX_EDGE {
                return false;
            }
            first = false;
        } else if kind == b"tEXt" {
            if let Some(split) = data.iter().position(|b| *b == 0) {
                let (key, value) = (&data[..split], &data[split + 1..]);
                match key {
                    b"Thumb::URI" => uri_ok = value == uri.as_bytes(),
                    b"Thumb::MTime" => mtime_ok = value == mtime.as_bytes(),
                    _ => {}
                }
            }
        } else if kind == b"IEND" {
            break;
        }
        rest = &rest[(12 + length).min(rest.len())..];
    }
    uri_ok && mtime_ok
}

/// One-slot, cross-thread wake-up. A worker thread calls [`Wake::notify`]
/// after publishing a result; a `spawn_local` task awaits [`Wake::wait`]
/// on the GTK thread, so nothing runs on a timer while the viewer is idle.
#[derive(Clone, Default)]
pub(super) struct Wake(Arc<WakeInner>);

#[derive(Default)]
struct WakeInner {
    signalled: AtomicBool,
    closed: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl Wake {
    pub(super) fn notify(&self) {
        self.0.signalled.store(true, Ordering::Release);
        self.wake();
    }

    /// Ends every current and future wait.
    pub(super) fn close(&self) {
        self.0.closed.store(true, Ordering::Release);
        self.wake();
    }

    fn wake(&self) {
        let waker = self.0.waker.lock().map(|mut slot| slot.take()).unwrap_or(None);
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Resolves to `true` after the next notification, `false` once closed.
    pub(super) fn wait(&self) -> impl Future<Output = bool> + 'static {
        Waiting(self.0.clone())
    }
}

struct Waiting(Arc<WakeInner>);

impl Future for Waiting {
    type Output = bool;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
        let ready = |inner: &WakeInner| {
            if inner.closed.load(Ordering::Acquire) {
                Some(false)
            } else if inner.signalled.swap(false, Ordering::AcqRel) {
                Some(true)
            } else {
                None
            }
        };
        if let Some(result) = ready(&self.0) {
            return Poll::Ready(result);
        }
        if let Ok(mut slot) = self.0.waker.lock() {
            *slot = Some(cx.waker().clone());
        }
        // A notification that raced with registering the waker is not lost.
        ready(&self.0).map_or(Poll::Pending, Poll::Ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_never_upscales_or_explodes_on_extreme_aspect_ratios() {
        // The reported crash: 1 × 65535 used to become 128 × 8 388 480.
        assert_eq!(thumbnail_dimensions(1, 65_535, 128), (1, 512));
        assert_eq!(thumbnail_dimensions(65_535, 1, 128), (512, 1));
        // Small images keep their size.
        assert_eq!(thumbnail_dimensions(40, 30, 128), (40, 30));
        // Ordinary photos: the short side covers the cell.
        assert_eq!(thumbnail_dimensions(4000, 3000, 128), (171, 128));
        assert_eq!(thumbnail_dimensions(3000, 4000, 128), (128, 171));
        // Panoramas are bounded on the long side.
        assert_eq!(thumbnail_dimensions(40_000, 1000, 128), (512, 128));
        for (w, h) in [(0, 0), (1, 1), (u32::MAX, 1), (1, u32::MAX), (u32::MAX, u32::MAX)] {
            let (tw, th) = thumbnail_dimensions(w, h, 128);
            assert!((1..=512).contains(&tw) && (1..=512).contains(&th), "{w}x{h} -> {tw}x{th}");
        }
    }

    #[test]
    fn film_strip_window_is_full_and_contains_the_current_image() {
        assert_eq!(film_strip_start(0, 0, 41), 0);
        assert_eq!(film_strip_start(10, 9, 41), 0);
        assert_eq!(film_strip_start(41, 40, 41), 0);
        // 42 files used to put the last one alone on a second page.
        assert_eq!(film_strip_start(42, 41, 41), 1);
        assert_eq!(film_strip_start(100, 50, 41), 30);
        assert_eq!(film_strip_start(100, 3, 41), 0);
        for len in [1, 2, 40, 41, 42, 82, 83, 10_000] {
            for idx in 0..len {
                let start = film_strip_start(len, idx, 41);
                let end = (start + 41).min(len);
                assert!(start <= idx && idx < end, "len={len} idx={idx} start={start}");
                assert_eq!(end - start, len.min(41), "partial window len={len} idx={idx}");
            }
        }
    }

    fn png_with_text(width: u32, height: u32, text: &[(&str, &str)]) -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut chunk = |kind: &[u8], data: &[u8]| {
            png.extend((data.len() as u32).to_be_bytes());
            png.extend(kind);
            png.extend(data);
            png.extend([0; 4]);
        };
        let mut ihdr = width.to_be_bytes().to_vec();
        ihdr.extend(height.to_be_bytes());
        ihdr.extend([8, 6, 0, 0, 0]);
        chunk(b"IHDR", &ihdr);
        for (key, value) in text {
            chunk(b"tEXt", format!("{key}\0{value}").as_bytes());
        }
        chunk(b"IDAT", &[]);
        chunk(b"IEND", &[]);
        png
    }

    #[test]
    fn cache_entry_requires_matching_uri_mtime_and_sane_header() {
        let ok = [("Thumb::URI", "file:///a.png"), ("Thumb::MTime", "42")];
        assert!(png_thumbnail_matches(&png_with_text(256, 192, &ok), "file:///a.png", "42"));
        assert!(!png_thumbnail_matches(&png_with_text(256, 192, &ok), "file:///b.png", "42"));
        assert!(!png_thumbnail_matches(&png_with_text(256, 192, &ok), "file:///a.png", "43"));
        assert!(!png_thumbnail_matches(&png_with_text(4096, 1, &ok), "file:///a.png", "42"));
        assert!(!png_thumbnail_matches(&png_with_text(0, 1, &ok), "file:///a.png", "42"));
        let no_mtime = [("Thumb::URI", "file:///a.png")];
        assert!(!png_thumbnail_matches(&png_with_text(64, 64, &no_mtime), "file:///a.png", "42"));
        // Truncated or garbage input never panics.
        let full = png_with_text(256, 192, &ok);
        for cut in 0..full.len() {
            let _ = png_thumbnail_matches(&full[..cut], "file:///a.png", "42");
        }
        assert!(!png_thumbnail_matches(b"not a png", "file:///a.png", "42"));
    }

    #[test]
    fn cache_lookup_follows_the_freedesktop_layout() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("photo 1.png");
        std::fs::write(&image, b"x").unwrap();
        let uri = gtk4::glib::filename_to_uri(&image, None).unwrap();
        let mtime = std::fs::metadata(&image)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let md5 =
            gtk4::glib::compute_checksum_for_string(gtk4::glib::ChecksumType::Md5, &uri).unwrap();
        let cache = dir.path().join("thumbnails");
        assert_eq!(cached_thumbnail_in(&cache, &image), None);
        std::fs::create_dir_all(cache.join("normal")).unwrap();
        let entry = cache.join("normal").join(format!("{md5}.png"));
        std::fs::write(
            &entry,
            png_with_text(128, 96, &[("Thumb::URI", &uri), ("Thumb::MTime", "1")]),
        )
        .unwrap();
        assert_eq!(cached_thumbnail_in(&cache, &image), None, "stale entry accepted");
        std::fs::write(
            &entry,
            png_with_text(128, 96, &[("Thumb::URI", &uri), ("Thumb::MTime", &mtime)]),
        )
        .unwrap();
        assert_eq!(cached_thumbnail_in(&cache, &image), Some(entry));
    }

    #[test]
    fn cache_reader_rejects_symlinks_and_fifos_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("entry.png");
        std::fs::write(&file, b"png").unwrap();
        assert_eq!(read_bounded(&file).as_deref(), Some(&b"png"[..]));
        let link = dir.path().join("link.png");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert_eq!(read_bounded(&link), None, "followed a symlink");
        // A FIFO without a writer would block a plain open forever.
        let fifo = dir.path().join("fifo.png");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();
        assert_eq!(read_bounded(&fifo), None, "read a FIFO");
        let big = dir.path().join("big.png");
        std::fs::write(&big, vec![0u8; CACHE_MAX_BYTES as usize + 1]).unwrap();
        assert_eq!(read_bounded(&big), None, "oversized entry accepted");
    }

    #[test]
    fn wake_delivers_notifications_across_threads_and_closes() {
        let wake = Wake::default();
        let context = gtk4::glib::MainContext::new();
        let worker = wake.clone();
        wake.notify();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            worker.close();
        });
        let results = context.block_on(async {
            let first = wake.wait().await;
            let second = wake.wait().await;
            (first, second)
        });
        handle.join().unwrap();
        assert_eq!(results, (true, false));
    }
}
