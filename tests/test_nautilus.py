# SPDX-License-Identifier: GPL-3.0-or-later
"""Contract tests without importing or modifying the running Nautilus."""
import importlib.util
from pathlib import Path
import sys
from types import ModuleType, SimpleNamespace
import unittest
from unittest.mock import patch

class Item:
    def __init__(self, **kwargs):
        self.options = kwargs
        self.callback = None
    def set_submenu(self, menu):
        self.submenu = menu
    def connect(self, _signal, callback, *args):
        self.callback = (callback, args)

class Menu:
    def __init__(self):
        self.items = []
    def append_item(self, item):
        self.items.append(item)

class GObjectBase:
    pass
class ProviderBase:
    pass

gi = ModuleType("gi")
gi.require_version = lambda *_: None
repository = ModuleType("gi.repository")
repository.GObject = SimpleNamespace(GObject=GObjectBase)
repository.Nautilus = SimpleNamespace(MenuProvider=ProviderBase, MenuItem=Item, Menu=Menu)
spec = importlib.util.spec_from_file_location("bigiris_menu", Path(__file__).resolve().parents[1] / "data/nautilus/bigiris-menu.py")
module = importlib.util.module_from_spec(spec)
with patch.dict(sys.modules, {"gi": gi, "gi.repository": repository}):
    spec.loader.exec_module(module)

def file(mime, path):
    return SimpleNamespace(get_mime_type=lambda: mime,
                           get_location=lambda: SimpleNamespace(get_path=lambda: path))

class NautilusContract(unittest.TestCase):
    def setUp(self):
        self.provider = module.BigIrisMenuProvider()

    def test_both_nautilus_callback_signatures_work(self):
        files = [file("image/png", "/tmp/imagem ç.png")]
        self.assertEqual(len(self.provider.get_file_items(files)), 1)
        self.assertEqual(len(self.provider.get_file_items(object(), files)), 1)

    def test_remote_files_and_unsupported_files_are_excluded(self):
        files = [file("image/png", None), file("text/unknown", "/tmp/no.txt")]
        self.assertEqual(self.provider.get_file_items(files), [])

    def test_mixed_selection_hides_format_specific_actions(self):
        top = self.provider.get_file_items([file("image/png", "/tmp/a.png"),
                                            file("text/plain", "/tmp/a.txt")])[0]
        self.assertEqual(len(top.submenu.items), 1)  # Only image viewing.

    def test_untrusted_filenames_are_passed_as_literal_argv(self):
        paths = ["/tmp/$(touch hacked); ' ç.png", "/tmp/line\nbreak.png"]
        with patch.object(module.subprocess, "Popen") as popen:
            self.provider._run_action(None, ["convert", "--to", "png"], paths)
        self.assertEqual(popen.call_args.args[0], ["bigiris", "convert", "--to", "png"] + paths)
        self.assertNotIn("shell", popen.call_args.kwargs)
        self.assertTrue(popen.call_args.kwargs["close_fds"])

    def test_folder_selection_and_background_dispatch_batch(self):
        folder = file("inode/directory", "/tmp/fotos")
        for items in (self.provider.get_file_items([folder]),
                      self.provider.get_background_items(folder),
                      self.provider.get_background_items(object(), folder)):
            self.assertEqual(len(items), 1)
            callback, args = items[0].callback
            with patch.object(module.subprocess, "Popen") as popen:
                callback(items[0], *args)
            self.assertEqual(popen.call_args.args[0], ["bigiris", "--dialog=batch", "/tmp/fotos"])
        self.assertEqual(self.provider.get_background_items(file("inode/directory", None)), [])

    def test_extension_errors_do_not_escape_to_file_manager(self):
        with patch.object(module.subprocess, "Popen", side_effect=FileNotFoundError):
            self.provider._run_action(None, [], ["/tmp/a.png"])
        with patch.object(self.provider, "_build_items", side_effect=RuntimeError):
            self.assertEqual(self.provider.get_file_items([]), [])

if __name__ == "__main__":
    unittest.main()
