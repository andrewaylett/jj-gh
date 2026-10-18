//! `--version` reports the version computed by `build.rs`.

use std::process::Command;

#[test]
fn version_is_semver_from_build_script() {
    let out = Command::new(env!("CARGO_BIN_EXE_jj-gh"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        format!("jj-gh {}", env!("JJ_GH_VERSION")),
        "unexpected --version output"
    );
    // MAJOR.MINOR.PATCH, optionally followed by `+build.metadata`.
    let version = stdout.trim().strip_prefix("jj-gh ").unwrap();
    let core = version.split('+').next().unwrap();
    assert!(
        core.split('.').count() == 3 || core.contains('-'),
        "not semver-shaped: {version}"
    );
}
