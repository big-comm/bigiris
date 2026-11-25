# Local BigIris candidate for BigCommunity GNOME

This candidate gathers the fixes and tests described in
[QUALITY.md](QUALITY.md). Adoption as an official application depends on the
distribution's acceptance testing; the local package lets that evaluation
start.

## Delivery and reproduction

- Complete binary: `target/release/bigiris`, with GTK/libadwaita and AI.
- Local package: `target/quality/candidate/bigiris-0.1.0.local-3-x86_64.pkg.tar.zst`, with its SHA-256.
- Gates: `target/quality/zoom-transition/gates/summary.json`.
- Release tests: `target/quality/zoom-transition/release/release-smoke.json`.
- Zoom frames: `target/quality/zoom-transition/painted-frames.json`.
- Instrumented fuzzing: `target/quality/fuzz/summary.json`.
- Long-running stability: `target/quality/long-soak/status.json`.

The package is reassembled with the `PKGBUILD.local` recipe from the compiled
release and the current data, after the checkout's tests. This validates the
contents and the `package()` step; it is not equivalent to a reproducible build
on a clean installation. Results that ran take precedence over commands that
were only prepared. `running` in the long-running test means pending.

To try the package on a BigCommunity acceptance-testing installation, install
the `.pkg.tar.zst` file with `sudo pacman -U path/to/package`. The runtime
requires `bubblewrap` and `util-linux`, available namespaces and the libraries
declared in the package. `bigiris --self-test` decodes a synthetic image
through the sandbox and checks that the private directories and the host
network are absent. If it fails, the candidate must not be approved on that
machine.

## Script for Leonardo and Tales

1. On a clean installation, install, upgrade and remove the package. Check the
   desktop entry, icon, schemas, translation, Nautilus and leftovers after
   removal.
2. From Nautilus, open an image, a selection and a folder; test navigation,
   zoom under the cursor with the wheel, buttons, `+`/`-`, `1` (100%) and `0`
   (fit); check smoothness with the wheel and the touchpad, reversal during the
   transition and manual drag; repeat with animations disabled in GTK. Judge
   personal visual comfort before submitting to the repository. Exercise
   conversion of the advertised formats, cancellation, errors and files being
   synced. Confirm that originals are not changed on failures.
3. Exercise keyboard and Orca, high contrast, light and dark themes, 100%, 150%
   and 200% scales and multiple monitors. The automated window test does not
   replace this accessibility and usability evaluation.
4. Compare time to first frame, navigation, RAM/GPU and interaction with the
   current viewer, using the same authorized collection and the same hardware.
   Include a low-RAM machine and a slow SSD.
5. Check orientation, color, transparency and precision with real images. RGB
   ICC is normalized to sRGB; non-RGB profiles are refused. The viewer shows
   the first frame or page; static editing refuses animations and multi-page
   TIFF. HDR has no full handling or tone mapping.
6. Evaluate the AI tools with and without the weight packages
   (`birefnet-native`, `realesrgan-native`, `scunet-native`, `lama-native`):
   nothing is downloaded, each tool without its package explains how to install
   it, and viewing and conversions stay available. Measure RAM, time,
   cancellation between files and model reuse on the CPU and on a Vulkan GPU,
   including low-RAM physical hardware; memory admission is a preventive
   check, with no hard RSS quota.
7. Require the final result of the long-running test and of Memcheck in a
   compatible environment. The soak that was started exercises the core; add
   an eight-hour GNOME session with slideshow and GPU monitoring before
   official adoption.

## Limits of this candidate

ASan/LSan, lifecycle tests and short stability runs do not prove the universal
absence of leaks. The sandbox covers the executable's decoding and metadata
inspection; AI inference and the encoders stay in the main process. The worker
has limits on virtual address space, CPU, deadline, files, descriptors and
scratch, with no cgroup RSS quota and no seccomp filter.

The real inferences recorded for this candidate used the previous ONNX engine,
with models downloaded on demand. The image-engine-native engines that replaced
it still need the same evaluation on the final package.

The first CI of PR #1 passed ten jobs, including Arch packaging and the
eight-cycle Memcheck of the core. Four jobs failed because fuzzing selected
stable and the sandbox was unavailable on the Ubuntu runner. The configuration
was fixed, but confirmation needs a new remote run. The local eight-hour core
test also passed; GUI, GPU and AI are not part of that test. Results and limits
are in [QUALITY.md](QUALITY.md). No broad privileges were granted to the Arch
container. Publishing to the distribution repository and the default MIME
association need a maintenance decision after acceptance testing.

## Managing through Git

Open the BigIris root folder that contains `.git`, `Cargo.toml`, `crates/` and
`pkgbuild/`. Check the selected branch before integrating changes. Opening that
folder in a Git client lets you review diffs and new files; it does not publish
or update the distribution repository automatically. Check the files included
in the commit, including new modules and tests. The artifacts in
`target/quality/` are local evidence and must not enter the history.

Send the candidate to the development branch first and get a green remote CI.
Integration into the official repository must follow the acceptance script
above and the maintenance decisions of Leonardo and Tales.
