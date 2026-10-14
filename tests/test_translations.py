# SPDX-License-Identifier: GPL-3.0-or-later
"""Catalog syntax and safety contracts; no translation service is contacted."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('translations', ROOT / 'scripts/translations.py')
translations = importlib.util.module_from_spec(spec)
spec.loader.exec_module(translations)

class TranslationContracts(unittest.TestCase):
    def parse(self, text):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'test.po'
            path.write_text(text)
            return translations.read_po(path)

    def test_unicode_and_multiline_messages_round_trip(self):
        text = 'msgid "Abrir"\n" imagens"\nmsgstr "画像を開く"\n'
        self.assertEqual(self.parse(text), {'Abrir imagens': '画像を開く'})

    def test_duplicate_message_is_rejected(self):
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            self.parse('msgid "A"\nmsgstr "B"\n\nmsgid "A"\nmsgstr "C"\n')

    def test_fuzzy_message_is_rejected(self):
        with self.assertRaisesRegex(ValueError, 'fuzzy'):
            self.parse('#, fuzzy\nmsgid "A"\nmsgstr "B"\n')

    def test_po_text_is_data_not_executable_code(self):
        with self.assertRaises((ValueError, SyntaxError)):
            self.parse('msgid __import__("os").getcwd()\nmsgstr "B"\n')

    def test_exact_community_locale_inventory(self):
        self.assertEqual(translations.languages(), translations.LANGUAGES)
        self.assertEqual(len(translations.languages()), 29)
        self.assertIn('pt_BR', translations.languages())

if __name__ == '__main__':
    unittest.main()
