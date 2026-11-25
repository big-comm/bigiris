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
# As acoes vem de crates/bigimage-integrations/src/action.rs: o instalador
# substitui a linha `_ACTIONS = []` abaixo pela tabela gerada dali.

import sys
import gettext
import os

_ = gettext.translation("bigiris", localedir=os.environ.get("BIGIRIS_LOCALE_DIR", "/usr/share/locale"), fallback=True).gettext

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
_FOLDER_MIMES = ("inode/directory",)

# (id, submenu id or None, submenu label, submenu icon, label, argv, icon, MIME patterns)
_ACTIONS = []


def _matches(mime, patterns):
    return any(mime == p or (p.endswith("/*") and mime.startswith(p[:-1])) for p in patterns)


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
        selected = self._local_files(files)
        folder_paths = [path for path, mime in selected if _matches(mime, _FOLDER_MIMES)]
        if folder_paths:
            image_paths = [path for path, mime in selected if mime.startswith("image/")]
            return self._folder_items(folder_paths + image_paths)

        # Paths per MIME filter. A selection that spans two filters (images
        # and documents) gets no submenu, so no command runs on a file of the
        # wrong type; actions outside submenus still run on their own files.
        paths_by_filter = {}
        for action in _ACTIONS:
            patterns = tuple(action[7])
            if patterns != _FOLDER_MIMES:
                paths_by_filter[patterns] = [path for path, mime in selected if _matches(mime, patterns)]
        mixed = sum(1 for paths in paths_by_filter.values() if paths) > 1

        entries = []
        submenus = {}
        for action_id, sub_id, sub_label, sub_icon, label, argv, icon, patterns in _ACTIONS:
            paths = paths_by_filter.get(tuple(patterns))
            if not paths or (sub_id is not None and mixed):
                continue
            item = Nautilus.MenuItem(
                name=f"BigIrisExtension::Action::{action_id}",
                label=_(label),
                icon=icon,
            )
            item.connect("activate", self._run_action, list(argv), list(paths))
            if sub_id is None:
                entries.append(item)
                continue
            if sub_id not in submenus:
                sub_item = Nautilus.MenuItem(
                    name=f"BigIrisExtension::Sub::{sub_id}",
                    label=_(sub_label),
                    icon=sub_icon,
                )
                submenus[sub_id] = Nautilus.Menu()
                sub_item.set_submenu(submenus[sub_id])
                entries.append(sub_item)
            submenus[sub_id].append_item(item)

        if not entries:
            return []
        top_menu = Nautilus.Menu()
        for entry in entries:
            top_menu.append_item(entry)
        top = Nautilus.MenuItem(
            name="BigIrisExtension::Top",
            label=_TOP_LABEL,
            tip=_("BigIris actions"),
            icon=_TOP_ICON,
        )
        top.set_submenu(top_menu)
        return [top]

    @staticmethod
    def _local_files(files):
        out = []
        for f in files:
            try:
                mime = f.get_mime_type() or ""
            except Exception:
                continue
            loc = f.get_location()
            if loc and loc.get_path():
                out.append((loc.get_path(), mime))
        return out

    def _folder_items(self, paths):
        folder_action = next((a for a in _ACTIONS if tuple(a[7]) == _FOLDER_MIMES), None)
        if not paths or folder_action is None:
            return []
        item = Nautilus.MenuItem(
            name="BigIrisExtension::BatchFolder",
            label=_("BigIris: Process this folder…"),
            tip=_("Convert folder images with Prisma"),
            icon=folder_action[6],
        )
        item.connect("activate", self._run_action, list(folder_action[5]), list(paths))
        return [item]

    def get_background_items(self, *args):
        try:
            folder = args[-1] if args else None
            paths = [path for path, mime in self._local_files([folder]) if _matches(mime, _FOLDER_MIMES)]
            return self._folder_items(paths)
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
