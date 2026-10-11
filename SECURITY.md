# Política de Segurança

## Versões com suporte

Apenas a `main` e a tag mais recente recebem correções. Ainda não há
linha LTS — o projeto está pré-1.0 e a velocidade de lançamento é
semanal.

## Reportando uma vulnerabilidade

**Não abra issue pública nem PR para questões de segurança.** Use um
dos canais privados:

1. **GitHub Security Advisories** — em
   [github.com/xathay/bigiris/security/advisories/new](https://github.com/xathay/bigiris/security/advisories/new).
   Preferido: criptografa em trânsito e fica invisível até o disclosure.
2. **E-mail** — `leoathayde@gmail.com` com o assunto começando com
   `[BigIris-Security]`. Se quiser PGP, peça a chave pública por esse
   mesmo canal.

Inclua, na medida do possível:

- Versão do BigIris afetada (`bigiris --version`).
- Distro + ambiente gráfico (OpenMandriva/BigLinux/Arch/etc., KDE/GNOME/...).
- Reprodução mínima — se envolver arquivo malicioso, descreva sem anexar
  o exploit nesta mensagem; combine um canal seguro depois.
- Impacto observado (ex.: leitura de arquivos fora do escopo, execução
  de código, escalonamento de privilégio).

## Tempo de resposta

| Severidade                                    | Resposta inicial |
|-----------------------------------------------|------------------|
| Crítica (RCE, escalonamento de privilégio)    | até 48 h         |
| Alta (DoS local, leitura de arquivo arbitrária) | até 7 dias     |
| Média / Baixa                                 | até 30 dias      |

## Escopo

São considerados em escopo:

- Decode de arquivos de imagem (`bigimage-core`).
- Pipeline de IA local (`bigimage-ai`) — incluindo verificação de
  integridade dos modelos.
- Geradores de service menus de file manager (`bigimage-integrations`).
- Visualizador GTK4 (`bigiris/gui`).
- PKGBUILDs do projeto (`pkgbuild/`).

Fora de escopo (encaminhe upstream): vulnerabilidades em `image`,
`fast_image_resize`, `dav1d`, `gtk4`, `libadwaita`, `ort`,
ONNX Runtime ou no kernel.

## Hardening já aplicado

Para contexto sobre o que já é defesa em profundidade:

- `#![forbid(unsafe_code)]` em todos os crates.
- Entradas regulares limitadas a 1 GiB, 256 MP e 512 MiB por buffer
  decodificado, tanto no viewer como nas conversões. Os limites de scratch
  dos codecs são best-effort. O executável confina a decodificação em processos
  Bubblewrap sem rede e sem acesso a home/run/etc: `/usr` e o executável são
  somente leitura, e apenas o descritor da imagem fornecida é montado.
  `/dev` é remontado somente leitura e o tmpfs de `/tmp` é limitado a 64 MiB.
  `prlimit` impõe 2 GiB de espaço virtual, 30 s de CPU, 60 s de prazo na
  supervisão, 64 descritores e limite por arquivo de saída de 512 MiB + cabeçalho.
  Não se trata de quota total de RSS/cgroup nem de filtro seccomp.
- Service menus escritos com `O_NOFOLLOW` para impedir
  symlink-follow durante `sudo install-integrations --system`.
- Modelos IA pinados por SHA-256 + allowlist FOSS de licenças
  (MIT/Apache-2.0/BSD/MPL-2.0/CC0); download abortado se exceder o
  tamanho esperado por mais de 16 MiB.
- Comandos do Thunar UCA empacotados em `sh -c '… "$@"' …` para que
  filenames não sejam reinterpretados pelo shell.
- TLS configurado explicitamente via `native-tls` (OpenSSL do sistema), sem `danger_*` desativando
  validação de certificado.

### Limitações a considerar antes de distribuição oficial

Decodificação, inspeção de formatos e EXIF do executável passam pelo worker
confinado. Consumidores autônomos de `bigimage-core` usam execução local por
padrão. Encoders, GTK, TLS e inferência ONNX continuam no processo do aplicativo.
O isolamento não substitui revisão das bibliotecas nativas ou do kernel. O relatório de qualidade registra os testes realizados e
as lacunas; uma execução curta sem crescimento de RSS não prova ausência de leaks.

Conversões estáticas removem EXIF/XMP pessoal; perfis ICC RGB são convertidos
para sRGB, e perfis inválidos ou não RGB são recusados explicitamente.
Animações e TIFF multipágina são recusados na edição estática. Não há suporte
de edição que preserve seus quadros ou páginas, nem gestão completa de HDR. Gravações novas usam arquivos privados; substituições
preservam as permissões do destino regular quando disponíveis. A publicação
atômica protege contra falha do encoder, mas não promete recuperação após queda
de energia em todos os sistemas de arquivos.

### Admissão de memória para IA

BiRefNet FP32 exige orçamento inicial de 8 GiB de RAM livre. A conferência
ocorre antes do download/carga e antes da inferência; considera MemAvailable,
limites dos ancestrais de cgroup v2 e RLIMIT_AS. Não conta swap. Sessões
reutilizadas creditam somente crescimento residente observado no código nativo,
com teto; a área de trabalho da imagem é acrescentada ao orçamento. A política
não é quota de RSS, não cobre cgroup v1 e não elimina competição por memória
após a conferência. ONNX permanece no processo principal. Um modelo menor
precisa de medições próprias antes de ser oferecido para máquinas de 4 GB.
