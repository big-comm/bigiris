#!/usr/bin/env bash
# Regenera data/po/bigiris.pot a partir das fontes listadas em POTFILES.in
# e propaga as novas msgids para cada catalogo .po em LINGUAS via msgmerge.
#
# Uso (a partir da raiz do repo):
#   data/po/regen-pot.sh
#
# Pre-requisitos: gettext (xgettext, msgmerge).

set -euo pipefail

cd "$(dirname "$0")/../.."

POT="data/po/bigiris.pot"
POTFILES="data/po/POTFILES.in"
LINGUAS="data/po/LINGUAS"

# `--language=C` faz xgettext aceitar literais Rust como se fossem C
# (sintaxe de chamada `gettext("…")` e a mesma). Avisos sobre apostrofos
# em comentarios sao falsos-positivos esperados.
xgettext \
  --language=C \
  --keyword=gettext \
  --keyword=n_ \
  --from-code=UTF-8 \
  --package-name=bigiris \
  --package-version=0.1.0 \
  --msgid-bugs-address=https://github.com/big-comm/bigiris/issues \
  --copyright-holder="BigCommunity Team" \
  -o "$POT" \
  $(grep -v '^\s*#' "$POTFILES" | grep -v '^\s*$' | grep '\.rs$')

# Extract Python markers with its own parser, without touching Nautilus' domain.
xgettext --language=Python --keyword=_ --keyword=n_ --from-code=UTF-8 \
  --join-existing --package-name=bigiris --package-version=0.1.0 \
  --msgid-bugs-address=https://github.com/big-comm/bigiris/issues \
  --copyright-holder="BigCommunity Team" -o "$POT" \
  $(grep '\.py$' "$POTFILES")

# Atualiza cada .po com as novas msgids preservando msgstrs existentes.
while read -r lang; do
  [[ -z "$lang" || "$lang" == \#* ]] && continue
  po="data/po/${lang}.po"
  if [[ -f "$po" ]]; then
    msgmerge --update --backup=off --no-fuzzy-matching "$po" "$POT"
  fi
done < "$LINGUAS"

echo "OK — $POT regenerado, .po de cada idioma atualizado."
