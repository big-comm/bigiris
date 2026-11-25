<p align="center">
  <img src="data/icons/hicolor/scalable/apps/com.biglinux.Iris.svg" alt="BigIris" width="160">
</p>

<h1 align="center">BigIris</h1>

<p align="center">
  <strong>A fast image viewer and an image studio on demand.</strong><br>
  Rust · GTK4/libadwaita · CLI · local processing · GNOME/Nautilus and other desktops
</p>

BigIris follows a simple idea: opening a photo should be instant and quiet;
working on many photos should offer a complete interface. That is why the
project has two experiences in the same binary:

- **Íris** is the minimal viewer: it opens on the chosen file, finds the
  sibling images in the folder, navigates in natural order and keeps the photo
  at the center.
- **Prisma** appears when you need to convert, resize, crop, adjust, compare,
  animate or process a whole folder.

The compact flow and the shortcuts take IrfanView as their functional
reference. The interface uses GTK4/libadwaita, with priority on BigCommunity
GNOME and integration with other desktops too. The result follows the theme,
uses the system's symbolic icons and keeps advanced operations out of the main
path.

The start screen opens in a compact window. Use **Open images…**, drag a
selection of local files, or paste files copied in the file manager with
`Ctrl+V`. **Process a folder…** opens the batch tools.

## Highlights

- Image and thumbnail decoding off the GUI thread.
- Fast folder navigation with coalescing: stale results are discarded when the
  user steps through several images quickly.
- Zoom centered on the cursor, drag to pan, 1:1, fit to window, full screen,
  slideshow, thumbnail strip and RGB histogram.
- Non-destructive rotate, flip and crop, with undo/redo. Cropping happens
  directly on the open image (`Ctrl+R`): drag the corners or edges, pick an
  aspect ratio and confirm with `Enter`. `Ctrl+S` confirms and writes by atomic
  replacement, keeping the original if the encoder fails.
- Translucent side edit panel (**Edit** button or `E`): rotate, flip,
  straighten (−45° to +45°, with a grid and automatic cropping of the empty
  edges) and crop; brightness, contrast, saturation and gamma with a live
  preview; resize with locked aspect ratio or by percentage; remove background,
  remove noise and upscale with AI directly on the image, with progress and a
  cancel button; **Save** and **Save as…** (writes an edited copy without
  touching the original). Each gesture becomes an undo step, and the result of
  the heavy tools is kept so that later edits do not repeat them. **View
  original** (or holding `\`) shows the image without the edits.
- Parallel, cancelable batch processing with progress, elapsed time and
  before/after size.
- Integration with Dolphin, Nautilus, Nemo, Thunar, PCManFM-Qt and elementary
  Files.
- Folder action in Nautilus and Dolphin: **BigIris → Process this folder…**.
- Local AI, with no image uploads and no downloads: background removal
  (BiRefNet-lite), upscaling (Real-ESRGAN), noise removal (SCUNet) and object
  erasing by mask (LaMa, CLI only for now). See [Local AI](#local-ai).
- Preventive limits of 1 GiB per file, 256 MP and 512 MiB per image buffer.

## Zoom

Place the cursor over the image detail and use the mouse wheel, `+`/`-` or the
zoom buttons. The transition is gradual, with proportional touchpad movements,
and honors the GTK preference to disable animations. Magnification keeps that
point fixed, within the image's scroll limits. `1` shows 100%; `0` fits to the
window. When the cursor position is unknown, the reference point is the center
of the visible area.

## Local AI

The AI tools use the engines from BigLinux's
[image-engine-native](https://github.com/biglinux/image-engine-native): the
models written in pure Rust, with no ONNX Runtime, PyTorch or Python. They run
on the GPU through Vulkan 1.2 when there is one (the first GPU load compiles the
shaders, which takes a few seconds and is cached) and on the CPU otherwise. The
device in use shows in the tooltip of the panel tools and in the CLI output.
The weights are **not downloaded**: they come from system packages, one per
tool, and each tool works without the others.

| Tool | Model | Package | Weights |
|---|---|---|---|
| Remove background | BiRefNet-lite (MIT) | `birefnet-native` | 90 MB |
| Upscale | Real-ESRGAN general x4v3 (BSD-3-Clause) | `realesrgan-native` | 4.9 MB |
| Remove noise | SCUNet (Apache-2.0) | `scunet-native` | 37 MB |
| Erase object | Big LaMa (Apache-2.0) | `lama-native` | 102 MB |

```bash
sudo pacman -S birefnet-native realesrgan-native scunet-native lama-native
```

The BigLinux package already installs `realesrgan-native` (4.9 MB) as a
dependency; the other weights are optional. Without its package, a tool shows
as unavailable and says which package to install; without `realesrgan-native`
(manual install or another distribution), **Upscale** uses Lanczos3. For
testing and development, `BIGIRIS_BIREFNET_WEIGHTS`,
`BIGIRIS_REALESRGAN_WEIGHTS`, `BIGIRIS_SCUNET_WEIGHTS` and
`BIGIRIS_LAMA_WEIGHTS` point to other weight files;
`IMAGE_ENGINE_NATIVE_GPU=0` forces the CPU.

Memory measured in this project (whole process, CPU): about 0.3 GB for
background removal, 0.1 to 0.4 GB plus the output image for Real-ESRGAN (it
works in tiles), 0.4 to 0.7 GB for SCUNet (also in tiles) and 0.25 GB to
0.5 GB for LaMa, which runs only on a box around the mask, scaled down to
1024 px on the longer side. Before loading and before each run, the
application checks whether the estimate fits in the memory free right now
(Linux available memory, cgroup v2 limits and virtual address space limits;
swap does not count) and refuses with an explanation if it does not fit. The
viewer and conversions stay available.

The networks work in 8-bit sRGB: 16-bit and floating-point images come out of
these tools as 8-bit (Lanczos3 keeps the bit depth). The alpha channel is
preserved: upscaled with Lanczos3 by Real-ESRGAN, untouched by SCUNet and LaMa.

## Formats

The complete default build reads and writes the formats provided by the
current pipeline: PNG, JPEG, WebP, AVIF, TIFF, BMP, GIF, ICO, PNM, TGA, QOI,
Radiance HDR and OpenEXR. HEIC/HEIF, JPEG XL, SVG and camera RAW are not
advertised as supported yet.

Static conversions re-encode the pixels and remove personal metadata. RGB ICC
profiles are normalized to sRGB; PNG/JPEG receive an sRGB profile generated
without data from the original image. Static editing of animations and
multi-page TIFF is refused to prevent content loss. The viewer shows the first
frame or page; animated playback and multi-page navigation remain pending. ICO
is limited to 256 × 256. Read the limitations in
[docs/QUALITY.md](docs/QUALITY.md) before converting a collection.

### Quality and size

Each conversion looks for **the smallest file that keeps the chosen visual
quality**, measured with SSIMULACRA2 against the written pixels (images with
transparency are scored over black and white backgrounds, and the worse score
counts):

| Mode (`--target`) | Minimum score | Use |
|---|---|---|
| `maximum` (`max`) | 85 | differences only show when comparing zoomed in |
| `balanced` (default) | 80 | no visible loss in normal use |
| `smallest` (`small`) | 70 | high quality, smaller files |

- **JPEG**: mozjpeg (progressive, trellis, optimized Huffman), 4:2:0, and
  4:4:4 when the colors require it.
- **WebP**: lossy libwebp (equivalent to `cwebp -m 6 -sharp_yuv -af`), compared
  with lossless and near-lossless versions; the smallest one that meets the
  target wins.
- **AVIF**: 10-bit ravif; fast search and a slower final encode.
- **PNG**: lossless optimization (oxipng); a 256-color palette only when it
  meets the target and comes out smaller (never with `maximum`; 16 bits only
  with `smallest`).
- **GIF**: exact palette or imagequant, with the dithering chosen by the score.
- **ICO, BMP, TGA, TIFF, OpenEXR**: the smallest lossless form of each (optimized
  PNG, palette/24 bits, RLE, Deflate with predictor, ZIP/PIZ).
- **QOI, PNM, HDR** have no size parameters.

`--quality N` (1 to 100) sets the JPEG/WebP/AVIF quality without a search; for
WebP, 100 writes lossless. When converting to the **same format**, if the
result would be larger than the original and the original has no personal
metadata (EXIF, XMP, text chunks), the original is copied without
recompression.

## Installing on BigCommunity GNOME

The runtime requires `bubblewrap` and `util-linux`, with unprivileged
namespaces available. The application refuses to decode if the sandbox cannot
start.

To build and package the local checkout on the Arch base:

```bash
cd pkgbuild
makepkg -si -p PKGBUILD.local
```

The Nautilus integration depends on `nautilus-python`. The package installs the
extension system-wide; you do not need to run `install-integrations --user`. To
load the extension after installing or upgrading, close Nautilus with
`nautilus -q` and open it again.

The package includes catalogs for 29 languages adopted by the BigCommunity
applications; English is the source language and needs no catalog. The
interface and the Nautilus extension follow the session language; see the
inventory and the maintenance flow in [CONTRIBUTING.md](CONTRIBUTING.md#i18n).

The test campaign prioritizes GNOME/Wayland; see the results, limits and
criteria for official adoption in [docs/QUALITY.md](docs/QUALITY.md), and the
reproducible commands in
[CONTRIBUTING.md](CONTRIBUTING.md#quality-campaign-on-gnome).

## Installing on OpenMandriva

To install the checkout for your user only:

```bash
sudo dnf install rust cargo lib64gtk4.0-devel lib64adwaita-devel \
  shaderc gettext bubblewrap util-linux
./install.sh
```

The installer builds the release version, places the application in
`~/.local`, registers only the file managers it detects and refreshes the
Plasma cache (`kbuildsycoca6`). If the menu does not show in an old Dolphin
window, close that window and open another.

The AI weight packages do not exist on OpenMandriva yet: there the AI tools
show as unavailable (and upscaling uses Lanczos3).

For ABF/Cooker, use the recipe and instructions in
[`packaging/openmandriva/`](packaging/openmandriva/README.md). It uses a
vendored tarball so the build is offline and reproducible.

## Other installations

On Arch, BigLinux or Manjaro:

```bash
cd pkgbuild
makepkg -si -p PKGBUILD.local
```

The `PKGBUILD` without a suffix is the BigCommunity build's: it derives the
version from the date and time, which change during a long build, so a local
`makepkg -si` cannot find the generated package. `PKGBUILD.local` has the same
recipe with a fixed version.

On other distributions, after installing Rust (1.95 or later), GTK4,
libadwaita, dav1d, gettext and `glslc` (from shaderc, used only at build time):

```bash
cargo build --locked --release --features "gui ai" -p bigiris
./install.sh --no-build
```

Thunar actions are always per user: `bigiris install-integrations --user`
merges the BigIris entries into `~/.config/Thunar/uca.xml`, keeping the actions
you already had. If you do not have that file yet, it starts as a copy of
`/etc/xdg/Thunar/uca.xml` (as Thunar itself does), so the system's default
actions are not hidden. The packages do not install `/etc/xdg/Thunar/uca.xml`:
that file belongs to the `thunar` package. Thunar users must run the command
above once per user (`install.sh` already does).

With the package installed, `install-integrations --user` does not write
personal copies of the integrations the package already provides in
`/usr/share` (Dolphin, Nautilus, Nemo, PCManFM-Qt, elementary Files, launcher
and icon), and it deletes old personal copies of them, which would otherwise
shadow the package's and go stale. In practice, only the Thunar actions are
written.

To remove the personal integrations before deleting the binary:

```bash
bigiris uninstall-integrations
```

To undo everything `install.sh` installed (integrations, binary, schema and
translations in `$PREFIX`), use the same `PREFIX` as the installation:

```bash
./install.sh --uninstall
```

## Usage

```bash
# Viewer / folder
bigiris photo.jpg
bigiris *.png
bigiris

# Prisma
bigiris --dialog=batch folder-with-photos
bigiris --dialog=convert *.jpg
bigiris --dialog=resize *.png
bigiris --dialog=crop photo.jpg
bigiris --dialog=adjust *.jpg
bigiris --dialog=compare before.png after.png
bigiris --dialog=remove-bg photo.jpg
bigiris --dialog=upscale photo.jpg

# CLI
bigiris convert --to avif --overwrite increment *.jpg
bigiris convert --to webp --target smallest *.png
bigiris convert --to jpg --quality 90 photo.png
bigiris resize --max-edge 1080 *.png
bigiris rotate --degrees 90 photo.jpg
bigiris flip --axis horizontal photo.jpg
bigiris crop --rect 800x600+100+50 photo.jpg
bigiris adjust --brightness 15 --contrast 10 photo.jpg
bigiris animate -o output.gif --delay 100 frame_*.png
bigiris remove-bg --overwrite increment photo.jpg
bigiris upscale --factor 4 photo.jpg               # Real-ESRGAN, or Lanczos3
bigiris upscale --factor 2 --engine lanczos *.png  # no AI
bigiris denoise noisy-photo.jpg                    # <name>_denoised.jpg
bigiris inpaint --mask mask.png photo.jpg          # white = erase
bigiris to-pdf --overwrite replace --timeout 300 document.docx
bigiris batch --profile docs/batch-profile.example.toml folder-with-photos

# Integrations and diagnostics
bigiris install-integrations --user
bigiris uninstall-integrations
bigiris --self-test
```

Every command that writes files accepts `--overwrite skip|replace|increment`
and, by default, **does not overwrite** (`skip`): the existing file is kept and
the output shows `skip`. This also applies to `animate` (the target `.gif`),
`remove-bg` (`<name>_nobg.png`), `upscale` (`<name>_<pct>pct.<ext>`),
`denoise` (`<name>_denoised.<ext>`), `inpaint` (`<name>_inpaint.<ext>`) and
`to-pdf`, which skips an existing `<name>.pdf` instead of replacing it.
`upscale --engine` accepts `auto` (default: Real-ESRGAN if installed, otherwise
Lanczos3), `realesrgan` (fails if the package is missing) and `lanczos`. In
`inpaint`, the mask has the size of the image and the white area is filled from
its surroundings. Apart from the marked areas and a narrow transition band
around them, the pixels stay identical in PNG and TIFF; JPEG, WebP and AVIF are
recompressed at the `maximum` quality target, as in `denoise`. The outputs of
the AI tools are 8-bit and do not carry the source EXIF. Without the model
package, `remove-bg`, `denoise`, `inpaint` and `upscale --engine realesrgan`
stop with exit code 1 and say what to install. `to-pdf --timeout <seconds>`
(1 to 3600, default 120) limits the time of each document in LibreOffice; when
it runs out, the process and its children are terminated and no partial PDF is
left behind. Outputs that would collide with each other in the same command are
refused before anything is written. With `--notify` (used by the context menus,
which do not show the terminal), a failure also shows in a desktop
notification, through `notify-send` from the `libnotify` package. The messages,
the notification and `--help` follow the system language; the status prefixes
`ok`, `skip`, `err` and `info` do not change, so scripts can read them.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Success, including when files were skipped because they already exist |
| `1` | Invalid usage or fatal error before processing; nothing was written |
| `2` | Partial failure: one or more input files could not be processed |

The same codes appear in `bigiris --help`.

### Viewer shortcuts

| Shortcut | Action |
|---|---|
| `←` / `→`, `Home` / `End` | Navigate the folder |
| mouse wheel, `+` / `-` | Zoom |
| `0` / `1` | Fit / actual size |
| drag | Pan |
| `R` / `Shift+R` | Rotate right / left |
| `Ctrl+R` | Crop on the image itself (`Enter` confirms, `Esc` cancels) |
| `E` | Open / close the edit panel (`Esc` closes) |
| `\` (hold) | View the original image, without the edits |
| `Ctrl+Z` / `Ctrl+Shift+Z` | Undo / redo |
| `Ctrl+S` | Save edits safely |
| `Space` | Slideshow |
| `F` / `G` / `T` | Thumbnails / histogram / theater mode |
| `H` | Keep controls visible |
| `F11` / `Esc` | Full screen / close |

## Architecture

```text
bigiris/
├── crates/
│   ├── bigimage-core/          headless pipeline and transforms
│   ├── bigimage-ai/            local AI (image-engine-native engines)
│   ├── bigimage-integrations/  ServiceMenus and file manager actions
│   └── bigiris/                CLI, Íris viewer and Prisma dialogs
├── data/                       desktop entry, AppStream, GSettings, icon and i18n
├── packaging/openmandriva/     ABF spec and vendor tarball creation
└── pkgbuild/                   Arch/BigLinux packages
```

Batches share a single pool in
[`batch_runner.rs`](crates/bigiris/src/gui/batch_runner.rs). The global
allocator is mimalloc, resizing uses SIMD through `fast_image_resize`, and the
pool limits concurrency by CPU, image dimensions and available memory (up to
four jobs).

## Development and quality

Rust stable 1.95 or later, and `glslc` (shaderc) for the `ai` feature:

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

Reports and patches are welcome: read [CONTRIBUTING.md](CONTRIBUTING.md).
Vulnerabilities must follow [SECURITY.md](SECURITY.md), with no public issue.

The tests with the real models are ignored by default; with the weights
installed (or pointed to by the `BIGIRIS_*_WEIGHTS` variables):

```bash
cargo test --release -p bigimage-ai --features native --test models -- \
  --ignored --test-threads=1 --nocapture
```

## License

GPL-3.0-or-later. See [LICENSE](LICENSE). The licenses of the libraries built
into the binary (Rust crates, mozjpeg, libwebp, libdeflate, mimalloc) are in
[THIRD_PARTY_LICENSES](THIRD_PARTY_LICENSES), generated by
`scripts/third_party_licenses.py`.

The AI engines come from
[image-engine-native](https://github.com/biglinux/image-engine-native)
(MIT, by Bruno Gonçalves / BigLinux; the exponential follows XNNPACK,
BSD-3-Clause). The weights are not part of BigIris: they are installed by the
`*-native` packages, each with the license of the original model (BiRefNet:
MIT; Real-ESRGAN: BSD-3-Clause; SCUNet and LaMa: Apache-2.0). Details in
[NOTICE](NOTICE).
