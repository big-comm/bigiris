// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end tests through the real executable, isolated in temporary folders.
use std::path::Path;
use std::process::{Command, Output};

/// The executable in the C locale, so messages are the English source text
/// whatever locale the test runner has.
fn bigiris() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bigiris"));
    command.env("LC_ALL", "C");
    command
}
fn run(dir: &Path, args: &[&str]) -> Output {
    bigiris().current_dir(dir).args(args).output().unwrap()
}
fn fixture(dir: &Path, name: &str) {
    image::RgbImage::from_pixel(32, 24, image::Rgb([30, 70, 120])).save(dir.join(name)).unwrap();
}
fn success(output: Output) {
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn self_test_and_help_exit_successfully() {
    let dir = tempfile::tempdir().unwrap();
    success(run(dir.path(), &["--self-test"]));
    success(run(dir.path(), &["--help"]));
}
/// `LC_ALL` value of an installed Brazilian Portuguese UTF-8 locale, if `locale -a` lists one.
fn brazilian_locale() -> Option<&'static str> {
    let listed = Command::new("locale").arg("-a").output().ok()?;
    String::from_utf8_lossy(&listed.stdout)
        .lines()
        .any(|name| name.eq_ignore_ascii_case("pt_BR.utf8") || name == "pt_BR.UTF-8")
        .then_some("pt_BR.UTF-8")
}
#[test]
fn messages_follow_the_user_locale() {
    let Some(brazilian) = brazilian_locale() else {
        // allow-noisy-log: a skipped check says so in the test output.
        eprintln!("skipped: no pt_BR.UTF-8 locale installed (see `locale -a`)");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let locale_dir = dir.path().join("locale");
    let catalog = locale_dir.join("pt_BR/LC_MESSAGES/bigiris.mo");
    std::fs::create_dir_all(catalog.parent().unwrap()).unwrap();
    let source = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/po/pt_BR.po");
    let compiled = Command::new("msgfmt")
        .arg("-o")
        .arg(&catalog)
        .arg(source)
        .status()
        .expect("msgfmt from gettext, a build dependency");
    assert!(compiled.success(), "msgfmt {source}");
    let in_locale = |lc_all: &str, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_bigiris"))
            .current_dir(dir.path())
            .env("BIGIRIS_LOCALE_DIR", &locale_dir)
            .env("LC_ALL", lc_all)
            .env("LANGUAGE", "pt_BR")
            .args(args)
            .output()
            .unwrap()
    };
    let bogus_target = ["convert", "--to", "bogus", "missing.png"];

    let help = String::from_utf8_lossy(&in_locale(brazilian, &["--help"]).stdout).into_owned();
    assert!(help.contains("Visualizador rápido e conversor de imagens para Linux"), "{help}");
    assert!(help.contains("falha parcial"), "{help}");
    let failure = in_locale(brazilian, &bogus_target);
    let error = String::from_utf8_lossy(&failure.stderr);
    assert!(error.contains("formato de destino inválido"), "{error}");

    let help = String::from_utf8_lossy(&in_locale("C", &["--help"]).stdout).into_owned();
    assert!(help.contains("Fast image viewer and converter for Linux"), "{help}");
    let failure = in_locale("C", &bogus_target);
    let error = String::from_utf8_lossy(&failure.stderr);
    assert!(error.contains("invalid target format"), "{error}");
}
#[test]
fn conversion_transform_and_animation_commands_produce_readable_outputs() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), "photo with space ç.png");
    let file = "photo with space ç.png";
    for args in [
        vec!["convert", "--to", "jpeg", file],
        vec!["resize", "--max-edge", "16", file],
        vec!["rotate", "--degrees", "90", file],
        vec!["flip", "--axis", "horizontal", file],
        vec!["crop", "--rect", "12x10+1+2", file],
        vec!["extent", "--size", "40x40", file],
        vec!["adjust", "--brightness", "10", file],
        vec!["animate", "-o", "animation.gif", file, file],
    ] {
        let output = run(dir.path(), &args);
        // The report alone: no diagnostics at the default log level.
        let report = [&output.stdout[..], &output.stderr[..]].concat();
        assert!(!String::from_utf8_lossy(&report).contains("INFO"), "{args:?}");
        success(output);
    }
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let path = entry.unwrap().path();
        assert!(bigimage_core::open_image(&path).is_ok(), "unreadable output: {}", path.display());
    }
    assert_eq!(
        image::image_dimensions(dir.path().join("photo with space ç_16.png")).unwrap(),
        (16, 12)
    );
    assert_eq!(
        image::image_dimensions(dir.path().join("photo with space ç_rot90.png")).unwrap(),
        (24, 32)
    );
}
#[test]
fn corrupt_input_returns_failure_and_preserves_existing_output() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bad.png"), b"not an image").unwrap();
    std::fs::write(dir.path().join("bad.jpg"), b"existing photo").unwrap();
    let output = run(dir.path(), &["convert", "--to", "jpeg", "--overwrite", "replace", "bad.png"]);
    assert!(!output.status.success());
    assert_eq!(std::fs::read(dir.path().join("bad.jpg")).unwrap(), b"existing photo");
}
/// Values outside the documented ranges are usage errors (exit 1) before
/// any file is read, never a 1-pixel output or a per-file failure.
#[test]
fn invalid_arguments_return_failure() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), "a.png");
    for args in [
        vec!["convert", "--to", "bogus", "a.png"],
        vec!["resize", "--percent", "NaN", "a.png"],
        vec!["resize", "--percent", "0", "a.png"],
        vec!["resize", "--percent", "1000.5", "a.png"],
        vec!["resize", "--max-edge", "0", "a.png"],
        vec!["resize", "--exact", "0x5", "a.png"],
        vec!["extent", "--size", "0x0", "a.png"],
        vec!["crop", "--rect", "nonsense", "a.png"],
        vec!["crop", "--rect", "0x0+0+0", "a.png"],
        vec!["adjust", "--gamma", "0", "a.png"],
        vec!["adjust", "--brightness", "500", "a.png"],
        vec!["adjust", "--contrast=-101", "a.png"],
        vec!["adjust", "--saturation", "inf", "a.png"],
        vec!["animate", "--speed", "99", "-o", "a.gif", "a.png"],
    ] {
        assert_eq!(run(dir.path(), &args).status.code(), Some(1), "{args:?}");
    }
    let names: Vec<_> =
        std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names, ["a.png"]);
}

/// File-manager menus pass `--notify`: a failure reaches the desktop through
/// notify-send, here a stub on PATH that records its arguments.
#[test]
fn notify_reports_failures_only_when_asked() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let record = dir.path().join("notified");
    let stub = bin.join("notify-send");
    std::fs::write(&stub, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", record.display()))
        .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let failing = |notify: bool| {
        let mut command = bigiris();
        command.current_dir(dir.path()).env("PATH", &path);
        if notify {
            command.arg("--notify");
        }
        command.args(["convert", "--to", "bogus", "missing.png"]).output().unwrap()
    };

    assert!(!failing(false).status.success());
    assert!(!record.exists(), "notified without --notify");
    assert!(!failing(true).status.success());
    let shown = std::fs::read_to_string(&record).expect("no notification with --notify");
    assert!(shown.contains("BigIris: the operation failed"), "{shown}");
    assert!(shown.contains("bogus"), "the notification should name the error: {shown}");
}

/// A readable file that is not a weights file: availability passes, a
/// load would fail. Commands that must not load the model stay green.
#[cfg(feature = "ai")]
fn fake_weights(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("fake.weights");
    std::fs::write(&path, b"not a weights file").unwrap();
    path
}

#[cfg(feature = "ai")]
#[test]
fn low_memory_refuses_ai_before_loading_and_preserves_inputs() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), "source.png");
    let original = std::fs::read(dir.path().join("source.png")).unwrap();
    let output = Command::new("/usr/bin/prlimit")
        .args(["--as=536870912", "--core=0", "--"])
        .arg(env!("CARGO_BIN_EXE_bigiris"))
        .args(["remove-bg", "source.png"])
        .env("LC_ALL", "C")
        .env("BIGIRIS_BIREFNET_WEIGHTS", fake_weights(dir.path()))
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    // Refused by the budget, not by the (fake) weights.
    assert!(error.contains("of free RAM") && error.contains("GiB"), "{error}");
    assert!(!dir.path().join("source_nobg.png").exists());
    assert_eq!(std::fs::read(dir.path().join("source.png")).unwrap(), original);
}
#[test]
fn batch_profile_converts_folder_and_skip_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("input")).unwrap();
    for i in 0..40 {
        fixture(&dir.path().join("input"), &format!("image{i}.png"));
    }
    std::fs::write(
        dir.path().join("profile.toml"),
        "format = 'webp'\noverwrite = 'skip'\noutput_dir = 'output'\n",
    )
    .unwrap();
    success(run(dir.path(), &["batch", "--profile", "profile.toml", "input"]));
    let paths: Vec<_> =
        std::fs::read_dir(dir.path().join("output")).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(paths.len(), 40);
    let before: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let output = run(dir.path(), &["batch", "--profile", "profile.toml", "input"]);
    success(output.clone());
    // Reported like every command: skip lines name the input, the tally is on stderr.
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(report.contains("skip input/image0.png (already exists: "), "{report}");
    let tally = String::from_utf8_lossy(&output.stderr);
    assert!(tally.contains("batch: 0 written, 40 skipped, 0 failed"), "{tally}");
    for (path, bytes) in paths.iter().zip(before) {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert_eq!(bigimage_core::open_image(path).unwrap().width(), 32);
    }
}

/// Outputs written beside their sources are not inputs of the next run:
/// a rerun skips, and `increment` adds one file per run, never doubling.
#[test]
fn batch_reruns_beside_sources_stay_stable() {
    if !sandbox_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("f");
    std::fs::create_dir(&folder).unwrap();
    fixture(&folder, "x.png");
    std::fs::write(dir.path().join("skip.toml"), "format = 'webp'\n").unwrap();
    std::fs::write(dir.path().join("inc.toml"), "format = 'webp'\noverwrite = 'increment'\n")
        .unwrap();
    let count = || std::fs::read_dir(&folder).unwrap().count();
    success(run(dir.path(), &["batch", "--profile", "skip.toml", "f"]));
    let output = run(dir.path(), &["batch", "--profile", "skip.toml", "f"]);
    success(output.clone());
    assert!(String::from_utf8_lossy(&output.stdout).contains("skip f/x.webp"));
    assert_eq!(count(), 2);
    for expected in [3, 4] {
        success(run(dir.path(), &["batch", "--profile", "inc.toml", "f"]));
        assert_eq!(count(), expected);
    }
    assert!(folder.join("x_2.webp").exists());
}

fn sized_fixture(dir: &Path, name: &str, width: u32, height: u32) {
    image::RgbImage::from_pixel(width, height, image::Rgb([200, 40, 10]))
        .save(dir.join(name))
        .unwrap();
}

/// Packaging containers may forbid the namespaces the codec sandbox needs;
/// `scripts/package-check.sh` filters sandbox tests there. Mirror that rule
/// for newer tests: skip only inside such a container, fail everywhere else.
fn sandbox_available() -> bool {
    let dir = tempfile::tempdir().unwrap();
    if run(dir.path(), &["--self-test"]).status.success() {
        return true;
    }
    let in_container =
        Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists();
    assert!(in_container, "codec sandbox unavailable outside a container");
    eprintln!("skipping: container forbids the codec sandbox");
    false
}

fn leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".bigiris-"))
        .collect()
}

#[test]
fn exit_codes_follow_one_convention() {
    let dir = tempfile::tempdir().unwrap();
    // 1: usage errors (clap would otherwise use 2) and fatal setup errors.
    assert_eq!(run(dir.path(), &["convert", "missing.png"]).status.code(), Some(1));
    assert_eq!(run(dir.path(), &["--no-such-flag"]).status.code(), Some(1));
    assert_eq!(run(dir.path(), &["convert", "--to", "bogus", "x.png"]).status.code(), Some(1));
    std::fs::write(dir.path().join("bad.toml"), "format = 'nope'\n").unwrap();
    assert_eq!(
        run(dir.path(), &["batch", "--profile", "bad.toml", "x.png"]).status.code(),
        Some(1)
    );
    // 2: the command ran, but inputs failed.
    assert_eq!(run(dir.path(), &["convert", "--to", "png", "missing.png"]).status.code(), Some(2));
    std::fs::write(dir.path().join("ok.toml"), "format = 'png'\n").unwrap();
    assert_eq!(
        run(dir.path(), &["batch", "--profile", "ok.toml", "missing.png"]).status.code(),
        Some(2)
    );
    assert_eq!(
        run(dir.path(), &["animate", "-o", "out.gif", "missing.png"]).status.code(),
        Some(2)
    );
    // 0: help, and the documented convention is part of it.
    let help = run(dir.path(), &["--help"]);
    assert_eq!(help.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&help.stdout).contains("partial failure"));
}

#[test]
fn fatal_errors_are_one_plain_line() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(dir.path(), &["convert", "--to", "bogus", "x.png"]);
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.starts_with("bigiris: invalid target format"), "{error}");
    assert_eq!(error.lines().count(), 1, "{error}");
    assert!(!error.contains('\x1b') && !error.contains("Location"), "{error}");
}

/// The worker's refusal reaches the parent's sentence without the report
/// decoration of a Rust `main` (location, source path, backtrace hint).
#[test]
fn worker_refusals_carry_no_report_decoration() {
    if !sandbox_available() {
        return;
    }
    use image::codecs::gif::GifEncoder;
    let dir = tempfile::tempdir().unwrap();
    let file = std::fs::File::create(dir.path().join("a.gif")).unwrap();
    let frame =
        || image::Frame::new(image::RgbaImage::from_pixel(4, 4, image::Rgba([9, 9, 9, 255])));
    GifEncoder::new(file).encode_frames([frame(), frame()]).unwrap();
    let output = run(dir.path(), &["flip", "--axis", "horizontal", "a.gif"]);
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("animated"), "{error}");
    assert!(!error.contains("Location") && !error.contains(".rs:"), "{error}");
    assert!(!error.contains("RUST_BACKTRACE") && !error.contains('\x1b'), "{error}");
}

/// A file name from a downloaded archive cannot retitle the terminal, set
/// its clipboard (OSC 52) or fake report lines.
#[test]
fn hostile_file_names_print_inert() {
    let dir = tempfile::tempdir().unwrap();
    let stem = "b\x1b]0;pwned\x07\nok  fake";
    fixture(dir.path(), &format!("{stem}.png"));
    std::fs::write(dir.path().join(format!("{stem}.jpg")), b"existing").unwrap();
    let output = run(dir.path(), &["convert", "--to", "jpeg", &format!("{stem}.png")]);
    success(output.clone());
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(report.starts_with("skip "), "{report}");
    assert_eq!(report.lines().count(), 1, "{report}");
    assert!(!report.contains(['\x1b', '\x07']), "{report:?}");
}

#[test]
fn colliding_outputs_are_refused_before_any_work() {
    let dir = tempfile::tempdir().unwrap();
    sized_fixture(dir.path(), "a.png", 32, 24);
    sized_fixture(dir.path(), "a.jpg", 16, 12);
    std::fs::write(dir.path().join("a.webp"), b"existing").unwrap();
    for policy in ["skip", "replace"] {
        let output =
            run(dir.path(), &["convert", "--to", "webp", "--overwrite", policy, "a.png", "a.jpg"]);
        assert_eq!(output.status.code(), Some(1), "{policy}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("a.png") && error.contains("a.jpg"), "{error}");
    }
    // Same names from different folders into one output folder.
    for sub in ["one", "two"] {
        std::fs::create_dir(dir.path().join(sub)).unwrap();
        sized_fixture(&dir.path().join(sub), "photo.png", 8, 8);
    }
    std::fs::write(dir.path().join("p.toml"), "format = 'webp'\noutput_dir = 'out'\n").unwrap();
    let output = run(dir.path(), &["batch", "--profile", "p.toml", "one", "two"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read(dir.path().join("a.webp")).unwrap(), b"existing");
    // A refused run creates no output folder either.
    assert!(!dir.path().join("out").exists());
}

#[test]
fn increment_resolves_collisions_deterministically() {
    if !sandbox_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    sized_fixture(dir.path(), "a.png", 32, 24);
    sized_fixture(dir.path(), "a.jpg", 16, 12);
    success(run(
        dir.path(),
        &["convert", "--to", "webp", "--overwrite", "increment", "a.png", "./a.png", "a.jpg"],
    ));
    assert_eq!(image::image_dimensions(dir.path().join("a.webp")).unwrap(), (32, 24));
    assert_eq!(image::image_dimensions(dir.path().join("a_1.webp")).unwrap(), (16, 12));
    assert!(!dir.path().join("a_2.webp").exists(), "duplicate spelling processed twice");

    for (sub, size) in [("one", 32), ("two", 16)] {
        std::fs::create_dir(dir.path().join(sub)).unwrap();
        sized_fixture(&dir.path().join(sub), "photo.png", size, size);
    }
    std::fs::write(
        dir.path().join("p.toml"),
        "format = 'png'\noverwrite = 'increment'\noutput_dir = 'out'\n",
    )
    .unwrap();
    success(run(dir.path(), &["batch", "--profile", "p.toml", "two", "one"]));
    let out = dir.path().join("out");
    assert_eq!(image::image_dimensions(out.join("photo.png")).unwrap(), (32, 32));
    assert_eq!(image::image_dimensions(out.join("photo_1.png")).unwrap(), (16, 16));
    assert!(leftovers(&out).is_empty());
}

#[test]
fn remove_bg_validates_model_and_refuses_collisions() {
    let dir = tempfile::tempdir().unwrap();
    sized_fixture(dir.path(), "a.png", 8, 8);
    let output = run(dir.path(), &["remove-bg", "--model", "u2net", "a.png"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("birefnet-lite"));
    sized_fixture(dir.path(), "a.jpg", 8, 8);
    let output = run(dir.path(), &["remove-bg", "a.png", "a.jpg"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(!dir.path().join("a_nobg.png").exists());
}

#[cfg(feature = "ai")]
#[test]
fn ai_commands_skip_existing_outputs_without_loading_the_model() {
    let dir = tempfile::tempdir().unwrap();
    sized_fixture(dir.path(), "source.png", 8, 8);
    std::fs::write(dir.path().join("source_nobg.png"), b"keep").unwrap();
    std::fs::write(dir.path().join("source_denoised.png"), b"keep").unwrap();
    let fake = fake_weights(dir.path());
    for (args, var) in [
        (vec!["remove-bg", "source.png"], "BIGIRIS_BIREFNET_WEIGHTS"),
        (vec!["denoise", "source.png"], "BIGIRIS_SCUNET_WEIGHTS"),
    ] {
        // Loading the fake weights would fail the file: a skip must not.
        let output =
            bigiris().args(&args).env(var, &fake).current_dir(dir.path()).output().unwrap();
        if args[0] == "denoise" && !output.status.success() && !sandbox_available() {
            continue;
        }
        success(output.clone());
        assert!(String::from_utf8_lossy(&output.stdout).contains("skip "), "{args:?}");
    }
    assert_eq!(std::fs::read(dir.path().join("source_nobg.png")).unwrap(), b"keep");
    assert_eq!(std::fs::read(dir.path().join("source_denoised.png")).unwrap(), b"keep");
}

#[cfg(feature = "ai")]
#[test]
fn ai_commands_name_the_missing_package_before_any_work() {
    let dir = tempfile::tempdir().unwrap();
    sized_fixture(dir.path(), "a.png", 16, 16);
    sized_fixture(dir.path(), "mask.png", 16, 16);
    let absent = dir.path().join("absent.weights");
    for (args, var, package) in [
        (vec!["remove-bg", "a.png"], "BIGIRIS_BIREFNET_WEIGHTS", "birefnet-native"),
        (vec!["denoise", "a.png"], "BIGIRIS_SCUNET_WEIGHTS", "scunet-native"),
        (vec!["inpaint", "--mask", "mask.png", "a.png"], "BIGIRIS_LAMA_WEIGHTS", "lama-native"),
        (
            vec!["upscale", "--engine", "realesrgan", "a.png"],
            "BIGIRIS_REALESRGAN_WEIGHTS",
            "realesrgan-native",
        ),
    ] {
        let output =
            bigiris().args(&args).env(var, &absent).current_dir(dir.path()).output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(&format!("sudo pacman -S {package}")), "{args:?}: {error}");
    }
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "something was written: {names:?}");
}

#[test]
fn upscale_falls_back_to_lanczos_without_real_esrgan() {
    if !sandbox_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    sized_fixture(dir.path(), "a.png", 16, 12);
    let output = bigiris()
        .args(["upscale", "--factor", "3", "a.png"])
        .env("BIGIRIS_REALESRGAN_WEIGHTS", dir.path().join("absent.weights"))
        .current_dir(dir.path())
        .output()
        .unwrap();
    success(output.clone());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Lanczos3"));
    assert_eq!(image::image_dimensions(dir.path().join("a_300pct.png")).unwrap(), (48, 36));
    // Explicit Lanczos (and its old spelling) never mentions the model.
    let output =
        run(dir.path(), &["upscale", "--engine", "lanczos3", "--overwrite", "replace", "a.png"]);
    success(output.clone());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("realesrgan-native"));
    assert_eq!(image::image_dimensions(dir.path().join("a_200pct.png")).unwrap(), (32, 24));
    let output = run(dir.path(), &["upscale", "--engine", "nope", "a.png"]);
    assert_eq!(output.status.code(), Some(1));
}

#[cfg(feature = "ai")]
#[test]
fn inpaint_refuses_empty_masks_and_reports_size_mismatches() {
    if !sandbox_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let fake = fake_weights(dir.path());
    sized_fixture(dir.path(), "a.png", 16, 16);
    image::GrayImage::new(16, 16).save(dir.path().join("empty.png")).unwrap();
    image::GrayImage::from_pixel(10, 10, image::Luma([255]))
        .save(dir.path().join("small.png"))
        .unwrap();
    let inpaint = |mask: &str| {
        bigiris()
            .args(["inpaint", "--mask", mask, "a.png"])
            .env("BIGIRIS_LAMA_WEIGHTS", &fake)
            .current_dir(dir.path())
            .output()
            .unwrap()
    };
    let output = inpaint("empty.png");
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("marks no area"));
    let output = inpaint("small.png");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("the mask is 10×10"));
    assert!(!dir.path().join("a_inpaint.png").exists());
}

/// Weights of `model` installed, or pointed at by its variable.
#[cfg(feature = "ai")]
fn weights_ready(model: &bigimage_ai::ModelInfo) -> bool {
    let ready = model.availability().is_ready();
    if !ready {
        eprintln!("skipping {}: weights absent (set {})", model.name, model.env);
    }
    ready
}

#[cfg(feature = "ai")]
#[test]
fn inpaint_reencodes_jpeg_at_maximum_target() {
    if !sandbox_available() || !weights_ready(&bigimage_ai::inpaint::MODEL) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let photo = image::RgbImage::from_fn(160, 120, |x, y| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let grain = (state >> 59) as u8;
        image::Rgb([(x as u8).wrapping_add(grain), (y * 2) as u8, 120 + grain])
    });
    let source = dir.path().join("photo.jpg");
    let options = bigimage_core::EncodeOptions {
        target: bigimage_core::QualityTarget::Maximum,
        ..Default::default()
    };
    let policy = bigimage_core::OverwritePolicy::Replace;
    let image = image::DynamicImage::ImageRgb8(photo);
    bigimage_core::save_image(image, &source, bigimage_core::Format::Jpeg, &options, policy)
        .unwrap();
    // Oracle: the same source re-encoded unchanged at the default target.
    let default = bigimage_core::EncodeOptions::default();
    bigimage_core::transform_file(&source, "_same", None, &default, policy, Ok).unwrap();
    let quality =
        |name: &str| bigimage_core::metadata::estimate_jpeg_quality(dir.path().join(name)).unwrap();
    let balanced = quality("photo_same.jpg");
    let mut mask = image::GrayImage::new(160, 120);
    for y in 50..60 {
        for x in 70..80 {
            mask.put_pixel(x, y, image::Luma([255]));
        }
    }
    mask.save(dir.path().join("mask.png")).unwrap();
    success(run(dir.path(), &["inpaint", "--mask", "mask.png", "photo.jpg"]));
    let written = quality("photo_inpaint.jpg");
    assert!(written > balanced, "inpaint wrote quality {written}; balanced {balanced}");
}

#[cfg(feature = "ai")]
#[test]
fn ai_commands_write_their_outputs_with_installed_weights() {
    if !sandbox_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let photo = image::RgbImage::from_fn(64, 48, |x, y| {
        let inside = (x as i32 - 32).pow(2) + (y as i32 - 24).pow(2) < 225;
        if inside { image::Rgb([200, 30, 20]) } else { image::Rgb([230, 230, 225]) }
    });
    photo.save(dir.path().join("photo.png")).unwrap();
    let mut mask = image::GrayImage::new(64, 48);
    for y in 20..28 {
        for x in 4..12 {
            mask.put_pixel(x, y, image::Luma([255]));
        }
    }
    mask.save(dir.path().join("mask.png")).unwrap();
    type Case<'a> = (&'a bigimage_ai::ModelInfo, Vec<&'a str>, &'a str, (u32, u32));
    let cases: [Case; 4] = [
        (
            &bigimage_ai::background::MODEL,
            vec!["remove-bg", "photo.png"],
            "photo_nobg.png",
            (64, 48),
        ),
        (
            &bigimage_ai::upscale::MODEL,
            vec!["upscale", "--engine", "realesrgan", "photo.png"],
            "photo_200pct.png",
            (128, 96),
        ),
        (
            &bigimage_ai::denoise::MODEL,
            vec!["denoise", "photo.png"],
            "photo_denoised.png",
            (64, 48),
        ),
        (
            &bigimage_ai::inpaint::MODEL,
            vec!["inpaint", "--mask", "mask.png", "photo.png"],
            "photo_inpaint.png",
            (64, 48),
        ),
    ];
    for (model, args, output, size) in cases {
        if !weights_ready(model) {
            continue;
        }
        let result = run(dir.path(), &args);
        success(result);
        let written = bigimage_core::open_image(dir.path().join(output)).unwrap();
        assert_eq!((written.width(), written.height()), size, "{output}");
        if output.ends_with("_nobg.png") {
            let rgba = written.to_rgba8();
            assert!(rgba.get_pixel(32, 24)[3] > rgba.get_pixel(1, 1)[3], "no cut-out");
        }
        if output.ends_with("_inpaint.png") {
            // Far from the mask nothing changes.
            assert_eq!(written.to_rgb8().get_pixel(60, 2), photo.get_pixel(60, 2));
        }
    }
}

#[cfg(not(feature = "ai"))]
#[test]
fn ai_commands_without_ai_are_fatal_errors() {
    let dir = tempfile::tempdir().unwrap();
    sized_fixture(dir.path(), "a.png", 8, 8);
    for args in [
        vec!["remove-bg", "a.png"],
        vec!["denoise", "a.png"],
        vec!["inpaint", "--mask", "a.png", "a.png"],
        vec!["upscale", "--engine", "realesrgan", "a.png"],
    ] {
        let output = run(dir.path(), &args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("--features ai"), "{args:?}");
    }
}

/// Stand-in for LibreOffice: checks the arguments bigiris must pass and
/// then behaves as requested by `FAKE_SOFFICE`.
const FAKE_SOFFICE: &str = r#"#!/bin/sh
outdir= file= profile=
while [ $# -gt 0 ]; do
  case "$1" in
    --outdir) outdir=$2; shift ;;
    -env:UserInstallation=file:///*) profile=$1 ;;
    --*) ;;
    *) file=$1 ;;
  esac
  shift
done
case "$file" in /*) ;; *) echo "relative input: $file" >&2; exit 3 ;; esac
[ -n "$profile" ] || exit 4
# A descriptor of bigiris' caller must not reach LibreOffice.
[ -e /proc/$$/fd/3 ] && exit 5
name=${file##*/}
case "$FAKE_SOFFICE" in
  write) printf '%%PDF-1.4 fake' > "$outdir/${name%.*}.pdf" ;;
  empty) : > "$outdir/${name%.*}.pdf" ;;
  silent) ;;
  hang) /usr/bin/sleep 30.4242; exit 0 ;;
esac
exit 0
"#;

fn to_pdf(dir: &Path, bin: &Path, mode: &str, args: &[&str]) -> Output {
    bigiris()
        .arg("to-pdf")
        .args(args)
        .env("PATH", bin)
        .env("FAKE_SOFFICE", mode)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn fake_soffice() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let bin = tempfile::tempdir().unwrap();
    let path = bin.path().join("soffice");
    std::fs::write(&path, FAKE_SOFFICE).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

#[test]
fn to_pdf_without_soffice_is_a_clean_fatal_error() {
    let dir = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("doc.txt"), b"hello").unwrap();
    let output = to_pdf(dir.path(), empty.path(), "write", &["doc.txt"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("soffice"));
    assert!(!dir.path().join("doc.pdf").exists());
}

#[test]
fn to_pdf_writes_next_to_a_symlink_under_its_name() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_soffice();
    std::fs::create_dir(dir.path().join("real")).unwrap();
    std::fs::write(dir.path().join("real/target.txt"), b"hello").unwrap();
    std::os::unix::fs::symlink("real/target.txt", dir.path().join("link.txt")).unwrap();
    success(to_pdf(dir.path(), bin.path(), "write", &["link.txt"]));
    assert!(std::fs::read(dir.path().join("link.pdf")).unwrap().starts_with(b"%PDF"));
    assert!(!dir.path().join("real/target.pdf").exists());
}

#[test]
fn to_pdf_verifies_output_and_honours_overwrite_policy() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_soffice();
    // A leading dash must reach soffice as a path, never as an option.
    std::fs::write(dir.path().join("-doc.txt"), b"hello").unwrap();
    success(to_pdf(dir.path(), bin.path(), "write", &["--", "-doc.txt"]));
    let pdf = dir.path().join("-doc.pdf");
    assert!(std::fs::read(&pdf).unwrap().starts_with(b"%PDF"));

    std::fs::write(&pdf, b"mine").unwrap();
    let output = to_pdf(dir.path(), bin.path(), "write", &["--", "-doc.txt"]);
    success(output.clone());
    assert!(String::from_utf8_lossy(&output.stdout).contains("skip "));
    assert_eq!(std::fs::read(&pdf).unwrap(), b"mine");

    let increment = ["--overwrite", "increment", "--", "-doc.txt"];
    success(to_pdf(dir.path(), bin.path(), "write", &increment));
    assert_eq!(std::fs::read(&pdf).unwrap(), b"mine");
    assert!(dir.path().join("-doc_1.pdf").exists());

    // Replace keeps the replaced file's permission bits, as every core output does.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&pdf, std::fs::Permissions::from_mode(0o640)).unwrap();
    let replace = ["--overwrite", "replace", "--", "-doc.txt"];
    success(to_pdf(dir.path(), bin.path(), "write", &replace));
    assert!(std::fs::read(&pdf).unwrap().starts_with(b"%PDF"));
    assert_eq!(std::fs::metadata(&pdf).unwrap().permissions().mode() & 0o777, 0o640);

    // soffice "succeeds" without producing anything (load failure or a
    // profile owned by another instance): that is a failure, not ok.
    std::fs::write(dir.path().join("other.txt"), b"x").unwrap();
    let output = to_pdf(dir.path(), bin.path(), "silent", &["other.txt"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(!dir.path().join("other.pdf").exists());
    assert!(leftovers(dir.path()).is_empty());
}

/// The refusals that keep a bad conversion from being published: an empty
/// PDF never replaces a good one, a folder never reaches soffice, and the
/// caller's descriptors never leak into it.
#[test]
fn to_pdf_refuses_what_it_cannot_publish() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_soffice();
    std::fs::write(dir.path().join("doc.txt"), b"hello").unwrap();
    std::fs::write(dir.path().join("doc.pdf"), b"good").unwrap();
    let replace = ["--overwrite", "replace", "doc.txt"];
    assert_eq!(to_pdf(dir.path(), bin.path(), "empty", &replace).status.code(), Some(2));
    assert_eq!(std::fs::read(dir.path().join("doc.pdf")).unwrap(), b"good");

    std::fs::create_dir(dir.path().join("folder.txt")).unwrap();
    assert_eq!(to_pdf(dir.path(), bin.path(), "write", &["folder.txt"]).status.code(), Some(2));
    assert!(!dir.path().join("folder.pdf").exists());

    std::fs::write(dir.path().join("other.txt"), b"x").unwrap();
    let inherited = Command::new("/bin/sh")
        .args(["-c", "exec 3</dev/null; exec \"$0\" to-pdf other.txt"])
        .arg(env!("CARGO_BIN_EXE_bigiris"))
        .env("LC_ALL", "C")
        .env("PATH", bin.path())
        .env("FAKE_SOFFICE", "write")
        .current_dir(dir.path())
        .output()
        .unwrap();
    success(inherited);
    assert!(dir.path().join("other.pdf").exists());
    assert!(leftovers(dir.path()).is_empty());
}

fn process_running(marker: &str) -> bool {
    std::fs::read_dir("/proc").unwrap().flatten().any(|entry| {
        std::fs::read(entry.path().join("cmdline"))
            .is_ok_and(|cmdline| String::from_utf8_lossy(&cmdline).contains(marker))
    })
}

#[test]
fn to_pdf_timeout_kills_the_whole_soffice_tree() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_soffice();
    std::fs::write(dir.path().join("slow.txt"), b"x").unwrap();
    let started = std::time::Instant::now();
    let output = to_pdf(dir.path(), bin.path(), "hang", &["--timeout", "1", "slow.txt"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(started.elapsed() < std::time::Duration::from_secs(15));
    assert!(!dir.path().join("slow.pdf").exists());
    assert!(leftovers(dir.path()).is_empty());
    assert!(!process_running("30.4242"), "soffice child survived the timeout");
}

#[test]
fn animate_keeps_an_existing_output_unless_asked() {
    // The replace/increment runs decode frames through the sandboxed codec.
    if !sandbox_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), "frame.png");
    let gif = dir.path().join("out.gif");
    std::fs::write(&gif, b"existing").unwrap();
    // Default skip: the existing file is left alone and that is not a failure.
    let output = run(dir.path(), &["animate", "-o", "out.gif", "frame.png"]);
    success(output.clone());
    assert!(String::from_utf8_lossy(&output.stdout).contains("skip "));
    assert_eq!(std::fs::read(&gif).unwrap(), b"existing");
    // A frame named as the output is not turned into a GIF by default either.
    success(run(dir.path(), &["animate", "-o", "frame.png", "frame.png"]));
    assert!(image::open(dir.path().join("frame.png")).is_ok());
    let increment = ["animate", "--overwrite", "increment", "-o", "out.gif", "frame.png"];
    success(run(dir.path(), &increment));
    assert!(image::open(dir.path().join("out_1.gif")).is_ok());
    let replace = ["animate", "--overwrite", "replace", "-o", "out.gif", "frame.png"];
    success(run(dir.path(), &replace));
    assert!(image::open(&gif).is_ok());
}

#[test]
fn inherited_descriptors_do_not_break_the_sandboxed_codec() {
    if !sandbox_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), "a.png");
    // A shell (or a GUI with Mesa memfds) hands over a non-CLOEXEC fd 3.
    let output = Command::new("/bin/sh")
        .args(["-c", "exec 3</dev/null; exec \"$0\" rotate --degrees 90 a.png"])
        .arg(env!("CARGO_BIN_EXE_bigiris"))
        .current_dir(dir.path())
        .output()
        .unwrap();
    success(output);
    assert_eq!(image::image_dimensions(dir.path().join("a_rot90.png")).unwrap(), (24, 32));
}

#[test]
fn extent_aspect_names_outputs_per_image() {
    if !sandbox_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    sized_fixture(dir.path(), "a.png", 32, 24);
    sized_fixture(dir.path(), "a.jpg", 16, 12);
    // Different canvases, different names: no false collision refusal.
    let args = ["extent", "--aspect", "1:1", "--to", "png", "a.png", "a.jpg"];
    success(run(dir.path(), &args));
    assert!(dir.path().join("a_extent-32x32.png").exists());
    assert!(dir.path().join("a_extent-16x16.png").exists());
    // Same canvas under replace: the second input is reported, not silent.
    sized_fixture(dir.path(), "b.png", 16, 12);
    sized_fixture(dir.path(), "b.jpg", 16, 12);
    let args = ["extent", "--aspect", "1:1", "--to", "png", "--overwrite", "replace"];
    let output = run(dir.path(), &[&args[..], &["b.png", "b.jpg"]].concat());
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("b.jpg"));
}

#[test]
fn collisions_are_planned_by_content_not_extension() {
    let dir = tempfile::tempdir().unwrap();
    sized_fixture(dir.path(), "a.png", 32, 24);
    // A PNG named .jpg: rotate keeps PNG, so both would write a_rot90.png.
    image::RgbImage::new(16, 12)
        .save_with_format(dir.path().join("a.jpg"), image::ImageFormat::Png)
        .unwrap();
    let args = ["rotate", "--degrees", "90", "--overwrite", "replace", "a.png", "a.jpg"];
    let output = run(dir.path(), &args);
    assert_eq!(output.status.code(), Some(1), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(!dir.path().join("a_rot90.png").exists());
}

#[test]
fn quality_target_is_accepted_in_both_languages_and_validated() {
    // The conversions decode through the sandboxed codec.
    if !sandbox_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let photo = image::RgbImage::from_fn(96, 64, |x, y| {
        image::Rgb([(x * 2 + y) as u8, (y * 3) as u8, ((x * y) % 251) as u8])
    });
    photo.save(dir.path().join("photo.png")).unwrap();
    let mut sizes = Vec::new();
    for target in ["maxima", "equilibrado", "menor", "max", "balanced", "small"] {
        let out = format!("out-{target}");
        std::fs::create_dir(dir.path().join(&out)).unwrap();
        std::fs::copy(dir.path().join("photo.png"), dir.path().join(&out).join("photo.png"))
            .unwrap();
        let input = format!("{out}/photo.png");
        success(run(dir.path(), &["convert", "--to", "webp", "--target", target, &input]));
        sizes.push(std::fs::metadata(dir.path().join(&out).join("photo.webp")).unwrap().len());
    }
    // Portuguese and English spellings are the same modes; smaller targets
    // never produce larger files.
    assert_eq!(sizes[..3], sizes[3..]);
    assert!(sizes[0] >= sizes[1] && sizes[1] >= sizes[2], "{sizes:?}");
    let invalid = ["convert", "--to", "webp", "--target", "ultra", "photo.png"];
    assert_eq!(run(dir.path(), &invalid).status.code(), Some(1));
    std::fs::write(dir.path().join("p.toml"), "format = 'jpg'\ntarget = 'menor'\n").unwrap();
    success(run(dir.path(), &["batch", "--profile", "p.toml", "photo.png"]));
    assert!(dir.path().join("photo.jpg").exists());
    std::fs::write(dir.path().join("bad.toml"), "format = 'jpg'\ntarget = 'ultra'\n").unwrap();
    assert_eq!(
        run(dir.path(), &["batch", "--profile", "bad.toml", "photo.png"]).status.code(),
        Some(1)
    );
}

/// A malformed embedded colour profile does not fail the conversion: the
/// pixels are used as sRGB and the run says so on its file's line.
#[test]
fn broken_profile_converts_with_note() {
    if !sandbox_available() {
        return;
    }
    use image::ImageEncoder;
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = Vec::new();
    let mut encoder = image::codecs::png::PngEncoder::new(&mut bytes);
    encoder.set_icc_profile(b"not an ICC profile".to_vec()).unwrap();
    let pixels = image::RgbImage::from_pixel(8, 6, image::Rgb([10, 128, 250]));
    encoder.write_image(pixels.as_raw(), 8, 6, image::ExtendedColorType::Rgb8).unwrap();
    std::fs::write(dir.path().join("broken.png"), bytes).unwrap();

    let output =
        run(dir.path(), &["convert", "--to", "png", "--overwrite", "increment", "broken.png"]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("info broken.png: the color profile is corrupted"), "{stderr}");
    let plain = dir.path().join("plain.png");
    pixels.save(&plain).unwrap();
    let output = run(dir.path(), &["convert", "--to", "jpg", "plain.png"]);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("color profile"));
}

/// Only a package may write the system-wide menus, which it then owns.
#[test]
fn system_install_needs_destdir() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(dir.path(), &["install-integrations", "--system"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--destdir"));

    let package = dir.path().join("pkg");
    let destdir = format!("--destdir={}", package.display());
    success(run(dir.path(), &["install-integrations", "--system", &destdir]));
    assert!(package.join("usr/share/kio/servicemenus/bigiris-view.desktop").is_file());
}

/// The menu's "Remove all metadata" runs this: a numbered copy in the same
/// format, without the source's EXIF, next to the untouched original.
#[test]
fn strip_metadata_writes_clean_copy() {
    if !sandbox_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut jpeg = Vec::new();
    let pixels = image::RgbImage::from_pixel(32, 24, image::Rgb([200, 40, 40]));
    pixels.write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg).unwrap();
    // APP1 Exif after SOI: little-endian TIFF header and one empty IFD.
    let tiff = b"Exif\0\0II*\0\x08\0\0\0\0\0\0\0\0\0";
    let length = u16::try_from(tiff.len() + 2).unwrap().to_be_bytes();
    let mut tagged = jpeg[..2].to_vec();
    tagged.extend_from_slice(&[0xFF, 0xE1, length[0], length[1]]);
    tagged.extend_from_slice(tiff);
    tagged.extend_from_slice(&jpeg[2..]);
    std::fs::write(dir.path().join("photo.jpg"), &tagged).unwrap();

    success(run(dir.path(), &["strip-metadata", "--overwrite", "increment", "photo.jpg"]));
    assert_eq!(std::fs::read(dir.path().join("photo.jpg")).unwrap(), tagged);
    let clean = std::fs::read(dir.path().join("photo_1.jpg")).unwrap();
    assert!(clean.starts_with(&[0xFF, 0xD8]));
    assert!(!clean.windows(6).any(|w| w == b"Exif\0\0"), "EXIF kept");
}
