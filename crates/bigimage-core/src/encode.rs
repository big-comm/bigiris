// SPDX-License-Identifier: GPL-3.0-or-later
//! Encoder settings shared by every `*_file` pipeline: a fixed quality or
//! the perceptual target, AVIF speed, compression effort and the metadata
//! policy, built once by the CLI or a dialog and passed by reference.

/// Perceptual quality the encoders aim for when no explicit
/// [`EncodeOptions::quality`] is given.
///
/// Lossy formats (JPEG, WebP, AVIF) search for the *lowest* encoder quality
/// whose output still scores at least [`QualityTarget::ssimulacra2`] against
/// the pixels being saved, so every file is as small as it can be without
/// dropping below the chosen visual quality. Lossless formats pick their
/// strongest lossless settings; PNG and GIF may also use a palette when the
/// palette version meets the same score (never in [`QualityTarget::Maximum`]
/// for PNG).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum QualityTarget {
    /// SSIMULACRA2 ≥ 85: differences only visible when flipping between
    /// the images at high zoom. PNG stays lossless.
    Maximum,
    /// SSIMULACRA2 ≥ 80: no visible loss at normal viewing distance. The
    /// default for every conversion.
    #[default]
    Balanced,
    /// SSIMULACRA2 ≥ 70: still high quality, smallest files.
    Smallest,
}

impl QualityTarget {
    /// Minimum SSIMULACRA2 score (0..=100, 100 = identical) the output
    /// must reach against the source pixels.
    pub fn ssimulacra2(self) -> f64 {
        match self {
            QualityTarget::Maximum => 85.0,
            QualityTarget::Balanced => 80.0,
            QualityTarget::Smallest => 70.0,
        }
    }

    /// Parse the CLI/profile spelling `maximum`, `balanced` or `smallest`
    /// (also `max`, `small`), case-insensitive. The Portuguese spellings stay
    /// accepted because installed file-manager menus and saved batch profiles use them.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "maximum" | "max" | "maxima" | "máxima" => Some(QualityTarget::Maximum),
            "balanced" | "equilibrado" => Some(QualityTarget::Balanced),
            "smallest" | "small" | "menor" => Some(QualityTarget::Smallest),
            _ => None,
        }
    }
}

/// Output encoder configuration.
///
/// `Default` produces library-default settings *plus* `strip_metadata =
/// true` (privacy-by-default contract). To change quality/progressive/
/// etc. in tests or call sites, prefer `..EncodeOptions::default()` so
/// new fields don't silently regress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeOptions {
    /// Fixed encoder quality `1..=100` for JPEG, WebP and AVIF, bypassing
    /// the perceptual search of [`EncodeOptions::target`]. WebP at 100 is
    /// written lossless. `None` (the default) searches for the smallest
    /// file meeting `target`. PNG/GIF stay lossless/unquantized when set.
    pub quality: Option<u8>,
    /// Kept for compatibility: JPEGs are always written progressive with
    /// optimized Huffman tables (mozjpeg), which is smaller and universally
    /// supported. No effect on other formats.
    pub progressive: bool,
    /// Apply stronger compression: PNG runs a slower oxipng preset, AVIF
    /// a slower encoder speed. Lossless — the same pixels.
    pub optimize: bool,
    /// Never publish source metadata (EXIF, IPTC, XMP, GPS, comments).
    /// **Default `true`.** The encoders write none of it anyway; this flag
    /// decides whether a same-format conversion may publish the smaller
    /// original bytes when they carry metadata (see
    /// [`crate::convert_file_report`]).
    pub strip_metadata: bool,
    /// AVIF encoder speed `1..=10` (1 = slowest / smallest file, 10 =
    /// fastest / larger file), default 8. With a fixed `quality` it is the
    /// encoder speed; with `quality: None` the search runs at speed 10 and
    /// the final encode at this speed capped at 6. The `optimize` flag
    /// caps it at 4 in both cases.
    pub avif_speed: u8,
    /// Visual quality the encoders aim for when `quality` is `None`.
    /// Default [`QualityTarget::Balanced`].
    pub target: QualityTarget,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            quality: None,
            progressive: false,
            optimize: false,
            strip_metadata: true,
            avif_speed: 8,
            target: QualityTarget::Balanced,
        }
    }
}

impl EncodeOptions {
    /// Effective AVIF speed: `min(avif_speed, 4)` when `optimize` is
    /// set so the user's "smaller file" choice survives even if a
    /// caller raised `avif_speed` for batch throughput.
    pub fn effective_avif_speed(&self) -> u8 {
        if self.optimize { self.avif_speed.min(4) } else { self.avif_speed }
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
    fn default_target_is_balanced_and_parses_both_languages() {
        assert_eq!(EncodeOptions::default().target, QualityTarget::Balanced);
        assert_eq!(EncodeOptions::default().quality, None);
        for (text, target) in [
            ("maxima", QualityTarget::Maximum),
            ("Máxima", QualityTarget::Maximum),
            ("max", QualityTarget::Maximum),
            ("equilibrado", QualityTarget::Balanced),
            ("balanced", QualityTarget::Balanced),
            ("menor", QualityTarget::Smallest),
            ("small", QualityTarget::Smallest),
        ] {
            assert_eq!(QualityTarget::parse(text), Some(target), "{text}");
        }
        assert_eq!(QualityTarget::parse("ultra"), None);
        assert!(QualityTarget::Maximum.ssimulacra2() > QualityTarget::Balanced.ssimulacra2());
        assert!(QualityTarget::Balanced.ssimulacra2() > QualityTarget::Smallest.ssimulacra2());
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
