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
