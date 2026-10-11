# Contribuindo

BigIris é Rust + GTK4/libadwaita, single-binary com módulos `Íris`
(viewer) e `Prisma` (transformações). Esse documento cobre o
necessário pra você compilar, testar, traduzir e enviar mudanças
sem fricção.

## Pré-requisitos

Distros baseadas em Arch/BigLinux:

```bash
sudo pacman -S --needed rust pkgconf gtk4 libadwaita dav1d openssl gettext
```

Distros Debian/Ubuntu:

```bash
sudo apt-get install -y rustc cargo pkg-config libgtk-4-dev \
    libadwaita-1-dev libdav1d-dev libssl-dev gettext
```

Rust mínimo: **1.92** (declarado no `Cargo.toml`; `rust-toolchain.toml` seleciona stable).

## Compilando

```bash
# Build sem GUI (rápido, headless — o que CI roda):
cargo build

# Build completo (viewer + diálogos + IA):
cargo build --features "gui ai" -p bigiris

# Release:
cargo build --release --features "gui ai" -p bigiris
```

## Testando

```bash
# Workspace inteiro (headless):
cargo test --workspace --all-targets --locked

# GUI + IA em todo o workspace (precisa das libs de sistema acima):
cargo test --workspace --all-features --all-targets --locked

# Self-test do binário:
./target/release/bigiris --self-test
```

## Qualidade — antes de cada commit

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-features --all-targets --locked -- -D warnings
```

Os 3 são gates obrigatórios no CI (`.github/workflows/ci.yml`).
Warnings viram erro, então PRs com clippy sujo não passam.

## Campanha de qualidade no GNOME

O runner preserva logs, comandos, códigos de saída e duração de cada verificação
em `target/quality/`. Falhas permanecem no histórico; uma nova execução gera
novas evidências. A matriz e os resultados medidos estão em
[docs/QUALITY.md](docs/QUALITY.md).

```bash
# Contratos Rust/Python, Clippy e arquivos de integração:
python3 scripts/quality.py --gui

# Na sessão GNOME, inclui abrir e destruir janelas com imagens sintéticas:
python3 scripts/quality.py --gui --graphical

# Benchmark release, 64 conversões concorrentes e estabilidade por 5 minutos:
python3 scripts/quality.py --gui --graphical --performance --soak-seconds 300

# Instala o auditor apenas no diretório de build deste checkout:
cargo install --locked cargo-audit --root target/quality/tools
python3 scripts/quality.py --gui --audit

# Depois de obter dependências e atualizar a base RustSec, repetição offline:
python3 scripts/quality.py --gui --graphical --audit --offline
```

O runner também exige `desktop-file-validate`, `appstreamcli`,
`glib-compile-schemas` e `msgfmt`. `--graphical` exige Wayland/X11 disponível;
no CI, usamos Xvfb e uma sessão D-Bus isolada. O backend GSettings é de memória
durante os testes gráficos, para preservar as preferências pessoais. O teste
recusa execução direta sem `GSETTINGS_BACKEND=memory`. Inclui preservação do
ponto do cursor no zoom por geometria real e pixels renderizados, além de
cancelamento na navegação e liberação de estado no fechamento.

A suíte CLI com a feature `ai` testa recusa sob RLIMIT_AS de 4 GiB antes do
download, sem cache/saída e com original preservado. Inferência real exige
8 GiB de RAM disponível e modelo na cache isolada; use `scripts/ai_smoke.py`
e consulte os limites em `docs/QUALITY.md`.

Capturas opcionais registram somente os widgets das janelas sintéticas:

```bash
BIGIRIS_TEST_SCREENSHOTS_DIR="$PWD/target/quality/screenshots" \
BIGIRIS_TEST_COLOR_SCHEME=light \
python3 scripts/quality.py --gui --graphical
```

Repita com `BIGIRIS_TEST_COLOR_SCHEME=dark` para o tema escuro. Para Memcheck,
compile o probe e use símbolos de depuração compatíveis com a glibc instalada:

```bash
cargo build -p bigimage-core --example quality_probe --locked
valgrind --tool=memcheck --leak-check=full \
  --errors-for-leak-kinds=definite,indirect --error-exitcode=99 \
  ./target/debug/examples/quality_probe soak 1 320x240
```

O probe mede o núcleo com o allocator padrão; o aplicativo usa mimalloc.
Estabilidade de RSS e liberação das referências GTK são verificações diferentes
de Memcheck e não demonstram, isoladamente, ausência de vazamentos.

## Estrutura

```
crates/
├── bigimage-core/         # decode/encode/transforms — pure Rust, headless
├── bigimage-ai/           # ONNX Runtime + modelos (feature `onnx`)
├── bigimage-integrations/ # service menus pros 6 file managers
└── bigiris/               # binário (CLI + GUI via feature `gui`)
data/
├── com.biglinux.Iris.{desktop,metainfo.xml,gschema.xml}
├── icons/hicolor/scalable/apps/com.biglinux.Iris.svg
├── nautilus/bigiris-menu.py
└── po/                    # catálogos i18n
docs/
└── ADR-00{1,2,3}-*.md     # decisões arquiteturais
pkgbuild/
├── PKGBUILD               # build do remoto
└── PKGBUILD.local         # build do checkout
```

ADRs explicam *por que* o código está assim — leia antes de propor
refatoração estrutural.

## i18n

Strings de UI ficam em `data/po/`. O fluxo:

```bash
# Após adicionar/alterar gettext("…") em código fonte:
data/po/regen-pot.sh

# Edite data/po/<lang>.po pra preencher os msgstr novos.
# msgfmt valida antes de comitar:
msgfmt --check data/po/en.po -o /dev/null
```

A linguagem-fonte é **pt-BR**. Catálogos (`en.po`, futuros `es.po`,
etc.) traduzem a partir do pt-BR.

## Convenções

- **Idioma**: pt-BR para conteúdo de usuário (UI, mensagens, docs);
  inglês para código, identificadores e mensagens de commit.
- **Commits**: estilo Conventional Commits sem ser dogmático —
  `feat:`, `fix:`, `docs:`, `ci:`, `refactor:`, `security:`,
  `i18n:`, `deps:`. Mensagens explicam *por quê*, não *o quê* (o
  diff já mostra o quê).
- **Comentários**: só quando o "por quê" não é óbvio do código.
  Não documente o que um nome bem escolhido já comunica.
- **Errors**: `bigimage-core` define `BigImageError`; binário
  converte para `color_eyre::Result` na borda.
- **No `unsafe`**: todos os crates têm `#![forbid(unsafe_code)]`.
- **GPL-3.0-or-later**: todo arquivo `.rs`/`.py` começa com
  `// SPDX-License-Identifier: GPL-3.0-or-later`.

## Fluxo de PR

1. Abra issue antes pra mudanças não-triviais — alinha escopo.
2. Branch a partir de `main`.
3. Commits pequenos, mensagens informativas.
4. Garanta `cargo fmt --check`, ambos `clippy -D warnings`, e `cargo
   test` passando antes de pedir review.
5. Se a mudança afeta a UI ou o pacote, descreva como testou
   localmente (a CI valida sintaxe, não UX).

## Reportando vulnerabilidades

Não abra PR/issue pública. Veja [SECURITY.md](SECURITY.md).

### Fuzzing e estabilidade prolongada

`python3 scripts/fuzz.py --seconds 120` gera somente imagens sintéticas e
instrumenta o core com ASan/LSan e cobertura LLVM. Use Rust nightly. Em hosts
que fornecem Rust pelo sistema, `--allow-bootstrap` habilita explicitamente
as opções experimentais só nesse build de desenvolvimento; o release não
usa essas opções. O relatório fica em `target/quality/fuzz/summary.json`.
A campanha limita a entrada mutada a 16 KiB e o decode completo a um milhão
de pixels; cabeçalhos maiores continuam inspecionados. Testes de limites e
o sandbox cobrem rejeições fora desse orçamento. Bibliotecas nativas ligadas
externamente não são instrumentadas. Valgrind continua sendo um gate
independente no CI.

`python3 scripts/release_smoke.py` verifica o sandbox e os 13 formatos no
release. `python3 scripts/ai_smoke.py` executa inferência ONNX real em duas
imagens sintéticas com transparência, usando somente o cache isolado em
`target/quality/ai/`. O primeiro download exige `--allow-download`; o teste
não toca no cache pessoal e registra tempo, RAM e preservação de RGB/alpha.

Após `cargo build --release -p bigimage-core --example quality_probe --locked`,
`python3 scripts/soak.py --seconds 28800 --background` executa oito horas do
probe em processo persistente. Consulte `target/quality/long-soak/status.json`:
`running` significa pendente, nunca aprovação. O probe exercita o core;
soak de sessão GNOME, GPU, slideshow e acessibilidade exige validação própria.

Para distribuir a candidata, confira `docs/RELEASE-CANDIDATE.md`. Instalação,
associação MIME padrão e publicação são ações diferentes da criação do pacote.
