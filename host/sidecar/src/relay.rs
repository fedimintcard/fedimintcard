//! `card-sidecar relay`: a USB (PC/SC) reader served over the bridge's HTTP protocol, so a till
//! started with `--relay` charges, checks and loads the card on the reader as it would a card
//! behind a phone. `POST /apdu {"apdu":"<hex>"}` → `{"response":"<hex data||SW>"}`, `503` with no
//! card; `POST /reset` lets the card go, so the next APDU connects afresh (a card swapped on the
//! reader is seen); `GET /health`. Loopback only: anyone who reaches it can talk to the card.

use std::net::SocketAddr;

use anyhow::Result;

pub async fn serve(listen: SocketAddr, reader: Option<String>) -> Result<()> {
    anyhow::ensure!(listen.ip().is_loopback(), "the relay listens on loopback only, not {listen}");
    #[cfg(feature = "pcsc")]
    {
        imp::serve(listen, reader).await
    }
    #[cfg(not(feature = "pcsc"))]
    {
        let _ = reader;
        anyhow::bail!("the relay needs a build with `--features pcsc`")
    }
}

#[cfg(feature = "pcsc")]
mod imp {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use anyhow::{Context, Result};
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use fedimint_card_host_card::Transport;
    use fedimint_card_host_card::pcsc_backend::PcscTransport as Reader;
    use serde_json::{Value, json};

    struct Relay {
        reader_name: Option<String>,
        card: Mutex<Option<Reader>>,
    }

    pub async fn serve(listen: SocketAddr, reader: Option<String>) -> Result<()> {
        let state = Arc::new(Relay { reader_name: reader, card: Mutex::new(None) });
        let app = Router::new()
            .route("/apdu", post(apdu))
            .route("/reset", post(reset))
            .route("/health", get(health))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("binding {listen}"))?;
        eprintln!("relay: the USB reader's card at http://{listen}");
        axum::serve(listener, app).await?;
        Ok(())
    }

    fn transmit(r: &Relay, apdu: &[u8]) -> Result<Vec<u8>> {
        let mut g = r.card.lock().unwrap();
        // One retry with a fresh connection: a card lifted and put back drops the old one.
        for attempt in 0..2 {
            if g.is_none() {
                *g = Some(Reader::connect(r.reader_name.as_deref())?);
            }
            match g.as_mut().unwrap().transmit(apdu) {
                Ok(resp) => return Ok(resp),
                Err(e) if attempt == 0 => {
                    eprintln!("relay: {e:#}; connecting again");
                    *g = None;
                }
                Err(e) => {
                    *g = None;
                    return Err(e);
                }
            }
        }
        unreachable!()
    }

    async fn apdu(State(r): State<Arc<Relay>>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
        let Some(apdu) = body["apdu"].as_str().and_then(|h| hex::decode(h).ok()) else {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": "expected {\"apdu\":\"<hex>\"}"})));
        };
        let r2 = r.clone();
        match tokio::task::spawn_blocking(move || transmit(&r2, &apdu)).await {
            Ok(Ok(resp)) => (StatusCode::OK, Json(json!({"response": hex::encode(resp)}))),
            Ok(Err(e)) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": format!("no card: {e:#}")}))),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
        }
    }

    async fn reset(State(r): State<Arc<Relay>>) -> Json<Value> {
        *r.card.lock().unwrap() = None;
        Json(json!({"ok": true}))
    }

    async fn health() -> Json<Value> {
        Json(json!({"ok": true}))
    }
}
