// SPDX-License-Identifier: GPL-3.0-or-later
//! Pure helpers behind the viewer window.

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
}
