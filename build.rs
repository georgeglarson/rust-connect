//! Stamp the build with its git sha (vk #973). The installed daemon trailed
//! main for six days once and three days again in the same month; each
//! time it was caught by comparing file mtimes. `rust-connect --version`
//! and `GET /api/v1/health` report `RC_GIT_SHA` so a lint can compare the
//! running binary to `origin/main`. Falls back to "unknown" outside a git
//! checkout (a source tarball, a vendored build).

use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    println!("cargo:rustc-env=RC_GIT_SHA={sha}");
    println!(
        "cargo:rustc-env=RC_GIT_DIRTY={}",
        if dirty { "1" } else { "0" }
    );
    // Re-stamp when HEAD moves (branch switch, commit, rebase).
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Ok(head) = std::fs::read_to_string(".git/HEAD") {
        if let Some(reference) = head.strip_prefix("ref: ") {
            println!("cargo:rerun-if-changed=.git/{}", reference.trim());
        }
    }
}
