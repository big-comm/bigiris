#!/usr/bin/env bash
# install.sh: build and install BigIris for the current user (~/.local), without root.
#
# Mirrors what the PKGBUILD does system-wide, under the user's prefix:
#   binary, gschema and translations in $PREFIX; .desktop, icon, metainfo and
#   file manager service menus through `bigiris install-integrations --user`.
#   Always builds with `--features "gui ai"` (GTK4 viewer and AI background
#   removal), the part a manual `cargo build` most easily gets wrong.
#
# Usage:
#   ./install.sh                 # build and install into ~/.local
#   PREFIX=~/.local ./install.sh # same, with a custom prefix
#   ./install.sh --no-build      # skip cargo build; install the binary already built
#   ./install.sh --bin-only      # binary only (no .desktop, icon or integrations)
#   ./install.sh --uninstall     # remove what this script installed in $PREFIX
#   ./install.sh --help
#
# .desktop, icon and metainfo always go to $XDG_DATA_HOME (~/.local/share),
# where desktops look for them, even with a custom PREFIX: they are written by
# `install-integrations --user`, which skips them when the distribution
# package already provides them, and removed by `bigiris uninstall-integrations`.
#
# For a system-wide install, use the distribution package (recipes for
# OpenMandriva and Arch live in packaging/ and pkgbuild/).

set -euo pipefail

# Project location (works from any cwd and with spaces in the path)
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
BUILD=1
BIN_ONLY=0
UNINSTALL=0
INTEGRATIONS_FAILED=0

for arg in "$@"; do
    case "$arg" in
    --no-build) BUILD=0 ;;
    --bin-only) BIN_ONLY=1 ;;
    --uninstall) UNINSTALL=1 ;;
    -h | --help)
        sed -n '2,/^set -euo/p' "$0" | sed 's/^# \{0,1\}//; /^set -euo/d'
        exit 0
        ;;
    *)
        echo "Unknown argument: $arg (see --help)" >&2
        exit 2
        ;;
    esac
done

say() { printf '\033[1;36m▸ %s\033[0m\n' "$*"; }
ok() { printf '\033[1;32m✓ %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m! %s\033[0m\n' "$*" >&2; }
die() {
    printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2
    exit 1
}

# Uninstall
if [ "$UNINSTALL" -eq 1 ]; then
    SHARE="$PREFIX/share"
    if [ -x "$PREFIX/bin/bigiris" ]; then
        say "Removing file manager integrations, .desktop, icon and metainfo"
        "$PREFIX/bin/bigiris" uninstall-integrations ||
            warn "uninstall-integrations failed (see the messages above)"
    fi
    # Earlier versions of this script copied these files into $PREFIX/share;
    # a copy is removed only while it matches the repository file.
    for f in applications/com.biglinux.Iris.desktop metainfo/com.biglinux.Iris.metainfo.xml \
        icons/hicolor/scalable/apps/com.biglinux.Iris.svg; do
        src="$SRC/data/${f##*/}"
        case "$f" in icons/*) src="$SRC/data/$f" ;; esac
        if [ -f "$SHARE/$f" ] && cmp -s "$src" "$SHARE/$f"; then
            rm -f -- "$SHARE/$f"
        fi
    done
    rm -f -- "$SHARE/glib-2.0/schemas/com.biglinux.Iris.gschema.xml"
    if compgen -G "$SHARE/glib-2.0/schemas/*.xml" >/dev/null; then
        if command -v glib-compile-schemas >/dev/null; then
            glib-compile-schemas "$SHARE/glib-2.0/schemas" || warn "glib-compile-schemas failed"
        fi
    else
        # With no schema left, the compiled cache would hold only ours, already removed.
        rm -f -- "$SHARE/glib-2.0/schemas/gschemas.compiled"
    fi
    if [ -f "$SRC/data/po/LINGUAS" ]; then
        while read -r lang; do
            [ -z "$lang" ] && continue
            case "$lang" in \#*) continue ;; esac
            rm -f -- "$SHARE/locale/${lang}/LC_MESSAGES/bigiris.mo"
        done <"$SRC/data/po/LINGUAS"
    fi
    rm -f -- "$PREFIX/bin/bigiris"
    ok "BigIris removed from $PREFIX"
    exit 0
fi

# Prerequisites
say "Checking dependencies"
command -v bwrap >/dev/null || die "bubblewrap is missing: install the bubblewrap package"
command -v prlimit >/dev/null || die "prlimit is missing: install the util-linux package"
command -v cargo >/dev/null || die "cargo not found: install Rust and Cargo from your distribution"
command -v msgfmt >/dev/null || warn "msgfmt (gettext) is missing: translations will be skipped"

# Build and runtime dependencies for gui+ai. Only a warning: not every distribution is Arch.
if [ -f /etc/openmandriva-release ] && command -v dnf >/dev/null; then
    miss=()
    for p in rust cargo lib64gtk4.0-devel lib64adwaita-devel shaderc gettext; do
        rpm -q "$p" >/dev/null 2>&1 || miss+=("$p")
    done
    if [ "${#miss[@]}" -gt 0 ]; then
        warn "Build packages possibly missing: ${miss[*]}"
        warn "On OpenMandriva: sudo dnf install ${miss[*]}"
    fi
elif command -v pacman >/dev/null; then
    miss=()
    for p in gtk4 libadwaita shaderc; do
        pacman -Qq "$p" >/dev/null 2>&1 || miss+=("$p")
    done
    if [ "${#miss[@]}" -gt 0 ]; then
        warn "Packages possibly missing: ${miss[*]}"
        warn "Install them with: sudo pacman -S ${miss[*]}"
    fi
    ai=()
    for p in birefnet-native realesrgan-native scunet-native lama-native; do
        pacman -Qq "$p" >/dev/null 2>&1 || ai+=("$p")
    done
    if ((${#ai[@]} > 0)); then
        warn "Optional AI tools without weights: ${ai[*]}"
        warn "To use them: sudo pacman -S ${ai[*]}"
    fi
else
    warn "pacman not found: check by hand that gtk4, libadwaita and glslc (shaderc) are installed."
fi
ok "Dependency check finished"

# Build
# The target directory is not necessarily `$SRC/target`: only asking cargo
# honors config.toml, CARGO_TARGET_DIR and the default without restating the rule.
TARGET_DIR=""
if command -v python3 >/dev/null; then
    # A failed query falls back below instead of ending the script under set -e.
    TARGET_DIR="$(
        cd "$SRC" && cargo metadata --no-deps --format-version 1 2>/dev/null |
            python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])' 2>/dev/null
    )" || TARGET_DIR=""
fi
[ -n "$TARGET_DIR" ] || TARGET_DIR="${CARGO_TARGET_DIR:-$SRC/target}"
BIN="$TARGET_DIR/release/bigiris"
if [ "$BUILD" -eq 1 ]; then
    say "Building bigiris (release, features: gui ai); this can take a few minutes"
    (cd "$SRC" && cargo build --locked --release --features "gui ai" --package bigiris)
    ok "Build finished"
else
    say "--no-build: using the binary already built"
fi
[ -x "$BIN" ] || die "Binary not found at $BIN (target-dir: $TARGET_DIR; run without --no-build)"

# Refuse a headless build, which would replace the GUI binary. The output is
# captured whole: `ldd | grep -q` fails under pipefail when ldd dies of SIGPIPE.
if [[ $(ldd "$BIN" 2>/dev/null) != *[Gg][Tt][Kk]-4* ]]; then
    die "The binary is NOT linked against GTK4 (headless build). Rebuild with --features \"gui ai\"."
fi

# Binary
say "Installing the binary into $PREFIX/bin"
install -Dm755 "$BIN" "$PREFIX/bin/bigiris"
ok "bigiris → $PREFIX/bin/bigiris"

if [ "$BIN_ONLY" -eq 0 ]; then
    DATA="$SRC/data"
    SHARE="$PREFIX/share"

    # The GUI reads and writes its preferences through this GSettings schema.
    say "Installing and compiling the gschema"
    install -Dm644 "$DATA/com.biglinux.Iris.gschema.xml" "$SHARE/glib-2.0/schemas/com.biglinux.Iris.gschema.xml"
    if command -v glib-compile-schemas >/dev/null; then
        # `cmd && ok` inside an if does not trigger set -e: the failure would vanish.
        if glib-compile-schemas "$SHARE/glib-2.0/schemas"; then
            ok "schemas compiled"
        else
            warn "glib-compile-schemas failed: the app may report a missing schema on first run"
        fi
    else
        warn "glib-compile-schemas is missing: the app may report a missing schema on first run"
    fi

    # Translations (.po → .mo) for every locale in data/po/LINGUAS.
    if command -v msgfmt >/dev/null && [ -f "$DATA/po/LINGUAS" ]; then
        say "Compiling translations"
        I18N_TMP="$(mktemp -d -t bigiris-i18n.XXXXXX)"
        trap 'rm -rf -- "$I18N_TMP"' EXIT
        while read -r lang; do
            [ -z "$lang" ] && continue
            case "$lang" in \#*) continue ;; esac
            if [ -f "$DATA/po/${lang}.po" ]; then
                msgfmt --check -o "$I18N_TMP/${lang}.mo" "$DATA/po/${lang}.po"
                install -Dm644 "$I18N_TMP/${lang}.mo" "$SHARE/locale/${lang}/LC_MESSAGES/bigiris.mo"
                ok "locale: $lang"
            fi
        done <"$DATA/po/LINGUAS"
    fi

    # .desktop, icon, metainfo and file manager service menus; skips what the
    # distribution package already provides and refreshes the XDG caches.
    say "Installing file manager integrations (--user)"
    # A failure here leaves context menus missing or stale, but the binary is
    # already installed: the self-test and the PATH hint still run, and the
    # script exits with an error at the end.
    if "$PREFIX/bin/bigiris" install-integrations --user; then
        ok "integrations installed"
    else
        INTEGRATIONS_FAILED=1
        warn "install-integrations --user failed (see the messages above)"
    fi
fi

# Final check
say "Self-test"
"$PREFIX/bin/bigiris" --self-test

echo
ok "BigIris installed in $PREFIX"
case ":$PATH:" in
*":$PREFIX/bin:"*) : ;;
*) warn "$PREFIX/bin is not on PATH; add: export PATH=\"$PREFIX/bin:\$PATH\"" ;;
esac
if [ "$INTEGRATIONS_FAILED" -eq 1 ]; then
    die "Integrations incomplete: fix the error above and run \"$PREFIX/bin/bigiris install-integrations --user\""
fi
