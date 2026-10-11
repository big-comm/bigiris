#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Configure an ephemeral Ubuntu CI runner, never a user's installation.
set -euo pipefail

# Use Ubuntu's packaged policy, including the child profile that denies
# capabilities. Do not disable AppArmor or the global userns restriction.
# https://discourse.ubuntu.com/t/understanding-apparmor-user-namespace-restriction/58007
sudo apt-get install -y --no-install-recommends apparmor-profiles
profile=/usr/share/apparmor/extra-profiles/bwrap-userns-restrict
test -f "$profile"
sudo apparmor_parser --replace "$profile"
sudo grep -qx 'bwrap (enforce)' /sys/kernel/security/apparmor/profiles
sudo grep -qx 'unpriv_bwrap (enforce)' /sys/kernel/security/apparmor/profiles

# Run as the ordinary runner user. Fail before compiling if namespaces or
# child confinement are unavailable; the application self-test follows later.
/usr/bin/bwrap --unshare-all --die-with-parent --new-session --clearenv \
  --cap-drop ALL --ro-bind /usr /usr \
  --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib64 /lib64 \
  --proc /proc --dev /dev --remount-ro /dev \
  /bin/sh -eu -c '
    test ! -e /home
    test ! -e /run
    test ! -e /etc
    awk '\''$1 == "CapEff:" { seen = 1; if ($2 != "0000000000000000") exit 1 }
         END { if (!seen) exit 1 }'\'' /proc/self/status
    awk '\''NR > 2 && $1 != "lo:" { exit 1 }'\'' /proc/net/dev
  '
