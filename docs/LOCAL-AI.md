# Local AI in BigIris

> All of BigIris's AI processing runs **on your computer**. No upload, no
> account, no external API and no download: the models come from system
> packages.

## Why this matters

Popular web services for background removal and photo "enhancement" have three
problems that BigIris solves by design:

1. **Privacy.** You send the image to the vendor's server. The service may
   log it, index it, train new models on it or leak it. For anyone who works
   with sensitive material (documents, identity papers, medical records,
   evidence in court cases), that is incompatible with a duty of
   confidentiality and with data protection law such as Brazil's LGPD.
2. **Dependency and operational risk.** The service may disappear, change its
   price, rate-limit requests or be down when you need it. BigIris works
   offline, always with the same result.
3. **Opaque licensing.** Many "open weights" models carry a hidden
   *non-commercial* license (`CC-BY-NC`, RAIL-M). BigIris's four models have
   FOSS licenses: MIT, BSD-3-Clause and Apache-2.0.

## The engines: image-engine-native

BigIris uses BigLinux's
[image-engine-native](https://github.com/biglinux/image-engine-native): each
model hand-written in pure Rust, with no ONNX Runtime, PyTorch or Python,
checked against the original PyTorch model. It runs on a GPU with Vulkan 1.2
(half precision) when there is one, and on the CPU (AVX2, AVX or SSE2, picked
at run time) when there is not. The version in use is pinned by commit in the
workspace `Cargo.toml`.

| Tool | Model | Package (weights) | Weights license |
|---|---|---|---|
| Remove background | BiRefNet-lite | `birefnet-native` | MIT |
| Upscale | Real-ESRGAN general x4v3 (dn 0.5) | `realesrgan-native` | BSD-3-Clause |
| Remove noise | SCUNet color, real PSNR | `scunet-native` | Apache-2.0 |
| Erase object | Big LaMa | `lama-native` | Apache-2.0 |

The weights live in `/usr/share/<package>/` and are read only from there. For
testing and development, one variable per model points to another file:
`BIGIRIS_BIREFNET_WEIGHTS`, `BIGIRIS_REALESRGAN_WEIGHTS`,
`BIGIRIS_SCUNET_WEIGHTS` and `BIGIRIS_LAMA_WEIGHTS`. The integrity of the
weights is the package manager's responsibility (repository signature), as for
any other system file.

The engine's own variables:

- `IMAGE_ENGINE_NATIVE_GPU=0` keeps everything on the CPU; `=1` turns a
  missing GPU into an error that explains the reason.
- `IMAGE_ENGINE_NATIVE_TIER=N` limits the CPU instruction set.

A loaded model stays loaded between runs; the viewer gives its memory (and
the GPU) back when its edit panel closes. If the GPU fails in the middle of a
run, BigIris repeats that run on the CPU and stays on the CPU until the
process ends.

## How to use it

```bash
sudo pacman -S birefnet-native realesrgan-native scunet-native lama-native
```

Each tool works without the others. The BigLinux `bigiris` package already
depends on `realesrgan-native`; the other weights are optional. Without its
package, a tool shows as unavailable, with the install command; without
`realesrgan-native` (manual install or another distribution), upscaling uses
Lanczos3.

From the file manager, the **BigIris ▸ AI** submenu opens the **Remove
background (BiRefNet)** and **Upscale…** dialogs.

### CLI

```bash
bigiris remove-bg photo.jpg                   # photo_nobg.png (PNG with alpha)
bigiris upscale --factor 4 photo.jpg          # photo_400pct.jpg
bigiris upscale --engine lanczos photo.jpg    # no AI
bigiris denoise photo.jpg                     # photo_denoised.jpg
bigiris inpaint --mask mask.png photo.jpg     # photo_inpaint.jpg
```

The device (for example `GPU: Intel(R) Graphics` or `CPU`) is printed on
stderr with the first image. The CLI uses every CPU core.

`denoise` and `inpaint` write in the source format. PNG stays lossless, but
JPEG, WebP and AVIF are re-encoded (at the `maximum` quality target), so even
the pixels the tool did not touch change slightly; to keep the exact result,
convert to PNG. Metadata (EXIF, GPS) is not copied to the output, and 16-bit
sources come out as 8-bit.

### Viewer

In the edit panel (`E`), the **Tools** section offers **Remove background
(AI)**, **Remove noise (AI)** and **Upscale** with the factor. They run with a
progress bar and a cancel button, and become undo steps; the result is cached,
so later edits do not run the model again. Each button's tooltip shows where
the model runs after the first load. The interface leaves one core free for
the desktop.

Upscaling records the engine used (Real-ESRGAN or Lanczos3) with the edit, so
**Save** and **Save as…** recompute the same thing from the file's pixels
(keeping metadata and color profile).

### Prisma

`bigiris --dialog=remove-bg` and `bigiris --dialog=upscale` process several
files; the model loads once for the whole batch.

## How each tool treats the image

All networks work in 8-bit sRGB. 16-bit images are reduced to 8 bits;
floating-point images (EXR, HDR) go through the sRGB curve. The output of these
tools is 8-bit (RGB, or RGBA if the source had alpha).

- **Remove background:** the image is scaled to 1024 × 1024 (the resolution
  BiRefNet was trained at), the mask goes back to the original size and is
  multiplied into any existing alpha. The output is always RGBA.
- **Upscale:** Real-ESRGAN always upscales 4×, in strips of rows; for 2× and
  3×, each strip is reduced with Lanczos3 as soon as it leaves the network, so
  the whole 4× image is never in memory. Alpha is upscaled with Lanczos3. The
  limit is that the result fits in 512 MiB as RGBA (about 134 MP): a photo of
  up to 33 MP at 2×, 14 MP at 3× or 8 MP at 4×.
- **Remove noise:** SCUNet in tiles of up to 896 px with a 64 px margin; alpha
  does not change. It costs 2.3 TFLOP per megapixel: a 12 MP photo takes about
  a minute and a half on a laptop CPU (much less on the GPU).
- **Erase object:** LaMa sees the whole image at once, so BigIris cuts a box
  around each group of marks (with context; distant marks get separate boxes),
  reduces each box to at most 1024 px on the longer side, fills it and pastes
  it back only inside the mask, with a feathered edge. The pixels outside the
  mask (grown by a few pixels) come out of the network unchanged; only
  re-encoding to a lossy format alters them. API for the future brush:
  `bigimage_ai::inpaint::inpaint(input, mask, progress)`.

## Memory

Measured in this project, peak of the whole process on a Core Ultra 5 225H CPU:

| Model | Input | CPU | GPU (Intel Arc, Vulkan) |
|---|---|---|---|
| BiRefNet-lite | 800 × 600 | 0.34 GB | 0.13 GB |
| LaMa | 256² box / 1024 × 773 | 0.34 / 0.59 GB | 0.15 GB |
| Real-ESRGAN | 384 × 256 → 4× | 0.59 GB* | 0.15 GB |
| SCUNet | 768 × 512 | 0.65 GB* | 0.15 GB |

\* cumulative within the same test run, after the previous models.

Before loading a model and before each run, BigIris estimates the memory it
needs (weights, network and image buffers, with a margin) and compares it with
what is free right now: `MemAvailable`, cgroup v2 limits (inactive file cache
counts as free, because the kernel reclaims it first) and the virtual address
space limit. Swap does not count. One AI tool runs at a time in the process,
so two tools started together do not count the same free memory. If it does
not fit, the tool refuses with an explanation, without allocating anything; the
viewer and conversions stay available. This is a preventive guard, not a hard
quota.

## Tests

```bash
# unit tests (no weights)
cargo test -p bigimage-ai --features native

# real models, ignored by default
BIGIRIS_BIREFNET_WEIGHTS=… BIGIRIS_REALESRGAN_WEIGHTS=… \
BIGIRIS_SCUNET_WEIGHTS=… BIGIRIS_LAMA_WEIGHTS=… \
  cargo test --release -p bigimage-ai --features native --test models -- \
  --ignored --test-threads=1 --nocapture
```

CI downloads the four packages from the BigLinux repository (pinned by name
and SHA-256), extracts the weights and runs these tests on the CPU.
