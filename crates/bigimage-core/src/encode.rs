// SPDX-License-Identifier: GPL-3.0-or-later
//! Encoder-side tuning knobs shared across every `*_file` pipeline:
//! JPEG quality, PNG compression level, progressive flag, etc.
//!
//! Kept as a single bundle so the CLI, dialogs and "Para web" presets can
//! build it once and pass a reference through the rest of the pipeline
//! without every call site exploding into positional arguments.

/// Output encoder configuration.
///
/// `Default` produces library-default settings *plus* `strip_metadata =
/// true` (privacy-by-default contract). To change quality/progressive/
/// etc. in tests or call sites, prefer `..EncodeOptions::default()` so
/// new fields don't silently regress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeOptions {
    /// Quality `1..=100` for JPEG/AVIF. WebP is always lossless. `None`
    /// falls back to the encoder's default (JPEG uses 75).
    pub quality: Option<u8>,
    /// Emit progressive JPEG (multi-pass). Accepted for now but the pure-
    /// Rust `image` encoder only writes baseline JPEGs — a future toggle
    /// to `mozjpeg-sys` will honour this flag. For other formats it's a
    /// no-op.
    pub progressive: bool,
    /// Apply stronger compression: PNG uses the `Best` zlib level
    /// (slower encode, smaller file). Lossless — the same pixels.
    pub optimize: bool,
    /// Strip EXIF/IPTC/XMP/GPS on encode. **Default `true`.** Today our
    /// pipeline already drops metadata as a side-effect of decoding to
    /// `DynamicImage` (which has no metadata carrier) before re-encoding,
    /// so this flag is documentation of intent — but it's the contract a
    /// future "preserve metadata" feature must override explicitly. The
    /// regression test in `tests::default_options_strip_metadata` exists
    /// to fail if the default ever silently flips to `false`.
    pub strip_metadata: bool,
    /// AVIF encoder speed `1..=10` (1 = slowest / smallest file, 10 =
    /// fastest / larger file). We default to 8, favoring throughput.
    /// Actual time depends on image content, dimensions and hardware.
    /// The `optimize` flag clamps the effective speed at 4 to favor
    /// stronger compression when the user opts in.
    pub avif_speed: u8,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            quality: None,
            progressive: false,
            optimize: false,
            strip_metadata: true,
            avif_speed: 8,
        }
    }
}

impl EncodeOptions {
    /// Effective AVIF speed: `min(avif_speed, 4)` when `optimize` is
    /// set so the user's "smaller file" choice survives even if a
    /// caller raised `avif_speed` for batch throughput.
    pub fn effective_avif_speed(&self) -> u8 {
        if self.optimize {
            self.avif_speed.min(4)
        } else {
            self.avif_speed
        }
    }
}

impl EncodeOptions {
    /// Convenience: a preset tailored for web uploads (WhatsApp, general
    /// email). JPEG quality 85. Progressive/optimize are compatibility
    /// flags with no JPEG effect in the current encoder. Metadata stripped.
    pub fn web_preset() -> Self {
        Self { quality: Some(85), progressive: true, optimize: true, ..Self::default() }
    }

    /// Returns the resolved JPEG quality — user value when set, otherwise
    /// the library default (75).
    pub fn jpeg_quality(&self) -> u8 {
        self.quality.unwrap_or(75)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Privacy-by-default: the bare `EncodeOptions::default()` MUST strip
    /// EXIF/GPS/IPTC. Failing this test means a code path will leak
    /// metadata that today gets dropped silently — usually GPS coords.
    #[test]
    fn default_options_strip_metadata() {
        assert!(EncodeOptions::default().strip_metadata);
    }

    #[test]
    fn web_preset_strips_metadata() {
        assert!(EncodeOptions::web_preset().strip_metadata);
    }

    #[test]
    fn default_avif_speed_is_fast() {
        // Default biased toward batch throughput: speed 8 of 1..10.
        // Any change here means a hundred-file AVIF batch slows down
        // by a noticeable factor — keep 8 unless the perf review says
        // otherwise.
        assert_eq!(EncodeOptions::default().avif_speed, 8);
        assert_eq!(EncodeOptions::default().effective_avif_speed(), 8);
    }

    #[test]
    fn optimize_clamps_avif_speed_for_smaller_files() {
        let opts = EncodeOptions { optimize: true, ..EncodeOptions::default() };
        assert_eq!(
            opts.effective_avif_speed(),
            4,
            "optimize should slow AVIF to recover compression"
        );

        // Even if a caller cranks avif_speed up, optimize wins.
        let opts = EncodeOptions { optimize: true, avif_speed: 10, ..EncodeOptions::default() };
        assert_eq!(opts.effective_avif_speed(), 4);
    }
}
