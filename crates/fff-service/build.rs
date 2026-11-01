use std::path::Path;
use std::process::Command;

// Inputs whose uncommitted changes can change what the service does.
const SOURCES: &[&str] = &["crates", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml"];

// Version, commit and features; a dirty tree adds a hash of its changes, so development
// builds join neither a release's service nor that of other changes.
fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let root = Path::new(&manifest).join("../..");
    let mut features: Vec<String> = std::env::vars()
        .filter_map(|(key, _)| key.strip_prefix("CARGO_FEATURE_").map(str::to_owned))
        .collect();
    features.sort();
    // A source copy inside another repository must not take that repository's commit.
    let commit = git(&root, &["rev-parse", "--show-toplevel"])
        .filter(|top| same_dir(Path::new(top), &root))
        .and_then(|_| git(&root, &["rev-parse", "HEAD"]));
    let mut hasher = blake3::Hasher::new();
    for part in [
        std::env::var("CARGO_PKG_VERSION").unwrap(),
        commit.clone().unwrap_or_else(|| "unknown".into()),
        features.join(","),
    ] {
        hasher.update(part.as_bytes()).update(b"\0");
    }
    if commit.is_some() {
        hash_changes(&root, &mut hasher);
        // What the commit depends on; the index does not count, so staging reruns nothing.
        for name in ["HEAD", "logs/HEAD", "packed-refs"] {
            rerun_on_git_path(&root, name);
        }
        if let Some(branch) = git(&root, &["symbolic-ref", "-q", "HEAD"]) {
            rerun_on_git_path(&root, &branch);
        }
        for source in SOURCES.iter().chain([&".gitignore"]) {
            println!("cargo:rerun-if-changed={}", root.join(source).display());
        }
    }
    println!(
        "cargo:rustc-env=FFF_SERVICE_BUILD_ID={}",
        hasher.finalize().to_hex()
    );
}

// Changed and untracked files by name and content, in name order, so staging a file does not
// change the identity. Hashing contents rather than git's diff output keeps the identity
// independent of git's configuration and abbreviation length.
fn hash_changes(root: &Path, hasher: &mut blake3::Hasher) {
    let mut changed = vec!["diff", "--name-only", "--no-renames", "-z", "HEAD", "--"];
    changed.extend(SOURCES);
    let mut untracked = vec!["ls-files", "--others", "--exclude-standard", "-z", "--"];
    untracked.extend(SOURCES);
    let mut names = std::collections::BTreeSet::new();
    for list in [changed, untracked] {
        if let Some(listed) = git(root, &list) {
            names.extend(
                listed
                    .split('\0')
                    .filter(|n| !n.is_empty())
                    .map(str::to_owned),
            );
        }
    }
    for name in names {
        hasher.update(name.as_bytes()).update(b"\0");
        match std::fs::read(root.join(&name)) {
            Ok(bytes) => hasher
                .update(&(bytes.len() as u64).to_le_bytes())
                .update(&bytes),
            Err(_) => hasher.update(b"deleted"),
        };
    }
}

fn rerun_on_git_path(root: &Path, name: &str) {
    // Cargo reruns on every build for a path that does not exist.
    if let Some(path) = git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    ) && Path::new(&path).exists()
    {
        println!("cargo:rerun-if-changed={path}");
    }
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.trim_end_matches('\n').to_owned())
}

fn same_dir(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}
