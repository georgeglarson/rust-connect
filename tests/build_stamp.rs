//! vk #973: the binary knows which commit it was built from, so a lint can
//! compare the installed daemon to origin/main instead of file mtimes.

#[test]
fn test_build_version_carries_a_git_sha() {
    let sha = rust_connect::GIT_SHA;
    assert!(
        sha == "unknown" || (sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit())),
        "GIT_SHA must be a hex sha or the documented fallback; got {sha}"
    );
    assert!(
        rust_connect::BUILD_VERSION.contains(env!("CARGO_PKG_VERSION"))
            && rust_connect::BUILD_VERSION.contains(sha),
        "BUILD_VERSION must carry both; got {}",
        rust_connect::BUILD_VERSION
    );
}
