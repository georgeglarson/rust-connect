//! Packaging lint.
//!
//! rust-connect is a `systemd --user` unit. A `systemctl --global enable`
//! in the deb's postinst enables it for EVERY user manager on the host,
//! and that includes display-manager greeter users: on Fedora the
//! `gdm-greeter` user (tmpfs home) started its own daemon at every boot,
//! minted a fresh identity each time, dialed the paired phones, and held
//! port 1716 so the real user's daemon crash-looped until login
//! (2026-09-02 audit, laptop: 41 restarts in one boot). Enabling stays a
//! per-user act: `systemctl --user enable --now rust-connect.service`.

use std::path::Path;

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {}", path.display(), e))
}

#[test]
fn test_deb_postinst_never_enables_the_unit_for_every_user() {
    let postinst = read("packaging/deb/DEBIAN/postinst");
    let offending: Vec<&str> = postinst
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter(|l| l.contains("--global") && l.contains("enable"))
        .collect();
    assert!(
        offending.is_empty(),
        "postinst must not `systemctl --global enable` the user unit (greeter users would run it): {:?}",
        offending
    );
}
