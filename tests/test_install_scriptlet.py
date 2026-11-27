# SPDX-License-Identifier: GPL-3.0-or-later
"""The package scriptlet's Thunar hint, against a stub pacman."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPTLET = Path(__file__).resolve().parents[1] / "pkgbuild" / "bigiris.install"
REMOVAL = "This upgrade removes /etc/xdg/Thunar/uca.xml"
HINT = 'run "bigiris install-integrations --user"'


def pre_upgrade(owners, thunar_installed):
    """Run pre_upgrade with `owners` printed by `pacman -Qqo`, one per line."""
    with tempfile.TemporaryDirectory(prefix="bigiris-scriptlet-") as name:
        stubs = Path(name)
        pacman = stubs / "pacman"
        pacman.write_text(
            "#!/bin/sh\n"
            'case "$1" in\n'
            '  -Qqo) [ -n "$OWNERS" ] || exit 1; printf "%s\\n" $OWNERS ;;\n'
            '  -Qq) [ "$THUNAR" = yes ] ;;\n'
            "esac\n",
            encoding="utf-8",
        )
        pacman.chmod(0o755)
        thunar = stubs / "thunar"
        thunar.write_text("#!/bin/sh\n", encoding="utf-8")
        thunar.chmod(0o755 if thunar_installed else 0o644)
        environment = dict(
            os.environ,
            PATH=str(stubs),
            OWNERS=" ".join(owners),
            THUNAR="yes" if thunar_installed else "no",
        )
        result = subprocess.run(
            [
                "/usr/bin/bash",
                "-c",
                'source "$1"; pre_upgrade 2-1 1-1',
                "scriptlet",
                str(SCRIPTLET),
            ],
            env=environment,
            capture_output=True,
            encoding="utf-8",
            check=True,
            timeout=30,
        )
        return result.stdout


class PreUpgradeTests(unittest.TestCase):
    def test_bigiris_among_owners_gets_the_hint(self):
        for owners in (["bigiris"], ["bigiris", "thunar"], ["thunar", "bigiris"]):
            output = pre_upgrade(owners, thunar_installed=True)
            self.assertIn(HINT, output, owners)
            self.assertIn(REMOVAL, output, owners)

    def test_reinstall_advice_needs_thunar(self):
        output = pre_upgrade(["bigiris"], thunar_installed=False)
        self.assertNotIn(REMOVAL, output)

    def test_other_owners_print_nothing(self):
        for owners in ([], ["thunar"], ["bigiris-extra"]):
            self.assertEqual(pre_upgrade(owners, thunar_installed=True), "", owners)


if __name__ == "__main__":
    unittest.main()
