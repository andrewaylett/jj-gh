//! Exposes the version shown by `--version` as `JJ_GH_VERSION`.
//!
//! Releases patch `Cargo.toml` to match the tag (see the release workflow),
//! so for those builds this is just the package version. Everywhere else the
//! nearest `v*` tag is used, plus `git describe` detail as semver build
//! metadata, e.g. `0.3.0+4.gabc1234.dirty`.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        // Don't take the index lock (or write the index) just to look.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn describe() -> Option<String> {
    // A source tarball unpacked inside someone else's checkout isn't ours.
    if !git(&["rev-parse", "--show-prefix"])?.is_empty() {
        return None;
    }
    let described = git(&[
        "describe", "--tags", "--long", "--always", "--match", "v[0-9]*",
    ])?;

    // `Cargo.toml` and `Cargo.lock` are rewritten in release builds, so they
    // must not make an exact tag look dirty.
    let dirty = git(&[
        "status",
        "--porcelain",
        "--untracked-files=no",
        "--",
        ".",
        ":(exclude)Cargo.toml",
        ":(exclude)Cargo.lock",
    ])
    .is_some_and(|s| !s.is_empty());

    let mut parts = described.rsplitn(3, '-');
    let (base, mut meta) = match (parts.next(), parts.next(), parts.next()) {
        // v1.2.3-4-gabc1234
        (Some(hash), Some(count), Some(tag)) => {
            let base = tag.strip_prefix('v').unwrap_or(tag).to_owned();
            let meta = if count == "0" {
                vec![]
            } else {
                vec![count.to_owned(), hash.to_owned()]
            };
            (base, meta)
        }
        // No tags at all: a bare abbreviated hash.
        _ => (
            std::env::var("CARGO_PKG_VERSION").ok()?,
            vec![format!("g{described}")],
        ),
    };
    if dirty {
        meta.push("dirty".to_owned());
    }

    Some(if meta.is_empty() {
        base
    } else {
        format!("{base}+{}", meta.join("."))
    })
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Re-run when HEAD moves or tags change. Paths that don't exist would
    // force a re-run every build, so only name the ones that do.
    for path in ["HEAD", "logs/HEAD", "packed-refs", "refs/tags"] {
        if let Some(p) = git(&["rev-parse", "--git-path", path])
            && std::path::Path::new(&p).exists()
        {
            println!("cargo:rerun-if-changed={p}");
        }
    }

    let version =
        describe().unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").expect("set by cargo"));
    println!("cargo:rustc-env=JJ_GH_VERSION={version}");
}
