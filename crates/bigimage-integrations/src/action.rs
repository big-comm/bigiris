// SPDX-License-Identifier: GPL-3.0-or-later
//! The matrix of right-click actions the installer ships.
//!
//! Every action is a pure data record so each file-manager generator can
//! translate it into its own format (KDE `.desktop`, Nemo `.nemo_action`,
//! Pantheon `.contract`, etc.) without duplicating the list.

/// A submenu grouping a handful of actions under a single label.
#[derive(Debug, Clone, Copy)]
pub struct Submenu {
    /// Filesystem-safe identifier. Used to build file names like
    /// `bigiris-convert-png.desktop`.
    pub id: &'static str,
    /// User-visible label (pt-BR by default; i18n comes with gettext).
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

/// Mime filter para documentos que o LibreOffice converte em PDF. Escolha
/// dos formatos segue a matriz que `soffice --convert-to pdf` atende bem
/// — texto, planilha e apresentação em ODF e em Office. Inclui `text/*`
/// porque o soffice importa TXT/MD/CSV direto no Writer.
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
    Submenu { id: "convert", label: "Converter", icon: "document-save-as-symbolic" };

/// Submenu: resize to a preset.
pub const SUB_RESIZE: Submenu =
    Submenu { id: "resize", label: "Redimensionar", icon: "view-fullscreen-symbolic" };

/// Submenu: cardinal rotation.
pub const SUB_ROTATE: Submenu =
    Submenu { id: "rotate", label: "Girar", icon: "object-rotate-right-symbolic" };

/// Submenu: mirror horizontally/vertically.
pub const SUB_FLIP: Submenu =
    Submenu { id: "flip", label: "Espelhar", icon: "object-flip-horizontal-symbolic" };

/// Submenu: tone / colour adjustments.
pub const SUB_ADJUST: Submenu =
    Submenu { id: "adjust", label: "Ajustar cores", icon: "image-adjust-symbolic" };

/// Submenu: social/web presets (1-clique: resize + quality + progressive).
/// Label sem `/` para nao colidir com separador de path quando a Nautilus
/// scripts tree cria a pasta do submenu.
pub const SUB_WEB: Submenu =
    Submenu { id: "web", label: "Para web", icon: "applications-internet-symbolic" };

/// Submenu: inspecao / limpeza de metadados (EXIF, IPTC, XMP).
pub const SUB_METADATA: Submenu =
    Submenu { id: "metadata", label: "Metadados", icon: "dialog-information-symbolic" };

/// Submenu: utilidades criativas (GIF animado, comparar, composicao).
pub const SUB_UTILS: Submenu =
    Submenu { id: "utils", label: "Utilidades", icon: "applications-graphics-symbolic" };

/// Submenu: IA local (background removal, upscale, denoise).
/// Requer build com feature `ai` e o pacote `onnxruntime` instalado.
pub const SUB_AI: Submenu =
    Submenu { id: "ai", label: "IA", icon: "applications-science-symbolic" };

/// Submenu: converter documento → PDF via LibreOffice. Só aparece quando
/// a seleção bate em [`DOC_MIME`] — em imagens o usuário usa Convert ▸.
pub const SUB_PDF: Submenu = Submenu { id: "pdf", label: "PDF", icon: "application-pdf-symbolic" };

/// All actions to install, in a display-friendly order.
///
/// **Order matters** — file managers render the menu in the order this
/// const declares the actions. The first entry here is the first row
/// the user sees in the right-click menu, so `view` (the most-frequent
/// single-click action: open the image in BigIris) deliberately sits
/// at the top, ahead of the multi-step Convert/Resize/etc. submenus.
///
/// `--overwrite skip` means the quick actions never clobber an existing
/// sibling file; users who want to replace or increment go through the
/// modal "Personalizar…" path.
pub const ACTIONS: &[Action] = &[
    // Visualizar (top-level, 1-click) — most common entry point.
    Action {
        id: "view",
        submenu: None,
        label: "Visualizar em BigIris",
        command: "bigiris %F",
        icon: "com.biglinux.Iris",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "batch-folder",
        submenu: None,
        label: "Processar esta pasta…",
        command: "bigiris --dialog=batch %F",
        icon: "folder-pictures-symbolic",
        mime_types: DIRECTORY_MIME,
    },
    // Convert ▸
    Action {
        id: "convert-png",
        submenu: Some(SUB_CONVERT),
        label: "PNG",
        command: "bigiris convert --to png --overwrite skip %F",
        icon: "image-x-generic-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "convert-jpg",
        submenu: Some(SUB_CONVERT),
        label: "JPG",
        command: "bigiris convert --to jpg --overwrite skip %F",
        icon: "image-x-generic-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "convert-webp",
        submenu: Some(SUB_CONVERT),
        label: "WebP",
        command: "bigiris convert --to webp --overwrite skip %F",
        icon: "image-x-generic-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "convert-avif",
        submenu: Some(SUB_CONVERT),
        label: "AVIF",
        command: "bigiris convert --to avif --overwrite skip %F",
        icon: "image-x-generic-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "convert-tiff",
        submenu: Some(SUB_CONVERT),
        label: "TIFF",
        command: "bigiris convert --to tiff --overwrite skip %F",
        icon: "image-x-generic-symbolic",
        mime_types: IMAGE_MIME,
    },
    // Imagem → PDF fica por conta do BigOCRPDF (já aparece no clique-
    // direito de imagens via sua própria extension). Não duplicamos
    // aqui pra não poluir o submenu Converter com o mesmo caminho.
    Action {
        id: "convert-custom",
        submenu: Some(SUB_CONVERT),
        label: "Mais opções…",
        command: "bigiris --dialog=convert %F",
        icon: "document-edit-symbolic",
        mime_types: IMAGE_MIME,
    },
    // Redimensionar ▸
    Action {
        id: "resize-25pct",
        submenu: Some(SUB_RESIZE),
        label: "25%",
        command: "bigiris resize --percent 25 %F",
        icon: "view-fullscreen-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "resize-50pct",
        submenu: Some(SUB_RESIZE),
        label: "50%",
        command: "bigiris resize --percent 50 %F",
        icon: "view-fullscreen-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "resize-200pct",
        submenu: Some(SUB_RESIZE),
        label: "200%",
        command: "bigiris resize --percent 200 %F",
        icon: "view-fullscreen-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "resize-hd-1920",
        submenu: Some(SUB_RESIZE),
        label: "HD (1920 px)",
        command: "bigiris resize --fit 1920x1920 %F",
        icon: "view-fullscreen-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "resize-4k-3840",
        submenu: Some(SUB_RESIZE),
        label: "4K (3840 px)",
        command: "bigiris resize --fit 3840x3840 %F",
        icon: "view-fullscreen-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "resize-custom",
        submenu: Some(SUB_RESIZE),
        label: "Mais opções…",
        command: "bigiris --dialog=resize %F",
        icon: "document-edit-symbolic",
        mime_types: IMAGE_MIME,
    },
    // Girar ▸
    Action {
        id: "rotate-90",
        submenu: Some(SUB_ROTATE),
        label: "90°",
        command: "bigiris rotate --degrees 90 %F",
        icon: "object-rotate-right-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "rotate-180",
        submenu: Some(SUB_ROTATE),
        label: "180°",
        command: "bigiris rotate --degrees 180 %F",
        icon: "object-rotate-right-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "rotate-270",
        submenu: Some(SUB_ROTATE),
        label: "270°",
        command: "bigiris rotate --degrees 270 %F",
        icon: "object-rotate-left-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "rotate-auto-exif",
        submenu: Some(SUB_ROTATE),
        label: "Automático (EXIF)",
        command: "bigiris rotate --auto --overwrite increment %F",
        icon: "object-rotate-right-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "rotate-custom",
        submenu: Some(SUB_ROTATE),
        label: "Mais opções…",
        command: "bigiris --dialog=rotate %F",
        icon: "document-edit-symbolic",
        mime_types: IMAGE_MIME,
    },
    // Espelhar ▸
    Action {
        id: "flip-horizontal",
        submenu: Some(SUB_FLIP),
        label: "Horizontal",
        command: "bigiris flip --axis horizontal %F",
        icon: "object-flip-horizontal-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "flip-vertical",
        submenu: Some(SUB_FLIP),
        label: "Vertical",
        command: "bigiris flip --axis vertical %F",
        icon: "object-flip-vertical-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "flip-custom",
        submenu: Some(SUB_FLIP),
        label: "Mais opções…",
        command: "bigiris --dialog=flip %F",
        icon: "document-edit-symbolic",
        mime_types: IMAGE_MIME,
    },
    // Ajustar cores ▸
    Action {
        id: "adjust-brightness-plus",
        submenu: Some(SUB_ADJUST),
        label: "+ Brilho (+10)",
        command: "bigiris adjust --brightness 10 %F",
        icon: "display-brightness-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "adjust-brightness-minus",
        submenu: Some(SUB_ADJUST),
        label: "− Brilho (-10)",
        command: "bigiris adjust --brightness -10 %F",
        icon: "display-brightness-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "adjust-contrast-plus",
        submenu: Some(SUB_ADJUST),
        label: "+ Contraste (+20)",
        command: "bigiris adjust --contrast 20 %F",
        icon: "display-brightness-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "adjust-bw",
        submenu: Some(SUB_ADJUST),
        label: "Preto e branco",
        command: "bigiris adjust --saturation -100 %F",
        icon: "color-pick-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "adjust-vivid",
        submenu: Some(SUB_ADJUST),
        label: "Cores vivas (+40)",
        command: "bigiris adjust --saturation 40 %F",
        icon: "color-pick-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "adjust-custom",
        submenu: Some(SUB_ADJUST),
        label: "Mais opções…",
        command: "bigiris --dialog=adjust %F",
        icon: "document-edit-symbolic",
        mime_types: IMAGE_MIME,
    },
    // Para web / redes ▸ — combos "1 clique" que fazem resize + JPEG com
    // qualidade otimizada para o canal. Todos usam --fit NxN (preserva
    // aspecto, faz upscale OU downscale conforme precisar) e JPEG
    // progressivo com quality preset.
    Action {
        id: "web-whatsapp",
        submenu: Some(SUB_WEB),
        label: "WhatsApp (1280 px · JPG q85)",
        command: "bigiris resize --fit 1280x1280 --to jpg --quality 85 --progressive %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-instagram",
        submenu: Some(SUB_WEB),
        label: "Instagram (1080 px · JPG q90)",
        command: "bigiris resize --fit 1080x1080 --to jpg --quality 90 --progressive %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-facebook",
        submenu: Some(SUB_WEB),
        label: "Facebook (2048 px · JPG q85)",
        command: "bigiris resize --fit 2048x2048 --to jpg --quality 85 --progressive %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-twitter",
        submenu: Some(SUB_WEB),
        label: "Twitter · X (1200 px · JPG q85)",
        command: "bigiris resize --fit 1200x1200 --to jpg --quality 85 --progressive %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-telegram",
        submenu: Some(SUB_WEB),
        label: "Telegram (2560 px · JPG q85)",
        command: "bigiris resize --fit 2560x2560 --to jpg --quality 85 --progressive %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-discord",
        submenu: Some(SUB_WEB),
        label: "Discord (2048 px · JPG q80)",
        command: "bigiris resize --fit 2048x2048 --to jpg --quality 80 --progressive %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-optimize-png",
        submenu: Some(SUB_WEB),
        label: "Otimizar PNG (compressão máxima)",
        command: "bigiris convert --to png --optimize --overwrite replace %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    // Letterbox presets — "Para web ▸" também cobre quando a rede social
    // força razão fixa e cortaria conteúdo importante da imagem. Estende a
    // tela em volta da imagem em vez de cortar; 0d0e12 (preto suave) é o
    // default que casa com o tema escuro do YouTube/Instagram.
    Action {
        id: "web-letterbox-yt",
        submenu: Some(SUB_WEB),
        label: "Letterbox YouTube post (1:1)",
        command: "bigiris extent --aspect 1:1 --bg 0d0e12 %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-letterbox-ig",
        submenu: Some(SUB_WEB),
        label: "Letterbox Instagram feed (4:5)",
        command: "bigiris extent --aspect 4:5 --bg 0d0e12 %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-letterbox-stories",
        submenu: Some(SUB_WEB),
        label: "Letterbox Stories / Shorts (9:16)",
        command: "bigiris extent --aspect 9:16 --bg 0d0e12 %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "web-letterbox-custom",
        submenu: Some(SUB_WEB),
        label: "Letterbox personalizado…",
        command: "bigiris --dialog=extent %F",
        icon: "applications-internet-symbolic",
        mime_types: IMAGE_MIME,
    },
    // Metadados ▸
    Action {
        id: "metadata-view",
        submenu: Some(SUB_METADATA),
        label: "Ver metadados…",
        command: "bigiris --dialog=metadata %F",
        icon: "dialog-information-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "metadata-strip-all",
        submenu: Some(SUB_METADATA),
        label: "Remover tudo (re-encode limpo)",
        // Nosso pipeline de convert ja re-encoda sem EXIF/IPTC/XMP,
        // entao "strip tudo" e um convert para o mesmo formato.
        command: "bigiris --dialog=convert %F",
        icon: "edit-delete-symbolic",
        mime_types: IMAGE_MIME,
    },
    // Utilidades ▸
    Action {
        id: "utils-batch",
        submenu: Some(SUB_UTILS),
        label: "Converter em lote (Prisma)…",
        command: "bigiris --dialog=batch %F",
        icon: "document-save-as-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "utils-animate-gif",
        submenu: Some(SUB_UTILS),
        label: "Criar GIF animado…",
        command: "bigiris --dialog=animate %F",
        icon: "video-x-generic-symbolic",
        mime_types: IMAGE_MIME,
    },
    Action {
        id: "utils-compare",
        submenu: Some(SUB_UTILS),
        label: "Comparar 2 imagens…",
        command: "bigiris --dialog=compare %F",
        icon: "view-dual-symbolic",
        mime_types: IMAGE_MIME,
    },
    // IA ▸
    Action {
        id: "ai-remove-bg",
        submenu: Some(SUB_AI),
        label: "Remover fundo (BiRefNet)",
        // Dispara o diálogo GUI com barra de progresso + compare ao fim.
        // Headless puro (`bigiris remove-bg`) fica só na CLI.
        command: "bigiris --dialog=remove-bg %F",
        icon: "applications-science-symbolic",
        mime_types: IMAGE_MIME,
    },
    // PDF ▸ (documentos, não imagens — filter DOC_MIME exclusivo)
    Action {
        id: "pdf-convert",
        submenu: Some(SUB_PDF),
        label: "Converter para PDF (LibreOffice)",
        command: "bigiris to-pdf %F",
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
            assert!(seen.insert(a.id), "id duplicado: {}", a.id);
        }
    }

    #[test]
    fn action_ids_are_filesystem_safe() {
        for a in ACTIONS {
            assert!(
                a.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "id não é filesystem-safe: {}",
                a.id
            );
            assert!(!a.id.is_empty());
        }
    }

    #[test]
    fn every_command_uses_file_placeholder() {
        for a in ACTIONS {
            assert!(a.command.contains("%F"), "comando sem %F: {} — {}", a.id, a.command);
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
