// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end tests through the real executable, isolated in temporary folders.
use std::path::Path;
use std::process::{Command, Output};

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bigiris")).current_dir(dir).args(args).output().unwrap()
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
#[test]
fn conversion_transform_and_animation_commands_produce_readable_outputs() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), "imagem com espaço ç.png");
    let file = "imagem com espaço ç.png";
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
        success(run(dir.path(), &args));
    }
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let path = entry.unwrap().path();
        assert!(bigimage_core::open_image(&path).is_ok(), "unreadable output: {}", path.display());
    }
    assert_eq!(
        image::image_dimensions(dir.path().join("imagem com espaço ç_16.png")).unwrap(),
        (16, 12)
    );
    assert_eq!(
        image::image_dimensions(dir.path().join("imagem com espaço ç_rot90.png")).unwrap(),
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
#[test]
fn invalid_arguments_return_failure() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["convert", "--to", "bogus", "missing.png"],
        vec!["resize", "--percent", "NaN", "missing.png"],
        vec!["crop", "--rect", "nonsense", "missing.png"],
        vec!["adjust", "--gamma", "0", "missing.png"],
    ] {
        assert!(!run(dir.path(), &args).status.success(), "{args:?}");
    }
}

#[cfg(feature = "ai")]
#[test]
fn low_memory_refuses_ai_before_download_and_preserves_inputs() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), "source.png");
    let original = std::fs::read(dir.path().join("source.png")).unwrap();
    let cache = dir.path().join("isolated-data");
    let output = Command::new("/usr/bin/prlimit")
        .args(["--as=4294967296", "--core=0", "--"])
        .arg(env!("CARGO_BIN_EXE_bigiris"))
        .args(["remove-bg", "source.png"])
        .env("XDG_DATA_HOME", &cache)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("8 GiB"), "missing memory explanation: {error}");
    assert!(!cache.exists(), "model download was attempted despite rejected budget");
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
    success(run(dir.path(), &["batch", "--profile", "profile.toml", "input"]));
    for (path, bytes) in paths.iter().zip(before) {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert_eq!(bigimage_core::open_image(path).unwrap().width(), 32);
    }
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
    assert!(String::from_utf8_lossy(&help.stdout).contains("falha parcial"));
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
    assert!(!dir.path().join("out/photo.webp").exists());
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
fn remove_bg_skips_existing_output_without_loading_the_model() {
    let dir = tempfile::tempdir().unwrap();
    sized_fixture(dir.path(), "source.png", 8, 8);
    std::fs::write(dir.path().join("source_nobg.png"), b"keep").unwrap();
    let cache = dir.path().join("isolated-data");
    let output = Command::new(env!("CARGO_BIN_EXE_bigiris"))
        .args(["remove-bg", "source.png"])
        .env("XDG_DATA_HOME", &cache)
        .current_dir(dir.path())
        .output()
        .unwrap();
    success(output.clone());
    assert!(String::from_utf8_lossy(&output.stdout).contains("skip "));
    assert_eq!(std::fs::read(dir.path().join("source_nobg.png")).unwrap(), b"keep");
    assert!(!cache.exists(), "model was loaded for a skipped file");
}

#[cfg(not(feature = "ai"))]
#[test]
fn remove_bg_without_ai_is_a_fatal_error() {
    let dir = tempfile::tempdir().unwrap();
    sized_fixture(dir.path(), "a.png", 8, 8);
    let output = run(dir.path(), &["remove-bg", "a.png"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("ai"));
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
name=${file##*/}
case "$FAKE_SOFFICE" in
  write) printf '%%PDF-1.4 fake' > "$outdir/${name%.*}.pdf" ;;
  silent) ;;
  hang) /usr/bin/sleep 30.4242; exit 0 ;;
esac
exit 0
"#;

fn to_pdf(dir: &Path, bin: &Path, mode: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bigiris"))
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

    let replace = ["--overwrite", "replace", "--", "-doc.txt"];
    success(to_pdf(dir.path(), bin.path(), "write", &replace));
    assert!(std::fs::read(&pdf).unwrap().starts_with(b"%PDF"));

    // soffice "succeeds" without producing anything (load failure or a
    // profile owned by another instance): that is a failure, not ok.
    std::fs::write(dir.path().join("other.txt"), b"x").unwrap();
    let output = to_pdf(dir.path(), bin.path(), "silent", &["other.txt"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(!dir.path().join("other.pdf").exists());
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
fn collisions_are_planned_by_content_not_extension() {
    let dir = tempfile::tempdir().unwrap();
    sized_fixture(dir.path(), "a.png", 32, 24);
    // A PNG named .jpg: rotate keeps PNG, so both would write a_rot90.png.
    sized_fixture(dir.path(), "b.png", 16, 12);
    std::fs::rename(dir.path().join("b.png"), dir.path().join("a.jpg")).unwrap();
    let args = ["rotate", "--degrees", "90", "--overwrite", "replace", "a.png", "a.jpg"];
    let output = run(dir.path(), &args);
    assert_eq!(output.status.code(), Some(1), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(!dir.path().join("a_rot90.png").exists());
}
