//! HTTP bridge backend: `POST {url}/apdu` with `{"apdu":"<hex>"}`, answer `{"response":"<hex data||SW>"}`.

use anyhow::{Context, Result};

use crate::Transport;

pub const DEFAULT_URL: &str = "http://127.0.0.1:9876";

pub fn url_from_env() -> String {
    std::env::var("CARD_BRIDGE_URL").unwrap_or_else(|_| DEFAULT_URL.into())
}

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

/// Turn a ureq error into an anyhow error that includes the bridge's response body.
fn describe(what: &str, err: ureq::Error) -> anyhow::Error {
    match err {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_else(|e| format!("<unreadable body: {e}>"));
            anyhow::anyhow!("{what}: HTTP {code}: {body}")
        }
        other => anyhow::Error::new(other).context(what.to_owned()),
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
