# Candidata local do BigIris para BigCommunity GNOME

Esta candidata reúne as correções e os testes descritos em
[QUALITY.md](QUALITY.md). A adoção como aplicativo oficial depende da
homologação da distribuição; o pacote local permite começar essa avaliação.

## Entrega e reprodução

- Binário completo: `target/release/bigiris`, com GTK/libadwaita e IA.
- Pacote local: `target/quality/candidate/bigiris-0.1.0.local-2-x86_64.pkg.tar.zst`, acompanhado de SHA-256.
- Gates: `target/quality/round3/package-fixed-gates/summary.json`.
- Testes do release: `target/quality/round3/release-final/release-smoke.json`.
- Fuzzing instrumentado: `target/quality/fuzz/summary.json`.
- Estabilidade prolongada: `target/quality/long-soak/status.json`.

O pacote é recomposto com a receita `PKGBUILD.local` a partir do release
compilado e dos dados atuais, depois dos testes do checkout. Isso valida o
conteúdo e a etapa `package()`; não equivale a um build reproduzível em uma
instalação limpa. Os resultados executados prevalecem sobre comandos apenas
preparados. `running` no ensaio prolongado significa pendente.

Para experimentar o pacote em uma instalação de homologação BigCommunity,
instale o arquivo `.pkg.tar.zst` com `sudo pacman -U caminho/do/pacote`.
O runtime exige `bubblewrap` e `util-linux`, namespaces disponíveis e as
bibliotecas declaradas no pacote. `bigiris --self-test` decodifica uma imagem
sintética pelo sandbox e verifica ausência dos diretórios privados e da
rede do host. Se falhar, a candidata não deve ser aprovada nessa máquina.

## Roteiro para Leonardo e Tales

1. Em instalação limpa, instalar, atualizar e remover o pacote. Conferir
   desktop entry, ícone, schemas, tradução, Nautilus e resíduos da remoção.
2. Abrir pelo Nautilus uma imagem, seleção e pasta; testar navegação,
   zoom sob o cursor com roda, botões, `+`/`-`, `1` (100%) e `0` (ajustar);
   conversão dos formatos anunciados, cancelamento, erros e arquivos em
   sincronização. Confirmar ausência de alteração dos originais em falhas.
3. Exercitar teclado e Orca, alto contraste, temas claro/escuro, escalas
   100%, 150% e 200% e múltiplos monitores. O teste automatizado de janelas
   não substitui essa avaliação de acessibilidade e uso.
4. Comparar tempo até o primeiro frame, navegação, RAM/GPU e interação
   com o visualizador atual, usando o mesmo acervo autorizado e o mesmo
   hardware. Incluir máquina de pouca RAM e SSD lento.
5. Conferir orientação, cor, transparência e precisão com imagens reais.
   ICC RGB é normalizado para sRGB; perfis não RGB são recusados. O viewer
   mostra o primeiro quadro/página; edição estática recusa animações e
   TIFF multipágina. HDR não dispõe de gestão completa ou tone mapping.
6. Avaliar remoção de fundo: download íntegro, inferência local, cancelamento
   entre arquivos e reutilização do modelo. O ensaio inicial atingiu cerca
   de 6 GiB; o ensaio da rodada 3, antes da correção de empacotamento, processou duas
   imagens em 21,172 s, com pico de
   aproximadamente 6,53 GiB. A candidata exige 8 GiB de RAM livre para iniciar esse modelo e recusa
   um orçamento de 4 GiB antes do download. Visualização e conversões continuam
   disponíveis. Homologar em hardware físico de pouca RAM; o limite é uma
   conferência preventiva, sem quota rígida de RSS.
7. Exigir resultado final do ensaio prolongado e do Memcheck em ambiente
   compatível. O soak iniciado exercita o core; adicionar sessão GNOME de
   oito horas com slideshow e monitoramento de GPU antes da adoção oficial.

## Limites desta candidata

ASan/LSan, testes de ciclo de vida e estabilidade curta não provam ausência
universal de vazamentos. O isolamento cobre decodificação e inspeção de
metadados do executável; inferência ONNX e encoders continuam no processo
principal. O worker tem limites de espaço virtual, CPU, prazo, arquivos,
descritores e scratch, sem quota cgroup de RSS nem filtro seccomp.

A repetição da inferência no binário final foi recusada pelo orçamento de RAM
do host, então permanece pendente com pelo menos 8 GiB disponíveis. As duas
inferências reais aprovadas precedem a correção de empacotamento Nautilus.
Os testes de recusa sob 4 GiB passaram também no binário final.

O CI contém gates preparados; sua execução remota e a compatibilidade do
sandbox no contêiner Arch continuam a conferir. Não foram concedidos
privilégios amplos ao contêiner para contornar essa restrição. A publicação
no repositório da distribuição e a associação MIME padrão exigem decisão
de manutenção após a homologação.

## Administração pelo Git

Abra a pasta raiz do BigIris que contém `.git`, `Cargo.toml`, `crates/` e
`pkgbuild/`. O checkout atual está na branch `dev-xathay`. Abrir essa pasta
em um cliente Git permite revisar diffs e arquivos novos; não publica nem
atualiza automaticamente o repositório da distribuição. Confira os arquivos
incluídos no commit, inclusive novos módulos e testes. Os artefatos de
`target/quality/` são evidência local e não devem entrar no histórico.

Envie a candidata primeiro para a branch de desenvolvimento e obtenha CI
remoto verde. A integração no repositório oficial deve seguir o roteiro de
homologação acima e as decisões de manutenção de Leonardo e Tales.
