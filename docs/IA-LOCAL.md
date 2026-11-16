# IA local no BigIris

> Todo o processamento de IA do BigIris roda **no seu computador**. Nada de
> upload, nada de conta, nada de API externa, e nada de download: os modelos
> vêm de pacotes do sistema.

## Por que isso importa

Soluções populares de remoção de fundo e "melhoria" de fotos via web têm três
problemas que o BigIris resolve por design:

1. **Privacidade.** Você envia a imagem para o servidor do fornecedor. O
   serviço pode registrar, indexar, treinar novos modelos ou sofrer
   vazamento. Para quem trabalha com material sensível (documentos,
   identidade, registros médicos, provas em processos judiciais), isso é
   incompatível com dever de sigilo e com a LGPD.
2. **Dependência e risco operacional.** O serviço pode sumir, mudar de
   preço, limitar requisições ou ficar fora do ar quando você precisa. O
   BigIris funciona offline, sempre com o mesmo resultado.
3. **Licenciamento opaco.** Muitos modelos "open weights" têm licença
   *non-commercial* escondida (`CC-BY-NC`, RAIL-M). Os quatro modelos do
   BigIris têm licenças FOSS: MIT, BSD-3-Clause e Apache-2.0.

## Os motores: image-engine-native

O BigIris usa o [image-engine-native](https://github.com/biglinux/image-engine-native),
do BigLinux: cada modelo escrito à mão em Rust puro, sem ONNX Runtime,
PyTorch ou Python, conferido contra o modelo original em PyTorch. Roda numa
GPU com Vulkan 1.2 (meia precisão) quando há uma, e na CPU (AVX2, AVX ou
SSE2, escolhido na hora) quando não há. A versão usada fica fixada por
commit no `Cargo.toml` do workspace.

| Ferramenta | Modelo | Pacote (pesos) | Licença dos pesos |
|---|---|---|---|
| Remover fundo | BiRefNet-lite | `birefnet-native` | MIT |
| Aumentar resolução | Real-ESRGAN general x4v3 (dn 0,5) | `realesrgan-native` | BSD-3-Clause |
| Remover ruído | SCUNet color, real PSNR | `scunet-native` | Apache-2.0 |
| Apagar objeto | Big LaMa | `lama-native` | Apache-2.0 |

Os pesos ficam em `/usr/share/<pacote>/` e só são lidos de lá. Para testes e
desenvolvimento, uma variável por modelo aponta outro arquivo:
`BIGIRIS_BIREFNET_WEIGHTS`, `BIGIRIS_REALESRGAN_WEIGHTS`,
`BIGIRIS_SCUNET_WEIGHTS` e `BIGIRIS_LAMA_WEIGHTS`. A integridade dos pesos é
responsabilidade do gerenciador de pacotes (assinatura do repositório), como
qualquer outro arquivo do sistema.

Variáveis do próprio motor:

- `IMAGE_ENGINE_NATIVE_GPU=0` mantém tudo na CPU; `=1` transforma a falta de
  GPU num erro que explica o motivo.
- `IMAGE_ENGINE_NATIVE_TIER=N` limita o conjunto de instruções da CPU.

Se a GPU falhar no meio de uma execução, o BigIris repete aquela execução na
CPU e fica na CPU até o fim do processo.

## Como usar

```bash
sudo pacman -S birefnet-native realesrgan-native scunet-native lama-native
```

Cada ferramenta funciona sem as outras. Sem o pacote, ela aparece
indisponível, com o comando de instalação; sem `realesrgan-native`, o aumento
de resolução usa Lanczos3.

### CLI

```bash
bigiris remove-bg foto.jpg                 # foto_nobg.png (PNG com alfa)
bigiris upscale --factor 4 foto.jpg        # foto_400pct.jpg
bigiris upscale --engine lanczos foto.jpg  # sem IA
bigiris denoise foto.jpg                   # foto_denoised.jpg
bigiris inpaint --mask mascara.png foto.jpg  # foto_inpaint.jpg
```

O dispositivo (por exemplo `GPU: Intel(R) Graphics` ou `CPU`) sai no stderr na
primeira imagem. A CLI usa todos os núcleos da CPU.

### Visualizador

No painel de edição (`E`), a seção **Ferramentas** traz **Remover fundo (IA)**,
**Remover ruído (IA)** e **Aumentar resolução** com o fator. Rodam com barra de
progresso e botão de cancelar, e viram passos de desfazer; o resultado fica em
cache, então as edições seguintes não repetem o modelo. A dica de cada botão
mostra onde o modelo roda depois da primeira carga. A interface deixa um
núcleo livre para a área de trabalho.

O aumento de resolução registra o motor usado (Real-ESRGAN ou Lanczos3) junto
com a edição, então **Salvar** e **Salvar como…** recalculam a mesma coisa a
partir dos pixels do arquivo (preservando metadados e perfil de cor).

### Prisma

`bigiris --dialog=remove-bg` e `bigiris --dialog=upscale` processam vários
arquivos; o modelo carrega uma vez para o lote todo.

## Como cada ferramenta trata a imagem

Todas as redes trabalham em sRGB de 8 bits. Imagens de 16 bits são reduzidas a
8 bits; imagens de ponto flutuante (EXR, HDR) passam pela curva sRGB. A saída
dessas ferramentas é de 8 bits (RGB, ou RGBA se a origem tinha alfa).

- **Remover fundo:** a imagem é reduzida a 1024 × 1024 (a resolução em que o
  BiRefNet foi treinado), a máscara volta ao tamanho original e é multiplicada
  no alfa que já existir. Saída sempre RGBA.
- **Aumentar resolução:** o Real-ESRGAN sempre amplia 4×, em blocos; para 2× e
  3× o resultado é reduzido com Lanczos3. O alfa é ampliado com Lanczos3.
- **Remover ruído:** SCUNet em blocos de até 896 px com margem de 64; o alfa
  não muda. Custa 2,3 TFLOP por megapixel: uma foto de 12 MP leva cerca de um
  minuto e meio numa CPU de notebook (bem menos na GPU).
- **Apagar objeto:** o LaMa enxerga a imagem inteira de uma vez, então o
  BigIris corta uma caixa em volta da máscara (com contexto), reduz a caixa a
  no máximo 1024 px no lado maior, preenche e cola de volta só dentro da
  máscara, com a borda suavizada. O que fica fora da máscara (ampliada em
  alguns pixels) não muda. API para o pincel futuro:
  `bigimage_ai::inpaint::inpaint(imagem, máscara, progresso)`.

## Memória

Medido neste projeto, pico do processo inteiro numa CPU Core Ultra 5 225H:

| Modelo | Entrada | CPU | GPU (Intel Arc, Vulkan) |
|---|---|---|---|
| BiRefNet-lite | 800 × 600 | 0,34 GB | 0,13 GB |
| LaMa | caixa de 256² / 1024 × 773 | 0,34 / 0,59 GB | 0,15 GB |
| Real-ESRGAN | 384 × 256 → 4× | 0,59 GB* | 0,15 GB |
| SCUNet | 768 × 512 | 0,65 GB* | 0,15 GB |

\* acumulado na mesma execução de testes, depois dos modelos anteriores.

Antes de carregar um modelo e antes de cada execução, o BigIris estima a
memória necessária (pesos, rede e buffers da imagem, com margem) e confere com
o que está livre agora: `MemAvailable`, limites cgroup v2 e o limite de espaço
virtual. Swap não conta. Se não couber, a ferramenta recusa com explicação, sem
alocar nada; o visualizador e as conversões continuam disponíveis. É uma
proteção preventiva, não uma quota rígida.

## Testes

```bash
# testes unitários (sem pesos)
cargo test -p bigimage-ai --features native

# modelos reais, ignorados por padrão
BIGIRIS_BIREFNET_WEIGHTS=… BIGIRIS_REALESRGAN_WEIGHTS=… \
BIGIRIS_SCUNET_WEIGHTS=… BIGIRIS_LAMA_WEIGHTS=… \
  cargo test --release -p bigimage-ai --features native --test models -- \
  --ignored --test-threads=1 --nocapture
```

O CI baixa os quatro pacotes do repositório do BigLinux (fixados por nome e
SHA-256), extrai os pesos e roda esses testes na CPU.
