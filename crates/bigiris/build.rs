// SPDX-License-Identifier: GPL-3.0-or-later
//! Derive `BIGIRIS_BUILD_ID`, the identity the GUI hands its codec worker.
//!
//! The crate version never changes between packaged builds (packages use a
//! date-based pkgver), so it cannot tell an old GUI from the upgraded binary
//! it launches as worker. The id mixes the git commit (when building from a
//! checkout) and `SOURCE_DATE_EPOCH` (set by makepkg and other reproducible
//! build tools), so it stays reproducible but changes with every release.
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    output.status.success().then(|| text.trim().to_owned()).filter(|text| !text.is_empty())
}

fn main() {
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    let commit = git(&["rev-parse", "HEAD"]);
    if commit.is_some() {
        // Rebuild the id when HEAD moves (commit, checkout, reset).
        for path in [
            git(&["rev-parse", "--git-path", "HEAD"]),
            git(&["rev-parse", "--git-path", "packed-refs"]),
        ]
        .into_iter()
        .flatten()
        {
            println!("cargo:rerun-if-changed={path}");
        }
        if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
            if let Some(path) = git(&["rev-parse", "--git-path", &reference]) {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
    let epoch = std::env::var("SOURCE_DATE_EPOCH").ok().filter(|epoch| !epoch.is_empty());
    let id = match (commit, epoch) {
        (Some(commit), Some(epoch)) => format!("{commit}.{epoch}"),
        (Some(commit), None) => commit,
        (None, Some(epoch)) => epoch,
        (None, None) => "unknown".to_owned(),
    };
    println!("cargo:rustc-env=BIGIRIS_BUILD_ID={id}");
}
