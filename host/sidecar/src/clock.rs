//! The card's clock (docs/OFFLINE-PROTOCOL.md § 4.3): the till keeps the newest Bitcoin block
//! header it has seen and shows it to a 3.0 card (`SET_HEADER`) whenever the card's clock is
//! older. The card's day, and so its daily offline limit, moves only with real headers.
//!
//! Headers come from an Esplora API (`GET /blocks/tip/hash`, then `GET /block/:hash/header`),
//! every [`REFRESH_EVERY`] in the background, and are kept in `<data_dir>/block-header.json` so a
//! till that restarts offline still has the last one. Nothing here ever blocks or fails a card
//! job: an unreachable Esplora only means the card's clock does not move this time.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use fedimint_card_host_card::v3::{HEADER_LEN, header_has_work, header_hash_hex, header_time};
use serde::{Deserialize, Serialize};

pub const DEFAULT_ESPLORA: &str = "https://mempool.space/api";
pub const REFRESH_EVERY: Duration = Duration::from_secs(600);
pub const HEADER_FILE: &str = "block-header.json";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize, Deserialize)]
struct HeaderFile {
    header: String,
    time: u32,
}

/// The newest header this till knows, and where it comes from.
pub struct Headers {
    url: Option<String>,
    path: PathBuf,
    latest: Mutex<Option<[u8; HEADER_LEN]>>,
    http: reqwest::Client,
}

impl Headers {
    /// `url`: the Esplora base URL; `None` keeps only what the data directory already has.
    pub fn new(url: Option<String>, data_dir: &Path) -> Arc<Self> {
        let path = data_dir.join(HEADER_FILE);
        let latest = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<HeaderFile>(&b).ok())
            .and_then(|f| hex::decode(f.header).ok())
            .and_then(|b| <[u8; HEADER_LEN]>::try_from(b).ok())
            .filter(header_has_work);
        let http = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build().expect("an HTTP client");
        Arc::new(Self {
            url: url.map(|u| u.trim_end_matches('/').to_owned()).filter(|u| !u.is_empty()),
            path,
            latest: Mutex::new(latest),
            http,
        })
    }

    /// The newest header kept.
    pub fn latest(&self) -> Option<[u8; HEADER_LEN]> {
        *self.latest.lock().unwrap()
    }

    /// Keep `h` if it has the work and is later than the one kept. Answers whether it was kept.
    pub fn offer(&self, h: [u8; HEADER_LEN]) -> Result<bool> {
        ensure!(header_has_work(&h), "the header lacks the work its bits claim, or the floor");
        let mut g = self.latest.lock().unwrap();
        if g.is_some_and(|old| header_time(&old) >= header_time(&h)) {
            return Ok(false);
        }
        *g = Some(h);
        drop(g);
        crate::state::write_private_json(&self.path, &HeaderFile { header: hex::encode(h), time: header_time(&h) })?;
        Ok(true)
    }

    /// Fetch the tip's header from Esplora and keep it if it is newer.
    pub async fn refresh(&self) -> Result<[u8; HEADER_LEN]> {
        let url = self.url.as_deref().context("no Esplora URL configured")?;
        let hash = self.get(&format!("{url}/blocks/tip/hash")).await?;
        let hash = hash.trim().to_ascii_lowercase();
        ensure!(hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()), "Esplora answered a bad tip hash");
        let hdr = self.get(&format!("{url}/block/{hash}/header")).await?;
        let bytes = hex::decode(hdr.trim()).context("Esplora answered a header that is not hex")?;
        let h: [u8; HEADER_LEN] = bytes.try_into().map_err(|_| anyhow::anyhow!("Esplora answered a header that is not 80 bytes"))?;
        ensure!(header_hash_hex(&h) == hash, "the header Esplora answered is not the tip it named");
        self.offer(h)?;
        Ok(h)
    }

    async fn get(&self, url: &str) -> Result<String> {
        let r = self.http.get(url).send().await.with_context(|| format!("GET {url}"))?;
        ensure!(r.status().is_success(), "GET {url}: {}", r.status());
        Ok(r.text().await?)
    }

    /// Refresh now and every `every` after, for as long as the till runs. Failures are logged.
    pub fn spawn(self: &Arc<Self>, every: Duration) -> Option<tokio::task::JoinHandle<()>> {
        self.url.as_ref()?;
        let me = self.clone();
        Some(tokio::spawn(async move {
            loop {
                if let Err(e) = me.refresh().await {
                    tracing::warn!("block header: {e:#}");
                }
                tokio::time::sleep(every).await;
            }
        }))
    }
}
