# Qualidade do BigIris para BigCommunity GNOME

A campanha local de 11 de outubro de 2026 produziu uma candidata com correções,
testes reproduzíveis e pacote para homologação. Os resultados abaixo permitem
avaliar o checkout; a adoção como aplicativo oficial depende dos critérios
pendentes. Não há certificação de ausência universal de bugs ou vazamentos.

## Ambiente e rastreabilidade

BigCommunity GNOME/Wayland, x86_64, Ryzen 7 5800H, 16 CPUs lógicas,
Rust 1.98.1, GTK 4.22.4, libadwaita 1.9.3, dav1d 1.5.4 e ONNX Runtime 1.29.0.
MSRV do projeto: 1.92. Runtime GTK mínimo: 4.12; libadwaita: 1.5.

Somente imagens sintéticas, fixtures de regressão e diretórios de teste foram
usados. Alterações preexistentes foram preservadas. Evidências locais ficam
em `target/quality/` e não integram o pacote distribuído:

- `zoom-transition/gates/summary.json`: verificações da correção da transição de zoom.
- `zoom-transition/painted-frames.json`: quadros pintados e escalas intermediárias.
- `round3/package-fixed-gates/summary.json`: verificações finais, com logs por gate.
- `round2/acceptance/{benchmark,stress,soak}.log`: performance e estabilidade.
- `fuzz/summary.json`, `decode.log` e `icc.log`: última campanha instrumentada.
- `round2/fuzz-before-gif*`, `fuzz-before-avif*` e `fuzz-artifacts/`: descobertas anteriores.
- `round3/release-final/release-smoke.json`: binário release, SHA-256 e 40 verificações.
- `round3/ai-release/summary.json`: inferência ONNX real antes da correção do empacotamento Nautilus.
- `round3/ai-final/summary.json`: repetição no binário final; conferir o resultado e o orçamento disponível.
- `round3/ai-preflight-final/summary.json`: recusa sob 4 GiB e runtime opcional ausente, conferidos no release.
- `round2/nautilus-real.json`: menu construído com GI/Nautilus instalado.
- `long-soak/status.json`: estado explícito do ensaio prolongado.
- `candidate/`: pacote candidato, hashes e manifesto.
- `screenshots-final/` e `screenshots-light-final/`: inspeção das janelas sintéticas.
- `valgrind.log`: impedimento local do Memcheck.

A rodada 3 adiciona proteção de RAM para IA e zoom ancorado. Performance,
fuzzing e soak da rodada 2 continuam referidos ao core, que não mudou nessa
rodada. Os gates iniciais da rodada 3 registram falhas de Clippy e sincronização
do zoom; foram corrigidas antes dos gates finais.

Relatórios anteriores que falharam continuam registrados como falha. A execução
`round2/acceptance/summary.json` teve uma falha de Clippy headless, corrigida
antes dos gates finais; seus três ensaios de performance passaram. Resultados
históricos não foram reescritos para aparentar aprovação.

## Cobertura executada

| Área | Resultado | Alcance |
| --- | --- | --- |
| Rust, todos os recursos/targets | 216 testes passaram | Workspace, GUI compilada, core, IA e integrações |
| Rust, headless | 178 testes passaram | Sobrepõe parte dos 216; não somar as contagens |
| GTK, teste gráfico separado | Passou | 30 aberturas/fechamentos do viewer, 13 janelas de ferramentas/propriedades e seleção sintética de 10 mil arquivos; estado liberado e no máximo 41 miniaturas simultâneas; seis cenários de zoom conferidos quadro a quadro, escalas intermediárias e pixels renderizados |
| Nautilus | 6 contratos passaram; GI real criou menus | FileInfo simulado; clique em Nautilus e política da distribuição exigem homologação |
| Rustfmt e Clippy | Passaram, warnings como erros | Headless e todos os recursos |
| Distribuição | Desktop, AppStream offline, schemas, inglês e sintaxe passaram | Sem instalar ou alterar associações padrão no host |
| Fidelidade | Testes passaram | EXIF, ICC RGB→sRGB, alpha, PNG 16 bits, todos os 13 formatos; RGB não linear/float dentro do comportamento anunciado |
| Preservação de conteúdo | Testes passaram | Transformações estáticas recusam GIF/APNG/WebP animados e TIFF multipágina |
| Gravação e concorrência | Testes passaram | Publicação atômica, preservação do destino em falhas, Skip/Increment concorrentes |
| CLI e release | 40 verificações no release | Self-test de isolamento, conversão, resize e reabertura dos 13 formatos |
| Fuzzing ASan/LSan | Última campanha passou | 17.304 execuções decode e 33.331 ICC, 61 s por alvo; sem erro reportado nessa amostra |
| Stress | 64 conversões, 4 trabalhadores, todas reabertas | Passou; 0,593 s medidos pelo probe, pico de 51.008 KiB |
| Estabilidade curta | 300,079 s, 3.029 ciclos | RSS estável, descritores e threads estáveis |
| IA real | Passou antes da correção do empacotamento; 21,172 s para duas imagens | RGB/alpha preservados, sessão reutilizada; pico de 6.847.848 KiB (~6,53 GiB). Repetição no binário final recusada por RAM disponível abaixo de 8 GiB |
| RustSec | Gate final passou | Zero vulnerabilidades e zero avisos unsound na base consultada; um aviso de manutenção |
| Memcheck | Sem aprovação local | Valgrind não inicia por falta de símbolos compatíveis do loader glibc; job preparado no CI, sem execução remota nesta campanha |
| Oito horas | Consultar `long-soak/status.json` | `running` significa pendente, nunca aprovação |

Os testes GTK executam loaders locais no binário de teste. O executável final
usa o worker confinado, exercitado pelos testes da CLI/release. Nenhum desses
ensaios substitui uma sessão prolongada do produto completo com GPU e Orca.

O fuzzing é guiado por cobertura LLVM, instrumenta o código Rust do core e usa
corpus sintético dos formatos e perfis ICC. A entrada mutada é limitada a 16 KiB;
o decode completo, a um milhão de pixels. Cabeçalhos de imagens maiores continuam
inspecionados. As bibliotecas nativas externas não são instrumentadas. Limites
maiores têm regressões e contenção pelo sandbox; o resultado não cobre todo
arquivo possível ou todos os encoders, GTK, ONNX e argumentos CLI.

Durante a campanha, GIF com frame maior que a tela lógica e AVIF com caixa além
do arquivo provocaram timeout/alocação excessiva. Foram acrescentadas inspeções
estruturais sem buffers proporcionais aos tamanhos declarados e regressões
reproduzíveis. Outro timeout vinha de um GIF válido com tela de 33 MP; o orçamento
de pixels do alvo instrumentado foi explicitado, sem reduzir a cobertura de
cabeçalhos ou fingir que esse caso era uma falha de memória corrigida.

## Performance medida

Imagem sintética de 1920 × 1080, release, cache aquecido, três amostras por
operação. Amostragem exploratória na sessão de desenvolvimento; não é uma
comparação controlada com outro visualizador nem mede abertura fria da GUI.

| Operação | Mediana em ms | P95 em ms |
| --- | ---: | ---: |
| Decode PNG | 11,206 | 11,230 |
| Resize para aresta 1080 | 15,030 | 16,653 |
| Preview até 800 | 15,673 | 16,840 |
| PNG | 23,588 | 23,626 |
| JPEG | 66,197 | 66,403 |
| WebP lossless | 33,396 | 34,892 |
| AVIF | 7.388,941 | 7.559,215 |

Resize/preview incluem decode; conversão inclui decode, preparação, encode,
gravação e sincronização. Pico do benchmark: 79.660 KiB. AVIF continua o gargalo;
limitar suas threads a duas contém a multiplicação de trabalhadores de lote,
mas esses números não demonstram uma melhoria percentual atribuível à mudança.

No soak de cinco minutos, RSS após aquecimento: 13.136 KiB, antes e depois;
pico: 26.056 KiB; quatro descritores e uma thread antes e depois. O probe usa
allocator padrão; o executável usa mimalloc. As medições não abrangem GPU,
memória de ONNX ou um acervo grande. O binário do ensaio prolongado tem hash
registrado; um resultado só vale para aquela compilação e aquele roteiro.

## Correções e arquitetura

O executável delega decode, dimensões, metadados e previews a um worker
Bubblewrap sem rede/home/run/etc, com entrada por descritor somente leitura,
`/usr` e executável somente leitura e tmpfs de 64 MiB. `prlimit` limita espaço
virtual a 2 GiB, CPU a 30 s, descritores a 64, core dumps a zero e cada arquivo
de saída a 512 MiB mais cabeçalho. O supervisor encerra o filho após 60 s ou
cancelamento. O protocolo de pixels é validado antes da alocação no pai.
Se o isolamento falhar, a imagem é recusada. Não há fallback sem sandbox.

Isso não é quota de RSS/cgroup nem filtro seccomp. Encoders, GTK, TLS e ONNX
continuam no pai. Consumidores autônomos da biblioteca core executam localmente
por padrão. O worker restringe entrada e saída; scratch interno dos codecs
não tem contabilização precisa por buffer. `/usr` inteiro é visível somente
leitura, em vez de uma allowlist mínima de bibliotecas.

O pipeline limita entrada regular a 1 GiB, até 256 × 1024² pixels e 512 MiB por
buffer. Lotes usam estimativa de CPU/RAM e até quatro trabalhadores. Escrita
usa temporário único, flush/sync e publicação atômica. PNG preserva 16 bits;
ICO normaliza RGBA e limita dimensões. ICC RGB é convertido a sRGB preservando
alpha/precisão; perfis inválidos ou não RGB são recusados. PNG/JPEG recebem
perfil sRGB gerado sem campos identificadores da origem. Metadados pessoais
não são copiados na conversão.

Descoberta de pastas, properties, carregamento dos diálogos, comparação e
preparo dos pixels de impressão saíram da thread GTK. Previews têm um trabalhador
coalescente, buffers limitados e apenas a revisão atual é apresentada. A fita
é paginada em até 41 widgets. Referências fracas e desconexão de sinais reduzem
retenções; testes verificam liberação. A impressão usa alpha premultiplicado,
com regressão específica; não foi enviado trabalho a uma impressora real.

A IA exige HTTPS/TLS e SHA-256, URL de revisão imutável, publicação atômica do
modelo, shape validado e sigmoid dos logits, preservando alpha original.
Uma sessão é reutilizada por lote e usa até quatro threads de inferência.
Não há interrupção segura no meio de uma inferência. O release da rodada 3 atingiu 6.847.848 KiB (~6,53 GiB) nas duas imagens
sintéticas, em 21,172 s. A rodada 2 mediu 6,12 GiB e 33,195 s; a diferença
entre sessões não demonstra redução de tempo atribuível às mudanças.
A feature é opcional. A candidata exige 8 GiB de RAM livre antes de baixar/carregar
esse modelo; reavalia o orçamento antes de cada inferência e acrescenta a área
de trabalho da imagem. Considera MemAvailable, todos os limites finitos dos
ancestrais cgroup v2 e RLIMIT_AS, sem somar swap. Sessões reutilizadas creditam
crescimento residente observado nas chamadas nativas, com teto conservador.
Isso é admissão preventiva, não uma quota rígida de RSS: a disponibilidade pode
mudar após a conferência e cgroup v1 não é abrangido.

Quatro GiB são recusados antes do download. O teste da CLI usa RLIMIT_AS de
4 GiB, verifica mensagem, ausência de cache/saída e preservação do original;
três testes puros cobrem capacidade, pressão, crédito e parsing do orçamento.
O release anterior à correção do empacotamento também recusou o orçamento de
4 GiB e tratou a ausência da biblioteca opcional ONNX como erro concluído,
sem saída nem alteração do original. No binário final, a recusa sob 4 GiB passou;
o teste de biblioteca ausente não alcançou a carga porque o host tinha menos de
3,3 GiB livres. A inferência foi recusada pela mesma política. Os resultados
não foram convertidos artificialmente em aprovação de inferência; repetição
no pacote final com RAM suficiente permanece pendente.
Isso não equivale a homologação em máquina física de 4 GB. Visualização e
conversão continuam disponíveis, sujeitas ao tamanho e aos limites das imagens.
O teste experimental sem arena do ORT sob limite de espaço virtual de 3 GiB
falhou na inferência; essa configuração não foi adotada no produto.

O zoom foi extraído para `gui/zoom.rs`. Roda, atalhos e botões usam o ponto do
cursor; sem posição válida, usam o centro. A regressão original conferia apenas
a posição após o término. Um novo teste em `after-paint` reproduziu dois quadros
com deslocamento transitório de aproximadamente 185 pixels da imagem no eixo
horizontal. A posição e os limites de rolagem agora são configurados antes da
nova escala; a transição usa o relógio de quadros do GTK e interpolação gradual
em escala logarítmica. Eventos fracionários do touchpad preservam sua magnitude.
A preferência `gtk-enable-animations=false` aplica o zoom imediatamente.

Comandos rápidos atualizam o destino da mesma animação, preservando a âncora
quando o cursor permanece no lugar. Arrastar, ajustar à janela, navegar e fechar
cancelam a animação. O zoom referencia a textura existente, sem redimensionar
pixels na thread gráfica. `1` corresponde a 100% e `0` restaura o ajuste à janela.
Quando a imagem cabe na janela ou alcança uma borda, a rolagem é limitada.

O teste gráfico confere cada quadro pintado contra a projeção geométrica com
esses limites, com tolerância inferior a 1,5 pixel da tela por eixo, monotonicidade
da escala, presença de quadros intermediários e pixels renderizados no destino.
Inclui teclado, botões, ampliações rápidas, inversão, 100%, animações desativadas,
arrasto manual, navegação e fechamento. Essa verificação não certifica conforto
visual pessoal nem ausência de frames perdidos em todo hardware; a avaliação
com mouse e touchpad reais permanece no roteiro de homologação.

O SVG do ícone também recebeu correção de cabeçalho: o comentário extenso foi
movido para dentro do elemento raiz. O desenho permaneceu igual, e o arquivo
passou a ser reconhecido pelo carregador GdkPixbuf usado no GNOME.

A validação do pacote também revelou que a extensão Nautilus já instalada no
host fazia o gerador omiti-la do DESTDIR. O escopo agora é explícito, sem inferir
instalação pessoal por prefixo do caminho. Regressões verificam inclusão no
pacote apesar da extensão existente, preservação do arquivo do host e prevenção
de duplicação na instalação pessoal. O arquivo incompleto anterior foi mantido
em `round3/package-before-nautilus-fix/`, sem aprovação de distribuição.

## Pendências para adoção oficial

| Prioridade | Pendência | Aceitação |
| --- | --- | --- |
| Alta | Instalação limpa BigCommunity | Build da receita completa; instalar, atualizar, remover; MIME, schemas, idiomas e Nautilus reais |
| Alta | Acessibilidade e uso | Orca, teclado, foco, alto contraste, claro/escuro, escalas 100/150/200% e múltiplos monitores |
| Alta | Estabilidade prolongada | Resultado de oito horas do core mais sessão GNOME/slideshow com memória CPU/GPU monitorada |
| Alta | Memória nativa | Memcheck em ambiente compatível; CI remoto verde e revisão dos logs |
| Alta | Acervo e fidelidade | Comparação visual/colorimétrica de imagens reais autorizadas, ICC e 16 bits; definir política para HDR, animação e páginas |
| Alta | IA em hardware modesto | Medir RAM, tempo, qualidade da máscara e cancelamento; homologar a recusa do modelo atual e medir alternativas menores |
| Média | Performance do produto | Primeiro frame frio, navegação e latência comparados ao visualizador nativo no mesmo hardware/acervo |
| Média | Dívida arquitetural restante | Extrair estado/controllers/diálogos do `gui.rs`; ampliar testes dos fluxos interativos |
| Média | Dependência sem manutenção | Retirar `paste` transitivo via rav1e/ravif, RUSTSEC-2024-0436, com regressão AVIF |
| Média | Confinamento adicional | Avaliar encoders/ONNX fora do pai, seccomp/cgroup e allowlist de bibliotecas |

O viewer mostra primeiro quadro/página; não há reprodução de animações,
navegação TIFF multipágina ou gestão HDR/tone mapping completa. A estabilidade
medida e a liberação de referências não provam ausência de leaks nativos.
Os logs GTK registraram aviso de tema e indisponibilidade do barramento AT-SPI;
Orca permanece pendente. O pacote é recomposto do release testado, não um build
limpo certificado. CI remoto não foi acionado/publicado nesta campanha; seus
gates não desabilitam AppArmor nem ampliam privilégios do contêiner Arch.

## Referências e reprodução

[GNOME HIG](https://developer.gnome.org/hig/index.html),
[acessibilidade](https://developer.gnome.org/hig/guidelines/accessibility.html),
[image::Limits](https://docs.rs/image/latest/image/struct.Limits.html),
[Bubblewrap](https://github.com/containers/bubblewrap),
[RustSec](https://github.com/RustSec/advisory-db),
[Valgrind](https://valgrind.org/docs/manual/manual-core.html) e
[restrições de namespaces do Ubuntu](https://documentation.ubuntu.com/release-notes/24.04/#unprivileged-user-namespace-restrictions)
orientam os critérios. Em runners Ubuntu, conferir o perfil de Bubblewrap da
distribuição; sandbox indisponível deve falhar, sem desativar proteções globais.

Comandos em [CONTRIBUTING.md](../CONTRIBUTING.md#campanha-de-qualidade-no-gnome).
Entrega e roteiro de homologação em [RELEASE-CANDIDATE.md](RELEASE-CANDIDATE.md).
