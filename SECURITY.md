# Security Policy

## Supported versions

Only `main` and the latest tag receive fixes. There is no LTS line yet: the
project is pre-1.0 and releases ship weekly.

## Reporting a vulnerability

**Do not open a public issue or pull request for security matters.** Use one
of the private channels:

1. **GitHub Security Advisories**, at
   [github.com/big-comm/bigiris/security/advisories/new](https://github.com/big-comm/bigiris/security/advisories/new).
   Preferred: encrypted in transit and invisible until disclosure.
2. **Email** to `leoathayde@gmail.com` with a subject starting with
   `[BigIris-Security]`. If you want PGP, ask for the public key through the
   same channel.

Include, as far as possible:

- The affected BigIris version (`bigiris --version`).
- Distribution and desktop environment (OpenMandriva/BigLinux/Arch/etc.,
  KDE/GNOME/...).
- A minimal reproduction. If it involves a malicious file, describe it without
  attaching the exploit to this message; agree on a secure channel afterwards.
- The observed impact (for example reading files out of scope, code execution,
  privilege escalation).

## Response time

| Severity                                      | First response |
|-----------------------------------------------|----------------|
| Critical (RCE, privilege escalation)          | within 48 h    |
| High (local DoS, arbitrary file read)         | within 7 days  |
| Medium / Low                                  | within 30 days |

## Scope

In scope:

- Image file decoding (`bigimage-core`).
- The local AI pipeline (`bigimage-ai`): reading the weights from the system
  packages and image pre- and post-processing.
- The file manager service menu generators (`bigimage-integrations`).
- The GTK4 viewer (`bigiris/gui`).
- The project's PKGBUILDs (`pkgbuild/`).

Out of scope (report upstream): vulnerabilities in `image`,
`fast_image_resize`, `dav1d`, `gtk4`, `libadwaita`, in the
[image-engine-native](https://github.com/biglinux/image-engine-native) engines,
in Vulkan drivers or in the kernel.

## Hardening already in place

For context on what is already defense in depth:

- `#![forbid(unsafe_code)]` in the library crates; the `bigiris` binary has
  `#![deny(unsafe_code)]` with audited exceptions in
  `crates/bigiris/src/main.rs` and `crates/bigiris/src/codec.rs`.
- Regular inputs limited to 1 GiB, 256 MP and 512 MiB per decoded buffer, in
  the viewer and in conversions alike. The codecs' scratch limits are best
  effort. The executable confines decoding to Bubblewrap processes without
  network and without access to home/run/etc: `/usr` and the executable are
  read-only, and only the descriptor of the given image is mounted. `/dev` is
  remounted read-only and the `/tmp` tmpfs is limited to 64 MiB. `prlimit`
  enforces 2 GiB of virtual address space, 30 s of CPU, a 60 s supervision
  deadline, 64 descriptors and a per-output-file limit of 512 MiB plus header.
  This is not a total RSS/cgroup quota nor a seccomp filter.
- Service menus written with `O_NOFOLLOW` to prevent following symlinks during
  `sudo install-integrations --system`.
- Nothing is downloaded: the application has no network client. The AI weights
  come only from the optional `birefnet-native`, `realesrgan-native`,
  `scunet-native` and `lama-native` packages, read from
  `/usr/share/<package>/`, and all four have FOSS licenses (MIT, BSD-3-Clause,
  Apache-2.0).
- Thunar UCA commands wrapped in `sh -c '… "$@"' …` so that file names are not
  reinterpreted by the shell.

### Limitations to consider before official distribution

The executable's decoding, format inspection and EXIF go through the confined
worker. Standalone consumers of `bigimage-core` run locally by default.
Encoders, GTK and AI inference (image-engine-native engines, CPU or Vulkan)
stay in the application process. The sandbox does not replace review of the
native libraries or the kernel. The quality report records the tests run and
the gaps; a short run without RSS growth does not prove the absence of leaks.

Static conversions remove personal EXIF/XMP; RGB ICC profiles are converted to
sRGB, and invalid or non-RGB profiles are refused explicitly. Animations and
multi-page TIFF are refused in static editing. There is no editing support that
preserves their frames or pages, nor full HDR handling. New writes use private
files; replacements keep the permissions of the regular target when available.
Atomic publication protects against encoder failure, but does not promise
recovery after a power loss on every file system.

### Memory admission for AI

Each model estimates the memory it needs (weights and fixed buffers at load;
network and image buffers at each run, with a margin over the measured peaks)
and checks it before loading and before each run; it considers MemAvailable,
the limits of the cgroup v2 ancestors and RLIMIT_AS. Swap does not count. The
weights come only from the system packages (`/usr/share/<package>`), never from
the network. The policy is not an RSS quota, does not cover cgroup v1 or GPU
memory, and does not remove competition for memory after the check. Inference
stays in the main process. A smaller model needs its own measurements before it
is offered for 4 GB machines.
