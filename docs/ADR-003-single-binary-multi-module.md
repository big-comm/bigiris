# ADR-003 — Binário único com múltiplos módulos (Íris + Prisma)

- **Status:** Aceito
- **Data:** 2026-04-19
- **Supersede:** variante anterior considerada com dois binários (`bigiris` + `bigprisma`)

## Contexto

O projeto precisa de:

1. Um visualizador de imagens moderno estilo Loupe (R7, R8, R9).
2. Um sistema completo de conversão/edição em lote, acionado por clique direito (R4, R5).
3. Preview ao vivo dos efeitos aplicados no módulo de conversão (R7: "visualizar as imagens e os efeitos ou recortes, rotações e etc sendo aplicados ao vivo na interface").
4. IA para remover fundo e upscale (R10).

Duas arquiteturas foram consideradas:

- **A — Dois binários:** `bigiris` (viewer) e `bigprisma` (conversor), seguindo o princípio GNOME HIG "um app, um propósito".
- **B — Um binário:** `bigiris` contendo módulos internos Íris (viewer) e Prisma (conversor), invocáveis por subcomandos, flags `--dialog=X` ou direto pela GUI.

## Decisão

**Arquitetura B.** Um único binário `bigiris`, um PKGBUILD `bigiris`, um `.desktop` principal. O módulo de conversão existe como submenu do viewer e como diálogos modais invocados por file-manager service menus.

## Justificativa

1. **Componentes compartilhados.** Viewer e diálogos usam o mesmo core de decode, transformação, preview e encode, sem duplicar regras de segurança e nomes de saída.
2. **Cache de modelo dentro de cada lote.** Uma sessão BiRefNet é reutilizada pelos arquivos processados no mesmo fluxo de IA; diálogos independentes continuam isolados em processos próprios.
3. **Velocidade de desenvolvimento.** Um codebase, um CI, um PKGBUILD, uma submissão BigCommunity/BigLinux/Flathub.
4. **UX sem fragmentação.** Usuário instala `bigiris` e tem tudo. Service menus disparam `bigiris --dialog=X` — mesma identidade, mesmo ícone, mesmo histórico, mesmas preferências.
5. **Precedente sólido.** LibreOffice (soffice) faz exatamente isso: um binário, múltiplos `.desktop`, várias identidades de app percebida.

## Trade-offs aceitos

- **Inchaço**: binário carrega código de todos os módulos mesmo quando usuário só quer converter. Mitigado por lazy-load: widgets IA só instanciados quando o diálogo IA é aberto; feature `ai` opcional no Cargo.toml permite distro enxuta.
- **Raio de explosão de crash**: um panic derruba o processo corrente. Os diálogos avançados são lançados como processos independentes, limitando o impacto sobre o viewer já aberto.
- **Imagem de marca**: "Prisma" perde visibilidade como produto separado. Mitigado mantendo "Prisma" como nome do módulo de conversão nos títulos de diálogo e em material de marketing — a narrativa Íris/Prisma sobrevive sem custo arquitetural.

## Rota de reversão

Se no futuro houver motivo forte para separar (ex.: Flatpak de viewer sem IA e Flatpak de converter com IA), a separação é factível porque:

- `bigimage-core`, `bigimage-ai`, `bigimage-integrations` são crates brand-agnósticas.
- Os módulos viewer e diálogos vivem em sub-módulos Rust do crate `bigiris` que podem ser promovidos a crates de biblioteca separados.
- A API do core já é estável entre os dois.

## Alternativas descartadas

- **A — Dois binários sob família BigPictus.** Aumento significativo de custo de manutenção sem benefício proporcional. Perde-se o preview ao vivo natural.
- **Um binário sem modo CLI.** Violaria R3 (CLI espelho da GUI).
- **Três binários (viewer, converter, CLI headless).** Complexidade explosiva, sem ganho concreto.
