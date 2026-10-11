<p align="center">
  <img src="data/icons/hicolor/scalable/apps/com.biglinux.Iris.svg" alt="BigIris" width="160">
</p>

<h1 align="center">BigIris</h1>

<p align="center">
  <strong>Um visualizador rápido e um estúdio de imagens sob demanda.</strong><br>
  Rust · GTK4/libadwaita · CLI · processamento local · GNOME/Nautilus e outros desktops
</p>

BigIris segue uma ideia simples: abrir uma foto deve ser imediato e silencioso;
trabalhar em muitas fotos deve oferecer uma interface completa. Por isso o
projeto tem duas experiências no mesmo binário:

- **Íris** é o visualizador minimalista: abre no arquivo escolhido, descobre as
  imagens irmãs da pasta, navega em ordem natural e mantém a foto no centro.
- **Prisma** aparece quando é necessário converter, redimensionar, recortar,
  ajustar, comparar, animar ou processar uma pasta inteira.

O fluxo compacto e os atalhos têm o IrfanView como referência funcional. A
interface usa GTK4/libadwaita, com prioridade para o BigCommunity GNOME
e integração também com outros desktops. O resultado respeita o tema,
usa ícones simbólicos do sistema e mantém as operações avançadas fora do caminho
principal.

## Destaques

- Decodificação da imagem e das miniaturas fora da thread gráfica.
- Navegação rápida por pasta com coalescência: resultados antigos são descartados
  quando o usuário percorre várias imagens rapidamente.
- Zoom centrado no cursor, pan por arrasto, 1:1, ajustar à janela, tela cheia,
  slideshow, fita de miniaturas e histograma RGB.
- Giro e espelhamento não destrutivos, com desfazer/refazer; `Ctrl+S` confirma e
  grava por substituição atômica, preservando o original se o encoder falhar.
- Processamento em lote paralelo, cancelável, com progresso, tempo e tamanho
  antes/depois.
- Integração com Dolphin, Nautilus, Nemo, Thunar, PCManFM-Qt e elementary Files.
- Ação de pasta no Nautilus e no Dolphin: **BigIris → Processar esta pasta…**.
- Remoção de fundo local com BiRefNet-lite, modelo verificado por SHA-256 e sem
  upload das imagens.
- Limites preventivos de 1 GiB por arquivo, 256 MP e 512 MiB por buffer de imagem.

## Zoom e memória da IA

Posicione o cursor sobre o detalhe da imagem e use a roda do mouse, `+`/`-`
ou os botões de zoom. A ampliação preserva esse ponto, respeitando os limites
de rolagem da imagem. `1` exibe 100%; `0` ajusta à janela. Sem posição conhecida
do cursor, o ponto de referência é o centro da área visível.

A remoção de fundo é opcional. O modelo atual, BiRefNet-lite FP32, atingiu
aproximadamente 6,53 GiB de RAM no ensaio local. O aplicativo exige um orçamento
inicial de **8 GiB de RAM livre** antes de baixar/carregar esse modelo e volta
a conferir o orçamento antes da inferência. Considera a memória disponível do
Linux, limites cgroup v2 e de espaço virtual; swap não substitui essa margem.
Isso é uma proteção preventiva, não uma quota rígida de memória.

Em uma máquina com 4 GB, a remoção de fundo por esse modelo é recusada com
explicação. O visualizador e as conversões continuam disponíveis, sujeitos ao
tamanho das imagens e aos limites do pipeline. Ainda é necessária homologação
em hardware físico de pouca RAM; não há promessa de suporte a qualquer acervo.

## Formatos

O build padrão completo lê e grava os formatos fornecidos pelo pipeline atual:
PNG, JPEG, WebP, AVIF, TIFF, BMP, GIF, ICO, PNM, TGA, QOI, Radiance HDR e
OpenEXR. HEIC/HEIF, JPEG XL, SVG e RAW de câmeras ainda não são anunciados como
suportados.

As conversões estáticas reencodam os pixels e removem metadados pessoais.
Perfis ICC RGB são normalizados para sRGB; PNG/JPEG recebem um perfil sRGB
gerado sem dados da imagem original. Edição estática de animações e TIFF
multipágina é recusada para impedir perda de conteúdo. O viewer exibe o
primeiro quadro/página; reprodução animada e navegação multipágina permanecem pendentes. WebP é gravado
sem perdas; ICO é limitado a 256 × 256. Consulte as limitações em
[docs/QUALITY.md](docs/QUALITY.md) antes de converter um acervo.

## Instalação no BigCommunity GNOME

O runtime exige `bubblewrap` e `util-linux`, com namespaces não privilegiados
disponíveis. O aplicativo recusa decodificação se o isolamento não puder iniciar.

Para compilar e empacotar o checkout local na base Arch:

```bash
cd pkgbuild
makepkg -si -p PKGBUILD.local
```

A integração com o Nautilus depende de `nautilus-python`. A campanha de testes
prioriza GNOME/Wayland; veja os resultados, limites e critérios para adoção
oficial em [docs/QUALITY.md](docs/QUALITY.md), e os comandos reproduzíveis em
[CONTRIBUTING.md](CONTRIBUTING.md#campanha-de-qualidade-no-gnome).

## Instalação no OpenMandriva

Para instalar o checkout apenas no seu usuário:

```bash
sudo dnf install rust cargo lib64gtk4.0-devel lib64adwaita-devel \
  onnxruntime-devel gettext bubblewrap util-linux
./install.sh
```

O instalador compila a versão release, coloca o aplicativo em `~/.local`,
registra somente os gerenciadores de arquivos detectados e atualiza o cache do
Plasma (`kbuildsycoca6`). Se o menu não aparecer em uma janela antiga do Dolphin,
feche essa janela e abra outra.

Para o ABF/Cooker, use a receita e as instruções em
[`packaging/openmandriva/`](packaging/openmandriva/README.md). Ela usa um
tarball vendorizado para que o build seja offline e reproduzível.

## Outras instalações

No Arch, BigLinux ou Manjaro:

```bash
cd pkgbuild
makepkg -si
```

Em outras distribuições, após instalar Rust, GTK4, libadwaita, dav1d, OpenSSL,
gettext e ONNX Runtime:

```bash
cargo build --locked --release --features "gui ai" -p bigiris
./install.sh --no-build
```

Para remover as integrações pessoais antes de apagar o binário:

```bash
bigiris uninstall-integrations
```

## Uso

```bash
# Visualizador / pasta
bigiris foto.jpg
bigiris *.png
bigiris

# Prisma
bigiris --dialog=batch pasta-com-fotos
bigiris --dialog=convert *.jpg
bigiris --dialog=resize *.png
bigiris --dialog=crop foto.jpg
bigiris --dialog=adjust *.jpg
bigiris --dialog=compare antes.png depois.png
bigiris --dialog=remove-bg foto.jpg

# CLI
bigiris convert --to avif --overwrite increment *.jpg
bigiris resize --max-edge 1080 *.png
bigiris rotate --degrees 90 foto.jpg
bigiris flip --axis horizontal foto.jpg
bigiris crop --rect 800x600+100+50 foto.jpg
bigiris adjust --brightness 15 --contrast 10 foto.jpg
bigiris animate -o saida.gif --delay 100 frame_*.png
bigiris remove-bg foto.jpg
bigiris to-pdf documento.docx
bigiris batch --profile docs/batch-profile.example.toml pasta-com-fotos

# Integrações e diagnóstico
bigiris install-integrations --user
bigiris uninstall-integrations
bigiris --self-test
```

### Atalhos do visualizador

| Atalho | Ação |
|---|---|
| `←` / `→`, `Home` / `End` | Navegar pela pasta |
| roda do mouse, `+` / `-` | Zoom |
| `0` / `1` | Ajustar / tamanho real |
| arrastar | Pan |
| `R` / `Shift+R` | Girar à direita / esquerda |
| `Ctrl+Z` / `Ctrl+Shift+Z` | Desfazer / refazer |
| `Ctrl+S` | Salvar edições com segurança |
| `Space` | Slideshow |
| `F` / `G` / `T` | Miniaturas / histograma / modo teatro |
| `H` | Fixar controles visíveis |
| `F11` / `Esc` | Tela cheia / fechar |

## Arquitetura

```text
bigiris/
├── crates/
│   ├── bigimage-core/          pipeline headless e transformações
│   ├── bigimage-ai/            modelos locais e download verificado
│   ├── bigimage-integrations/  ServiceMenus e ações de gerenciadores
│   └── bigiris/                CLI, viewer Íris e diálogos Prisma
├── data/                       desktop, AppStream, GSettings, ícone e i18n
├── packaging/openmandriva/     spec ABF e criação do vendor tarball
└── pkgbuild/                   pacotes Arch/BigLinux
```

Os lotes compartilham um único pool em
[`batch_runner.rs`](crates/bigiris/src/gui/batch_runner.rs). A alocação global
usa mimalloc, o resize usa SIMD por `fast_image_resize`, e o pool limita a concorrência
conforme CPU, dimensões das imagens e memória disponível (até quatro jobs).

## Desenvolvimento e qualidade

Rust stable 1.92 ou posterior:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --locked
cargo build --release --features "gui ai" -p bigiris
./target/release/bigiris --self-test

desktop-file-validate data/com.biglinux.Iris.desktop
appstreamcli validate --no-net data/com.biglinux.Iris.metainfo.xml
glib-compile-schemas --strict --dry-run data
```

Relatos e patches são bem-vindos: leia [CONTRIBUTING.md](CONTRIBUTING.md).
Vulnerabilidades devem seguir [SECURITY.md](SECURITY.md), sem issue pública.

## Licença

GPL-3.0-or-later. Consulte [LICENSE](LICENSE).
