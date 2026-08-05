//! Blocking HTTP client for the daemon's REST API.
//!
//! The API is plain HTTP/1.1 on localhost with JSON bodies, so this is a
//! thin wrapper over `ureq` (built without TLS features): every request
//! carries the `X-API-Key` header, transport failures classify as
//! "daemon unreachable" (exit 2), and non-2xx responses surface the API
//! error envelope (exit 1).

use std::time::Duration;

use crate::cli::CliError;

/// Overall per-request timeout so a hung daemon cannot wedge the CLI
/// forever. Kept under the pair poll budget (35s) so a poll iteration
/// fails instead of hanging past it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on response bodies read into memory; the daemon only returns small
/// JSON envelopes, so anything past this is a malfunctioning endpoint.
const MAX_RESPONSE_BODY_BYTES: u64 = 10 * 1024 * 1024;

pub struct ClientConfig {
    pub base_url: String,
    pub api_key: String,
}

pub struct ApiClient {
    base_url: String,
    api_key: String,
    agent: ureq::Agent,
    file_agent: ureq::Agent,
}

impl ApiClient {
    pub fn new(config: ClientConfig) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(3)))
            // Overall timeout (covers send + receive of the whole call).
            .timeout_global(Some(REQUEST_TIMEOUT))
            // Status codes are handled explicitly so error envelopes survive.
            .http_status_as_error(false)
            .build()
            .new_agent();
        // File uploads: the daemon holds the request open until the PHONE
        // finishes pulling the payload, so a short global timeout would
        // abort slow-but-healthy transfers client-side. 10 minutes is a
        // budget, not a latency expectation.
        let file_agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(3)))
            .timeout_global(Some(Duration::from_secs(600)))
            .http_status_as_error(false)
            .build()
            .new_agent();
        Self {
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_key: config.api_key,
            agent,
            file_agent,
        }
    }

    pub fn get(&self, path: &str) -> Result<String, CliError> {
        let url = self.url(path);
        let resp = self
            .agent
            .get(&url)
            .header("X-API-Key", &self.api_key)
            .call();
        self.finish(url, resp)
    }

    pub fn delete(&self, path: &str) -> Result<String, CliError> {
        let url = self.url(path);
        let resp = self
            .agent
            .delete(&url)
            .header("X-API-Key", &self.api_key)
            .call();
        self.finish(url, resp)
    }

    pub fn post_empty(&self, path: &str) -> Result<String, CliError> {
        let url = self.url(path);
        let resp = self
            .agent
            .post(&url)
            .header("X-API-Key", &self.api_key)
            .send_empty();
        self.finish(url, resp)
    }

    pub fn post_json(&self, path: &str, payload: &serde_json::Value) -> Result<String, CliError> {
        let url = self.url(path);
        let resp = self
            .agent
            .post(&url)
            .header("X-API-Key", &self.api_key)
            .content_type("application/json")
            .send(payload.to_string());
        self.finish(url, resp)
    }

    /// Stream `file` as the request body (ureq reads it in chunks with a
    /// Content-Length from the file size — no full-file buffering).
    pub fn post_file(
        &self,
        path: &str,
        content_type: &str,
        file: std::fs::File,
    ) -> Result<String, CliError> {
        let url = self.url(path);
        let resp = self
            .file_agent
            .post(&url)
            .header("X-API-Key", &self.api_key)
            .content_type(content_type)
            .send(file);
        self.finish(url, resp)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Read the body, then map: 2xx -> body; transport error -> Unreachable;
    /// anything else -> the API error envelope's message when parseable.
    fn finish(
        &self,
        url: String,
        resp: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<String, CliError> {
        let mut resp =
            resp.map_err(|e| CliError::Unreachable(format!("cannot reach daemon at {url}: {e}")))?;
        let status: u16 = resp.status().into();
        let body = resp
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BODY_BYTES)
            .read_to_string()
            .map_err(|e| {
                CliError::Unreachable(format!("failed reading response from {url}: {e}"))
            })?;

        if (200..300).contains(&status) {
            return Ok(body);
        }

        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) {
            if let Some(error) = value.get("error") {
                let code = error
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("ERROR");
                let message = error
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                return Err(CliError::Api(format!("{code}: {message}")));
            }
        }
        Err(CliError::Api(format!("HTTP {status}: {body}")))
    }
}

/// Percent-encode a query parameter value (unreserved characters per
/// RFC 3986 pass through; everything else is %XX UTF-8).
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn test_percent_encode() {
        assert_eq!(percent_encode("file.pdf"), "file.pdf");
        assert_eq!(percent_encode("my file (1).pdf"), "my%20file%20%281%29.pdf");
        assert_eq!(percent_encode("a&b=c?d"), "a%26b%3Dc%3Fd");
        assert_eq!(percent_encode("ü.txt"), "%C3%BC.txt");
        assert_eq!(percent_encode(""), "");
    }

    #[test]
    fn test_base_url_trailing_slash_trimmed() {
        let client = ApiClient::new(ClientConfig {
            base_url: "http://127.0.0.1:9090/".to_string(),
            api_key: "k".to_string(),
        });
        assert_eq!(
            client.url("/api/v1/health"),
            "http://127.0.0.1:9090/api/v1/health"
        );
    }
}
