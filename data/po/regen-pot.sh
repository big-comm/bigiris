#!/usr/bin/env bash
# Regenerate data/po/bigiris.pot from the sources in POTFILES.in and merge
# the new msgids into each catalog listed in LINGUAS.
#
# Usage, from the repository root:
#   data/po/regen-pot.sh
#
# Requires gettext (xgettext, msgmerge).

set -euo pipefail

cd "$(dirname "$0")/../.."

POT="data/po/bigiris.pot"
POTFILES="data/po/POTFILES.in"
LINGUAS="data/po/LINGUAS"

# The C scanner reads Rust's `gettext("…")` calls but skips `#` lines as
# preprocessor directives, so it scans copies with `#[` turned into ` [`.
scan=$(mktemp -d)
trap 'rm -rf "$scan"' EXIT
mapfile -t rust_sources < <(grep -v '^\s*#' "$POTFILES" | grep -v '^\s*$' | grep '\.rs$')
for source in "${rust_sources[@]}"; do
  mkdir -p "$scan/${source%/*}"
  sed 's/^\([[:space:]]*\)#\[/\1 [/' "$source" >"$scan/$source"
done
xgettext \
  --directory="$scan" \
  --language=C \
  --keyword=gettext \
  --keyword=n_ \
  --from-code=UTF-8 \
  --package-name=bigiris \
  --package-version=0.1.0 \
  --msgid-bugs-address=https://github.com/big-comm/bigiris/issues \
  --copyright-holder="BigCommunity Team" \
  -o "$POT" \
  "${rust_sources[@]}"

# Extract Python markers with its own parser, without touching Nautilus' domain.
xgettext --language=Python --keyword=_ --keyword=n_ --from-code=UTF-8 \
  --join-existing --package-name=bigiris --package-version=0.1.0 \
  --msgid-bugs-address=https://github.com/big-comm/bigiris/issues \
  --copyright-holder="BigCommunity Team" -o "$POT" \
  $(grep '\.py$' "$POTFILES")

# Merge the new msgids into each catalog, keeping existing translations.
while read -r lang; do
  [[ -z "$lang" || "$lang" == \#* ]] && continue
  po="data/po/${lang}.po"
  if [[ -f "$po" ]]; then
    msgmerge --update --backup=off --no-fuzzy-matching "$po" "$POT"
  fi
done < "$LINGUAS"

echo "OK: regenerated $POT and updated every catalog."
