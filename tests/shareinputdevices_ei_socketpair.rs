//! Integration tests for the M3 EI transport — fake EI peer over a
//! `UnixStream::pair()` so we exercise the real `handshake_tokio` +
//! `EiConvertEventStream` + dispatch path end-to-end without a portal.
//!
//! **Why a socketpair (not a tokio mock).** `EiConvertEventStream` is
//! `!Send`, so any test that drives it must run inside a single-thread
//! executor. `UnixStream::pair()` plus reis's own `request::Connection`
//! high-level wrappers play the EIS role directly — the same bytes an
//! emulating portal would emit. That gives us a real handshake, real
//! seat binding, real keymap fd delivery, and real input events without
//! mocking the wire.
//!
//! **Why these tests are red-before-green.** Each test wires up the
//! receiver, drives a sequence through the fake peer, and asserts the
//! exact `WireBody` value the receiver emits on its mpsc. They fail
//! at compile time when the public API drifts (which is the point —
//! the M1/M2/M3 boundaries are contractual, not just internal).

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::os::fd::{AsFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use reis::eis::device::DeviceType;
use reis::eis::keyboard::{KeyState as EisKeyState, KeymapType as EisKeymapType};
use reis::eis::{self};
use reis::event::DeviceCapability;
use reis::handshake::EisHandshaker;
use reis::request::{Connection as EisConnection, Device as EisDevice};
use tokio::time::timeout;

use rust_connect::plugins::shareinputdevices::ei::EiReceiver;

/// Helper: build a current-thread runtime + LocalSet, then run a
/// test future on it. Each test embeds this directly because the
/// macro form interferes with the closure body.
#[allow(dead_code)]
fn make_local_set() -> (tokio::runtime::Runtime, tokio::task::LocalSet) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    (rt, local)
}

/// Minimal valid XKB keymap. KEY_H is evdev 35 → xkbcommon keycode
/// 43 (the evdev +8 offset documented at ei.rs:73). We bind the
/// "HKTG" semantic at keycode 43 so the test's KEY_H lookup hits
/// XK_h. The keymap doesn't have to be production-quality — it just
/// has to parse, and one key needs to be addressable.
const TEST_KEYMAP: &str = r#"xkb_keymap {
xkb_keycodes {
	minimum = 8;
	maximum = 255;
	<ESC> = 9;
	<AE01> = 10;
	<AE02> = 11;
	<BKSP> = 22;
	<HKTG> = 43;
	<HOME> = 110;
	<UP> = 111;
	<RIGHT> = 114;
	<END> = 115;
	<DOWN> = 116;
	<CAPS> = 66;
};
xkb_types {
	type "ONE_LEVEL" {
		modifiers= none;
		level_name[1]= "Any";
	};
};
xkb_compat {
	interpret Any+Any { action= NoAction(); };
};
xkb_symbols {
	key <ESC> {	[ Escape	]	};
	key <AE01> {	[ 1	]	};
	key <AE02> {	[ 2	]	};
	key <BKSP> {	[ BackSpace	]	};
	key <HOME> {	[ Home	]	};
	key <UP> {	[ Up	]	};
	key <RIGHT> {	[ Right	]	};
	key <END> {	[ End	]	};
	key <DOWN> {	[ Down	]	};
	key <CAPS> {	[ Caps_Lock	]	};
	key <HKTG> {	[ h	]	};
};
};
"#;

/// Sets up an `EisConnection` (high-level wrapper) and the
/// `EiReceiver` end of the socketpair. Returns the wrapped
/// connection and the receiver's wire mpsc + drive future.
///
/// **Cooperative handshake.** Both sides are non-blocking. We must
/// drive the EIS read loop CONCURRENTLY with the receiver's async
/// handshake — otherwise the two deadlock on each other's responses.
/// We do this by spawning the EIS read loop as `spawn_local` inside
/// the same LocalSet as the test body.
///
/// **Caller contract:** this must be invoked from within a
/// `tokio::task::LocalSet` (the `each_test_local_set` macro below
/// sets one up). The test body, the eis drive task, and the
/// `receiver.start()` future all run on the same thread.
async fn setup() -> (
    EisConnection,
    std::sync::Arc<EiReceiver>,
    tokio::sync::mpsc::UnboundedReceiver<rust_connect::plugins::shareinputdevices::ei::WireBody>,
    tokio::sync::watch::Receiver<bool>,
    impl std::future::Future<Output = ()>,
    Option<tokio::sync::oneshot::Sender<()>>,
) {
    let (peer_stream, client_stream) = UnixStream::pair().expect("UnixStream::pair");

    // The eis drive task owns one end of the socketpair and the
    // EIS handshake state. The receiver is created and started on
    // OUR LocalSet task — they make progress cooperatively.
    //
    // Order matters: spawn the eis drive FIRST so it can start
    // sending the initial handshake_version request; then build
    // the receiver (which writes its HELLO + finish on the other
    // end); then `await` both — the eis drive sends `connection`
    // through `conn_tx` and the receiver returns from `start()`
    // once it sees the `Connection` event.
    let (conn_tx, conn_rx) = tokio::sync::oneshot::channel();
    let (eis_done_tx, eis_done_rx) = tokio::sync::oneshot::channel::<()>();
    let eis_ctx = eis::Context::new(peer_stream).expect("eis Context::new");
    let handshaker = std::sync::Arc::new(std::sync::Mutex::new(EisHandshaker::new(&eis_ctx, 1)));
    let eis_drive = {
        let handshaker = handshaker.clone();
        let eis_ctx = eis_ctx.clone();
        async move {
            let resp = loop {
                let _ = eis_ctx.read();
                let mut got_resp = None;
                while let Some(result) = eis_ctx.pending_request() {
                    let request = match result {
                        reis::PendingRequestResult::Request(r) => r,
                        reis::PendingRequestResult::ParseError(e) => panic!("parse error: {e}"),
                        reis::PendingRequestResult::InvalidObject(id) => {
                            panic!("invalid object: {id}")
                        }
                    };
                    if let Some(r) = handshaker
                        .lock()
                        .unwrap()
                        .handle_request(request)
                        .expect("handshake handle_request")
                    {
                        got_resp = Some(r);
                    }
                }
                if let Some(r) = got_resp {
                    eis_ctx.flush().expect("flush handshake");
                    break r;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            };
            let converter = reis::request::EisRequestConverter::new(&eis_ctx, resp, 1);
            let connection = converter.handle().clone();
            let _ = conn_tx.send(connection);
            // Hold the converter alive until the test signals teardown,
            // then drop it — which closes the eis-side socket and lets
            // the receiver's pump see EOF. (Without this, the converter
            // stays alive forever and the socket never closes.)
            let _ = eis_done_rx.await;
            drop(converter);
        }
    };
    let _eis_handle = tokio::task::spawn_local(eis_drive);

    let receiver =
        EiReceiver::new(client_stream.into(), "shareinputdevices-test").expect("EiReceiver::new");

    // Kick the receiver off as a spawn_local so it makes progress
    // while we wait for the eis drive to complete the handshake.
    // We then await both — the receiver's `start()` returns once
    // it sees the `Connection` event from the eis side.
    let (wire_rx, disconnect_rx, drive) = {
        let receiver = receiver.clone();
        let start_fut = async move { receiver.start().await };
        let join = tokio::task::spawn_local(start_fut);
        let start_result = timeout(Duration::from_secs(5), join)
            .await
            .expect("receiver start timed out")
            .expect("spawn join failed");
        start_result.expect("receiver start failed")
    };

    let connection = conn_rx.await.expect("EIS handshake setup");

    (
        connection,
        receiver,
        wire_rx,
        disconnect_rx,
        drive,
        Some(eis_done_tx),
    )
}

/// Send a `pointer_motion` event by reaching into the device's
/// `ei_pointer` interface proxy, then commit it with a Frame so the
/// converter flushes the timestamped event out of its pending queue.
/// (reis's EiEventConverter holds PointerMotion in pending_events
/// until a Frame for the same device arrives — same libei semantics.)
fn pointer_motion(connection: &EisConnection, device: &EisDevice, dx: f32, dy: f32) {
    let ptr: eis::Pointer = device.interface().expect("device has pointer interface");
    ptr.motion_relative(dx, dy);
    device.frame(0);
    connection.flush().expect("flush motion+frame");
}

fn button_event(connection: &EisConnection, device: &EisDevice, btn: u32, is_press: bool) {
    let btn_iface: eis::Button = device.interface().expect("device has button interface");
    let state = if is_press {
        reis::eis::button::ButtonState::Press
    } else {
        reis::eis::button::ButtonState::Released
    };
    btn_iface.button(btn, state);
    device.frame(0);
    connection.flush().expect("flush button+frame");
}

fn key_event(connection: &EisConnection, device: &EisDevice, keycode: u32, is_press: bool) {
    let kb: eis::Keyboard = device.interface().expect("device has keyboard interface");
    let state = if is_press {
        EisKeyState::Press
    } else {
        EisKeyState::Released
    };
    kb.key(keycode, state);
    // Keys only need a frame for the press path (the receiver drops
    // releases — see ei.rs:549-551), but sending a frame unconditionally
    // matches the libei semantics and keeps the helper uniform.
    device.frame(0);
    connection.flush().expect("flush key+frame");
}

/// Build a keymap memfd and return the fd to bind via before_done_cb.
/// xkb_keymap_new_from_string (the C-side parser the upstream cpp
/// calls at inputcapturesession.cpp:57) consumes the buffer as a
/// null-terminated string — without the trailing null, the parse
/// silently fails and the cpp falls back to the default keymap
/// (:63). For our tests we want a known keymap, so we append the
/// null. The reis side passes `size` through as the byte count.
///
/// NOTE: the xkbcommon Rust binding wraps the buffer-style C entry
/// point `xkb_keymap_new_from_buffer` (explicit length, NOT null-
/// terminated), so the trailing `\0` here would land inside the
/// keymap text and xkb's parser rejects it with
/// `[XKB-822] Failed to parse input xkb string`. We strip any
/// trailing NUL after reading in `build_xkb_state`, and we pass the
/// buffer length WITHOUT the null from this helper. Real portal
/// delivery writes the raw xkb text, not a NUL-padded buffer.
fn keymap_fd(text: &str) -> (std::fs::File, u32) {
    let memfd = memfd_create().expect("memfd_create");
    use std::io::Write;
    let bytes = text.as_bytes();
    let size = bytes.len() as u32;
    (&memfd).write_all(bytes).expect("write keymap");
    (memfd, size)
}

#[test]
fn pointer_motion_round_trip() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        let (connection, receiver, mut wire_rx, _disconnect, drive, eis_done_tx) = setup().await;
        let pump = tokio::task::spawn_local(drive);

        // Add a seat with pointer capability.
        let seat = connection.add_seat(
            Some("test"),
            DeviceCapability::Pointer | DeviceCapability::Button | DeviceCapability::Scroll,
        );

        // Add a virtual pointer device.
        let device = seat.add_device(
            Some("test-pointer"),
            DeviceType::Virtual,
            DeviceCapability::Pointer | DeviceCapability::Button | DeviceCapability::Scroll,
            |_device| {},
        );
        device.resumed();
        device.start_emulating(1);

        // Wait for SeatAdded/DeviceAdded to settle on the receiver side.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // The activation gate is armed by start_emulating(1). In production
        // the D-Bus Activated signal would carry activation_id=1 and clear
        // the gate. We mirror that here so the motion flows through to the
        // wire body instead of being queued. The activation-gate ordering
        // dance itself is asserted in `activation_gate_queues_until_activated`.
        receiver.handle_activated(1).await;

        pointer_motion(&connection, &device, 1.5, 2.5);
        let body = timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("timed out waiting for motion")
            .expect("wire rx closed");
        // M1's plan_motion emits the upstream cpp wire shape:
        // `{dx, dy}` (no `scroll` field — the scroll flag is only set on
        // scrollDelta / scrollDiscrete packets, shareinputdevicesplugin.cpp:95).
        assert_eq!(body.into_json(), serde_json::json!({"dx": 1.5, "dy": 2.5}));

        // Trigger the eis drive to drop its converter (closing the socket
        // from our side); then drop the test's Connection wrapper. With
        // both ends closed, the receiver's pump sees EOF and exits.
        let _ = eis_done_tx.unwrap().send(());
        drop(connection);
        let _ = timeout(Duration::from_secs(2), pump)
            .await
            .expect("pump did not exit on disconnect");
    });
}

#[test]
fn button_press_release_round_trip() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        let (connection, _receiver, mut wire_rx, _disconnect, drive, eis_done_tx) = setup().await;
        let pump = tokio::task::spawn_local(drive);

        let seat = connection.add_seat(
            Some("test"),
            DeviceCapability::Button | DeviceCapability::Pointer | DeviceCapability::Scroll,
        );
        let device = seat.add_device(
            Some("test-pointer"),
            DeviceType::Virtual,
            DeviceCapability::Button | DeviceCapability::Pointer | DeviceCapability::Scroll,
            |_device| {},
        );
        device.resumed();
        device.start_emulating(1);

        tokio::time::sleep(Duration::from_millis(150)).await;

        // Arm-then-clear the activation gate so events flow through.
        // The ordering dance itself is asserted in
        // `activation_gate_queues_until_activated`.
        _receiver.handle_activated(1).await;

        // BTN_LEFT = 0x110: press → singlehold, release → singlerelease.
        button_event(&connection, &device, 0x110, true);
        let body = timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("press timed out")
            .expect("wire rx closed");
        assert_eq!(body.into_json(), serde_json::json!({"singlehold": true}));

        button_event(&connection, &device, 0x110, false);
        let body = timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("release timed out")
            .expect("wire rx closed");
        assert_eq!(body.into_json(), serde_json::json!({"singlerelease": true}));

        let _ = eis_done_tx.unwrap().send(());
        drop(connection);
        let _ = timeout(Duration::from_secs(2), pump)
            .await
            .expect("pump did not exit");
    });
}

#[test]
fn activation_gate_queues_until_activated() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        // Critical scenario: the cpp's `m_currentEisSequence > m_currentActivationId`
        // dance. We send start_emulating(7), then a motion, then call
        // handle_activated(7) — the motion should NOT arrive on the wire
        // until activated, then it should be replayed.
        let (connection, receiver, mut wire_rx, _disconnect, drive, eis_done_tx) = setup().await;
        let pump = tokio::task::spawn_local(drive);

        let seat = connection.add_seat(
            Some("test"),
            DeviceCapability::Pointer | DeviceCapability::Button | DeviceCapability::Scroll,
        );
        let device = seat.add_device(
            Some("test-pointer"),
            DeviceType::Virtual,
            DeviceCapability::Pointer | DeviceCapability::Button | DeviceCapability::Scroll,
            |_device| {},
        );
        device.resumed();
        device.start_emulating(7);

        tokio::time::sleep(Duration::from_millis(150)).await;

        pointer_motion(&connection, &device, 9.0, 10.0);

        // The motion should NOT have made it through yet — it's queued.
        let premature = timeout(Duration::from_millis(150), wire_rx.recv()).await;
        assert!(
            premature.is_err(),
            "motion arrived on the wire while gate was armed"
        );

        // The activation-id arrives. The queue drains in arrival order.
        receiver.handle_activated(7).await;
        let body = timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("replay timed out")
            .expect("wire rx closed");
        assert_eq!(body.into_json(), serde_json::json!({"dx": 9.0, "dy": 10.0}));

        let _ = eis_done_tx.unwrap().send(());
        drop(connection);
        let _ = timeout(Duration::from_secs(2), pump)
            .await
            .expect("pump did not exit");
    });
}

#[test]
fn events_passthrough_when_not_armed() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        // The default state: no start_emulating has run. Events should
        // flow straight through (matches the cpp's initial 0/0 gate
        // condition).
        let (connection, _receiver, mut wire_rx, _disconnect, drive, eis_done_tx) = setup().await;
        let pump = tokio::task::spawn_local(drive);

        let seat = connection.add_seat(
            Some("test"),
            DeviceCapability::Pointer | DeviceCapability::Button | DeviceCapability::Scroll,
        );
        let device = seat.add_device(
            Some("test-pointer"),
            DeviceType::Virtual,
            DeviceCapability::Pointer | DeviceCapability::Button | DeviceCapability::Scroll,
            |_device| {},
        );
        device.resumed();
        // Note: no start_emulating — gate never arms.

        tokio::time::sleep(Duration::from_millis(150)).await;

        pointer_motion(&connection, &device, 3.0, 4.0);
        let body = timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("motion timed out")
            .expect("wire rx closed");
        assert_eq!(body.into_json(), serde_json::json!({"dx": 3.0, "dy": 4.0}));

        let _ = eis_done_tx.unwrap().send(());
        drop(connection);
        let _ = timeout(Duration::from_secs(2), pump)
            .await
            .expect("pump did not exit");
    });
}

#[test]
fn keyboard_keymap_loads_and_emits_text() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        // Sanity test for the xkb path: send a key event and assert
        // that the receiver emits a `WireBody::Key` whose `key` field
        // is the keysym text for the keycode. The text is "h" for
        // KEY_H (evdev 35). We don't pin exact wire shape (special_key
        // codes come from a Qt::Key→int table that lives outside M3) —
        // only that the body shape is a JSON object with a string `key`.
        let (connection, _receiver, mut wire_rx, _disconnect, drive, eis_done_tx) = setup().await;
        let pump = tokio::task::spawn_local(drive);

        let seat = connection.add_seat(Some("test"), DeviceCapability::Keyboard.into());
        let keymap_text = TEST_KEYMAP.to_string();
        let device = seat.add_device(
            Some("test-kb"),
            DeviceType::Virtual,
            DeviceCapability::Keyboard.into(),
            move |device| {
                let kb: eis::Keyboard = device
                    .interface()
                    .expect("keyboard interface available pre-done");
                let (memfd, size) = keymap_fd(&keymap_text);
                kb.keymap(EisKeymapType::Xkb, size, memfd.as_fd());
            },
        );
        device.resumed();
        device.start_emulating(1);

        tokio::time::sleep(Duration::from_millis(200)).await;

        // Arm-then-clear the activation gate. Same idiom as
        // pointer_motion_round_trip / button_press_release_round_trip.
        _receiver.handle_activated(1).await;

        // KEY_H = 35 (evdev) → produces XK_h.
        key_event(&connection, &device, 35, true);

        let body = timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("key timed out")
            .expect("wire rx closed");
        let json = body.into_json();
        // The body shape is what `plan_key` produces: {key, specialKey, shift, ctrl, alt, super}.
        let obj = json.as_object().expect("key body is an object");
        assert_eq!(
            obj.get("key").and_then(|v| v.as_str()),
            Some("h"),
            "key text should be 'h' for KEY_H; got {}",
            json
        );
        assert_eq!(obj.get("shift").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(obj.get("ctrl").and_then(|v| v.as_bool()), Some(false));

        let _ = eis_done_tx.unwrap().send(());
        drop(connection);
        let _ = timeout(Duration::from_secs(2), pump)
            .await
            .expect("pump did not exit");
    });
}

// ---------- memfd helper ----------

fn memfd_create() -> std::io::Result<std::fs::File> {
    use std::ffi::CString;
    use std::io::Error;
    // memfd_create syscall number is 319 on x86_64 Linux.
    const SYS_MEMFD_CREATE: libc::c_long = 319;
    let name = CString::new("reis-test-keymap").expect("cstring");
    let res = unsafe { libc::syscall(SYS_MEMFD_CREATE, name.as_ptr(), 0u32) };
    if res < 0 {
        return Err(Error::last_os_error());
    }
    let fd = res as std::os::unix::io::RawFd;
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}
