//! X11 clipboard backend integration test (Task 1.6 Backend C, vk #1010):
//! proves `X11Clipboard` reaches a real X11 CLIPBOARD selection in both
//! directions — write via the backend, read back via an independent xclip
//! call; and the reverse, an independent xclip write that the backend's
//! watcher picks up.
//!
//! Spawns a private Xvfb and sets DISPLAY for the WHOLE PROCESS via
//! `std::env::set_var` before any X11 connection exists (edition 2021, so
//! `set_var` is safe). That is why this file is its own test process and
//! MUST stay the only `#[tokio::test]` here — mirrors
//! tests/mpris_bus_recovery.rs's rationale for `DBUS_SESSION_BUS_ADDRESS`.
//!
//! Skips cleanly (passes) when Xvfb or xclip is not on PATH.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rust_connect::plugins::clipboard::{ClipboardBackend, X11Clipboard};

/// Kills the Xvfb child on drop, even if the test panics partway through.
struct XvfbGuard(Option<Child>);

impl Drop for XvfbGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Bounded poll for Xvfb's X11 socket to appear, mirroring
/// tests/mpris_bus_recovery.rs's wait_for_socket.
async fn wait_for_x_socket(display_num: u32) {
    let path = PathBuf::from(format!("/tmp/.X11-unix/X{display_num}"));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if path.exists() {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("Xvfb socket {} never appeared", path.display());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Independent read, NOT going through the backend under test — this is
/// the oracle the write direction is checked against.
fn xclip_read() -> Option<String> {
    let out = Command::new("xclip")
        .args(["-o", "-selection", "clipboard"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .expect("spawn xclip read");
    if !out.status.success() {
        return None;
    }
    let content = String::from_utf8_lossy(&out.stdout).into_owned();
    if content.is_empty() {
        None
    } else {
        Some(content)
    }
}

/// Independent write, NOT going through the backend under test — this
/// drives the reverse direction (external change, backend's watcher picks
/// it up).
fn xclip_write(content: &str) {
    let mut child = Command::new("xclip")
        .args(["-i", "-selection", "clipboard"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn xclip write");
    child
        .stdin
        .take()
        .expect("xclip stdin piped")
        .write_all(content.as_bytes())
        .expect("write to xclip");
    let status = child.wait().expect("wait on xclip write");
    assert!(status.success(), "xclip write failed: {status}");
}

#[tokio::test]
async fn x11_backend_roundtrips_with_independent_xclip() {
    if Command::new("Xvfb").arg("-help").output().is_err()
        || Command::new("xclip").arg("-version").output().is_err()
    {
        eprintln!("Xvfb or xclip not on PATH — skipping");
        return;
    }

    // A display number derived from the pid keeps concurrent test runs
    // from colliding on the same Xvfb socket.
    let display_num = 100 + (std::process::id() % 800);
    let display = format!(":{display_num}");

    let xvfb = XvfbGuard(Some(
        Command::new("Xvfb")
            .arg(&display)
            .args(["-screen", "0", "1024x768x24"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn Xvfb"),
    ));
    wait_for_x_socket(display_num).await;

    // SAFETY: process-wide env mutation, done once, before any X11
    // connection exists anywhere in this process. This file is
    // deliberately the only #[tokio::test] here (see module doc) so
    // nothing else in the process races this var.
    std::env::set_var("DISPLAY", &display);

    let backend =
        X11Clipboard::detect().expect("X11Clipboard::detect must succeed with DISPLAY + xclip");
    assert_eq!(backend.name(), "x11-xclip");

    // 1. Write via the backend; read back via an independent xclip call.
    backend
        .set("hello from rust-connect")
        .expect("backend set must succeed");
    let observed = tokio::task::spawn_blocking(xclip_read)
        .await
        .expect("join xclip_read");
    assert_eq!(observed.as_deref(), Some("hello from rust-connect"));

    // 2. The reverse: an independent xclip write, the backend's watcher
    // (poll fallback here — no clipnotify on this host) picks it up.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    backend.spawn_watcher(tx).expect("spawn_watcher");

    tokio::task::spawn_blocking(|| xclip_write("set externally"))
        .await
        .expect("join xclip_write");

    // The poll watcher's first tick pushes whatever the clipboard already
    // holds ("hello from rust-connect" from step 1, if it beats the
    // xclip_write above) — drain until the expected value arrives instead
    // of asserting on the first message (PR #11 review: the first-recv
    // assert was a timing-dependent flake).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let received = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("watcher must observe the external change within 5s")
            .expect("watcher channel must not close");
        if received == "set externally" {
            break;
        }
        assert_eq!(
            received, "hello from rust-connect",
            "watcher emitted content nothing in this test ever set"
        );
    }

    drop(xvfb);
}
