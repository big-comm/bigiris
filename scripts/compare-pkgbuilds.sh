#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Fail when pkgbuild/PKGBUILD.local, the recipe CI builds, declares other
# dependencies, licenses, architectures or options than pkgbuild/PKGBUILD,
# the recipe that ships. Only the release recipe needs git, to clone.
set -euo pipefail
cd "$(dirname "$0")/../pkgbuild"

fields='^\s*(arch|license|depends|optdepends|makedepends|options) = '
release=$(makepkg --printsrcinfo -p PKGBUILD | grep -E "$fields" | grep -vx $'\tmakedepends = git' | sort)
local_build=$(makepkg --printsrcinfo -p PKGBUILD.local | grep -E "$fields" | sort)
diff -u --label PKGBUILD --label PKGBUILD.local <(printf '%s\n' "$release") <(printf '%s\n' "$local_build")
