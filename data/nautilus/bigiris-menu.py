# SPDX-License-Identifier: GPL-3.0-or-later
# -*- coding: utf-8 -*-
#
# BigIris — extensao top-level do Nautilus.
#
# Requer o pacote `nautilus-python` instalado. Instalado sob:
#   - /usr/share/nautilus-python/extensions/       (system-wide, via PKGBUILD)
#   - ~/.local/share/nautilus-python/extensions/  (por usuario, via
#     `bigiris install-integrations --user`)
#
# Depois de instalar, recarregue o Nautilus: `nautilus -q`.
#
# O espelho textual deste menu em `.nemo_action` / `.desktop` / `.contract`
# para outros gerenciadores fica em
# crates/bigimage-integrations/src/action.rs — mantenha em sincronia quando
# acoes forem adicionadas.

import sys

import gi

# Nautilus >= 43 expoe o namespace `Nautilus` sem typelib versionado no
# gi.repository (via nautilus-python). `gi.require_version` pode falhar
# silenciosamente nesse caso — so tentamos fixar se o typelib existir e
# ignoramos qualquer outro cenario. O import subsequente continua
# funcionando porque nautilus-python injeta o modulo diretamente.
for _nautilus_version in ("4.1", "4.0", "3.0"):
    try:
        gi.require_version("Nautilus", _nautilus_version)
        break
    except (ValueError, Exception):
        continue

from gi.repository import GObject, Nautilus  # noqa: E402
import subprocess  # noqa: E402

sys.stderr.write("BigIris: extensão Nautilus carregada\n")


_TOP_LABEL = "BigIris"
_TOP_ICON = "com.biglinux.Iris"
_BINARY = "bigiris"

# Symbolic icon names picked per-action. Falls back to the submenu icon
# when an action leaves it as None. Keeps the icon column explicit so
# non-obvious verbs (Personalizar…, AI models) signal affordance visually.
_ICON_CONVERT = "image-x-generic-symbolic"
_ICON_CUSTOM = "document-edit-symbolic"
_ICON_RESIZE = "view-fullscreen-symbolic"
_ICON_ROTATE_R = "object-rotate-right-symbolic"
_ICON_ROTATE_L = "object-rotate-left-symbolic"
_ICON_FLIP_H = "object-flip-horizontal-symbolic"
_ICON_FLIP_V = "object-flip-vertical-symbolic"
_ICON_BRIGHT = "display-brightness-symbolic"
_ICON_COLOR = "color-pick-symbolic"
_ICON_INFO = "dialog-information-symbolic"
_ICON_TRASH = "edit-delete-symbolic"
_ICON_GIF = "video-x-generic-symbolic"
_ICON_COMPARE = "view-dual-symbolic"
_ICON_AI = "applications-science-symbolic"
_ICON_WEB = "applications-internet-symbolic"
_ICON_PDF = "application-pdf-symbolic"

# Filtros por mimetype. Mantenha em sincronia com IMAGE_MIME / DOC_MIME
# em crates/bigimage-integrations/src/action.rs.
_IMAGE_MIMES = ("image/",)  # prefix-match (image/*)
_DOC_MIMES = {
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
}

# (submenu_id, submenu_label, submenu_icon, required_kind, [(label, [bigiris args], icon)])
# `required_kind` é "image" ou "doc" — filtra o submenu por tipo de
# seleção. Submenus só aparecem quando TODA a seleção bate no kind
# exigido (all-or-nothing evita comandos rodando em arquivos inválidos).
_SUBMENUS = [
    (
        "convert",
        "Converter",
        "document-save-as-symbolic",
        "image",
        [
            ("PNG", ["convert", "--to", "png", "--overwrite", "skip"], _ICON_CONVERT),
            ("JPG", ["convert", "--to", "jpg", "--overwrite", "skip"], _ICON_CONVERT),
            ("WebP", ["convert", "--to", "webp", "--overwrite", "skip"], _ICON_CONVERT),
            ("AVIF", ["convert", "--to", "avif", "--overwrite", "skip"], _ICON_CONVERT),
            ("TIFF", ["convert", "--to", "tiff", "--overwrite", "skip"], _ICON_CONVERT),
            # PDF fica no menu do BigOCRPDF — evita redundância.
            ("Mais opções…", ["--dialog=convert"], _ICON_CUSTOM),
        ],
    ),
    (
        "resize",
        "Redimensionar",
        "view-fullscreen-symbolic",
        "image",
        [
            ("25%", ["resize", "--percent", "25"], _ICON_RESIZE),
            ("50%", ["resize", "--percent", "50"], _ICON_RESIZE),
            ("200%", ["resize", "--percent", "200"], _ICON_RESIZE),
            ("HD (1920 px)", ["resize", "--fit", "1920x1920"], _ICON_RESIZE),
            ("4K (3840 px)", ["resize", "--fit", "3840x3840"], _ICON_RESIZE),
            ("Mais opções…", ["--dialog=resize"], _ICON_CUSTOM),
        ],
    ),
    (
        "rotate",
        "Girar",
        "object-rotate-right-symbolic",
        "image",
        [
            ("90°", ["rotate", "--degrees", "90"], _ICON_ROTATE_R),
            ("180°", ["rotate", "--degrees", "180"], _ICON_ROTATE_R),
            ("270°", ["rotate", "--degrees", "270"], _ICON_ROTATE_L),
            ("Automático (EXIF)", ["rotate", "--auto", "--overwrite", "increment"], _ICON_ROTATE_R),
            ("Mais opções…", ["--dialog=rotate"], _ICON_CUSTOM),
        ],
    ),
    (
        "flip",
        "Espelhar",
        "object-flip-horizontal-symbolic",
        "image",
        [
            ("Horizontal", ["flip", "--axis", "horizontal"], _ICON_FLIP_H),
            ("Vertical", ["flip", "--axis", "vertical"], _ICON_FLIP_V),
            ("Mais opções…", ["--dialog=flip"], _ICON_CUSTOM),
        ],
    ),
    (
        "adjust",
        "Ajustar cores",
        "image-adjust-symbolic",
        "image",
        [
            ("+ Brilho (+10)", ["adjust", "--brightness", "10"], _ICON_BRIGHT),
            ("− Brilho (-10)", ["adjust", "--brightness", "-10"], _ICON_BRIGHT),
            ("+ Contraste (+20)", ["adjust", "--contrast", "20"], _ICON_BRIGHT),
            ("Preto e branco", ["adjust", "--saturation", "-100"], _ICON_COLOR),
            ("Cores vivas (+40)", ["adjust", "--saturation", "40"], _ICON_COLOR),
            ("Mais opções…", ["--dialog=adjust"], _ICON_CUSTOM),
        ],
    ),
    (
        "web",
        "Para web",
        "applications-internet-symbolic",
        "image",
        [
            ("WhatsApp (1280 px · JPG q85)", [
                "resize", "--fit", "1280x1280", "--to", "jpg",
                "--quality", "85", "--progressive",
            ], _ICON_WEB),
            ("Instagram (1080 px · JPG q90)", [
                "resize", "--fit", "1080x1080", "--to", "jpg",
                "--quality", "90", "--progressive",
            ], _ICON_WEB),
            ("Facebook (2048 px · JPG q85)", [
                "resize", "--fit", "2048x2048", "--to", "jpg",
                "--quality", "85", "--progressive",
            ], _ICON_WEB),
            ("Twitter · X (1200 px · JPG q85)", [
                "resize", "--fit", "1200x1200", "--to", "jpg",
                "--quality", "85", "--progressive",
            ], _ICON_WEB),
            ("Telegram (2560 px · JPG q85)", [
                "resize", "--fit", "2560x2560", "--to", "jpg",
                "--quality", "85", "--progressive",
            ], _ICON_WEB),
            ("Discord (2048 px · JPG q80)", [
                "resize", "--fit", "2048x2048", "--to", "jpg",
                "--quality", "80", "--progressive",
            ], _ICON_WEB),
            ("Otimizar PNG (compressão máxima)", [
                "convert", "--to", "png", "--optimize", "--overwrite", "replace",
            ], _ICON_CONVERT),
            ("Letterbox YouTube post (1:1)", [
                "extent", "--aspect", "1:1", "--bg", "#0d0e12",
            ], _ICON_WEB),
            ("Letterbox Instagram feed (4:5)", [
                "extent", "--aspect", "4:5", "--bg", "#0d0e12",
            ], _ICON_WEB),
            ("Letterbox Stories / Shorts (9:16)", [
                "extent", "--aspect", "9:16", "--bg", "#0d0e12",
            ], _ICON_WEB),
            ("Letterbox personalizado…", ["--dialog=extent"], _ICON_WEB),
        ],
    ),
    (
        "metadata",
        "Metadados",
        "dialog-information-symbolic",
        "image",
        [
            ("Ver metadados…", ["--dialog=metadata"], _ICON_INFO),
            ("Remover tudo (re-encode limpo)", ["--dialog=convert"], _ICON_TRASH),
        ],
    ),
    (
        "utils",
        "Utilidades",
        "applications-graphics-symbolic",
        "image",
        [
            ("Converter em lote (Prisma)…", ["--dialog=batch"], _ICON_CONVERT),
            ("Criar GIF animado…", ["--dialog=animate"], _ICON_GIF),
            ("Comparar 2 imagens…", ["--dialog=compare"], _ICON_COMPARE),
        ],
    ),
    (
        "ai",
        "IA",
        "applications-science-symbolic",
        "image",
        [
            ("Remover fundo (BiRefNet)", ["--dialog=remove-bg"], _ICON_AI),
        ],
    ),
    (
        "pdf",
        "PDF",
        _ICON_PDF,
        "doc",
        [
            ("Converter para PDF (LibreOffice)", ["to-pdf"], _ICON_PDF),
        ],
    ),
]

# Acoes top-level dentro do submenu Iris (fora de sub-sub-menus).
_TOP_LEVEL_ACTIONS = [
    ("Visualizar em BigIris", [], _TOP_ICON),
]


class BigIrisMenuProvider(GObject.GObject, Nautilus.MenuProvider):
    """Provider que acrescenta o submenu 'BigIris ▸' ao clique direito."""

    def get_file_items(self, *args):
        # Nunca deixe escapar exceção daqui: nautilus-python carrega
        # todas as extensions no mesmo processo e uma exceção aqui pode
        # matar o menu inteiro (inclusive de outras extensions). Logamos
        # em stderr pra `journalctl --user -t nautilus` mostrar, e
        # retornamos [] silenciosamente pro usuário não ver nada quebrado.
        try:
            return self._build_items(args)
        except Exception as exc:  # noqa: BLE001
            sys.stderr.write(f"BigIris: erro em get_file_items: {exc!r}\n")
            return []

    def _build_items(self, args):
        # Nautilus 3.x chama `get_file_items(window, files)`, Nautilus 4.x
        # chama `get_file_items(files)`. Pegar sempre o ultimo argumento
        # preserva compatibilidade com ambos.
        files = args[-1] if args else []
        image_paths = self._paths_of_kind(files, "image")
        doc_paths = self._paths_of_kind(files, "doc")
        folder_paths = self._paths_of_kind(files, "folder")
        if folder_paths:
            return self._folder_items(folder_paths + image_paths)
        if not image_paths and not doc_paths:
            return []

        # All-or-nothing: quando a seleção mistura imagens e documentos,
        # escondemos os submenus específicos pra não rodar comando em
        # arquivo do tipo errado. "Visualizar" continua só pra imagens.
        mixed = bool(image_paths) and bool(doc_paths)

        top = Nautilus.MenuItem(
            name="BigIrisExtension::Top",
            label=_TOP_LABEL,
            tip="Operações BigIris",
            icon=_TOP_ICON,
        )
        top_menu = Nautilus.Menu()
        top.set_submenu(top_menu)

        for sub_id, sub_label, sub_icon, kind, actions in _SUBMENUS:
            if mixed:
                continue
            if kind == "image" and not image_paths:
                continue
            if kind == "doc" and not doc_paths:
                continue
            paths_for_sub = image_paths if kind == "image" else doc_paths

            sub_item = Nautilus.MenuItem(
                name=f"BigIrisExtension::Sub::{sub_id}",
                label=sub_label,
                icon=sub_icon,
            )
            sub_menu = Nautilus.Menu()
            sub_item.set_submenu(sub_menu)

            for action_label, action_args, action_icon in actions:
                action_item = Nautilus.MenuItem(
                    name=f"BigIrisExtension::Action::{sub_id}::{action_label}",
                    label=action_label,
                    icon=action_icon or sub_icon,
                )
                # `list(...)` defeats Python's late-binding-closure gotcha —
                # every menu item gets its own snapshot of args/paths.
                action_item.connect(
                    "activate",
                    self._run_action,
                    list(action_args),
                    list(paths_for_sub),
                )
                sub_menu.append_item(action_item)

            top_menu.append_item(sub_item)

        if image_paths:
            for label, args, icon in _TOP_LEVEL_ACTIONS:
                item = Nautilus.MenuItem(
                    name=f"BigIrisExtension::Top::{label}",
                    label=label,
                    icon=icon,
                )
                item.connect("activate", self._run_action, list(args), list(image_paths))
                top_menu.append_item(item)

        return [top]

    @staticmethod
    def _paths_of_kind(files, kind):
        out = []
        for f in files:
            try:
                mime = f.get_mime_type() or ""
            except Exception:
                continue
            if kind == "image" and mime.startswith(_IMAGE_MIMES):
                pass
            elif kind == "doc" and mime in _DOC_MIMES:
                pass
            elif kind == "folder" and mime == "inode/directory":
                pass
            else:
                continue
            loc = f.get_location()
            if loc and loc.get_path():
                out.append(loc.get_path())
        return out

    def _folder_items(self, paths):
        if not paths:
            return []
        item = Nautilus.MenuItem(
            name="BigIrisExtension::BatchFolder",
            label="BigIris — Processar esta pasta…",
            tip="Converter imagens da pasta com o Prisma",
            icon=_ICON_CONVERT,
        )
        item.connect("activate", self._run_action, ["--dialog=batch"], list(paths))
        return [item]

    def get_background_items(self, *args):
        try:
            folder = args[-1] if args else None
            return self._folder_items(self._paths_of_kind([folder], "folder"))
        except Exception as exc:
            sys.stderr.write(f"BigIris: erro em get_background_items: {exc!r}\n")
            return []

    def _run_action(self, menu_item, args, paths):
        try:
            subprocess.Popen(
                [_BINARY] + list(args) + list(paths),
                close_fds=True,
                start_new_session=True,
            )
        except Exception as exc:
            sys.stderr.write(f"BigIris: falha ao invocar {_BINARY} {args}: {exc}\n")

