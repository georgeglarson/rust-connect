#![no_main]

use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("create tokio runtime"))
}

// Share-upload parsing path (api::handlers::share::stream_upload_to_file):
// alternating raw-body and multipart shapes with attacker-controlled bytes.
// Must never panic; the 100 MiB cap must hold regardless of framing.
fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let (content_type, body) = if data[0] % 2 == 0 {
        ("multipart/form-data; boundary=FUZZBOUNDARY", &data[1..])
    } else {
        ("application/octet-stream", data)
    };
    let request = axum::http::Request::builder()
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .body(axum::body::Body::from(body.to_vec()))
        .expect("build request");
    let dir = tempfile::tempdir().expect("create temp dir");
    let dest = dir.path().join("payload");
    // Errors are fine (garbage input is rejected); panics are not.
    let _ = runtime().block_on(
        rust_connect::api::handlers::share::stream_upload_to_file(request, &dest),
    );
});
