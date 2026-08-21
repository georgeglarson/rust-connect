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

use enumflags2::BitFlags;
use reis::eis::device::DeviceType;
use reis::eis::keyboard::{KeyState as EisKeyState, KeymapType as EisKeymapType};
use reis::eis::{self};
use reis::event::DeviceCapability;
use reis::handshake::EisHandshaker;
use reis::request::{Connection as EisConnection, Device as EisDevice, EisRequest};
use tokio::time::timeout;

use rust_connect::plugins::shareinputdevices::ei::EiReceiver;

/// Minimal valid XKB keymap. KEY_H is evdev 35 → xkbcommon keycode
/// 43 (the evdev +8 offset lives in `XKB_KEYCODE_OFFSET`). We bind
/// the "HKTG" semantic at keycode 43 so the test's KEY_H lookup hits
/// XK_h, and "AC03" at 54 (evdev KEY_C = 46) so the Ctrl-shortcut
/// test has a letter key whose state-based lookup would collapse to
/// a control byte. The keymap doesn't have to be production-quality
/// — it just has to parse, and the keys under test need to be
/// addressable.
const TEST_KEYMAP: &str = r#"xkb_keymap {
xkb_keycodes {
	minimum = 8;
	maximum = 255;
	<ESC> = 9;
	<AE01> = 10;
	<AE02> = 11;
	<BKSP> = 22;
	<HKTG> = 43;
	<AC03> = 54;
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
	key <AC03> {	[ c	]	};
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
/// **The fake keeps reading after the handshake.** A fake EIS that
/// stops draining its socket cannot distinguish a request the
/// receiver merely buffered from one it actually flushed — both look
/// like silence. Draining post-handshake is what lets
/// `seat_bind_reaches_the_eis_peer_before_devices` observe the
/// `ei_seat.bind` request as a real wire event. The bound capability
/// set from the first `EisRequest::Bind` is published on the returned
/// oneshot; every other request is drained and dropped (the receiver
/// is a pure consumer and sends nothing else).
///
/// **Caller contract:** this must be invoked from within a
/// `tokio::task::LocalSet` (each test builds its own with
/// `LocalSet::new()` and `local.block_on(&rt, ...)` — see the test
/// bodies below). The test body, the eis drive task, and the
/// `receiver.start()` future all run on the same thread.
#[allow(clippy::type_complexity)]
async fn setup() -> (
    EisConnection,
    std::sync::Arc<EiReceiver>,
    tokio::sync::mpsc::UnboundedReceiver<rust_connect::plugins::shareinputdevices::ei::WireBody>,
    tokio::sync::watch::Receiver<bool>,
    impl std::future::Future<Output = ()>,
    Option<tokio::sync::oneshot::Sender<()>>,
    tokio::sync::oneshot::Receiver<BitFlags<DeviceCapability>>,
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
    let (bind_tx, bind_rx) = tokio::sync::oneshot::channel::<BitFlags<DeviceCapability>>();
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
            let mut converter = reis::request::EisRequestConverter::new(&eis_ctx, resp, 1);
            let connection = converter.handle().clone();
            let _ = conn_tx.send(connection);
            // Keep draining the socket until the test signals teardown,
            // then drop the converter — which closes the eis-side
            // socket and lets the receiver's pump see EOF. (Without
            // the drop the converter stays alive forever and the
            // socket never closes.)
            let mut bind_tx = Some(bind_tx);
            let mut done = std::pin::pin!(eis_done_rx);
            loop {
                let _ = eis_ctx.read();
                while let Some(result) = eis_ctx.pending_request() {
                    let request = match result {
                        reis::PendingRequestResult::Request(r) => r,
                        reis::PendingRequestResult::ParseError(e) => panic!("parse error: {e}"),
                        reis::PendingRequestResult::InvalidObject(id) => {
                            panic!("invalid object: {id}")
                        }
                    };
                    converter
                        .handle_request(request)
                        .expect("post-handshake handle_request");
                }
                while let Some(request) = converter.next_request() {
                    if let EisRequest::Bind(bind) = request {
                        if let Some(tx) = bind_tx.take() {
                            let _ = tx.send(bind.capabilities);
                        }
                    }
                }
                tokio::select! {
                    _ = &mut done => break,
                    () = tokio::time::sleep(Duration::from_millis(5)) => {}
                }
            }
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

    let connection = timeout(Duration::from_secs(5), conn_rx)
        .await
        .expect("EIS handshake setup timed out")
        .expect("EIS handshake setup");

    (
        connection,
        receiver,
        wire_rx,
        disconnect_rx,
        drive,
        Some(eis_done_tx),
        bind_rx,
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
        let (connection, receiver, mut wire_rx, _disconnect, drive, eis_done_tx, _bind_rx) =
            setup().await;
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
        let (connection, _receiver, mut wire_rx, _disconnect, drive, eis_done_tx, _bind_rx) =
            setup().await;
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
        let (connection, receiver, mut wire_rx, _disconnect, drive, eis_done_tx, _bind_rx) =
            setup().await;
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
        let (connection, _receiver, mut wire_rx, _disconnect, drive, eis_done_tx, _bind_rx) =
            setup().await;
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
        let (connection, _receiver, mut wire_rx, _disconnect, drive, eis_done_tx, _bind_rx) =
            setup().await;
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

// ---------- scroll tests ----------

/// Send an EI `ScrollDelta` (smooth path) by reaching into the device's
/// `ei_scroll` interface proxy, then commit it with a Frame so the
/// converter flushes the timestamped event out of its pending queue.
/// Mirrors the `pointer_motion` / `button_event` / `key_event` helpers
/// above — same `device.frame(0); connection.flush()` close.
fn scroll_delta(connection: &EisConnection, device: &EisDevice, dx: f32, dy: f32) {
    let scroll: eis::Scroll = device.interface().expect("device has scroll interface");
    scroll.scroll(dx, dy);
    device.frame(0);
    connection.flush().expect("flush scroll+frame");
}

/// Send an EI `ScrollDiscrete` (high-res wheel path).
fn scroll_discrete(connection: &EisConnection, device: &EisDevice, dx: i32, dy: i32) {
    let scroll: eis::Scroll = device.interface().expect("device has scroll interface");
    scroll.scroll_discrete(dx, dy);
    device.frame(0);
    connection.flush().expect("flush scroll_discrete+frame");
}

/// Send an EI `ScrollStop` (is_cancel=0). The upstream cpp at
/// inputcapturesession.cpp:418-419 breaks out of the switch with no
/// emit; the transport must NOT produce a `WireBody`.
fn scroll_stop(connection: &EisConnection, device: &EisDevice, x: u32, y: u32) {
    let scroll: eis::Scroll = device.interface().expect("device has scroll interface");
    scroll.scroll_stop(x, y, 0);
    device.frame(0);
    connection.flush().expect("flush scroll_stop+frame");
}

/// Send an EI `ScrollCancel` (is_cancel=1). The upstream cpp at
/// inputcapturesession.cpp:420-421 also breaks with no emit.
fn scroll_cancel(connection: &EisConnection, device: &EisDevice, x: u32, y: u32) {
    let scroll: eis::Scroll = device.interface().expect("device has scroll interface");
    scroll.scroll_stop(x, y, 1);
    device.frame(0);
    connection.flush().expect("flush scroll_cancel+frame");
}

#[test]
fn scroll_delta_round_trip() {
    // Oracle: the upstream cpp at inputcapturesession.cpp:412-413 emits
    // the smooth delta verbatim — `Q_EMIT scrollDelta(dx, dy)`. M1's
    // `plan_scroll(dx, dy, 0, 0)` returns `{scroll: true, dx, dy}` with
    // both fields passing through verbatim (mod.rs:233-256; the y-asymmetry
    // lives in the discrete path only — shareinputdevicesplugin.cpp:100-101
    // negates y on the discrete side, NOT the smooth side). The transport
    // exists to route the EI event to the planner.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        let (connection, receiver, mut wire_rx, _disconnect, drive, eis_done_tx, _bind_rx) =
            setup().await;
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
        device.start_emulating(1);

        tokio::time::sleep(Duration::from_millis(150)).await;

        // Arm-then-clear the activation gate, same idiom as
        // `pointer_motion_round_trip`.
        receiver.handle_activated(1).await;

        scroll_delta(&connection, &device, 3.5, -4.5);
        let body = timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("scroll delta timed out")
            .expect("wire rx closed");
        assert_eq!(
            body.into_json(),
            serde_json::json!({"scroll": true, "dx": 3.5, "dy": -4.5})
        );

        let _ = eis_done_tx.unwrap().send(());
        drop(connection);
        let _ = timeout(Duration::from_secs(2), pump)
            .await
            .expect("pump did not exit");
    });
}

#[test]
fn scroll_discrete_round_trip() {
    // Oracle: the upstream cpp at inputcapturesession.cpp:415-416 emits
    // `Q_EMIT scrollDiscrete(discrete_dx, discrete_dy)`. M1's
    // `plan_scroll_discrete(dx, dy)` returns `{scroll: true, dx:
    // dx * 15/120, dy: -dy * 15/120}` (mod.rs:267-274) — the upstream
    // y-negation is the planner's pinned job. The transport exists to
    // route to it.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        let (connection, receiver, mut wire_rx, _disconnect, drive, eis_done_tx, _bind_rx) =
            setup().await;
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
        device.start_emulating(1);

        tokio::time::sleep(Duration::from_millis(150)).await;

        receiver.handle_activated(1).await;

        // discrete_dx=0, discrete_dy=1 — the y-negation gives
        // wire dy = -1 * 15/120 = -0.125; x stays signed normally
        // (mod.rs:956-965 pins x is NOT negated).
        scroll_discrete(&connection, &device, 0, 1);
        let body = timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("scroll discrete timed out")
            .expect("wire rx closed");
        assert_eq!(
            body.into_json(),
            serde_json::json!({"scroll": true, "dx": 0.0, "dy": -0.125})
        );

        let _ = eis_done_tx.unwrap().send(());
        drop(connection);
        let _ = timeout(Duration::from_secs(2), pump)
            .await
            .expect("pump did not exit");
    });
}

#[test]
fn scroll_stop_and_cancel_are_noops() {
    // Oracle: the upstream cpp at inputcapturesession.cpp:418-421 has
    // `case EI_EVENT_SCROLL_STOP: break; case EI_EVENT_SCROLL_CANCEL:
    // break;` — both with no emit. The transport's `dispatch`
    // (ei.rs:523-525) collapses them into a single no-op arm.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        let (connection, receiver, mut wire_rx, _disconnect, drive, eis_done_tx, _bind_rx) =
            setup().await;
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
        device.start_emulating(1);

        tokio::time::sleep(Duration::from_millis(150)).await;

        receiver.handle_activated(1).await;

        // One real ScrollDelta first — we expect exactly ONE wire body.
        scroll_delta(&connection, &device, 1.0, 1.0);
        let body = timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("scroll delta timed out")
            .expect("wire rx closed");
        assert_eq!(
            body.into_json(),
            serde_json::json!({"scroll": true, "dx": 1.0, "dy": 1.0})
        );

        // Now the no-ops: ScrollStop (is_cancel=0) and ScrollCancel
        // (is_cancel=1). Neither must produce a wire body.
        scroll_stop(&connection, &device, 1, 1);
        scroll_cancel(&connection, &device, 1, 1);

        // Wait a bit to let any spurious body arrive; assert none does.
        let spurious = timeout(Duration::from_millis(200), wire_rx.recv()).await;
        assert!(
            spurious.is_err(),
            "ScrollStop/ScrollCancel must not produce a wire body"
        );

        let _ = eis_done_tx.unwrap().send(());
        drop(connection);
        let _ = timeout(Duration::from_secs(2), pump)
            .await
            .expect("pump did not exit");
    });
}

#[test]
fn gate_queues_scroll_until_activated() {
    // Oracle: the cpp's `m_currentEisSequence > m_currentActivationId`
    // dance (inputcapturesession.cpp:362-366) holds scroll events in
    // `queuedEiEvents` until the D-Bus `Activated` signal arrives.
    // The transport ports this via `ActivationGate::should_queue()` →
    // `PendingInput::ScrollDelta` / `PendingInput::ScrollDiscrete`
    // (ei.rs:505-509, :515-522) and the drain at :620-623.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        let (connection, receiver, mut wire_rx, _disconnect, drive, eis_done_tx, _bind_rx) =
            setup().await;
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
        device.start_emulating(11);

        tokio::time::sleep(Duration::from_millis(150)).await;

        // Gate is armed (sequence=11, activation_id=0). Send a
        // ScrollDelta AND a ScrollDiscrete — both must be queued,
        // NOT emitted on the wire.
        scroll_delta(&connection, &device, 2.0, 3.0);
        scroll_discrete(&connection, &device, 1, 1);

        let premature = timeout(Duration::from_millis(200), wire_rx.recv()).await;
        assert!(
            premature.is_err(),
            "scroll events arrived on the wire while gate was armed"
        );

        // Activated. The drain emits in arrival order: ScrollDelta
        // body first (smooth passthrough), then ScrollDiscrete body
        // (y-negated dy).
        receiver.handle_activated(11).await;

        let first = timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("queued scroll delta replay timed out")
            .expect("wire rx closed");
        assert_eq!(
            first.into_json(),
            serde_json::json!({"scroll": true, "dx": 2.0, "dy": 3.0})
        );

        let second = timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("queued scroll discrete replay timed out")
            .expect("wire rx closed");
        assert_eq!(
            second.into_json(),
            serde_json::json!({"scroll": true, "dx": 0.125, "dy": -0.125})
        );

        let _ = eis_done_tx.unwrap().send(());
        drop(connection);
        let _ = timeout(Duration::from_secs(2), pump)
            .await
            .expect("pump did not exit");
    });
}

// ---------- disconnect tests ----------

#[test]
fn disconnect_via_explicit_event_signals_and_completes() {
    // Oracle: the cpp at inputcapturesession.cpp:372-374 logs the
    // EI disconnect and falls through. The receiver's `dispatch`
    // (ei.rs:410-417) fires `disconnect_tx.send(true)` on the event
    // AND exits the pump (the M3 fix-lane break — reis does not EOF
    // the stream on `connection.disconnected`, so a Disconnected
    // event alone would otherwise hang the pump until the socket
    // closes). The eis drive task here is also torn down for
    // hygiene, but the pump exits on the protocol event itself.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        let (connection, _receiver, _wire_rx, mut disconnect_rx, drive, eis_done_tx, _bind_rx) =
            setup().await;
        let pump = tokio::task::spawn_local(drive);

        // 1. Send the explicit Disconnected event from the EIS side.
        connection.disconnected(reis::ei::connection::DisconnectReason::Disconnected, None);

        // 2. disconnect_rx flips to true once dispatch sees the event.
        //    This is the oracle assertion for the Disconnected event.
        let _ = timeout(Duration::from_secs(2), disconnect_rx.changed())
            .await
            .expect("disconnect_rx did not flip on Disconnected event");
        assert!(
            *disconnect_rx.borrow_and_update(),
            "disconnect_rx must be true after Disconnected event"
        );

        // 3. Close the socket to end the eis_drive task (hygiene;
        //    the pump has already exited on the protocol event above).
        let _ = eis_done_tx.unwrap().send(());
        drop(connection);

        // 4. drive future completes — primary oracle: pump exits on the
        //    Disconnected event itself, not on socket EOF.
        let _ = timeout(Duration::from_secs(2), pump)
            .await
            .expect("pump did not exit after Disconnected event");
    });
}

#[test]
fn disconnect_event_alone_completes_pump_without_socket_close() {
    // Red-before-green oracle for the M3 fix-lane B: the receiver's
    // pump must exit on the explicit Disconnected protocol event
    // alone — no socket close, no `eis_done_tx` send. reis's
    // `Connection::disconnected` (reis request.rs:106) calls
    // `shutdown_read` on the EIS side, NOT `shutdown_write` — that
    // half-shutdown does not produce an EOF on the Receiver's read
    // end. Without the fix, the pump's `while let Some(event) =
    // stream.next().await` blocks forever after dispatching the
    // Disconnected event. This test pins the post-fix invariant:
    // the terminal protocol event ends the pump on its own.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        let (connection, _receiver, _wire_rx, mut disconnect_rx, drive, _eis_done_tx, _bind_rx) =
            setup().await;
        let pump = tokio::task::spawn_local(drive);

        // Send ONLY the explicit Disconnected event. Keep the
        // socket open (`connection` stays in scope; `eis_done_tx`
        // is dropped without being sent so the eis_drive task
        // continues holding its end of the socketpair).
        connection.disconnected(reis::ei::connection::DisconnectReason::Disconnected, None);

        // Oracle 1: disconnect_rx flips to true on the event.
        let _ = timeout(Duration::from_secs(2), disconnect_rx.changed())
            .await
            .expect("disconnect_rx did not flip on Disconnected event");
        assert!(
            *disconnect_rx.borrow_and_update(),
            "disconnect_rx must be true after Disconnected event"
        );

        // Oracle 2: drive completes WITHOUT any socket close.
        // Pre-fix this hangs forever; post-fix the pump's
        // Disconnected arm breaks out of the dispatch loop and
        // the function returns.
        let _ = timeout(Duration::from_secs(2), pump)
            .await
            .expect("pump did not exit on Disconnected event alone");
    });
}

#[test]
fn disconnect_via_eof_signals_and_completes() {
    // Oracle: ei.rs:392 — when the stream ends (the `while let Some(event)
    // = stream.next().await` returns None), the pump exits the loop and
    // calls `let _ = disconnect_tx.send(true);` at the bottom. This is
    // the EOF-without-explicit-Disconnected path: the EIS side just
    // closes the socket. We trigger that here by signaling the eis_drive
    // to drop its converter + eis::Context WITHOUT going through
    // `connection.disconnected()` first.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        let (connection, _receiver, _wire_rx, mut disconnect_rx, drive, eis_done_tx, _bind_rx) =
            setup().await;
        let pump = tokio::task::spawn_local(drive);

        // Signal the eis_drive task to end and drop the test's
        // Connection wrapper. The eis_drive's closure drops the
        // converter and (when the task ends) eis::Context — which
        // owns the UnixStream. Socket closes; pump sees EOF.
        let _ = eis_done_tx.unwrap().send(());
        drop(connection);

        // disconnect_rx flips to true once the pump's while-loop
        // exits on None.
        let _ = timeout(Duration::from_secs(2), disconnect_rx.changed())
            .await
            .expect("disconnect_rx did not flip on EOF");
        assert!(
            *disconnect_rx.borrow_and_update(),
            "disconnect_rx must be true after EOF"
        );

        // drive future completes.
        let _ = timeout(Duration::from_secs(2), pump)
            .await
            .expect("pump did not exit on EOF");
    });
}

#[test]
fn unmapped_keycode_does_not_panic_pump_survives() {
    // Red-before-green oracle for fix C: a keycode the delivered
    // keymap maps to XKB_KEY_NoSymbol (real keymaps have
    // unmapped/vendor codes) must NOT panic the pump. Pre-fix, the
    // `debug_assert!(keysym.raw() != 0)` in the KeyboardKey arm
    // would panic debug builds, killing the pump task silently.
    // `disconnect_tx` is dropped without `send(true)`, so
    // `disconnect_rx.changed()` resolves to `Err(closed)` and the
    // receiver is dead without a signal. Post-fix: warn + drop the
    // event (no wire body), pump survives, subsequent mapped keys
    // still emit normally.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        let (connection, _receiver, mut wire_rx, _disconnect, drive, eis_done_tx, _bind_rx) =
            setup().await;
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

        // Arm-then-clear the activation gate.
        _receiver.handle_activated(1).await;

        // Send an unmapped keycode. The TEST_KEYMAP does not bind
        // anything at keycode 50 (evdev) → xkb 58; key_get_one_sym
        // returns NoSymbol (raw 0). Pre-fix this panicked the pump.
        key_event(&connection, &device, 50, true);

        // No body should arrive — the unmapped event is dropped.
        let spurious = timeout(Duration::from_millis(200), wire_rx.recv()).await;
        assert!(
            spurious.is_err(),
            "unmapped keycode must NOT produce a wire body"
        );

        // The pump must still be alive — a subsequent mapped key
        // (KEY_H=35 → xkb 43 → XK_h) must produce a body.
        key_event(&connection, &device, 35, true);
        let body = timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("mapped key after unmapped timed out — pump must survive")
            .expect("wire rx closed");
        let json = body.into_json();
        let obj = json.as_object().expect("key body is an object");
        assert_eq!(
            obj.get("key").and_then(|v| v.as_str()),
            Some("h"),
            "subsequent mapped key still emits; got {}",
            json
        );

        // Cleanup.
        let _ = eis_done_tx.unwrap().send(());
        drop(connection);
        let _ = timeout(Duration::from_secs(2), pump)
            .await
            .expect("pump did not exit");
    });
}

#[test]
fn latched_shift_modifier_surfaces_as_shift_on_wire() {
    // Red-before-green oracle for fix D: modifiers must be read from
    // STATE_MODS_EFFECTIVE (depressed | latched | locked), not
    // STATE_MODS_DEPRESSED alone. The cpp at
    // inputcapturesession.cpp:422-426 calls
    // `xkb_state_update_mask(depressed, latched, locked, ...)` and
    // reads modifiers from the resulting effective state via
    // QXkbCommon::modifiers. Sticky-key (latched) shift must show
    // up on the wire as `shift: true`. Pre-fix, the receiver read
    // DEPRESSED only — the latched mask was dropped while
    // `key_get_utf8` (effective-state) already emitted the shifted
    // text, producing inconsistent wire packets.
    //
    // XKB modifier mask bit 0 = Shift (xkbcommon.h XKB_MOD_SHIFT).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        let (connection, _receiver, mut wire_rx, _disconnect, drive, eis_done_tx, _bind_rx) =
            setup().await;
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

        // Arm-then-clear the activation gate.
        _receiver.handle_activated(1).await;

        // Send a KeyboardModifiers event with LATCHED Shift set
        // (bit 0 of the XKB modifier mask), depressed=0, locked=0.
        // The current keymap has no Shift binding, so the latched
        // shift does not transform the keysym text — but the
        // modifiers on the wire MUST still report `shift: true`
        // because the consumer relies on them for control flow.
        {
            let kb: eis::Keyboard = device.interface().expect("device has keyboard interface");
            // serial=0 is fine for tests; group=0 (no layout switch).
            kb.modifiers(
                0, /* depressed */ 0, /* locked */ 0, /* latched */ 1,
                /* group */ 0,
            );
            device.frame(0);
            connection.flush().expect("flush modifiers+frame");
        }

        // Now press KEY_H. Pre-fix this emits shift:false; post-fix
        // it emits shift:true.
        key_event(&connection, &device, 35, true);

        let body = timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("key timed out")
            .expect("wire rx closed");
        let json = body.into_json();
        let obj = json.as_object().expect("key body is an object");
        assert_eq!(
            obj.get("key").and_then(|v| v.as_str()),
            Some("h"),
            "KEY_H should still emit 'h'; got {}",
            json
        );
        assert_eq!(
            obj.get("shift").and_then(|v| v.as_bool()),
            Some(true),
            "LATCHED Shift must surface as shift:true on the wire; got {}",
            json
        );

        let _ = eis_done_tx.unwrap().send(());
        drop(connection);
        let _ = timeout(Duration::from_secs(2), pump)
            .await
            .expect("pump did not exit");
    });
}

#[test]
fn control_char_keys_emit_empty_text() {
    // Red-before-green oracle for fix E: the cpp producer at
    // inputcapturesession.cpp:436 calls
    // `QXkbCommon::lookupStringNoKeysymTransformations(sym)`, whose
    // Qt-side equivalent strips chars < 0x20 before emitting —
    // Escape, Backspace, and Tab all produce empty text on the wire.
    // Our transport's equivalent (`xkb_state_key_get_utf8`) yields
    // the raw control byte (`\x1b` for Escape, `\x08` for Backspace,
    // `\t` for Tab). Pre-fix the wire body carried the control byte
    // in its `key` field; the Android consumer would then key a
    // unicode glyph that maps to nothing. Post-fix the transport
    // filters chars < 0x20, so the wire body carries `key: ""`.
    //
    // We test Escape here (keycode 9 → xkb 17 → XK_Escape → "\x1b").
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        let (connection, _receiver, mut wire_rx, _disconnect, drive, eis_done_tx, _bind_rx) =
            setup().await;
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

        // Arm-then-clear the activation gate.
        _receiver.handle_activated(1).await;

        // Press KEY_ESCAPE (evdev 1) → xkb 9. The keymap binds
        // <ESC>=9 → Escape, so key_get_utf8 returns "\x1b" pre-fix
        // (a control char that should be filtered).
        key_event(&connection, &device, 1, true);

        let body = timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("key timed out")
            .expect("wire rx closed");
        let json = body.into_json();
        let obj = json.as_object().expect("key body is an object");
        assert_eq!(
            obj.get("key").and_then(|v| v.as_str()),
            Some(""),
            "Escape must produce empty text on the wire (filter <0x20); got {}",
            json
        );

        let _ = eis_done_tx.unwrap().send(());
        drop(connection);
        let _ = timeout(Duration::from_secs(2), pump)
            .await
            .expect("pump did not exit");
    });
}

#[test]
fn ctrl_shortcut_keeps_the_letter_text_on_the_wire() {
    // Red-before-green oracle for the state-free text lookup. The cpp
    // producer's KeyboardKey arm calls
    // `QXkbCommon::lookupStringNoKeysymTransformations(sym)`, whose
    // Qt-side body is `xkb_keysym_to_utf8` on the BARE keysym — it
    // deliberately skips xkbcommon's Control and capitalization
    // transformations. `xkb_state_key_get_utf8` applies them: with
    // Control active and unconsumed it collapses `c` to "\x03", which
    // the `< 0x20` filter then erases entirely. Every Ctrl shortcut
    // would reach the phone as `{key: "", ctrl: true}` — nothing to
    // type. Post-fix the transport looks the text up from the keysym,
    // so the wire body carries `key: "c"` alongside `ctrl: true`.
    //
    // XKB real-modifier index 2 = Control (mask bit 2 = 4).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        let (connection, _receiver, mut wire_rx, _disconnect, drive, eis_done_tx, _bind_rx) =
            setup().await;
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

        // Arm-then-clear the activation gate.
        _receiver.handle_activated(1).await;

        // Depress Control (mask bit 2). Nothing latched, nothing locked.
        {
            let kb: eis::Keyboard = device.interface().expect("device has keyboard interface");
            kb.modifiers(
                0, /* depressed */ 4, /* locked */ 0, /* latched */ 0,
                /* group */ 0,
            );
            device.frame(0);
            connection.flush().expect("flush modifiers+frame");
        }

        // Press KEY_C (evdev 46 → xkb 54 → XK_c).
        key_event(&connection, &device, 46, true);

        let body = timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("key timed out")
            .expect("wire rx closed");
        let json = body.into_json();
        let obj = json.as_object().expect("key body is an object");
        assert_eq!(
            obj.get("ctrl").and_then(|v| v.as_bool()),
            Some(true),
            "depressed Control must surface as ctrl:true; got {}",
            json
        );
        assert_eq!(
            obj.get("key").and_then(|v| v.as_str()),
            Some("c"),
            "Ctrl+C must keep the letter text on the wire (state-free \
             keysym lookup); got {}",
            json
        );

        let _ = eis_done_tx.unwrap().send(());
        drop(connection);
        let _ = timeout(Duration::from_secs(2), pump)
            .await
            .expect("pump did not exit");
    });
}

fn memfd_create() -> std::io::Result<std::fs::File> {
    use std::ffi::CString;
    // libc::memfd_create dispatches the right syscall per
    // architecture — number 319 on x86_64, 279 on aarch64, etc.
    // The hand-rolled syscall(constant) form here was x86_64-only
    // and would break cross-arch builds.
    let name = CString::new("reis-test-keymap").expect("cstring");
    let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}
