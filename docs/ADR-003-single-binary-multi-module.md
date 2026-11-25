# ADR-003: Single binary with multiple modules (Íris + Prisma)

- **Status:** Accepted
- **Date:** 2026-04-19
- **Supersedes:** an earlier variant considered with two binaries (`bigiris` + `bigprisma`)

## Context

The project needs:

1. A modern image viewer in the style of Loupe (R7, R8, R9).
2. A complete batch conversion and editing system, launched from the right-click menu (R4, R5).
3. A live preview of the effects applied in the conversion module (R7: "see the images and the effects, crops, rotations and so on being applied live in the interface").
4. AI to remove backgrounds and upscale (R10).

Two architectures were considered:

- **A, two binaries:** `bigiris` (viewer) and `bigprisma` (converter), following the GNOME HIG principle of "one app, one purpose".
- **B, one binary:** `bigiris` containing the internal Íris (viewer) and Prisma (converter) modules, invoked through subcommands, `--dialog=X` flags or directly from the GUI.

## Decision

**Architecture B.** A single `bigiris` binary, one `bigiris` PKGBUILD, one main `.desktop` file. The conversion module exists as a submenu of the viewer and as modal dialogs invoked by file manager service menus.

## Rationale

1. **Shared components.** The viewer and the dialogs use the same decode, transform, preview and encode core, without duplicating safety rules and output names.
2. **Model cache within each batch.** A BiRefNet session is reused by the files processed in the same AI flow; independent dialogs stay isolated in their own processes.
3. **Development speed.** One codebase, one CI, one PKGBUILD, one submission to BigCommunity/BigLinux/Flathub.
4. **UX without fragmentation.** The user installs `bigiris` and has everything. Service menus launch `bigiris --dialog=X`: same identity, same icon, same history, same preferences.
5. **Solid precedent.** LibreOffice (soffice) does exactly this: one binary, several `.desktop` files, several perceived app identities.

## Accepted trade-offs

- **Bloat**: the binary carries code for every module even when the user only wants to convert. Mitigated by lazy loading: AI widgets are created only when the AI dialog opens; the optional `ai` feature in `Cargo.toml` allows a lean distribution build.
- **Crash blast radius**: a panic brings down the current process. The advanced dialogs are launched as independent processes, limiting the impact on an already open viewer.
- **Brand image**: "Prisma" loses visibility as a separate product. Mitigated by keeping "Prisma" as the name of the conversion module in dialog titles and marketing material: the Íris/Prisma story survives at no architectural cost.

## Reversal path

If a strong reason to split ever appears (for example a viewer Flatpak without AI and a converter Flatpak with AI), the split is feasible because:

- `bigimage-core`, `bigimage-ai` and `bigimage-integrations` are brand-agnostic crates.
- The viewer and dialog modules live in Rust submodules of the `bigiris` crate that can be promoted to separate library crates.
- The core API is already stable across both.

## Rejected alternatives

- **A, two binaries under a BigPictus family.** A significant increase in maintenance cost with no proportional benefit. The natural live preview is lost.
- **One binary without a CLI mode.** Would violate R3 (CLI mirroring the GUI).
- **Three binaries (viewer, converter, headless CLI).** Explosive complexity, with no concrete gain.
