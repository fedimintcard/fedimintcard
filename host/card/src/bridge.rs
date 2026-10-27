//! HTTP bridge backend: `POST {url}/apdu` with `{"apdu":"<hex>"}`, answer `{"response":"<hex data||SW>"}`.

use anyhow::{Context, Result};

use crate::Transport;

pub const DEFAULT_URL: &str = "http://127.0.0.1:9876";

pub fn url_from_env() -> String {
    std::env::var("CARD_BRIDGE_URL").unwrap_or_else(|_| DEFAULT_URL.into())
}

/// Why a call to the bridge failed. The `Display` text is what the CLI has always printed
/// (`POST /apdu: HTTP 503: <body>`); callers match on the variant instead of the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    /// The bridge answered 503: no card in the field (or it did not answer). Holds the full message.
    NoCard(String),
    /// The bridge could not be reached at all (connection refused, timeout, DNS). Full message.
    Unreachable(String),
    /// Any other HTTP status, with the full message.
    Http(u16, String),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::NoCard(m) | BridgeError::Unreachable(m) | BridgeError::Http(_, m) => f.write_str(m),
        }
    }
}
impl std::error::Error for BridgeError {}

pub struct BridgeTransport {
    base: String,
    agent: ureq::Agent,
}

impl BridgeTransport {
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            agent: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(10))
                .build(),
        }
    }

    pub fn health(&self) -> Result<()> {
        self.agent.get(&format!("{}/health", self.base)).call().map_err(|e| describe("GET /health", e))?;
        Ok(())
    }

    /// Ask the bridge for a fresh card (all slots empty).
    pub fn reset(&self) -> Result<()> {
        self.agent.post(&format!("{}/reset", self.base)).send_string("").map_err(|e| describe("POST /reset", e))?;
        Ok(())
    }
}

/// Turn a ureq error into a [`BridgeError`] that includes the bridge's response body.
fn describe(what: &str, err: ureq::Error) -> anyhow::Error {
    match err {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_else(|e| format!("<unreadable body: {e}>"));
            let msg = format!("{what}: HTTP {code}: {body}");
            if code == 503 { BridgeError::NoCard(msg) } else { BridgeError::Http(code, msg) }.into()
        }
        other => BridgeError::Unreachable(format!("{what}: {other}")).into(),
    }
}

impl Transport for BridgeTransport {
    fn transmit(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
        let resp = self
            .agent
            .post(&format!("{}/apdu", self.base))
            .send_json(serde_json::json!({ "apdu": hex::encode(apdu) }))
            .map_err(|e| describe("POST /apdu", e))?;
        let json: serde_json::Value = resp.into_json().context("bridge reply is not valid JSON")?;
        let hex_resp = json["response"].as_str().context("bridge reply has no `response` string")?;
        hex::decode(hex_resp).with_context(|| format!("bridge `response` is not hex: {hex_resp:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// Read one whole HTTP request (headers and body), so closing the socket afterwards cannot
    /// reset the connection before the client has read its answer.
    fn drain_request(s: &mut std::net::TcpStream) {
        let mut data = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = s.read(&mut buf).unwrap_or(0);
            if n == 0 {
                return;
            }
            data.extend_from_slice(&buf[..n]);
            if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
                let want = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                    .unwrap_or(0);
                if data.len() >= end + 4 + want {
                    return;
                }
            }
        }
    }

    /// One-shot HTTP server answering every request with `status` and `body`.
    fn serve(status: &'static str, body: &'static str) -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        std::thread::spawn(move || {
            for s in l.incoming() {
                let mut s = s.unwrap();
                drain_request(&mut s);
                let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        url
    }

    fn bridge_err(e: &anyhow::Error) -> BridgeError {
        e.chain().find_map(|c| c.downcast_ref::<BridgeError>()).cloned().expect("a BridgeError")
    }

    #[test]
    fn status_503_is_no_card_and_keeps_the_old_text() {
        let url = serve("503 Service Unavailable", "no card");
        let err = BridgeTransport::new(&url).transmit(&[0x00, 0xA4]).unwrap_err();
        assert_eq!(bridge_err(&err), BridgeError::NoCard("POST /apdu: HTTP 503: no card".into()));
        assert_eq!(err.to_string(), "POST /apdu: HTTP 503: no card");
    }

    #[test]
    fn other_status_is_http_and_refused_connection_is_unreachable() {
        let url = serve("500 Internal Server Error", "boom");
        let err = BridgeTransport::new(&url).transmit(&[0]).unwrap_err();
        assert!(matches!(bridge_err(&err), BridgeError::Http(500, _)));

        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let err = BridgeTransport::new(&format!("http://127.0.0.1:{port}")).health().unwrap_err();
        assert!(matches!(bridge_err(&err), BridgeError::Unreachable(_)), "{err:#}");
    }
}
