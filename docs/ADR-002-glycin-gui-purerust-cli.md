# ADR-002 — backend de decodificação

- **Status:** Implementado com worker Bubblewrap; expansão de formatos adiada
- **Data:** 2026-04-19
- **Revisão:** 2026-10-11

## Contexto

O visualizador e a CLI precisam entregar os mesmos pixels, orientação EXIF e
limites contra imagens hostis. Usar backends diferentes antes de existir uma
camada comum aumentaria a chance de divergência e anunciaria formatos que o
produto ainda não abre.

## Decisão atual

GUI e CLI usam `bigimage_core::open_image`, baseado no crate `image`, com
detecção por conteúdo, orientação EXIF e limites de 1 GiB/256 MP. No viewer, a
decodificação roda em uma thread dedicada e pedidos sucessivos são coalescidos,
evitando bloquear o loop do GTK.

No executável, o core recebe um backend que delega decode, dimensões, previews
e metadados a um worker Bubblewrap separado. Esse worker usa a mesma
implementação `image`, lê a entrada por descritor e entrega pixels por um
protocolo validado, com limites de processo e cancelamento supervisionado.
O aplicativo recusa a imagem se o sandbox não iniciar. A biblioteca standalone
continua local por padrão; encoders e inferência ONNX não foram isolados.
Detalhes e limites em [QUALITY.md](QUALITY.md).

Os formatos efetivamente suportados são apenas os expostos por
`bigimage_core::Format`. HEIC, JPEG XL, SVG e RAW não têm feature flags vazias
nem aparecem nos metadados do desktop.

## Evolução considerada

Glycin continua sendo uma opção para substituir ou ampliar os loaders atuais,
desde que:

1. exista adaptação única para o tipo interno do core;
2. os mesmos testes de orientação, dimensão e limites cubram os dois backends;
3. cada formato só seja anunciado depois de passar por testes reais;
4. a dependência esteja disponível nas distribuições suportadas.

Até esses critérios serem atendidos, o backend atual é a implementação oficial,
não um fallback temporário.
