//! Puts the git commit into `sm --version`. The backup mini compares that string
//! with the one the server advertises to decide whether to update (#2264), so two
//! builds of one commit must print the same version and different commits must not.
use std::{path::PathBuf, process::Command};

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn main() {
    println!("cargo:rerun-if-env-changed=SM_BUILD_SHA");
    // Rerun when HEAD moves. Only existing paths: a missing one reruns every build.
    if let (Some(dir), Some(common)) = (
        git(&["rev-parse", "--git-dir"]),
        git(&["rev-parse", "--git-common-dir"]),
    ) {
        let mut watched = vec![PathBuf::from(&dir).join("HEAD")];
        if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
            watched.push(PathBuf::from(&common).join(reference));
        }
        watched.push(PathBuf::from(&common).join("packed-refs"));
        for path in watched.into_iter().filter(|p| p.exists()) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    let commit = std::env::var("SM_BUILD_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| git(&["rev-parse", "HEAD"]))
        .map(|sha| sha.chars().take(12).collect::<String>())
        .unwrap_or_else(|| "unknown".into());
    println!(
        "cargo:rustc-env=SM_CLI_VERSION={} ({commit})",
        std::env::var("CARGO_PKG_VERSION").unwrap()
    );
}
