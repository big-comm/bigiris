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

A tela inicial abre em uma janela compacta. Use **Abrir imagens**, arraste uma
seleção de arquivos locais ou cole arquivos copiados no gerenciador com
`Ctrl+V`. **Processar uma pasta** abre as ferramentas de lote.

## Destaques

- Decodificação da imagem e das miniaturas fora da thread gráfica.
- Navegação rápida por pasta com coalescência: resultados antigos são descartados
  quando o usuário percorre várias imagens rapidamente.
- Zoom centrado no cursor, pan por arrasto, 1:1, ajustar à janela, tela cheia,
  slideshow, fita de miniaturas e histograma RGB.
- Giro, espelhamento e recorte não destrutivos, com desfazer/refazer. O recorte
  acontece direto na imagem aberta (`Ctrl+R`): arraste os cantos ou as bordas,
  escolha uma proporção e confirme com `Enter`. `Ctrl+S` confirma e grava por
  substituição atômica, preservando o original se o encoder falhar.
- Painel de edição lateral e translúcido (botão **Editar** ou `E`): girar,
  espelhar, endireitar (−45° a +45°, com grade e recorte automático das bordas
  vazias) e recortar; brilho, contraste, saturação e gama com prévia ao vivo;
  redimensionar com proporção travada ou em porcentagem; remover fundo,
  remover ruído e aumentar resolução com IA direto na imagem, com progresso e
  botão de cancelar;
  **Salvar** e **Salvar como…** (grava uma cópia editada, sem tocar no
  original). Cada gesto vira um passo de desfazer, e o resultado das
  ferramentas pesadas fica guardado para que as edições seguintes não as
  repitam. **Ver original** (ou segurar `\`) mostra a imagem sem as edições.
- Processamento em lote paralelo, cancelável, com progresso, tempo e tamanho
  antes/depois.
- Integração com Dolphin, Nautilus, Nemo, Thunar, PCManFM-Qt e elementary Files.
- Ação de pasta no Nautilus e no Dolphin: **BigIris → Processar esta pasta…**.
- IA local, sem upload das imagens e sem downloads: remoção de fundo
  (BiRefNet-lite), aumento de resolução (Real-ESRGAN), remoção de ruído
  (SCUNet) e apagar objetos por máscara (LaMa, por enquanto na CLI). Veja
  [IA local](#ia-local).
- Limites preventivos de 1 GiB por arquivo, 256 MP e 512 MiB por buffer de imagem.

## Zoom

Posicione o cursor sobre o detalhe da imagem e use a roda do mouse, `+`/`-`
ou os botões de zoom. A transição é gradual, com movimentos proporcionais no
touchpad, e respeita a preferência de desativar animações do GTK. A ampliação
preserva esse ponto, respeitando os limites
de rolagem da imagem. `1` exibe 100%; `0` ajusta à janela. Sem posição conhecida
do cursor, o ponto de referência é o centro da área visível.

## IA local

As ferramentas de IA usam os motores do
[image-engine-native](https://github.com/biglinux/image-engine-native), do
BigLinux: os modelos escritos em Rust puro, sem ONNX Runtime, PyTorch ou
Python. Rodam na GPU por Vulkan 1.2 quando há uma (a primeira carga na GPU
compila os shaders, alguns segundos, e fica em cache) e na CPU caso contrário.
O dispositivo em uso aparece na dica das ferramentas do painel e na saída da
CLI. Os pesos **não são baixados**: vêm de pacotes do sistema, um por
ferramenta, e cada ferramenta funciona sem as outras.

| Ferramenta | Modelo | Pacote | Pesos |
|---|---|---|---|
| Remover fundo | BiRefNet-lite (MIT) | `birefnet-native` | 90 MB |
| Aumentar resolução | Real-ESRGAN general x4v3 (BSD-3-Clause) | `realesrgan-native` | 4,9 MB |
| Remover ruído | SCUNet (Apache-2.0) | `scunet-native` | 37 MB |
| Apagar objeto | Big LaMa (Apache-2.0) | `lama-native` | 102 MB |

```bash
sudo pacman -S birefnet-native realesrgan-native scunet-native lama-native
```

O pacote do BigLinux já instala o `realesrgan-native` (4,9 MB) como
dependência; os outros pesos são opcionais. Sem o pacote, a ferramenta aparece
indisponível e diz qual pacote instalar; sem `realesrgan-native` (instalação
manual ou outra distribuição), **Aumentar resolução** usa Lanczos3. Para testes e
desenvolvimento, `BIGIRIS_BIREFNET_WEIGHTS`, `BIGIRIS_REALESRGAN_WEIGHTS`,
`BIGIRIS_SCUNET_WEIGHTS` e `BIGIRIS_LAMA_WEIGHTS` apontam para outros arquivos
de pesos; `IMAGE_ENGINE_NATIVE_GPU=0` força a CPU.

Memória medida neste projeto (processo inteiro, CPU): cerca de 0,3 GB para a
remoção de fundo, 0,1 a 0,4 GB mais a imagem de saída para o Real-ESRGAN
(trabalha em blocos), 0,4 a 0,7 GB para o SCUNet (também em blocos) e 0,25 GB
a 0,5 GB para o LaMa, que roda só numa caixa em volta da máscara, reduzida a
1024 px no lado maior. Antes de carregar e de cada execução o aplicativo
confere se a estimativa cabe na memória livre agora (memória disponível do
Linux, limites cgroup v2 e de espaço virtual; swap não conta) e recusa com
explicação se não couber. O visualizador e as conversões seguem disponíveis.

As redes trabalham em sRGB de 8 bits: imagens de 16 bits ou de ponto
flutuante saem em 8 bits destas ferramentas (o Lanczos3 mantém a
profundidade). O canal alfa é preservado: ampliado com Lanczos3 no Real-ESRGAN,
intacto no SCUNet e no LaMa.

## Formatos

O build padrão completo lê e grava os formatos fornecidos pelo pipeline atual:
PNG, JPEG, WebP, AVIF, TIFF, BMP, GIF, ICO, PNM, TGA, QOI, Radiance HDR e
OpenEXR. HEIC/HEIF, JPEG XL, SVG e RAW de câmeras ainda não são anunciados como
suportados.

As conversões estáticas reencodam os pixels e removem metadados pessoais.
Perfis ICC RGB são normalizados para sRGB; PNG/JPEG recebem um perfil sRGB
gerado sem dados da imagem original. Edição estática de animações e TIFF
multipágina é recusada para impedir perda de conteúdo. O viewer exibe o
primeiro quadro/página; reprodução animada e navegação multipágina permanecem pendentes. ICO é
limitado a 256 × 256. Consulte as limitações em
[docs/QUALITY.md](docs/QUALITY.md) antes de converter um acervo.

### Qualidade e tamanho

Cada conversão procura **o menor arquivo que mantém a qualidade visual
escolhida**, medida com SSIMULACRA2 contra os pixels gravados (imagens com
transparência são avaliadas sobre fundo preto e branco, valendo a pior nota):

| Modo (`--target`) | Nota mínima | Uso |
|---|---|---|
| `maxima` (`max`) | 85 | diferença só aparece comparando com zoom |
| `equilibrado` (`balanced`) — padrão | 80 | sem perda visível em uso normal |
| `menor` (`small`) | 70 | alta qualidade, arquivos menores |

- **JPEG**: mozjpeg (progressivo, trellis, Huffman otimizado), 4:2:0 e 4:4:4
  quando as cores exigem.
- **WebP**: libwebp com perdas (equivalente a `cwebp -m 6 -sharp_yuv -af`),
  comparado com versões sem perdas e quase sem perdas; vence a menor que
  atinge a meta.
- **AVIF**: ravif 10 bits; busca rápida e codificação final mais lenta.
- **PNG**: otimização sem perdas (oxipng); paleta de 256 cores só quando atinge
  a meta e fica menor (nunca em `maxima`; 16 bits só em `menor`).
- **GIF**: paleta exata ou imagequant, com o pontilhado escolhido pela nota.
- **ICO, BMP, TGA, TIFF, OpenEXR**: o menor formato sem perdas de cada um
  (PNG otimizado, paleta/24 bits, RLE, Deflate com preditor, ZIP/PIZ).
- **QOI, PNM, HDR** não têm parâmetros de tamanho.

`--quality N` (1 a 100) fixa a qualidade de JPEG/WebP/AVIF sem busca, como
antes; em WebP, 100 grava sem perdas. Ao converter para o **mesmo formato**,
se o resultado ficasse maior que o original e o original não tiver metadados
pessoais (EXIF, XMP, textos), o original é copiado sem recompressão.

## Instalação no BigCommunity GNOME

O runtime exige `bubblewrap` e `util-linux`, com namespaces não privilegiados
disponíveis. O aplicativo recusa decodificação se o isolamento não puder iniciar.

Para compilar e empacotar o checkout local na base Arch:

```bash
cd pkgbuild
makepkg -si -p PKGBUILD.local
```

A integração com o Nautilus depende de `nautilus-python`. O pacote instala a
extensão no sistema; não é necessário executar `install-integrations --user`.
Para carregar a extensão após instalar ou atualizar, feche o Nautilus com
`nautilus -q` e abra-o novamente.

O pacote inclui catálogos para os 29 idiomas adotados pelos aplicativos
BigCommunity. A interface e a extensão Nautilus seguem o idioma da sessão;
veja o inventário e o fluxo de manutenção em [CONTRIBUTING.md](CONTRIBUTING.md#i18n).

A campanha de testes prioriza GNOME/Wayland; veja os resultados, limites e
critérios para adoção oficial em [docs/QUALITY.md](docs/QUALITY.md), e os
comandos reproduzíveis em
[CONTRIBUTING.md](CONTRIBUTING.md#campanha-de-qualidade-no-gnome).

## Instalação no OpenMandriva

Para instalar o checkout apenas no seu usuário:

```bash
sudo dnf install rust cargo lib64gtk4.0-devel lib64adwaita-devel \
  shaderc gettext bubblewrap util-linux
./install.sh
```

O instalador compila a versão release, coloca o aplicativo em `~/.local`,
registra somente os gerenciadores de arquivos detectados e atualiza o cache do
Plasma (`kbuildsycoca6`). Se o menu não aparecer em uma janela antiga do Dolphin,
feche essa janela e abra outra.

Os pacotes de pesos da IA ainda não existem no OpenMandriva: lá as ferramentas
de IA aparecem indisponíveis (e o aumento de resolução usa Lanczos3).

Para o ABF/Cooker, use a receita e as instruções em
[`packaging/openmandriva/`](packaging/openmandriva/README.md). Ela usa um
tarball vendorizado para que o build seja offline e reproduzível.

## Outras instalações

No Arch, BigLinux ou Manjaro:

```bash
cd pkgbuild
makepkg -si
```

Em outras distribuições, após instalar Rust (1.95 ou posterior), GTK4,
libadwaita, dav1d, gettext e `glslc` (do shaderc, usado só na compilação):

```bash
cargo build --locked --release --features "gui ai" -p bigiris
./install.sh --no-build
```

As ações do Thunar são sempre por usuário: `bigiris install-integrations --user`
mescla as entradas do BigIris em `~/.config/Thunar/uca.xml`, preservando as
ações que você já tinha. Se você ainda não tem esse arquivo, ele começa como
cópia de `/etc/xdg/Thunar/uca.xml` (como o próprio Thunar faz), para não esconder
as ações padrão do sistema. Os pacotes não instalam `/etc/xdg/Thunar/uca.xml`:
esse arquivo pertence ao pacote `thunar`. Quem usa Thunar deve rodar o comando
acima uma vez por usuário (`install.sh` já faz isso).

Com o pacote instalado, `install-integrations --user` não grava cópias pessoais
das integrações que o pacote já fornece em `/usr/share` (Dolphin, Nautilus, Nemo,
PCManFM-Qt, elementary Files, atalho e ícone) e apaga cópias pessoais antigas
delas, que de outro modo sombreariam as do pacote e ficariam desatualizadas.
Na prática, só as ações do Thunar são gravadas.

Para remover as integrações pessoais antes de apagar o binário:

```bash
bigiris uninstall-integrations
```

Para desfazer tudo o que o `install.sh` instalou (integrações, binário, schema e
traduções em `$PREFIX`), use o mesmo `PREFIX` da instalação:

```bash
./install.sh --uninstall
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
bigiris --dialog=upscale foto.jpg

# CLI
bigiris convert --to avif --overwrite increment *.jpg
bigiris convert --to webp --target menor *.png
bigiris convert --to jpg --quality 90 foto.png
bigiris resize --max-edge 1080 *.png
bigiris rotate --degrees 90 foto.jpg
bigiris flip --axis horizontal foto.jpg
bigiris crop --rect 800x600+100+50 foto.jpg
bigiris adjust --brightness 15 --contrast 10 foto.jpg
bigiris animate -o saida.gif --delay 100 frame_*.png
bigiris remove-bg --overwrite increment foto.jpg
bigiris upscale --factor 4 foto.jpg               # Real-ESRGAN, ou Lanczos3
bigiris upscale --factor 2 --engine lanczos *.png  # sem IA
bigiris denoise foto-ruidosa.jpg                  # <nome>_denoised.jpg
bigiris inpaint --mask mascara.png foto.jpg       # branco = apagar
bigiris to-pdf --overwrite replace --timeout 300 documento.docx
bigiris batch --profile docs/batch-profile.example.toml pasta-com-fotos

# Integrações e diagnóstico
bigiris install-integrations --user
bigiris uninstall-integrations
bigiris --self-test
```

Todos os comandos que gravam arquivos aceitam
`--overwrite skip|replace|increment` e, por padrão, **não sobrescrevem**
(`skip`): o arquivo existente é mantido e a saída mostra `skip`. Isso vale
também para `animate` (o `.gif` de destino), `remove-bg` (`<nome>_nobg.png`),
`upscale` (`<nome>_<pct>pct.<ext>`), `denoise` (`<nome>_denoised.<ext>`),
`inpaint` (`<nome>_inpaint.<ext>`) e `to-pdf`, que agora ignora o
`<nome>.pdf` já existente em vez de substituí-lo. `upscale --engine` aceita
`auto` (padrão: Real-ESRGAN se instalado, senão Lanczos3), `realesrgan` (falha
se o pacote faltar) e `lanczos`. Em `inpaint`, a máscara tem o tamanho da
imagem; o branco é preenchido a partir dos arredores e o resto da imagem não
muda. Sem o pacote do modelo, `remove-bg`, `denoise`, `inpaint` e
`upscale --engine realesrgan` param com código 1 e dizem o que instalar.
`to-pdf --timeout <segundos>` (1 a 3600, padrão 120) limita o tempo de cada
documento no LibreOffice; ao estourar, o processo e seus filhos são encerrados
e nenhum PDF parcial fica para trás. Saídas que colidiriam entre si no mesmo
comando são recusadas antes de qualquer gravação.

### Códigos de saída

| Código | Significado |
|---|---|
| `0` | Sucesso, inclusive quando arquivos foram ignorados por já existirem |
| `1` | Uso inválido ou erro fatal antes de processar; nada foi gravado |
| `2` | Falha parcial: um ou mais arquivos de entrada não puderam ser processados |

Os mesmos códigos aparecem em `bigiris --help`.

### Atalhos do visualizador

| Atalho | Ação |
|---|---|
| `←` / `→`, `Home` / `End` | Navegar pela pasta |
| roda do mouse, `+` / `-` | Zoom |
| `0` / `1` | Ajustar / tamanho real |
| arrastar | Pan |
| `R` / `Shift+R` | Girar à direita / esquerda |
| `Ctrl+R` | Recortar na própria imagem (`Enter` confirma, `Esc` cancela) |
| `E` | Abrir / fechar o painel de edição (`Esc` fecha) |
| `\` (segurar) | Ver a imagem original, sem as edições |
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
│   ├── bigimage-ai/            IA local (motores image-engine-native)
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

Rust stable 1.95 ou posterior e `glslc` (shaderc) para a feature `ai`:

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

Os testes com os modelos reais são ignorados por padrão; com os pesos
instalados (ou apontados pelas variáveis `BIGIRIS_*_WEIGHTS`):

```bash
cargo test --release -p bigimage-ai --features native --test models -- \
  --ignored --test-threads=1 --nocapture
```

## Licença

GPL-3.0-or-later. Consulte [LICENSE](LICENSE).

Os motores de IA vêm do
[image-engine-native](https://github.com/biglinux/image-engine-native)
(MIT, de Bruno Gonçalves / BigLinux; o exponencial segue o XNNPACK, BSD-3-Clause).
Os pesos não fazem parte do BigIris: são instalados pelos pacotes
`*-native`, cada um com a licença do modelo original (BiRefNet: MIT;
Real-ESRGAN: BSD-3-Clause; SCUNet e LaMa: Apache-2.0). Detalhes em
[NOTICE](NOTICE).
