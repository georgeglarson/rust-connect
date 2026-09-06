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

/// 2026-09-06 audit C3: the unit set `Environment=RUST_LOG=info`, and
/// `EnvFilter::try_from_default_env` prefers `RUST_LOG` when present, so the
/// documented `log_level` config knob was dead under the unit and live
/// under `cargo run`. The unit must not pin the filter; `RUST_LOG` is the
/// operator's override, via a drop-in.
#[test]
fn test_unit_does_not_pin_rust_log() {
    let unit = read("packaging/rust-connect.service");
    let offending: Vec<&str> = unit
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter(|l| l.trim_start().starts_with("Environment=") && l.contains("RUST_LOG"))
        .collect();
    assert!(
        offending.is_empty(),
        "the unit must not set RUST_LOG (it shadows config `log_level`): {:?}",
        offending
    );
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
