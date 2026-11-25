# Contributing

BigIris is Rust + GTK4/libadwaita, a single binary with the `Íris` (viewer)
and `Prisma` (transforms) modules. This document covers what you need to
build, test, translate and send changes without friction.

The About dialog and the AppStream metadata use `BigCommunity Team` as the
responsible team and point to `big-comm/bigiris`. Individual authorship stays
in the Git history.

## Prerequisites

Arch/BigLinux-based distributions:

```bash
sudo pacman -S --needed rust pkgconf gtk4 libadwaita dav1d gettext shaderc
```

Debian/Ubuntu distributions:

```bash
sudo apt-get install -y rustc cargo pkg-config libgtk-4-dev \
    libadwaita-1-dev libdav1d-dev gettext glslc
```

Minimum Rust: **1.95** (declared in `Cargo.toml`; `rust-toolchain.toml`
selects stable). `glslc` (from shaderc) is used only when building with the
`ai` feature, for the Vulkan shaders of the AI engines.

To test the AI tools without installing the weight packages, point the
`BIGIRIS_BIREFNET_WEIGHTS`, `BIGIRIS_REALESRGAN_WEIGHTS`,
`BIGIRIS_SCUNET_WEIGHTS` and `BIGIRIS_LAMA_WEIGHTS` variables at the files
extracted from the `*-native` packages (see
[docs/LOCAL-AI.md](docs/LOCAL-AI.md)). The tests that load models use a lot of
memory: run them with `--test-threads=1`.

The mozjpeg, libwebp and libdeflate (oxipng) encoders are built from the C
sources shipped inside the crates, with the system C compiler (`base-devel` on
Arch, `build-essential` on Debian/Ubuntu). Neither `nasm` nor `cmake` is
needed: mozjpeg's assembly SIMD is turned off.

## Building

```bash
# Build without the GUI (fast, headless; what CI runs):
cargo build

# Full build (viewer + dialogs + AI):
cargo build --features "gui ai" -p bigiris

# Release:
cargo build --release --features "gui ai" -p bigiris
```

## Testing

```bash
# Whole workspace (headless):
cargo test --workspace --all-targets --locked

# GUI + AI across the workspace (needs the system libraries above):
cargo test --workspace --all-features --all-targets --locked

# Binary self-test:
./target/release/bigiris --self-test
```

## Quality before each commit

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-features --all-targets --locked -- -D warnings
```

All three are mandatory CI gates (`.github/workflows/ci.yml`). Warnings become
errors, so pull requests with Clippy warnings do not pass.

## Quality campaign on GNOME

The runner keeps the logs, commands, exit codes and duration of each check in
`target/quality/`. Failures stay in the record; a new run produces new
evidence. The matrix and the measured results are in
[docs/QUALITY.md](docs/QUALITY.md).

```bash
# Rust/Python contracts, Clippy and integration files:
python3 scripts/quality.py --gui

# In the GNOME session, also opens and destroys windows with synthetic images:
python3 scripts/quality.py --gui --graphical

# Release benchmark, 64 concurrent conversions and 5 minutes of stability:
python3 scripts/quality.py --gui --graphical --performance --soak-seconds 300

# Install the auditor only in this checkout's build directory:
cargo install --locked cargo-audit --root target/quality/tools
python3 scripts/quality.py --gui --audit

# After fetching dependencies and updating the RustSec database, an offline rerun:
python3 scripts/quality.py --gui --graphical --audit --offline
```

The runner also requires `desktop-file-validate`, `appstreamcli`,
`glib-compile-schemas` and `msgfmt`. `--graphical` requires Wayland or X11; in
CI we use Xvfb and an isolated D-Bus session. The GSettings backend is the
memory backend during the graphical tests, to keep personal preferences
intact. The test refuses to run directly without `GSETTINGS_BACKEND=memory`.
It covers keeping the cursor point during zoom, checked by real geometry and
rendered pixels, as well as cancellation during navigation and state release
on close.

The CLI suite with the `ai` feature tests the refusal under a 512 MiB
RLIMIT_AS before the model loads, with no output and the original preserved,
and the missing-package message. The tests that run the real models only run
with the weights installed or pointed to by `BIGIRIS_*_WEIGHTS`; see
[docs/LOCAL-AI.md](docs/LOCAL-AI.md) and `scripts/ai_smoke.py`.

Optional screenshots capture only the widgets of the synthetic windows:

```bash
BIGIRIS_TEST_SCREENSHOTS_DIR="$PWD/target/quality/screenshots" \
BIGIRIS_TEST_COLOR_SCHEME=light \
python3 scripts/quality.py --gui --graphical
```

Repeat with `BIGIRIS_TEST_COLOR_SCHEME=dark` for the dark theme. For Memcheck,
build the probe and use debug symbols matching the installed glibc:

```bash
cargo build -p bigimage-core --example quality_probe --locked
valgrind --tool=memcheck --leak-check=full \
  --errors-for-leak-kinds=definite,indirect --error-exitcode=99 \
  ./target/debug/examples/quality_probe soak 1 320x240
```

The probe measures the core with the default allocator; the application uses
mimalloc. RSS stability and the release of GTK references are checks separate
from Memcheck and do not, on their own, show the absence of leaks.

## Layout

```
crates/
├── bigimage-core/         # decode/encode/transforms, pure Rust, headless
├── bigimage-ai/           # local AI: image-engine-native engines (feature `native`)
├── bigimage-integrations/ # service menus for the 6 file managers
└── bigiris/               # binary (CLI + GUI behind the `gui` feature)
data/
├── com.biglinux.Iris.{desktop,metainfo.xml,gschema.xml}
├── icons/hicolor/scalable/apps/com.biglinux.Iris.svg
├── nautilus/bigiris-menu.py
└── po/                    # i18n catalogs
docs/
└── ADR-00{1,2,3}-*.md     # architecture decisions
pkgbuild/
├── PKGBUILD               # build from the remote
└── PKGBUILD.local         # build from the checkout
```

The ADRs explain *why* the code is the way it is: read them before proposing a
structural refactor.

## i18n

The catalog follows the BigLinux languages. English is the source language:
every msgid is English and needs no catalog. `data/po/LINGUAS` lists 29 codes:
bg, cs, da, de, el, es, et, fi, fr, he, hr, hu, is, it, ja, ko, nb, nl, pl,
pt, pt_BR, ro, ru, sk, sv, tr, uk, zh_CN and zh_TW. Chinese has two catalogs,
`zh_CN` (Simplified) and `zh_TW` (Traditional, with Taiwan's vocabulary): a
single `zh` would show the same script to both audiences. Norwegian lives in
`nb`, not in the `no` of the BigLinux list: Norwegian systems use `nb_NO`, which
gettext and `.desktop` files look up as `nb_NO` and then `nb`, never `no`.
`pt_BR` uses Brazilian Portuguese terminology; `pt` uses European Portuguese.

Use `gettext("…")` in the Rust interface and `_("…")` in the Nautilus
extension. `n_("…")` marks constant labels; translation happens at
presentation time, so ids, options and command arguments stay unchanged. The
Nautilus domain is local to the extension and does not replace the file
manager's own texts. In the CLI, clap help uses `#[arg(help = gettext("…"))]`
and `#[command(about = gettext("…"))]`: a `///` comment becomes fixed text at
compile time and is never translated. The status prefixes (`ok`, `skip`,
`err`, `info`, `erro`) and the `--self-test` line stay untranslated, because
scripts read them.

```bash
# Extract new messages and update the catalogs:
data/po/regen-pot.sh
# Fill in or review msgstr in data/po/<language>.po; then:
python3 scripts/translations.py generate
python3 scripts/translations.py check --check-generated
```

The validator requires every language, the same message inventory, non-empty
translations, no fuzzy entries and preserved named arguments. It runs
`msgfmt --check` on each catalog and checks the generated files. The recipes
build and install every `.mo` at
`/usr/share/locale/<language>/LC_MESSAGES/bigiris.mo`. The menu files receive
localized fields; Thunar supports `xml:lang` names and descriptions, but keeps
the submenu in the source language.

The initial catalogs were prepared with local translation assistance and terms
reused from GTK/libadwaita; linguistic review by native speakers is welcome.
Technical validation does not certify fluency in every language. Preparation
models and tools stay out of the package; there is no network translation and
no additional inference while the application runs.

To check a local build with isolated catalogs, without installing:

```bash
python3 scripts/translations.py compile --destdir "$PWD/target/review-locale"
BIGIRIS_LOCALE_DIR="$PWD/target/review-locale" LANGUAGE=fr ./target/release/bigiris
```


## Conventions

- **Language**: English for everything in the repository: source strings of
  the interface and messages, docs, code, identifiers and commit messages.
  Users read their own language through the catalogs.
- **Commits**: Conventional Commits style without being dogmatic: `feat:`,
  `fix:`, `docs:`, `ci:`, `refactor:`, `security:`, `i18n:`, `deps:`. Messages
  explain *why*, not *what* (the diff already shows what).
- **Comments**: only when the "why" is not obvious from the code. Do not
  document what a well-chosen name already says.
- **Errors**: `bigimage-core` defines `BigImageError`; the binary converts it
  to `color_eyre::Result` at the edge.
- **CLI exit codes**: every command follows the same convention (`EXIT_PARTIAL`
  and the `after_help` text of `Cli` in `crates/bigiris/src/main.rs`, shown in
  `--help`): `0` success (files skipped because they already exist count as
  success), `1` invalid usage or fatal error before anything is written, `2`
  one or more input files failed. New commands that write files accept
  `--overwrite skip|replace|increment` with `skip` as the default and are
  covered in `crates/bigiris/tests/cli.rs`.
- **Thunar integrations**: they exist only in the user scope
  (`bigiris install-integrations --user` merges into
  `~/.config/Thunar/uca.xml`). Do not package `/etc/xdg/Thunar/uca.xml`: it
  belongs to the `thunar` package, and the PKGBUILDs fail if it shows up. A new
  `uca.xml` starts as a copy of the first `Thunar/uca.xml` in
  `$XDG_CONFIG_DIRS` (as Thunar does), and a file without `</actions>` is
  refused, never rewritten.
- **User scope with the package installed**: `install-integrations --user`
  skips every file manager whose `bigiris-*` files the package already
  installed in `/usr/share`, and removes old personal copies of them. In tests,
  use `ScopePaths::rooted_at` (without `package_data` or `xdg_config_dirs`) so
  the real `/usr/share` and `/etc/xdg` are never read.
- **`unsafe`**: the library crates have `#![forbid(unsafe_code)]`; the
  `bigiris` binary has `#![deny(unsafe_code)]`, with audited exceptions only in
  `crates/bigiris/src/main.rs` and `crates/bigiris/src/codec.rs`.
- **GPL-3.0-or-later**: every `.rs`/`.py` file starts with
  `// SPDX-License-Identifier: GPL-3.0-or-later`.

## Pull request flow

1. Open an issue before non-trivial changes, to agree on scope.
2. Branch from `main`.
3. Small commits, informative messages.
4. Make sure `cargo fmt --check`, both `clippy -D warnings` runs and
   `cargo test` pass before asking for review.
5. If the change affects the UI or the package, describe how you tested it
   locally (CI checks syntax, not UX).

## Reporting vulnerabilities

Do not open a public pull request or issue. See [SECURITY.md](SECURITY.md).

### Fuzzing and long-running stability

`python3 scripts/fuzz.py --seconds 120` generates only synthetic images and
instruments the core with ASan/LSan and LLVM coverage. Use Rust nightly. On
hosts that provide Rust through the system, `--allow-bootstrap` explicitly
enables the experimental options for this development build only; the release
does not use them. The report goes to `target/quality/fuzz/summary.json`. The
campaign limits the mutated input to 16 KiB and the full decode to one million
pixels; larger headers are still inspected. Limit tests and the sandbox cover
rejections outside that budget. Externally linked native libraries are not
instrumented. Valgrind remains an independent CI gate.

`python3 scripts/release_smoke.py` checks the sandbox and the 13 formats in the
release build. `python3 scripts/ai_smoke.py` runs the real background removal
on two synthetic images with transparency, with the weights from the
`birefnet-native` package (or `--weights`), and records time, RAM and RGB/alpha
preservation.

After `cargo build --release -p bigimage-core --example quality_probe --locked`,
`python3 scripts/soak.py --seconds 28800 --background` runs eight hours of the
probe in a persistent process. Check `target/quality/long-soak/status.json`:
`running` means pending, never approval. The probe exercises the core; a soak
of the GNOME session, GPU, slideshow and accessibility needs its own
validation.

To distribute the candidate, read `docs/RELEASE-CANDIDATE.md`. Installation,
default MIME association and publication are separate actions from building
the package.
