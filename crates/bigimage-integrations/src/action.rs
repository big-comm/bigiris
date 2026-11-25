// SPDX-License-Identifier: GPL-3.0-or-later
//! The matrix of right-click actions the installer ships.
//!
//! Every action is a pure data record so each file-manager generator can
//! translate it into its own format (KDE `.desktop`, Nemo `.nemo_action`,
//! Pantheon `.contract`, etc.) without duplicating the list.

use crate::i18n::mark as n_;

/// A submenu grouping a handful of actions under a single label.
#[derive(Debug, Clone, Copy)]
pub struct Submenu {
    /// Filesystem-safe identifier. Used to build file names like
    /// `bigiris-convert-png.desktop`.
    pub id: &'static str,
    /// User-visible label, an English gettext msgid.
    pub label: &'static str,
    /// Symbolic icon name from the standard icon theme.
    pub icon: &'static str,
}

/// One right-click action. Enough information to generate every target
/// format without the generators having to peek at each other.
#[derive(Debug, Clone, Copy)]
pub struct Action {
    /// Stable identifier, globally unique across [`ACTIONS`]. Filesystem-safe
    /// (kebab-case, no spaces, ASCII only).
    pub id: &'static str,
    /// Optional grouping. `None` means the action sits at the top of the
    /// Íris submenu itself.
    pub submenu: Option<Submenu>,
    /// User-visible label.
    pub label: &'static str,
    /// Shell command template. `%F` expands to the selected files in the
    /// file manager's own convention.
    pub command: &'static str,
    /// Symbolic icon name.
    pub icon: &'static str,
    /// Mime-type globs this action accepts (e.g. `"image/*"`).
    pub mime_types: &'static [&'static str],
}

/// Top-level menu label that every generator nests into.
pub const TOP_LEVEL_LABEL: &str = "BigIris";

/// Mime filter reused for every action (all image formats).
const IMAGE_MIME: &[&str] = &["image/*"];
const DIRECTORY_MIME: &[&str] = &["inode/directory"];

/// Mime filter for documents LibreOffice converts to PDF: the text,
/// spreadsheet and presentation formats, ODF and Office, that `soffice
/// --convert-to pdf` handles well. Plain text types are included because
/// soffice imports TXT, MD and CSV straight into Writer.
const DOC_MIME: &[&str] = &[
    "application/vnd.oasis.opendocument.text",
    "application/vnd.oasis.opendocument.spreadsheet",
    "application/vnd.oasis.opendocument.presentation",
    "application/vnd.oasis.opendocument.graphics",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "application/msword",
    "application/vnd.ms-excel",
    "application/vnd.ms-powerpoint",
    "application/rtf",
    "text/plain",
    "text/markdown",
    "text/csv",
    "text/html",
    "application/xhtml+xml",
];

/// Submenu: convert to another format.
pub const SUB_CONVERT: Submenu =
    Submenu { id: "convert", label: n_("Convert"), icon: "document-save-as-symbolic" };

/// Submenu: resize to a preset.
pub const SUB_RESIZE: Submenu =
    Submenu { id: "resize", label: n_("Resize"), icon: "view-fullscreen-symbolic" };

/// Submenu: cardinal rotation.
pub const SUB_ROTATE: Submenu =
    Submenu { id: "rotate", label: n_("Rotate"), icon: "object-rotate-right-symbolic" };

/// Submenu: mirror horizontally/vertically.
pub const SUB_FLIP: Submenu =
    Submenu { id: "flip", label: n_("Flip"), icon: "object-flip-horizontal-symbolic" };

/// Submenu: tone and color adjustments.
pub const SUB_ADJUST: Submenu =
    Submenu { id: "adjust", label: n_("Adjust colors"), icon: "image-adjust-symbolic" };

/// Submenu: one-click social and web presets (resize, quality, progressive).
/// The label has no `/`, which would collide with the path separator when a
/// file manager makes a folder for the submenu.
pub const SUB_WEB: Submenu =
    Submenu { id: "web", label: n_("For the web"), icon: "applications-internet-symbolic" };

/// Submenu: inspect or strip metadata (EXIF, IPTC, XMP).
pub const SUB_METADATA: Submenu =
    Submenu { id: "metadata", label: n_("Metadata"), icon: "dialog-information-symbolic" };

/// Submenu: creative utilities (animated GIF, compare, composition).
pub const SUB_UTILS: Submenu =
    Submenu { id: "utils", label: n_("Utilities"), icon: "applications-graphics-symbolic" };

/// Submenu: local AI (background removal, upscale, denoise).
/// Needs a build with the `ai` feature; each tool needs its weights package
/// (`birefnet-native`, `realesrgan-native`, ...).
pub const SUB_AI: Submenu =
    Submenu { id: "ai", label: n_("AI"), icon: "applications-science-symbolic" };

/// Submenu: convert a document to PDF through LibreOffice. Shown only when
/// the selection matches [`DOC_MIME`]; images go through Convert ▸.
pub const SUB_PDF: Submenu =
    Submenu { id: "pdf", label: n_("PDF"), icon: "application-pdf-symbolic" };

/// All actions to install, in a display-friendly order.
///
/// **Order matters**: file managers render the menu in the order this
/// const declares the actions. The first entry here is the first row
/// the user sees in the right-click menu, so `view` (the most-frequent
/// single-click action: open the image in BigIris) deliberately sits
/// at the top, ahead of the multi-step Convert/Resize/etc. submenus.
///
/// `--overwrite skip` means the quick actions never clobber an existing
/// sibling file; users who want to replace or increment go through the
/// modal "More options…" path.
pub const ACTIONS: &[Action] = &[
    // View (top level, one click): the most common entry point.
    Action {
        id: "view",
        submenu: None,
        label: n_("View in BigIris"),
        command: "bigiris %F",
        icon: "com.biglinux.Iris",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "batch-folder",
        submenu: None,
        label: n_("Process this folder…"),
        command: "bigiris --dialog=batch %F",
        icon: "folder-pictures-symbolic",
        mime_types: DIRECTORY_MIME,
    },
    // Convert ▸
    Action {
        id: "convert-png",
        submenu: Some(SUB_CONVERT),
        label: n_("PNG"),
        command: "bigiris --notify convert --to png --overwrite skip %F",
        icon: "image-x-generic-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "convert-jpg",
        submenu: Some(SUB_CONVERT),
        label: n_("JPG"),
        command: "bigiris --notify convert --to jpg --overwrite skip %F",
        icon: "image-x-generic-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "convert-webp",
        submenu: Some(SUB_CONVERT),
        label: n_("WebP"),
        command: "bigiris --notify convert --to webp --overwrite skip %F",
        icon: "image-x-generic-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "convert-avif",
        submenu: Some(SUB_CONVERT),
        label: n_("AVIF"),
        command: "bigiris --notify convert --to avif --overwrite skip %F",
        icon: "image-x-generic-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "convert-tiff",
        submenu: Some(SUB_CONVERT),
        label: n_("TIFF"),
        command: "bigiris --notify convert --to tiff --overwrite skip %F",
        icon: "image-x-generic-symbolic",
        mime_types: IMAGE_MIME,
    },
    // Image to PDF belongs to BigOCRPDF, whose own extension already adds
    // it to the image right-click menu; repeating it here would clutter Convert.
    Action {
        id: "convert-custom",
        submenu: Some(SUB_CONVERT),
        label: n_("More options…"),
        command: "bigiris --dialog=convert %F",
        icon: "document-edit-symbolic",
        mime_types: IMAGE_MIME,
    },
    // Resize ▸
    Action {
        id: "resize-25pct",
        submenu: Some(SUB_RESIZE),
        label: n_("25%"),
        command: "bigiris --notify resize --percent 25 %F",
        icon: "view-fullscreen-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "resize-50pct",
        submenu: Some(SUB_RESIZE),
        label: n_("50%"),
        command: "bigiris --notify resize --percent 50 %F",
        icon: "view-fullscreen-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "resize-200pct",
        submenu: Some(SUB_RESIZE),
        label: n_("200%"),
        command: "bigiris --notify resize --percent 200 %F",
        icon: "view-fullscreen-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "resize-hd-1920",
        submenu: Some(SUB_RESIZE),
        label: n_("HD (1920 px)"),
        command: "bigiris --notify resize --fit 1920x1920 %F",
        icon: "view-fullscreen-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "resize-4k-3840",
        submenu: Some(SUB_RESIZE),
        label: n_("4K (3840 px)"),
        command: "bigiris --notify resize --fit 3840x3840 %F",
        icon: "view-fullscreen-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "resize-custom",
        submenu: Some(SUB_RESIZE),
        label: n_("More options…"),
        command: "bigiris --dialog=resize %F",
        icon: "document-edit-symbolic",
        mime_types: IMAGE_MIME,
    },
    // Rotate ▸
    Action {
        id: "rotate-90",
        submenu: Some(SUB_ROTATE),
        label: n_("90°"),
        command: "bigiris --notify rotate --degrees 90 %F",
        icon: "object-rotate-right-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "rotate-180",
        submenu: Some(SUB_ROTATE),
        label: n_("180°"),
        command: "bigiris --notify rotate --degrees 180 %F",
        icon: "object-rotate-right-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "rotate-270",
        submenu: Some(SUB_ROTATE),
        label: n_("270°"),
        command: "bigiris --notify rotate --degrees 270 %F",
        icon: "object-rotate-left-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "rotate-auto-exif",
        submenu: Some(SUB_ROTATE),
        label: n_("Automatic (EXIF)"),
        command: "bigiris --notify rotate --auto --overwrite increment %F",
        icon: "object-rotate-right-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "rotate-custom",
        submenu: Some(SUB_ROTATE),
        label: n_("More options…"),
        command: "bigiris --dialog=rotate %F",
        icon: "document-edit-symbolic",
        mime_types: IMAGE_MIME,
    },
    // Flip ▸
    Action {
        id: "flip-horizontal",
        submenu: Some(SUB_FLIP),
        label: n_("Horizontal"),
        command: "bigiris --notify flip --axis horizontal %F",
        icon: "object-flip-horizontal-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "flip-vertical",
        submenu: Some(SUB_FLIP),
        label: n_("Vertical"),
        command: "bigiris --notify flip --axis vertical %F",
        icon: "object-flip-vertical-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "flip-custom",
        submenu: Some(SUB_FLIP),
        label: n_("More options…"),
        command: "bigiris --dialog=flip %F",
        icon: "document-edit-symbolic",
        mime_types: IMAGE_MIME,
    },
    // Adjust colors ▸
    Action {
        id: "adjust-brightness-plus",
        submenu: Some(SUB_ADJUST),
        label: n_("+ Brightness (+10)"),
        command: "bigiris --notify adjust --brightness 10 %F",
        icon: "display-brightness-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "adjust-brightness-minus",
        submenu: Some(SUB_ADJUST),
        label: n_("− Brightness (-10)"),
        command: "bigiris --notify adjust --brightness -10 %F",
        icon: "display-brightness-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "adjust-contrast-plus",
        submenu: Some(SUB_ADJUST),
        label: n_("+ Contrast (+20)"),
        command: "bigiris --notify adjust --contrast 20 %F",
        icon: "display-brightness-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "adjust-bw",
        submenu: Some(SUB_ADJUST),
        label: n_("Black and white"),
        command: "bigiris --notify adjust --saturation -100 %F",
        icon: "color-pick-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "adjust-vivid",
        submenu: Some(SUB_ADJUST),
        label: n_("Vivid colors (+40)"),
        command: "bigiris --notify adjust --saturation 40 %F",
        icon: "color-pick-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "adjust-custom",
        submenu: Some(SUB_ADJUST),
        label: n_("More options…"),
        command: "bigiris --dialog=adjust %F",
        icon: "document-edit-symbolic",
        mime_types: IMAGE_MIME,
    },
    // For the web ▸: one-click resize plus a progressive JPEG at the quality
    // each channel wants. `--fit NxN` keeps the aspect ratio and scales up or down.
    Action {
        id: "web-whatsapp",
        submenu: Some(SUB_WEB),
        label: n_("WhatsApp (1280 px · JPG q85)"),
        command: "bigiris --notify resize --fit 1280x1280 --to jpg --quality 85 --progressive %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-instagram",
        submenu: Some(SUB_WEB),
        label: n_("Instagram (1080 px · JPG q90)"),
        command: "bigiris --notify resize --fit 1080x1080 --to jpg --quality 90 --progressive %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-facebook",
        submenu: Some(SUB_WEB),
        label: n_("Facebook (2048 px · JPG q85)"),
        command: "bigiris --notify resize --fit 2048x2048 --to jpg --quality 85 --progressive %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-twitter",
        submenu: Some(SUB_WEB),
        label: n_("Twitter · X (1200 px · JPG q85)"),
        command: "bigiris --notify resize --fit 1200x1200 --to jpg --quality 85 --progressive %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-telegram",
        submenu: Some(SUB_WEB),
        label: n_("Telegram (2560 px · JPG q85)"),
        command: "bigiris --notify resize --fit 2560x2560 --to jpg --quality 85 --progressive %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-discord",
        submenu: Some(SUB_WEB),
        label: n_("Discord (2048 px · JPG q80)"),
        command: "bigiris --notify resize --fit 2048x2048 --to jpg --quality 80 --progressive %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-optimize-png",
        submenu: Some(SUB_WEB),
        label: n_("Optimize PNG (maximum compression)"),
        command:
            "bigiris --notify convert --to png --target maxima --optimize --overwrite replace %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    // Letterbox presets extend the canvas instead of cropping when a network
    // forces a fixed ratio; 0d0e12 (soft black) matches the YouTube and
    // Instagram dark themes.
    Action {
        id: "web-letterbox-yt",
        submenu: Some(SUB_WEB),
        label: n_("Letterbox YouTube post (1:1)"),
        command: "bigiris --notify extent --aspect 1:1 --bg 0d0e12 %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-letterbox-ig",
        submenu: Some(SUB_WEB),
        label: n_("Letterbox Instagram feed (4:5)"),
        command: "bigiris --notify extent --aspect 4:5 --bg 0d0e12 %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-letterbox-stories",
        submenu: Some(SUB_WEB),
        label: n_("Letterbox Stories / Shorts (9:16)"),
        command: "bigiris --notify extent --aspect 9:16 --bg 0d0e12 %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-letterbox-custom",
        submenu: Some(SUB_WEB),
        label: n_("Custom letterbox…"),
        command: "bigiris --dialog=extent %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    // Metadata ▸
    Action {
        id: "metadata-view",
        submenu: Some(SUB_METADATA),
        label: n_("View metadata…"),
        command: "bigiris --dialog=metadata %F",
        icon: "dialog-information-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "metadata-strip-all",
        submenu: Some(SUB_METADATA),
        label: n_("Remove all metadata (clean re-encode)"),
        // Convert re-encodes without EXIF, IPTC or XMP, so stripping
        // everything is a convert to the same format.
        command: "bigiris --dialog=convert %F",
        icon: "edit-delete-symbolic",
        mime_types: IMAGE_MIME,
    },
    // Utilities ▸
    Action {
        id: "utils-batch",
        submenu: Some(SUB_UTILS),
        label: n_("Batch convert (Prisma)…"),
        command: "bigiris --dialog=batch %F",
        icon: "document-save-as-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "utils-animate-gif",
        submenu: Some(SUB_UTILS),
        label: n_("Create animated GIF…"),
        command: "bigiris --dialog=animate %F",
        icon: "video-x-generic-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "utils-compare",
        submenu: Some(SUB_UTILS),
        label: n_("Compare 2 images…"),
        command: "bigiris --dialog=compare %F",
        icon: "view-dual-symbolic",
        mime_types: IMAGE_MIME,
    },
    // AI ▸
    Action {
        id: "ai-remove-bg",
        submenu: Some(SUB_AI),
        label: n_("Remove background (BiRefNet)"),
        // Opens the dialog with a progress bar and a comparison at the end;
        // the headless `bigiris remove-bg` stays CLI only.
        command: "bigiris --dialog=remove-bg %F",
        icon: "applications-science-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "ai-upscale",
        submenu: Some(SUB_AI),
        label: n_("Upscale…"),
        // Same dialog as the viewer's edit panel: Real-ESRGAN when
        // `realesrgan-native` is installed, Lanczos3 otherwise.
        command: "bigiris --dialog=upscale %F",
        icon: "applications-science-symbolic",
        mime_types: IMAGE_MIME,
    },
    // PDF ▸ (documents, not images: DOC_MIME only)
    Action {
        id: "pdf-convert",
        submenu: Some(SUB_PDF),
        label: n_("Convert to PDF (LibreOffice)"),
        command: "bigiris --notify to-pdf %F",
        icon: "application-pdf-symbolic",
        mime_types: DOC_MIME,
    },
];

/// Every distinct submenu referenced by [`ACTIONS`], in declaration order.
pub fn submenus() -> Vec<Submenu> {
    let mut seen: Vec<&'static str> = Vec::new();
    let mut out: Vec<Submenu> = Vec::new();
    for action in ACTIONS {
        if let Some(sub) = action.submenu {
            if !seen.contains(&sub.id) {
                seen.push(sub.id);
                out.push(sub);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for a in ACTIONS {
            assert!(seen.insert(a.id), "duplicate id: {}", a.id);
        }
    }

    #[test]
    fn action_ids_are_filesystem_safe() {
        for a in ACTIONS {
            assert!(
                a.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "id is not filesystem-safe: {}",
                a.id
            );
            assert!(!a.id.is_empty());
        }
    }

    #[test]
    fn every_command_uses_file_placeholder() {
        for a in ACTIONS {
            assert!(a.command.contains("%F"), "command without %F: {}: {}", a.id, a.command);
        }
    }

    #[test]
    fn submenus_collected_in_order() {
        let s = submenus();
        let ids: Vec<_> = s.iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            vec![
                "convert", "resize", "rotate", "flip", "adjust", "web", "metadata", "utils", "ai",
                "pdf",
            ]
        );
    }
}
