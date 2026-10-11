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
    # Ubuntu awk resolves through /etc/alternatives, which is deliberately
    # absent. Shell builtins keep this probe independent of those links.
    seen_cap=false
    while read -r key value rest; do
      if test "$key" = CapEff:; then
        test "$value" = 0000000000000000
        seen_cap=true
      fi
    done < /proc/self/status
    test "$seen_cap" = true
    seen_loopback=false
    while read -r interface rest; do
      case "$interface" in
        lo:) seen_loopback=true ;;
        *:) exit 1 ;;
      esac
    done < /proc/net/dev
    test "$seen_loopback" = true
  '
