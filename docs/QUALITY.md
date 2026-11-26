# BigIris quality for BigCommunity GNOME

The local campaign of October 11, 2026 produced a candidate with fixes,
reproducible tests and a package for acceptance testing. The results below make
it possible to assess the checkout; adoption as an official application depends
on the pending criteria. There is no certification of the universal absence of
bugs or leaks.

## Environment and traceability

BigCommunity GNOME/Wayland, x86_64, Ryzen 7 5800H, 16 logical CPUs,
Rust 1.98.1, GTK 4.22.4, libadwaita 1.9.3 and dav1d 1.5.4.
Project MSRV: 1.95. Minimum GTK runtime: 4.12; libadwaita: 1.6.

Only synthetic images, regression fixtures and test directories were used.
Pre-existing changes were preserved. Local evidence lives in `target/quality/`
and is not part of the distributed package:

- `zoom-transition/gates/summary.json`: checks for the zoom transition fix.
- `zoom-transition/painted-frames.json`: painted frames and intermediate scales.
- `round3/package-fixed-gates/summary.json`: final checks, with logs per gate.
- `round2/acceptance/{benchmark,stress,soak}.log`: performance and stability.
- `fuzz/summary.json`, `decode.log` and `icc.log`: the latest instrumented campaign.
- `round2/fuzz-before-gif*`, `fuzz-before-avif*` and `fuzz-artifacts/`: earlier findings.
- `round3/release-final/release-smoke.json`: release binary, SHA-256 and 40 checks.
- `round3/ai-release/summary.json`: real inference with the previous ONNX engine, before the Nautilus packaging fix.
- `round3/ai-final/summary.json`: rerun on the final binary; check the result and the available budget.
- `round3/ai-preflight-final/summary.json`: refusal under 4 GiB and missing optional runtime, checked on the release with the previous ONNX engine.
- `round2/nautilus-real.json`: menu built with GI/Nautilus installed.
- `long-soak/status.json`: explicit state of the long-running test.
- `candidate/`: candidate package, hashes and manifest.
- `screenshots-final/` and `screenshots-light-final/`: inspection of the synthetic windows.
- `valgrind.log`: the local Memcheck blocker.

Round 3 adds RAM protection for AI and anchored zoom. Performance, fuzzing and
soak from round 2 still refer to the core, which did not change in that round.
The initial round 3 gates recorded Clippy failures and a zoom synchronization
failure; they were fixed before the final gates.

Earlier reports that failed stay recorded as failures. The
`round2/acceptance/summary.json` run had a headless Clippy failure, fixed
before the final gates; its three performance tests passed. Historical results
were not rewritten to look like approvals.

## Coverage run

| Area | Result | Reach |
| --- | --- | --- |
| Rust, all features/targets | 216 tests passed | Workspace, compiled GUI, core, AI and integrations |
| Rust, headless | 178 tests passed | Overlaps part of the 216; do not add the counts |
| GTK, separate graphical test | Passed | 30 viewer opens/closes, 13 tool/properties windows and a synthetic selection of 10,000 files; state released and at most 41 simultaneous thumbnails; six zoom scenarios checked frame by frame, intermediate scales and rendered pixels |
| Nautilus | 6 contracts passed; real GI created menus | Simulated FileInfo; a click in Nautilus and the distribution policy need acceptance testing |
| Rustfmt and Clippy | Passed, warnings as errors | Headless and all features |
| Distribution | Desktop entry, offline AppStream, schemas, English and syntax passed | Without installing or changing default associations on the host |
| Fidelity | Tests passed | EXIF, ICC RGB→sRGB, alpha, 16-bit PNG, all 13 formats; non-linear/float RGB within the advertised behavior |
| Content preservation | Tests passed | Static transforms refuse animated GIF/APNG/WebP and multi-page TIFF |
| Writing and concurrency | Tests passed | Atomic publication, target preserved on failures, concurrent Skip/Increment |
| CLI and release | 40 checks on the release | Sandbox self-test, conversion, resize and reopening of the 13 formats |
| ASan/LSan fuzzing | Latest campaign passed | 17,304 decode and 33,331 ICC executions, 61 s per target; no error reported in that sample |
| Stress | 64 conversions, 4 workers, all reopened | Passed; 0.593 s measured by the probe, peak of 51,008 KiB |
| Short stability | 300.079 s, 3,029 cycles | Stable RSS, stable descriptors and threads |
| Real AI (previous ONNX engine) | Passed before the packaging fix; 21.172 s for two images | RGB/alpha preserved, session reused; peak of 6,847,848 KiB (~6.53 GiB). Rerun on the final binary refused because available RAM was below 8 GiB |
| RustSec | Final gate passed | Zero vulnerabilities and zero unsound warnings in the database queried; one maintenance warning |
| Memcheck | Passed in CI over eight synthetic core cycles | Zero errors, zero bytes definitely/indirectly lost; 48 bytes possibly lost and 548 bytes still reachable. Does not cover GUI or AI; the local glibc loader blocker remains |
| Eight hours of the core | Passed; 28,800.047 s and 298,657 cycles | Initial/final RSS of 13,200/7,988 KiB, peak of 26,108 KiB; four descriptors and one thread before/after. Binary and final state recorded in `long-soak/`; does not cover the complete product |

The GTK tests run local loaders in the test binary. The final executable uses
the confined worker, exercised by the CLI/release tests. None of these tests
replaces a long session of the complete product with GPU and Orca.

Fuzzing is guided by LLVM coverage, instruments the core's Rust code and uses a
synthetic corpus of the formats and ICC profiles. The mutated input is limited
to 16 KiB; the full decode, to one million pixels. Headers of larger images are
still inspected. External native libraries are not instrumented. Larger limits
have regressions and are contained by the sandbox; the result does not cover
every possible file nor all encoders, GTK, AI and CLI arguments.

During the campaign, a GIF with a frame larger than the logical screen and an
AVIF with a box beyond the end of the file caused a timeout and excessive
allocation. Structural inspections without buffers proportional to the
declared sizes were added, with reproducible regressions. Another timeout came
from a valid GIF with a 33 MP screen; the pixel budget of the instrumented
target was made explicit, without reducing header coverage or pretending that
case was a fixed memory failure.

## Measured performance

Synthetic 1920 × 1080 image, release build, warm cache, three samples per
operation. Exploratory sampling in the development session; it is not a
controlled comparison with another viewer and does not measure a cold GUI
start.

| Operation | Median in ms | P95 in ms |
| --- | ---: | ---: |
| PNG decode | 11.206 | 11.230 |
| Resize to 1080 edge | 15.030 | 16.653 |
| Preview up to 800 | 15.673 | 16.840 |
| PNG | 23.588 | 23.626 |
| JPEG | 66.197 | 66.403 |
| WebP lossless | 33.396 | 34.892 |
| AVIF | 7,388.941 | 7,559.215 |

Resize and preview include decoding; conversion includes decoding,
preparation, encoding, writing and syncing. Benchmark peak: 79,660 KiB. AVIF
remains the bottleneck; limiting its threads to two contains the
multiplication of batch workers, but these numbers do not show a percentage
improvement attributable to that change.

In the five-minute soak, RSS after warm-up: 13,136 KiB, before and after; peak:
26,056 KiB; four descriptors and one thread before and after. The probe uses
the default allocator; the executable uses mimalloc. The measurements do not
cover GPU, AI memory or a large collection. The binary of the long-running test
has its hash recorded; a result holds only for that build and that script.

## Fixes and architecture

The executable delegates decoding, dimensions, metadata and previews to a
Bubblewrap worker without network/home/run/etc, with input through a read-only
descriptor, the root, `/usr` and the executable read-only, and a 64 MiB `/tmp`
as its only writable place. `prlimit` limits virtual address space to 2 GiB,
CPU to 30 s, descriptors to 64, core dumps to zero and each output file to
512 MiB plus header; the worker's `attest` mode, run by `--self-test`, checks
from inside that every mount but `/proc`, `/tmp`, `/dev/pts` and the device
nodes is read-only, the `prlimit` values, capabilities, session, parent-death
signal, environment and namespaces (the user namespace unless bwrap is
setuid). The supervisor ends the
child after 60 s or on cancellation. The pixel protocol is validated before
allocation in the parent. If the sandbox fails, the image is refused. There is
no fallback without the sandbox.

This is not an RSS/cgroup quota nor a seccomp filter. Encoders, GTK and AI
inference stay in the parent. Standalone consumers of the core library run
locally by default. The worker restricts input and output; the codecs' internal
scratch has no precise per-buffer accounting. The whole of `/usr` is visible
read-only, instead of a minimal allowlist of libraries.

The pipeline limits regular input to 1 GiB, up to 256 × 1024² pixels and
512 MiB per buffer. Batches use a CPU/RAM estimate and up to four workers.
Writing uses a unique temporary file, flush/sync and atomic publication. PNG
keeps 16 bits; ICO normalizes RGBA and limits dimensions. RGB ICC is converted
to sRGB preserving alpha and precision; invalid or non-RGB profiles are
refused. PNG/JPEG receive an sRGB profile generated without identifying fields
from the source. Personal metadata is not copied during conversion.

Folder discovery, properties, dialog loading, comparison and preparing the
print pixels were moved off the GTK thread. Previews have one coalescing
worker, bounded buffers, and only the current revision is presented. The strip
is paged at up to 41 widgets. Weak references and signal disconnection reduce
retention; tests check the release. Printing uses premultiplied alpha, with a
specific regression test; no job was sent to a real printer.

The AI tests of this campaign used ONNX Runtime 1.29.0 with models downloaded
on demand; that engine was replaced, and the AI numbers in the table above do
not apply to the current product. The AI uses BigLinux's image-engine-native
engines (pure Rust, CPU or Vulkan) and nothing is downloaded: each tool reads
its weights from its optional package (`birefnet-native`, `realesrgan-native`,
`scunet-native`, `lama-native`) in `/usr/share/<package>/`. The loaded model is
reused across the files of a batch, the mask preserves the original alpha, and
cancellation is honored between stages, not in the middle of one.

Before loading and before each run, each model estimates the memory it needs
(0.1 to 0.7 GB measured in the largest cases, plus the image's working area)
and compares it with the smallest of MemAvailable, the cgroup v2 limits of the
ancestors and RLIMIT_AS, without adding swap. This is preventive admission, not
a hard RSS quota: availability can change after the check and cgroup v1 is not
covered. Viewing and conversion stay available without any weight package.
Measuring the RAM, time and quality of the native engines on modest hardware
remains pending.

Zoom lives in `gui/zoom.rs`. The wheel, shortcuts and buttons use the cursor
point; without a valid position, they use the center. The original regression
test checked only the position after the transition ended. A new test in
`after-paint` reproduced two frames with a transient offset of about 185 image
pixels on the horizontal axis. The position and scroll limits are set before
the new scale; the transition uses the GTK frame clock and gradual
interpolation on a logarithmic scale. Fractional touchpad events keep their
magnitude. The `gtk-enable-animations=false` preference applies zoom
immediately.

Quick commands update the target of the same animation, keeping the anchor
when the cursor stays in place. Dragging, fitting to the window, navigating and
closing cancel the animation. Zoom references the existing texture, without
resizing pixels on the GUI thread. `1` corresponds to 100% and `0` restores
fit to window. When the image fits in the window or reaches an edge, scrolling
is limited.

The graphical test checks each painted frame against the geometric projection
with those limits, with a tolerance below 1.5 screen pixels per axis,
monotonic scale, the presence of intermediate frames and rendered pixels at the
target. It includes keyboard, buttons, fast zoom-ins, reversal, 100%, disabled
animations, manual drag, navigation and closing. This check does not certify
personal visual comfort or the absence of dropped frames on all hardware;
evaluation with a real mouse and touchpad remains in the acceptance script.

The icon SVG also got a header fix: the long comment was moved inside the root
element. The drawing stayed the same, and the file became recognized by the
GdkPixbuf loader used in GNOME.

Package validation also revealed that a Nautilus extension already installed on
the host made the generator leave it out of the DESTDIR. The scope is explicit,
without inferring a personal installation from the path prefix. Regression
tests check inclusion in the package despite the existing extension,
preservation of the host's file and prevention of duplication in the personal
installation. The earlier incomplete file was kept in
`round3/package-before-nautilus-fix/`, without distribution approval.

## Pending work for official adoption

| Priority | Pending item | Acceptance |
| --- | --- | --- |
| High | Clean BigCommunity installation | Build of the complete recipe; install, upgrade, remove; real MIME, schemas, languages and Nautilus |
| High | Accessibility and use | Orca, keyboard, focus, high contrast, light/dark, 100/150/200% scales and multiple monitors |
| High | Long-running stability | Eight-hour core result plus a GNOME/slideshow session with CPU/GPU memory monitored |
| High | Native memory | Memcheck in a compatible environment; green remote CI and log review |
| High | Collection and fidelity | Visual/colorimetric comparison of authorized real images, ICC and 16 bits; define a policy for HDR, animation and pages |
| High | AI on modest hardware | Measure RAM, time, mask quality and cancellation of the image-engine-native engines, on the CPU and on a Vulkan GPU |
| Medium | Product performance | Cold first frame, navigation and latency compared with the native viewer on the same hardware and collection |
| Medium | Remaining architectural debt | Extract state, controllers and dialogs from `gui.rs`; widen tests of the interactive flows |
| Medium | Unmaintained dependency | Drop the transitive `paste` through rav1e/ravif, RUSTSEC-2024-0436, with an AVIF regression test |
| Medium | Additional confinement | Evaluate encoders/AI outside the parent, seccomp/cgroup and a library allowlist |

The viewer shows the first frame or page; there is no animation playback,
multi-page TIFF navigation or full HDR handling/tone mapping. The measured
stability and reference release do not prove the absence of native leaks. The
GTK logs recorded a theme warning and an unavailable AT-SPI bus; Orca remains
pending. The package is reassembled from the tested release, not a certified
clean build.

The first [remote CI of PR #1](https://github.com/xathay/bigiris/actions/runs/38161983238)
ended with ten jobs passed and four failures. Fuzzing selected stable because
of `rust-toolchain.toml`, before testing the corpus. The two functional test
jobs and the release self-test were blocked by Bubblewrap's loopback setup on
the Ubuntu runner. The release build itself compiled. The fix sets
`RUSTUP_TOOLCHAIN=nightly` only for fuzzing and loads the Bubblewrap profile
provided by the `apparmor-profiles` package in the three runtime jobs, with a
child profile that denies capabilities. A preflight requires the sandbox and
zero effective capabilities before compiling. AppArmor and the global
restrictions stay active; the Arch container does not get broad privileges.
The new configuration needs a new remote run before approval. The graphical
test already includes the zoom transitions and publishes the painted-frame
report as a CI artifact.

## References and reproduction

[GNOME HIG](https://developer.gnome.org/hig/index.html),
[accessibility](https://developer.gnome.org/hig/guidelines/accessibility.html),
[image::Limits](https://docs.rs/image/latest/image/struct.Limits.html),
[Bubblewrap](https://github.com/containers/bubblewrap),
[RustSec](https://github.com/RustSec/advisory-db),
[Valgrind](https://valgrind.org/docs/manual/manual-core.html) and
[Ubuntu's namespace restrictions](https://documentation.ubuntu.com/release-notes/24.04/#unprivileged-user-namespace-restrictions)
guide the criteria. On Ubuntu runners, check the distribution's Bubblewrap
profile; an unavailable sandbox must fail, without disabling global
protections.

Commands in [CONTRIBUTING.md](../CONTRIBUTING.md#quality-campaign-on-gnome).
Delivery and acceptance script in [RELEASE-CANDIDATE.md](RELEASE-CANDIDATE.md).
