use std::process::Command;

fn main() {
    // Re-run if git HEAD changes (new commit, checkout, etc.)
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/tags");

    let hash = git_short_hash();
    let tag = git_exact_tag();

    let version = env!("CARGO_PKG_VERSION");
    let full_version = match (tag.as_deref(), hash.as_deref()) {
        (Some(tag), Some(hash)) => format!("{version} ({tag}, {hash})"),
        (None, Some(hash)) => format!("{version} ({hash})"),
        _ => version.to_string(),
    };

    println!("cargo:rustc-env=FULL_VERSION={full_version}");
}

fn git_short_hash() -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn git_exact_tag() -> Option<String> {
    Command::new("git")
        .args(["describe", "--tags", "--exact-match", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}
