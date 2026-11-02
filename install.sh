#!/usr/bin/env bash
# install.sh — build + instala o BigIris para o usuário atual (~/.local), sem root.
#
# Espelha o que o PKGBUILD faz system-wide, mas com prefixo do usuário:
#   binário, gschema e traduções em $PREFIX; .desktop, ícone, metainfo e
#   service menus dos gerenciadores de arquivos via
#   `bigiris install-integrations --user`. Compila SEMPRE com
#   `--features "gui ai"` (visualizador GTK4 + remover-fundo por IA), que é o
#   ponto fácil de errar ao rodar `cargo build` na mão.
#
# Uso:
#   ./install.sh                 # build + instala em ~/.local
#   PREFIX=~/.local ./install.sh # idem (prefixo customizável)
#   ./install.sh --no-build      # pula o cargo build; instala o target já compilado
#   ./install.sh --bin-only      # só o binário (sem .desktop/ícone/integrações)
#   ./install.sh --uninstall     # remove o que este script instalou em $PREFIX
#   ./install.sh --help
#
# .desktop, ícone e metainfo vão sempre para $XDG_DATA_HOME (~/.local/share),
# onde os ambientes gráficos procuram, mesmo com PREFIX customizado: quem
# os grava é `install-integrations --user`, que também os pula quando o
# pacote da distribuição já os fornece e os remove em
# `bigiris uninstall-integrations`.
#
# Para instalação system-wide, use o pacote da distribuição (há receitas
# para OpenMandriva e Arch em packaging/ e pkgbuild/).

set -euo pipefail

# ── Localização do projeto (funciona de qualquer cwd; tolera espaços no path) ──
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
        -h|--help)
            sed -n '2,/^set -euo/p' "$0" | sed 's/^# \{0,1\}//; /^set -euo/d'
            exit 0 ;;
        *) echo "Argumento desconhecido: $arg (use --help)" >&2; exit 2 ;;
    esac
done

say()  { printf '\033[1;36m▸ %s\033[0m\n' "$*"; }
ok()   { printf '\033[1;32m✓ %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m! %s\033[0m\n' "$*" >&2; }
die()  { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

# ── Desinstalação ─────────────────────────────────────────────────────────────
if [ "$UNINSTALL" -eq 1 ]; then
    SHARE="$PREFIX/share"
    if [ -x "$PREFIX/bin/bigiris" ]; then
        say "Removendo integrações dos gerenciadores de arquivos, .desktop, ícone e metainfo"
        "$PREFIX/bin/bigiris" uninstall-integrations \
            || warn "uninstall-integrations falhou (veja as mensagens acima)"
    fi
    # Cópias literais que versões antigas deste script gravavam em
    # $PREFIX/share; só removidas se idênticas ao arquivo do repositório.
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
            glib-compile-schemas "$SHARE/glib-2.0/schemas" || warn "glib-compile-schemas falhou"
        fi
    else
        # Sem nenhum schema, o cache compilado só guardaria o nosso, já removido.
        rm -f -- "$SHARE/glib-2.0/schemas/gschemas.compiled"
    fi
    if [ -f "$SRC/data/po/LINGUAS" ]; then
        while read -r lang; do
            [ -z "$lang" ] && continue
            case "$lang" in \#*) continue ;; esac
            rm -f -- "$SHARE/locale/${lang}/LC_MESSAGES/bigiris.mo"
        done < "$SRC/data/po/LINGUAS"
    fi
    rm -f -- "$PREFIX/bin/bigiris"
    ok "BigIris removido de $PREFIX"
    exit 0
fi

# ── Pré-requisitos ────────────────────────────────────────────────────────────
say "Verificando dependências"
command -v bwrap >/dev/null || die "bubblewrap ausente — instale o pacote bubblewrap"
command -v prlimit >/dev/null || die "prlimit ausente — instale o pacote util-linux"
command -v cargo  >/dev/null || die "cargo não encontrado — instale Rust e Cargo pela sua distribuição"
command -v msgfmt >/dev/null || warn "msgfmt (gettext) ausente — traduções serão puladas"

# Dependências de runtime para gui+ai. Aviso brando: nem toda distro é Arch.
if [ -f /etc/openmandriva-release ] && command -v dnf >/dev/null; then
    miss=()
    for p in rust cargo lib64gtk4.0-devel lib64adwaita-devel onnxruntime-devel gettext; do
        rpm -q "$p" >/dev/null 2>&1 || miss+=("$p")
    done
    if [ "${#miss[@]}" -gt 0 ]; then
        warn "Pacotes de compilação possivelmente faltando: ${miss[*]}"
        warn "No OpenMandriva: sudo dnf install ${miss[*]}"
    fi
elif command -v pacman >/dev/null; then
    miss=()
    for p in gtk4 libadwaita onnxruntime; do
        pacman -Qq "$p" >/dev/null 2>&1 || pacman -Qq "${p}-cpu" >/dev/null 2>&1 || miss+=("$p")
    done
    if [ "${#miss[@]}" -gt 0 ]; then
        warn "Pacotes possivelmente faltando: ${miss[*]}"
        warn "Instale com: sudo pacman -S ${miss[*]}   (onnxruntime pode ser onnxruntime-cpu)"
    fi
else
    warn "pacman ausente — confirme manualmente: gtk4, libadwaita, onnxruntime instalados."
fi
ok "Checagem de dependências concluída"

# ── Build ─────────────────────────────────────────────────────────────────────
# O target-dir não é necessariamente `$SRC/target`: o ~/.cargo/config.toml
# desta máquina aponta para fora da pasta sincronizada, porque o `target/`
# do Rust reescreve milhares de arquivos por build e afogava o Nextcloud.
# Perguntar ao cargo é o único jeito que respeita config.toml, CARGO_TARGET_DIR
# e o padrão, sem duplicar a regra aqui.
TARGET_DIR=""
if command -v python3 >/dev/null; then
    TARGET_DIR="$(
        cd "$SRC" && cargo metadata --no-deps --format-version 1 2>/dev/null |
            python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])' 2>/dev/null
    )"
fi
[ -n "$TARGET_DIR" ] || TARGET_DIR="${CARGO_TARGET_DIR:-$SRC/target}"
[ -n "$TARGET_DIR" ] || TARGET_DIR="$SRC/target"
BIN="$TARGET_DIR/release/bigiris"
if [ "$BUILD" -eq 1 ]; then
    say "Compilando bigiris (release, features: gui ai) — pode levar alguns minutos"
    ( cd "$SRC" && cargo build --locked --release --features "gui ai" --package bigiris )
    ok "Build concluído"
else
    say "--no-build: usando binário já compilado"
fi
[ -x "$BIN" ] || die "Binário não encontrado em $BIN (target-dir: $TARGET_DIR; rode sem --no-build)"

# Confirma que saiu com gui+ai linkados (evita reinstalar uma build headless).
if ! ldd "$BIN" 2>/dev/null | grep -qi gtk-4; then
    die "O binário NÃO está linkado ao GTK4 — build headless. Recompile com --features \"gui ai\"."
fi

# ── Instalação do binário ─────────────────────────────────────────────────────
say "Instalando binário em $PREFIX/bin"
install -Dm755 "$BIN" "$PREFIX/bin/bigiris"
ok "bigiris → $PREFIX/bin/bigiris"

if [ "$BIN_ONLY" -eq 0 ]; then
    DATA="$SRC/data"
    SHARE="$PREFIX/share"

    # GSettings schema (necessário para o app GUI ler/gravar preferências)
    say "Instalando e compilando gschema"
    install -Dm644 "$DATA/com.biglinux.Iris.gschema.xml" "$SHARE/glib-2.0/schemas/com.biglinux.Iris.gschema.xml"
    if command -v glib-compile-schemas >/dev/null; then
        # `cmd && ok` dentro de um if não dispara o set -e: a falha sumiria.
        if glib-compile-schemas "$SHARE/glib-2.0/schemas"; then
            ok "schemas compilados"
        else
            warn "glib-compile-schemas falhou — o app pode reclamar de schema na 1ª execução"
        fi
    else
        warn "glib-compile-schemas ausente — o app pode reclamar de schema na 1ª execução"
    fi

    # Traduções (.po → .mo) conforme data/po/LINGUAS
    if command -v msgfmt >/dev/null && [ -f "$DATA/po/LINGUAS" ]; then
        say "Compilando traduções"
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
        done < "$DATA/po/LINGUAS"
    fi

    # .desktop, ícone, metainfo e service menus dos gerenciadores de arquivos
    # (pula o que o pacote da distro já fornece e atualiza os caches XDG).
    say "Instalando integrações dos gerenciadores de arquivos (--user)"
    # Uma falha aqui deixa menus de contexto ausentes ou desatualizados, mas
    # o binário já está instalado: o self-test e a dica de PATH ainda rodam,
    # e o script termina com erro no fim.
    if "$PREFIX/bin/bigiris" install-integrations --user; then
        ok "integrações instaladas"
    else
        INTEGRATIONS_FAILED=1
        warn "install-integrations --user falhou (veja as mensagens acima)"
    fi
fi

# ── Verificação final ─────────────────────────────────────────────────────────
say "Self-test"
"$PREFIX/bin/bigiris" --self-test

echo
ok "BigIris instalado em $PREFIX"
case ":$PATH:" in
    *":$PREFIX/bin:"*) : ;;
    *) warn "$PREFIX/bin não está no PATH — adicione: export PATH=\"$PREFIX/bin:\$PATH\"" ;;
esac
if [ "$INTEGRATIONS_FAILED" -eq 1 ]; then
    die "Integrações incompletas: corrija o erro acima e rode \"$PREFIX/bin/bigiris install-integrations --user\""
fi
