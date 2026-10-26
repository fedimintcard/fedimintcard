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
        self.agent.get(&format!("{}/health", self.base)).call()?;
        Ok(())
    }

    /// Ask the bridge for a fresh card (all slots empty).
    pub fn reset(&self) -> Result<()> {
        self.agent.post(&format!("{}/reset", self.base)).send_string("")?;
        Ok(())
    }
}

impl Transport for BridgeTransport {
    fn transmit(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
        let resp: serde_json::Value = self
            .agent
            .post(&format!("{}/apdu", self.base))
            .send_json(serde_json::json!({ "apdu": hex::encode(apdu) }))
            .context("POST /apdu")?
            .into_json()?;
        let hex_resp = resp["response"].as_str().context("bridge reply has no `response`")?;
        Ok(hex::decode(hex_resp)?)
    }
}
